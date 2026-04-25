//! bmux recording plugin — typed recording lifecycle handlers.
//!
//! The plugin implements a typed byte-dispatch service for
//! `recording-commands` that accepts [`RecordingRequest`] payloads and
//! returns [`RecordingResponse`] payloads. Each operation reads the
//! manual / rolling runtime handles out of `PluginStateRegistry`,
//! performs the lifecycle operation, and returns the typed response.
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

use bmux_ipc::{
    RecordingCaptureTarget, RecordingEventKind, RecordingPayload, RecordingProfile,
    RecordingRollingStartOptions, RecordingRollingStatus, RecordingRollingUsage, RecordingStatus,
    RecordingSummary,
};
use bmux_plugin::global_plugin_state_registry;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_recording_plugin_api::{
    RECORDING_COMMANDS_INTERFACE, RECORDING_READ, RECORDING_WRITE, RecordingPluginConfig,
    RecordingRequest, RecordingResponse, RollingRecordingSettings,
};
use bmux_recording_runtime::{RecordMeta, RecordingSink, RecordingSinkHandle};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

/// Newtype wrapper for registering the manual recording runtime handle
/// in [`bmux_plugin::PluginStateRegistry`]. Plugin-local domain type;
/// server never names it.
pub struct ManualRecordingRuntimeHandle(pub Arc<Mutex<RecordingRuntime>>);

/// Newtype wrapper for registering the rolling recording runtime
/// handle in [`bmux_plugin::PluginStateRegistry`]. The inner option
/// is `None` when rolling recording is disabled in config.
pub struct RollingRecordingRuntimeHandle(pub Arc<Mutex<Option<RecordingRuntime>>>);

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
        let Ok(config) = config_handle.read() else {
            return Ok(bmux_plugin_sdk::EXIT_OK);
        };

        let recordings_dir = config.recordings_dir.clone();
        let rolling_recordings_dir = config.rolling_recordings_dir.clone();
        let rolling_segment_mb = config.rolling_segment_mb;
        let retention_days = config.retention_days;
        let rolling_defaults = config.rolling_defaults.clone();
        let rolling_auto_start = config.rolling_auto_start;
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
            "recording-commands", "dispatch" => |req: RecordingRequest, _ctx| {
                Ok::<RecordingResponse, ServiceResponse>(handle_recording_request(req))
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

#[allow(clippy::too_many_lines)]
fn handle_recording_request(req: RecordingRequest) -> RecordingResponse {
    match req {
        RecordingRequest::Start {
            session_id,
            capture_input,
            name,
            profile,
            event_kinds,
        } => handle_start(session_id, capture_input, name, profile, event_kinds),
        RecordingRequest::Stop { recording_id } => handle_stop(recording_id),
        RecordingRequest::Status => handle_status(),
        RecordingRequest::List => handle_list(),
        RecordingRequest::Delete { recording_id } => handle_delete(recording_id),
        RecordingRequest::DeleteAll => handle_delete_all(),
        RecordingRequest::Prune { older_than_days } => handle_prune(older_than_days),
        RecordingRequest::WriteCustomEvent {
            session_id,
            pane_id,
            source,
            name,
            payload,
        } => handle_write_custom_event(session_id, pane_id, source, name, payload),
        RecordingRequest::CaptureTargets => handle_capture_targets(),
        RecordingRequest::RollingStatus => handle_rolling_status(),
        RecordingRequest::RollingStop => handle_rolling_stop(),
        RecordingRequest::RollingStart { options } => handle_rolling_start(options),
        RecordingRequest::Cut { last_seconds, name } => handle_cut(last_seconds, name),
        RecordingRequest::RollingClear { restart_if_active } => {
            handle_rolling_clear(restart_if_active)
        }
    }
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

// ────────────────────────────────────────────────────────────────────
// Simple operations against the manual runtime
// ────────────────────────────────────────────────────────────────────

fn handle_start(
    session_id: Option<uuid::Uuid>,
    capture_input: bool,
    name: Option<String>,
    profile: Option<RecordingProfile>,
    event_kinds: Option<Vec<RecordingEventKind>>,
) -> RecordingResponse {
    let Some(handle) = manual_handle() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(mut runtime) = guard.0.lock() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };

    let profile = profile.unwrap_or(RecordingProfile::Full);
    let event_kinds = event_kinds.unwrap_or_else(default_event_kinds);
    match runtime.start(session_id, capture_input, name, profile, event_kinds) {
        Ok(recording) => RecordingResponse::Started { recording },
        Err(_) => RecordingResponse::CustomEventWritten { accepted: false },
    }
}

fn handle_stop(recording_id: Option<uuid::Uuid>) -> RecordingResponse {
    let Some(handle) = manual_handle() else {
        return RecordingResponse::Stopped { recording_id: None };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::Stopped { recording_id: None };
    };
    let Ok(mut runtime) = guard.0.lock() else {
        return RecordingResponse::Stopped { recording_id: None };
    };
    match runtime.stop(recording_id) {
        Ok(summary) => RecordingResponse::Stopped {
            recording_id: Some(summary.id),
        },
        Err(_) => RecordingResponse::Stopped { recording_id: None },
    }
}

fn empty_status() -> RecordingStatus {
    RecordingStatus {
        active: None,
        queue_len: 0,
    }
}

fn handle_status() -> RecordingResponse {
    let Some(handle) = manual_handle() else {
        return RecordingResponse::Status {
            status: empty_status(),
        };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::Status {
            status: empty_status(),
        };
    };
    let Ok(runtime) = guard.0.lock() else {
        return RecordingResponse::Status {
            status: empty_status(),
        };
    };
    RecordingResponse::Status {
        status: runtime.status(),
    }
}

fn handle_list() -> RecordingResponse {
    let Some(handle) = manual_handle() else {
        return RecordingResponse::List {
            recordings: Vec::new(),
        };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::List {
            recordings: Vec::new(),
        };
    };
    let Ok(runtime) = guard.0.lock() else {
        return RecordingResponse::List {
            recordings: Vec::new(),
        };
    };
    RecordingResponse::List {
        recordings: runtime.list().unwrap_or_default(),
    }
}

fn handle_delete(recording_id: uuid::Uuid) -> RecordingResponse {
    let Some(handle) = manual_handle() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(mut runtime) = guard.0.lock() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    match runtime.delete(recording_id) {
        Ok(summary) => RecordingResponse::Deleted {
            recording_id: summary.id,
        },
        Err(_) => RecordingResponse::CustomEventWritten { accepted: false },
    }
}

fn handle_delete_all() -> RecordingResponse {
    let Some(handle) = manual_handle() else {
        return RecordingResponse::DeleteAll { removed_count: 0 };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::DeleteAll { removed_count: 0 };
    };
    let Ok(mut runtime) = guard.0.lock() else {
        return RecordingResponse::DeleteAll { removed_count: 0 };
    };
    RecordingResponse::DeleteAll {
        removed_count: runtime.delete_all().unwrap_or(0),
    }
}

fn handle_prune(older_than_days: Option<u64>) -> RecordingResponse {
    let Some(handle) = manual_handle() else {
        return RecordingResponse::Pruned { pruned_count: 0 };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::Pruned { pruned_count: 0 };
    };
    let Ok(runtime) = guard.0.lock() else {
        return RecordingResponse::Pruned { pruned_count: 0 };
    };
    RecordingResponse::Pruned {
        pruned_count: runtime.prune(older_than_days).unwrap_or(0),
    }
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
) -> RecordingResponse {
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

    RecordingResponse::CustomEventWritten { accepted }
}

// ────────────────────────────────────────────────────────────────────
// Capture-targets query
// ────────────────────────────────────────────────────────────────────

fn handle_capture_targets() -> RecordingResponse {
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

    RecordingResponse::CaptureTargets { targets }
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

fn handle_rolling_status() -> RecordingResponse {
    let Some(config) = config_handle() else {
        return RecordingResponse::RollingStatus {
            status: empty_rolling_status(String::new()),
        };
    };
    let Ok(cfg) = config.read() else {
        return RecordingResponse::RollingStatus {
            status: empty_rolling_status(String::new()),
        };
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

    RecordingResponse::RollingStatus {
        status: RecordingRollingStatus {
            root_path,
            auto_start: defaults.is_available(),
            available: defaults.is_available(),
            active,
            rolling_window_secs: rolling_window_secs.or(Some(defaults.window_secs)),
            event_kinds,
            usage,
        },
    }
}

fn handle_rolling_stop() -> RecordingResponse {
    let Some(handle) = rolling_handle() else {
        return RecordingResponse::RollingStopped { recording_id: None };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::RollingStopped { recording_id: None };
    };
    let Ok(mut rolling) = guard.0.lock() else {
        return RecordingResponse::RollingStopped { recording_id: None };
    };
    let Some(runtime) = rolling.as_mut() else {
        return RecordingResponse::RollingStopped { recording_id: None };
    };
    if runtime.status().active.is_none() {
        return RecordingResponse::RollingStopped { recording_id: None };
    }
    match runtime.stop(None) {
        Ok(summary) => RecordingResponse::RollingStopped {
            recording_id: Some(summary.id),
        },
        Err(_) => RecordingResponse::RollingStopped { recording_id: None },
    }
}

fn handle_rolling_start(options: RecordingRollingStartOptions) -> RecordingResponse {
    let Some(config) = config_handle() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(cfg) = config.read() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let resolved = apply_rolling_start_options(&cfg.rolling_defaults, &options);
    let rolling_dir = cfg.rolling_recordings_dir.clone();
    let segment_mb = cfg.rolling_segment_mb;
    drop(cfg);

    if !resolved.is_available() {
        return RecordingResponse::CustomEventWritten { accepted: false };
    }

    let Some(handle) = rolling_handle() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(mut rolling) = guard.0.lock() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };

    if rolling.is_none() {
        *rolling = Some(RecordingRuntime::new_rolling(
            rolling_dir.clone(),
            segment_mb,
            resolved.window_secs,
        ));
    }

    let options_empty = rolling_start_options_is_empty(&options);
    let Some(runtime) = rolling.as_mut() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };

    if let Some(active) = runtime.status().active {
        if options_empty {
            return RecordingResponse::RollingStarted { recording: active };
        }
        return RecordingResponse::CustomEventWritten { accepted: false };
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
        return RecordingResponse::CustomEventWritten { accepted: false };
    };

    RecordingResponse::RollingStarted { recording }
}

fn handle_cut(last_seconds: Option<u64>, name: Option<String>) -> RecordingResponse {
    let Some(config) = config_handle() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(cfg) = config.read() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let output_root = cfg.recordings_dir.clone();
    drop(cfg);

    let Some(handle) = rolling_handle() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(mut rolling) = guard.0.lock() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Some(runtime) = rolling.as_mut() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };

    match runtime.cut(&output_root, last_seconds, name) {
        Ok(recording) => RecordingResponse::Cut { recording },
        Err(_) => RecordingResponse::CustomEventWritten { accepted: false },
    }
}

fn handle_rolling_clear(restart_if_active: bool) -> RecordingResponse {
    let Some(config) = config_handle() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(cfg) = config.read() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
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
        return RecordingResponse::CustomEventWritten { accepted: false };
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
) -> RecordingResponse {
    let usage_after = collect_rolling_usage(root).unwrap_or_default();
    RecordingResponse::RollingCleared {
        report: bmux_ipc::RecordingRollingClearReport {
            root_path: root.display().to_string(),
            was_active,
            restarted,
            stopped_recording_id,
            restarted_recording,
            usage_before: usage_before.clone(),
            usage_after,
        },
    }
}

// Silence unused-variable warnings on the constants until the full
// handler set lands.
const _KEEPS_CONSTS_ALIVE: (
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::InterfaceId,
) = (
    RECORDING_READ,
    RECORDING_WRITE,
    RECORDING_COMMANDS_INTERFACE,
);

bmux_plugin_sdk::export_plugin!(RecordingPlugin, include_str!("../plugin.toml"));
