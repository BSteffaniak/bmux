//! Typed public API of the bmux recording plugin.
//!
//! Unlike most plugin-api crates, this one does not use BPDL codegen.
//! Recording operations carry rich typed payloads (`RecordingProfile`,
//! `RecordingEventKind`, `RecordingRollingStartOptions`, etc.) that
//! already live in `bmux_ipc`, and duplicating them into a BPDL schema
//! would be noise. Instead this crate exposes:
//!
//! - [`RecordingRuntime`] — the runtime type owned by the recording
//!   plugin; declared here so that `packages/server` can name the
//!   type for fast-path writes without depending on the plugin impl
//!   crate.
//! - [`DualRuntimeSink`] — [`bmux_recording_runtime::RecordingSink`]
//!   impl that fans out to both manual and rolling runtimes.
//! - [`RecordingRequest`] / [`RecordingResponse`] — hand-written wire
//!   enums the recording plugin's typed service dispatches over.
//! - Constants for the interface id and capability ids.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod recording_runtime;

pub use recording_runtime::{
    RecordingCutError, RecordingRuntime, cut_missing_active_recording_dir, prune_old_recordings,
};

use bmux_ipc::{
    RecordingEventKind, RecordingPayload, RecordingProfile, RecordingRollingStartOptions,
    RecordingStatus, RecordingSummary,
};
use bmux_plugin_sdk::{CapabilityId, InterfaceId};
use bmux_recording_runtime::{RecordMeta, RecordingSink};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Canonical capability for the recording plugin's read surface.
pub const RECORDING_READ: CapabilityId = CapabilityId::from_static("bmux.recording.read");

/// Canonical capability for the recording plugin's write surface.
pub const RECORDING_WRITE: CapabilityId = CapabilityId::from_static("bmux.recording.write");

/// Interface id for recording control operations (typed dispatch).
pub const RECORDING_COMMANDS_INTERFACE: InterfaceId =
    InterfaceId::from_static("recording-commands");

/// Newtype wrapper for registering the manual recording runtime handle
/// in [`bmux_plugin::PluginStateRegistry`]. Used by the recording
/// plugin's typed service handlers to perform lifecycle operations
/// (start/stop/list/etc.) on the manual recording runtime.
pub struct ManualRecordingRuntimeHandle(pub std::sync::Arc<std::sync::Mutex<RecordingRuntime>>);

/// Newtype wrapper for registering the rolling recording runtime
/// handle in [`bmux_plugin::PluginStateRegistry`]. The inner option
/// is `None` when rolling recording is disabled in config.
pub struct RollingRecordingRuntimeHandle(
    pub std::sync::Arc<std::sync::Mutex<Option<RecordingRuntime>>>,
);

/// Default rolling-recording configuration (window seconds + enabled
/// event kinds). Relocated from `packages/server` so plugin handlers
/// can use the same settings.
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

/// Server-provided configuration values needed by the recording
/// plugin's rolling/cut/write handlers. Registered into
/// `PluginStateRegistry` by `BmuxServer::new` so the plugin can reach
/// them without depending on `packages/server`.
#[derive(Debug, Clone)]
pub struct RecordingPluginConfig {
    /// Root directory for the (non-rolling) manual recordings.
    pub recordings_dir: std::path::PathBuf,
    /// Root directory for rolling-recording buffers.
    pub rolling_recordings_dir: std::path::PathBuf,
    /// Segment size in MB for rolling recording buffers.
    pub rolling_segment_mb: usize,
    /// Default rolling-recording settings (window + event kinds).
    pub rolling_defaults: RollingRecordingSettings,
}

/// `RecordingSink` impl that fans out each record to both a manual
/// and a rolling `RecordingRuntime` handle.
///
/// Lives here (in `bmux_recording_plugin_api`) rather than in the
/// plugin impl crate so that `packages/server` can construct the sink
/// at server-construction time (when it has the config it needs to
/// create the runtimes) without depending on the plugin impl crate.
pub struct DualRuntimeSink {
    manual: std::sync::Arc<std::sync::Mutex<RecordingRuntime>>,
    rolling: std::sync::Arc<std::sync::Mutex<Option<RecordingRuntime>>>,
}

impl DualRuntimeSink {
    #[must_use]
    pub const fn new(
        manual: std::sync::Arc<std::sync::Mutex<RecordingRuntime>>,
        rolling: std::sync::Arc<std::sync::Mutex<Option<RecordingRuntime>>>,
    ) -> Self {
        Self { manual, rolling }
    }
}

impl RecordingSink for DualRuntimeSink {
    fn record(&self, kind: RecordingEventKind, payload: RecordingPayload, meta: RecordMeta) {
        if let Ok(runtime) = self.manual.lock() {
            let _ = runtime.record(kind, payload.clone(), meta);
        }
        if let Ok(runtime) = self.rolling.lock()
            && let Some(runtime) = runtime.as_ref()
        {
            let _ = runtime.record(kind, payload, meta);
        }
    }
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
