//! Bounded reliable and keyed latest-value admission.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Notify, mpsc};

use crate::ids::MessageKey;
use crate::stats::RuntimeStats;

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
    sender: mpsc::Sender<T>,
    stats: Arc<Mutex<RuntimeStats>>,
    terminal: bool,
}

impl<T> Clone for ReliableSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            stats: Arc::clone(&self.stats),
            terminal: self.terminal,
        }
    }
}

impl<T> ReliableSender<T> {
    pub const fn new(
        sender: mpsc::Sender<T>,
        stats: Arc<Mutex<RuntimeStats>>,
        terminal: bool,
    ) -> Self {
        Self {
            sender,
            stats,
            terminal,
        }
    }

    pub async fn send(&self, value: T) -> Result<(), SendError> {
        self.sender.send(value).await.map_err(|_| SendError)?;
        self.observe_depth();
        Ok(())
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        match self.sender.try_send(value) {
            Ok(()) => {
                self.observe_depth();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(value)) => {
                let mut stats = lock_unpoisoned(&self.stats);
                if self.terminal {
                    stats.terminal_rejected = stats.terminal_rejected.saturating_add(1);
                } else {
                    stats.reliable_rejected = stats.reliable_rejected.saturating_add(1);
                }
                drop(stats);
                Err(TrySendError::Full(value))
            }
            Err(mpsc::error::TrySendError::Closed(value)) => Err(TrySendError::Closed(value)),
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
    pending: BTreeMap<MessageKey, T>,
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
) -> (LatestSender<T>, LatestReceiver<T>) {
    let shared = Arc::new(LatestShared {
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
                *slot = value;
                LatestSendOutcome::Replaced
            } else {
                if state.pending.len() >= self.shared.capacity {
                    let mut measurements = lock_unpoisoned(&self.shared.stats);
                    measurements.latest_rejected = measurements.latest_rejected.saturating_add(1);
                    drop(measurements);
                    return Err(LatestSendError::Full(value));
                }
                state.pending.insert(key, value);
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
        let item = lock_unpoisoned(&self.shared.state).pending.pop_first();
        if item.is_some() {
            let depth = lock_unpoisoned(&self.shared.state).pending.len();
            lock_unpoisoned(&self.shared.stats).latest_depth = depth;
        }
        item
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
