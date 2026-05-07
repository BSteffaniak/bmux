//! bmux recording plugin — typed recording lifecycle handlers.
//!
//! The plugin implements BPDL-generated typed services for
//! `recording-state` and `recording-commands`. Each operation reads the
//! manual / rolling runtime handles out of `PluginStateRegistry`,
//! performs the lifecycle operation, and returns the generated response.
//!
//! The plugin itself owns construction of both runtimes. During
//! `activate` it reads the CLI-provided [`RecordingPluginConfig`] from
//! the plugin state registry, constructs manual + rolling runtimes,
//! registers them + the fan-out sink, spawns the hourly prune loop,
//! and optionally auto-starts the rolling recording.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod recording_runtime;
pub use recording_runtime::{
    RecordingCutError, RecordingRuntime, cut_missing_active_recording_dir, prune_old_recordings,
};

use bmux_plugin::global_plugin_state_registry;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_recording_plugin_api::{
    ManualRecordingStartOptions, RecordingPluginConfig, recording_events, recording_types,
};
use bmux_recording_protocol::{
    RecordingCaptureTarget, RecordingEventKind, RecordingPayload as ProtocolRecordingPayload,
    RecordingProfile, RecordingRollingClearReport, RecordingRollingStartOptions,
    RecordingRollingStatus, RecordingRollingUsage, RecordingStatus, RecordingSummary,
};
use bmux_recording_runtime::{
    RecordMeta, RecordingSink, RecordingSinkHandle, RollingRecordingSettings,
};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

type RecordingPayload = ProtocolRecordingPayload<bmux_ipc::Event, bmux_ipc::ErrorCode>;

/// Newtype wrapper for registering the manual recording runtime handle
/// in [`bmux_plugin::PluginStateRegistry`]. Plugin-local domain type;
/// server never names it.
pub struct ManualRecordingRuntimeHandle(pub Arc<Mutex<RecordingRuntime>>);

/// Newtype wrapper for registering the rolling recording runtime
/// handle in [`bmux_plugin::PluginStateRegistry`]. The inner option
/// is `None` when rolling recording is disabled in config.
pub struct RollingRecordingRuntimeHandle(pub Arc<Mutex<Option<RecordingRuntime>>>);

#[derive(Debug, Clone, serde::Deserialize)]
struct StartRequest {
    session_id: Option<uuid::Uuid>,
    capture_input: bool,
    name: Option<String>,
    profile: Option<recording_types::RecordingProfile>,
    event_kinds: Option<Vec<recording_types::RecordingEventKind>>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StopRequest {
    recording_id: Option<uuid::Uuid>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct WriteCustomEventRequest {
    session_id: Option<uuid::Uuid>,
    pane_id: Option<uuid::Uuid>,
    source: String,
    name: String,
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    payload: Vec<u8>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct DeleteRequest {
    recording_id: uuid::Uuid,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CutRequest {
    last_seconds: Option<u64>,
    name: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RollingStartRequest {
    options: recording_types::RecordingRollingStartOptions,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RollingClearRequest {
    restart_if_active: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct PruneRequest {
    older_than_days: Option<u64>,
}

/// `RecordingSink` impl that fans out each record to both the manual
/// and rolling runtimes. Registered behind a
/// `bmux_recording_runtime::RecordingSinkHandle` in the plugin state
/// registry so server's hot-path pane-output writes reach both
/// runtimes without naming this plugin impl crate.
struct DualRuntimeSink {
    manual: Arc<Mutex<RecordingRuntime>>,
    rolling: Arc<Mutex<Option<RecordingRuntime>>>,
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

#[derive(Default)]
pub struct RecordingPlugin;

impl RustPlugin for RecordingPlugin {
    type Contract = bmux_recording_plugin_api::Contract;

    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        // Read CLI-provided plugin config; silently succeed without
        // constructing runtimes when missing so headless / test
        // deployments still load the plugin.
        let Some(config_handle) = global_plugin_state_registry().get::<RecordingPluginConfig>()
        else {
            return Ok(bmux_plugin_sdk::EXIT_OK);
        };
        bmux_plugin::global_event_bus()
            .register_channel::<recording_events::RecordingEvent>(recording_events::EVENT_KIND);
        let Ok(config) = config_handle.read() else {
            return Ok(bmux_plugin_sdk::EXIT_OK);
        };

        let recordings_dir = config.recordings_dir.clone();
        let rolling_recordings_dir = config.rolling_recordings_dir.clone();
        let rolling_segment_mb = config.rolling_segment_mb;
        let retention_days = config.retention_days;
        let rolling_defaults = config.rolling_defaults.clone();
        let rolling_auto_start = config.rolling_auto_start;
        let startup_recording = config.startup_recording.clone();
        drop(config);

        let manual_runtime = Arc::new(Mutex::new(RecordingRuntime::new(
            recordings_dir,
            rolling_segment_mb,
            retention_days,
        )));

        let rolling_runtime_available = rolling_defaults.is_available();
        let rolling_runtime = Arc::new(Mutex::new(if rolling_runtime_available {
            Some(RecordingRuntime::new_rolling(
                rolling_recordings_dir.clone(),
                rolling_segment_mb,
                rolling_defaults.window_secs,
            ))
        } else {
            None
        }));

        // Register the fan-out sink first so server can hot-path
        // record as soon as its first pane event fires.
        let sink: Arc<dyn RecordingSink> = Arc::new(DualRuntimeSink {
            manual: Arc::clone(&manual_runtime),
            rolling: Arc::clone(&rolling_runtime),
        });
        let sink_handle = Arc::new(RwLock::new(RecordingSinkHandle::from_arc(sink)));
        global_plugin_state_registry().register::<RecordingSinkHandle>(&sink_handle);

        // Register the lifecycle handles the plugin's own typed
        // handlers read on every request.
        let manual_handle = Arc::new(RwLock::new(ManualRecordingRuntimeHandle(Arc::clone(
            &manual_runtime,
        ))));
        global_plugin_state_registry().register::<ManualRecordingRuntimeHandle>(&manual_handle);

        let rolling_handle = Arc::new(RwLock::new(RollingRecordingRuntimeHandle(Arc::clone(
            &rolling_runtime,
        ))));
        global_plugin_state_registry().register::<RollingRecordingRuntimeHandle>(&rolling_handle);

        if let Some(options) = startup_recording.as_ref() {
            auto_start_manual(&manual_runtime, options);
        }

        // Hourly prune loop. Runs on a bare OS thread (plugin
        // activation can't assume a tokio runtime; bundled-rlib and
        // dynamic-cdylib hosts both spawn threads the same way).
        spawn_prune_loop(Arc::clone(&manual_runtime));

        // Optional auto-start of the rolling recording.
        if rolling_auto_start && rolling_runtime_available {
            auto_start_rolling(&rolling_runtime, &rolling_defaults);
        }

        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "recording-state", "status" => |_req: (), _ctx| {
                Ok::<recording_types::RecordingStatus, ServiceResponse>(recording_status_generated())
            },
            "recording-state", "list-recordings" => |_req: (), _ctx| {
                Ok::<Vec<recording_types::RecordingSummary>, ServiceResponse>(recording_list_generated())
            },
            "recording-state", "rolling-status" => |_req: (), _ctx| {
                Ok::<recording_types::RecordingRollingStatus, ServiceResponse>(recording_rolling_status_generated())
            },
            "recording-state", "capture-targets" => |_req: (), _ctx| {
                Ok::<Vec<recording_types::RecordingCaptureTarget>, ServiceResponse>(recording_capture_targets_generated())
            },
            "recording-commands", "start" => |req: StartRequest, _ctx| {
                Ok::<Result<recording_types::RecordingSummary, recording_types::RecordingError>, ServiceResponse>(
                    recording_start_generated(req),
                )
            },
            "recording-commands", "stop" => |req: StopRequest, _ctx| {
                Ok::<Result<uuid::Uuid, recording_types::RecordingError>, ServiceResponse>(
                    recording_stop_generated(req.recording_id),
                )
            },
            "recording-commands", "write-custom-event" => |req: WriteCustomEventRequest, _ctx| {
                Ok::<Result<(), recording_types::RecordingError>, ServiceResponse>(
                    recording_write_custom_event_generated(req),
                )
            },
            "recording-commands", "delete" => |req: DeleteRequest, _ctx| {
                Ok::<Result<uuid::Uuid, recording_types::RecordingError>, ServiceResponse>(
                    recording_delete_generated(req.recording_id),
                )
            },
            "recording-commands", "delete-all" => |_req: (), _ctx| {
                Ok::<u64, ServiceResponse>(recording_delete_all_generated())
            },
            "recording-commands", "cut" => |req: CutRequest, _ctx| {
                Ok::<Result<recording_types::RecordingSummary, recording_types::RecordingError>, ServiceResponse>(
                    recording_cut_generated(req),
                )
            },
            "recording-commands", "rolling-start" => |req: RollingStartRequest, _ctx| {
                Ok::<Result<recording_types::RecordingSummary, recording_types::RecordingError>, ServiceResponse>(
                    recording_rolling_start_generated(req.options),
                )
            },
            "recording-commands", "rolling-stop" => |_req: (), _ctx| {
                Ok::<Result<uuid::Uuid, recording_types::RecordingError>, ServiceResponse>(
                    recording_rolling_stop_generated(),
                )
            },
            "recording-commands", "rolling-clear" => |req: RollingClearRequest, _ctx| {
                Ok::<recording_types::RecordingRollingClearReport, ServiceResponse>(
                    recording_rolling_clear_generated(req.restart_if_active),
                )
            },
            "recording-commands", "prune" => |req: PruneRequest, _ctx| {
                Ok::<u64, ServiceResponse>(recording_prune_generated(req.older_than_days))
            },
        })
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
        // No typed Arc<dyn Trait> surface today — recording operations
        // dispatch exclusively through the byte-service path.
    }
}

fn spawn_prune_loop(manual_runtime: Arc<Mutex<RecordingRuntime>>) {
    std::thread::spawn(move || {
        // Initial prune on startup.
        if let Ok(runtime) = manual_runtime.lock() {
            let _ = runtime.prune(None);
        }
        loop {
            std::thread::sleep(std::time::Duration::from_hours(1));
            if let Ok(runtime) = manual_runtime.lock() {
                let _ = runtime.prune(None);
            }
        }
    });
}

fn auto_start_manual(
    manual_runtime: &Arc<Mutex<RecordingRuntime>>,
    options: &ManualRecordingStartOptions,
) {
    let Ok(mut runtime) = manual_runtime.lock() else {
        return;
    };
    if runtime.status().active.is_some() {
        return;
    }
    let profile = options.profile.unwrap_or(RecordingProfile::Full);
    let event_kinds = options
        .event_kinds
        .clone()
        .unwrap_or_else(default_event_kinds);
    let _ = runtime.start(
        None,
        options.capture_input,
        options.name.clone(),
        profile,
        event_kinds,
    );
}

fn auto_start_rolling(
    rolling_runtime: &Arc<Mutex<Option<RecordingRuntime>>>,
    settings: &RollingRecordingSettings,
) {
    let Ok(mut guard) = rolling_runtime.lock() else {
        return;
    };
    let Some(runtime) = guard.as_mut() else {
        return;
    };
    if runtime.status().active.is_some() {
        return;
    }
    let _ = runtime.start(
        None,
        settings.capture_input(),
        None,
        RecordingProfile::Full,
        settings.event_kinds.clone(),
    );
}

fn generated_failed(reason: impl Into<String>) -> recording_types::RecordingError {
    recording_types::RecordingError::Failed {
        reason: reason.into(),
    }
}

fn recording_status_generated() -> recording_types::RecordingStatus {
    handle_status().into()
}

fn recording_list_generated() -> Vec<recording_types::RecordingSummary> {
    handle_list().into_iter().map(Into::into).collect()
}

fn recording_rolling_status_generated() -> recording_types::RecordingRollingStatus {
    handle_rolling_status().into()
}

fn recording_capture_targets_generated() -> Vec<recording_types::RecordingCaptureTarget> {
    handle_capture_targets()
        .into_iter()
        .map(Into::into)
        .collect()
}

fn recording_start_generated(
    req: StartRequest,
) -> Result<recording_types::RecordingSummary, recording_types::RecordingError> {
    let recording = handle_start(
        req.session_id,
        req.capture_input,
        req.name,
        req.profile.map(Into::into),
        req.event_kinds
            .map(|kinds| kinds.into_iter().map(Into::into).collect()),
    )
    .ok_or_else(|| generated_failed("recording start failed"))?;
    publish_recording_started(&recording, None);
    Ok(recording.into())
}

fn recording_stop_generated(
    recording_id: Option<uuid::Uuid>,
) -> Result<uuid::Uuid, recording_types::RecordingError> {
    let recording_id =
        handle_stop(recording_id).ok_or(recording_types::RecordingError::NoActive)?;
    publish_recording_stopped(recording_id);
    Ok(recording_id)
}

fn recording_write_custom_event_generated(
    req: WriteCustomEventRequest,
) -> Result<(), recording_types::RecordingError> {
    if handle_write_custom_event(
        req.session_id,
        req.pane_id,
        req.source,
        req.name,
        req.payload,
    ) {
        Ok(())
    } else {
        Err(generated_failed("custom recording event was not accepted"))
    }
}

fn recording_delete_generated(
    recording_id: uuid::Uuid,
) -> Result<uuid::Uuid, recording_types::RecordingError> {
    handle_delete(recording_id).ok_or_else(|| generated_failed("recording delete failed"))
}

fn recording_delete_all_generated() -> u64 {
    u64::try_from(handle_delete_all()).unwrap_or(u64::MAX)
}

fn recording_cut_generated(
    req: CutRequest,
) -> Result<recording_types::RecordingSummary, recording_types::RecordingError> {
    publish_recording_cut_started(req.last_seconds, req.name.clone());
    match handle_cut(req.last_seconds, req.name) {
        Ok(summary) => {
            publish_recording_cut_completed(&summary);
            Ok(summary.into())
        }
        Err(error) => {
            let reason = error.to_string();
            publish_recording_cut_failed(reason.clone());
            Err(generated_failed(reason))
        }
    }
}

fn recording_rolling_start_generated(
    options: recording_types::RecordingRollingStartOptions,
) -> Result<recording_types::RecordingSummary, recording_types::RecordingError> {
    let recording =
        handle_rolling_start(options.into()).ok_or(recording_types::RecordingError::Unavailable)?;
    publish_recording_started(&recording, handle_rolling_status().rolling_window_secs);
    Ok(recording.into())
}

fn recording_rolling_stop_generated() -> Result<uuid::Uuid, recording_types::RecordingError> {
    let recording_id = handle_rolling_stop().ok_or(recording_types::RecordingError::NoActive)?;
    publish_recording_stopped(recording_id);
    Ok(recording_id)
}

fn recording_rolling_clear_generated(
    restart_if_active: bool,
) -> recording_types::RecordingRollingClearReport {
    let report = handle_rolling_clear(restart_if_active);
    if let Some(recording_id) = report.stopped_recording_id {
        publish_recording_stopped(recording_id);
    }
    if let Some(recording) = report.restarted_recording.as_ref() {
        publish_recording_started(recording, handle_rolling_status().rolling_window_secs);
    }
    report.into()
}

fn recording_prune_generated(older_than_days: Option<u64>) -> u64 {
    u64::try_from(handle_prune(older_than_days)).unwrap_or(u64::MAX)
}

// ────────────────────────────────────────────────────────────────────
// Registry lookup helpers
// ────────────────────────────────────────────────────────────────────

fn manual_handle() -> Option<Arc<RwLock<ManualRecordingRuntimeHandle>>> {
    global_plugin_state_registry().get::<ManualRecordingRuntimeHandle>()
}

fn rolling_handle() -> Option<Arc<RwLock<RollingRecordingRuntimeHandle>>> {
    global_plugin_state_registry().get::<RollingRecordingRuntimeHandle>()
}

fn config_handle() -> Option<Arc<RwLock<RecordingPluginConfig>>> {
    global_plugin_state_registry().get::<RecordingPluginConfig>()
}

fn publish_recording_started(recording: &RecordingSummary, rolling_window_secs: Option<u64>) {
    let target = recording_types::RecordingCaptureTarget {
        recording_id: recording.id,
        path: recording.path.clone(),
        rolling_window_secs,
    };
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::Started { target },
    );
}

fn publish_recording_stopped(recording_id: uuid::Uuid) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::Stopped { recording_id },
    );
}

fn publish_recording_cut_started(last_seconds: Option<u64>, name: Option<String>) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::CutStarted { last_seconds, name },
    );
}

fn publish_recording_cut_completed(summary: &RecordingSummary) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::CutCompleted {
            summary: summary.clone().into(),
        },
    );
}

fn publish_recording_cut_failed(reason: String) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::CutFailed { reason },
    );
}

// ────────────────────────────────────────────────────────────────────
// Simple operations against the manual runtime
// ────────────────────────────────────────────────────────────────────

fn handle_start(
    session_id: Option<uuid::Uuid>,
    capture_input: bool,
    name: Option<String>,
    profile: Option<RecordingProfile>,
    event_kinds: Option<Vec<RecordingEventKind>>,
) -> Option<RecordingSummary> {
    let handle = manual_handle()?;
    let guard = handle.read().ok()?;
    let mut runtime = guard.0.lock().ok()?;

    let profile = profile.unwrap_or(RecordingProfile::Full);
    let event_kinds = event_kinds.unwrap_or_else(default_event_kinds);
    runtime
        .start(session_id, capture_input, name, profile, event_kinds)
        .ok()
}

fn handle_stop(recording_id: Option<uuid::Uuid>) -> Option<uuid::Uuid> {
    let handle = manual_handle()?;
    let guard = handle.read().ok()?;
    let mut runtime = guard.0.lock().ok()?;
    runtime.stop(recording_id).ok().map(|summary| summary.id)
}

fn empty_status() -> RecordingStatus {
    RecordingStatus {
        active: None,
        queue_len: 0,
    }
}

fn handle_status() -> RecordingStatus {
    let Some(handle) = manual_handle() else {
        return empty_status();
    };
    let Ok(guard) = handle.read() else {
        return empty_status();
    };
    let Ok(runtime) = guard.0.lock() else {
        return empty_status();
    };
    runtime.status()
}

fn handle_list() -> Vec<RecordingSummary> {
    let Some(handle) = manual_handle() else {
        return Vec::new();
    };
    let Ok(guard) = handle.read() else {
        return Vec::new();
    };
    let Ok(runtime) = guard.0.lock() else {
        return Vec::new();
    };
    runtime.list().unwrap_or_default()
}

fn handle_delete(recording_id: uuid::Uuid) -> Option<uuid::Uuid> {
    let handle = manual_handle()?;
    let guard = handle.read().ok()?;
    let mut runtime = guard.0.lock().ok()?;
    runtime.delete(recording_id).ok().map(|summary| summary.id)
}

fn handle_delete_all() -> usize {
    let Some(handle) = manual_handle() else {
        return 0;
    };
    let Ok(guard) = handle.read() else {
        return 0;
    };
    let Ok(mut runtime) = guard.0.lock() else {
        return 0;
    };
    runtime.delete_all().unwrap_or(0)
}

fn handle_prune(older_than_days: Option<u64>) -> usize {
    let Some(handle) = manual_handle() else {
        return 0;
    };
    let Ok(guard) = handle.read() else {
        return 0;
    };
    let Ok(runtime) = guard.0.lock() else {
        return 0;
    };
    runtime.prune(older_than_days).unwrap_or(0)
}

// ────────────────────────────────────────────────────────────────────
// Custom event writes (both manual + rolling runtimes)
// ────────────────────────────────────────────────────────────────────

fn handle_write_custom_event(
    session_id: Option<uuid::Uuid>,
    pane_id: Option<uuid::Uuid>,
    source: String,
    name: String,
    payload: Vec<u8>,
) -> bool {
    let payload = RecordingPayload::Custom {
        source,
        name,
        payload,
    };
    let meta = RecordMeta {
        session_id,
        pane_id,
        client_id: None,
    };

    let mut accepted = false;
    if let Some(handle) = manual_handle()
        && let Ok(guard) = handle.read()
        && let Ok(runtime) = guard.0.lock()
        && let Ok(recorded) = runtime.record(RecordingEventKind::Custom, payload.clone(), meta)
    {
        accepted |= recorded;
    }
    if let Some(handle) = rolling_handle()
        && let Ok(guard) = handle.read()
        && let Ok(rolling) = guard.0.lock()
        && let Some(runtime) = rolling.as_ref()
        && let Ok(recorded) = runtime.record(RecordingEventKind::Custom, payload, meta)
    {
        accepted |= recorded;
    }

    accepted
}

// ────────────────────────────────────────────────────────────────────
// Capture-targets query
// ────────────────────────────────────────────────────────────────────

fn handle_capture_targets() -> Vec<RecordingCaptureTarget> {
    let mut targets: Vec<RecordingCaptureTarget> = Vec::new();

    if let Some(handle) = manual_handle()
        && let Ok(guard) = handle.read()
        && let Ok(runtime) = guard.0.lock()
        && let Some((id, path)) = runtime.active_capture_target()
    {
        targets.push(RecordingCaptureTarget {
            recording_id: id,
            path: path.display().to_string(),
            rolling_window_secs: None,
        });
    }
    if let Some(handle) = rolling_handle()
        && let Ok(guard) = handle.read()
        && let Ok(rolling) = guard.0.lock()
        && let Some(runtime) = rolling.as_ref()
        && let Some((id, path)) = runtime.active_capture_target()
    {
        targets.push(RecordingCaptureTarget {
            recording_id: id,
            path: path.display().to_string(),
            rolling_window_secs: runtime.rolling_window_secs(),
        });
    }

    targets
}

// ────────────────────────────────────────────────────────────────────
// Rolling-recording operations
// ────────────────────────────────────────────────────────────────────

fn empty_rolling_status(root_path: String) -> RecordingRollingStatus {
    RecordingRollingStatus {
        root_path,
        auto_start: false,
        available: false,
        active: None,
        rolling_window_secs: None,
        event_kinds: Vec::new(),
        usage: RecordingRollingUsage::default(),
    }
}

fn handle_rolling_status() -> RecordingRollingStatus {
    let Some(config) = config_handle() else {
        return empty_rolling_status(String::new());
    };
    let Ok(cfg) = config.read() else {
        return empty_rolling_status(String::new());
    };
    let root_path = cfg.rolling_recordings_dir.display().to_string();
    let defaults = cfg.rolling_defaults.clone();
    drop(cfg);

    let usage = collect_rolling_usage(Path::new(&root_path)).unwrap_or_default();

    let (active, rolling_window_secs, event_kinds) = rolling_handle()
        .and_then(|handle| {
            let guard = handle.read().ok()?;
            let rolling = guard.0.lock().ok()?;
            let runtime = rolling.as_ref()?;
            let status = runtime.status();
            let window = runtime.rolling_window_secs();
            let kinds = status.active.as_ref().map_or_else(
                || defaults.event_kinds.clone(),
                |summary| summary.event_kinds.clone(),
            );
            Some((status.active, window, kinds))
        })
        .unwrap_or((None, None, defaults.event_kinds.clone()));

    RecordingRollingStatus {
        root_path,
        auto_start: defaults.is_available(),
        available: defaults.is_available(),
        active,
        rolling_window_secs: rolling_window_secs.or(Some(defaults.window_secs)),
        event_kinds,
        usage,
    }
}

fn handle_rolling_stop() -> Option<uuid::Uuid> {
    let handle = rolling_handle()?;
    let guard = handle.read().ok()?;
    let mut rolling = guard.0.lock().ok()?;
    let runtime = rolling.as_mut()?;
    runtime.status().active.as_ref()?;
    runtime.stop(None).ok().map(|summary| summary.id)
}

fn handle_rolling_start(options: RecordingRollingStartOptions) -> Option<RecordingSummary> {
    let config = config_handle()?;
    let Ok(cfg) = config.read() else {
        return None;
    };
    let resolved = apply_rolling_start_options(&cfg.rolling_defaults, &options);
    let rolling_dir = cfg.rolling_recordings_dir.clone();
    let segment_mb = cfg.rolling_segment_mb;
    drop(cfg);

    if !resolved.is_available() {
        return None;
    }

    let handle = rolling_handle()?;
    let Ok(guard) = handle.read() else {
        return None;
    };
    let Ok(mut rolling) = guard.0.lock() else {
        return None;
    };

    if rolling.is_none() {
        *rolling = Some(RecordingRuntime::new_rolling(
            rolling_dir.clone(),
            segment_mb,
            resolved.window_secs,
        ));
    }

    let options_empty = rolling_start_options_is_empty(&options);
    let runtime = rolling.as_mut()?;

    if let Some(active) = runtime.status().active {
        if options_empty {
            return Some(active);
        }
        return None;
    }

    if runtime.rolling_window_secs() != Some(resolved.window_secs) {
        *runtime = RecordingRuntime::new_rolling(rolling_dir, segment_mb, resolved.window_secs);
    }

    let Ok(recording) = runtime.start(
        None,
        resolved.capture_input(),
        options.name,
        RecordingProfile::Full,
        resolved.event_kinds.clone(),
    ) else {
        return None;
    };

    Some(recording)
}

fn handle_cut(last_seconds: Option<u64>, name: Option<String>) -> anyhow::Result<RecordingSummary> {
    let config =
        config_handle().ok_or_else(|| anyhow::anyhow!("recording plugin config is unavailable"))?;
    let cfg = config
        .read()
        .map_err(|_| anyhow::anyhow!("recording plugin config lock is poisoned"))?;
    let output_root = cfg.recordings_dir.clone();
    drop(cfg);

    let handle = rolling_handle()
        .ok_or_else(|| anyhow::anyhow!("recording rolling runtime handle is unavailable"))?;
    let guard = handle
        .read()
        .map_err(|_| anyhow::anyhow!("recording rolling runtime handle lock is poisoned"))?;
    let mut rolling = guard
        .0
        .lock()
        .map_err(|_| anyhow::anyhow!("recording rolling runtime lock is poisoned"))?;
    let runtime = rolling.as_mut().ok_or_else(|| {
        anyhow::anyhow!("recording cut requires rolling recording mode to be enabled")
    })?;

    runtime.cut(&output_root, last_seconds, name)
}

fn handle_rolling_clear(restart_if_active: bool) -> RecordingRollingClearReport {
    let Some(config) = config_handle() else {
        return empty_clear_report();
    };
    let Ok(cfg) = config.read() else {
        return empty_clear_report();
    };
    let root = cfg.rolling_recordings_dir.clone();
    let segment_mb = cfg.rolling_segment_mb;
    let defaults = cfg.rolling_defaults.clone();
    drop(cfg);

    let usage_before = collect_rolling_usage(&root).unwrap_or_default();

    // Stop the active rolling recording if any; capture enough state
    // to restart it after clearing.
    let (was_active, stopped_recording_id, restart_settings, restart_name) = {
        let Some(handle) = rolling_handle() else {
            return clear_report_response(&root, false, false, None, None, &usage_before);
        };
        let Ok(guard) = handle.read() else {
            return clear_report_response(&root, false, false, None, None, &usage_before);
        };
        let Ok(mut rolling) = guard.0.lock() else {
            return clear_report_response(&root, false, false, None, None, &usage_before);
        };
        let Some(runtime) = rolling.as_mut() else {
            return clear_report_response(&root, false, false, None, None, &usage_before);
        };
        let Some(active) = runtime.status().active else {
            return clear_report_response(&root, false, false, None, None, &usage_before);
        };
        let name = active.name.clone();
        let settings = RollingRecordingSettings {
            window_secs: runtime
                .rolling_window_secs()
                .unwrap_or(defaults.window_secs),
            event_kinds: active.event_kinds.clone(),
        };
        let stopped_id = runtime.stop(None).ok().map(|summary| summary.id);
        (true, stopped_id, Some(settings), name)
    };

    if clear_rolling_root(&root).is_err() {
        return empty_clear_report();
    }

    let (restarted, restarted_recording) = if restart_if_active && was_active {
        let settings = restart_settings.unwrap_or_else(|| defaults.clone());
        if settings.is_available() {
            let recording = try_restart_rolling(&root, segment_mb, &settings, restart_name);
            match recording {
                Some(rec) => (true, Some(rec)),
                None => (false, None),
            }
        } else {
            (false, None)
        }
    } else {
        (false, None)
    };

    clear_report_response(
        &root,
        was_active,
        restarted,
        stopped_recording_id,
        restarted_recording,
        &usage_before,
    )
}

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

fn default_event_kinds() -> Vec<RecordingEventKind> {
    use RecordingEventKind::{
        PaneImage, PaneInputRaw, PaneOutputRaw, ProtocolReplyRaw, RequestDone, RequestError,
        RequestStart, ServerEvent,
    };
    vec![
        PaneInputRaw,
        PaneOutputRaw,
        ProtocolReplyRaw,
        PaneImage,
        ServerEvent,
        RequestStart,
        RequestDone,
        RequestError,
    ]
}

fn apply_rolling_start_options(
    defaults: &RollingRecordingSettings,
    options: &RecordingRollingStartOptions,
) -> RollingRecordingSettings {
    let window_secs = options.window_secs.unwrap_or(defaults.window_secs);
    let event_kinds = options
        .event_kinds
        .clone()
        .unwrap_or_else(|| defaults.event_kinds.clone());
    RollingRecordingSettings {
        window_secs,
        event_kinds,
    }
}

fn rolling_start_options_is_empty(options: &RecordingRollingStartOptions) -> bool {
    options.window_secs.is_none() && options.event_kinds.is_none() && options.name.is_none()
}

fn collect_rolling_usage(root: &Path) -> std::io::Result<RecordingRollingUsage> {
    let mut bytes = 0_u64;
    let mut files = 0_u64;
    let mut directories = 0_u64;
    if !root.exists() {
        return Ok(RecordingRollingUsage {
            bytes,
            files,
            directories,
            recording_dirs: 0,
        });
    }
    collect_rolling_usage_recursive(root, &mut bytes, &mut files, &mut directories)?;
    Ok(RecordingRollingUsage {
        bytes,
        files,
        directories,
        recording_dirs: 0,
    })
}

fn collect_rolling_usage_recursive(
    dir: &Path,
    bytes: &mut u64,
    files: &mut u64,
    directories: &mut u64,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            *directories += 1;
            collect_rolling_usage_recursive(&path, bytes, files, directories)?;
        } else if path.is_file()
            && let Ok(meta) = entry.metadata()
        {
            *bytes += meta.len();
            *files += 1;
        }
    }
    Ok(())
}

fn clear_rolling_root(root: &Path) -> std::io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else if path.is_file() {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn try_restart_rolling(
    root: &Path,
    segment_mb: usize,
    settings: &RollingRecordingSettings,
    name: Option<String>,
) -> Option<RecordingSummary> {
    let handle = rolling_handle()?;
    let guard = handle.read().ok()?;
    let mut rolling = guard.0.lock().ok()?;
    if rolling.is_none() {
        *rolling = Some(RecordingRuntime::new_rolling(
            root.to_path_buf(),
            segment_mb,
            settings.window_secs,
        ));
    }
    let runtime = rolling.as_mut()?;
    if runtime.rolling_window_secs() != Some(settings.window_secs) {
        *runtime =
            RecordingRuntime::new_rolling(root.to_path_buf(), segment_mb, settings.window_secs);
    }
    runtime
        .start(
            None,
            settings.capture_input(),
            name,
            RecordingProfile::Full,
            settings.event_kinds.clone(),
        )
        .ok()
}

fn clear_report_response(
    root: &Path,
    was_active: bool,
    restarted: bool,
    stopped_recording_id: Option<uuid::Uuid>,
    restarted_recording: Option<RecordingSummary>,
    usage_before: &RecordingRollingUsage,
) -> RecordingRollingClearReport {
    let usage_after = collect_rolling_usage(root).unwrap_or_default();
    RecordingRollingClearReport {
        root_path: root.display().to_string(),
        was_active,
        restarted,
        stopped_recording_id,
        restarted_recording,
        usage_before: usage_before.clone(),
        usage_after,
    }
}

fn empty_clear_report() -> RecordingRollingClearReport {
    RecordingRollingClearReport {
        root_path: String::new(),
        was_active: false,
        restarted: false,
        stopped_recording_id: None,
        restarted_recording: None,
        usage_before: RecordingRollingUsage::default(),
        usage_after: RecordingRollingUsage::default(),
    }
}

bmux_plugin_sdk::export_plugin!(RecordingPlugin, include_str!("../plugin.toml"));
