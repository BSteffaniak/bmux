//! Layout timings are diagnostics only: no routing or layout decisions use them.
//! wasm32-unknown-unknown has no clock. Preserve native timing unchanged, while
//! portable guest diagnostics report zero instead of trapping in Instant::now.

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) use std::time::Instant;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) struct Instant;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
impl Instant {
    pub(crate) fn now() -> Self {
        Self
    }

    pub(crate) fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}
