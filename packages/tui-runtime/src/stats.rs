//! Domain-neutral runtime measurements.

use std::time::{Duration, Instant};

/// Fixed duration distribution. Bucket upper bounds in microseconds are
/// 100, 500, `1_000`, `4_000`, `16_000`, `64_000`, `256_000`, and infinity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DurationHistogram {
    /// Saturating observation counts, including zero-duration observations.
    pub buckets: [u64; 8],
    /// Saturating sum of observed microseconds.
    pub sum_us: u64,
    /// Largest observed duration, in microseconds.
    pub max_us: u64,
}

impl DurationHistogram {
    pub(crate) fn record(&mut self, duration: Duration) {
        let us = micros(duration);
        let bucket =
            [100, 500, 1_000, 4_000, 16_000, 64_000, 256_000].partition_point(|bound| *bound < us);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }
}

/// Source of an admitted item. Capacity wait is not queue age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueSource {
    /// Reliable terminal events.
    TerminalFifo,
    /// Coalesced terminal events; age starts at the latest replacement.
    TerminalLatest,
    /// Reliable application messages.
    ApplicationFifo,
    /// Coalesced application messages.
    ApplicationLatest,
    /// Subscription messages.
    Subscription,
    /// Completed commands, measured from future readiness to collection.
    /// Includes collected stale completions. Panicked and aborted tasks produce
    /// no completion-age observation.
    CommandCompletion,
}

impl QueueSource {
    const fn index(self) -> usize {
        match self {
            Self::TerminalFifo => 0,
            Self::TerminalLatest => 1,
            Self::ApplicationFifo => 2,
            Self::ApplicationLatest => 3,
            Self::Subscription => 4,
            Self::CommandCompletion => 5,
        }
    }
}

/// Terminal dispatches since the preceding presentation attempt (successful or
/// failed). Diagnostic window only: not causality, input admission, or visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalDispatchRange {
    /// Correlation is invalid, including after ordinal overflow.
    Unknown,
    /// No terminal dispatch occurred in this window.
    #[default]
    Empty,
    /// Inclusive runtime-local terminal dispatch ordinals, starting at one.
    Span {
        /// First dispatch in the window.
        first: u64,
        /// Last dispatch in the window.
        last: u64,
    },
}

/// Latest presentation attempt; failed attempts consume the diagnostic window
/// but do not commit output or advance interaction authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationSample {
    /// Attempt ordinal, starting at one; saturates on overflow.
    pub attempt: u64,
    /// Monotonic microseconds since runtime construction.
    pub started_us: u64,
    /// Presenter call duration, including failed calls.
    pub elapsed_us: u64,
    /// Whether the presenter returned success, not whether pixels were visible.
    pub succeeded: bool,
    /// Dispatch window ending immediately before this presenter call.
    pub terminal_dispatches: TerminalDispatchRange,
}

pub(crate) fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// Timestamp travels inside the existing bounded mailbox, never in a side queue.
#[derive(Debug)]
pub(crate) struct Admitted<T> {
    pub value: T,
    pub at: Option<Instant>,
}

impl<T> Admitted<T> {
    pub(crate) fn new(value: T, detailed: bool) -> Self {
        Self {
            value,
            at: detailed.then(Instant::now),
        }
    }
}

/// Snapshot of runtime admission, scheduling, command, timer, and presentation measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeStats {
    /// Opt-in diagnostic measurements. `None` means unavailable, not zero.
    pub detailed: Option<DetailedRuntimeStats>,
    /// Current reliable application queue depth.
    pub reliable_depth: usize,
    /// Highest observed reliable application queue depth.
    pub reliable_high_water: usize,
    /// Current terminal queue depth.
    pub terminal_depth: usize,
    /// Highest observed terminal queue depth.
    pub terminal_high_water: usize,
    /// Current latest-value key count.
    pub latest_depth: usize,
    /// Highest observed latest-value key count.
    pub latest_high_water: usize,
    /// Reliable `try_send` calls rejected because admission was full.
    pub reliable_rejected: u64,
    /// Terminal `try_send` calls rejected because admission was full.
    pub terminal_rejected: u64,
    /// Latest-value sends rejected because key admission was full.
    pub latest_rejected: u64,
    /// Pending latest values replaced before delivery.
    pub latest_replaced: u64,
    /// Reliable application messages delivered to the program.
    pub reliable_processed: u64,
    /// Terminal events delivered to the program.
    pub terminal_processed: u64,
    /// Latest-value messages delivered to the program.
    pub latest_processed: u64,
    /// Subscription messages currently waiting for delivery.
    pub subscription_depth: usize,
    /// Highest observed subscription message depth.
    pub subscription_high_water: usize,
    /// Subscription sends rejected because the runtime was closed.
    pub subscription_rejected: u64,
    /// Subscriptions started or replaced.
    pub subscriptions_started: u64,
    /// Subscriptions cancelled by replacement, explicit cancellation, or shutdown.
    pub subscriptions_cancelled: u64,
    /// Subscriptions that completed normally.
    pub subscriptions_completed: u64,
    /// Timer events delivered to the program.
    pub timers_delivered: u64,
    /// Scheduler turns that exhausted a count or wall-time budget.
    pub scheduler_budget_exhausted: u64,
    /// Redraw requests observed from program updates or handles.
    pub redraw_requests: u64,
    /// Redraw requests absorbed while already dirty.
    pub redraw_coalesced: u64,
    /// Frames successfully presented.
    pub frames_presented: u64,
    /// Successful presentations that repainted the complete terminal surface.
    pub full_repaints: u64,
    /// Total terminal cells reported changed by successful presentations.
    pub presented_changed_cells: u64,
    /// Total presentation scheduling delay in microseconds.
    pub presentation_delay_us: u64,
    /// Total time spent in successful presenter calls, in microseconds.
    pub presentation_time_us: u64,
    /// Program update calls completed successfully.
    pub updates_completed: u64,
    /// Total time spent in successful program update calls, in microseconds.
    pub update_time_us: u64,
    /// Commands started by the runtime.
    pub commands_started: u64,
    /// Commands queued behind keyed work.
    pub commands_queued: u64,
    /// Commands rejected by configured bounds.
    pub commands_rejected: u64,
    /// Active commands aborted by replacement, cancellation, or shutdown.
    pub commands_cancelled: u64,
    /// Command completions suppressed because their generation was stale.
    pub stale_command_completions: u64,
}

/// Fixed-size opt-in timing and presentation diagnostics.
/// Histogram percentiles identify bucket bounds, not exact quantiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DetailedRuntimeStats {
    queue_age: [DurationHistogram; 6],
    /// Deadline-to-delivery delay for timers; not mailbox queue age.
    pub timer_lateness: DurationHistogram,
    /// Durations of all update calls that return, successful and failed.
    /// Panics are not recorded.
    pub update_duration: DurationHistogram,
    /// Durations of all presenter calls that return, successful and failed.
    /// Panics are not recorded.
    pub present_duration: DurationHistogram,
    /// Positive scheduler-turn excess over the configured processing budget.
    /// Presentation is excluded; failed update turns are included.
    pub turn_over_budget: DurationHistogram,
    /// Failed program update calls.
    pub updates_failed: u64,
    /// Failed presenter calls.
    pub presentations_failed: u64,
    /// Only the most recent attempt is retained; no history is stored.
    pub last_presentation: Option<PresentationSample>,
}

impl RuntimeStats {
    /// Queue-age distribution; unavailable when detailed telemetry is disabled.
    /// An enabled empty histogram means no observations, not measured zero age.
    #[must_use]
    pub const fn queue_age(&self, source: QueueSource) -> Option<&DurationHistogram> {
        match &self.detailed {
            Some(detailed) => Some(&detailed.queue_age[source.index()]),
            None => None,
        }
    }

    pub(crate) fn new(collect_detailed: bool) -> Self {
        Self {
            detailed: collect_detailed.then(DetailedRuntimeStats::default),
            ..Self::default()
        }
    }

    pub(crate) fn record_queue_age(&mut self, source: QueueSource, at: Instant, now: Instant) {
        if let Some(detailed) = &mut self.detailed {
            detailed.queue_age[source.index()].record(now.saturating_duration_since(at));
        }
    }

    pub(crate) fn record_update(&mut self, elapsed: Duration, succeeded: bool) {
        if let Some(detailed) = &mut self.detailed {
            detailed.update_duration.record(elapsed);
            if !succeeded {
                detailed.updates_failed = detailed.updates_failed.saturating_add(1);
            }
        }
        if succeeded {
            self.updates_completed = self.updates_completed.saturating_add(1);
            self.update_time_us = self.update_time_us.saturating_add(micros(elapsed));
        }
    }

    pub(crate) fn record_turn(&mut self, elapsed: Duration, budget: Duration) {
        if let Some(detailed) = &mut self.detailed
            && elapsed > budget
        {
            detailed
                .turn_over_budget
                .record(elapsed.saturating_sub(budget));
        }
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn histogram_boundaries_and_saturation_are_exact() {
        let mut histogram = DurationHistogram::default();
        for us in [
            0, 100, 101, 500, 501, 1_000, 1_001, 4_000, 4_001, 16_000, 16_001, 64_000, 64_001,
            256_000, 256_001,
        ] {
            histogram.record(Duration::from_micros(us));
        }
        assert_eq!(histogram.buckets, [2, 2, 2, 2, 2, 2, 2, 1]);
        assert_eq!(histogram.max_us, 256_001);
        histogram.buckets[7] = u64::MAX;
        histogram.sum_us = u64::MAX;
        histogram.record(Duration::MAX);
        assert_eq!(histogram.buckets[7], u64::MAX);
        assert_eq!(histogram.sum_us, u64::MAX);
        assert_eq!(histogram.max_us, u64::MAX);
    }

    #[test]
    fn synthetic_time_separates_sources_and_positive_budget_excess() {
        let mut stats = RuntimeStats::new(true);
        let at = Instant::now();
        let now = at + Duration::from_micros(501);
        stats.record_queue_age(QueueSource::TerminalFifo, at, now);
        assert_eq!(
            stats.queue_age(QueueSource::TerminalFifo).unwrap().buckets[2],
            1
        );
        assert_eq!(
            stats
                .queue_age(QueueSource::ApplicationFifo)
                .unwrap()
                .sum_us,
            0
        );
        assert_eq!(
            stats.detailed.as_ref().unwrap().timer_lateness,
            DurationHistogram::default()
        );
        stats.record_update(Duration::from_micros(501), false);
        stats.record_update(Duration::from_micros(100), true);
        assert_eq!(stats.detailed.as_ref().unwrap().update_duration.sum_us, 601);
        assert_eq!(stats.update_time_us, 100);
        assert_eq!(stats.detailed.as_ref().unwrap().updates_failed, 1);
        assert_eq!(stats.updates_completed, 1);
        stats.record_turn(Duration::from_millis(4), Duration::from_millis(4));
        assert_eq!(
            stats.detailed.as_ref().unwrap().turn_over_budget,
            DurationHistogram::default()
        );
        stats.record_turn(Duration::from_micros(4_101), Duration::from_millis(4));
        assert_eq!(
            stats.detailed.as_ref().unwrap().turn_over_budget.buckets[1],
            1
        );
        assert_eq!(
            stats.detailed.as_ref().unwrap().turn_over_budget.sum_us,
            101
        );
    }
}
