//! Opt-in, bounded queue diagnostics. No logging, IO, or event payload ownership.
//!
//! Integration: reserve channel capacity before `Attempt::admit`, then immediately
//! send an envelope holding the returned `QueueTicket`. Call `dequeued` when the
//! receiver removes it, not after event processing. Dropping an unsent/undelivered
//! ticket accounts a discard. This orders admission before any concurrent dequeue.
//! A cancelled reservation drops its attempt and records a cancellation instead.
//! Snapshots are cumulative counters plus percentiles of the latest 256 samples;
//! they are diagnostics, never queue admission or output-acknowledgement authority.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SAMPLE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueLatencySnapshot {
    pub samples: usize,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
}

/// Payload bytes are caller-supplied logical lengths, not capacities, resident
/// allocations, or inline event size. All counters saturate rather than wrap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueMetricsSnapshot {
    pub enabled: bool,
    pub admitted: u64,
    pub dequeued: u64,
    pub rejected_full: u64,
    pub rejected_closed: u64,
    pub rejected_cancelled: u64,
    pub discarded: u64,
    pub pending: u64,
    pub pending_high_water: u64,
    pub payload_bytes: u64,
    pub payload_bytes_high_water: u64,
    /// Completed admission attempts, including rejection/cancellation waits.
    pub admission_wait: QueueLatencySnapshot,
    /// Successfully dequeued envelopes only; discards are not residence samples.
    pub queue_residence: QueueLatencySnapshot,
}

#[derive(Clone, Copy)]
pub enum Rejection {
    Full,
    Closed,
    Cancelled,
}

/// Clones refer to the same queue. A disabled instance allocates nothing and
/// takes no clocks or locks on begin/admit/dequeue/drop/snapshot paths.
#[derive(Clone, Default)]
pub struct QueueMetrics(Option<Arc<Mutex<State>>>);

impl QueueMetrics {
    /// Read once at queue construction, matching the existing opt-in switches.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os("BTERM_RENDER_TELEMETRY").is_some_and(|value| value == "1")
                || std::env::var_os("BMUX_ATTACH_PHASE_TIMING").is_some(),
        )
    }

    fn new(enabled: bool) -> Self {
        Self(enabled.then(|| Arc::new(Mutex::new(State::default()))))
    }

    pub fn begin_attempt(&self) -> Attempt {
        Attempt {
            metrics: self.clone(),
            started: self.0.as_ref().map(|_| Instant::now()),
        }
    }

    pub fn snapshot(&self) -> QueueMetricsSnapshot {
        let Some(state) = &self.0 else {
            return QueueMetricsSnapshot::default();
        };
        // Release the producer lock before bounded percentile sorting.
        let state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        QueueMetricsSnapshot {
            enabled: true,
            admission_wait: state.admission_wait.snapshot(),
            queue_residence: state.queue_residence.snapshot(),
            ..state.counters
        }
    }

    fn update(&self, update: impl FnOnce(&mut State)) {
        if let Some(state) = &self.0 {
            update(
                &mut state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
    }
}

/// One reservation attempt. Drop means cancellation before admission. Not Clone.
pub struct Attempt {
    metrics: QueueMetrics,
    started: Option<Instant>,
}

impl Attempt {
    /// Call after successful `reserve`/`try_reserve` and before publishing the event.
    /// Include only known logical payload length (e.g. `text.len()`); use zero for
    /// events with no variable payload. Do not claim allocator-wide accounting.
    pub fn admit(mut self, payload_bytes: usize) -> QueueTicket {
        let elapsed = self.started.take().map(|started| started.elapsed());
        self.admit_after(elapsed, payload_bytes)
    }

    fn admit_after(&self, elapsed: Option<Duration>, payload_bytes: usize) -> QueueTicket {
        let payload_bytes = u64::try_from(payload_bytes).unwrap_or(u64::MAX);
        self.metrics.update(|state| {
            if let Some(elapsed) = elapsed {
                state.admission_wait.push(elapsed);
            }
            state.counters.admitted = state.counters.admitted.saturating_add(1);
            state.counters.pending = state.counters.pending.saturating_add(1);
            state.counters.payload_bytes =
                state.counters.payload_bytes.saturating_add(payload_bytes);
            state.counters.pending_high_water = state
                .counters
                .pending_high_water
                .max(state.counters.pending);
            state.counters.payload_bytes_high_water = state
                .counters
                .payload_bytes_high_water
                .max(state.counters.payload_bytes);
        });
        QueueTicket {
            metrics: self.metrics.clone(),
            enqueued: self.metrics.0.as_ref().map(|_| Instant::now()),
            payload_bytes,
            active: true,
        }
    }

    pub fn reject(mut self, reason: Rejection) {
        if let Some(started) = self.started.take() {
            self.reject_after(started.elapsed(), reason);
        }
    }

    fn reject_after(&self, elapsed: Duration, reason: Rejection) {
        self.metrics.update(|state| {
            state.admission_wait.push(elapsed);
            let counter = match reason {
                Rejection::Full => &mut state.counters.rejected_full,
                Rejection::Closed => &mut state.counters.rejected_closed,
                Rejection::Cancelled => &mut state.counters.rejected_cancelled,
            };
            *counter = counter.saturating_add(1);
        });
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if let Some(started) = self.started.take() {
            self.reject_after(started.elapsed(), Rejection::Cancelled);
        }
    }
}

/// Exactly one pending envelope, including the brief admitted-before-send span.
/// Move with the event. Drop handles send failure or receiver teardown without
/// depending on producer/consumer scheduling or a separate decrement call.
pub struct QueueTicket {
    metrics: QueueMetrics,
    enqueued: Option<Instant>,
    payload_bytes: u64,
    active: bool,
}

impl QueueTicket {
    pub fn dequeued(mut self) {
        let residence = self.enqueued.map(|enqueued| enqueued.elapsed());
        self.finish(residence, true);
    }

    fn finish(&mut self, residence: Option<Duration>, dequeued: bool) {
        if !self.active {
            return;
        }
        self.active = false;
        self.metrics.update(|state| {
            state.counters.pending = state.counters.pending.saturating_sub(1);
            state.counters.payload_bytes = state
                .counters
                .payload_bytes
                .saturating_sub(self.payload_bytes);
            if dequeued {
                state.counters.dequeued = state.counters.dequeued.saturating_add(1);
                if let Some(residence) = residence {
                    state.queue_residence.push(residence);
                }
            } else {
                state.counters.discarded = state.counters.discarded.saturating_add(1);
            }
        });
    }
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.finish(None, false);
    }
}

#[derive(Clone, Default)]
struct State {
    counters: QueueMetricsSnapshot,
    admission_wait: Samples,
    queue_residence: Samples,
}

#[derive(Clone)]
struct Samples {
    values: [u64; SAMPLE_CAPACITY],
    len: usize,
    next: usize,
}

impl Default for Samples {
    fn default() -> Self {
        Self {
            values: [0; SAMPLE_CAPACITY],
            len: 0,
            next: 0,
        }
    }
}

impl Samples {
    fn push(&mut self, duration: Duration) {
        self.values[self.next] = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.next = (self.next + 1) % SAMPLE_CAPACITY;
        self.len = (self.len + 1).min(SAMPLE_CAPACITY);
    }

    fn snapshot(&self) -> QueueLatencySnapshot {
        if self.len == 0 {
            return QueueLatencySnapshot::default();
        }
        let mut values = self.values;
        values[..self.len].sort_unstable();
        // Nearest rank, including zero-duration samples; no interpolation.
        let percentile = |percent: usize| values[(self.len * percent).div_ceil(100) - 1];
        QueueLatencySnapshot {
            samples: self.len,
            p50_ns: percentile(50),
            p95_ns: percentile(95),
            p99_ns: percentile(99),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admitted(metrics: &QueueMetrics, wait_ns: u64, bytes: usize) -> QueueTicket {
        // Explicit durations isolate accounting from wall-clock scheduling.
        let attempt = Attempt {
            metrics: metrics.clone(),
            started: None,
        };
        attempt.admit_after(Some(Duration::from_nanos(wait_ns)), bytes)
    }

    #[test]
    fn percentiles_keep_only_latest_bounded_samples() {
        let mut samples = Samples::default();
        assert_eq!(samples.snapshot(), QueueLatencySnapshot::default());
        for ns in 0..512 {
            samples.push(Duration::from_nanos(ns));
        }
        assert_eq!(
            samples.snapshot(),
            QueueLatencySnapshot {
                samples: 256,
                p50_ns: 383,
                p95_ns: 499,
                p99_ns: 509,
            }
        );
    }

    #[test]
    fn admission_dequeue_and_discard_are_exactly_once() {
        let metrics = QueueMetrics::new(true);
        let mut first = admitted(&metrics, 10, 7);
        let second = admitted(&metrics, 20, 11);
        let pending = metrics.snapshot();
        assert_eq!((pending.pending, pending.payload_bytes), (2, 18));
        first.finish(Some(Duration::from_nanos(30)), true);
        drop(first);
        drop(second);
        let result = metrics.snapshot();
        assert_eq!(
            (result.admitted, result.dequeued, result.discarded),
            (2, 1, 1)
        );
        assert_eq!((result.pending, result.payload_bytes), (0, 0));
        assert_eq!(
            (result.pending_high_water, result.payload_bytes_high_water),
            (2, 18)
        );
        assert_eq!(result.admission_wait.p50_ns, 10);
        assert_eq!(result.admission_wait.p99_ns, 20);
        assert_eq!(result.queue_residence.p95_ns, 30);
        assert_eq!(result.queue_residence.samples, 1);
    }

    #[test]
    fn rejection_reasons_do_not_admit_payloads() {
        let metrics = QueueMetrics::new(true);
        let attempt = Attempt {
            metrics: metrics.clone(),
            started: None,
        };
        for reason in [Rejection::Full, Rejection::Closed, Rejection::Cancelled] {
            attempt.reject_after(Duration::ZERO, reason);
        }
        let result = metrics.snapshot();
        assert_eq!(
            (
                result.rejected_full,
                result.rejected_closed,
                result.rejected_cancelled,
            ),
            (1, 1, 1)
        );
        assert_eq!(result.admitted, 0);
        assert_eq!(result.pending, 0);
        assert_eq!(result.admission_wait.samples, 3);
        assert_eq!(result.admission_wait.p99_ns, 0);
    }

    #[test]
    fn disabled_attempts_and_tickets_have_no_timestamps_or_stats() {
        let metrics = QueueMetrics::new(false);
        let attempt = metrics.begin_attempt();
        assert!(attempt.started.is_none());
        let ticket = attempt.admit(99);
        assert!(ticket.enqueued.is_none());
        ticket.dequeued();
        metrics.begin_attempt().reject(Rejection::Full);
        drop(metrics.begin_attempt());
        assert_eq!(metrics.snapshot(), QueueMetricsSnapshot::default());
    }

    #[test]
    fn interleaved_producers_and_receiver_drop_balance_pending() {
        let metrics = QueueMetrics::new(true);
        let producer = metrics.clone();
        let mut delivered = admitted(&metrics, 1, 4);
        let queued = admitted(&producer, 2, 8);
        delivered.finish(Some(Duration::ZERO), true);
        let unsent = admitted(&producer, 3, 16);
        // Channel failure after admission, then receiver teardown.
        drop(unsent);
        drop(queued);
        let result = metrics.snapshot();
        assert_eq!(
            (result.admitted, result.dequeued, result.discarded),
            (3, 1, 2)
        );
        assert_eq!((result.pending, result.payload_bytes), (0, 0));
        assert_eq!(result.payload_bytes_high_water, 24);
    }
}
