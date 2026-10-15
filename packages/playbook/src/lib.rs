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
pub mod managed_display;
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

#[cfg(feature = "runtime")]
mod pacing;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlaybackSpeed {
    #[default]
    Instant,
    Half,
    Normal,
    Double,
}

impl PlaybackSpeed {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "instant" => Some(Self::Instant),
            "half" => Some(Self::Half),
            "normal" => Some(Self::Normal),
            "double" => Some(Self::Double),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instant => "instant",
            Self::Half => "half",
            Self::Normal => "normal",
            Self::Double => "double",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    pub interactive: bool,
    pub playback_speed: PlaybackSpeed,
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
