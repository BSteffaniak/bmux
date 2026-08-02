//! Runtime configuration.

use std::time::Duration;

/// Bounded scheduling and presentation configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Maximum reliable application messages waiting for delivery.
    pub reliable_capacity: usize,
    /// Maximum terminal events waiting for delivery.
    pub terminal_capacity: usize,
    /// Maximum distinct latest-value keys waiting for delivery.
    pub latest_capacity: usize,
    /// Maximum messages waiting from long-lived subscriptions.
    pub subscription_capacity: usize,
    /// Maximum concurrently active long-lived subscriptions.
    pub max_active_subscriptions: usize,
    /// Maximum messages processed before yielding a scheduler turn.
    pub messages_per_turn: usize,
    /// Maximum wall time spent processing messages before yielding.
    pub processing_time_per_turn: Duration,
    /// Minimum interval between completed presentations, or `None` for no limit.
    pub frame_interval: Option<Duration>,
    /// Maximum number of active command tasks.
    pub max_active_commands: usize,
    /// Maximum number of queued keyed commands.
    pub max_queued_commands: usize,
}

impl RuntimeConfig {
    /// Validate and normalize runtime bounds.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            reliable_capacity: self.reliable_capacity.max(1),
            terminal_capacity: self.terminal_capacity.max(1),
            latest_capacity: self.latest_capacity.max(1),
            subscription_capacity: self.subscription_capacity.max(1),
            max_active_subscriptions: self.max_active_subscriptions.max(1),
            messages_per_turn: self.messages_per_turn.max(1),
            processing_time_per_turn: self.processing_time_per_turn.max(Duration::from_micros(1)),
            frame_interval: self.frame_interval,
            max_active_commands: self.max_active_commands.max(1),
            max_queued_commands: self.max_queued_commands.max(1),
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            reliable_capacity: 256,
            terminal_capacity: 64,
            latest_capacity: 128,
            subscription_capacity: 256,
            max_active_subscriptions: 32,
            messages_per_turn: 64,
            processing_time_per_turn: Duration::from_millis(4),
            frame_interval: Some(Duration::from_secs_f64(1.0 / 60.0)),
            max_active_commands: 64,
            max_queued_commands: 256,
        }
    }
}
