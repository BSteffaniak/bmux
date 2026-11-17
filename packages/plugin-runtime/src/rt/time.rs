//! `sleep`, `timeout`, `interval` and `Instant` on the host clock.
//!
//! Bundled builds re-export Tokio's timers. Dynamic builds use host timers by
//! id through the installed table. Portable plugin code uses only the shared
//! subset: `sleep`, `sleep_until`, `timeout`, `timeout_at`, `interval`,
//! `interval_at`, `Sleep::{deadline, is_elapsed, reset}`, `Interval::{tick,
//! reset, period}`, `Timeout::{get_ref, get_mut, into_inner}`, `Elapsed`, and
//! `Instant::{now, elapsed, duration_since, checked_duration_since,
//! saturating_duration_since, checked_add, checked_sub}` plus `Duration`
//! arithmetic.

#[cfg(not(bmux_plugin_dylib))]
pub use tokio::time::{
    Instant, Interval, Sleep, Timeout, error::Elapsed, interval, interval_at, sleep, sleep_until,
    timeout, timeout_at,
};

#[cfg(bmux_plugin_dylib)]
pub use super::guest::time::{
    Elapsed, Instant, Interval, Sleep, Timeout, interval, interval_at, sleep, sleep_until, timeout,
    timeout_at,
};
