//! Bounded reliable and keyed latest-value admission.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Notify, mpsc};

use crate::ids::MessageKey;
use crate::stats::{Admitted, QueueSource, RuntimeStats};

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Error returned when reliable admission is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendError;

/// Error returned by non-blocking reliable admission.
#[derive(Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    /// The bounded mailbox is full.
    Full(T),
    /// The runtime has closed admission.
    Closed(T),
}

#[derive(Debug)]
pub struct ReliableSender<T> {
    sender: mpsc::Sender<Admitted<T>>,
    stats: Arc<Mutex<RuntimeStats>>,
    terminal: bool,
    detailed: bool,
}

impl<T> Clone for ReliableSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            stats: Arc::clone(&self.stats),
            terminal: self.terminal,
            detailed: self.detailed,
        }
    }
}

impl<T> ReliableSender<T> {
    pub const fn new(
        sender: mpsc::Sender<Admitted<T>>,
        stats: Arc<Mutex<RuntimeStats>>,
        terminal: bool,
        detailed: bool,
    ) -> Self {
        Self {
            sender,
            stats,
            terminal,
            detailed,
        }
    }

    pub async fn send(&self, value: T) -> Result<(), SendError> {
        let permit = self.sender.reserve().await.map_err(|_| SendError)?;
        permit.send(Admitted::new(value, self.detailed));
        self.observe_depth();
        Ok(())
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        match self.sender.try_reserve() {
            Ok(permit) => {
                permit.send(Admitted::new(value, self.detailed));
                self.observe_depth();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(())) => {
                let mut stats = lock_unpoisoned(&self.stats);
                if self.terminal {
                    stats.terminal_rejected = stats.terminal_rejected.saturating_add(1);
                } else {
                    stats.reliable_rejected = stats.reliable_rejected.saturating_add(1);
                }
                drop(stats);
                Err(TrySendError::Full(value))
            }
            Err(mpsc::error::TrySendError::Closed(())) => Err(TrySendError::Closed(value)),
        }
    }

    fn observe_depth(&self) {
        let depth = self.sender.max_capacity() - self.sender.capacity();
        let mut stats = lock_unpoisoned(&self.stats);
        if self.terminal {
            stats.terminal_depth = depth;
            stats.terminal_high_water = stats.terminal_high_water.max(depth);
        } else {
            stats.reliable_depth = depth;
            stats.reliable_high_water = stats.reliable_high_water.max(depth);
        }
    }
}

#[derive(Debug)]
struct LatestState<T> {
    pending: BTreeMap<MessageKey, Admitted<T>>,
    closed: bool,
}

/// Result of admitting a keyed latest-value message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatestSendOutcome {
    /// A previously absent key was admitted.
    Inserted,
    /// The pending value for this key was replaced.
    Replaced,
}

/// Error returned by keyed latest-value admission.
#[derive(Debug, PartialEq, Eq)]
pub enum LatestSendError<T> {
    /// The configured distinct-key capacity is full.
    Full(T),
    /// The runtime has closed admission.
    Closed(T),
}

/// Cloneable keyed latest-value sender.
#[derive(Debug)]
pub struct LatestSender<T> {
    shared: Arc<LatestShared<T>>,
}

impl<T> Clone for LatestSender<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

#[derive(Debug)]
struct LatestShared<T> {
    source: QueueSource,
    detailed: bool,
    state: Mutex<LatestState<T>>,
    capacity: usize,
    notify: Notify,
    stats: Arc<Mutex<RuntimeStats>>,
}

pub struct LatestReceiver<T> {
    shared: Arc<LatestShared<T>>,
}

pub fn latest_channel<T>(
    capacity: usize,
    stats: Arc<Mutex<RuntimeStats>>,
    source: QueueSource,
    detailed: bool,
) -> (LatestSender<T>, LatestReceiver<T>) {
    let shared = Arc::new(LatestShared {
        source,
        detailed,
        state: Mutex::new(LatestState {
            pending: BTreeMap::new(),
            closed: false,
        }),
        capacity,
        notify: Notify::new(),
        stats,
    });
    (
        LatestSender {
            shared: Arc::clone(&shared),
        },
        LatestReceiver { shared },
    )
}

impl<T> LatestSender<T> {
    /// Admit or replace one keyed pending value.
    ///
    /// # Errors
    ///
    /// Returns [`LatestSendError::Full`] when admitting a new key would exceed the configured
    /// key capacity, or [`LatestSendError::Closed`] after runtime shutdown.
    pub fn send_latest(
        &self,
        key: MessageKey,
        value: T,
    ) -> Result<LatestSendOutcome, LatestSendError<T>> {
        let (outcome, depth) = {
            let mut state = lock_unpoisoned(&self.shared.state);
            if state.closed {
                return Err(LatestSendError::Closed(value));
            }
            let outcome = if let Some(slot) = state.pending.get_mut(&key) {
                *slot = Admitted::new(value, self.shared.detailed);
                LatestSendOutcome::Replaced
            } else {
                if state.pending.len() >= self.shared.capacity {
                    let mut measurements = lock_unpoisoned(&self.shared.stats);
                    measurements.latest_rejected = measurements.latest_rejected.saturating_add(1);
                    drop(measurements);
                    return Err(LatestSendError::Full(value));
                }
                state
                    .pending
                    .insert(key, Admitted::new(value, self.shared.detailed));
                LatestSendOutcome::Inserted
            };
            (outcome, state.pending.len())
        };
        {
            let mut stats = lock_unpoisoned(&self.shared.stats);
            stats.latest_depth = depth;
            stats.latest_high_water = stats.latest_high_water.max(depth);
            if outcome == LatestSendOutcome::Replaced {
                stats.latest_replaced = stats.latest_replaced.saturating_add(1);
            }
        }
        self.shared.notify.notify_one();
        Ok(outcome)
    }
}

impl<T> LatestReceiver<T> {
    pub fn try_recv(&self) -> Option<(MessageKey, T)> {
        self.try_recv_with_clock(std::time::Instant::now)
    }

    /// Return the most recent value using a supplied clock sample.
    fn try_recv_with_clock(
        &self,
        clock: impl FnOnce() -> std::time::Instant,
    ) -> Option<(MessageKey, T)> {
        let (item, depth) = {
            let mut state = lock_unpoisoned(&self.shared.state);
            (state.pending.pop_first(), state.pending.len())
        };
        item.map(|(key, admitted)| {
            let now = admitted.at.map(|_| clock());
            with_stats(&self.shared.stats, |stats| {
                stats.latest_depth = depth;
                if let (Some(at), Some(now)) = (admitted.at, now) {
                    stats.record_queue_age(self.shared.source, at, now);
                }
            });
            (key, admitted.value)
        })
    }

    pub async fn notified(&self) {
        self.shared.notify.notified().await;
    }

    pub fn close(&self) {
        lock_unpoisoned(&self.shared.state).closed = true;
        self.shared.notify.notify_waiters();
    }
}

pub fn observe_receive<T>(
    receiver: &mpsc::Receiver<T>,
    stats: &Arc<Mutex<RuntimeStats>>,
    terminal: bool,
) {
    let depth = receiver.max_capacity() - receiver.capacity();
    let mut stats = lock_unpoisoned(stats);
    if terminal {
        stats.terminal_depth = depth;
    } else {
        stats.reliable_depth = depth;
    }
}

pub fn stats_snapshot(stats: &Arc<Mutex<RuntimeStats>>) -> RuntimeStats {
    *lock_unpoisoned(stats)
}

pub fn with_stats(stats: &Arc<Mutex<RuntimeStats>>, update: impl FnOnce(&mut RuntimeStats)) {
    update(&mut lock_unpoisoned(stats));
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn disabled_admission_has_no_timestamp_or_latest_clock_read() {
        let stats = Arc::new(Mutex::new(RuntimeStats::default()));
        let (tx, mut rx) = mpsc::channel(1);
        let sender = ReliableSender::new(tx, Arc::clone(&stats), false, false);
        sender.try_send(7).unwrap();
        assert!(rx.try_recv().unwrap().at.is_none());
        let (latest, receiver) =
            latest_channel(1, Arc::clone(&stats), QueueSource::TerminalLatest, false);
        latest.send_latest(MessageKey::new("off"), 8).unwrap();
        assert_eq!(
            receiver
                .try_recv_with_clock(|| panic!("disabled telemetry read clock"))
                .unwrap()
                .1,
            8
        );
        assert!(stats_snapshot(&stats).detailed.is_none());
        assert!(
            stats_snapshot(&stats)
                .queue_age(QueueSource::TerminalLatest)
                .is_none()
        );
    }

    #[test]
    fn latest_replacement_uses_replacement_age_and_preserves_key_bound() {
        let stats = Arc::new(Mutex::new(RuntimeStats::new(true)));
        let (sender, receiver) =
            latest_channel(1, Arc::clone(&stats), QueueSource::ApplicationLatest, true);
        let key = MessageKey::new("same");
        assert_eq!(
            sender.send_latest(key.clone(), 1),
            Ok(LatestSendOutcome::Inserted)
        );
        let old = Instant::now();
        lock_unpoisoned(&receiver.shared.state)
            .pending
            .get_mut(&key)
            .unwrap()
            .at = Some(old);
        let replaced_after = Instant::now();
        assert_eq!(
            sender.send_latest(key.clone(), 2),
            Ok(LatestSendOutcome::Replaced)
        );
        assert!(
            lock_unpoisoned(&receiver.shared.state).pending[&key]
                .at
                .unwrap()
                >= replaced_after
        );
        assert_eq!(
            sender.send_latest(MessageKey::new("other"), 3),
            Err(LatestSendError::Full(3))
        );
        let at = Instant::now();
        lock_unpoisoned(&receiver.shared.state)
            .pending
            .get_mut(&key)
            .unwrap()
            .at = Some(at);
        assert_eq!(
            receiver.try_recv_with_clock(|| at + Duration::from_micros(501)),
            Some((key, 2))
        );
        let snapshot = stats_snapshot(&stats);
        assert_eq!(
            snapshot
                .queue_age(QueueSource::ApplicationLatest)
                .unwrap()
                .sum_us,
            501
        );
        assert_eq!(
            snapshot
                .queue_age(QueueSource::TerminalLatest)
                .unwrap()
                .sum_us,
            0
        );
        assert_eq!(snapshot.latest_replaced, 1);
        assert_eq!(snapshot.latest_depth, 0);
    }

    /// Same-binary diagnostic, not a latency gate. Compares the real timed FIFO
    /// path with the pre-telemetry raw FIFO admission/dequeue operations. Both
    /// retain the existing depth/counter locks. Excludes update/presenter timing,
    /// latest mailboxes, snapshot growth, and payload-size/cache-pressure effects.
    #[test]
    #[ignore = "manual same-binary FIFO telemetry overhead measurement"]
    fn fifo_telemetry_overhead_comparison() {
        use std::hint::black_box;

        fn run(enabled: bool, count: u64) -> (Duration, u64) {
            let stats = Arc::new(Mutex::new(RuntimeStats::new(true)));
            let (raw_tx, mut raw_rx) = mpsc::channel(64);
            let (timed_tx, mut timed_rx) = mpsc::channel(64);
            let timed_tx = ReliableSender::new(timed_tx, Arc::clone(&stats), false, true);
            let started = Instant::now();
            let mut checksum = 0_u64;
            for value in 0..count {
                let received = if black_box(enabled) {
                    timed_tx.try_send(black_box(value)).unwrap();
                    let admitted = timed_rx.try_recv().unwrap();
                    observe_receive(&timed_rx, &stats, false);
                    let now = Instant::now();
                    with_stats(&stats, |stats| {
                        stats.record_queue_age(
                            QueueSource::ApplicationFifo,
                            admitted.at.unwrap(),
                            now,
                        );
                    });
                    admitted.value
                } else {
                    raw_tx.try_send(black_box(value)).unwrap();
                    let depth = raw_tx.max_capacity() - raw_tx.capacity();
                    with_stats(&stats, |stats| {
                        stats.reliable_depth = depth;
                        stats.reliable_high_water = stats.reliable_high_water.max(depth);
                    });
                    let received = raw_rx.try_recv().unwrap();
                    observe_receive(&raw_rx, &stats, false);
                    received
                };
                with_stats(&stats, |stats| {
                    stats.reliable_processed = stats.reliable_processed.saturating_add(1);
                });
                checksum = checksum.wrapping_add(black_box(received));
            }
            let elapsed = started.elapsed();
            let snapshot = stats_snapshot(&stats);
            assert_eq!(snapshot.reliable_processed, count);
            assert_eq!(snapshot.reliable_depth, 0);
            assert_eq!(snapshot.reliable_high_water, 1);
            assert_eq!(
                snapshot
                    .queue_age(QueueSource::ApplicationFifo)
                    .unwrap()
                    .buckets
                    .iter()
                    .sum::<u64>(),
                if enabled { count } else { 0 }
            );
            (elapsed, checksum)
        }

        const COUNT: u64 = 100_000;
        let _warmup = (run(false, 1_000), run(true, 1_000));
        let mut baseline = Vec::new();
        let mut telemetry = Vec::new();
        for pair in 0..8 {
            let (off, on) = if pair % 2 == 0 {
                (run(false, COUNT), run(true, COUNT))
            } else {
                let on = run(true, COUNT);
                (run(false, COUNT), on)
            };
            assert_eq!(off.1, on.1);
            baseline.push(off.0.as_nanos() / u128::from(COUNT));
            telemetry.push(on.0.as_nanos() / u128::from(COUNT));
        }
        baseline.sort_unstable();
        telemetry.sort_unstable();
        eprintln!(
            "FIFO telemetry diagnostic: iterations={COUNT}, pairs=8, baseline_ns_per_item={baseline:?}, timed_ns_per_item={telemetry:?}; upper_medians={}/{}; not end-to-end runtime overhead",
            baseline[4], telemetry[4]
        );
    }

    #[tokio::test]
    async fn reliable_capacity_wait_is_outside_admission_timestamp() {
        let stats = Arc::new(Mutex::new(RuntimeStats::new(true)));
        let (tx, mut rx) = mpsc::channel(1);
        let sender = ReliableSender::new(tx, stats, false, true);
        sender.send(1).await.unwrap();
        assert_eq!(sender.try_send(9), Err(TrySendError::Full(9)));
        let waiting = sender.send(2);
        tokio::pin!(waiting);
        assert!(
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(waiting.as_mut().poll(cx).is_pending())
            })
            .await
        );
        assert_eq!(rx.recv().await.unwrap().value, 1);
        let released_at = Instant::now();
        waiting.await.unwrap();
        let admitted = rx.recv().await.unwrap();
        assert_eq!(admitted.value, 2);
        assert!(admitted.at.unwrap() >= released_at);
        drop(rx);
        assert_eq!(sender.try_send(3), Err(TrySendError::Closed(3)));
    }
}
