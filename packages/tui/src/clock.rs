//! Per-frame presentation time.
//!
//! A [`PresentationClock`] is one sampled instant shared by everything that
//! lays out and paints a single frame. Components read it through
//! [`LayoutCx::clock`](crate::component::LayoutCx::clock) and
//! [`PaintCx::clock`](crate::paint::PaintCx::clock) instead of reading a wall
//! clock, so animation phases, the cells they produce, and the hit regions and
//! selection fragments registered beside those cells all describe the same
//! moment.
//!
//! BMUX never samples time here. A presentation driver (for example the
//! `bmux_tui_runtime` presenter loop, which uses its runtime clock, or a test
//! harness with a manual clock) supplies the instant through
//! [`PresentationState::set_presentation_clock`](crate::presentation::PresentationState::set_presentation_clock)
//! or [`Terminal::set_presentation_clock`](crate::terminal::Terminal::set_presentation_clock)
//! before the frame is prepared. Frames prepared without one report `None`.

use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

/// One instant sampled for a complete frame.
///
/// The value is copied into every layout and paint context of the frame and
/// never changes while the frame is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PresentationClock {
    now: Instant,
}

impl PresentationClock {
    /// Create a clock for a frame presented at `now`.
    #[must_use]
    pub const fn new(now: Instant) -> Self {
        Self { now }
    }

    /// The frame's sampled instant.
    #[must_use]
    pub const fn now(self) -> Instant {
        self.now
    }

    /// Time from `earlier` to this frame, or zero when `earlier` is later.
    #[must_use]
    pub fn since(self, earlier: Instant) -> Duration {
        self.now.saturating_duration_since(earlier)
    }

    /// Zero-based animation step for a cycle of `period` per step, counted
    /// from `start`.
    ///
    /// Returns `0` before `start` and for a zero `period`, so a disabled or
    /// reduced-motion animation stays on its first frame.
    #[must_use]
    pub fn step(self, start: Instant, period: Duration) -> u64 {
        let period = period.as_nanos();
        if period == 0 {
            return 0;
        }
        u64::try_from(self.since(start).as_nanos() / period).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_counts_whole_periods_from_start() {
        let start = Instant::now();
        let clock = PresentationClock::new(start + Duration::from_millis(250));
        assert_eq!(clock.step(start, Duration::from_millis(100)), 2);
        assert_eq!(clock.step(start, Duration::ZERO), 0);
        assert_eq!(clock.since(start), Duration::from_millis(250));
    }

    #[test]
    fn instants_before_start_saturate_to_zero() {
        let start = Instant::now();
        let clock = PresentationClock::new(start);
        let later = start + Duration::from_secs(1);
        assert_eq!(clock.since(later), Duration::ZERO);
        assert_eq!(clock.step(later, Duration::from_millis(10)), 0);
    }
}
