//! What the `bmux_pty` simulation shows in `bmux_simvar_tui`: the shared simulation telemetry
//! (see `sim_support`), plus this simulation's invariants.

pub use sim_support::{PtyPanel, PtyTelemetry, delivered, emit_now, failure, watch_run};

/// This simulation's own telemetry: its invariants' status.
#[must_use]
pub fn extra() -> Vec<std::sync::Arc<dyn std::any::Any + Send + Sync>> {
    vec![std::sync::Arc::new(crate::invariants())]
}
