use crate::connection::{
    ConnectionPolicyScope, ServerRuntimeMetadata, connect, connect_if_running, connect_raw,
    current_cli_build_id, map_client_connect_error, read_server_runtime_metadata,
    remove_server_runtime_metadata_file, write_server_runtime_metadata,
};
use crate::input::{InputProcessor, Keymap, RuntimeAction};
use crate::status::{AttachTab, build_attach_status_line};
use anyhow::{Context, Result};
use bmux_cli_schema::{
    Cli, Command, KeymapCommand, LogLevel, LogsCommand, LogsProfilesCommand, PlaybookCommand,
    RecordingCommand, RecordingCursorBlinkMode, RecordingCursorMode, RecordingCursorProfile,
    RecordingCursorShape, RecordingEventKindArg, RecordingExportFormat, RecordingProfileArg,
    RecordingRenderMode, RecordingReplayMode, ServerCommand, SessionCommand, TerminalCommand,
    TraceFamily,
};
use bmux_client::{AttachLayoutState, AttachSnapshotState, BmuxClient, ClientError};
use bmux_config::{
    BmuxConfig, ConfigPaths, RecordingExportCursorBlinkMode, RecordingExportCursorMode,
    RecordingExportCursorProfile, RecordingExportCursorShape, ResolvedTimeout, TerminfoAutoInstall,
};
use bmux_ipc::{
    AttachRect, AttachViewComponent, ContextSelector, ContextSummary, InvokeServiceKind,
    PaneFocusDirection, PaneSelector, PaneSplitDirection, RecordingEventEnvelope,
    RecordingEventKind, RecordingPayload, RecordingStatus, RecordingSummary, SessionSelector,
    SessionSummary,
};
use bmux_keybind::action_to_config_name;
use bmux_plugin::{
    PluginManifest, PluginRegistry, load_registered_plugin as load_native_registered_plugin,
};
use bmux_plugin_sdk::{
    CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostConnectionInfo, HostMetadata,
    HostScope, NativeCommandContext, NativeLifecycleContext, PluginCommandEffect,
    PluginCommandOutcome, PluginEvent, PluginEventKind, RegisteredService, ServiceKind,
    ServiceRequest,
};
use bmux_server::BmuxServer;
use clap::{CommandFactory, FromArgMatches};
use crossterm::cursor::{MoveTo, SavePosition, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::terminal::{Clear, ClearType};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use gif::{Encoder as GifEncoder, Frame as GifFrame, Repeat};
use std::cell::RefCell;
use std::io::{self, BufWriter, IsTerminal, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::{Duration as TimeDuration, OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::{Level, debug, trace, warn};
use uuid::Uuid;

use bmux_server::ServiceInvokeContext;

thread_local! {
    static SERVICE_KERNEL_CONTEXT: RefCell<Option<ServiceInvokeContext>> = const { RefCell::new(None) };
    static HOST_KERNEL_CONNECTION: RefCell<Option<HostConnectionInfo>> = const { RefCell::new(None) };
    static HOST_KERNEL_EFFECT_CAPTURE: RefCell<Option<Vec<PluginCommandEffect>>> = const { RefCell::new(None) };
}

struct ServiceKernelContextGuard;
struct HostKernelConnectionGuard;

static EFFECTIVE_LOG_LEVEL: OnceLock<Level> = OnceLock::new();

static LOG_WRITER_GUARD: OnceLock<moosicbox_log_runtime::init::LoggingHandle> = OnceLock::new();

impl Drop for ServiceKernelContextGuard {
    fn drop(&mut self) {
        SERVICE_KERNEL_CONTEXT.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

impl Drop for HostKernelConnectionGuard {
    fn drop(&mut self) {
        HOST_KERNEL_CONNECTION.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

fn enter_service_kernel_context(context: ServiceInvokeContext) -> ServiceKernelContextGuard {
    SERVICE_KERNEL_CONTEXT.with(|slot| {
        *slot.borrow_mut() = Some(context);
    });
    ServiceKernelContextGuard
}

fn enter_host_kernel_connection(connection: HostConnectionInfo) -> HostKernelConnectionGuard {
    HOST_KERNEL_CONNECTION.with(|slot| {
        *slot.borrow_mut() = Some(connection);
    });
    HostKernelConnectionGuard
}

fn begin_host_kernel_effect_capture() {
    HOST_KERNEL_EFFECT_CAPTURE.with(|slot| {
        *slot.borrow_mut() = Some(Vec::new());
    });
}

fn record_host_kernel_effect(effect: PluginCommandEffect) {
    HOST_KERNEL_EFFECT_CAPTURE.with(|slot| {
        if let Some(captured) = slot.borrow_mut().as_mut() {
            captured.push(effect);
        }
    });
}

fn finish_host_kernel_effect_capture() -> Vec<PluginCommandEffect> {
    HOST_KERNEL_EFFECT_CAPTURE
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_default()
}

fn maybe_record_host_kernel_effect(request: &bmux_ipc::Request, response: &bmux_ipc::Response) {
    let effect = match (request, response) {
        (
            bmux_ipc::Request::CreateContext { .. },
            bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextCreated { context }),
        ) => Some(PluginCommandEffect::SelectContext {
            context_id: context.id,
        }),
        (
            bmux_ipc::Request::SelectContext { .. },
            bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextSelected { context }),
        ) => Some(PluginCommandEffect::SelectContext {
            context_id: context.id,
        }),
        _ => None,
    };
    if let Some(effect) = effect {
        record_host_kernel_effect(effect);
    }
}

fn call_host_kernel_via_client(
    connection: &HostConnectionInfo,
    payload: Vec<u8>,
) -> Result<Vec<u8>> {
    let request: bmux_ipc::Request =
        bmux_ipc::decode(&payload).context("failed decoding kernel bridge request payload")?;
    let paths = ConfigPaths::new(
        connection.config_dir.clone().into(),
        connection.runtime_dir.clone().into(),
        connection.data_dir.clone().into(),
        connection.state_dir.clone().into(),
    );
    let request_name = "bmux-cli-host-kernel-bridge".to_string();
    let response: bmux_ipc::Response = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                let mut client = BmuxClient::connect_with_paths(&paths, &request_name).await?;
                client.request_raw(request.clone()).await
            })
        })
    } else {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed creating kernel bridge runtime")?;
        runtime.block_on(async {
            let mut client = BmuxClient::connect_with_paths(&paths, &request_name).await?;
            client.request_raw(request.clone()).await
        })
    }?;
    maybe_record_host_kernel_effect(&request, &response);
    bmux_ipc::encode(&response).context("failed encoding kernel bridge response payload")
}

unsafe extern "C" fn host_kernel_bridge(
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    if input_ptr.is_null() || output_len.is_null() {
        return 2;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let request: bmux_plugin_sdk::HostKernelBridgeRequest =
        match bmux_plugin_sdk::decode_service_message(input) {
            Ok(value) => value,
            Err(_) => return 3,
        };

    let payload = if let Some(context) = SERVICE_KERNEL_CONTEXT.with(|slot| slot.borrow().clone()) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(async { context.execute_raw(request.payload).await })
            })
        } else {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => return 5,
            };
            runtime.block_on(async { context.execute_raw(request.payload).await })
        }
    } else if let Some(connection) = HOST_KERNEL_CONNECTION.with(|slot| slot.borrow().clone()) {
        call_host_kernel_via_client(&connection, request.payload)
    } else {
        return 5;
    };

    let response = match payload {
        Ok(payload) => bmux_plugin_sdk::HostKernelBridgeResponse { payload },
        Err(_) => return 5,
    };

    let encoded = match bmux_plugin_sdk::encode_service_message(&response) {
        Ok(value) => value,
        Err(_) => return 5,
    };

    unsafe {
        *output_len = encoded.len();
    }
    if output_ptr.is_null() || encoded.len() > output_capacity {
        return 4;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }
    0
}

mod attach;
mod built_in_commands;
mod cli_parse;
mod dispatch;
mod logs_cli;
mod logs_watch;
mod playbook_cli;
mod plugin_commands;
mod plugin_host;
mod plugin_runtime;
mod recording;
mod server_commands;
mod server_runtime;
mod session_cli;
mod session_follow;
mod terminal_doctor;
mod terminal_protocol;

#[cfg(test)]
pub use self::logs_watch::{
    LogFilterCaseMode, LogFilterKind, LogFilterRule, compile_filter_regex, line_visible_in_watch,
    logs_watch_filter_rule_to_state, logs_watch_filter_state_to_rule, normalize_logs_watch_profile,
};
use self::logs_watch::{
    active_log_file_path, run_logs_profiles_delete, run_logs_profiles_list,
    run_logs_profiles_rename, run_logs_profiles_show, run_logs_watch,
};
use attach::cursor::apply_attach_cursor_state;
use attach::events::{AttachLoopControl, AttachLoopEvent, collect_attach_loop_events};
use attach::render::{
    AttachLayer, AttachLayerSurface, append_pane_output, opaque_row_text, queue_layer_fill,
    render_attach_scene, visible_scene_pane_ids,
};
use attach::runtime::*;
use attach::state::{
    AttachEventAction, AttachExitReason, AttachScrollbackCursor, AttachScrollbackPosition,
    AttachUiMode, AttachViewState, PaneRect,
};
use built_in_commands::{BuiltInHandlerId, built_in_command_by_handler};
use cli_parse::*;
use dispatch::*;
use logs_cli::*;
use playbook_cli::*;
use plugin_commands::PluginCommandRegistry;
use plugin_runtime::*;
use server_commands::*;
use server_runtime::*;
use session_cli::*;
use session_follow::*;
use terminal_doctor::*;
use terminal_protocol::{
    ProtocolDirection, ProtocolProfile, ProtocolTraceEvent, primary_da_for_profile,
    protocol_profile_name, secondary_da_for_profile, supported_query_names,
};

const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(200);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_STATUS_TIMEOUT: Duration = Duration::from_millis(1000);
const SERVER_STOP_TIMEOUT: Duration = Duration::from_millis(5000);
const VERIFY_SERVER_START_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);
const ATTACH_IO_POLL_INTERVAL: Duration = Duration::from_millis(15);
const ATTACH_SNAPSHOT_MAX_BYTES_PER_PANE: usize = 1_048_576;
const ATTACH_SCROLLBACK_UNAVAILABLE_STATUS: &str = "scrollback unavailable for focused pane";
const ATTACH_SELECTION_STARTED_STATUS: &str = "selection started";
const ATTACH_SELECTION_CLEARED_STATUS: &str = "selection cleared";
const ATTACH_SELECTION_COPIED_STATUS: &str = "selection copied";
const ATTACH_SELECTION_EMPTY_STATUS: &str = "no selection";
const ATTACH_TRANSIENT_STATUS_TTL: Duration = Duration::from_millis(1800);
const ATTACH_WELCOME_STATUS_TTL: Duration = Duration::from_millis(2600);
const HELP_OVERLAY_SURFACE_ID: Uuid = Uuid::from_u128(1);
fn core_provided_capabilities() -> Vec<HostScope> {
    [
        "bmux.commands",
        "bmux.config.read",
        "bmux.events.subscribe",
        "bmux.key_actions",
        "bmux.status_bar_items",
        "bmux.storage",
        "bmux.logs.write",
        "bmux.clients.read",
        "bmux.contexts.read",
        "bmux.contexts.write",
        "bmux.sessions.read",
        "bmux.sessions.write",
        "bmux.panes.read",
        "bmux.panes.write",
        "bmux.follow.read",
        "bmux.follow.write",
        "bmux.persistence.read",
        "bmux.persistence.write",
        "bmux.attach.overlay",
        "bmux.terminal.observe",
        "bmux.terminal.input_intercept",
        "bmux.terminal.output_intercept",
        "bmux.recording.write",
    ]
    .into_iter()
    .map(|scope| HostScope::new(scope).expect("supported plugin host scope should parse"))
    .collect()
}

fn core_service_descriptors() -> Vec<RegisteredService> {
    vec![
        RegisteredService {
            capability: HostScope::new("bmux.config.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "config-query/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.storage").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "storage-query/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.storage").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "storage-command/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.logs.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "logging-command/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.clients.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "client-query/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.contexts.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "context-query/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.contexts.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "context-command/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.sessions.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "session-query/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.sessions.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "session-command/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.panes.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "pane-query/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.panes.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "pane-command/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.recording.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "recording-command/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
    ]
}

fn available_capability_providers(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<std::collections::BTreeMap<HostScope, bmux_plugin::CapabilityProvider>> {
    let enabled_plugins = effective_enabled_plugins(config, registry);
    registry
        .capability_providers_for(&enabled_plugins, &core_provided_capabilities())
        .context("failed resolving capability providers")
}

fn available_service_descriptors(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<Vec<RegisteredService>> {
    let enabled_plugins = effective_enabled_plugins(config, registry);
    let mut services = core_service_descriptors();
    services.extend(
        registry
            .service_providers_for(&enabled_plugins)
            .context("failed resolving service providers")?
            .into_values()
            .map(|provider| provider.service),
    );
    Ok(services)
}

const fn invoke_kind_from_service_kind(kind: ServiceKind) -> Option<InvokeServiceKind> {
    match kind {
        ServiceKind::Query => Some(InvokeServiceKind::Query),
        ServiceKind::Command => Some(InvokeServiceKind::Command),
        ServiceKind::Event => None,
    }
}

fn register_plugin_service_handlers(
    server: &BmuxServer,
    config: &BmuxConfig,
    paths: &ConfigPaths,
    registry: &PluginRegistry,
) -> Result<()> {
    let enabled_plugins = effective_enabled_plugins(config, registry);
    let available_capabilities = available_capability_providers(config, registry)?;
    let services = available_service_descriptors(config, registry)?;
    let plugin_search_roots = resolve_plugin_search_paths(config, paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let connection_info = bmux_plugin_sdk::HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let available_capability_names = available_capabilities
        .keys()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    for service in services {
        let Some(invoke_kind) = invoke_kind_from_service_kind(service.kind) else {
            continue;
        };
        let bmux_plugin_sdk::ProviderId::Plugin(provider_plugin_id) = service.provider.clone()
        else {
            continue;
        };
        let Some(provider) = registry.get(&provider_plugin_id) else {
            continue;
        };

        let provider = provider.clone();
        let host = plugin_host_metadata();
        let available_capabilities_for_handler = available_capabilities.clone();
        let services_for_handler = available_service_descriptors(config, registry)?;
        let capability_names_for_handler = available_capability_names.clone();
        let plugin_search_roots_for_handler = plugin_search_roots.clone();
        let config_for_handler = config.clone();
        let connection_info_for_handler = connection_info.clone();
        let enabled_plugins_for_handler = enabled_plugins.clone();

        server.register_service_handler(
            service.capability.as_str().to_string(),
            invoke_kind,
            service.interface_id.clone(),
            "*",
            move |route, invoke_context, payload| {
                let provider = provider.clone();
                let host = host.clone();
                let available_capabilities = available_capabilities_for_handler.clone();
                let services = services_for_handler.clone();
                let capability_names = capability_names_for_handler.clone();
                let plugin_search_roots = plugin_search_roots_for_handler.clone();
                let config = config_for_handler.clone();
                let connection = connection_info_for_handler.clone();
                let enabled_plugins = enabled_plugins_for_handler.clone();
                async move {
                    let loaded = load_plugin(&provider, &host, &available_capabilities)
                        .with_context(|| {
                            format!(
                                "failed loading service provider plugin '{}'",
                                provider.declaration.id.as_str()
                            )
                        })?;
                    let _kernel_context_guard =
                        enter_service_kernel_context(invoke_context.clone());
                    let _host_kernel_connection_guard =
                        enter_host_kernel_connection(connection.clone());
                    let response =
                        loaded.invoke_service(&bmux_plugin_sdk::NativeServiceContext {
                            plugin_id: provider.declaration.id.as_str().to_string(),
                            request: ServiceRequest {
                                caller_plugin_id: "bmux.core".to_string(),
                                service: RegisteredService {
                                    capability: HostScope::new(route.capability.as_str())?,
                                    kind: match route.kind {
                                        InvokeServiceKind::Query => ServiceKind::Query,
                                        InvokeServiceKind::Command => ServiceKind::Command,
                                    },
                                    interface_id: route.interface_id,
                                    provider: bmux_plugin_sdk::ProviderId::Plugin(
                                        provider.declaration.id.as_str().to_string(),
                                    ),
                                },
                                operation: route.operation,
                                payload,
                            },
                            required_capabilities: provider
                                .declaration
                                .required_capabilities
                                .iter()
                                .map(ToString::to_string)
                                .collect(),
                            provided_capabilities: provider
                                .declaration
                                .provided_capabilities
                                .iter()
                                .map(ToString::to_string)
                                .collect(),
                            services,
                            available_capabilities: capability_names,
                            enabled_plugins,
                            plugin_search_roots,
                            host,
                            connection,
                            settings: config
                                .plugins
                                .settings
                                .get(provider.declaration.id.as_str())
                                .cloned(),
                            plugin_settings_map: config.plugins.settings.clone(),
                            host_kernel_bridge: Some(bmux_plugin_sdk::HostKernelBridge::from_fn(
                                host_kernel_bridge,
                            )),
                        })?;
                    if let Some(error) = response.error {
                        anyhow::bail!(error.message);
                    }

                    Ok(response.payload)
                }
            },
        )?;
    }

    Ok(())
}

fn service_descriptors_from_declarations<'a>(
    declarations: impl IntoIterator<Item = &'a bmux_plugin::PluginDeclaration>,
) -> Vec<RegisteredService> {
    let mut services = core_service_descriptors();
    services.extend(declarations.into_iter().flat_map(|declaration| {
        declaration
            .services
            .iter()
            .map(move |service| RegisteredService {
                capability: service.capability.clone(),
                kind: service.kind,
                interface_id: service.interface_id.clone(),
                provider: bmux_plugin_sdk::ProviderId::Plugin(declaration.id.as_str().to_string()),
            })
    }));
    services
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalProfile {
    Bmux256Color,
    Screen256Color,
    Xterm256Color,
    Conservative,
}

pub async fn run() -> Result<u8> {
    match parse_runtime_cli()? {
        ParsedRuntimeCli::BuiltIn {
            cli,
            log_level,
            verbose,
        } => {
            init_logging(verbose, Some(log_level));
            validate_record_bootstrap_flags(&cli)?;

            if let Some(command) = &cli.command {
                return run_command(command).await;
            }

            let options = DefaultAttachOptions {
                record: cli.record,
                capture_input: !cli.no_capture_input,
                profile: cli.record_profile,
                event_kinds: cli.record_event_kind.clone(),
                recording_id_file: cli.recording_id_file.clone(),
                stop_server_on_exit: cli.stop_server_on_exit,
            };
            run_default_server_attach(options).await
        }
        ParsedRuntimeCli::Plugin {
            log_level,
            plugin_id,
            command_name,
            arguments,
        } => {
            init_logging(false, Some(log_level));
            run_plugin_command(&plugin_id, &command_name, &arguments).await
        }
        ParsedRuntimeCli::ImmediateExit {
            code,
            output,
            stderr,
        } => {
            if stderr {
                eprint!("{output}");
            } else {
                print!("{output}");
            }
            Ok(code)
        }
    }
}

#[derive(Debug, Clone)]
struct DefaultAttachOptions {
    record: bool,
    capture_input: bool,
    profile: Option<RecordingProfileArg>,
    event_kinds: Vec<RecordingEventKindArg>,
    recording_id_file: Option<String>,
    stop_server_on_exit: bool,
}

#[derive(Debug, Clone)]
struct AttachDisplayCapturePlan {
    recording_id: Uuid,
    recording_path: PathBuf,
}

async fn run_default_server_attach(options: DefaultAttachOptions) -> Result<u8> {
    if options.record {
        ensure_server_not_running_for_record_bootstrap().await?;
    }
    ensure_server_running_for_default_attach().await?;

    let mut active_recording_id = None;
    let mut capture_plan = None;
    if options.record {
        let mut recording_client = connect(
            ConnectionPolicyScope::Normal,
            "bmux-cli-default-attach-recording-start",
        )
        .await?;
        let started = recording_client
            .recording_start(
                None,
                options.capture_input,
                recording::recording_profile_arg_to_ipc(options.profile),
                recording::resolve_event_kind_override(
                    options.profile,
                    &options.event_kinds,
                    options.capture_input,
                ),
            )
            .await
            .map_err(map_cli_client_error)?;
        active_recording_id = Some(started.id);
        capture_plan = Some(AttachDisplayCapturePlan {
            recording_id: started.id,
            recording_path: PathBuf::from(&started.path),
        });
        println!(
            "recording started: {} (capture_input={})",
            started.id, started.capture_input
        );
        if let Some(path) = options.recording_id_file.as_deref() {
            std::fs::write(path, format!("{}\n", started.id))
                .with_context(|| format!("failed writing recording id file {path}"))?;
        }
    }

    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-default-attach").await?;
    let target = resolve_default_attach_target(&mut client).await?;
    let target = target.to_string();
    let attach_result =
        run_session_attach_with_client(client, Some(target.as_str()), None, false, capture_plan)
            .await;

    if let Some(recording_id) = active_recording_id {
        let mut stop_client = connect(
            ConnectionPolicyScope::Normal,
            "bmux-cli-default-attach-recording-stop",
        )
        .await?;
        let stopped_id = stop_client
            .recording_stop(Some(recording_id))
            .await
            .map_err(map_cli_client_error)
            .with_context(|| format!("failed stopping recording {recording_id}"))?;
        let mut list_client = connect(
            ConnectionPolicyScope::Normal,
            "bmux-cli-default-attach-recording-list",
        )
        .await?;
        let recording = list_client
            .recording_list()
            .await
            .map_err(map_cli_client_error)?
            .into_iter()
            .find(|summary| summary.id == stopped_id);
        if let Some(recording) = recording {
            println!(
                "recording stopped: {} events={} bytes={} path={}",
                recording.id, recording.event_count, recording.payload_bytes, recording.path
            );
        } else {
            println!("recording stopped: {stopped_id}");
        }
    }

    if options.record && options.stop_server_on_exit {
        let _ = run_server_stop().await;
    }

    attach_result
}

async fn ensure_server_not_running_for_record_bootstrap() -> Result<()> {
    if server_is_running().await? {
        anyhow::bail!(
            "--record requires a fresh start but server is already running; stop it first or run without --record"
        )
    }
    Ok(())
}

async fn ensure_server_running_for_default_attach() -> Result<()> {
    if server_is_running().await? {
        return Ok(());
    }

    let _ = run_server_start(true, false).await?;
    if !server_is_running().await? {
        anyhow::bail!("bmux server failed to start for default attach")
    }
    Ok(())
}

async fn resolve_default_attach_target(client: &mut BmuxClient) -> Result<Uuid> {
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;

    if sessions.is_empty() {
        let name = next_default_session_name(&sessions);
        let id = client
            .new_session(Some(name.clone()))
            .await
            .map_err(map_cli_client_error)?;
        return Ok(id);
    }

    let _client_id = client.whoami().await.map_err(map_cli_client_error)?;
    let writable_sessions = sessions.clone();

    if writable_sessions.is_empty() {
        let name = next_default_session_name(&sessions);
        let id = client
            .new_session(Some(name.clone()))
            .await
            .map_err(map_cli_client_error)?;
        return Ok(id);
    }

    let mut sorted = writable_sessions;
    sorted.sort_by(|left, right| {
        let left_key = left.name.as_deref().unwrap_or("");
        let right_key = right.name.as_deref().unwrap_or("");
        left_key.cmp(right_key).then_with(|| left.id.cmp(&right.id))
    });

    let session = sorted
        .into_iter()
        .next()
        .expect("non-empty sessions should have first entry");
    Ok(session.id)
}

fn next_default_session_name(sessions: &[SessionSummary]) -> String {
    let mut next = 1_u32;
    loop {
        let candidate = format!("session-{next}");
        if sessions
            .iter()
            .all(|session| session.name.as_deref() != Some(candidate.as_str()))
        {
            return candidate;
        }
        next = next.saturating_add(1);
    }
}

async fn run_server_start(daemon: bool, foreground_internal: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    if server_is_running().await? {
        println!("bmux server is already running");
        return Ok(1);
    }

    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    validate_enabled_plugins(&config, &registry)?;
    let _preloaded_plugins = load_enabled_plugins(&config, &registry)?;

    if daemon && !foreground_internal {
        let executable =
            std::env::current_exe().context("failed to resolve bmux executable path")?;
        let mut child = ProcessCommand::new(executable);
        let log_level = EFFECTIVE_LOG_LEVEL.get().copied().unwrap_or(Level::INFO);
        child
            .arg("server")
            .arg("start")
            .arg("--foreground-internal")
            .env(
                "BMUX_LOG_LEVEL",
                match log_level {
                    Level::ERROR => "error",
                    Level::WARN => "warn",
                    Level::INFO => "info",
                    Level::DEBUG => "debug",
                    Level::TRACE => "trace",
                },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = child.spawn().context("failed to spawn background server")?;
        write_server_pid_file(child.id())?;
        write_server_runtime_metadata(child.id())?;

        if !wait_for_server_running(SERVER_START_TIMEOUT).await? {
            let _ = try_kill_pid(child.id());
            let _ = remove_server_pid_file();
            anyhow::bail!("background server did not become ready before timeout")
        }

        println!("bmux server started in daemon mode (pid {})", child.id());
        return Ok(0);
    }

    let loaded_plugins = load_enabled_plugins(&config, &registry)?;
    activate_loaded_plugins(&loaded_plugins, &config, &paths)?;
    dispatch_loaded_plugin_event(&loaded_plugins, plugin_system_event("server_starting"))?;
    let server = BmuxServer::from_config_paths(&paths);
    register_plugin_service_handlers(&server, &config, &paths, &registry)?;
    write_server_pid_file(std::process::id())?;
    write_server_runtime_metadata(std::process::id())?;
    dispatch_loaded_plugin_event(&loaded_plugins, plugin_system_event("server_started"))?;
    let run_result = if loaded_plugins.is_empty() {
        server.run().await
    } else {
        let (plugin_bridge_shutdown_tx, plugin_bridge_shutdown_rx) =
            tokio::sync::watch::channel(false);
        let plugin_bridge = plugin_event_bridge_loop(&loaded_plugins, plugin_bridge_shutdown_rx);
        tokio::pin!(plugin_bridge);
        tokio::select! {
            result = server.run() => {
                let _ = plugin_bridge_shutdown_tx.send(true);
                result
            }
            result = &mut plugin_bridge => {
                let _ = plugin_bridge_shutdown_tx.send(true);
                result?;
                Ok(())
            }
        }
    };
    if let Err(error) =
        dispatch_loaded_plugin_event(&loaded_plugins, plugin_system_event("server_stopping"))
    {
        warn!("failed delivering server_stopping plugin event: {error}");
    }
    if let Err(error) = deactivate_loaded_plugins(&loaded_plugins, &config, &paths) {
        warn!("failed deactivating plugins during server shutdown: {error}");
    }
    let _ = remove_server_pid_file();
    run_result?;
    Ok(0)
}

async fn run_recording_start(
    session_id: Option<&str>,
    capture_input: bool,
    profile: Option<RecordingProfileArg>,
    event_kinds: &[RecordingEventKindArg],
) -> Result<u8> {
    recording::run_recording_start(session_id, capture_input, profile, event_kinds).await
}

async fn run_recording_stop(recording_id: Option<&str>) -> Result<u8> {
    recording::run_recording_stop(recording_id).await
}

async fn run_recording_status(as_json: bool) -> Result<u8> {
    recording::run_recording_status(as_json).await
}

async fn run_recording_list(as_json: bool) -> Result<u8> {
    recording::run_recording_list(as_json).await
}

async fn run_recording_delete(recording_id_or_prefix: &str) -> Result<u8> {
    recording::run_recording_delete(recording_id_or_prefix).await
}

async fn run_recording_delete_all(yes: bool) -> Result<u8> {
    recording::run_recording_delete_all(yes).await
}

fn run_recording_inspect(
    recording_id: &str,
    limit: usize,
    kind: Option<&str>,
    as_json: bool,
) -> Result<u8> {
    recording::run_recording_inspect(recording_id, limit, kind, as_json)
}

async fn run_recording_replay(
    recording_id: &str,
    mode: RecordingReplayMode,
    speed: f64,
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<u8> {
    recording::run_recording_replay(
        recording_id,
        mode,
        speed,
        target_bmux,
        compare_recording,
        ignore,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
    )
    .await
}

async fn run_recording_verify_smoke(
    recording_id: &str,
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<u8> {
    recording::run_recording_verify_smoke(
        recording_id,
        target_bmux,
        compare_recording,
        ignore,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
    )
    .await
}

async fn run_recording_export(
    recording_id: &str,
    format: RecordingExportFormat,
    output: &str,
    view_client: Option<&str>,
    speed: f64,
    fps: u32,
    max_duration: Option<u64>,
    max_frames: Option<u32>,
    renderer: RecordingRenderMode,
    cell_size: Option<(u16, u16)>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
    font_family: Option<&str>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    font_path: &[String],
    cursor: Option<RecordingCursorMode>,
    cursor_shape: Option<RecordingCursorShape>,
    cursor_blink: Option<RecordingCursorBlinkMode>,
    cursor_blink_period_ms: Option<u32>,
    cursor_color: Option<&str>,
    cursor_profile: Option<RecordingCursorProfile>,
    cursor_solid_after_activity_ms: Option<u32>,
    export_metadata: Option<&str>,
    show_progress: bool,
) -> Result<u8> {
    let paths = ConfigPaths::default();
    let config = BmuxConfig::load_from_path(&paths.config_file()).unwrap_or_default();
    let export_defaults = &config.recording.export;

    let resolved_cursor = cursor.unwrap_or(match export_defaults.cursor {
        RecordingExportCursorMode::Auto => RecordingCursorMode::Auto,
        RecordingExportCursorMode::On => RecordingCursorMode::On,
        RecordingExportCursorMode::Off => RecordingCursorMode::Off,
    });
    let resolved_cursor_shape = cursor_shape.unwrap_or(match export_defaults.cursor_shape {
        RecordingExportCursorShape::Auto => RecordingCursorShape::Auto,
        RecordingExportCursorShape::Block => RecordingCursorShape::Block,
        RecordingExportCursorShape::Bar => RecordingCursorShape::Bar,
        RecordingExportCursorShape::Underline => RecordingCursorShape::Underline,
    });
    let resolved_cursor_blink = cursor_blink.unwrap_or(match export_defaults.cursor_blink {
        RecordingExportCursorBlinkMode::Auto => RecordingCursorBlinkMode::Auto,
        RecordingExportCursorBlinkMode::On => RecordingCursorBlinkMode::On,
        RecordingExportCursorBlinkMode::Off => RecordingCursorBlinkMode::Off,
    });
    let resolved_cursor_blink_period_ms =
        cursor_blink_period_ms.unwrap_or(export_defaults.cursor_blink_period_ms.max(1));
    let resolved_cursor_color = cursor_color
        .map(str::to_string)
        .or_else(|| {
            let value = export_defaults.cursor_color.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
        .unwrap_or_else(|| "auto".to_string());
    let resolved_cursor_profile = cursor_profile.unwrap_or(match export_defaults.cursor_profile {
        RecordingExportCursorProfile::Auto => RecordingCursorProfile::Auto,
        RecordingExportCursorProfile::Ghostty => RecordingCursorProfile::Ghostty,
        RecordingExportCursorProfile::Generic => RecordingCursorProfile::Generic,
    });
    let resolved_cursor_solid_after_activity_ms =
        cursor_solid_after_activity_ms.or(export_defaults.cursor_solid_after_activity_ms);

    recording::run_recording_export(
        recording_id,
        format,
        output,
        view_client,
        speed,
        fps,
        max_duration,
        max_frames,
        renderer,
        cell_size,
        cell_width,
        cell_height,
        font_family,
        font_size,
        line_height,
        font_path,
        resolved_cursor,
        resolved_cursor_shape,
        resolved_cursor_blink,
        resolved_cursor_blink_period_ms,
        &resolved_cursor_color,
        resolved_cursor_profile,
        resolved_cursor_solid_after_activity_ms,
        export_metadata,
        show_progress,
    )
    .await
}

fn replay_watch(events: &[RecordingEventEnvelope], speed: f64) -> Result<u8> {
    let clamped_speed = if speed <= 0.0 { 1.0 } else { speed };
    let mut last_ns = 0_u64;
    let mut stdout = io::stdout().lock();
    for event in events {
        if event.mono_ns > last_ns {
            let delta = event.mono_ns.saturating_sub(last_ns);
            let delay = (delta as f64 / clamped_speed) as u64;
            if delay > 0 {
                std::thread::sleep(Duration::from_nanos(delay));
            }
        }
        match &event.payload {
            RecordingPayload::Bytes { data }
                if matches!(
                    event.kind,
                    RecordingEventKind::PaneOutputRaw | RecordingEventKind::ProtocolReplyRaw
                ) =>
            {
                stdout.write_all(data)?;
            }
            _ => {}
        }
        last_ns = event.mono_ns;
    }
    stdout.flush()?;
    Ok(0)
}

#[derive(Debug, Clone, serde::Serialize)]
struct VerifySmokeReport {
    pass: bool,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_binary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compare_recording: Option<String>,
    strict_timing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_verify_duration_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify_start_timeout_secs: Option<u64>,
    ignored_kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mismatch_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_output_len: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_output_len: Option<usize>,
    monotonic_timeline: bool,
}

async fn replay_verify(
    baseline: &[RecordingEventEnvelope],
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<u8> {
    let report = verify_recording_report(
        baseline,
        target_bmux,
        compare_recording,
        ignore,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
    )
    .await?;

    if let Some(target_binary) = &report.target_binary {
        println!("verify target binary: {target_binary}");
    }

    if report.pass {
        println!("verify PASS: {}", report.reason);
        return Ok(0);
    }

    if let (Some(index), Some(expected), Some(actual), Some(expected_kind), Some(actual_kind)) = (
        report.mismatch_index,
        report.expected_seq,
        report.actual_seq,
        report.expected_kind.as_ref(),
        report.actual_kind.as_ref(),
    ) {
        println!(
            "verify FAIL: mismatch at index {index} expected_seq={expected} actual_seq={actual} expected_kind={expected_kind} actual_kind={actual_kind}"
        );
        return Ok(1);
    }
    if let (Some(expected), Some(actual)) = (report.expected_output_len, report.actual_output_len) {
        println!("verify FAIL: output length mismatch expected={expected} actual={actual}");
        return Ok(1);
    }
    println!("verify FAIL: {}", report.reason);
    Ok(1)
}

async fn verify_recording_report(
    baseline: &[RecordingEventEnvelope],
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<VerifySmokeReport> {
    let ignore_rules = parse_ignore_rules(ignore);
    let baseline_filtered = apply_ignore_rules(baseline, &ignore_rules);
    if let Some(other_id) = compare_recording {
        let other = load_recording_events(other_id)?;
        let other_filtered = apply_ignore_rules(&other, &ignore_rules);
        let mismatch = baseline_filtered
            .iter()
            .zip(other_filtered.iter())
            .position(|(left, right)| left != right);
        if let Some(index) = mismatch {
            let expected = &baseline_filtered[index];
            let actual = &other_filtered[index];
            return Ok(VerifySmokeReport {
                pass: false,
                reason: "recordings diverged".to_string(),
                target_binary: None,
                compare_recording: Some(other_id.to_string()),
                strict_timing,
                max_verify_duration_secs,
                verify_start_timeout_secs,
                ignored_kinds: ignore_rules,
                mismatch_index: Some(index),
                expected_seq: Some(expected.seq),
                actual_seq: Some(actual.seq),
                expected_kind: Some(recording_event_kind_name(expected.kind)),
                actual_kind: Some(recording_event_kind_name(actual.kind)),
                expected_output_len: Some(baseline_filtered.len()),
                actual_output_len: Some(other_filtered.len()),
                monotonic_timeline: true,
            });
        }
        if baseline_filtered.len() != other_filtered.len() {
            return Ok(VerifySmokeReport {
                pass: false,
                reason: "recordings length mismatch".to_string(),
                target_binary: None,
                compare_recording: Some(other_id.to_string()),
                strict_timing,
                max_verify_duration_secs,
                verify_start_timeout_secs,
                ignored_kinds: ignore_rules,
                mismatch_index: None,
                expected_seq: None,
                actual_seq: None,
                expected_kind: None,
                actual_kind: None,
                expected_output_len: Some(baseline_filtered.len()),
                actual_output_len: Some(other_filtered.len()),
                monotonic_timeline: true,
            });
        }
        return Ok(VerifySmokeReport {
            pass: true,
            reason: "recordings are identical".to_string(),
            target_binary: None,
            compare_recording: Some(other_id.to_string()),
            strict_timing,
            max_verify_duration_secs,
            verify_start_timeout_secs,
            ignored_kinds: ignore_rules,
            mismatch_index: None,
            expected_seq: None,
            actual_seq: None,
            expected_kind: None,
            actual_kind: None,
            expected_output_len: Some(baseline_filtered.len()),
            actual_output_len: Some(other_filtered.len()),
            monotonic_timeline: true,
        });
    }

    let target_binary = match target_bmux {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe().context("failed resolving current bmux binary")?,
    };
    let input_timeline = input_timeline(&baseline_filtered);
    let first_input_ns = input_timeline.first().map(|event| event.mono_ns);
    let expected_output = first_input_ns.map_or_else(Vec::new, |min_ns| {
        expected_output_bytes(&baseline_filtered, Some(min_ns))
    });
    // Extract viewport dimensions from recording (first AttachSetViewport request).
    let viewport = extract_viewport_from_events(&baseline_filtered);
    let actual_output = run_target_verify_capture(
        &target_binary,
        &input_timeline,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
        viewport,
    )
    .await?;

    // Compare output: first try byte-exact, then fall back to structural
    // (vt100-rendered) comparison which tolerates byte-level differences from
    // timing/chunking while catching actual content divergence.
    let byte_mismatch = expected_output
        .iter()
        .zip(actual_output.iter())
        .position(|(left, right)| left != right);
    let length_mismatch = expected_output.len() != actual_output.len();

    if byte_mismatch.is_some() || length_mismatch {
        // Byte comparison failed — try structural comparison via vt100.
        let (vp_cols, vp_rows) = viewport.unwrap_or((120, 40));
        let expected_text = render_output_via_vt100(&expected_output, vp_cols, vp_rows);
        let actual_text = render_output_via_vt100(&actual_output, vp_cols, vp_rows);

        // Normalize both: collapse digit sequences, strip trailing whitespace.
        let expected_norm = normalize_screen_text(&expected_text);
        let actual_norm = normalize_screen_text(&actual_text);

        if expected_norm != actual_norm {
            let mismatch_detail = find_text_mismatch(&expected_norm, &actual_norm);
            return Ok(VerifySmokeReport {
                pass: false,
                reason: format!("output mismatch (structural comparison): {mismatch_detail}"),
                target_binary: Some(target_binary.display().to_string()),
                compare_recording: None,
                strict_timing,
                max_verify_duration_secs,
                verify_start_timeout_secs,
                ignored_kinds: ignore_rules,
                mismatch_index: byte_mismatch,
                expected_seq: None,
                actual_seq: None,
                expected_kind: None,
                actual_kind: None,
                expected_output_len: Some(expected_output.len()),
                actual_output_len: Some(actual_output.len()),
                monotonic_timeline: true,
            });
        }
        // Structural comparison passed — byte differences were cosmetic.
    }

    let monotonic = baseline_filtered
        .windows(2)
        .all(|pair| pair[1].seq > pair[0].seq && pair[1].mono_ns >= pair[0].mono_ns);
    if !monotonic {
        return Ok(VerifySmokeReport {
            pass: false,
            reason: "non-monotonic sequence or timestamp ordering".to_string(),
            target_binary: Some(target_binary.display().to_string()),
            compare_recording: None,
            strict_timing,
            max_verify_duration_secs,
            verify_start_timeout_secs,
            ignored_kinds: ignore_rules,
            mismatch_index: None,
            expected_seq: None,
            actual_seq: None,
            expected_kind: None,
            actual_kind: None,
            expected_output_len: Some(expected_output.len()),
            actual_output_len: Some(actual_output.len()),
            monotonic_timeline: false,
        });
    }
    Ok(VerifySmokeReport {
        pass: true,
        reason: "target output and timeline integrity checks succeeded".to_string(),
        target_binary: Some(target_binary.display().to_string()),
        compare_recording: None,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
        ignored_kinds: ignore_rules,
        mismatch_index: None,
        expected_seq: None,
        actual_seq: None,
        expected_kind: None,
        actual_kind: None,
        expected_output_len: Some(expected_output.len()),
        actual_output_len: Some(actual_output.len()),
        monotonic_timeline: true,
    })
}

#[derive(Debug, Clone)]
struct ReplayInputEvent {
    mono_ns: u64,
    data: Vec<u8>,
}

fn expected_output_bytes(events: &[RecordingEventEnvelope], min_mono_ns: Option<u64>) -> Vec<u8> {
    let mut output = Vec::new();
    for event in events {
        if let Some(min_mono_ns) = min_mono_ns
            && event.mono_ns < min_mono_ns
        {
            continue;
        }
        if matches!(event.kind, RecordingEventKind::PaneOutputRaw)
            && let RecordingPayload::Bytes { data } = &event.payload
        {
            output.extend_from_slice(data);
        }
    }
    output
}

fn input_timeline(events: &[RecordingEventEnvelope]) -> Vec<ReplayInputEvent> {
    events
        .iter()
        .filter_map(|event| {
            if !matches!(event.kind, RecordingEventKind::PaneInputRaw) {
                return None;
            }
            match &event.payload {
                RecordingPayload::Bytes { data } => Some(ReplayInputEvent {
                    mono_ns: event.mono_ns,
                    data: data.clone(),
                }),
                _ => None,
            }
        })
        .collect()
}

/// Extract viewport dimensions from recording events by finding the first
/// `AttachSetViewport` request. Returns `None` if no viewport was recorded.
fn extract_viewport_from_events(events: &[RecordingEventEnvelope]) -> Option<(u16, u16)> {
    for event in events {
        if let (
            RecordingEventKind::RequestStart,
            RecordingPayload::RequestStart { request_data, .. },
        ) = (&event.kind, &event.payload)
        {
            if request_data.is_empty() {
                continue;
            }
            if let Ok(request) = bmux_ipc::decode::<bmux_ipc::Request>(request_data) {
                if let bmux_ipc::Request::AttachSetViewport { cols, rows, .. } = request {
                    return Some((cols, rows));
                }
            }
        }
    }
    None
}

/// Render raw output bytes through a vt100 terminal emulator and return the
/// visible screen text.
fn render_output_via_vt100(output: &[u8], cols: u16, rows: u16) -> String {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(output);
    let screen = parser.screen();
    let mut lines = Vec::new();
    for row in 0..rows {
        lines.push(screen.contents_between(row, 0, row, cols));
    }
    // Trim trailing empty lines.
    while lines.last().map_or(false, |l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Normalize screen text for structural comparison: collapse digit sequences
/// to a placeholder, trim trailing whitespace per line.
fn normalize_screen_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            let trimmed = line.trim_end();
            // Replace sequences of digits with a placeholder to tolerate PIDs,
            // timestamps, and other non-deterministic numeric values.
            let mut result = String::new();
            let mut chars = trimmed.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch.is_ascii_digit() {
                    while chars.peek().map_or(false, |c| c.is_ascii_digit()) {
                        chars.next();
                    }
                    result.push_str("<N>");
                } else {
                    result.push(ch);
                }
            }
            result
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find the first line where two texts differ and return a human-readable
/// description.
fn find_text_mismatch(expected: &str, actual: &str) -> String {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    for (i, (e, a)) in expected_lines.iter().zip(actual_lines.iter()).enumerate() {
        if e != a {
            return format!(
                "line {}: expected {:?}, got {:?}",
                i + 1,
                truncate_str(e, 80),
                truncate_str(a, 80)
            );
        }
    }
    if expected_lines.len() != actual_lines.len() {
        return format!(
            "line count: expected {}, got {}",
            expected_lines.len(),
            actual_lines.len()
        );
    }
    "unknown difference".to_string()
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len])
    } else {
        s.to_string()
    }
}

async fn run_target_verify_capture(
    target_binary: &Path,
    inputs: &[ReplayInputEvent],
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
    viewport: Option<(u16, u16)>,
) -> Result<Vec<u8>> {
    let max_verify_duration = max_verify_duration_secs.map(Duration::from_secs);
    let (paths, root_dir) = verify_temp_paths();
    paths
        .ensure_dirs()
        .context("failed preparing verify temp paths")?;
    write_verify_config(&paths)?;

    let verify_start_timeout =
        verify_start_timeout_secs.map_or(VERIFY_SERVER_START_TIMEOUT_DEFAULT, Duration::from_secs);
    let mut server = start_verify_server(target_binary, &paths, &root_dir, verify_start_timeout)
        .await
        .with_context(|| format!("verify startup failed; artifacts at {}", root_dir.display()))?;

    let run_result = async {
        wait_for_verify_server_ready(&paths, Duration::from_secs(5)).await?;
        let mut client = BmuxClient::connect_with_paths(&paths, "bmux-cli-recording-verify")
            .await
            .map_err(map_cli_client_error)?;
        let session_id = client
            .new_session(Some("verify-replay".to_string()))
            .await
            .map_err(map_cli_client_error)?;
        let grant = client
            .attach_grant(SessionSelector::ById(session_id))
            .await
            .map_err(map_cli_client_error)?;
        let attach = client
            .open_attach_stream_info(&grant)
            .await
            .map_err(map_cli_client_error)?;
        let (vp_cols, vp_rows) = viewport.unwrap_or((120, 40));
        let _ = client
            .attach_set_viewport(attach.session_id, vp_cols, vp_rows)
            .await
            .map_err(map_cli_client_error);

        let mut output = Vec::new();
        let mut last_input_ns = 0_u64;
        let verify_started = Instant::now();
        for input in inputs {
            if let Some(limit) = max_verify_duration
                && verify_started.elapsed() > limit
            {
                anyhow::bail!(
                    "verify aborted after exceeding max duration of {}s",
                    limit.as_secs()
                );
            }
            if input.mono_ns > last_input_ns {
                let delta = input.mono_ns.saturating_sub(last_input_ns);
                let sleep_ns = if strict_timing {
                    delta
                } else {
                    delta.min(25_000_000)
                };
                if sleep_ns > 0 {
                    tokio::time::sleep(Duration::from_nanos(sleep_ns)).await;
                }
            }
            if !input.data.is_empty() {
                client
                    .attach_input(attach.session_id, input.data.clone())
                    .await
                    .map_err(map_cli_client_error)?;
            }
            let _ = collect_attach_output_until_idle(
                &mut client,
                attach.session_id,
                &mut output,
                Duration::from_millis(500),
            )
            .await;
            last_input_ns = input.mono_ns;
        }
        for _ in 0..6 {
            if let Some(limit) = max_verify_duration
                && verify_started.elapsed() > limit
            {
                anyhow::bail!(
                    "verify aborted after exceeding max duration of {}s",
                    limit.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = collect_attach_output_until_idle(
            &mut client,
            attach.session_id,
            &mut output,
            Duration::from_millis(600),
        )
        .await;
        Ok::<Vec<u8>, anyhow::Error>(output)
    }
    .await;

    let stop_result = server.shutdown().await;
    if run_result.is_ok() && stop_result.is_ok() {
        let _ = std::fs::remove_dir_all(&root_dir);
    } else {
        warn!(
            "recording verify artifacts retained at {}",
            root_dir.display()
        );
        warn!(
            "recording verify server stdout log: {}",
            server.stdout_log_path().display()
        );
        warn!(
            "recording verify server stderr log: {}",
            server.stderr_log_path().display()
        );
    }

    if let Err(error) = stop_result {
        return Err(error).with_context(|| {
            format!(
                "verify server shutdown failed; artifacts at {} (stdout: {}, stderr: {})",
                root_dir.display(),
                server.stdout_log_path().display(),
                server.stderr_log_path().display()
            )
        });
    }

    if let Err(error) = run_result {
        return Err(error).with_context(|| {
            format!(
                "verify run failed; artifacts at {} (stdout: {}, stderr: {})",
                root_dir.display(),
                server.stdout_log_path().display(),
                server.stderr_log_path().display()
            )
        });
    }

    run_result
}

async fn wait_for_verify_server_ready(paths: &ConfigPaths, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let mut poll_delay = Duration::from_millis(50);
    loop {
        match BmuxClient::connect_with_paths(paths, "bmux-cli-recording-verify-ready").await {
            Ok(_) => return Ok(()),
            Err(_) if start.elapsed() < timeout => {
                tokio::time::sleep(poll_delay).await;
                poll_delay = (poll_delay * 2).min(Duration::from_millis(250));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "verify server did not become ready: {error}"
                ));
            }
        }
    }
}

async fn drain_attach_output(
    client: &mut BmuxClient,
    session_id: Uuid,
    output: &mut Vec<u8>,
) -> Result<usize> {
    let mut total = 0_usize;
    loop {
        let chunk = client
            .attach_output(session_id, 65_536)
            .await
            .map_err(map_cli_client_error)?;
        if chunk.is_empty() {
            break;
        }
        total = total.saturating_add(chunk.len());
        output.extend_from_slice(&chunk);
    }
    Ok(total)
}

async fn collect_attach_output_until_idle(
    client: &mut BmuxClient,
    session_id: Uuid,
    output: &mut Vec<u8>,
    max_wait: Duration,
) -> Result<usize> {
    let started = Instant::now();
    let mut collected = 0_usize;
    let mut idle_polls = 0_u8;
    while started.elapsed() < max_wait {
        let read = drain_attach_output(client, session_id, output).await?;
        collected = collected.saturating_add(read);
        if read == 0 {
            idle_polls = idle_polls.saturating_add(1);
            if idle_polls >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        } else {
            idle_polls = 0;
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    Ok(collected)
}

#[derive(Debug)]
enum VerifyServerHandle {
    Foreground {
        child: std::process::Child,
        paths: ConfigPaths,
        stdout_log: PathBuf,
        stderr_log: PathBuf,
    },
    Daemon {
        paths: ConfigPaths,
        stdout_log: PathBuf,
        stderr_log: PathBuf,
    },
}

impl VerifyServerHandle {
    async fn shutdown(&mut self) -> Result<()> {
        stop_verify_server(self.paths()).await?;
        match self {
            Self::Foreground { child, .. } => {
                if wait_for_child_exit(child, Duration::from_secs(2)).await? {
                    return Ok(());
                }
                if try_kill_pid(child.id())? {
                    let _ = wait_for_child_exit(child, Duration::from_secs(2)).await;
                }
                Ok(())
            }
            Self::Daemon { paths, .. } => {
                if wait_until_verify_server_stopped(paths, Duration::from_secs(2)).await? {
                    return Ok(());
                }
                if let Some(pid) = read_server_pid_file_at(paths)? {
                    let _ = try_kill_pid(pid);
                }
                Ok(())
            }
        }
    }

    const fn paths(&self) -> &ConfigPaths {
        match self {
            Self::Foreground { paths, .. } | Self::Daemon { paths, .. } => paths,
        }
    }

    fn stdout_log_path(&self) -> &Path {
        match self {
            Self::Foreground { stdout_log, .. } | Self::Daemon { stdout_log, .. } => {
                stdout_log.as_path()
            }
        }
    }

    fn stderr_log_path(&self) -> &Path {
        match self {
            Self::Foreground { stderr_log, .. } | Self::Daemon { stderr_log, .. } => {
                stderr_log.as_path()
            }
        }
    }
}

async fn start_verify_server(
    target_binary: &Path,
    paths: &ConfigPaths,
    root_dir: &Path,
    timeout: Duration,
) -> Result<VerifyServerHandle> {
    match start_verify_server_foreground(target_binary, paths, root_dir, timeout).await {
        Ok(handle) => Ok(handle),
        Err(foreground_error) => {
            warn!(
                "recording verify foreground server startup failed, falling back to daemon: {foreground_error}"
            );
            start_verify_server_daemon(target_binary, paths, root_dir, timeout)
                .await
                .with_context(|| {
                    format!(
                        "verify startup failed in foreground and daemon fallback; foreground error: {foreground_error:#}"
                    )
                })
        }
    }
}

async fn start_verify_server_foreground(
    target_binary: &Path,
    paths: &ConfigPaths,
    root_dir: &Path,
    timeout: Duration,
) -> Result<VerifyServerHandle> {
    let logs_dir = root_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("failed creating verify logs dir {}", logs_dir.display()))?;
    let stdout_log = logs_dir.join("verify-server-foreground.stdout.log");
    let stderr_log = logs_dir.join("verify-server-foreground.stderr.log");
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_log)
        .with_context(|| format!("failed opening verify stdout log {}", stdout_log.display()))?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_log)
        .with_context(|| format!("failed opening verify stderr log {}", stderr_log.display()))?;

    let child = ProcessCommand::new(target_binary)
        .arg("server")
        .arg("start")
        .env("BMUX_CONFIG_DIR", &paths.config_dir)
        .env("BMUX_RUNTIME_DIR", &paths.runtime_dir)
        .env("BMUX_DATA_DIR", &paths.data_dir)
        .env("BMUX_STATE_DIR", &paths.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| {
            format!(
                "failed spawning foreground verify target binary {}",
                target_binary.display()
            )
        })?;

    let mut handle = VerifyServerHandle::Foreground {
        child,
        paths: paths.clone(),
        stdout_log: stdout_log.clone(),
        stderr_log: stderr_log.clone(),
    };

    match wait_for_verify_server_ready_with_child(paths, timeout, handle.child_mut()).await {
        Ok(()) => Ok(handle),
        Err(error) => {
            let stderr_excerpt = read_verify_log_excerpt(&stderr_log);
            let _ = handle.shutdown().await;
            Err(error).with_context(|| {
                format!(
                    "foreground verify startup failed (stdout: {}, stderr: {}, stderr_excerpt: {})",
                    stdout_log.display(),
                    stderr_log.display(),
                    stderr_excerpt
                )
            })
        }
    }
}

async fn start_verify_server_daemon(
    target_binary: &Path,
    paths: &ConfigPaths,
    root_dir: &Path,
    timeout: Duration,
) -> Result<VerifyServerHandle> {
    let logs_dir = root_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("failed creating verify logs dir {}", logs_dir.display()))?;
    let stdout_log = logs_dir.join("verify-server-daemon.stdout.log");
    let stderr_log = logs_dir.join("verify-server-daemon.stderr.log");
    let output = ProcessCommand::new(target_binary)
        .arg("server")
        .arg("start")
        .arg("--daemon")
        .env("BMUX_CONFIG_DIR", &paths.config_dir)
        .env("BMUX_RUNTIME_DIR", &paths.runtime_dir)
        .env("BMUX_DATA_DIR", &paths.data_dir)
        .env("BMUX_STATE_DIR", &paths.state_dir)
        .output()
        .context("failed starting verify target daemon fallback")?;
    std::fs::write(&stdout_log, &output.stdout)
        .with_context(|| format!("failed writing verify stdout log {}", stdout_log.display()))?;
    std::fs::write(&stderr_log, &output.stderr)
        .with_context(|| format!("failed writing verify stderr log {}", stderr_log.display()))?;
    if !output.status.success() {
        let stderr_excerpt = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "verify daemon fallback start failed with status {} (stdout: {}, stderr: {}, stderr_excerpt: {})",
            output.status,
            stdout_log.display(),
            stderr_log.display(),
            stderr_excerpt
        );
    }
    wait_for_verify_server_ready(paths, timeout).await?;
    Ok(VerifyServerHandle::Daemon {
        paths: paths.clone(),
        stdout_log,
        stderr_log,
    })
}

async fn wait_for_verify_server_ready_with_child(
    paths: &ConfigPaths,
    timeout: Duration,
    child: Option<&mut std::process::Child>,
) -> Result<()> {
    let start = Instant::now();
    let mut poll_delay = Duration::from_millis(50);
    let mut child = child;
    loop {
        match BmuxClient::connect_with_paths(paths, "bmux-cli-recording-verify-ready").await {
            Ok(_) => return Ok(()),
            Err(_) if start.elapsed() < timeout => {
                if let Some(child) = child.as_deref_mut()
                    && let Some(status) = child
                        .try_wait()
                        .context("failed checking verify target process status")?
                {
                    anyhow::bail!(
                        "verify target process exited before readiness (status: {status})"
                    );
                }
                tokio::time::sleep(poll_delay).await;
                poll_delay = (poll_delay * 2).min(Duration::from_millis(250));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "verify server did not become ready within {}s: {error}",
                    timeout.as_secs()
                ));
            }
        }
    }
}

async fn wait_until_verify_server_stopped(paths: &ConfigPaths, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match BmuxClient::connect_with_paths(paths, "bmux-cli-recording-verify-stop-check").await {
            Ok(_) => tokio::time::sleep(Duration::from_millis(80)).await,
            Err(_) => return Ok(true),
        }
    }
    Ok(false)
}

async fn wait_for_child_exit(child: &mut std::process::Child, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child
            .try_wait()
            .context("failed checking verify child process state")?
            .is_some()
        {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    Ok(child
        .try_wait()
        .context("failed checking verify child process state")?
        .is_some())
}

fn read_server_pid_file_at(paths: &ConfigPaths) -> Result<Option<u32>> {
    let pid_file = paths.server_pid_file();
    let content = match std::fs::read_to_string(&pid_file) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed reading pid file {}", pid_file.display()));
        }
    };
    Ok(parse_pid_content(&content))
}

fn read_verify_log_excerpt(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.lines().last().map(str::to_string))
        .filter(|line| !line.trim().is_empty())
        .unwrap_or_else(|| "<empty>".to_string())
}

impl VerifyServerHandle {
    const fn child_mut(&mut self) -> Option<&mut std::process::Child> {
        match self {
            Self::Foreground { child, .. } => Some(child),
            Self::Daemon { .. } => None,
        }
    }
}

async fn stop_verify_server(paths: &ConfigPaths) -> Result<()> {
    if let Ok(mut client) =
        BmuxClient::connect_with_paths(paths, "bmux-cli-recording-verify-stop").await
    {
        let _ = client.stop_server().await.map_err(map_cli_client_error);
    }
    Ok(())
}

fn verify_temp_paths() -> (ConfigPaths, PathBuf) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let root = std::env::temp_dir().join(format!("brv-{nanos:x}"));
    let paths = ConfigPaths::new(
        root.join("c"),
        root.join("r"),
        root.join("d"),
        root.join("s"),
    );
    (paths, root)
}

fn write_verify_config(paths: &ConfigPaths) -> Result<()> {
    let config_path = paths.config_file();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating verify config dir {}", parent.display()))?;
    }
    let config = BmuxConfig::default();
    let registry = scan_available_plugins(&config, paths)?;
    let bundled_roots = bundled_plugin_roots()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut disabled_plugins = registry
        .iter()
        .filter(|&plugin| {
            bundled_roots.contains(&plugin.search_root) && registered_plugin_entry_exists(plugin)
        })
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    disabled_plugins.sort();

    let disabled = if disabled_plugins.is_empty() {
        String::new()
    } else {
        disabled_plugins
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let payload = format!("[plugins]\ndisabled = [{disabled}]\n");
    std::fs::write(&config_path, payload)
        .with_context(|| format!("failed writing verify config {}", config_path.display()))
}

fn parse_ignore_rules(ignore: Option<&str>) -> Vec<String> {
    recording::parse_ignore_rules(ignore)
}

fn apply_ignore_rules(
    events: &[RecordingEventEnvelope],
    ignore_rules: &[String],
) -> Vec<RecordingEventEnvelope> {
    recording::apply_ignore_rules(events, ignore_rules)
}

fn recording_event_kind_name(kind: RecordingEventKind) -> String {
    recording::recording_event_kind_name(kind)
}

fn load_recording_events(recording_id: &str) -> Result<Vec<RecordingEventEnvelope>> {
    recording::load_recording_events(recording_id)
}

async fn run_session_attach(
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<u8> {
    let client = connect(ConnectionPolicyScope::Normal, "bmux-cli-attach").await?;
    run_session_attach_with_client(client, target, follow, global, None).await
}

fn map_attach_client_error(error: ClientError) -> anyhow::Error {
    match error {
        ClientError::ServerError { code, message } => match code {
            bmux_ipc::ErrorCode::AlreadyExists => {
                anyhow::anyhow!("attach failed: session already has an active attached client")
            }
            bmux_ipc::ErrorCode::NotFound => anyhow::anyhow!("attach failed: {message}"),
            _ => anyhow::anyhow!("attach failed: {message}"),
        },
        other => map_client_connect_error(other),
    }
}

fn map_cli_client_error(error: ClientError) -> anyhow::Error {
    map_client_connect_error(error)
}

fn init_logging(verbose: bool, cli_level: Option<LogLevel>) {
    let level = resolve_log_level(
        verbose,
        cli_level,
        std::env::var("BMUX_LOG_LEVEL").ok().as_deref(),
    );
    let tracing_level = tracing_level(level);
    let _ = EFFECTIVE_LOG_LEVEL.set(tracing_level);

    {
        let paths =
            moosicbox_log_runtime::resolve_paths(&moosicbox_log_runtime::LogRuntimePathsConfig {
                app_name: "bmux",
                state_dir_env: "BMUX_STATE_DIR",
                log_dir_env: "BMUX_LOG_DIR",
            });
        let runtime_level = match level {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        };
        let mut log_config = moosicbox_log_runtime::init::InitConfig::new(&paths);
        log_config.default_env_filter = Some(format!("bmux={runtime_level}"));
        log_config.sinks.file = Some(moosicbox_log_runtime::init::FileSinkConfig {
            mode: moosicbox_log_runtime::init::FileMode::Exact("bmux.log"),
        });
        match moosicbox_log_runtime::init::init(log_config) {
            Ok(handle) => {
                let _ = LOG_WRITER_GUARD.set(handle);
            }
            Err(error) => {
                eprintln!("bmux warning: failed to initialize file logging: {error}");
            }
        }
    }
}

// ── Playbook commands ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::recording::{
        confirm_delete_all_recordings, delete_all_recordings_from_dir, delete_recording_dir_at,
        list_recordings_from_dir, offline_recording_status, resolve_recording_id_prefix,
    };
    use super::{
        ProtocolDirection, ProtocolTraceEvent, TerminalProfile, TraceFamily,
        apply_attach_view_change_components, attach_keymap_from_config, filter_trace_events,
        map_attach_client_error, map_cli_client_error, merged_runtime_keybindings,
        parse_pid_content, profile_for_term, protocol_profile_for_terminal_profile,
        resolve_pane_term_with_checker,
    };
    use crate::input::InputProcessor;
    use crate::runtime::attach::state::AttachViewState;
    use bmux_cli_schema::{Cli, Command};
    use bmux_client::{AttachLayoutState, AttachOpenInfo, ClientError};
    use bmux_config::{BmuxConfig, ConfigPaths, ResolvedTimeout};
    use bmux_ipc::transport::IpcTransportError;
    use bmux_ipc::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
        AttachViewComponent, ErrorCode, PaneLayoutNode, PaneSummary, RecordingSummary,
        SessionSummary,
    };
    use bmux_plugin::{PluginManifest, PluginRegistry};
    use bmux_plugin_sdk::PluginCommandEffect;
    use crossterm::event::{
        Event as CrosstermEvent, KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
        KeyEventKind as CrosstermKeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic for test")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bmux-cli-plugin-test-{nanos}"));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn empty_cli() -> Cli {
        Cli {
            record: false,
            no_capture_input: false,
            recording_id_file: None,
            record_profile: None,
            record_event_kind: Vec::new(),
            stop_server_on_exit: false,
            command: None,
            verbose: false,
            log_level: None,
        }
    }

    #[test]
    fn validate_record_bootstrap_flags_accepts_plain_defaults() {
        let cli = empty_cli();
        assert!(super::validate_record_bootstrap_flags(&cli).is_ok());
    }

    #[test]
    fn validate_record_bootstrap_flags_rejects_orphaned_record_flags() {
        let mut cli = empty_cli();
        cli.no_capture_input = true;
        let error =
            super::validate_record_bootstrap_flags(&cli).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("--no-capture-input requires --record"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn validate_record_bootstrap_flags_rejects_record_with_subcommand() {
        let mut cli = empty_cli();
        cli.record = true;
        cli.command = Some(Command::ListSessions { json: false });
        let error =
            super::validate_record_bootstrap_flags(&cli).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("--record is only supported for top-level interactive start"),
            "unexpected error: {error}"
        );
    }

    fn plugin_manifest(id: &str, entry: &str) -> PluginManifest {
        PluginManifest::from_toml_str(&format!(
            "id = '{id}'\nname = 'Example'\nversion='0.1.0'\nentry='{entry}'\nrequired_capabilities=['bmux.commands']\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n"
        ))
        .expect("manifest should parse")
    }

    fn plugin_manifest_with_commands(id: &str, entry: &str, commands: &str) -> PluginManifest {
        PluginManifest::from_toml_str(&format!(
            "id = '{id}'\nname = 'Example'\nversion='0.1.0'\nentry='{entry}'\nrequired_capabilities=['bmux.commands']\n{commands}\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n"
        ))
        .expect("manifest should parse")
    }

    fn attach_view_state_with_scrollback_fixture() -> AttachViewState {
        let pane_id = Uuid::from_u128(11);
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: Uuid::from_u128(12),
            can_write: true,
        });
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id: Uuid::from_u128(12),
            focused_pane_id: pane_id,
            panes: vec![PaneSummary {
                id: pane_id,
                index: 0,
                name: None,
                focused: true,
            }],
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id: Uuid::from_u128(12),
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 0,
                        y: 1,
                        w: 12,
                        h: 6,
                    },
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(pane_id),
                }],
            },
        });
        let mut buffer = super::attach::state::PaneRenderBuffer::default();
        buffer.parser.screen_mut().set_size(4, 10);
        buffer.parser.process(b"one\ntwo\nthree\nfour\nfive\nsix\n");
        view_state.pane_buffers.insert(pane_id, buffer);
        view_state
    }

    #[test]
    fn validate_enabled_plugins_accepts_registered_plugin() {
        let dir = temp_dir();
        let plugin_dir = dir.join("example");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("example.dylib"), []).expect("entry should be written");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &plugin_dir.join("plugin.toml"),
                plugin_manifest("example.plugin", "example.dylib"),
            )
            .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("example.plugin".to_string());

        assert!(super::validate_enabled_plugins(&config, &registry).is_ok());
    }

    #[test]
    fn effective_enabled_plugins_includes_bundled_plugins_by_default() {
        let Some(bundled_root) = super::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        fs::write(dir.join("windows.dylib"), []).expect("entry should be written");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("plugin.toml"),
                plugin_manifest("bmux.windows", "windows.dylib"),
            )
            .expect("bundled plugin should register");

        let config = BmuxConfig::default();
        let enabled = super::effective_enabled_plugins(&config, &registry);
        assert!(enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
    }

    #[test]
    fn effective_enabled_plugins_include_windows_and_permissions_by_default() {
        let Some(bundled_root) = super::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        fs::write(dir.join("windows.dylib"), []).expect("windows entry should be written");
        fs::write(dir.join("permissions.dylib"), []).expect("permissions entry should be written");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("windows.toml"),
                plugin_manifest("bmux.windows", "windows.dylib"),
            )
            .expect("windows plugin should register");
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("permissions.toml"),
                plugin_manifest("bmux.permissions", "permissions.dylib"),
            )
            .expect("permissions plugin should register");

        let config = BmuxConfig::default();
        let enabled = super::effective_enabled_plugins(&config, &registry);
        assert!(enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
        assert!(
            enabled
                .iter()
                .any(|plugin_id| plugin_id == "bmux.permissions")
        );
    }

    #[test]
    fn effective_enabled_plugins_honors_disabled_overrides() {
        let Some(bundled_root) = super::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        fs::write(dir.join("windows.dylib"), []).expect("entry should be written");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("plugin.toml"),
                plugin_manifest("bmux.windows", "windows.dylib"),
            )
            .expect("bundled plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.disabled.push("bmux.windows".to_string());
        let enabled = super::effective_enabled_plugins(&config, &registry);
        assert!(!enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
    }

    #[test]
    fn effective_enabled_plugins_skips_bundled_plugins_with_missing_entry() {
        let Some(bundled_root) = super::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("plugin.toml"),
                plugin_manifest("bmux.windows", "windows.dylib"),
            )
            .expect("bundled plugin should register");

        let config = BmuxConfig::default();
        let enabled = super::effective_enabled_plugins(&config, &registry);
        assert!(!enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
    }

    #[test]
    fn validate_enabled_plugins_accepts_plugin_provided_capabilities() {
        let dir = temp_dir();
        let provider_dir = dir.join("provider");
        let dependent_dir = dir.join("consumer");
        fs::create_dir_all(&provider_dir).expect("provider dir should exist");
        fs::create_dir_all(&dependent_dir).expect("dependent dir should exist");
        fs::write(provider_dir.join("provider.dylib"), []).expect("provider entry should exist");
        fs::write(dependent_dir.join("consumer.dylib"), []).expect("dependent entry should exist");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &provider_dir.join("plugin.toml"),
                PluginManifest::from_toml_str(
                    "id='provider.plugin'\nname='Provider'\nversion='0.1.0'\nentry='provider.dylib'\nrequired_capabilities=['bmux.commands']\nprovided_capabilities=['example.cap.read','example.cap.write']\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
                )
                .expect("provider manifest should parse"),
            )
            .expect("provider should register");
        registry
            .register_manifest(
                &dependent_dir.join("plugin.toml"),
                PluginManifest::from_toml_str(
                    "id='consumer.plugin'\nname='Consumer'\nversion='0.1.0'\nentry='consumer.dylib'\nrequired_capabilities=['example.cap.read']\n[[dependencies]]\nplugin_id='provider.plugin'\nversion_req='^0.1'\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
                )
                .expect("dependent manifest should parse"),
            )
            .expect("dependent should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("provider.plugin".to_string());
        config.plugins.enabled.push("consumer.plugin".to_string());

        assert!(super::validate_enabled_plugins(&config, &registry).is_ok());
    }

    #[test]
    fn validate_enabled_plugins_rejects_missing_plugin() {
        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("missing.plugin".to_string());

        let error = super::validate_enabled_plugins(&config, &PluginRegistry::new())
            .expect_err("validation should fail");
        assert!(error.to_string().contains("missing.plugin"));
    }

    #[test]
    fn validate_configured_plugins_discovers_plugins_from_default_layout() {
        let dir = temp_dir();
        let plugin_dir = dir.join("data").join("plugins").join("example");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("example.dylib"), []).expect("entry should be written");
        fs::write(
            plugin_dir.join("plugin.toml"),
            "id = 'example.plugin'\nname = 'Example'\nversion='0.1.0'\nentry='example.dylib'\nrequired_capabilities=['bmux.commands']\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
        )
        .expect("manifest should be written");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("example.plugin".to_string());
        let paths = ConfigPaths::new(
            dir.join("config"),
            dir.join("runtime"),
            dir.join("data"),
            dir.join("state"),
        );

        assert!(super::validate_configured_plugins(&config, &paths).is_ok());
    }

    #[test]
    fn runtime_cli_prefers_dynamic_session_plugin_aliases_over_static_cli_rejection() {
        let dir = temp_dir();
        let plugin_dir = dir.join("policy");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("policy.dylib"), []).expect("entry should be written");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &plugin_dir.join("plugin.toml"),
                plugin_manifest_with_commands(
                    "policy.plugin",
                    "policy.dylib",
                    "[[commands]]\nname='roles'\npath=['roles']\naliases=[[\"session\",\"roles\"]]\nsummary='list'\nexecution='provider_exec'\nexpose_in_cli=true\n[[commands.arguments]]\nname='session'\nkind='string'\nlong='session'\nrequired=true\n",
                ),
            )
            .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("policy.plugin".to_string());
        let argv = vec![
            OsString::from("bmux"),
            OsString::from("session"),
            OsString::from("roles"),
            OsString::from("--session"),
            OsString::from("dev"),
        ];

        let parsed = super::parse_runtime_cli_with_registry(&argv, &config, &registry)
            .expect("runtime CLI should parse plugin alias under session namespace");
        match parsed {
            super::ParsedRuntimeCli::Plugin {
                plugin_id,
                command_name,
                arguments,
                ..
            } => {
                assert_eq!(plugin_id, "policy.plugin");
                assert_eq!(command_name, "roles");
                assert_eq!(arguments, vec!["--session".to_string(), "dev".to_string()]);
            }
            other => panic!("expected plugin runtime parse, got {other:?}"),
        }
    }

    #[test]
    fn runtime_cli_allows_plugin_owned_plugin_namespace_commands() {
        let dir = temp_dir();
        let plugin_dir = dir.join("plugin-cli");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("plugin-cli.dylib"), []).expect("entry should be written");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &plugin_dir.join("plugin.toml"),
                plugin_manifest_with_commands(
                    "bmux.plugin_cli",
                    "plugin-cli.dylib",
                    "[[commands]]\nname='list'\npath=['plugin','list']\nsummary='list'\nexecution='provider_exec'\nexpose_in_cli=true\n",
                ),
            )
            .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("bmux.plugin_cli".to_string());
        let argv = vec![
            OsString::from("bmux"),
            OsString::from("plugin"),
            OsString::from("list"),
        ];

        let parsed = super::parse_runtime_cli_with_registry(&argv, &config, &registry)
            .expect("runtime CLI should parse plugin-owned plugin namespace command");
        match parsed {
            super::ParsedRuntimeCli::Plugin {
                plugin_id,
                command_name,
                arguments,
                ..
            } => {
                assert_eq!(plugin_id, "bmux.plugin_cli");
                assert_eq!(command_name, "list");
                assert!(arguments.is_empty());
            }
            other => panic!("expected plugin runtime parse, got {other:?}"),
        }
    }

    #[test]
    fn runtime_cli_parses_bundled_plugin_command_without_explicit_enable() {
        let Some(bundled_root) = super::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        fs::write(dir.join("windows.dylib"), []).expect("entry should be written");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("plugin.toml"),
                plugin_manifest_with_commands(
                    "bmux.windows",
                    "windows.dylib",
                    "[[commands]]\nname='new-window'\npath=['new-window']\nsummary='new'\nexecution='provider_exec'\nexpose_in_cli=true\n",
                ),
            )
            .expect("plugin should register");

        let config = BmuxConfig::default();
        let argv = vec![OsString::from("bmux"), OsString::from("new-window")];
        let parsed = super::parse_runtime_cli_with_registry(&argv, &config, &registry)
            .expect("runtime CLI should parse bundled plugin command");
        match parsed {
            super::ParsedRuntimeCli::Plugin { plugin_id, .. } => {
                assert_eq!(plugin_id, "bmux.windows");
            }
            other => panic!("expected plugin runtime parse, got {other:?}"),
        }
    }

    #[test]
    fn runtime_cli_attach_remains_builtin_without_windows_plugin() {
        let config = BmuxConfig::default();
        let registry = PluginRegistry::new();
        let argv = vec![
            OsString::from("bmux"),
            OsString::from("attach"),
            OsString::from("dev"),
        ];

        let parsed = super::parse_runtime_cli_with_registry(&argv, &config, &registry)
            .expect("runtime CLI should parse built-in attach command");

        match parsed {
            super::ParsedRuntimeCli::BuiltIn { cli, .. } => {
                assert!(matches!(
                    cli.command,
                    Some(Command::Attach {
                        target: Some(ref target),
                        follow: None,
                        global: false,
                    }) if target == "dev"
                ));
            }
            other => panic!("expected built-in CLI parse, got {other:?}"),
        }
    }

    #[test]
    fn plugin_lifecycle_context_uses_plugin_specific_settings() {
        let mut config = BmuxConfig::default();
        config
            .plugins
            .settings
            .insert("example.plugin".to_string(), "configured".into());

        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let declaration = bmux_plugin::PluginDeclaration {
            id: bmux_plugin::PluginId::new("example.plugin").expect("id should parse"),
            display_name: "Example".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: bmux_plugin_sdk::VersionRange::at_least(bmux_plugin_sdk::ApiVersion::new(
                1, 0,
            )),
            native_abi: bmux_plugin_sdk::VersionRange::at_least(bmux_plugin_sdk::ApiVersion::new(
                1, 0,
            )),
            entrypoint: bmux_plugin::PluginEntrypoint::Native {
                symbol: bmux_plugin_sdk::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            },
            description: None,
            homepage: None,
            provider_priority: 0,
            required_capabilities: std::collections::BTreeSet::from([
                bmux_plugin_sdk::HostScope::new("bmux.commands").expect("capability should parse"),
            ]),
            provided_capabilities: std::collections::BTreeSet::from([
                bmux_plugin_sdk::HostScope::new("example.provider.write")
                    .expect("capability should parse"),
            ]),
            provided_features: std::collections::BTreeSet::new(),
            services: vec![bmux_plugin_sdk::PluginService {
                capability: bmux_plugin_sdk::HostScope::new("example.provider.write")
                    .expect("capability should parse"),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "provider-command/v1".to_string(),
            }],
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle::default(),
        };
        let context = super::plugin_lifecycle_context(
            &config,
            &paths,
            &declaration,
            super::service_descriptors_from_declarations([&declaration]),
            vec![
                "bmux.commands".to_string(),
                "example.provider.write".to_string(),
            ],
            vec!["example.plugin".to_string()],
            vec!["/plugins".to_string()],
            Vec::new(),
        );
        assert_eq!(context.plugin_id, "example.plugin");
        assert_eq!(context.connection.data_dir, "/data");
        assert_eq!(
            context.required_capabilities,
            vec!["bmux.commands".to_string()]
        );
        assert_eq!(
            context.provided_capabilities,
            vec!["example.provider.write".to_string()]
        );
        assert_eq!(context.services.len(), 13);
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "config-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "storage-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "storage-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "logging-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "client-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "context-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "context-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "session-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "session-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "pane-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "pane-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "recording-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "provider-command/v1")
        );
        assert_eq!(
            context.settings.as_ref().and_then(|value| value.as_str()),
            Some("configured")
        );
    }

    #[test]
    fn plugin_command_context_includes_capability_sets() {
        let config = BmuxConfig::default();
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let declaration = bmux_plugin::PluginDeclaration {
            id: bmux_plugin::PluginId::new("provider.plugin").expect("id should parse"),
            display_name: "Provider".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: bmux_plugin_sdk::VersionRange::at_least(bmux_plugin_sdk::ApiVersion::new(
                1, 0,
            )),
            native_abi: bmux_plugin_sdk::VersionRange::at_least(bmux_plugin_sdk::ApiVersion::new(
                1, 0,
            )),
            entrypoint: bmux_plugin::PluginEntrypoint::Native {
                symbol: bmux_plugin_sdk::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            },
            description: None,
            homepage: None,
            provider_priority: 0,
            required_capabilities: std::collections::BTreeSet::from([
                bmux_plugin_sdk::HostScope::new("bmux.commands").expect("capability should parse"),
                bmux_plugin_sdk::HostScope::new("example.base.read")
                    .expect("capability should parse"),
            ]),
            provided_capabilities: std::collections::BTreeSet::from([
                bmux_plugin_sdk::HostScope::new("example.provider.read")
                    .expect("capability should parse"),
                bmux_plugin_sdk::HostScope::new("example.provider.write")
                    .expect("capability should parse"),
            ]),
            provided_features: std::collections::BTreeSet::new(),
            services: vec![
                bmux_plugin_sdk::PluginService {
                    capability: bmux_plugin_sdk::HostScope::new("example.provider.read")
                        .expect("capability should parse"),
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface_id: "provider-query/v1".to_string(),
                },
                bmux_plugin_sdk::PluginService {
                    capability: bmux_plugin_sdk::HostScope::new("example.provider.write")
                        .expect("capability should parse"),
                    kind: bmux_plugin_sdk::ServiceKind::Command,
                    interface_id: "provider-command/v1".to_string(),
                },
            ],
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle::default(),
        };

        let context = super::plugin_command_context(
            &config,
            &paths,
            &declaration,
            "run-action",
            &["--name".to_string(), "editor".to_string()],
            super::service_descriptors_from_declarations([&declaration]),
            vec![
                "bmux.commands".to_string(),
                "example.base.read".to_string(),
                "example.provider.read".to_string(),
                "example.provider.write".to_string(),
            ],
            vec!["provider.plugin".to_string()],
            vec!["/plugins".to_string()],
            Vec::new(),
        );

        assert_eq!(context.plugin_id, "provider.plugin");
        assert_eq!(context.command, "run-action");
        assert_eq!(
            context.required_capabilities,
            vec!["bmux.commands".to_string(), "example.base.read".to_string()]
        );
        assert_eq!(
            context.provided_capabilities,
            vec![
                "example.provider.read".to_string(),
                "example.provider.write".to_string()
            ]
        );
        assert_eq!(context.services.len(), 14);
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "config-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "storage-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "storage-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "logging-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "client-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "context-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "context-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "session-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "session-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "pane-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "pane-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "recording-command/v1")
        );
    }

    #[test]
    fn plugin_system_event_uses_system_kind_and_name() {
        let event = super::plugin_system_event("server_started");
        assert_eq!(event.kind, bmux_plugin_sdk::PluginEventKind::System);
        assert_eq!(event.name, "server_started");
        assert_eq!(
            event
                .payload
                .get("product")
                .and_then(serde_json::Value::as_str),
            Some("bmux")
        );
    }

    #[test]
    fn plugin_event_from_server_event_maps_kind_and_payload() {
        let session_id = Uuid::from_u128(1);
        let event =
            super::plugin_event_from_server_event(&bmux_client::ServerEvent::SessionCreated {
                id: session_id,
                name: Some("editor".to_string()),
            })
            .expect("plugin event should build");
        let session_id_text = session_id.to_string();
        assert_eq!(event.kind, bmux_plugin_sdk::PluginEventKind::Session);
        assert_eq!(event.name, "session_created");
        assert!(event.payload.to_string().contains(&session_id_text));
    }

    #[test]
    fn built_in_handler_mapping_stays_in_sync_for_core_native_commands() {
        let command = Command::KillSession {
            target: "dev".to_string(),
            force_local: false,
        };
        assert_eq!(
            super::built_in_handler_for_command(&command),
            super::BuiltInHandlerId::KillSession
        );
    }

    #[test]
    fn pane_term_profile_mapping_is_stable() {
        assert_eq!(
            profile_for_term("bmux-256color"),
            TerminalProfile::Bmux256Color
        );
        assert_eq!(
            profile_for_term("screen-256color"),
            TerminalProfile::Screen256Color
        );
        assert_eq!(
            profile_for_term("tmux-256color"),
            TerminalProfile::Screen256Color
        );
        assert_eq!(
            profile_for_term("xterm-256color"),
            TerminalProfile::Xterm256Color
        );
        assert_eq!(
            profile_for_term("weird-term"),
            TerminalProfile::Conservative
        );
    }

    #[test]
    fn pane_term_falls_back_to_xterm_then_screen() {
        let resolved = resolve_pane_term_with_checker("bmux-256color", |term| match term {
            "bmux-256color" => Some(false),
            "xterm-256color" => Some(true),
            "screen-256color" => Some(true),
            _ => Some(false),
        });

        assert_eq!(resolved.pane_term, "xterm-256color");
        assert_eq!(resolved.profile, TerminalProfile::Xterm256Color);
    }

    #[test]
    fn pane_term_uses_screen_when_xterm_unavailable() {
        let resolved = resolve_pane_term_with_checker("bmux-256color", |term| match term {
            "bmux-256color" => Some(false),
            "xterm-256color" => Some(false),
            "screen-256color" => Some(true),
            _ => Some(false),
        });

        assert_eq!(resolved.pane_term, "screen-256color");
        assert_eq!(resolved.profile, TerminalProfile::Screen256Color);
    }

    #[test]
    fn pane_term_keeps_configured_when_no_fallback_available() {
        let resolved = resolve_pane_term_with_checker("bmux-256color", |_term| Some(false));

        assert_eq!(resolved.pane_term, "bmux-256color");
        assert!(
            resolved
                .warnings
                .iter()
                .any(|w| w.contains("no fallback available"))
        );
    }

    #[test]
    fn protocol_profile_mapping_is_stable() {
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Bmux256Color),
            super::ProtocolProfile::Bmux
        );
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Xterm256Color),
            super::ProtocolProfile::Xterm
        );
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Screen256Color),
            super::ProtocolProfile::Screen
        );
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Conservative),
            super::ProtocolProfile::Conservative
        );
    }

    #[test]
    fn runtime_keybindings_deep_merge_defaults_and_overrides() {
        let mut config = BmuxConfig::default();
        config.keybindings.runtime.clear();
        config
            .keybindings
            .runtime
            .insert("o".to_string(), "quit".to_string());

        let (runtime, _global, _scroll) = merged_runtime_keybindings(&config);

        assert_eq!(runtime.get("o"), Some(&"quit".to_string()));
        assert_eq!(
            runtime.get("%"),
            Some(&"split_focused_vertical".to_string())
        );
        assert_eq!(runtime.get("["), Some(&"enter_scroll_mode".to_string()));
    }

    #[test]
    fn describe_timeout_formats_resolved_timeout_states() {
        assert_eq!(
            super::describe_timeout(&ResolvedTimeout::Indefinite),
            "indefinite"
        );
        assert_eq!(
            super::describe_timeout(&ResolvedTimeout::Exact(275)),
            "exact (275ms)"
        );
        assert_eq!(
            super::describe_timeout(&ResolvedTimeout::Profile {
                name: "traditional".to_string(),
                ms: 450,
            }),
            "profile:traditional (450ms)"
        );
    }

    #[test]
    fn attach_view_change_components_mark_expected_dirty_flags() {
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.dirty.status_needs_redraw = false;
        view_state.dirty.layout_needs_refresh = false;
        view_state.dirty.full_pane_redraw = false;

        apply_attach_view_change_components(&[AttachViewComponent::Status], &mut view_state);
        assert!(view_state.dirty.status_needs_redraw);
        assert!(!view_state.dirty.layout_needs_refresh);
        assert!(!view_state.dirty.full_pane_redraw);

        view_state.dirty.status_needs_redraw = false;
        apply_attach_view_change_components(&[AttachViewComponent::Layout], &mut view_state);
        assert!(view_state.dirty.status_needs_redraw);
        assert!(view_state.dirty.layout_needs_refresh);
        assert!(view_state.dirty.full_pane_redraw);

        view_state.dirty.status_needs_redraw = false;
        view_state.dirty.layout_needs_refresh = false;
        apply_attach_view_change_components(
            &[AttachViewComponent::Scene, AttachViewComponent::Layout],
            &mut view_state,
        );
        assert!(view_state.dirty.status_needs_redraw);
        assert!(view_state.dirty.layout_needs_refresh);
        assert!(view_state.dirty.full_pane_redraw);
    }

    #[test]
    fn trace_filtering_applies_family_and_pane_constraints() {
        let events = vec![
            ProtocolTraceEvent {
                timestamp_ms: 1,
                pane_id: Some(1),
                profile: "xterm".to_string(),
                family: "csi".to_string(),
                name: "csi_primary_da".to_string(),
                direction: ProtocolDirection::Query,
                raw_hex: "1b5b63".to_string(),
                decoded: "\u{1b}[c".to_string(),
            },
            ProtocolTraceEvent {
                timestamp_ms: 2,
                pane_id: Some(2),
                profile: "xterm".to_string(),
                family: "osc".to_string(),
                name: "osc_color_query".to_string(),
                direction: ProtocolDirection::Reply,
                raw_hex: "1b5d31303b3f".to_string(),
                decoded: "...".to_string(),
            },
            ProtocolTraceEvent {
                timestamp_ms: 3,
                pane_id: Some(2),
                profile: "xterm".to_string(),
                family: "csi".to_string(),
                name: "csi_primary_da".to_string(),
                direction: ProtocolDirection::Reply,
                raw_hex: "1b5b3f313b3263".to_string(),
                decoded: "...".to_string(),
            },
        ];

        let by_family = filter_trace_events(&events, Some(TraceFamily::Csi), None, 50);
        assert_eq!(by_family.len(), 2);

        let by_pane = filter_trace_events(&events, None, Some(2), 50);
        assert_eq!(by_pane.len(), 2);

        let both = filter_trace_events(&events, Some(TraceFamily::Csi), Some(2), 50);
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].timestamp_ms, 3);
    }

    #[test]
    fn parse_pid_content_accepts_positive_pid() {
        assert_eq!(parse_pid_content("123\n"), Some(123));
    }

    #[test]
    fn parse_pid_content_rejects_invalid_values() {
        assert_eq!(parse_pid_content(""), None);
        assert_eq!(parse_pid_content("0"), None);
        assert_eq!(parse_pid_content("abc"), None);
    }

    #[test]
    fn list_recordings_from_dir_returns_empty_when_missing() {
        let missing_dir = temp_dir().join("does-not-exist");
        let recordings = list_recordings_from_dir(&missing_dir).expect("listing should succeed");
        assert!(recordings.is_empty());
    }

    #[test]
    fn list_recordings_from_dir_reads_and_sorts_manifests() {
        let root = temp_dir();
        let newer_id = Uuid::new_v4();
        let older_id = Uuid::new_v4();
        let newer_dir = root.join(newer_id.to_string());
        let older_dir = root.join(older_id.to_string());
        fs::create_dir_all(&newer_dir).expect("newer recording dir should exist");
        fs::create_dir_all(&older_dir).expect("older recording dir should exist");

        let newer_manifest = serde_json::json!({
            "summary": {
                "id": newer_id,
                "session_id": serde_json::Value::Null,
                "capture_input": true,
                "started_epoch_ms": 200,
                "ended_epoch_ms": serde_json::Value::Null,
                "event_count": 12,
                "payload_bytes": 1024,
                "path": newer_dir.to_string_lossy().to_string()
            }
        });
        let older_manifest = serde_json::json!({
            "summary": {
                "id": older_id,
                "session_id": serde_json::Value::Null,
                "capture_input": false,
                "started_epoch_ms": 100,
                "ended_epoch_ms": 150,
                "event_count": 4,
                "payload_bytes": 128,
                "path": older_dir.to_string_lossy().to_string()
            }
        });

        fs::write(
            newer_dir.join("manifest.json"),
            serde_json::to_vec(&newer_manifest).expect("newer manifest should encode"),
        )
        .expect("newer manifest should write");
        fs::write(
            older_dir.join("manifest.json"),
            serde_json::to_vec(&older_manifest).expect("older manifest should encode"),
        )
        .expect("older manifest should write");

        let recordings = list_recordings_from_dir(&root).expect("listing should succeed");
        assert_eq!(recordings.len(), 2);
        assert_eq!(recordings[0].id, newer_id);
        assert_eq!(recordings[1].id, older_id);
    }

    #[test]
    fn offline_recording_status_reports_no_active_recording() {
        let status = offline_recording_status();
        assert!(status.active.is_none());
        assert_eq!(status.queue_len, 0);
    }

    #[test]
    fn resolve_recording_id_prefix_prefers_exact_match() {
        let exact = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("exact uuid should parse");
        let other = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001")
            .expect("other uuid should parse");
        let recordings = vec![
            RecordingSummary {
                id: other,
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 1,
                ended_epoch_ms: Some(2),
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/other".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
            RecordingSummary {
                id: exact,
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 3,
                ended_epoch_ms: Some(4),
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/exact".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
        ];

        let resolved =
            resolve_recording_id_prefix("550e8400-e29b-41d4-a716-446655440000", &recordings)
                .expect("exact id should resolve");
        assert_eq!(resolved, exact);
    }

    #[test]
    fn resolve_recording_id_prefix_rejects_ambiguous_prefix() {
        let first = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("first uuid should parse");
        let second = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001")
            .expect("second uuid should parse");
        let recordings = vec![
            RecordingSummary {
                id: first,
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 1,
                ended_epoch_ms: None,
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/first".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
            RecordingSummary {
                id: second,
                format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
                session_id: None,
                capture_input: true,
                profile: bmux_ipc::RecordingProfile::Functional,
                event_kinds: vec![bmux_ipc::RecordingEventKind::PaneOutputRaw],
                started_epoch_ms: 2,
                ended_epoch_ms: None,
                event_count: 0,
                payload_bytes: 0,
                path: "/tmp/second".to_string(),
                segments: vec!["events_0.bin".to_string()],
                total_segment_bytes: 0,
            },
        ];

        let error = resolve_recording_id_prefix("550e8400", &recordings)
            .expect_err("ambiguous prefix should fail");
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn delete_recording_helpers_remove_manifest_directories() {
        let root = temp_dir();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        fs::create_dir_all(root.join(first.to_string())).expect("first dir should exist");
        fs::create_dir_all(root.join(second.to_string())).expect("second dir should exist");
        fs::write(
            root.join(first.to_string()).join("manifest.json"),
            br#"{"summary":{"id":"00000000-0000-0000-0000-000000000000","session_id":null,"capture_input":true,"started_epoch_ms":1,"ended_epoch_ms":null,"event_count":0,"payload_bytes":0,"path":"x"}}"#,
        )
        .expect("first manifest should write");
        fs::write(
            root.join(second.to_string()).join("manifest.json"),
            br#"{"summary":{"id":"00000000-0000-0000-0000-000000000000","session_id":null,"capture_input":true,"started_epoch_ms":1,"ended_epoch_ms":null,"event_count":0,"payload_bytes":0,"path":"x"}}"#,
        )
        .expect("second manifest should write");

        delete_recording_dir_at(&root, first).expect("single delete should succeed");
        assert!(!root.join(first.to_string()).exists());

        let deleted_count =
            delete_all_recordings_from_dir(&root).expect("delete-all helper should succeed");
        assert_eq!(deleted_count, 1);
        assert!(!root.join(second.to_string()).exists());
    }

    #[test]
    fn confirm_delete_all_requires_yes_for_non_interactive_mode() {
        assert!(confirm_delete_all_recordings(true).expect("--yes should bypass prompt"));
        let error = confirm_delete_all_recordings(false).expect_err("non-interactive should fail");
        assert!(error.to_string().contains("requires --yes"));
    }

    #[test]
    fn attach_exit_events_ignore_session_scoped_client_detach() {
        let session_id = uuid::Uuid::new_v4();
        assert!(super::is_attach_terminal_server_exit_event(
            &bmux_client::ServerEvent::SessionRemoved { id: session_id },
            session_id,
        ));
        assert!(!super::is_attach_terminal_server_exit_event(
            &bmux_client::ServerEvent::ClientDetached { id: session_id },
            session_id,
        ));
    }

    #[test]
    fn map_attach_client_error_formats_busy_session() {
        let error = map_attach_client_error(ClientError::ServerError {
            code: ErrorCode::AlreadyExists,
            message: "session busy".to_string(),
        });
        assert!(
            error
                .to_string()
                .contains("session already has an active attached client")
        );
    }

    #[test]
    fn map_cli_client_error_formats_transport_not_found() {
        let error = map_cli_client_error(ClientError::Transport(IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )));
        let message = error.to_string();

        assert!(message.contains("bmux server is not running"));
        assert!(message.contains("bmux server start --daemon"));
        assert!(message.contains("XDG_RUNTIME_DIR"));
        assert!(message.contains("TMPDIR"));
    }

    #[test]
    fn map_cli_client_error_keeps_non_not_found_errors() {
        let error = map_cli_client_error(ClientError::Transport(IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        )));
        let message = error.to_string();

        assert!(message.contains("transport error"));
        assert!(!message.contains("bmux server is not running"));
    }

    #[test]
    fn destructive_op_error_formats_session_policy_guidance() {
        let message = super::format_destructive_op_error(
            "session",
            ClientError::ServerError {
                code: ErrorCode::InvalidRequest,
                message: "session policy denied for this operation".to_string(),
            },
            false,
        );

        assert!(message.contains("not permitted by current session policy"));
    }

    #[test]
    fn destructive_op_error_formats_force_local_guidance() {
        let message = super::format_destructive_op_error(
            "window",
            ClientError::ServerError {
                code: ErrorCode::InvalidRequest,
                message: "force-local is only allowed for the server control principal".to_string(),
            },
            true,
        );

        assert!(message.contains("--force-local"));
        assert!(message.contains("bmux server whoami-principal"));
    }

    #[test]
    fn attach_quit_failure_status_is_actionable_for_policy_errors() {
        let status = super::attach_quit_failure_status(&ClientError::ServerError {
            code: ErrorCode::InvalidRequest,
            message: "session policy denied for this operation".to_string(),
        });

        assert_eq!(status, "quit blocked by session policy");
    }

    #[test]
    fn format_plugin_command_run_error_adds_policy_hint_when_denied() {
        let error = anyhow::anyhow!("session policy denied for this operation");
        let message = super::format_plugin_command_run_error("bmux.windows", "kill", &error);
        assert!(message.contains("failed running plugin command 'bmux.windows:kill'"));
        assert!(message.contains("operation denied by an active policy provider"));
        assert!(message.contains("authorized principal"));
    }

    #[test]
    fn format_plugin_command_run_error_keeps_generic_failures_without_hint() {
        let error = anyhow::anyhow!("unsupported service operation");
        let message = super::format_plugin_command_run_error("bmux.permissions", "grant", &error);
        assert!(message.contains("failed running plugin command 'bmux.permissions:grant'"));
        assert!(!message.contains("operation denied by session policy"));
    }

    #[test]
    fn unknown_external_command_message_points_to_plugin_list_help() {
        let message =
            super::unknown_external_command_message(&["session".to_string(), "roles".to_string()]);
        assert!(message.contains("unknown command 'session roles'"));
        assert!(message.contains("bmux plugin list"));
    }

    #[test]
    fn format_plugin_not_found_message_lists_available_plugins() {
        let message = super::format_plugin_not_found_message(
            "missing.plugin",
            &["bmux.windows".to_string(), "bmux.permissions".to_string()],
        );
        assert!(message.contains("plugin 'missing.plugin' was not found"));
        assert!(message.contains("bmux.windows, bmux.permissions"));
    }

    #[test]
    fn format_plugin_not_found_message_handles_empty_registry() {
        let empty: [&str; 0] = [];
        let message = super::format_plugin_not_found_message("missing.plugin", &empty);
        assert_eq!(message, "plugin 'missing.plugin' was not found");
    }

    #[test]
    fn format_plugin_not_enabled_message_points_to_plugins_enabled() {
        let message = super::format_plugin_not_enabled_message("bmux.windows");
        assert!(message.contains("plugin 'bmux.windows' is not enabled"));
        assert!(message.contains("plugins.disabled"));
        assert!(message.contains("plugins.enabled"));
    }

    #[test]
    fn format_plugin_argument_validation_error_adds_help_hint_for_missing_required() {
        let error = anyhow::anyhow!("missing required option '--session'");
        let message = super::format_plugin_argument_validation_error(
            &["session".to_string(), "roles".to_string()],
            &error,
        );
        assert!(message.contains("failed validating plugin command arguments for 'session roles'"));
        assert!(message.contains("missing required option '--session'"));
        assert!(message.contains("--help"));
    }

    #[test]
    fn format_plugin_argument_validation_error_keeps_non_required_errors_without_hint() {
        let error = anyhow::anyhow!("unknown option '--wat'");
        let message = super::format_plugin_argument_validation_error(
            &["session".to_string(), "roles".to_string()],
            &error,
        );
        assert!(message.contains("failed validating plugin command arguments for 'session roles'"));
        assert!(message.contains("unknown option '--wat'"));
        assert!(!message.contains("--help"));
    }

    #[test]
    fn server_event_name_maps_known_variants() {
        assert_eq!(
            super::server_event_name(&bmux_client::ServerEvent::ServerStarted),
            "server_started"
        );
        assert_eq!(
            super::server_event_name(&bmux_client::ServerEvent::ClientDetached {
                id: uuid::Uuid::new_v4()
            }),
            "client_detached"
        );
    }

    #[test]
    fn attach_key_event_action_detaches_on_prefix_d() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('d'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], super::AttachEventAction::Detach));
    }

    #[test]
    fn attach_key_event_action_ctrl_d_forwards_to_pane() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('d'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(actions[0], super::AttachEventAction::Send(ref bytes) if bytes == &[0x04])
        );
    }

    #[test]
    fn attach_key_event_action_encodes_char_input() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('x'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], super::AttachEventAction::Send(ref bytes) if bytes == b"x"));
    }

    #[test]
    fn attach_event_actions_maps_mouse_events() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let event = CrosstermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 8,
            modifiers: KeyModifiers::NONE,
        });

        let actions =
            super::attach_event_actions(&event, &mut processor, super::AttachUiMode::Normal)
                .expect("mouse event should map");

        assert!(matches!(
            actions.first(),
            Some(super::AttachEventAction::Mouse(mouse)) if mouse.column == 12 && mouse.row == 8
        ));
    }

    #[test]
    fn record_attach_mouse_event_tracks_position_and_timestamp() {
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: Uuid::new_v4(),
            can_write: true,
        });
        let event = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };

        super::record_attach_mouse_event(event, &mut view_state);

        assert_eq!(view_state.mouse.last_position, Some((3, 4)));
        assert!(view_state.mouse.last_event_at.is_some());
    }

    #[test]
    fn resolve_mouse_gesture_action_parses_plugin_command() {
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: Uuid::new_v4(),
            can_write: true,
        });
        view_state.mouse.config.gesture_actions.insert(
            "click_left".to_string(),
            "plugin:bmux.windows:new-window".to_string(),
        );

        let resolved = super::resolve_mouse_gesture_action(&view_state, "click_left");
        assert!(matches!(
            resolved,
            Some(super::AttachEventAction::PluginCommand {
                plugin_id,
                command_name
            }) if plugin_id == "bmux.windows" && command_name == "new-window"
        ));
    }

    #[test]
    fn attach_scene_pane_at_prefers_topmost_surface() {
        let session_id = Uuid::new_v4();
        let background_pane = Uuid::new_v4();
        let floating_pane = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: background_pane,
            panes: Vec::new(),
            layout_root: PaneLayoutNode::Leaf {
                pane_id: background_pane,
            },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane {
                    pane_id: background_pane,
                },
                surfaces: vec![
                    AttachSurface {
                        id: Uuid::new_v4(),
                        kind: AttachSurfaceKind::Pane,
                        layer: AttachLayer::Pane,
                        z: 1,
                        rect: AttachRect {
                            x: 0,
                            y: 0,
                            w: 20,
                            h: 10,
                        },
                        opaque: true,
                        visible: true,
                        accepts_input: true,
                        cursor_owner: true,
                        pane_id: Some(background_pane),
                    },
                    AttachSurface {
                        id: Uuid::new_v4(),
                        kind: AttachSurfaceKind::FloatingPane,
                        layer: AttachLayer::FloatingPane,
                        z: 10,
                        rect: AttachRect {
                            x: 2,
                            y: 2,
                            w: 8,
                            h: 5,
                        },
                        opaque: true,
                        visible: true,
                        accepts_input: true,
                        cursor_owner: false,
                        pane_id: Some(floating_pane),
                    },
                ],
            },
        });

        assert_eq!(
            super::attach_scene_pane_at(&view_state, 4, 4),
            Some(floating_pane)
        );
        assert_eq!(
            super::attach_scene_pane_at(&view_state, 1, 1),
            Some(background_pane)
        );
        assert_eq!(super::attach_scene_pane_at(&view_state, 30, 30), None);
    }

    #[test]
    fn attach_key_event_action_maps_prefixed_runtime_defaults() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let prefix = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(prefix.is_empty());

        let new_window = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('c'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            new_window.first(),
            Some(super::AttachEventAction::PluginCommand { plugin_id, command_name })
                if plugin_id == "bmux.windows" && command_name == "new-window"
        ));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let next_window = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('n'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            next_window.first(),
            Some(super::AttachEventAction::PluginCommand { plugin_id, command_name })
                if plugin_id == "bmux.windows" && command_name == "next-window"
        ));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let previous_window = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('p'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            previous_window.first(),
            Some(super::AttachEventAction::PluginCommand { plugin_id, command_name })
                if plugin_id == "bmux.windows" && command_name == "prev-window"
        ));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let last_window = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('w'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            last_window.first(),
            Some(super::AttachEventAction::PluginCommand { plugin_id, command_name })
                if plugin_id == "bmux.windows" && command_name == "last-window"
        ));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let split_vertical = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('%'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            split_vertical.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::SplitFocusedVertical
            ))
        ));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let quit = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('q'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            quit.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::Quit
            ))
        ));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let new_session = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('C'),
                KeyModifiers::SHIFT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            new_session.first(),
            Some(super::AttachEventAction::Runtime(
                crate::input::RuntimeAction::NewSession
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_forwards_ctrl_t_to_pane_by_default() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('t'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            actions.first(),
            Some(super::AttachEventAction::Send(bytes)) if bytes.as_slice() == [0x14]
        ));
    }

    #[test]
    fn attach_key_event_action_routes_h_to_pane_in_normal_mode() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let normal_actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('h'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            normal_actions.first(),
            Some(super::AttachEventAction::Send(bytes)) if bytes.as_slice() == b"h"
        ));

        let _ = processor;
    }

    #[test]
    fn attach_key_event_action_routes_enter_scroll_mode_to_ui() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('['),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            actions.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::EnterScrollMode
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_routes_alt_h_as_session_ui() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('h'),
                KeyModifiers::ALT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            actions.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::SessionPrev
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_routes_n_to_pane_in_normal_mode() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let normal_actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('n'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            normal_actions.first(),
            Some(super::AttachEventAction::Send(bytes)) if bytes.as_slice() == b"n"
        ));
    }

    #[test]
    fn attach_keybindings_allow_global_override_of_default_session_key() {
        let mut config = BmuxConfig::default();
        config
            .keybindings
            .global
            .insert("ctrl+t".to_string(), "new_session".to_string());

        let mut processor = InputProcessor::new(attach_keymap_from_config(&config), false);
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('t'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            actions.first(),
            Some(super::AttachEventAction::Runtime(
                crate::input::RuntimeAction::NewSession
            ))
        ));
    }

    #[test]
    fn attach_mode_hint_reflects_remapped_normal_mode_keys() {
        let mut config = BmuxConfig::default();
        config
            .keybindings
            .runtime
            .insert("d".to_string(), "quit".to_string());
        config
            .keybindings
            .runtime
            .insert("z".to_string(), "detach".to_string());

        let keymap = attach_keymap_from_config(&config);
        let hint = super::attach_mode_hint(super::AttachUiMode::Normal, &keymap);
        assert!(hint.contains("Ctrl-A z detach"));
        assert!(hint.contains("Ctrl-A d quit"));
    }

    #[test]
    fn attach_mode_hint_includes_session_navigation_overrides() {
        let mut config = BmuxConfig::default();
        config
            .keybindings
            .global
            .insert("alt+h".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("alt+l".to_string(), "detach".to_string());
        config
            .keybindings
            .global
            .insert("q".to_string(), "quit".to_string());

        let keymap = attach_keymap_from_config(&config);
        let hint = super::attach_mode_hint(super::AttachUiMode::Normal, &keymap);
        assert!(hint.contains("Ctrl-A d quit") || hint.contains("q quit"));
        assert!(hint.contains("detach"));
    }

    #[test]
    fn relative_session_id_wraps_between_sessions() {
        let session_a = Uuid::from_u128(1);
        let session_b = Uuid::from_u128(2);
        let sessions = vec![
            SessionSummary {
                id: session_a,
                name: Some("a".to_string()),
                client_count: 1,
            },
            SessionSummary {
                id: session_b,
                name: Some("b".to_string()),
                client_count: 1,
            },
        ];

        assert_eq!(
            super::relative_session_id(&sessions, session_a, -1),
            Some(session_b)
        );
        assert_eq!(
            super::relative_session_id(&sessions, session_a, 1),
            Some(session_b)
        );
        assert_eq!(
            super::relative_session_id(&sessions, session_b, 1),
            Some(session_a)
        );
    }

    #[test]
    fn plugin_fallback_retarget_context_id_returns_changed_context_when_no_effect_applied() {
        let before = Some(Uuid::from_u128(1));
        let after = Some(Uuid::from_u128(2));
        let attached = Some(Uuid::from_u128(1));

        assert_eq!(
            super::plugin_fallback_retarget_context_id(before, after, attached, false),
            after
        );
    }

    #[test]
    fn plugin_fallback_retarget_context_id_ignores_when_outcome_already_applied() {
        let before = Some(Uuid::from_u128(1));
        let after = Some(Uuid::from_u128(2));
        let attached = Some(Uuid::from_u128(2));

        assert_eq!(
            super::plugin_fallback_retarget_context_id(before, after, attached, true),
            None
        );
    }

    #[test]
    fn plugin_fallback_new_context_id_returns_single_new_context() {
        let before = [Uuid::from_u128(1)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let after = [Uuid::from_u128(1), Uuid::from_u128(2)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            super::plugin_fallback_new_context_id(
                Some(&before),
                Some(&after),
                Some(Uuid::from_u128(1)),
                Some(Uuid::from_u128(1)),
                false,
            ),
            Some(Uuid::from_u128(2))
        );
    }

    #[test]
    fn plugin_fallback_new_context_id_prefers_after_context_when_multiple_new() {
        let before = [Uuid::from_u128(1)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let after = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            super::plugin_fallback_new_context_id(
                Some(&before),
                Some(&after),
                Some(Uuid::from_u128(1)),
                Some(Uuid::from_u128(3)),
                false,
            ),
            Some(Uuid::from_u128(3))
        );
    }

    #[test]
    fn plugin_fallback_new_context_id_ignores_when_outcome_applied() {
        let before = [Uuid::from_u128(1)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let after = [Uuid::from_u128(1), Uuid::from_u128(2)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            super::plugin_fallback_new_context_id(
                Some(&before),
                Some(&after),
                Some(Uuid::from_u128(1)),
                Some(Uuid::from_u128(2)),
                true,
            ),
            None
        );
    }

    #[test]
    fn host_kernel_effect_capture_records_select_context_from_select_response() {
        super::begin_host_kernel_effect_capture();
        let context_id = Uuid::from_u128(42);
        super::maybe_record_host_kernel_effect(
            &bmux_ipc::Request::SelectContext {
                selector: bmux_ipc::ContextSelector::ById(context_id),
            },
            &bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextSelected {
                context: bmux_ipc::ContextSummary {
                    id: context_id,
                    name: Some("ctx".to_string()),
                    attributes: std::collections::BTreeMap::new(),
                },
            }),
        );
        let captured = super::finish_host_kernel_effect_capture();
        assert_eq!(
            captured,
            vec![PluginCommandEffect::SelectContext { context_id }]
        );
    }

    #[test]
    fn host_kernel_effect_capture_ignores_non_context_responses() {
        super::begin_host_kernel_effect_capture();
        super::maybe_record_host_kernel_effect(
            &bmux_ipc::Request::ListSessions,
            &bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::SessionList {
                sessions: Vec::new(),
            }),
        );
        let captured = super::finish_host_kernel_effect_capture();
        assert!(captured.is_empty());
    }

    #[test]
    fn adjust_attach_scrollback_offset_clamps_within_bounds() {
        assert_eq!(super::adjust_attach_scrollback_offset(0, -1, 4), 1);
        assert_eq!(super::adjust_attach_scrollback_offset(3, -10, 4), 4);
        assert_eq!(super::adjust_attach_scrollback_offset(4, 1, 4), 3);
        assert_eq!(super::adjust_attach_scrollback_offset(1, 50, 4), 0);
    }

    #[test]
    fn adjust_scrollback_cursor_component_clamps_within_bounds() {
        assert_eq!(super::adjust_scrollback_cursor_component(0, -1, 5), 0);
        assert_eq!(super::adjust_scrollback_cursor_component(2, -1, 5), 1);
        assert_eq!(super::adjust_scrollback_cursor_component(2, 10, 5), 5);
    }

    #[test]
    fn enter_attach_scrollback_initializes_cursor_from_live_position() {
        let mut view_state = attach_view_state_with_scrollback_fixture();

        assert!(super::enter_attach_scrollback(&mut view_state));
        assert!(view_state.scrollback_active);
        assert_eq!(view_state.scrollback_offset, 0);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 3, col: 2 })
        );
    }

    #[test]
    fn move_attach_scrollback_cursor_vertical_scrolls_at_viewport_edges() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));

        super::move_attach_scrollback_cursor_vertical(&mut view_state, -1);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 2, col: 2 })
        );
        assert_eq!(view_state.scrollback_offset, 0);

        super::move_attach_scrollback_cursor_vertical(&mut view_state, -3);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 0, col: 2 })
        );
        assert_eq!(view_state.scrollback_offset, 1);

        super::move_attach_scrollback_cursor_vertical(&mut view_state, 1);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 1, col: 2 })
        );
        assert_eq!(view_state.scrollback_offset, 1);
    }

    #[test]
    fn move_attach_scrollback_cursor_horizontal_updates_column() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));

        super::move_attach_scrollback_cursor_horizontal(&mut view_state, 3);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 3, col: 5 })
        );

        super::move_attach_scrollback_cursor_horizontal(&mut view_state, -10);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 3, col: 0 })
        );
    }

    #[test]
    fn begin_attach_selection_uses_absolute_cursor_position() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));
        view_state.scrollback_offset = 2;

        assert!(super::begin_attach_selection(&mut view_state));
        assert_eq!(
            view_state.selection_anchor,
            Some(super::attach::state::AttachScrollbackPosition { row: 5, col: 2 })
        );
    }

    #[test]
    fn clear_attach_selection_removes_anchor() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));
        assert!(super::begin_attach_selection(&mut view_state));

        super::clear_attach_selection(&mut view_state, false);
        assert_eq!(view_state.selection_anchor, None);
    }

    #[test]
    fn selected_attach_text_extracts_multiline_range() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));
        view_state.selection_anchor =
            Some(super::attach::state::AttachScrollbackPosition { row: 1, col: 1 });
        view_state.scrollback_cursor =
            Some(super::attach::state::AttachScrollbackCursor { row: 3, col: 2 });
        view_state.scrollback_offset = 0;

        assert_eq!(
            super::selected_attach_text(&mut view_state),
            Some("four\n     five\n".to_string())
        );
    }

    #[test]
    fn confirm_attach_scrollback_exits_when_no_selection() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));

        super::confirm_attach_scrollback(&mut view_state);
        assert!(!view_state.scrollback_active);
    }

    #[test]
    fn mouse_scroll_up_enters_scrollback_and_steps_by_configured_lines() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.mouse.config.scroll_lines_per_tick = 1;
        view_state.mouse.config.scroll_scrollback = true;

        assert!(super::handle_attach_mouse_scrollback(
            &mut view_state,
            MouseEventKind::ScrollUp,
        ));
        assert!(view_state.scrollback_active);
        assert_eq!(view_state.scrollback_offset, 1);
    }

    #[test]
    fn mouse_scroll_down_exits_scrollback_at_bottom_when_enabled() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.mouse.config.scroll_lines_per_tick = 1;
        view_state.mouse.config.scroll_scrollback = true;
        view_state.mouse.config.exit_scrollback_on_bottom = true;
        assert!(super::enter_attach_scrollback(&mut view_state));
        view_state.scrollback_offset = 1;

        assert!(super::handle_attach_mouse_scrollback(
            &mut view_state,
            MouseEventKind::ScrollDown,
        ));
        assert!(!view_state.scrollback_active);
        assert_eq!(view_state.scrollback_offset, 0);
    }

    #[test]
    fn attach_scrollback_hint_uses_default_bindings() {
        let keymap = attach_keymap_from_config(&BmuxConfig::default());
        let hint = super::attach_scrollback_hint(&keymap);

        assert!(hint.contains("select"));
        assert!(hint.contains("copy"));
        assert!(hint.contains("page"));
        assert!(hint.contains("top/bottom"));
        assert!(hint.contains("exit scroll"));
    }

    #[test]
    fn attach_keybindings_keep_focus_next_pane_binding() {
        let (runtime, _global, _scroll) =
            super::filtered_attach_keybindings(&BmuxConfig::default());
        assert_eq!(runtime.get("o"), Some(&"focus_next_pane".to_string()));
    }

    #[test]
    fn attach_key_event_action_maps_show_help_to_ui() {
        let config = BmuxConfig::default();
        let keymap = super::attach_keymap_from_config(&config);
        let mut processor = InputProcessor::new(keymap, false);

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let help_question = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('?'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let help_shift_slash = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('/'),
                KeyModifiers::SHIFT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            help_question.first().or_else(|| help_shift_slash.first()),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::ShowHelp
            ))
        ));
    }

    #[test]
    fn effective_attach_keybindings_include_scope_and_canonical_action_names() {
        let entries = super::effective_attach_keybindings(&BmuxConfig::default());
        assert!(entries.iter().any(|entry| {
            entry.scope == super::AttachKeybindingScope::Runtime
                && entry.chord == "o"
                && entry.action_name == "focus_next_pane"
                && entry.action == crate::input::RuntimeAction::FocusNext
        }));
        assert!(entries.iter().any(|entry| {
            entry.scope == super::AttachKeybindingScope::Global
                && entry.chord == "alt+h"
                && entry.action_name == "session_prev"
                && entry.action == crate::input::RuntimeAction::SessionPrev
        }));
    }

    #[test]
    fn adjust_help_overlay_scroll_clamps_to_bounds() {
        assert_eq!(super::adjust_help_overlay_scroll(0, -10, 20, 5), 0);
        assert_eq!(super::adjust_help_overlay_scroll(0, 3, 20, 5), 3);
        assert_eq!(super::adjust_help_overlay_scroll(17, 10, 20, 5), 15);
        assert_eq!(super::adjust_help_overlay_scroll(4, -2, 20, 5), 2);
        assert_eq!(super::adjust_help_overlay_scroll(0, 4, 0, 5), 0);
    }

    #[test]
    fn help_overlay_repeat_navigation_is_handled() {
        let mut view_state = super::AttachViewState::new(bmux_client::AttachOpenInfo {
            context_id: None,
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.help_overlay_open = true;
        let lines = (0..200)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>();

        let handled = super::handle_help_overlay_key_event(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Down,
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Repeat,
            ),
            &lines,
            &mut view_state,
        );
        assert!(handled);
        assert!(view_state.help_overlay_scroll > 0);
    }

    #[test]
    fn help_overlay_release_is_ignored() {
        let mut view_state = super::AttachViewState::new(bmux_client::AttachOpenInfo {
            context_id: None,
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.help_overlay_open = true;
        view_state.help_overlay_scroll = 5;
        let lines = (0..200)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>();

        let handled = super::handle_help_overlay_key_event(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Down,
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Release,
            ),
            &lines,
            &mut view_state,
        );
        assert!(!handled);
        assert_eq!(view_state.help_overlay_scroll, 5);
    }

    #[test]
    fn build_attach_help_lines_groups_entries_by_category() {
        let lines = super::build_attach_help_lines(&BmuxConfig::default());
        assert_eq!(lines.first().map(String::as_str), Some("Attach Help"));
        assert!(lines[1].contains("Normal mode sends typing to the pane"));
        assert!(lines.iter().any(|line| line == "-- Session --"));
        assert!(lines.iter().any(|line| line == "-- Pane --"));
        assert!(lines.iter().any(|line| line == "-- Mode --"));
    }

    #[test]
    fn attach_exit_message_suppresses_normal_detach_and_formats_stream_close() {
        assert_eq!(
            super::attach_exit_message(super::AttachExitReason::Detached),
            None
        );
        assert_eq!(
            super::attach_exit_message(super::AttachExitReason::Quit),
            None
        );
        assert_eq!(
            super::attach_exit_message(super::AttachExitReason::StreamClosed),
            Some("attach ended unexpectedly: server stream closed")
        );
    }

    #[test]
    fn initial_attach_status_mentions_help_and_typing() {
        let keymap = attach_keymap_from_config(&BmuxConfig::default());
        let status = super::initial_attach_status(&keymap, true);
        assert!(status.contains("help"));
        assert!(status.contains("typing goes to pane"));
    }

    #[test]
    fn resize_attach_parsers_applies_layout_size_before_snapshot_bytes() {
        let pane_id = uuid::Uuid::new_v4();
        let scene = bmux_ipc::AttachScene {
            session_id: uuid::Uuid::new_v4(),
            focus: bmux_ipc::AttachFocusTarget::Pane { pane_id },
            surfaces: vec![bmux_ipc::AttachSurface {
                id: pane_id,
                kind: bmux_ipc::AttachSurfaceKind::Pane,
                layer: bmux_ipc::AttachLayer::Pane,
                z: 0,
                rect: bmux_ipc::AttachRect {
                    x: 0,
                    y: 1,
                    w: 120,
                    h: 49,
                },
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        pane_buffers.insert(pane_id, super::attach::state::PaneRenderBuffer::default());

        super::resize_attach_parsers_for_scene_with_size(&mut pane_buffers, &scene, 120, 50);

        let buffer = pane_buffers
            .get_mut(&pane_id)
            .expect("pane buffer should exist");
        super::append_pane_output(&mut *buffer, b"\x1b[999;999H");
        let (row, col) = buffer.parser.screen().cursor_position();

        assert_eq!(row, 46, "cursor row should clamp to pane inner height");
        assert_eq!(col, 117, "cursor col should clamp to pane inner width");
    }

    #[test]
    fn parse_since_duration_accepts_supported_units() {
        assert_eq!(
            super::parse_since_duration("45s").expect("seconds should parse"),
            time::Duration::seconds(45)
        );
        assert_eq!(
            super::parse_since_duration("10m").expect("minutes should parse"),
            time::Duration::minutes(10)
        );
        assert_eq!(
            super::parse_since_duration("2h").expect("hours should parse"),
            time::Duration::hours(2)
        );
        assert_eq!(
            super::parse_since_duration("1d").expect("days should parse"),
            time::Duration::days(1)
        );
        assert_eq!(
            super::parse_since_duration("30").expect("plain values should default to seconds"),
            time::Duration::seconds(30)
        );
    }

    #[test]
    fn parse_since_duration_rejects_invalid_values() {
        assert!(super::parse_since_duration("").is_err());
        assert!(super::parse_since_duration("abc").is_err());
        assert!(super::parse_since_duration("5w").is_err());
        assert!(super::parse_since_duration("-1m").is_err());
    }

    #[test]
    fn line_matches_since_uses_rfc3339_prefix() {
        let cutoff = time::OffsetDateTime::parse(
            "2026-03-15T10:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .expect("cutoff should parse");
        assert!(super::line_matches_since(
            "2026-03-15T10:30:00Z INFO bmux started",
            Some(cutoff)
        ));
        assert!(!super::line_matches_since(
            "2026-03-15T09:30:00Z INFO bmux started",
            Some(cutoff)
        ));
        assert!(!super::line_matches_since(
            "INFO missing timestamp",
            Some(cutoff)
        ));
    }

    #[test]
    fn compile_filter_regex_supports_case_modes() {
        let sensitive = super::compile_filter_regex("error", super::LogFilterCaseMode::Sensitive)
            .expect("sensitive regex should compile");
        let insensitive =
            super::compile_filter_regex("error", super::LogFilterCaseMode::Insensitive)
                .expect("insensitive regex should compile");

        assert!(sensitive.is_match("error line"));
        assert!(!sensitive.is_match("ERROR line"));
        assert!(insensitive.is_match("ERROR line"));
    }

    #[test]
    fn line_visible_in_watch_respects_include_and_exclude_rules() {
        let filters = vec![
            super::LogFilterRule::new(
                super::LogFilterKind::Include,
                "server".to_string(),
                super::LogFilterCaseMode::Sensitive,
            ),
            super::LogFilterRule::new(
                super::LogFilterKind::Exclude,
                "listening".to_string(),
                super::LogFilterCaseMode::Sensitive,
            ),
        ];

        assert!(!super::line_visible_in_watch(
            "INFO bmux server listening",
            &filters,
            None
        ));
        assert!(super::line_visible_in_watch(
            "INFO bmux server started",
            &filters,
            None
        ));
        assert!(!super::line_visible_in_watch(
            "INFO unrelated",
            &filters,
            None
        ));
    }

    #[test]
    fn line_visible_in_watch_supports_quick_filter() {
        assert!(super::line_visible_in_watch(
            "INFO subsystem ready",
            &[],
            Some("subsystem")
        ));
        assert!(!super::line_visible_in_watch(
            "INFO subsystem ready",
            &[],
            Some("error")
        ));
    }

    #[test]
    fn normalize_logs_watch_profile_defaults_and_validates() {
        assert_eq!(
            super::normalize_logs_watch_profile(None).expect("default profile should resolve"),
            "default"
        );
        assert_eq!(
            super::normalize_logs_watch_profile(Some("incident_db"))
                .expect("valid profile should resolve"),
            "incident_db"
        );
        assert!(super::normalize_logs_watch_profile(Some("bad name")).is_err());
        assert!(super::normalize_logs_watch_profile(Some("")).is_err());
    }

    #[test]
    fn logs_watch_filter_state_roundtrip_preserves_case_and_enabled() {
        let mut rule = super::LogFilterRule::new(
            super::LogFilterKind::Exclude,
            "server listening".to_string(),
            super::LogFilterCaseMode::Insensitive,
        );
        rule.enabled = false;
        let state = super::logs_watch_filter_rule_to_state(&rule);
        let roundtrip = super::logs_watch_filter_state_to_rule(state);
        assert!(matches!(roundtrip.kind, super::LogFilterKind::Exclude));
        assert!(matches!(
            roundtrip.case_mode,
            super::LogFilterCaseMode::Insensitive
        ));
        assert!(!roundtrip.enabled);
        assert_eq!(roundtrip.pattern, "server listening");
    }
}
