//! Typed public API of the bmux recording plugin.
//!
//! Unlike most plugin-api crates, this one does not use BPDL codegen.
//! Recording operations carry rich typed payloads (`RecordingProfile`,
//! `RecordingEventKind`, `RecordingRollingStartOptions`, etc.) that
//! already live in `bmux_ipc`, and duplicating them into a BPDL schema
//! would be noise. Instead this crate exposes:
//!
//! - [`RecordingRequest`] / [`RecordingResponse`] — hand-written wire
//!   enums the recording plugin's typed service dispatches over.
//! - [`RollingRecordingSettings`] — normalized rolling-recording
//!   configuration (window-secs + event kinds); registered into the
//!   plugin state registry by CLI startup so the recording plugin can
//!   read it during `activate`.
//! - [`RecordingPluginConfig`] — recordings/rolling-recordings
//!   directory paths + segment size; registered by CLI startup.
//! - Constants for the interface id and capability ids.
//! - [`typed_client`] helpers for downstream callers (CLI, tests) that
//!   want to invoke recording operations through any
//!   `TypedDispatchClient` transport.
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
pub mod typed_client;

pub use offline_prune::prune_old_recordings;

use bmux_ipc::{
    RecordingEventKind, RecordingProfile, RecordingRollingStartOptions, RecordingStatus,
    RecordingSummary,
};
use bmux_plugin_sdk::{CapabilityId, InterfaceId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Canonical capability for the recording plugin's read surface.
pub const RECORDING_READ: CapabilityId = CapabilityId::from_static("bmux.recording.read");

/// Canonical capability for the recording plugin's write surface.
pub const RECORDING_WRITE: CapabilityId = CapabilityId::from_static("bmux.recording.write");

/// Interface id for recording control operations (typed dispatch).
pub const RECORDING_COMMANDS_INTERFACE: InterfaceId =
    InterfaceId::from_static("recording-commands");

/// Default rolling-recording configuration (window seconds + enabled
/// event kinds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollingRecordingSettings {
    pub window_secs: u64,
    pub event_kinds: Vec<RecordingEventKind>,
}

impl RollingRecordingSettings {
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.window_secs > 0 && !self.event_kinds.is_empty()
    }

    #[must_use]
    pub fn capture_input(&self) -> bool {
        self.event_kinds.contains(&RecordingEventKind::PaneInputRaw)
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

/// Typed request variants for the recording plugin's typed service
/// dispatch surface. Replaces the former `Request::Recording*`
/// variants that used to live on `bmux_ipc::Request` before the
/// recording plugin migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RecordingRequest {
    Start {
        #[serde(default)]
        session_id: Option<Uuid>,
        capture_input: bool,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        profile: Option<RecordingProfile>,
        #[serde(default)]
        event_kinds: Option<Vec<RecordingEventKind>>,
    },
    Stop {
        #[serde(default)]
        recording_id: Option<Uuid>,
    },
    Status,
    List,
    Delete {
        recording_id: Uuid,
    },
    WriteCustomEvent {
        #[serde(default)]
        session_id: Option<Uuid>,
        #[serde(default)]
        pane_id: Option<Uuid>,
        source: String,
        name: String,
        payload: Vec<u8>,
    },
    DeleteAll,
    Cut {
        #[serde(default)]
        last_seconds: Option<u64>,
        #[serde(default)]
        name: Option<String>,
    },
    RollingStart {
        #[serde(default)]
        options: RecordingRollingStartOptions,
    },
    RollingStop,
    RollingStatus,
    RollingClear {
        restart_if_active: bool,
    },
    CaptureTargets,
    Prune {
        #[serde(default)]
        older_than_days: Option<u64>,
    },
}

/// Typed response variants for the recording plugin's typed service
/// dispatch surface. Replaces the former
/// `ResponsePayload::Recording*` variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordingResponse {
    Started {
        recording: RecordingSummary,
    },
    Stopped {
        recording_id: Option<Uuid>,
    },
    Status {
        status: RecordingStatus,
    },
    List {
        recordings: Vec<RecordingSummary>,
    },
    Deleted {
        recording_id: Uuid,
    },
    CustomEventWritten {
        accepted: bool,
    },
    DeleteAll {
        removed_count: usize,
    },
    Cut {
        recording: RecordingSummary,
    },
    RollingStarted {
        recording: RecordingSummary,
    },
    RollingStopped {
        recording_id: Option<Uuid>,
    },
    RollingStatus {
        status: bmux_ipc::RecordingRollingStatus,
    },
    RollingCleared {
        report: bmux_ipc::RecordingRollingClearReport,
    },
    CaptureTargets {
        targets: Vec<bmux_ipc::RecordingCaptureTarget>,
    },
    Pruned {
        pruned_count: usize,
    },
}
