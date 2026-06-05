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

mod export;
pub mod recording_runtime;
pub use recording_runtime::{
    RecordingCutError, RecordingRuntime, cut_missing_active_recording_dir, execute_recording_cut,
    prune_old_recordings,
};

use bmux_plugin::{HostRuntimeApi, global_plugin_state_registry};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    CoreCliCommandRequest, HostScope, ProviderId, RegisteredService, ServiceKind, ServiceRequest,
    TypedServiceRegistrationContext, TypedServiceRegistry,
};
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
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

type RecordingPayload = ProtocolRecordingPayload<bmux_ipc::Event, bmux_ipc::ErrorCode>;

/// Newtype wrapper for registering the manual recording runtime handle
/// in [`bmux_plugin::PluginStateRegistry`]. Plugin-local domain type;
/// server never names it.
pub struct ManualRecordingRuntimeHandle(pub Arc<Mutex<RecordingRuntime>>);

/// Newtype wrapper for registering the rolling recording runtime
/// handle in [`bmux_plugin::PluginStateRegistry`]. The inner option
/// is `None` when rolling recording is disabled in config.
pub struct RollingRecordingRuntimeHandle(pub Arc<Mutex<Option<RecordingRuntime>>>);

static CUT_FILESYSTEM_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static RECORDING_JOBS: OnceLock<Mutex<BTreeMap<uuid::Uuid, recording_types::RecordingJob>>> =
    OnceLock::new();

fn cut_filesystem_lock() -> &'static Mutex<()> {
    CUT_FILESYSTEM_LOCK.get_or_init(|| Mutex::new(()))
}

fn recording_jobs() -> &'static Mutex<BTreeMap<uuid::Uuid, recording_types::RecordingJob>> {
    RECORDING_JOBS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

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
struct QueueCutRequest {
    last_seconds: Option<u64>,
    name: Option<String>,
    export_fps: Option<u32>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct JobStatusRequest {
    job_id: uuid::Uuid,
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
        context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        if should_queue_recording_cut_for_non_cli(&context) {
            return run_recording_cut_command_queued(&context);
        }
        if context.command == "recording-export" {
            return run_recording_export_command(&context);
        }
        run_recording_command_sync(&context)
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
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
            "recording-state", "job-status" => |req: JobStatusRequest, _ctx| {
                Ok::<Option<recording_types::RecordingJob>, ServiceResponse>(recording_job_status_generated(req.job_id))
            },
            "recording-state", "list-jobs" => |_req: (), _ctx| {
                Ok::<Vec<recording_types::RecordingJob>, ServiceResponse>(recording_list_jobs_generated())
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
            "recording-commands", "queue-cut" => |req: QueueCutRequest, ctx| {
                recording_queue_cut_generated(req, ctx).map_err(|error| recording_error_response(&error))
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

fn recording_command_path(command_name: &str) -> Option<&'static [&'static str]> {
    match command_name {
        "recording-start" => Some(&["recording", "start"]),
        "recording-stop" => Some(&["recording", "stop"]),
        "recording-status" => Some(&["recording", "status"]),
        "recording-path" => Some(&["recording", "path"]),
        "recording-list" => Some(&["recording", "list"]),
        "recording-delete" => Some(&["recording", "delete"]),
        "recording-delete-all" => Some(&["recording", "delete-all"]),
        "recording-cut" => Some(&["recording", "cut"]),
        "recording-inspect" => Some(&["recording", "inspect"]),
        "recording-analyze" => Some(&["recording", "analyze"]),
        "recording-replay" => Some(&["recording", "replay"]),
        "recording-verify-smoke" => Some(&["recording", "verify-smoke"]),
        "recording-prune" => Some(&["recording", "prune"]),
        _ => None,
    }
}

fn should_queue_recording_cut_for_non_cli(context: &NativeCommandContext) -> bool {
    context.command == "recording-cut"
        && context.invocation_source != bmux_plugin_sdk::NativeCommandInvocationSource::Cli
}

fn run_recording_export_command(
    context: &NativeCommandContext,
) -> std::result::Result<i32, PluginCommandError> {
    reject_unsupported_recording_export_args(&context.arguments)?;
    let recording_id = first_positional_argument(&context.arguments)
        .ok_or_else(|| PluginCommandError::failed("recording export requires a recording id"))?;
    let format =
        parse_string_option(&context.arguments, "format").unwrap_or_else(|| "gif".to_string());
    if format != "gif" {
        return Err(PluginCommandError::failed(format!(
            "unsupported recording export format '{format}'; only gif is supported"
        )));
    }
    let output = parse_string_option(&context.arguments, "output")
        .ok_or_else(|| PluginCommandError::failed("recording export requires --output <PATH>"))?;
    let fps = parse_u32_option(&context.arguments, "fps")
        .unwrap_or(24)
        .max(1);
    let recordings_root = recordings_root_for_command(context)?;

    export::export_recording_gif_from_root(&recordings_root, &recording_id, &output, fps)
        .map_err(|error| PluginCommandError::failed(format!("recording export failed: {error}")))?;
    bmux_plugin_sdk::record_command_outcome_metadata(
        bmux_plugin_sdk::COMMAND_OUTCOME_STATUS_MESSAGE_KEY,
        serde_json::json!(format!("recording export complete: {output}")),
    );
    Ok(0)
}

fn reject_unsupported_recording_export_args(
    arguments: &[String],
) -> std::result::Result<(), PluginCommandError> {
    const SUPPORTED_FLAGS: &[&str] = &["format", "output", "fps"];
    for argument in arguments {
        let Some(flag) = argument.strip_prefix("--") else {
            continue;
        };
        let name = flag.split_once('=').map_or(flag, |(name, _)| name);
        if !SUPPORTED_FLAGS.contains(&name) {
            return Err(PluginCommandError::failed(format!(
                "unsupported recording export option '--{name}'; bmux.recording currently supports --format gif, --output, and --fps"
            )));
        }
    }
    Ok(())
}

fn recordings_root_for_command(
    context: &NativeCommandContext,
) -> std::result::Result<PathBuf, PluginCommandError> {
    if let Some(config_handle) = config_handle() {
        return Ok(config_handle
            .read()
            .map_err(|_| PluginCommandError::failed("recording plugin config lock is poisoned"))?
            .recordings_dir
            .clone());
    }

    Ok(Path::new(&context.connection.state_dir)
        .join("runtime")
        .join("recordings"))
}

fn first_positional_argument(arguments: &[String]) -> Option<String> {
    let mut iter = arguments.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return iter.next().cloned();
        }
        if arg.starts_with("--") {
            let _ = iter.next();
            continue;
        }
        return Some(arg.clone());
    }
    None
}

fn run_recording_cut_command_queued(
    context: &NativeCommandContext,
) -> std::result::Result<i32, PluginCommandError> {
    let last_seconds = parse_u64_option(&context.arguments, "last-seconds");
    let export_fps = parse_u32_option(&context.arguments, "export-fps");
    let name = parse_string_option(&context.arguments, "name");
    tracing::info!(
        invocation_source = ?context.invocation_source,
        ?last_seconds,
        ?export_fps,
        name = ?name,
        "recording cut enqueue requested"
    );

    let service_context = queue_cut_service_context_from_command(context);
    let job = recording_queue_cut_generated(
        QueueCutRequest {
            last_seconds,
            name,
            export_fps,
        },
        &service_context,
    )
    .map_err(|error| {
        tracing::warn!(?error, "recording cut queue failed");
        PluginCommandError::failed(format!("failed queueing recording cut: {error:?}"))
    })?;

    tracing::info!(job_id = %job.id, status = ?job.status, "recording cut queued");
    bmux_plugin_sdk::record_command_outcome_metadata(
        bmux_plugin_sdk::COMMAND_OUTCOME_STATUS_MESSAGE_KEY,
        serde_json::json!(format!("recording cut queued: {}", job.id)),
    );
    Ok(0)
}

fn queue_cut_service_context_from_command(context: &NativeCommandContext) -> NativeServiceContext {
    let service = RegisteredService {
        capability: HostScope::new("bmux.recording.write")
            .expect("recording write capability should be valid"),
        kind: ServiceKind::Command,
        interface_id: "recording-commands".to_string(),
        provider: ProviderId::Plugin("bmux.recording".to_string()),
    };
    NativeServiceContext {
        plugin_id: context.plugin_id.clone(),
        request: ServiceRequest {
            caller_plugin_id: context.plugin_id.clone(),
            service,
            operation: "queue-cut".to_string(),
            payload: Vec::new(),
        },
        required_capabilities: context.required_capabilities.clone(),
        provided_capabilities: context.provided_capabilities.clone(),
        services: context.services.clone(),
        available_capabilities: context.available_capabilities.clone(),
        enabled_plugins: context.enabled_plugins.clone(),
        plugin_search_roots: context.plugin_search_roots.clone(),
        host: context.host.clone(),
        connection: context.connection.clone(),
        settings: context.settings.clone(),
        plugin_settings_map: context.plugin_settings_map.clone(),
        caller_client_id: context.caller_client_id,
        host_kernel_bridge: context.host_kernel_bridge,
    }
}

fn run_recording_command_sync(
    context: &NativeCommandContext,
) -> std::result::Result<i32, PluginCommandError> {
    let command_path = recording_command_path(context.command.as_str())
        .ok_or_else(|| PluginCommandError::unknown_command(&context.command))?;
    let request = CoreCliCommandRequest::new(
        command_path.iter().map(ToString::to_string).collect(),
        context.arguments.clone(),
    );
    let response = context
        .core_cli_command_run_path(&request)
        .map_err(|error| {
            PluginCommandError::from(format!(
                "failed running recording command path via host bridge: {error}"
            ))
        })?;
    if let Some(error) = response.error {
        return Err(PluginCommandError::new(response.exit_code.max(1), error));
    }
    Ok(response.exit_code)
}

fn parse_u64_option(arguments: &[String], long_name: &str) -> Option<u64> {
    let long_flag = format!("--{long_name}");
    let mut iter = arguments.iter();
    while let Some(arg) = iter.next() {
        if arg == &long_flag
            && let Some(value) = iter.next()
            && let Ok(value) = value.parse::<u64>()
        {
            return Some(value);
        }
    }
    None
}

fn parse_u32_option(arguments: &[String], long_name: &str) -> Option<u32> {
    parse_u64_option(arguments, long_name).and_then(|value| u32::try_from(value).ok())
}

fn parse_string_option(arguments: &[String], long_name: &str) -> Option<String> {
    let long_flag = format!("--{long_name}");
    let mut iter = arguments.iter();
    while let Some(arg) = iter.next() {
        if arg == &long_flag {
            return iter.next().cloned();
        }
    }
    None
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

fn recording_error_response(error: &recording_types::RecordingError) -> ServiceResponse {
    ServiceResponse::error("recording_error", recording_error_message(error))
}

fn recording_error_message(error: &recording_types::RecordingError) -> String {
    match error {
        recording_types::RecordingError::NoActive => "no active recording".to_string(),
        recording_types::RecordingError::Unavailable => "recording unavailable".to_string(),
        recording_types::RecordingError::Failed { reason } => reason.clone(),
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

fn recording_job_status_generated(job_id: uuid::Uuid) -> Option<recording_types::RecordingJob> {
    recording_jobs()
        .lock()
        .ok()
        .and_then(|jobs| jobs.get(&job_id).cloned())
}

fn recording_list_jobs_generated() -> Vec<recording_types::RecordingJob> {
    recording_jobs()
        .lock()
        .map(|jobs| jobs.values().cloned().collect())
        .unwrap_or_default()
}

fn recording_queue_cut_generated(
    req: QueueCutRequest,
    _ctx: &NativeServiceContext,
) -> Result<recording_types::RecordingJob, recording_types::RecordingError> {
    // Validate preconditions synchronously so the caller gets fast failure when
    // the plugin state is unavailable.
    let config_handle = global_plugin_state_registry()
        .get::<RecordingPluginConfig>()
        .ok_or_else(|| generated_failed("recording plugin config is unavailable"))?;
    if config_handle.read().is_err() {
        return Err(generated_failed("recording plugin config lock is poisoned"));
    }

    let rolling_handle = rolling_handle()
        .ok_or_else(|| generated_failed("recording rolling runtime handle is unavailable"))?;
    let guard = rolling_handle
        .read()
        .map_err(|_| generated_failed("recording rolling runtime handle lock is poisoned"))?;
    let rolling = guard
        .0
        .lock()
        .map_err(|_| generated_failed("recording rolling runtime lock is poisoned"))?;
    let runtime = rolling.as_ref().ok_or_else(|| {
        generated_failed("recording cut requires rolling recording mode to be enabled")
    })?;
    let has_active = runtime.status().active.is_some();
    let _ = runtime;
    drop(rolling);
    drop(guard);
    drop(rolling_handle);

    if !has_active {
        return Err(generated_failed(
            "no active rolling recording available for cut",
        ));
    }

    let job = new_recording_job(recording_types::RecordingJobKind::Cut);
    upsert_recording_job(job.clone());
    publish_recording_job_updated(&job);

    let job_id = job.id;
    if let Err(error) = std::thread::Builder::new()
        .name("bmux-recording-cut-worker".to_string())
        .spawn(move || run_queued_cut_job(job_id, req))
    {
        let reason = format!("failed spawning cut worker thread: {error}");
        let failed = update_recording_job(job_id, |job| {
            job.status = recording_types::RecordingJobStatus::Failed;
            job.error = Some(reason.clone());
        });
        publish_recording_job_updated(&failed);
        publish_recording_cut_failed(reason.clone());
        return Err(generated_failed(reason));
    }

    Ok(job)
}

fn new_recording_job(kind: recording_types::RecordingJobKind) -> recording_types::RecordingJob {
    recording_types::RecordingJob {
        id: uuid::Uuid::new_v4(),
        kind,
        status: recording_types::RecordingJobStatus::Queued,
        recording_id: None,
        recording_path: None,
        export_output_path: None,
        error: None,
    }
}

fn upsert_recording_job(job: recording_types::RecordingJob) {
    if let Ok(mut jobs) = recording_jobs().lock() {
        jobs.insert(job.id, job);
    }
}

fn update_recording_job(
    job_id: uuid::Uuid,
    update: impl FnOnce(&mut recording_types::RecordingJob),
) -> recording_types::RecordingJob {
    let mut jobs = recording_jobs()
        .lock()
        .expect("recording jobs lock should not be poisoned");
    let job = jobs.entry(job_id).or_insert_with(|| {
        let mut job = new_recording_job(recording_types::RecordingJobKind::Cut);
        job.id = job_id;
        job
    });
    update(job);
    job.clone()
}

fn run_queued_cut_job(job_id: uuid::Uuid, req: QueueCutRequest) {
    let cutting = update_recording_job(job_id, |job| {
        job.status = recording_types::RecordingJobStatus::Cutting;
    });
    publish_recording_job_updated(&cutting);
    publish_recording_cut_started(req.last_seconds, req.name.clone());

    let summary = match handle_cut(req.last_seconds, req.name) {
        Ok(summary) => summary,
        Err(error) => {
            let reason = error.to_string();
            let failed = update_recording_job(job_id, |job| {
                job.status = recording_types::RecordingJobStatus::Failed;
                job.error = Some(reason.clone());
            });
            publish_recording_job_updated(&failed);
            publish_recording_cut_failed(reason);
            return;
        }
    };

    publish_recording_cut_completed(&summary);
    let cut_completed = update_recording_job(job_id, |job| {
        job.status = recording_types::RecordingJobStatus::CutCompleted;
        job.recording_id = Some(summary.id);
        job.recording_path = Some(summary.path.clone());
    });
    publish_recording_job_updated(&cut_completed);

    let export_settings = current_auto_export_settings(req.export_fps);
    if !export_settings.enabled {
        tracing::info!(
            job_id = %job_id,
            recording_id = %summary.id,
            "recording cut auto-export disabled"
        );
        let completed = update_recording_job(job_id, |job| {
            job.status = recording_types::RecordingJobStatus::Completed;
        });
        publish_recording_job_updated(&completed);
        return;
    }

    let recording_dir = PathBuf::from(&summary.path);
    let output_path = auto_export_output_path(
        &recording_dir,
        export_settings.output_dir.as_deref(),
        summary.id,
    );
    let output = output_path.to_string_lossy().to_string();
    tracing::info!(
        job_id = %job_id,
        recording_id = %summary.id,
        output_path = %output,
        fps = export_settings.fps,
        "recording cut auto-export starting"
    );
    publish_recording_export_started(summary.id, output.clone());
    let exporting = update_recording_job(job_id, |job| {
        job.status = recording_types::RecordingJobStatus::Exporting;
        job.export_output_path = Some(output.clone());
    });
    publish_recording_job_updated(&exporting);

    match export::export_recording_gif_for_recording_dir(
        &recording_dir,
        summary.id,
        &output,
        export_settings.fps,
    ) {
        Ok(()) => {
            tracing::info!(
                job_id = %job_id,
                recording_id = %summary.id,
                output_path = %output,
                "recording cut auto-export completed"
            );
            publish_recording_export_completed(summary.id, output);
            let completed = update_recording_job(job_id, |job| {
                job.status = recording_types::RecordingJobStatus::Completed;
            });
            publish_recording_job_updated(&completed);
        }
        Err(error) => {
            let reason = error.to_string();
            tracing::warn!(
                job_id = %job_id,
                recording_id = %summary.id,
                output_path = %output,
                error = %reason,
                "recording cut auto-export failed"
            );
            publish_recording_export_failed(summary.id, output, reason.clone());
            let failed = update_recording_job(job_id, |job| {
                job.status = recording_types::RecordingJobStatus::Failed;
                job.error = Some(reason);
            });
            publish_recording_job_updated(&failed);
        }
    }
}

struct AutoExportSettings {
    enabled: bool,
    output_dir: Option<PathBuf>,
    fps: u32,
}

fn current_auto_export_settings(fps_override: Option<u32>) -> AutoExportSettings {
    let Some(config_handle) = config_handle() else {
        return AutoExportSettings {
            enabled: false,
            output_dir: None,
            fps: fps_override.unwrap_or(24).max(1),
        };
    };
    let Ok(config) = config_handle.read() else {
        return AutoExportSettings {
            enabled: false,
            output_dir: None,
            fps: fps_override.unwrap_or(24).max(1),
        };
    };
    AutoExportSettings {
        enabled: config.auto_export || config.auto_export_dir.is_some(),
        output_dir: config.auto_export_dir.clone(),
        fps: fps_override.unwrap_or(config.auto_export_fps).max(1),
    }
}

fn auto_export_output_path(
    recording_dir: &Path,
    explicit_output_dir: Option<&Path>,
    recording_id: uuid::Uuid,
) -> PathBuf {
    let output_dir = explicit_output_dir.map_or_else(
        || {
            recording_dir
                .parent()
                .map_or_else(|| recording_dir.to_path_buf(), Path::to_path_buf)
        },
        Path::to_path_buf,
    );
    output_dir.join(format!("recording-{recording_id}.gif"))
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

fn publish_recording_export_started(recording_id: uuid::Uuid, output_path: String) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::ExportStarted {
            recording_id,
            output_path,
        },
    );
}

fn publish_recording_export_completed(recording_id: uuid::Uuid, output_path: String) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::ExportCompleted {
            recording_id,
            output_path,
        },
    );
}

fn publish_recording_export_failed(recording_id: uuid::Uuid, output_path: String, reason: String) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::ExportFailed {
            recording_id,
            output_path,
            reason,
        },
    );
}

fn publish_recording_job_updated(job: &recording_types::RecordingJob) {
    let _ = bmux_plugin::global_event_bus().emit(
        &recording_events::EVENT_KIND,
        recording_events::RecordingEvent::JobUpdated { job: job.clone() },
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

    let plan = {
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
        runtime.prepare_cut(output_root, last_seconds, name)?
    };

    let _cut_guard = cut_filesystem_lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("recording cut filesystem lock is poisoned"))?;
    execute_recording_cut(plan)
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

    {
        let Ok(_cut_guard) = cut_filesystem_lock().lock() else {
            return empty_clear_report();
        };
        if clear_rolling_root(&root).is_err() {
            return empty_clear_report();
        }
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

#[cfg(test)]
mod tests {
    use super::{RecordingPlugin, auto_export_output_path};
    use bmux_plugin_sdk::{
        CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostConnectionInfo, HostMetadata,
        NativeCommandContext, NativeCommandInvocationSource, RustPlugin,
    };
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn auto_export_output_path_uses_configured_directory() {
        let recording_id =
            uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("valid uuid");
        let output = auto_export_output_path(
            Path::new("/raw/recordings/22222222-2222-4222-8222-222222222222"),
            Some(Path::new("/exports")),
            recording_id,
        );

        assert_eq!(
            output,
            Path::new("/exports/recording-11111111-1111-4111-8111-111111111111.gif")
        );
    }

    #[test]
    fn auto_export_output_path_defaults_next_to_recordings_root() {
        let recording_id =
            uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("valid uuid");
        let output = auto_export_output_path(
            Path::new("/raw/recordings/22222222-2222-4222-8222-222222222222"),
            None,
            recording_id,
        );

        assert_eq!(
            output,
            Path::new("/raw/recordings/recording-11111111-1111-4111-8111-111111111111.gif")
        );
    }

    #[test]
    fn recording_plugin_export_does_not_shell_out_to_bmux_cli() {
        let export_source = include_str!("export.rs");
        let plugin_source = include_str!("lib.rs");

        for forbidden in [
            concat!("std::process", "::Command"),
            concat!("current", "_exe"),
            concat!("run_core", "_gif_export"),
            concat!("recording export", " subprocess"),
        ] {
            assert!(
                !export_source.contains(forbidden),
                "export module must not contain {forbidden}"
            );
            assert!(
                !plugin_source.contains(forbidden),
                "plugin module must not contain {forbidden}"
            );
        }
    }

    #[test]
    fn non_cli_recording_cut_reports_queue_precondition_without_self_dispatch_deadlock() {
        let mut plugin = RecordingPlugin;
        let error = plugin
            .run_command(command_context(
                "recording-cut",
                &["--last-seconds", "30", "--export-fps", "12"],
                NativeCommandInvocationSource::AttachKeybinding,
            ))
            .expect_err("recording cut should report missing queue precondition");

        assert!(
            error.to_string().contains("failed queueing recording cut"),
            "unexpected error: {error}"
        );
    }

    fn command_context(
        command: &str,
        arguments: &[&str],
        invocation_source: NativeCommandInvocationSource,
    ) -> NativeCommandContext {
        NativeCommandContext {
            plugin_id: "bmux.recording".to_string(),
            command: command.to_string(),
            arguments: arguments.iter().map(ToString::to_string).collect(),
            required_capabilities: Vec::new(),
            provided_capabilities: vec!["bmux.recording.write".to_string()],
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            active_keybindings: Vec::new(),
            host: HostMetadata {
                product_name: "bmux-test".to_string(),
                product_version: "0.0.0".to_string(),
                plugin_api_version: CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: Vec::new(),
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source,
            host_kernel_bridge: None,
        }
    }
}
