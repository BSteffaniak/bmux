//! Shared playbook domain models and real execution runtime.
//!
//! The executor owns action dispatch, scheduling, recording and sandbox lifecycle.
//! Applications supply required primitive adapters and retain terminal/signals and
//! process selection. This domain library never depends on `bmux_cli`.
pub mod diff;
#[cfg(feature = "runtime")]
pub mod display_track;
#[cfg(feature = "runtime")]
pub mod engine;
mod escape;
#[cfg(feature = "runtime")]
pub mod host;
#[cfg(feature = "runtime")]
pub mod managed;
pub mod parse_dsl;
pub mod parse_toml;
pub mod parser;
#[cfg(feature = "runtime")]
pub mod sandbox;
#[cfg(feature = "runtime")]
pub mod screen;
pub mod subst;
#[cfg(feature = "runtime")]
pub mod transport;
pub mod types;

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    pub interactive: bool,
}

/// Execute a real playbook using caller-owned infrastructure.
///
/// Dropping this future cancels execution and triggers sandbox emergency cleanup.
/// Signal policy is deliberately left to the caller.
/// # Errors
/// Returns execution, transport, admission or cleanup failures.
#[cfg(feature = "runtime")]
pub async fn run(
    playbook: types::Playbook,
    target_server: bool,
    options: RunOptions,
    host: std::sync::Arc<dyn host::HostAdapter>,
) -> anyhow::Result<types::PlaybookResult> {
    engine::run_playbook(playbook, target_server, options, host).await
}
