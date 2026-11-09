//! Runtime time source.
//!
//! Production runtimes read the monotonic system clock. Deterministic test
//! harnesses (the `testing` feature) substitute a manually advanced clock so
//! timer deadlines and presentation cadence never depend on wall-clock time.

use std::time::Instant;

#[cfg(any(test, feature = "testing"))]
use std::sync::{Arc, Mutex};
#[cfg(any(test, feature = "testing"))]
use std::time::Duration;

/// Time source consulted by runtime scheduling decisions.
#[derive(Debug, Clone, Default)]
pub enum RuntimeClock {
    /// Monotonic operating-system clock.
    #[default]
    System,
    /// Manually advanced deterministic clock.
    #[cfg(any(test, feature = "testing"))]
    Manual(ManualClock),
}

impl RuntimeClock {
    /// Return the current instant of this time source.
    #[inline]
    pub fn now(&self) -> Instant {
        match self {
            Self::System => Instant::now(),
            #[cfg(any(test, feature = "testing"))]
            Self::Manual(clock) => clock.now(),
        }
    }
}

/// Shared, manually advanced monotonic clock.
///
/// The clock captures one anchor instant at construction and afterwards only
/// moves when [`ManualClock::advance`] is called; it never reads wall-clock
/// time again. Clones observe and advance the same time.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Clone)]
pub struct ManualClock {
    state: Arc<Mutex<ManualClockState>>,
}

#[cfg(any(test, feature = "testing"))]
#[derive(Debug)]
struct ManualClockState {
    anchor: Instant,
    elapsed: Duration,
}

#[cfg(any(test, feature = "testing"))]
impl ManualClock {
    /// Create a clock positioned at its anchor instant.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ManualClockState {
                anchor: Instant::now(),
                elapsed: Duration::ZERO,
            })),
        }
    }

    /// Return the current manual instant.
    #[must_use]
    pub fn now(&self) -> Instant {
        let state = self.lock();
        state.anchor + state.elapsed
    }

    /// Return the total time advanced since construction.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.lock().elapsed
    }

    /// Advance the clock by `delta`.
    pub fn advance(&self, delta: Duration) {
        let mut state = self.lock();
        state.elapsed = state.elapsed.saturating_add(delta);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ManualClockState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(any(test, feature = "testing"))]
impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}
