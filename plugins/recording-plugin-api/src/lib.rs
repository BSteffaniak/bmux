//! Typed public API of the bmux recording plugin.
//!
//! The [`recording_types`], [`recording_state`], and
//! [`recording_commands`] modules are generated from
//! `bpdl/recording-plugin.bpdl` at compile time via the
//! [`bmux_plugin_schema_macros::schema!`] macro. This crate also exposes:
//!
//! - [`RollingRecordingSettings`] — normalized rolling-recording
//!   configuration (window-secs + event kinds); registered into the
//!   plugin state registry by CLI startup so the recording plugin can
//!   read it during `activate`.
//! - [`RecordingPluginConfig`] — recordings/rolling-recordings
//!   directory paths + segment size; registered by CLI startup.
//!
//! The `RecordingRuntime` concrete type + the `DualRuntimeSink`
//! fan-out impl + `ManualRecordingRuntimeHandle` /
//! `RollingRecordingRuntimeHandle` registry newtypes all live in the
//! plugin impl crate (`bmux_recording_plugin`) so the server never
//! names the concrete runtime type.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod offline_prune;

pub use bmux_recording_runtime::RollingRecordingSettings;
pub use offline_prune::prune_old_recordings;

bmux_plugin_schema_macros::schema! {
    source: "bpdl/recording-plugin.bpdl",
}

impl From<recording_types::RecordingProfile> for bmux_ipc::RecordingProfile {
    fn from(value: recording_types::RecordingProfile) -> Self {
        match value {
            recording_types::RecordingProfile::Full => Self::Full,
            recording_types::RecordingProfile::Functional => Self::Functional,
            recording_types::RecordingProfile::Visual => Self::Visual,
        }
    }
}

impl From<bmux_ipc::RecordingProfile> for recording_types::RecordingProfile {
    fn from(value: bmux_ipc::RecordingProfile) -> Self {
        match value {
            bmux_ipc::RecordingProfile::Full => Self::Full,
            bmux_ipc::RecordingProfile::Functional => Self::Functional,
            bmux_ipc::RecordingProfile::Visual => Self::Visual,
        }
    }
}

impl From<recording_types::RecordingEventKind> for bmux_ipc::RecordingEventKind {
    fn from(value: recording_types::RecordingEventKind) -> Self {
        match value {
            recording_types::RecordingEventKind::PaneInputRaw => Self::PaneInputRaw,
            recording_types::RecordingEventKind::PaneOutputRaw => Self::PaneOutputRaw,
            recording_types::RecordingEventKind::ProtocolReplyRaw => Self::ProtocolReplyRaw,
            recording_types::RecordingEventKind::PaneImage => Self::PaneImage,
            recording_types::RecordingEventKind::ServerEvent => Self::ServerEvent,
            recording_types::RecordingEventKind::RequestStart => Self::RequestStart,
            recording_types::RecordingEventKind::RequestDone => Self::RequestDone,
            recording_types::RecordingEventKind::RequestError => Self::RequestError,
            recording_types::RecordingEventKind::Custom => Self::Custom,
        }
    }
}

impl From<bmux_ipc::RecordingEventKind> for recording_types::RecordingEventKind {
    fn from(value: bmux_ipc::RecordingEventKind) -> Self {
        match value {
            bmux_ipc::RecordingEventKind::PaneInputRaw => Self::PaneInputRaw,
            bmux_ipc::RecordingEventKind::PaneOutputRaw => Self::PaneOutputRaw,
            bmux_ipc::RecordingEventKind::ProtocolReplyRaw => Self::ProtocolReplyRaw,
            bmux_ipc::RecordingEventKind::PaneImage => Self::PaneImage,
            bmux_ipc::RecordingEventKind::ServerEvent => Self::ServerEvent,
            bmux_ipc::RecordingEventKind::RequestStart => Self::RequestStart,
            bmux_ipc::RecordingEventKind::RequestDone => Self::RequestDone,
            bmux_ipc::RecordingEventKind::RequestError => Self::RequestError,
            bmux_ipc::RecordingEventKind::Custom => Self::Custom,
        }
    }
}

impl From<bmux_ipc::RecordingSummary> for recording_types::RecordingSummary {
    fn from(value: bmux_ipc::RecordingSummary) -> Self {
        Self {
            id: value.id,
            name: value.name,
            format_version: value.format_version,
            session_id: value.session_id,
            capture_input: value.capture_input,
            profile: value.profile.into(),
            event_kinds: value.event_kinds.into_iter().map(Into::into).collect(),
            started_epoch_ms: value.started_epoch_ms,
            ended_epoch_ms: value.ended_epoch_ms,
            event_count: value.event_count,
            payload_bytes: value.payload_bytes,
            path: value.path,
            segments: value.segments,
            total_segment_bytes: value.total_segment_bytes,
        }
    }
}

impl From<recording_types::RecordingSummary> for bmux_ipc::RecordingSummary {
    fn from(value: recording_types::RecordingSummary) -> Self {
        Self {
            id: value.id,
            name: value.name,
            format_version: value.format_version,
            session_id: value.session_id,
            capture_input: value.capture_input,
            profile: value.profile.into(),
            event_kinds: value.event_kinds.into_iter().map(Into::into).collect(),
            started_epoch_ms: value.started_epoch_ms,
            ended_epoch_ms: value.ended_epoch_ms,
            event_count: value.event_count,
            payload_bytes: value.payload_bytes,
            path: value.path,
            segments: value.segments,
            total_segment_bytes: value.total_segment_bytes,
        }
    }
}

impl From<bmux_ipc::RecordingStatus> for recording_types::RecordingStatus {
    fn from(value: bmux_ipc::RecordingStatus) -> Self {
        Self {
            active: value.active.map(Into::into),
            queue_len: u64::try_from(value.queue_len).unwrap_or(u64::MAX),
        }
    }
}

impl From<recording_types::RecordingStatus> for bmux_ipc::RecordingStatus {
    fn from(value: recording_types::RecordingStatus) -> Self {
        Self {
            active: value.active.map(Into::into),
            queue_len: usize::try_from(value.queue_len).unwrap_or(usize::MAX),
        }
    }
}

impl From<bmux_ipc::RecordingCaptureTarget> for recording_types::RecordingCaptureTarget {
    fn from(value: bmux_ipc::RecordingCaptureTarget) -> Self {
        Self {
            recording_id: value.recording_id,
            path: value.path,
            rolling_window_secs: value.rolling_window_secs,
        }
    }
}

impl From<recording_types::RecordingCaptureTarget> for bmux_ipc::RecordingCaptureTarget {
    fn from(value: recording_types::RecordingCaptureTarget) -> Self {
        Self {
            recording_id: value.recording_id,
            path: value.path,
            rolling_window_secs: value.rolling_window_secs,
        }
    }
}

impl From<recording_types::RecordingRollingStartOptions>
    for bmux_ipc::RecordingRollingStartOptions
{
    fn from(value: recording_types::RecordingRollingStartOptions) -> Self {
        Self {
            window_secs: value.window_secs,
            name: value.name,
            event_kinds: value
                .event_kinds
                .map(|kinds| kinds.into_iter().map(Into::into).collect()),
            capture_input: value.capture_input,
            capture_output: value.capture_output,
            capture_events: value.capture_events,
            capture_protocol_replies: value.capture_protocol_replies,
            capture_images: value.capture_images,
        }
    }
}

impl From<bmux_ipc::RecordingRollingStartOptions>
    for recording_types::RecordingRollingStartOptions
{
    fn from(value: bmux_ipc::RecordingRollingStartOptions) -> Self {
        Self {
            window_secs: value.window_secs,
            name: value.name,
            event_kinds: value
                .event_kinds
                .map(|kinds| kinds.into_iter().map(Into::into).collect()),
            capture_input: value.capture_input,
            capture_output: value.capture_output,
            capture_events: value.capture_events,
            capture_protocol_replies: value.capture_protocol_replies,
            capture_images: value.capture_images,
        }
    }
}

impl From<bmux_ipc::RecordingRollingUsage> for recording_types::RecordingRollingUsage {
    fn from(value: bmux_ipc::RecordingRollingUsage) -> Self {
        Self {
            bytes: value.bytes,
            files: value.files,
            directories: value.directories,
            recording_dirs: value.recording_dirs,
        }
    }
}

impl From<recording_types::RecordingRollingUsage> for bmux_ipc::RecordingRollingUsage {
    fn from(value: recording_types::RecordingRollingUsage) -> Self {
        Self {
            bytes: value.bytes,
            files: value.files,
            directories: value.directories,
            recording_dirs: value.recording_dirs,
        }
    }
}

impl From<bmux_ipc::RecordingRollingStatus> for recording_types::RecordingRollingStatus {
    fn from(value: bmux_ipc::RecordingRollingStatus) -> Self {
        Self {
            root_path: value.root_path,
            auto_start: value.auto_start,
            available: value.available,
            active: value.active.map(Into::into),
            rolling_window_secs: value.rolling_window_secs,
            event_kinds: value.event_kinds.into_iter().map(Into::into).collect(),
            usage: value.usage.into(),
        }
    }
}

impl From<recording_types::RecordingRollingStatus> for bmux_ipc::RecordingRollingStatus {
    fn from(value: recording_types::RecordingRollingStatus) -> Self {
        Self {
            root_path: value.root_path,
            auto_start: value.auto_start,
            available: value.available,
            active: value.active.map(Into::into),
            rolling_window_secs: value.rolling_window_secs,
            event_kinds: value.event_kinds.into_iter().map(Into::into).collect(),
            usage: value.usage.into(),
        }
    }
}

impl From<bmux_ipc::RecordingRollingClearReport> for recording_types::RecordingRollingClearReport {
    fn from(value: bmux_ipc::RecordingRollingClearReport) -> Self {
        Self {
            root_path: value.root_path,
            was_active: value.was_active,
            restarted: value.restarted,
            stopped_recording_id: value.stopped_recording_id,
            restarted_recording: value.restarted_recording.map(Into::into),
            usage_before: value.usage_before.into(),
            usage_after: value.usage_after.into(),
        }
    }
}

impl From<recording_types::RecordingRollingClearReport> for bmux_ipc::RecordingRollingClearReport {
    fn from(value: recording_types::RecordingRollingClearReport) -> Self {
        Self {
            root_path: value.root_path,
            was_active: value.was_active,
            restarted: value.restarted,
            stopped_recording_id: value.stopped_recording_id,
            restarted_recording: value.restarted_recording.map(Into::into),
            usage_before: value.usage_before.into(),
            usage_after: value.usage_after.into(),
        }
    }
}

/// CLI-provided configuration values needed by the recording plugin's
/// `activate` callback to construct manual + rolling runtimes.
/// Registered into `PluginStateRegistry` by CLI bootstrap (before
/// plugin activation) so the plugin can reach startup config without
/// depending on `packages/server` or CLI internals.
#[derive(Debug, Clone)]
pub struct RecordingPluginConfig {
    /// Root directory for the (non-rolling) manual recordings.
    pub recordings_dir: std::path::PathBuf,
    /// Root directory for rolling-recording buffers.
    pub rolling_recordings_dir: std::path::PathBuf,
    /// Segment size in MB for rolling recording buffers.
    pub rolling_segment_mb: usize,
    /// Retention cutoff in days for completed recordings (hourly prune
    /// loop owned by the recording plugin reads this).
    pub retention_days: u64,
    /// Default rolling-recording settings (window + event kinds).
    pub rolling_defaults: RollingRecordingSettings,
    /// Whether to auto-start a rolling recording on plugin activation.
    pub rolling_auto_start: bool,
}
