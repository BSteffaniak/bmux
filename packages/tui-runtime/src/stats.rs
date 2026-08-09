//! Domain-neutral runtime measurements.

/// Snapshot of runtime admission, scheduling, command, timer, and presentation measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeStats {
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
