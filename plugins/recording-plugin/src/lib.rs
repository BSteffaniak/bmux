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

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_recording_plugin_api::export_schema,
            crate_path: ::bmux_recording_plugin_api,
        },
        settings: "plugin.toml",
    }
}

mod activation_binding;
pub use activation_binding::definition as plugin_definition_with_activation_configuration;

mod commands;
mod durable;
mod export;
mod from_recording;
mod interactive_command;
mod interactive_terminal;
mod platform;
mod process_artifacts;
mod process_bootstrap;
mod process_commands;
mod process_dispatch;
mod process_display;
mod process_live_presentation;
mod process_runtime;
mod process_store;
pub mod recording_runtime;
mod services;
pub use recording_runtime::{
    CutWindow, RecordingCutError, RecordingRuntime, cut_missing_active_recording_dir,
    execute_recording_cut, prune_old_recordings,
};

use bmux_plugin::PluginStateRegistry;
use bmux_plugin_sdk::author::{self, Activation, Deactivation, Declarations, Plugin};
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
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
    #[serde(default)]
    start_epoch_ms: Option<u64>,
    #[serde(default)]
    end_epoch_ms: Option<u64>,
    #[serde(default)]
    without_input: Option<bool>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct QueueCutRequest {
    last_seconds: Option<u64>,
    name: Option<String>,
    export_fps: Option<u32>,
    #[serde(default)]
    start_epoch_ms: Option<u64>,
    #[serde(default)]
    end_epoch_ms: Option<u64>,
    #[serde(default)]
    without_input: Option<bool>,
}

/// Build the cut window from wire fields, rejecting a range combined with `last_seconds`.
fn cut_window(
    last_seconds: Option<u64>,
    start_epoch_ms: Option<u64>,
    end_epoch_ms: Option<u64>,
) -> anyhow::Result<CutWindow> {
    match (last_seconds, start_epoch_ms, end_epoch_ms) {
        (seconds, None, None) => Ok(CutWindow::LastSeconds(seconds)),
        (None, Some(start_epoch_ms), end_epoch_ms) => Ok(CutWindow::Range {
            start_epoch_ms,
            end_epoch_ms,
        }),
        (Some(_), _, _) => {
            anyhow::bail!("recording cut takes either last seconds or a start/end range, not both")
        }
        (None, None, Some(_)) => anyhow::bail!("recording cut end requires a start"),
    }
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
    fn record_checked(
        &self,
        kind: RecordingEventKind,
        payload: RecordingPayload,
        meta: RecordMeta,
    ) -> Result<(), String> {
        let manual = self
            .manual
            .lock()
            .map_err(|_| "manual recording lock poisoned")?;
        if manual.active_capture_target().is_none() {
            return Err("manual recording capture owner unavailable".into());
        }
        manual
            .record(kind, payload.clone(), meta)
            .map_err(|error| error.to_string())?;
        let rolling = self
            .rolling
            .lock()
            .map_err(|_| "rolling recording lock poisoned")?;
        if let Some(runtime) = rolling.as_ref() {
            runtime
                .record(kind, payload, meta)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn accepts(&self, kind: RecordingEventKind, meta: RecordMeta) -> bool {
        let manual = self
            .manual
            .lock()
            .is_ok_and(|runtime| runtime.accepts(kind, meta));
        manual
            || self.rolling.lock().is_ok_and(|runtime| {
                runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.accepts(kind, meta))
            })
    }

    fn record(&self, kind: RecordingEventKind, payload: RecordingPayload, meta: RecordMeta) {
        // Copy the payload only when both runtimes keep it.
        let rolling_accepts = self.rolling.lock().is_ok_and(|runtime| {
            runtime
                .as_ref()
                .is_some_and(|runtime| runtime.accepts(kind, meta))
        });
        let mut payload = Some(payload);
        if let Ok(runtime) = self.manual.lock()
            && runtime.accepts(kind, meta)
        {
            let event = if rolling_accepts {
                payload.clone()
            } else {
                payload.take()
            };
            if let Some(event) = event {
                let _ = runtime.record(kind, event, meta);
            }
        }
        if rolling_accepts
            && let Some(payload) = payload
            && let Ok(runtime) = self.rolling.lock()
            && let Some(runtime) = runtime.as_ref()
        {
            let _ = runtime.record(kind, payload, meta);
        }
    }
}

pub struct RecordingPlugin {
    owner: RwLock<Option<RecordingOwner>>,
    // A selected scoped definition supplies this slot. None is the default
    // composition, which reads the activation's registry; Some(empty) is
    // explicitly unbound.
    binding: Option<Arc<RwLock<Option<RecordingActivationConfiguration>>>>,
    /// The activation's view of the host state registry (default
    /// composition only); an empty private one before activation.
    states: RwLock<Arc<PluginStateRegistry>>,
    /// The activation binding of a scoped definition; `None` for the default
    /// composition.
    scoped: Option<activation_binding::Scoped>,
}

impl Default for RecordingPlugin {
    fn default() -> Self {
        Self {
            owner: RwLock::new(None),
            binding: None,
            states: RwLock::new(Arc::new(PluginStateRegistry::new())),
            scoped: None,
        }
    }
}

fn slot_mut<T>(
    slot: &std::sync::RwLock<T>,
) -> std::result::Result<std::sync::RwLockWriteGuard<'_, T>, bmux_plugin_sdk::PluginCommandError> {
    slot.write()
        .map_err(|_| bmux_plugin_sdk::PluginCommandError::failed("plugin state lock poisoned"))
}

/// Product configuration selected through the actual lifecycle host port.
/// The host owns config resolution; the plugin owns recording construction.
#[derive(Clone)]
pub struct RecordingActivationConfiguration {
    /// Absolute recording roots and product-owned recording policy.
    pub config: RecordingPluginConfig,
    /// Actual sink registration and recording event authority for this activation.
    pub host: Arc<dyn RecordingHost>,
    /// Required on scoped definitions. None is reserved for legacy registration.
    /// The host must supply a caller backed by a real bound service port.
    pub process_caller: Option<Arc<bmux_plugin::TypedServiceCaller>>,
}

impl RecordingPlugin {
    fn states(&self) -> Arc<PluginStateRegistry> {
        self.states.read().map_or_else(
            |_| Arc::new(PluginStateRegistry::new()),
            |states| Arc::clone(&states),
        )
    }

    /// `events` is consulted only for the default composition, which publishes
    /// on the bus the activation received.
    fn configuration(
        &self,
        events: impl FnOnce() -> Option<Arc<bmux_plugin::EventBus>>,
    ) -> Result<RecordingActivationConfiguration, PluginCommandError> {
        if let Some(binding) = &self.binding {
            return binding
                .read()
                .map_err(|_| PluginCommandError::failed("recording configuration poisoned"))?
                .clone()
                .ok_or_else(|| {
                    PluginCommandError::unavailable("recording activation configuration not bound")
                });
        }
        let config = self
            .states()
            .get::<RecordingPluginConfig>()
            .ok_or_else(|| PluginCommandError::unavailable("recording configuration missing"))?;
        let config = config
            .read()
            .map_err(|_| PluginCommandError::failed("recording configuration poisoned"))?
            .clone();
        Ok(RecordingActivationConfiguration {
            config,
            host: Arc::new(ActivationRecordingHost {
                events: events(),
                states: self.states(),
            }),
            process_caller: None,
        })
    }
}

impl Plugin for RecordingPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_activate(Self::activate)
            .blocking_deactivate(Self::deactivate)
            .blocking_command("record", Self::run_command)
            .blocking_command("server-recording", Self::run_command)
            .blocking_command("playbook-from-recording", Self::run_command)
            .blocking_command("recording-start", Self::run_command)
            .blocking_command("recording-stop", Self::run_command)
            .blocking_command("recording-status", Self::run_command)
            .blocking_command("recording-path", Self::run_command)
            .blocking_command("recording-list", Self::run_command)
            .blocking_command("recording-delete", Self::run_command)
            .blocking_command("recording-delete-all", Self::run_command)
            .blocking_command("recording-cut", Self::run_command)
            .blocking_command("recording-inspect", Self::run_command)
            .blocking_command("recording-analyze", Self::run_command)
            .blocking_command("recording-replay", Self::run_command)
            .blocking_command("recording-verify-smoke", Self::run_command)
            .blocking_command("recording-export", Self::run_command)
            .blocking_command("recording-prune", Self::run_command)
            .blocking_command("recording-process-start", Self::run_command)
            .blocking_command("recording-process-run", Self::run_command)
            .blocking_command("recording-process-status", Self::run_command)
            .blocking_command("recording-process-read", Self::run_command)
            .blocking_command("recording-process-screen", Self::run_command)
            .blocking_command("recording-process-send", Self::run_command)
            .blocking_command("recording-process-wait", Self::run_command)
            .blocking_command("recording-process-terminate", Self::run_command)
            .blocking_command("recording-process-finish", Self::run_command)
            .blocking_services(Self::invoke_service, None)
            .typed_services(|plugin, context, registry| {
                if let (Some(_), Some(configuration)) = (&plugin.scoped, &plugin.binding) {
                    activation_binding::require_configuration(configuration)?;
                }
                plugin.register_typed_services(&context, registry);
                Ok(())
            });
    }

    fn activate(
        &self,
        cx: &mut Activation,
    ) -> impl std::future::Future<Output = author::Result<()>> + Send {
        std::future::ready(match (&self.scoped, &self.binding) {
            (Some(scoped), Some(configuration)) => scoped.bind(cx.lifecycle(), configuration),
            _ => Ok(()),
        })
    }

    fn deactivate(
        &self,
        cx: &mut Deactivation,
    ) -> impl std::future::Future<Output = author::Result<()>> + Send {
        std::future::ready(match &self.scoped {
            Some(scoped) => scoped.begin_cleanup(cx.lifecycle()),
            None => Ok(()),
        })
    }
}

impl RecordingPlugin {
    fn activate(
        &self,
        context: &bmux_plugin_sdk::NativeLifecycleContext,
        _tasks: Option<bmux_plugin_sdk::HostAsyncHandle>,
    ) -> std::result::Result<(), PluginCommandError> {
        if self.binding.is_none() {
            *slot_mut(&self.states)? = bmux_plugin::activation_state_registry(context);
        }
        let binding = self.configuration(|| bmux_plugin::activation_event_bus(context))?;
        let config = binding.config;
        let mut selected = self
            .owner
            .write()
            .map_err(|_| PluginCommandError::failed("recording owner poisoned"))?;
        let owner = selected.get_or_insert_with(|| RecordingOwner::new(binding.host, config));
        owner.activate_owner()?;
        drop(selected);
        if let Some(scoped) = &self.scoped {
            scoped.mark_active();
        }
        Ok(())
    }
    fn deactivate(
        &self,
        _context: &bmux_plugin_sdk::NativeLifecycleContext,
    ) -> std::result::Result<(), PluginCommandError> {
        let mut owner = self
            .owner
            .write()
            .map_err(|_| PluginCommandError::failed("recording owner poisoned"))?;
        owner
            .as_ref()
            .ok_or_else(|| PluginCommandError::unavailable("recording inactive"))?
            .deactivate_owner()?;
        *owner = None;
        drop(owner);
        if let (Some(scoped), Some(configuration)) = (&self.scoped, &self.binding) {
            scoped.finish_cleanup(configuration)?;
        }
        Ok(())
    }

    fn run_command(
        &self,
        context: &NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        if let Some(scoped) = &self.scoped {
            scoped.ready()?;
        }
        if context.command == "record" {
            return Ok(interactive_command::run(context));
        }
        if context.command.starts_with("recording-process-") {
            return Ok(process_commands::run(context));
        }
        let owner = self
            .owner
            .read()
            .map_err(|_| PluginCommandError::failed("recording owner poisoned"))?
            .clone()
            .ok_or_else(|| PluginCommandError::unavailable("recording inactive"))?;
        if should_queue_recording_cut_for_non_cli(context) {
            return owner.run_recording_cut_command_queued(context);
        }
        if context.command == "recording-export" {
            return owner.run_recording_export_command(context);
        }
        commands::run_recording_command(&owner, context)
    }

    // Keep generated interface routing together so each operation retains one
    // auditable typed dispatch boundary; splitting the route obscures fallback.
    #[allow(clippy::too_many_lines)]
    fn invoke_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        if let Some(Err(error)) = self.scoped.as_ref().map(activation_binding::Scoped::ready) {
            return ServiceResponse::error("recording_unavailable", error.message);
        }
        match self.owner.read() {
            Ok(owner) => match owner.as_ref() {
                Some(owner) => owner.service(context),
                None => ServiceResponse::error("recording_unavailable", "recording inactive"),
            },
            Err(_) => ServiceResponse::error("recording_unavailable", "recording owner poisoned"),
        }
    }

    fn register_typed_services(
        &self,
        context: &TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        // Activation already created the owner with its bus; a registration
        // without a prior activation has no bus to publish on.
        let Ok(binding) = self.configuration(|| None) else {
            return;
        };
        let config = binding.config;
        let owner = {
            let Ok(mut selected) = self.owner.write() else {
                return;
            };
            selected
                .get_or_insert_with(|| {
                    RecordingOwner::new(Arc::clone(&binding.host), config.clone())
                })
                .clone()
        };
        bmux_recording_plugin_api::recording_state::register_provider(
            registry,
            Arc::new(owner.clone()),
        )
        .expect("recording state schema capability must be valid");
        // Explicit plugin configuration establishes opt-in. Never infer isolation
        // from an arbitrary caller-supplied path or mutate a normal user runtime.
        let configured = context
            .plugin_settings_map
            .get("bmux.recording")
            .and_then(|settings| settings.get("process-runtime-root"))
            .and_then(|value| value.as_str());
        let isolated = configured.is_some_and(|root| root == context.connection.runtime_dir);
        let service = Arc::new(process_runtime::ProcessService::new(
            match binding.process_caller {
                Some(caller) => caller,
                None if self.binding.is_none() => Arc::new(
                    bmux_plugin::TypedServiceCaller::from_registration_context(context),
                ),
                None => return,
            },
            config.recordings_dir.clone(),
            std::path::Path::new(&context.connection.runtime_dir)
                .parent()
                .map_or_else(
                    || context.connection.runtime_dir.clone(),
                    |path| path.to_string_lossy().into_owned(),
                ),
            isolated,
            Arc::clone(&owner.manual),
        ));
        if let Ok(mut process) = owner.process.write() {
            *process = Some(Arc::clone(&service));
        }
        let presentation: Arc<dyn bmux_recording_plugin_api::recording_process_presentation_v1::RecordingProcessPresentationV1Service + Send + Sync> = service.clone();
        let _ = bmux_recording_plugin_api::recording_process_presentation_v1::register_provider(
            registry,
            presentation,
        );
        let provider: Arc<
            dyn bmux_recording_plugin_api::recording_process_v4::RecordingProcessV4Service
                + Send
                + Sync,
        > = service;
        let _ =
            bmux_recording_plugin_api::recording_process_v4::register_provider(registry, provider);
    }
}

/// Explicit owner-bound host mechanisms. The host installs/revokes the sink
/// using its activation registration port; no `TypeId` lookup is used by owners.
pub trait RecordingHost: Send + Sync {
    /// # Errors
    /// Reports unavailable or conflicting activation-owned sink registration.
    fn install_sink(&self, sink: RecordingSinkHandle) -> Result<(), String>;
    /// # Errors
    /// Reports failure to revoke the activation-owned sink.
    fn remove_sink(&self) -> Result<(), String>;
    /// # Errors
    /// Reports rejected or unavailable event publication.
    fn publish(&self, event: recording_events::RecordingEvent) -> Result<(), String>;
}
type PruneWorker = (
    Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
);

#[derive(Clone)]
pub struct RecordingOwner {
    host: Arc<dyn RecordingHost>,
    config: Arc<RwLock<RecordingPluginConfig>>,
    manual: Arc<RwLock<ManualRecordingRuntimeHandle>>,
    rolling: Arc<RwLock<RollingRecordingRuntimeHandle>>,
    jobs: Arc<Mutex<BTreeMap<uuid::Uuid, recording_types::RecordingJob>>>,
    cut_lock: Arc<Mutex<()>>,
    tasks: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    prune: Arc<Mutex<Option<PruneWorker>>>,
    process: Arc<RwLock<Option<Arc<process_runtime::ProcessService>>>>,
    cleanup_failed: Arc<std::sync::atomic::AtomicBool>,
    lifecycle: Arc<Mutex<()>>,
    activation_claimed: Arc<std::sync::atomic::AtomicBool>,
}
impl RecordingOwner {
    pub fn new(host: Arc<dyn RecordingHost>, config: RecordingPluginConfig) -> Self {
        let manual = Arc::new(Mutex::new(RecordingRuntime::new(
            config.recordings_dir.clone(),
            config.rolling_segment_mb,
            config.retention_days,
        )));
        let rolling = Arc::new(Mutex::new(if config.rolling_defaults.is_available() {
            Some(RecordingRuntime::new_rolling(
                config.rolling_recordings_dir.clone(),
                config.rolling_segment_mb,
                config.rolling_defaults.window_secs,
            ))
        } else {
            None
        }));
        Self {
            host,
            config: Arc::new(RwLock::new(config)),
            manual: Arc::new(RwLock::new(ManualRecordingRuntimeHandle(manual))),
            rolling: Arc::new(RwLock::new(RollingRecordingRuntimeHandle(rolling))),
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            cut_lock: Arc::new(Mutex::new(())),
            tasks: Arc::new(Mutex::new(Vec::new())),
            prune: Arc::new(Mutex::new(None)),
            process: Arc::new(RwLock::new(None)),
            cleanup_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lifecycle: Arc::new(Mutex::new(())),
            activation_claimed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
    fn activate_owner(&self) -> Result<(), PluginCommandError> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| PluginCommandError::failed("recording lifecycle poisoned"))?;
        // Claim before installing the sink or starting writers. Failed partial
        // activation remains owned until explicit successful cleanup.
        if self
            .cleanup_failed
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .activation_claimed
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(PluginCommandError::unavailable(
                "recording active or cleanup required",
            ));
        }
        let mut prune = self
            .prune
            .lock()
            .map_err(|_| PluginCommandError::failed("recording prune lock poisoned"))?;
        if prune.is_some() {
            return Err(PluginCommandError::failed("recording already active"));
        }
        let manual = Arc::clone(
            &self
                .manual
                .read()
                .map_err(|_| PluginCommandError::failed("manual handle poisoned"))?
                .0,
        );
        let rolling = Arc::clone(
            &self
                .rolling
                .read()
                .map_err(|_| PluginCommandError::failed("rolling handle poisoned"))?
                .0,
        );
        let config = self
            .config
            .read()
            .map_err(|_| PluginCommandError::failed("recording configuration poisoned"))?;
        self.host
            .install_sink(RecordingSinkHandle::from_arc(Arc::new(DualRuntimeSink {
                manual: Arc::clone(&manual),
                rolling: Arc::clone(&rolling),
            })))
            .map_err(PluginCommandError::failed)?;
        if let Some(options) = config.startup_recording.as_ref() {
            auto_start_manual(&manual, options)?;
        }
        // Auto-start applies only to an available selected capture runtime.
        // Standalone command hosts can carry enabled recording preferences but
        // no capture settings; preserve that distinction rather than inventing
        // a frame authority or failing unrelated recording-process commands.
        if config.rolling_auto_start && config.rolling_defaults.is_available() {
            auto_start_rolling(&rolling, &config.rolling_defaults)?;
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let task = std::thread::Builder::new()
            .name("recording-prune".into())
            .spawn(move || {
                while !stopped.load(std::sync::atomic::Ordering::Acquire) {
                    if let Ok(runtime) = manual.lock()
                        && let Err(error) = runtime.prune(None)
                    {
                        tracing::warn!(%error, "recording prune failed");
                    }
                    std::thread::park_timeout(std::time::Duration::from_secs(3600));
                }
            })
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        *prune = Some((stop, task));
        Ok(())
    }
    fn deactivate_owner(&self) -> Result<(), PluginCommandError> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| PluginCommandError::failed("recording lifecycle poisoned"))?;
        if self
            .cleanup_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(PluginCommandError::failed(
                "recording producer failed; cleanup requires explicit recovery",
            ));
        }
        // A managed process owns its recording writer until explicit finish or
        // recovery finalization. Plugin retirement cannot truncate that work.
        {
            let manual = self
                .manual
                .read()
                .map_err(|_| PluginCommandError::failed("manual handle poisoned"))?;
            let runtime = manual
                .0
                .lock()
                .map_err(|_| PluginCommandError::failed("manual runtime poisoned"))?;
            if let Some(active) = runtime.status().active
                && Path::new(&active.path).join("process-capture").exists()
            {
                return Err(PluginCommandError::unavailable(
                    "recording cleanup required: managed process capture must finish explicitly",
                ));
            }
        }
        if let Some((stop, task)) = self
            .prune
            .lock()
            .map_err(|_| PluginCommandError::failed("recording prune lock poisoned"))?
            .take()
        {
            stop.store(true, std::sync::atomic::Ordering::Release);
            task.thread().unpark();
            if task.join().is_err() {
                self.cleanup_failed
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(PluginCommandError::failed("recording prune task panicked"));
            }
        }
        // The lifecycle host has already closed invocation admission. Complete
        // admitted exports before stopping writers; failures retain owner state.
        let tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .map_err(|_| PluginCommandError::failed("recording task lock poisoned"))?,
        );
        let mut task_failed = false;
        for task in tasks {
            task_failed |= task.join().is_err();
        }
        if task_failed {
            self.cleanup_failed
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(PluginCommandError::failed(
                "recording export task panicked; cleanup required",
            ));
        }
        {
            let manual = self
                .manual
                .read()
                .map_err(|_| PluginCommandError::failed("manual handle poisoned"))?;
            let mut runtime = manual
                .0
                .lock()
                .map_err(|_| PluginCommandError::failed("manual runtime poisoned"))?;
            if runtime.status().active.is_some() {
                runtime
                    .stop(None)
                    .map_err(|error| PluginCommandError::failed(error.to_string()))?;
            }
        }
        {
            let rolling = self
                .rolling
                .read()
                .map_err(|_| PluginCommandError::failed("rolling handle poisoned"))?;
            let mut runtime = rolling
                .0
                .lock()
                .map_err(|_| PluginCommandError::failed("rolling runtime poisoned"))?;
            if let Some(runtime) = runtime.as_mut()
                && runtime.status().active.is_some()
            {
                runtime
                    .stop(None)
                    .map_err(|error| PluginCommandError::failed(error.to_string()))?;
            }
        }
        self.host
            .remove_sink()
            .map_err(PluginCommandError::failed)?;
        self.activation_claimed
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    fn dispatch_process<T: Send + 'static>(
        &self,
        context: &NativeServiceContext,
        operation: impl FnOnce(
            Arc<process_runtime::ProcessService>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>
        + Send
        + 'static,
    ) -> anyhow::Result<T> {
        let process = self
            .process
            .read()
            .map_err(|_| anyhow::anyhow!("process owner poisoned"))?
            .clone()
            .ok_or_else(|| anyhow::anyhow!("process service not bound"))?;
        process_runtime::dispatch_on(&process, context, operation)
    }
    // Keep the explicit operation table together; splitting it obscures the
    // checked interface/operation routing and shared owner delegation.
    #[allow(clippy::too_many_lines)]
    fn service(&self, context: &NativeServiceContext) -> ServiceResponse {
        use bmux_recording_plugin_api::recording_process_presentation_v1::RecordingProcessPresentationV1Service;
        use bmux_recording_plugin_api::recording_process_v4::{
            self as process, RecordingProcessV4Service,
        };
        bmux_plugin_sdk::route_service!(context, {
            "recording-process-live-presentation-v1", "capabilities" => |_req: (), ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.live_capabilities().await })).and_then(|value| value).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-live-presentation-v1", "read" => |req: bmux_recording_plugin_api::recording_process_live_presentation_v1::client::ReadRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.live_read(req.run_id, req.after, req.max_bytes).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-live-presentation-v1", "wait-present" => |req: bmux_recording_plugin_api::recording_process_live_presentation_v1::client::WaitPresentRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.live_wait_present(req.run_id, req.after, req.timeout_ms, req.max_bytes).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-human-input-v1", "capabilities" => |_req: (), ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.human_capabilities().await })).and_then(|value| value).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-human-input-v1", "open" => |req: bmux_recording_plugin_api::recording_process_human_input_v1::client::OpenRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.human_open(req.run_id).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-human-input-v1", "push" => |req: bmux_recording_plugin_api::recording_process_human_input_v1::client::PushRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.human_push(req.run_id, req.stream, req.sequence, req.data).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-human-input-v1", "close" => |req: bmux_recording_plugin_api::recording_process_human_input_v1::client::CloseRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.human_close(req.run_id, req.stream).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-presentation-wait-v1", "capabilities" => |_req: (), ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.wait_capabilities().await })).and_then(|value| value).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-presentation-wait-v1", "wait-present" => |req: bmux_recording_plugin_api::recording_process_presentation_wait_v1::client::WaitPresentRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.wait_present(req.run_id, req.after, req.timeout_ms, req.max_bytes).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-presentation-v1", "capabilities" => |_req: (), ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.capabilities().await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-presentation-v1", "resize" => |req: bmux_recording_plugin_api::recording_process_presentation_v1::client::ResizeRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.resize(req.run_id, req.format, req.cols, req.rows).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-presentation-v1", "present" => |req: bmux_recording_plugin_api::recording_process_presentation_v1::client::PresentRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.present(req.run_id).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-environment-v1", "capabilities" => |_req: (), ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.environment_capabilities().await })).and_then(|value| value).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-environment-v1", "start" => |req: bmux_recording_plugin_api::recording_process_environment_v1::client::StartRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.start_environment(req.launch_mutation_id, req.options, req.environment).await.map_err(|error| process::ProcessError::OperationFailed { reason: error.to_string() }) })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "start" => |req: process::client::StartRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.start(req.run_id, req.launch_mutation_id, req.options, req.runtime_root).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "reconcile" => |req: process::client::ReconcileRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.reconcile(req.run_id).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "status" => |req: process::client::StatusRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.status(req.run_id).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "read" => |req: process::client::ReadRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.read(req.run_id, req.since, req.max_bytes).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "screen" => |req: process::client::ScreenRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.screen(req.run_id).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "send" => |req: process::client::SendRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.send(req.run_id, req.data, req.mutation_id).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "input_receipt" => |req: process::client::InputReceiptRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.input_receipt(req.run_id, req.mutation_id).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "resolve" => |req: process::client::ResolveRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.resolve(req.run_id, req.mutation_id, req.expected_revision, req.action, req.input_mutation_id, req.grace_ms).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "wait" => |req: process::client::WaitRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.wait(req.run_id, req.timeout_ms).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "terminate" => |req: process::client::TerminateRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.terminate(req.run_id, req.grace_ms).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-process-v4", "finish" => |req: process::client::FinishRequest, ctx| {
                self.dispatch_process(ctx, move |service| Box::pin(async move { service.finish(req.run_id, req.expected_revision, req.terminate_running).await })).map_err(|error| ServiceResponse::error("process_dispatch", error.to_string()))
            },
            "recording-state", "status" => |_req: (), _ctx| {
                Ok::<recording_types::RecordingStatus, ServiceResponse>(self.recording_status_generated())
            },
            "recording-state", "list-recordings" => |_req: (), _ctx| {
                Ok::<Vec<recording_types::RecordingSummary>, ServiceResponse>(self.recording_list_generated())
            },
            "recording-state", "rolling-status" => |_req: (), _ctx| {
                Ok::<recording_types::RecordingRollingStatus, ServiceResponse>(self.recording_rolling_status_generated())
            },
            "recording-state", "capture-targets" => |_req: (), _ctx| {
                Ok::<Vec<recording_types::RecordingCaptureTarget>, ServiceResponse>(self.recording_capture_targets_generated())
            },
            "recording-state", "job-status" => |req: JobStatusRequest, _ctx| {
                Ok::<Option<recording_types::RecordingJob>, ServiceResponse>(self.recording_job_status_generated(req.job_id))
            },
            "recording-state", "list-jobs" => |_req: (), _ctx| {
                Ok::<Vec<recording_types::RecordingJob>, ServiceResponse>(self.recording_list_jobs_generated())
            },
            "recording-commands/v3", "start" => |req: StartRequest, _ctx| {
                Ok::<Result<recording_types::RecordingSummary, recording_types::RecordingError>, ServiceResponse>(
                    self.recording_start_generated(req),
                )
            },
            "recording-commands/v3", "stop" => |req: StopRequest, _ctx| {
                Ok::<Result<uuid::Uuid, recording_types::RecordingError>, ServiceResponse>(
                    self.recording_stop_generated(req.recording_id),
                )
            },
            "recording-commands/v3", "write-custom-event" => |req: WriteCustomEventRequest, _ctx| {
                Ok::<Result<(), recording_types::RecordingError>, ServiceResponse>(
                    self.recording_write_custom_event_generated(req),
                )
            },
            "recording-commands/v3", "delete" => |req: DeleteRequest, _ctx| {
                Ok::<Result<uuid::Uuid, recording_types::RecordingError>, ServiceResponse>(
                    self.recording_delete_generated(req.recording_id),
                )
            },
            "recording-commands/v3", "delete-all" => |_req: (), _ctx| {
                Ok::<u64, ServiceResponse>(self.recording_delete_all_generated())
            },
            "recording-commands/v3", "cut" => |req: CutRequest, _ctx| {
                Ok::<Result<recording_types::RecordingSummary, recording_types::RecordingError>, ServiceResponse>(
                    self.recording_cut_generated(req),
                )
            },
            "recording-commands/v3", "queue-cut" => |req: QueueCutRequest, _ctx| {
                self.recording_queue_cut_generated(req).map_err(|error| recording_error_response(&error))
            },
            "recording-commands/v3", "rolling-start" => |req: RollingStartRequest, _ctx| {
                Ok::<Result<recording_types::RecordingSummary, recording_types::RecordingError>, ServiceResponse>(
                    self.recording_rolling_start_generated(req.options),
                )
            },
            "recording-commands/v3", "rolling-stop" => |_req: (), _ctx| {
                Ok::<Result<uuid::Uuid, recording_types::RecordingError>, ServiceResponse>(
                    self.recording_rolling_stop_generated(),
                )
            },
            "recording-commands/v3", "rolling-clear" => |req: RollingClearRequest, _ctx| {
                Ok::<recording_types::RecordingRollingClearReport, ServiceResponse>(
                    self.recording_rolling_clear_generated(req.restart_if_active),
                )
            },
            "recording-commands/v3", "prune" => |req: PruneRequest, _ctx| {
                Ok::<u64, ServiceResponse>(self.recording_prune_generated(req.older_than_days))
            },
        })
    }

    fn config_handle(&self) -> Arc<RwLock<RecordingPluginConfig>> {
        Arc::clone(&self.config)
    }
    pub(crate) fn current_auto_export_settings(
        &self,
        fps_override: Option<u32>,
    ) -> AutoExportSettings {
        let config_handle = self.config_handle();
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
    fn cut_filesystem_lock(&self) -> &Mutex<()> {
        &self.cut_lock
    }
    fn handle_capture_targets(&self) -> Vec<RecordingCaptureTarget> {
        let mut targets: Vec<RecordingCaptureTarget> = Vec::new();

        let handle = self.manual_handle();
        if let Ok(guard) = handle.read()
            && let Ok(runtime) = guard.0.lock()
            && let Some((id, path)) = runtime.active_capture_target()
        {
            targets.push(RecordingCaptureTarget {
                recording_id: id,
                path: path.display().to_string(),
                rolling_window_secs: None,
            });
        }
        let handle = self.rolling_handle();
        if let Ok(guard) = handle.read()
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
    /// Announce a cut, then run it.
    fn announced_cut(
        &self,
        last_seconds: Option<u64>,
        name: Option<String>,
        start_epoch_ms: Option<u64>,
        end_epoch_ms: Option<u64>,
        without_input: bool,
    ) -> anyhow::Result<RecordingSummary> {
        self.publish_recording_cut_started(
            last_seconds,
            name.clone(),
            start_epoch_ms,
            end_epoch_ms,
        );
        self.handle_cut(
            cut_window(last_seconds, start_epoch_ms, end_epoch_ms),
            name,
            without_input,
        )
    }

    fn handle_cut(
        &self,
        window: anyhow::Result<CutWindow>,
        name: Option<String>,
        without_input: bool,
    ) -> anyhow::Result<RecordingSummary> {
        let window = window?;
        let config = self.config_handle();
        let cfg = config
            .read()
            .map_err(|_| anyhow::anyhow!("recording plugin config lock is poisoned"))?;
        let output_root = cfg.recordings_dir.clone();
        drop(cfg);

        let plan = {
            let handle = self.rolling_handle();
            let guard = handle.read().map_err(|_| {
                anyhow::anyhow!("recording rolling runtime handle lock is poisoned")
            })?;
            let mut rolling = guard
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("recording rolling runtime lock is poisoned"))?;
            let runtime = rolling.as_mut().ok_or_else(|| {
                anyhow::anyhow!("recording cut requires rolling recording mode to be enabled")
            })?;
            runtime.prepare_cut(output_root, window, name, without_input)?
        };

        let _cut_guard = self
            .cut_filesystem_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("recording cut filesystem lock is poisoned"))?;
        execute_recording_cut(plan)
    }
    fn handle_delete(&self, recording_id: uuid::Uuid) -> Option<uuid::Uuid> {
        let handle = self.manual_handle();
        let guard = handle.read().ok()?;
        let mut runtime = guard.0.lock().ok()?;
        runtime.delete(recording_id).ok().map(|summary| summary.id)
    }
    fn handle_delete_all(&self) -> usize {
        let handle = self.manual_handle();
        let Ok(guard) = handle.read() else {
            return 0;
        };
        let Ok(mut runtime) = guard.0.lock() else {
            return 0;
        };
        runtime.delete_all().unwrap_or(0)
    }
    fn handle_list(&self) -> Vec<RecordingSummary> {
        let handle = self.manual_handle();
        let Ok(guard) = handle.read() else {
            return Vec::new();
        };
        let Ok(runtime) = guard.0.lock() else {
            return Vec::new();
        };
        runtime.list().unwrap_or_default()
    }
    fn handle_prune(&self, older_than_days: Option<u64>) -> usize {
        let handle = self.manual_handle();
        let Ok(guard) = handle.read() else {
            return 0;
        };
        let Ok(runtime) = guard.0.lock() else {
            return 0;
        };
        runtime.prune(older_than_days).unwrap_or(0)
    }
    fn handle_rolling_clear(&self, restart_if_active: bool) -> RecordingRollingClearReport {
        let config = self.config_handle();
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
            let handle = self.rolling_handle();
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
            let Ok(_cut_guard) = self.cut_filesystem_lock().lock() else {
                return empty_clear_report();
            };
            if clear_rolling_root(&root).is_err() {
                return empty_clear_report();
            }
        }

        let (restarted, restarted_recording) = if restart_if_active && was_active {
            let settings = restart_settings.unwrap_or_else(|| defaults.clone());
            if settings.is_available() {
                let recording =
                    self.try_restart_rolling(&root, segment_mb, &settings, restart_name);
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
    fn handle_rolling_start(
        &self,
        options: RecordingRollingStartOptions,
    ) -> Option<RecordingSummary> {
        let config = self.config_handle();
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

        let handle = self.rolling_handle();
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
    fn handle_rolling_status(&self) -> RecordingRollingStatus {
        let config = self.config_handle();
        let Ok(cfg) = config.read() else {
            return empty_rolling_status(String::new());
        };
        let root_path = cfg.rolling_recordings_dir.display().to_string();
        let defaults = cfg.rolling_defaults.clone();
        drop(cfg);

        let usage = collect_rolling_usage(Path::new(&root_path)).unwrap_or_default();

        let handle = self.rolling_handle();
        let (active, rolling_window_secs, event_kinds) = (|| {
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
        })()
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
    fn handle_rolling_stop(&self) -> Option<uuid::Uuid> {
        let handle = self.rolling_handle();
        let guard = handle.read().ok()?;
        let mut rolling = guard.0.lock().ok()?;
        let runtime = rolling.as_mut()?;
        runtime.status().active.as_ref()?;
        runtime.stop(None).ok().map(|summary| summary.id)
    }
    fn handle_start(
        &self,
        session_id: Option<uuid::Uuid>,
        capture_input: bool,
        name: Option<String>,
        profile: Option<RecordingProfile>,
        event_kinds: Option<Vec<RecordingEventKind>>,
    ) -> Option<RecordingSummary> {
        let handle = self.manual_handle();
        let guard = handle.read().ok()?;
        let mut runtime = guard.0.lock().ok()?;

        let profile = profile.unwrap_or(RecordingProfile::Full);
        let event_kinds = event_kinds.unwrap_or_else(default_event_kinds);
        runtime
            .start(session_id, capture_input, name, profile, event_kinds)
            .ok()
    }
    fn handle_status(&self) -> RecordingStatus {
        let handle = self.manual_handle();
        let Ok(guard) = handle.read() else {
            return empty_status();
        };
        let Ok(runtime) = guard.0.lock() else {
            return empty_status();
        };
        runtime.status()
    }
    fn handle_stop(&self, recording_id: Option<uuid::Uuid>) -> Option<RecordingSummary> {
        let handle = self.manual_handle();
        let guard = handle.read().ok()?;
        let mut runtime = guard.0.lock().ok()?;
        runtime.stop(recording_id).ok()
    }
    fn handle_write_custom_event(
        &self,
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
        let handle = self.manual_handle();
        if let Ok(guard) = handle.read()
            && let Ok(runtime) = guard.0.lock()
            && let Ok(recorded) = runtime.record(RecordingEventKind::Custom, payload.clone(), meta)
        {
            accepted |= recorded;
        }
        let handle = self.rolling_handle();
        if let Ok(guard) = handle.read()
            && let Ok(rolling) = guard.0.lock()
            && let Some(runtime) = rolling.as_ref()
            && let Ok(recorded) = runtime.record(RecordingEventKind::Custom, payload, meta)
        {
            accepted |= recorded;
        }

        accepted
    }
    fn manual_handle(&self) -> Arc<RwLock<ManualRecordingRuntimeHandle>> {
        Arc::clone(&self.manual)
    }
    fn maybe_auto_export_stopped_recording(&self, summary: &RecordingSummary) {
        let export_settings = self.current_auto_export_settings(None);
        if !export_settings.enabled {
            tracing::info!(recording_id = %summary.id, "recording stop auto-export disabled");
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
            recording_id = %summary.id,
            output_path = %output,
            fps = export_settings.fps,
            "recording stop auto-export starting"
        );
        self.publish_recording_export_started(summary.id, output.clone());
        match export::export_recording_gif_for_recording_dir(
            &recording_dir,
            summary.id,
            &output,
            export_settings.fps,
        ) {
            Ok(()) => {
                tracing::info!(
                    recording_id = %summary.id,
                    output_path = %output,
                    "recording stop auto-export completed"
                );
                self.publish_recording_export_completed(summary.id, output);
            }
            Err(error) => {
                let reason = error.to_string();
                tracing::warn!(
                    recording_id = %summary.id,
                    output_path = %output,
                    error = %reason,
                    "recording stop auto-export failed"
                );
                self.publish_recording_export_failed(summary.id, output, reason);
            }
        }
    }
    fn publish_recording_cut_completed(&self, summary: &RecordingSummary) {
        let _ = self
            .host
            .publish(recording_events::RecordingEvent::CutCompleted {
                summary: summary.clone().into(),
            });
    }
    fn publish_recording_cut_failed(&self, reason: String) {
        let _ = self
            .host
            .publish(recording_events::RecordingEvent::CutFailed { reason });
    }
    fn publish_recording_cut_started(
        &self,
        last_seconds: Option<u64>,
        name: Option<String>,
        start_epoch_ms: Option<u64>,
        end_epoch_ms: Option<u64>,
    ) {
        let _ = self
            .host
            .publish(recording_events::RecordingEvent::CutStarted {
                last_seconds,
                name,
                start_epoch_ms,
                end_epoch_ms,
            });
    }
    pub(crate) fn publish_recording_export_completed(
        &self,
        recording_id: uuid::Uuid,
        output_path: String,
    ) {
        let _ = self
            .host
            .publish(recording_events::RecordingEvent::ExportCompleted {
                recording_id,
                output_path,
            });
    }
    pub(crate) fn publish_recording_export_failed(
        &self,
        recording_id: uuid::Uuid,
        output_path: String,
        reason: String,
    ) {
        let _ = self
            .host
            .publish(recording_events::RecordingEvent::ExportFailed {
                recording_id,
                output_path,
                reason,
            });
    }
    pub(crate) fn publish_recording_export_started(
        &self,
        recording_id: uuid::Uuid,
        output_path: String,
    ) {
        let _ = self
            .host
            .publish(recording_events::RecordingEvent::ExportStarted {
                recording_id,
                output_path,
            });
    }
    fn publish_recording_job_updated(&self, job: &recording_types::RecordingJob) {
        let _ = self
            .host
            .publish(recording_events::RecordingEvent::JobUpdated { job: job.clone() });
    }
    fn publish_recording_started(
        &self,
        recording: &RecordingSummary,
        rolling_window_secs: Option<u64>,
    ) {
        let target = recording_types::RecordingCaptureTarget {
            recording_id: recording.id,
            path: recording.path.clone(),
            rolling_window_secs,
        };
        let _ = self
            .host
            .publish(recording_events::RecordingEvent::Started { target });
    }
    fn publish_recording_stopped(&self, recording_id: uuid::Uuid) {
        let _ = self
            .host
            .publish(recording_events::RecordingEvent::Stopped { recording_id });
    }
    fn recording_capture_targets_generated(&self) -> Vec<recording_types::RecordingCaptureTarget> {
        self.handle_capture_targets()
            .into_iter()
            .map(Into::into)
            .collect()
    }
    fn recording_cut_generated(
        &self,
        req: CutRequest,
    ) -> Result<recording_types::RecordingSummary, recording_types::RecordingError> {
        match self.announced_cut(
            req.last_seconds,
            req.name,
            req.start_epoch_ms,
            req.end_epoch_ms,
            req.without_input.unwrap_or(false),
        ) {
            Ok(summary) => {
                self.publish_recording_cut_completed(&summary);
                Ok(summary.into())
            }
            Err(error) => {
                let reason = error.to_string();
                self.publish_recording_cut_failed(reason.clone());
                Err(generated_failed(reason))
            }
        }
    }
    fn recording_delete_all_generated(&self) -> u64 {
        u64::try_from(self.handle_delete_all()).unwrap_or(u64::MAX)
    }
    fn recording_delete_generated(
        &self,
        recording_id: uuid::Uuid,
    ) -> Result<uuid::Uuid, recording_types::RecordingError> {
        self.handle_delete(recording_id)
            .ok_or_else(|| generated_failed("recording delete failed"))
    }
    fn recording_job_status_generated(
        &self,
        job_id: uuid::Uuid,
    ) -> Option<recording_types::RecordingJob> {
        self.recording_jobs()
            .lock()
            .ok()
            .and_then(|jobs| jobs.get(&job_id).cloned())
    }
    fn recording_jobs(&self) -> &Mutex<BTreeMap<uuid::Uuid, recording_types::RecordingJob>> {
        &self.jobs
    }
    fn recording_list_generated(&self) -> Vec<recording_types::RecordingSummary> {
        self.handle_list().into_iter().map(Into::into).collect()
    }
    fn recording_list_jobs_generated(&self) -> Vec<recording_types::RecordingJob> {
        self.recording_jobs()
            .lock()
            .map(|jobs| jobs.values().cloned().collect())
            .unwrap_or_default()
    }
    fn recording_prune_generated(&self, older_than_days: Option<u64>) -> u64 {
        u64::try_from(self.handle_prune(older_than_days)).unwrap_or(u64::MAX)
    }
    fn recording_queue_cut_generated(
        &self,
        req: QueueCutRequest,
    ) -> Result<recording_types::RecordingJob, recording_types::RecordingError> {
        // Validate preconditions synchronously so the caller gets fast failure when
        // the plugin state is unavailable.
        let config_handle = Some(Arc::clone(&self.config))
            .ok_or_else(|| generated_failed("recording plugin config is unavailable"))?;
        if config_handle.read().is_err() {
            return Err(generated_failed("recording plugin config lock is poisoned"));
        }

        let rolling_handle = self.rolling_handle();
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

        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| generated_failed("recording tasks poisoned"))?;
        let mut index = 0;
        while index < tasks.len() {
            if tasks[index].is_finished() {
                tasks
                    .remove(index)
                    .join()
                    .map_err(|_| generated_failed("prior recording task panicked"))?;
            } else {
                index += 1;
            }
        }
        if tasks.len() >= 16 {
            return Err(generated_failed("recording task limit reached"));
        }
        if self
            .jobs
            .lock()
            .map_err(|_| generated_failed("recording jobs poisoned"))?
            .len()
            >= 256
        {
            return Err(generated_failed(
                "recording outcome retention full; new jobs unavailable",
            ));
        }
        let job = new_recording_job(recording_types::RecordingJobKind::Cut);
        self.upsert_recording_job(job.clone());
        self.publish_recording_job_updated(&job);
        let job_id = job.id;
        let owner = self.clone();
        match std::thread::Builder::new()
            .name("bmux-recording-cut-worker".to_string())
            .spawn(move || owner.run_queued_cut_job(job_id, req))
        {
            Ok(task) => tasks.push(task),
            Err(error) => {
                let reason = format!("failed spawning cut worker thread: {error}");
                let failed = self.update_recording_job(job_id, |job| {
                    job.status = recording_types::RecordingJobStatus::Failed;
                    job.error = Some(reason.clone());
                });
                self.publish_recording_job_updated(&failed);
                self.publish_recording_cut_failed(reason.clone());
                return Err(generated_failed(reason));
            }
        }

        Ok(job)
    }
    fn recording_rolling_clear_generated(
        &self,
        restart_if_active: bool,
    ) -> recording_types::RecordingRollingClearReport {
        let report = self.handle_rolling_clear(restart_if_active);
        if let Some(recording_id) = report.stopped_recording_id {
            self.publish_recording_stopped(recording_id);
        }
        if let Some(recording) = report.restarted_recording.as_ref() {
            self.publish_recording_started(
                recording,
                self.handle_rolling_status().rolling_window_secs,
            );
        }
        report.into()
    }
    fn recording_rolling_start_generated(
        &self,
        options: recording_types::RecordingRollingStartOptions,
    ) -> Result<recording_types::RecordingSummary, recording_types::RecordingError> {
        let recording = self
            .handle_rolling_start(options.into())
            .ok_or(recording_types::RecordingError::Unavailable)?;
        self.publish_recording_started(
            &recording,
            self.handle_rolling_status().rolling_window_secs,
        );
        Ok(recording.into())
    }
    fn recording_rolling_status_generated(&self) -> recording_types::RecordingRollingStatus {
        self.handle_rolling_status().into()
    }
    fn recording_rolling_stop_generated(
        &self,
    ) -> Result<uuid::Uuid, recording_types::RecordingError> {
        let recording_id = self
            .handle_rolling_stop()
            .ok_or(recording_types::RecordingError::NoActive)?;
        self.publish_recording_stopped(recording_id);
        Ok(recording_id)
    }
    fn recording_start_generated(
        &self,
        req: StartRequest,
    ) -> Result<recording_types::RecordingSummary, recording_types::RecordingError> {
        let recording = self
            .handle_start(
                req.session_id,
                req.capture_input,
                req.name,
                req.profile.map(Into::into),
                req.event_kinds
                    .map(|kinds| kinds.into_iter().map(Into::into).collect()),
            )
            .ok_or_else(|| generated_failed("recording start failed"))?;
        self.publish_recording_started(&recording, None);
        Ok(recording.into())
    }
    fn recording_status_generated(&self) -> recording_types::RecordingStatus {
        self.handle_status().into()
    }
    fn recording_stop_generated(
        &self,
        recording_id: Option<uuid::Uuid>,
    ) -> Result<uuid::Uuid, recording_types::RecordingError> {
        let summary = self
            .handle_stop(recording_id)
            .ok_or(recording_types::RecordingError::NoActive)?;
        let recording_id = summary.id;
        self.publish_recording_stopped(recording_id);
        self.maybe_auto_export_stopped_recording(&summary);
        Ok(recording_id)
    }
    fn recording_write_custom_event_generated(
        &self,
        req: WriteCustomEventRequest,
    ) -> Result<(), recording_types::RecordingError> {
        if self.handle_write_custom_event(
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
    pub(crate) fn recordings_root_for_command(
        &self,
        _context: &NativeCommandContext,
    ) -> std::result::Result<PathBuf, PluginCommandError> {
        Ok(self
            .config
            .read()
            .map_err(|_| PluginCommandError::failed("recording plugin config lock is poisoned"))?
            .recordings_dir
            .clone())
    }
    fn rolling_handle(&self) -> Arc<RwLock<RollingRecordingRuntimeHandle>> {
        Arc::clone(&self.rolling)
    }
    fn run_queued_cut_job(&self, job_id: uuid::Uuid, req: QueueCutRequest) {
        let cutting = self.update_recording_job(job_id, |job| {
            job.status = recording_types::RecordingJobStatus::Cutting;
        });
        self.publish_recording_job_updated(&cutting);

        let summary = match self.announced_cut(
            req.last_seconds,
            req.name,
            req.start_epoch_ms,
            req.end_epoch_ms,
            req.without_input.unwrap_or(false),
        ) {
            Ok(summary) => summary,
            Err(error) => {
                let reason = error.to_string();
                let failed = self.update_recording_job(job_id, |job| {
                    job.status = recording_types::RecordingJobStatus::Failed;
                    job.error = Some(reason.clone());
                });
                self.publish_recording_job_updated(&failed);
                self.publish_recording_cut_failed(reason);
                return;
            }
        };

        self.publish_recording_cut_completed(&summary);
        let cut_completed = self.update_recording_job(job_id, |job| {
            job.status = recording_types::RecordingJobStatus::CutCompleted;
            job.recording_id = Some(summary.id);
            job.recording_path = Some(summary.path.clone());
        });
        self.publish_recording_job_updated(&cut_completed);

        let export_settings = self.current_auto_export_settings(req.export_fps);
        if !export_settings.enabled {
            tracing::info!(
                job_id = %job_id,
                recording_id = %summary.id,
                "recording cut auto-export disabled"
            );
            let completed = self.update_recording_job(job_id, |job| {
                job.status = recording_types::RecordingJobStatus::Completed;
            });
            self.publish_recording_job_updated(&completed);
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
        self.publish_recording_export_started(summary.id, output.clone());
        let exporting = self.update_recording_job(job_id, |job| {
            job.status = recording_types::RecordingJobStatus::Exporting;
            job.export_output_path = Some(output.clone());
        });
        self.publish_recording_job_updated(&exporting);

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
                self.publish_recording_export_completed(summary.id, output);
                let completed = self.update_recording_job(job_id, |job| {
                    job.status = recording_types::RecordingJobStatus::Completed;
                });
                self.publish_recording_job_updated(&completed);
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
                self.publish_recording_export_failed(summary.id, output, reason.clone());
                let failed = self.update_recording_job(job_id, |job| {
                    job.status = recording_types::RecordingJobStatus::Failed;
                    job.error = Some(reason);
                });
                self.publish_recording_job_updated(&failed);
            }
        }
    }
    fn run_recording_cut_command_queued(
        &self,
        context: &NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        let last_seconds = parse_u64_option(&context.arguments, "last-seconds");
        let start_epoch_ms = parse_u64_option(&context.arguments, "start-epoch-ms");
        let end_epoch_ms = parse_u64_option(&context.arguments, "end-epoch-ms");
        let export_fps = parse_u32_option(&context.arguments, "export-fps");
        let name = parse_string_option(&context.arguments, "name");
        tracing::info!(
            invocation_source = ?context.invocation_source,
            ?last_seconds,
            ?export_fps,
            name = ?name,
            "recording cut enqueue requested"
        );

        let job = self
            .recording_queue_cut_generated(QueueCutRequest {
                last_seconds,
                name,
                export_fps,
                start_epoch_ms,
                end_epoch_ms,
                without_input: Some(context.arguments.iter().any(|arg| arg == "--without-input")),
            })
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
    fn run_recording_export_command(
        &self,
        context: &NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        reject_unsupported_recording_export_args(&context.arguments)?;
        let recording_id = first_positional_argument(&context.arguments).ok_or_else(|| {
            PluginCommandError::failed("recording export requires a recording id")
        })?;
        let format =
            parse_string_option(&context.arguments, "format").unwrap_or_else(|| "gif".to_string());
        if format != "gif" {
            return Err(PluginCommandError::failed(format!(
                "unsupported recording export format '{format}'; only gif is supported"
            )));
        }
        let output = parse_string_option(&context.arguments, "output").ok_or_else(|| {
            PluginCommandError::failed("recording export requires --output <PATH>")
        })?;
        let fps = parse_u32_option(&context.arguments, "fps")
            .unwrap_or(24)
            .max(1);
        let recordings_root = self.recordings_root_for_command(context)?;

        export::export_recording_gif_from_root(&recordings_root, &recording_id, &output, fps)
            .map_err(|error| {
                PluginCommandError::failed(format!("recording export failed: {error}"))
            })?;
        bmux_plugin_sdk::record_command_outcome_metadata(
            bmux_plugin_sdk::COMMAND_OUTCOME_STATUS_MESSAGE_KEY,
            serde_json::json!(format!("recording export complete: {output}")),
        );
        Ok(0)
    }
    fn try_restart_rolling(
        &self,
        root: &Path,
        segment_mb: usize,
        settings: &RollingRecordingSettings,
        name: Option<String>,
    ) -> Option<RecordingSummary> {
        let handle = self.rolling_handle();
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
    fn update_recording_job(
        &self,
        job_id: uuid::Uuid,
        update: impl FnOnce(&mut recording_types::RecordingJob),
    ) -> recording_types::RecordingJob {
        let mut jobs = self
            .recording_jobs()
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
    fn upsert_recording_job(&self, job: recording_types::RecordingJob) {
        if let Ok(mut jobs) = self.recording_jobs().lock() {
            jobs.insert(job.id, job);
        }
    }
}
fn should_queue_recording_cut_for_non_cli(context: &NativeCommandContext) -> bool {
    context.command == "recording-cut" && !context.invocation_source.is_cli()
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

fn auto_start_manual(
    manual_runtime: &Arc<Mutex<RecordingRuntime>>,
    options: &ManualRecordingStartOptions,
) -> Result<(), PluginCommandError> {
    let mut runtime = manual_runtime
        .lock()
        .map_err(|_| PluginCommandError::failed("manual recording runtime poisoned"))?;
    if runtime.status().active.is_some() {
        return Err(PluginCommandError::failed(
            "manual recording already active",
        ));
    }
    runtime
        .start(
            None,
            options.capture_input,
            options.name.clone(),
            options.profile.unwrap_or(RecordingProfile::Full),
            options
                .event_kinds
                .clone()
                .unwrap_or_else(default_event_kinds),
        )
        .map_err(|error| PluginCommandError::failed(error.to_string()))?;
    Ok(())
}
fn auto_start_rolling(
    rolling_runtime: &Arc<Mutex<Option<RecordingRuntime>>>,
    settings: &RollingRecordingSettings,
) -> Result<(), PluginCommandError> {
    let mut guard = rolling_runtime
        .lock()
        .map_err(|_| PluginCommandError::failed("rolling recording runtime poisoned"))?;
    let runtime = guard.as_mut().ok_or_else(|| {
        PluginCommandError::unavailable(
            "rolling recording requested without available capture settings",
        )
    })?;
    if runtime.status().active.is_some() {
        return Err(PluginCommandError::failed(
            "rolling recording already active",
        ));
    }
    runtime
        .start(
            None,
            settings.capture_input(),
            None,
            RecordingProfile::Full,
            settings.event_kinds.clone(),
        )
        .map_err(|error| PluginCommandError::failed(error.to_string()))?;
    Ok(())
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

pub(crate) struct AutoExportSettings {
    pub(crate) enabled: bool,
    pub(crate) output_dir: Option<PathBuf>,
    pub(crate) fps: u32,
}

pub(crate) fn auto_export_output_path(
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

// ────────────────────────────────────────────────────────────────────
// Registry lookup helpers
// ────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────
// Simple operations against the manual runtime
// ────────────────────────────────────────────────────────────────────

fn empty_status() -> RecordingStatus {
    RecordingStatus {
        active: None,
        queue_len: 0,
    }
}

// ────────────────────────────────────────────────────────────────────
// Custom event writes (both manual + rolling runtimes)
// ────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────
// Capture-targets query
// ────────────────────────────────────────────────────────────────────

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

/// Default composition: publishes on the bus this activation received.
/// Without one (for example a dynamic artifact) recording events are
/// dropped, never routed to a process-wide bus.
struct ActivationRecordingHost {
    events: Option<Arc<bmux_plugin::EventBus>>,
    states: Arc<PluginStateRegistry>,
}
impl RecordingHost for ActivationRecordingHost {
    fn install_sink(&self, sink: RecordingSinkHandle) -> Result<(), String> {
        if let Some(events) = &self.events {
            events
                .register_channel::<recording_events::RecordingEvent>(recording_events::EVENT_KIND);
        }
        self.states.register(&Arc::new(RwLock::new(sink)));
        Ok(())
    }
    fn remove_sink(&self) -> Result<(), String> {
        // Existing server-held sink handles share the owned stopped runtimes.
        // Revocation is supplied by activation-owned ports on the shared path.
        Ok(())
    }
    fn publish(&self, event: recording_events::RecordingEvent) -> Result<(), String> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        events
            .emit(&recording_events::EVENT_KIND, event)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

// The one definition both bundling and generated dynamic packaging use:
// fresh default author state per created instance.
bmux_plugin_sdk::plugin!(RecordingPlugin);

#[cfg(test)]
mod tests {
    use super::{RecordingPlugin, auto_export_output_path};
    use bmux_plugin_sdk::{
        CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostConnectionInfo, HostMetadata,
        NativeCommandContext, NativeCommandInvocationSource,
    };
    use std::collections::BTreeMap;
    use std::path::Path;

    struct Host;
    impl super::RecordingHost for Host {
        fn install_sink(&self, _sink: super::RecordingSinkHandle) -> Result<(), String> {
            Ok(())
        }
        fn remove_sink(&self) -> Result<(), String> {
            Ok(())
        }
        fn publish(&self, _event: super::recording_events::RecordingEvent) -> Result<(), String> {
            Ok(())
        }
    }
    fn owner(root: &Path) -> super::RecordingOwner {
        super::RecordingOwner::new(
            std::sync::Arc::new(Host),
            super::RecordingPluginConfig {
                recordings_dir: root.join("manual"),
                rolling_recordings_dir: root.join("rolling"),
                rolling_segment_mb: 8,
                retention_days: 7,
                rolling_defaults: super::RollingRecordingSettings {
                    window_secs: 0,
                    event_kinds: Vec::new(),
                },
                rolling_auto_start: false,
                auto_export: false,
                auto_export_dir: None,
                auto_export_fps: 24,
                startup_recording: None,
            },
        )
    }
    #[test]
    fn recording_owner_has_separate_writers_jobs_and_cut_lock() {
        let root = tempfile::tempdir().unwrap();
        let first = owner(&root.path().join("first"));
        let second = owner(&root.path().join("second"));
        assert!(!std::sync::Arc::ptr_eq(&first.manual, &second.manual));
        assert!(!std::sync::Arc::ptr_eq(&first.cut_lock, &second.cut_lock));
        first.jobs.lock().unwrap().insert(
            uuid::Uuid::new_v4(),
            super::new_recording_job(super::recording_types::RecordingJobKind::Cut),
        );
        assert!(second.jobs.lock().unwrap().is_empty());
    }
    #[test]
    fn standalone_activation_preserves_enabled_preference_without_capture_runtime() {
        let root = tempfile::tempdir().unwrap();
        let owner = owner(root.path());
        {
            let mut config = owner.config.write().unwrap();
            config.rolling_auto_start = true;
            assert!(!config.rolling_defaults.is_available());
        }
        owner.activate_owner().unwrap();
        assert!(owner.config.read().unwrap().rolling_auto_start);
        assert!(owner.rolling.read().unwrap().0.lock().unwrap().is_none());
        assert!(owner.prune.lock().unwrap().is_some());
        // An explicit request still rejects unavailable capture; only automatic
        // activation is conditional on the actual runtime's availability.
        let rolling = owner.rolling.read().unwrap().0.clone();
        let settings = owner.config.read().unwrap().rolling_defaults.clone();
        assert!(super::auto_start_rolling(&rolling, &settings).is_err());
        owner.deactivate_owner().unwrap();
    }

    #[test]
    fn prune_producer_is_retained_until_deactivation() {
        let root = tempfile::tempdir().unwrap();
        let owner = owner(root.path());
        owner.activate_owner().unwrap();
        assert!(owner.activate_owner().is_err());
        owner.deactivate_owner().unwrap();
        assert!(owner.prune.lock().unwrap().is_none());
    }
    #[test]
    fn retired_process_interfaces_are_rejected_by_actual_plugin_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let plugin = RecordingPlugin {
            owner: std::sync::RwLock::new(Some(owner(root.path()))),
            ..RecordingPlugin::default()
        };
        let command = command_context(
            "recording-process-status",
            &[],
            NativeCommandInvocationSource::AttachKeybinding,
        );
        for interface in [
            "recording-process-v1",
            "recording-process-recovery-v2",
            "recording-process-terminal-v3",
        ] {
            let mut context = queue_cut_service_context_from_command(&command);
            context.request.service.interface_id = interface.into();
            context.request.operation = "start".into();
            // Invalid bytes must never be decoded as v4 or routed to an alias.
            context.request.payload = vec![255];
            let response = plugin.invoke_service(&context);
            assert_eq!(response.payload, [] as [u8; 0]);
            let error = response
                .error
                .expect("retired interface must explicitly reject");
            assert_eq!(error.code, "unsupported_service_operation");
            assert!(error.message.contains(interface));
        }
    }

    #[test]
    fn cohesive_process_dispatch_uses_generated_v4_request_decoder() {
        let root = tempfile::tempdir().unwrap();
        let plugin = RecordingPlugin {
            owner: std::sync::RwLock::new(Some(owner(root.path()))),
            ..RecordingPlugin::default()
        };
        let command = command_context(
            "recording-process-status",
            &[],
            NativeCommandInvocationSource::AttachKeybinding,
        );
        let mut context = queue_cut_service_context_from_command(&command);
        context.request.service.interface_id =
            bmux_recording_plugin_api::recording_process_v4::INTERFACE_ID.into();
        context.request.operation = "start".into();
        context.request.payload = vec![255];
        let response = plugin.invoke_service(&context);
        let error = response
            .error
            .expect("malformed v4 request must reject before effect");
        assert_ne!(error.code, "unsupported_service_operation");
    }

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
        let plugin = RecordingPlugin::default();
        let error = plugin
            .run_command(&command_context(
                "recording-cut",
                &["--last-seconds", "30", "--export-fps", "12"],
                NativeCommandInvocationSource::AttachKeybinding,
            ))
            .expect_err("recording cut should report missing queue precondition");

        assert!(
            error.to_string().contains("recording inactive"),
            "unexpected error: {error}"
        );
    }

    fn queue_cut_service_context_from_command(
        context: &NativeCommandContext,
    ) -> bmux_plugin_sdk::NativeServiceContext {
        bmux_plugin_sdk::NativeServiceContext {
            plugin_id: context.plugin_id.clone(),
            request: bmux_plugin_sdk::ServiceRequest {
                caller_plugin_id: context.plugin_id.clone(),
                service: bmux_plugin_sdk::RegisteredService {
                    capability: bmux_plugin_sdk::HostScope::new("bmux.recording.write").unwrap(),
                    kind: bmux_plugin_sdk::ServiceKind::Command,
                    interface_id: "recording-commands/v3".into(),
                    provider: bmux_plugin_sdk::ProviderId::Plugin(context.plugin_id.clone()),
                },
                operation: "queue-cut".into(),
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
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: context.host_kernel_bridge.clone(),
            host_service_port: None,
        }
    }

    pub(crate) fn command_context(
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
            host_service_port: None,
            host_kernel_bridge: None,
        }
    }
}

#[cfg(test)]
mod canonical_metadata_tests {
    #[test]
    fn embedded_manifest_matches_definition_metadata() {
        let definition = super::plugin_definition();
        definition
            .validate()
            .expect("canonical embedded identity must validate");
    }
}

#[cfg(test)]
mod cut_window_tests {
    use super::{CutWindow, cut_window};

    #[test]
    fn wire_fields_select_exactly_one_window() {
        assert_eq!(
            cut_window(Some(30), None, None).unwrap(),
            CutWindow::LastSeconds(Some(30))
        );
        assert_eq!(
            cut_window(None, None, None).unwrap(),
            CutWindow::LastSeconds(None)
        );
        assert_eq!(
            cut_window(None, Some(10), Some(20)).unwrap(),
            CutWindow::Range {
                start_epoch_ms: 10,
                end_epoch_ms: Some(20)
            }
        );
        assert!(cut_window(Some(30), Some(10), None).is_err());
        assert!(cut_window(None, None, Some(20)).is_err());
    }
}
