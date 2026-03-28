use crate::cli::{
    Cli, Command, KeymapCommand, LogLevel, LogsCommand, LogsProfilesCommand, PlaybookCommand,
    RecordingCommand, RecordingEventKindArg, RecordingExportFormat, RecordingProfileArg,
    RecordingRenderMode, RecordingReplayMode, ServerCommand, SessionCommand, TerminalCommand,
    TraceFamily,
};
use crate::connection::{
    ConnectionPolicyScope, ServerRuntimeMetadata, connect, connect_if_running, connect_raw,
    current_cli_build_id, map_client_connect_error, read_server_runtime_metadata,
    remove_server_runtime_metadata_file, write_server_runtime_metadata,
};
use crate::input::{InputProcessor, Keymap, RuntimeAction};
use crate::status::{AttachTab, build_attach_status_line};
use anyhow::{Context, Result};
use bmux_client::{AttachLayoutState, AttachSnapshotState, BmuxClient, ClientError};
use bmux_config::{BmuxConfig, ConfigPaths, ResolvedTimeout, TerminfoAutoInstall};
use bmux_ipc::{
    AttachViewComponent, ContextSelector, ContextSummary, InvokeServiceKind, PaneFocusDirection,
    PaneSplitDirection, RecordingEventEnvelope, RecordingEventKind, RecordingPayload,
    RecordingStatus, RecordingSummary, SessionSelector, SessionSummary,
};
use bmux_keybind::action_to_config_name;
use bmux_plugin::{
    CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostConnectionInfo, HostMetadata,
    HostScope, NativeCommandContext, NativeLifecycleContext, PluginCommandEffect,
    PluginCommandOutcome, PluginEvent, PluginEventKind, PluginManifest, PluginRegistry,
    RegisteredService, ServiceKind, ServiceRequest,
    load_registered_plugin as load_native_registered_plugin,
};
use bmux_server::BmuxServer;
use clap::{CommandFactory, FromArgMatches};
use crossterm::cursor::{MoveTo, SavePosition, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
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
    let request: bmux_plugin::HostKernelBridgeRequest =
        match bmux_plugin::decode_service_message(input) {
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
        Ok(payload) => bmux_plugin::HostKernelBridgeResponse { payload },
        Err(_) => return 5,
    };

    let encoded = match bmux_plugin::encode_service_message(&response) {
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
mod logs_watch;
mod plugin_commands;
mod plugin_host;
mod recording;
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
use attach::state::{
    AttachEventAction, AttachExitReason, AttachScrollbackCursor, AttachScrollbackPosition,
    AttachUiMode, AttachViewState, PaneRect,
};
use built_in_commands::{BuiltInHandlerId, built_in_command_by_handler};
use plugin_commands::PluginCommandRegistry;
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
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.storage").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "storage-query/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.storage").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "storage-command/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.logs.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "logging-command/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.clients.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "client-query/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.contexts.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "context-query/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.contexts.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "context-command/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.sessions.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "session-query/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.sessions.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "session-command/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.panes.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "pane-query/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.panes.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "pane-command/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.recording.write").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "recording-command/v1".to_string(),
            provider: bmux_plugin::ProviderId::Host,
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
    let connection_info = bmux_plugin::HostConnectionInfo {
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
        let bmux_plugin::ProviderId::Plugin(provider_plugin_id) = service.provider.clone() else {
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
                    let response = loaded.invoke_service(&bmux_plugin::NativeServiceContext {
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
                                provider: bmux_plugin::ProviderId::Plugin(
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
                            .and_then(parse_plugin_settings_value)
                            .unwrap_or_default(),
                        plugin_settings_map: config
                            .plugins
                            .settings
                            .iter()
                            .filter_map(|(plugin_id, value)| {
                                parse_plugin_settings_value(value)
                                    .map(|settings| (plugin_id.clone(), settings))
                            })
                            .collect(),
                        host_kernel_bridge: Some(bmux_plugin::HostKernelBridge::from_fn(
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

fn parse_plugin_settings_value(
    value: &toml::Value,
) -> Option<std::collections::BTreeMap<String, String>> {
    let table = value.as_table()?;
    let mut parsed = std::collections::BTreeMap::new();
    for (key, entry) in table {
        match entry {
            toml::Value::String(v) => {
                parsed.insert(key.clone(), v.clone());
            }
            toml::Value::Integer(v) => {
                parsed.insert(key.clone(), v.to_string());
            }
            toml::Value::Float(v) => {
                parsed.insert(key.clone(), v.to_string());
            }
            toml::Value::Boolean(v) => {
                parsed.insert(key.clone(), v.to_string());
            }
            _ => {}
        }
    }
    Some(parsed)
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
                provider: bmux_plugin::ProviderId::Plugin(declaration.id.as_str().to_string()),
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

#[derive(Debug)]
enum ParsedRuntimeCli {
    BuiltIn {
        cli: Cli,
        log_level: LogLevel,
        verbose: bool,
    },
    ImmediateExit {
        code: u8,
        output: String,
        stderr: bool,
    },
    Plugin {
        log_level: LogLevel,
        plugin_id: String,
        command_name: String,
        arguments: Vec<String>,
    },
}

fn parse_runtime_cli() -> Result<ParsedRuntimeCli> {
    let argv = std::env::args_os().collect::<Vec<_>>();
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    parse_runtime_cli_with_registry(&argv, &config, &registry)
}

fn parse_runtime_cli_with_registry(
    argv: &[std::ffi::OsString],
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<ParsedRuntimeCli> {
    let mut command_config = config.clone();
    command_config.plugins.enabled = effective_enabled_plugins(config, registry);
    let command_registry = PluginCommandRegistry::build(&command_config, registry)
        .context("failed building plugin CLI command registry")?;
    let clap_command = command_registry
        .augment_clap_command(Cli::command())
        .context("failed augmenting CLI with plugin commands")?;
    let matches = match clap_command.try_get_matches_from(argv.iter().cloned()) {
        Ok(matches) => matches,
        Err(error) => {
            return Ok(match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    ParsedRuntimeCli::ImmediateExit {
                        code: 0,
                        output: error.to_string(),
                        stderr: false,
                    }
                }
                _ => ParsedRuntimeCli::ImmediateExit {
                    code: 2,
                    output: error.to_string(),
                    stderr: true,
                },
            });
        }
    };
    let verbose = matches.get_flag("verbose");
    let log_level = resolve_log_level(
        verbose,
        matches.get_one::<LogLevel>("log_level").copied(),
        std::env::var("BMUX_LOG_LEVEL").ok().as_deref(),
    );
    let (path, leaf_matches) = plugin_commands::selected_subcommand_path(&matches);
    if let Some(resolved) = command_registry.resolve_exact_path(&path) {
        let arguments =
            PluginCommandRegistry::normalize_arguments_from_matches(&resolved.schema, leaf_matches);
        return Ok(ParsedRuntimeCli::Plugin {
            log_level,
            plugin_id: resolved.plugin_id,
            command_name: resolved.command_name,
            arguments,
        });
    }

    let cli =
        Cli::from_arg_matches(&matches).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(ParsedRuntimeCli::BuiltIn {
        cli,
        log_level,
        verbose,
    })
}

fn resolve_log_level(
    verbose: bool,
    cli_level: Option<LogLevel>,
    env_level: Option<&str>,
) -> LogLevel {
    if let Some(level) = cli_level {
        return level;
    }
    if verbose {
        return LogLevel::Debug;
    }
    if let Some(raw) = env_level
        && let Some(level) = parse_log_level(raw)
    {
        return level;
    }
    LogLevel::Info
}

fn parse_log_level(raw: &str) -> Option<LogLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => Some(LogLevel::Error),
        "warn" | "warning" => Some(LogLevel::Warn),
        "info" => Some(LogLevel::Info),
        "debug" => Some(LogLevel::Debug),
        "trace" => Some(LogLevel::Trace),
        _ => None,
    }
}

const fn tracing_level(level: LogLevel) -> Level {
    match level {
        LogLevel::Error => Level::ERROR,
        LogLevel::Warn => Level::WARN,
        LogLevel::Info => Level::INFO,
        LogLevel::Debug => Level::DEBUG,
        LogLevel::Trace => Level::TRACE,
    }
}

fn validate_record_bootstrap_flags(cli: &Cli) -> Result<()> {
    if cli.command.is_some() {
        if cli.record {
            anyhow::bail!(
                "--record is only supported for top-level interactive start (no subcommand)"
            )
        }
        if cli.no_capture_input {
            anyhow::bail!("--no-capture-input requires --record")
        }
        if cli.recording_id_file.is_some() {
            anyhow::bail!("--recording-id-file requires --record")
        }
        if cli.record_profile.is_some() {
            anyhow::bail!("--record-profile requires --record")
        }
        if !cli.record_event_kind.is_empty() {
            anyhow::bail!("--record-event-kind requires --record")
        }
        if cli.stop_server_on_exit {
            anyhow::bail!("--stop-server-on-exit requires --record")
        }
    } else if !cli.record {
        if cli.no_capture_input {
            anyhow::bail!("--no-capture-input requires --record")
        }
        if cli.recording_id_file.is_some() {
            anyhow::bail!("--recording-id-file requires --record")
        }
        if cli.record_profile.is_some() {
            anyhow::bail!("--record-profile requires --record")
        }
        if !cli.record_event_kind.is_empty() {
            anyhow::bail!("--record-event-kind requires --record")
        }
        if cli.stop_server_on_exit {
            anyhow::bail!("--stop-server-on-exit requires --record")
        }
    }
    Ok(())
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

async fn run_command(command: &Command) -> Result<u8> {
    match command {
        Command::External(args) => run_external_plugin_command(args).await,
        _ => dispatch_built_in_command(command).await,
    }
}

fn built_in_handler_for_command(command: &Command) -> BuiltInHandlerId {
    match command {
        Command::NewSession { .. } => BuiltInHandlerId::NewSession,
        Command::ListSessions { .. } => BuiltInHandlerId::ListSessions,
        Command::ListClients { .. } => BuiltInHandlerId::ListClients,
        Command::KillSession { .. } => BuiltInHandlerId::KillSession,
        Command::KillAllSessions { .. } => BuiltInHandlerId::KillAllSessions,
        Command::Attach { .. } => BuiltInHandlerId::Attach,
        Command::Detach => BuiltInHandlerId::Detach,
        Command::Follow { .. } => BuiltInHandlerId::Follow,
        Command::Unfollow => BuiltInHandlerId::Unfollow,
        Command::Session { command } => match command {
            SessionCommand::New { .. } => BuiltInHandlerId::SessionNew,
            SessionCommand::List { .. } => BuiltInHandlerId::SessionList,
            SessionCommand::Clients { .. } => BuiltInHandlerId::SessionClients,
            SessionCommand::Kill { .. } => BuiltInHandlerId::SessionKill,
            SessionCommand::KillAll { .. } => BuiltInHandlerId::SessionKillAll,
            SessionCommand::Attach { .. } => BuiltInHandlerId::SessionAttach,
            SessionCommand::Detach => BuiltInHandlerId::SessionDetach,
            SessionCommand::Follow { .. } => BuiltInHandlerId::SessionFollow,
            SessionCommand::Unfollow => BuiltInHandlerId::SessionUnfollow,
        },
        Command::Server { command } => match command {
            ServerCommand::Start { .. } => BuiltInHandlerId::ServerStart,
            ServerCommand::Status { .. } => BuiltInHandlerId::ServerStatus,
            ServerCommand::WhoamiPrincipal { .. } => BuiltInHandlerId::ServerWhoamiPrincipal,
            ServerCommand::Save => BuiltInHandlerId::ServerSave,
            ServerCommand::Restore { .. } => BuiltInHandlerId::ServerRestore,
            ServerCommand::Stop => BuiltInHandlerId::ServerStop,
        },
        Command::Logs { command } => match command {
            LogsCommand::Path { .. } => BuiltInHandlerId::LogsPath,
            LogsCommand::Level { .. } => BuiltInHandlerId::LogsLevel,
            LogsCommand::Tail { .. } => BuiltInHandlerId::LogsTail,
            LogsCommand::Watch { .. } => BuiltInHandlerId::LogsWatch,
            LogsCommand::Profiles { command } => match command {
                LogsProfilesCommand::List { .. } => BuiltInHandlerId::LogsProfilesList,
                LogsProfilesCommand::Show { .. } => BuiltInHandlerId::LogsProfilesShow,
                LogsProfilesCommand::Delete { .. } => BuiltInHandlerId::LogsProfilesDelete,
                LogsProfilesCommand::Rename { .. } => BuiltInHandlerId::LogsProfilesRename,
            },
        },
        Command::Keymap { .. } => BuiltInHandlerId::KeymapDoctor,
        Command::Terminal { command } => match command {
            TerminalCommand::Doctor { .. } => BuiltInHandlerId::TerminalDoctor,
            TerminalCommand::InstallTerminfo { .. } => BuiltInHandlerId::TerminalInstallTerminfo,
        },
        Command::Recording { command } => match command {
            RecordingCommand::Start { .. } => BuiltInHandlerId::RecordingStart,
            RecordingCommand::Stop { .. } => BuiltInHandlerId::RecordingStop,
            RecordingCommand::Status { .. } => BuiltInHandlerId::RecordingStatus,
            RecordingCommand::List { .. } => BuiltInHandlerId::RecordingList,
            RecordingCommand::Delete { .. } => BuiltInHandlerId::RecordingDelete,
            RecordingCommand::DeleteAll { .. } => BuiltInHandlerId::RecordingDeleteAll,
            RecordingCommand::Inspect { .. } => BuiltInHandlerId::RecordingInspect,
            RecordingCommand::Replay { .. } => BuiltInHandlerId::RecordingReplay,
            RecordingCommand::VerifySmoke { .. } => BuiltInHandlerId::RecordingVerifySmoke,
            RecordingCommand::Export { .. } => BuiltInHandlerId::RecordingExport,
        },
        Command::Playbook { command } => match command {
            PlaybookCommand::Run { .. } => BuiltInHandlerId::PlaybookRun,
            PlaybookCommand::Validate { .. } => BuiltInHandlerId::PlaybookValidate,
            PlaybookCommand::Interactive { .. } => BuiltInHandlerId::PlaybookInteractive,
            PlaybookCommand::FromRecording { .. } => BuiltInHandlerId::PlaybookFromRecording,
        },
        Command::External(_) => unreachable!("external commands are dispatched separately"),
    }
}

async fn dispatch_built_in_command(command: &Command) -> Result<u8> {
    let handler = built_in_handler_for_command(command);
    let _descriptor = built_in_command_by_handler(handler);
    match (handler, command) {
        (BuiltInHandlerId::NewSession, Command::NewSession { name }) => {
            run_session_new(name.clone()).await
        }
        (BuiltInHandlerId::ListSessions, Command::ListSessions { json }) => {
            run_session_list(*json).await
        }
        (BuiltInHandlerId::ListClients, Command::ListClients { json }) => {
            run_client_list(*json).await
        }
        (
            BuiltInHandlerId::KillSession,
            Command::KillSession {
                target,
                force_local,
            },
        ) => run_session_kill(target, *force_local).await,
        (BuiltInHandlerId::KillAllSessions, Command::KillAllSessions { force_local }) => {
            run_session_kill_all(*force_local).await
        }
        (
            BuiltInHandlerId::Attach,
            Command::Attach {
                target,
                follow,
                global,
            },
        ) => run_session_attach(target.as_deref(), follow.as_deref(), *global).await,
        (BuiltInHandlerId::Detach, Command::Detach) => run_session_detach().await,
        (
            BuiltInHandlerId::Follow,
            Command::Follow {
                target_client_id,
                global,
            },
        ) => run_follow(target_client_id, *global).await,
        (BuiltInHandlerId::Unfollow, Command::Unfollow) => run_unfollow().await,
        (
            BuiltInHandlerId::SessionNew,
            Command::Session {
                command: SessionCommand::New { name },
            },
        ) => run_session_new(name.clone()).await,
        (
            BuiltInHandlerId::SessionList,
            Command::Session {
                command: SessionCommand::List { json },
            },
        ) => run_session_list(*json).await,
        (
            BuiltInHandlerId::SessionClients,
            Command::Session {
                command: SessionCommand::Clients { json },
            },
        ) => run_client_list(*json).await,
        (
            BuiltInHandlerId::SessionKill,
            Command::Session {
                command:
                    SessionCommand::Kill {
                        target,
                        force_local,
                    },
            },
        ) => run_session_kill(target, *force_local).await,
        (
            BuiltInHandlerId::SessionKillAll,
            Command::Session {
                command: SessionCommand::KillAll { force_local },
            },
        ) => run_session_kill_all(*force_local).await,
        (
            BuiltInHandlerId::SessionAttach,
            Command::Session {
                command:
                    SessionCommand::Attach {
                        target,
                        follow,
                        global,
                    },
            },
        ) => run_session_attach(target.as_deref(), follow.as_deref(), *global).await,
        (
            BuiltInHandlerId::SessionDetach,
            Command::Session {
                command: SessionCommand::Detach,
            },
        ) => run_session_detach().await,
        (
            BuiltInHandlerId::SessionFollow,
            Command::Session {
                command:
                    SessionCommand::Follow {
                        target_client_id,
                        global,
                    },
            },
        ) => run_follow(target_client_id, *global).await,
        (
            BuiltInHandlerId::SessionUnfollow,
            Command::Session {
                command: SessionCommand::Unfollow,
            },
        ) => run_unfollow().await,
        (
            BuiltInHandlerId::ServerStart,
            Command::Server {
                command:
                    ServerCommand::Start {
                        daemon,
                        foreground_internal,
                    },
            },
        ) => run_server_start(*daemon, *foreground_internal).await,
        (
            BuiltInHandlerId::ServerStatus,
            Command::Server {
                command: ServerCommand::Status { json },
            },
        ) => run_server_status(*json).await,
        (
            BuiltInHandlerId::ServerWhoamiPrincipal,
            Command::Server {
                command: ServerCommand::WhoamiPrincipal { json },
            },
        ) => run_server_whoami_principal(*json).await,
        (
            BuiltInHandlerId::ServerSave,
            Command::Server {
                command: ServerCommand::Save,
            },
        ) => run_server_save().await,
        (
            BuiltInHandlerId::ServerRestore,
            Command::Server {
                command: ServerCommand::Restore { dry_run, yes },
            },
        ) => run_server_restore(*dry_run, *yes).await,
        (
            BuiltInHandlerId::ServerStop,
            Command::Server {
                command: ServerCommand::Stop,
            },
        ) => run_server_stop().await,
        (
            BuiltInHandlerId::LogsPath,
            Command::Logs {
                command: LogsCommand::Path { json },
            },
        ) => run_logs_path(*json),
        (
            BuiltInHandlerId::LogsLevel,
            Command::Logs {
                command: LogsCommand::Level { json },
            },
        ) => run_logs_level(*json),
        (
            BuiltInHandlerId::LogsTail,
            Command::Logs {
                command:
                    LogsCommand::Tail {
                        lines,
                        since,
                        no_follow,
                    },
            },
        ) => run_logs_tail(*lines, since.as_deref(), !*no_follow),
        (
            BuiltInHandlerId::LogsWatch,
            Command::Logs {
                command:
                    LogsCommand::Watch {
                        lines,
                        since,
                        profile,
                        include,
                        include_i,
                        exclude,
                        exclude_i,
                    },
            },
        ) => run_logs_watch(
            *lines,
            since.as_deref(),
            profile.as_deref(),
            include,
            include_i,
            exclude,
            exclude_i,
        ),
        (
            BuiltInHandlerId::LogsProfilesList,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::List { json },
                    },
            },
        ) => run_logs_profiles_list(*json),
        (
            BuiltInHandlerId::LogsProfilesShow,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Show { profile, json },
                    },
            },
        ) => run_logs_profiles_show(profile.as_deref(), *json),
        (
            BuiltInHandlerId::LogsProfilesDelete,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Delete { profile },
                    },
            },
        ) => run_logs_profiles_delete(profile),
        (
            BuiltInHandlerId::LogsProfilesRename,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Rename { from, to },
                    },
            },
        ) => run_logs_profiles_rename(from, to),
        (
            BuiltInHandlerId::KeymapDoctor,
            Command::Keymap {
                command: KeymapCommand::Doctor { json },
            },
        ) => run_keymap_doctor(*json),
        (
            BuiltInHandlerId::TerminalDoctor,
            Command::Terminal {
                command:
                    TerminalCommand::Doctor {
                        json,
                        trace,
                        trace_limit,
                        trace_family,
                        trace_pane,
                    },
            },
        ) => run_terminal_doctor(*json, *trace, *trace_limit, *trace_family, *trace_pane),
        (
            BuiltInHandlerId::TerminalInstallTerminfo,
            Command::Terminal {
                command: TerminalCommand::InstallTerminfo { yes, check },
            },
        ) => run_terminal_install_terminfo(*yes, *check),
        (
            BuiltInHandlerId::RecordingStart,
            Command::Recording {
                command:
                    RecordingCommand::Start {
                        session_id,
                        no_capture_input,
                        profile,
                        event_kind,
                    },
            },
        ) => {
            run_recording_start(
                session_id.as_deref(),
                !*no_capture_input,
                *profile,
                event_kind,
            )
            .await
        }
        (
            BuiltInHandlerId::RecordingStop,
            Command::Recording {
                command: RecordingCommand::Stop { recording_id },
            },
        ) => run_recording_stop(recording_id.as_deref()).await,
        (
            BuiltInHandlerId::RecordingStatus,
            Command::Recording {
                command: RecordingCommand::Status { json },
            },
        ) => run_recording_status(*json).await,
        (
            BuiltInHandlerId::RecordingList,
            Command::Recording {
                command: RecordingCommand::List { json },
            },
        ) => run_recording_list(*json).await,
        (
            BuiltInHandlerId::RecordingDelete,
            Command::Recording {
                command: RecordingCommand::Delete { recording_id },
            },
        ) => run_recording_delete(recording_id).await,
        (
            BuiltInHandlerId::RecordingDeleteAll,
            Command::Recording {
                command: RecordingCommand::DeleteAll { yes },
            },
        ) => run_recording_delete_all(*yes).await,
        (
            BuiltInHandlerId::RecordingInspect,
            Command::Recording {
                command:
                    RecordingCommand::Inspect {
                        recording_id,
                        limit,
                        kind,
                        json,
                    },
            },
        ) => run_recording_inspect(recording_id, *limit, kind.as_deref(), *json),
        (
            BuiltInHandlerId::RecordingReplay,
            Command::Recording {
                command:
                    RecordingCommand::Replay {
                        recording_id,
                        mode,
                        speed,
                        target_bmux,
                        compare_recording,
                        ignore,
                        strict_timing,
                        max_verify_duration,
                        verify_start_timeout,
                    },
            },
        ) => {
            run_recording_replay(
                recording_id,
                *mode,
                *speed,
                target_bmux.as_deref(),
                compare_recording.as_deref(),
                ignore.as_deref(),
                *strict_timing,
                *max_verify_duration,
                *verify_start_timeout,
            )
            .await
        }
        (
            BuiltInHandlerId::RecordingVerifySmoke,
            Command::Recording {
                command:
                    RecordingCommand::VerifySmoke {
                        recording_id,
                        target_bmux,
                        compare_recording,
                        ignore,
                        strict_timing,
                        max_verify_duration,
                        verify_start_timeout,
                    },
            },
        ) => {
            run_recording_verify_smoke(
                recording_id,
                target_bmux.as_deref(),
                compare_recording.as_deref(),
                ignore.as_deref(),
                *strict_timing,
                *max_verify_duration,
                *verify_start_timeout,
            )
            .await
        }
        (
            BuiltInHandlerId::RecordingExport,
            Command::Recording {
                command:
                    RecordingCommand::Export {
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
                    },
            },
        ) => {
            run_recording_export(
                recording_id,
                *format,
                output,
                view_client.as_deref(),
                *speed,
                *fps,
                *max_duration,
                *max_frames,
                *renderer,
                *cell_size,
                *cell_width,
                *cell_height,
                font_family.as_deref(),
                *font_size,
                *line_height,
                font_path,
            )
            .await
        }
        (
            BuiltInHandlerId::PlaybookRun,
            Command::Playbook {
                command:
                    PlaybookCommand::Run {
                        source,
                        json,
                        target_server,
                        record,
                        export_gif,
                        viewport,
                        timeout,
                        shell,
                    },
            },
        ) => {
            run_playbook_run(
                source,
                *json,
                *target_server,
                *record,
                export_gif.as_deref(),
                viewport.as_deref(),
                *timeout,
                shell.as_deref(),
            )
            .await
        }
        (
            BuiltInHandlerId::PlaybookValidate,
            Command::Playbook {
                command: PlaybookCommand::Validate { source, json },
            },
        ) => run_playbook_validate(source, *json),
        (
            BuiltInHandlerId::PlaybookInteractive,
            Command::Playbook {
                command:
                    PlaybookCommand::Interactive {
                        socket,
                        record,
                        viewport,
                        shell,
                        timeout,
                    },
            },
        ) => {
            run_playbook_interactive(
                socket.as_deref(),
                *record,
                viewport,
                shell.as_deref(),
                *timeout,
            )
            .await
        }
        (
            BuiltInHandlerId::PlaybookFromRecording,
            Command::Playbook {
                command:
                    PlaybookCommand::FromRecording {
                        recording_id,
                        output,
                    },
            },
        ) => run_playbook_from_recording(recording_id, output.as_deref()),
        _ => unreachable!("built-in command handler and command variant should stay in sync"),
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

fn plugin_host_metadata() -> HostMetadata {
    HostMetadata {
        product_name: "bmux".to_string(),
        product_version: env!("CARGO_PKG_VERSION").to_string(),
        plugin_api_version: CURRENT_PLUGIN_API_VERSION,
        plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
    }
}

fn plugin_host_for_declaration(
    declaration: &bmux_plugin::PluginDeclaration,
    paths: &ConfigPaths,
    config: &BmuxConfig,
    available_services: Vec<RegisteredService>,
) -> plugin_host::CliPluginHost {
    plugin_host::CliPluginHost::for_plugin(
        declaration.id.as_str(),
        plugin_host_metadata(),
        paths,
        config.clone(),
        declaration.required_capabilities.clone(),
        declaration.provided_capabilities.clone(),
        available_services,
    )
}

#[cfg(test)]
fn validate_configured_plugins(config: &BmuxConfig, paths: &ConfigPaths) -> Result<()> {
    let registry = scan_available_plugins(config, paths)?;
    validate_enabled_plugins(config, &registry)
}

/// Register statically-linked bundled plugins whose feature flags are active.
///
/// Each `cfg` block is only compiled when the corresponding Cargo feature is
/// enabled, so the binary only contains the plugin code the user opted into
/// (all four by default via the `bundled-plugins` feature).
#[allow(unused_variables)]
fn register_static_bundled_plugins(registry: &mut PluginRegistry) {
    #[cfg(feature = "bundled-plugin-clipboard")]
    if let Err(e) = registry.register_bundled_manifest(include_str!(
        "../../../../plugins/bundled/clipboard/plugin.toml"
    )) {
        tracing::warn!("failed to register bundled clipboard plugin: {e}");
    }

    #[cfg(feature = "bundled-plugin-permissions")]
    if let Err(e) = registry.register_bundled_manifest(include_str!(
        "../../../../plugins/bundled/permissions/plugin.toml"
    )) {
        tracing::warn!("failed to register bundled permissions plugin: {e}");
    }

    #[cfg(feature = "bundled-plugin-cli")]
    if let Err(e) = registry.register_bundled_manifest(include_str!(
        "../../../../plugins/bundled/plugin-cli/plugin.toml"
    )) {
        tracing::warn!("failed to register bundled plugin-cli plugin: {e}");
    }

    #[cfg(feature = "bundled-plugin-windows")]
    if let Err(e) = registry.register_bundled_manifest(include_str!(
        "../../../../plugins/bundled/windows/plugin.toml"
    )) {
        tracing::warn!("failed to register bundled windows plugin: {e}");
    }
}

/// Look up the [`StaticPluginVtable`] for a statically-linked bundled plugin.
///
/// Returns `None` when the plugin id does not correspond to a compiled-in
/// plugin (either because the feature flag is off or it's a third-party
/// plugin).
#[allow(unused_variables)]
fn static_bundled_vtable(plugin_id: &str) -> Option<bmux_plugin::StaticPluginVtable> {
    #[cfg(feature = "bundled-plugin-clipboard")]
    if plugin_id == "bmux.clipboard" {
        return Some(bmux_plugin::bundled_plugin_vtable!(
            bmux_clipboard_plugin::ClipboardPlugin
        ));
    }
    #[cfg(feature = "bundled-plugin-permissions")]
    if plugin_id == "bmux.permissions" {
        return Some(bmux_plugin::bundled_plugin_vtable!(
            bmux_permissions_plugin::PermissionsPlugin
        ));
    }
    #[cfg(feature = "bundled-plugin-cli")]
    if plugin_id == "bmux.plugin_cli" {
        return Some(bmux_plugin::bundled_plugin_vtable!(
            bmux_plugin_cli_plugin::PluginCliPlugin
        ));
    }
    #[cfg(feature = "bundled-plugin-windows")]
    if plugin_id == "bmux.windows" {
        return Some(bmux_plugin::bundled_plugin_vtable!(
            bmux_windows_plugin::WindowsPlugin
        ));
    }
    None
}

/// Load a registered plugin, using the static vtable path for bundled plugins
/// and the dynamic `dlopen` path for everything else.
fn load_plugin(
    plugin: &bmux_plugin::RegisteredPlugin,
    host: &HostMetadata,
    available_capabilities: &std::collections::BTreeMap<HostScope, bmux_plugin::CapabilityProvider>,
) -> bmux_plugin::Result<bmux_plugin::LoadedPlugin> {
    if plugin.bundled_static {
        let vtable = static_bundled_vtable(plugin.declaration.id.as_str()).ok_or_else(|| {
            bmux_plugin::PluginError::MissingStaticVtable {
                plugin_id: plugin.declaration.id.as_str().to_string(),
            }
        })?;
        bmux_plugin::load_static_plugin(plugin, vtable, host, available_capabilities)
    } else {
        load_native_registered_plugin(plugin, host, available_capabilities)
    }
}

pub(crate) fn scan_available_plugins(
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<PluginRegistry> {
    let workspace_bundled_root = workspace_bundled_plugin_root();
    let search_paths = resolve_plugin_search_paths(config, paths)?;
    let reports = bmux_plugin::discover_plugin_manifests_in_roots(&search_paths)?;
    let mut registry = PluginRegistry::new();

    // Register statically-linked bundled plugins first (behind feature flags).
    register_static_bundled_plugins(&mut registry);

    for report in reports {
        for manifest_path in report.manifest_paths {
            match PluginManifest::from_path(&manifest_path) {
                Ok(mut manifest) => {
                    let entry_path = manifest.resolve_entry_path(
                        manifest_path
                            .parent()
                            .unwrap_or_else(|| std::path::Path::new(".")),
                    );
                    if !entry_path.exists()
                        && workspace_bundled_root
                            .as_ref()
                            .is_some_and(|root| report.search_root == *root)
                        && let Ok(executable) = std::env::current_exe()
                        && let Some(executable_dir) = executable.parent()
                    {
                        let executable_candidate = executable_dir.join(&manifest.entry);
                        if executable_candidate.exists() {
                            manifest.entry = executable_candidate;
                        }
                    }
                    if let Err(error) = registry.register_manifest_from_root(
                        &report.search_root,
                        &manifest_path,
                        manifest,
                    ) {
                        // DuplicatePluginId is expected when a static-bundled
                        // plugin is also discovered on the filesystem -- skip
                        // since the static registration already won.
                        if matches!(error, bmux_plugin::PluginError::DuplicatePluginId { .. }) {
                            debug!(
                                "skipping filesystem plugin {} (duplicate of static-bundled plugin)",
                                manifest_path.display()
                            );
                        } else {
                            warn!(
                                "skipping plugin manifest {} during enabled-plugin scan: {error}",
                                manifest_path.display()
                            );
                        }
                    }
                }
                Err(error) => {
                    warn!(
                        "skipping unreadable plugin manifest {} during enabled-plugin scan: {error}",
                        manifest_path.display()
                    );
                }
            }
        }
    }
    Ok(registry)
}

fn resolve_plugin_search_paths(config: &BmuxConfig, paths: &ConfigPaths) -> Result<Vec<PathBuf>> {
    let mut resolved = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for bundled in bundled_plugin_roots() {
        if seen.insert(bundled.clone()) {
            resolved.push(bundled);
        }
    }

    let user_plugins = paths.plugins_dir();
    if seen.insert(user_plugins.clone()) {
        resolved.push(user_plugins);
    }

    for search_path in &config.plugins.search_paths {
        let absolute = if search_path.is_absolute() {
            search_path.clone()
        } else {
            std::env::current_dir()
                .context("failed resolving current directory for plugin search path")?
                .join(search_path)
        };
        if seen.insert(absolute.clone()) {
            resolved.push(absolute);
        }
    }

    Ok(resolved)
}

fn bundled_plugin_root() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let parent = executable.parent()?;
    Some(parent.join("plugins"))
}

fn workspace_bundled_plugin_root() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent()?.parent()?;
    let bundled = root.join("plugins").join("bundled");
    bundled.exists().then_some(bundled)
}

pub(crate) fn bundled_plugin_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if let Some(root) = bundled_plugin_root()
        && seen.insert(root.clone())
    {
        roots.push(root);
    }
    if let Some(root) = workspace_bundled_plugin_root()
        && seen.insert(root.clone())
    {
        roots.push(root);
    }
    roots
}

pub(crate) fn registered_plugin_entry_exists(plugin: &bmux_plugin::RegisteredPlugin) -> bool {
    plugin
        .manifest
        .resolve_entry_path(
            plugin
                .manifest_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
        )
        .exists()
}

/// Discover bundled plugin IDs using the same dynamic discovery as the runtime.
/// Returns an empty vec on failure (non-fatal).
pub(crate) fn discover_bundled_plugin_ids() -> Vec<String> {
    let config = BmuxConfig::default();
    let paths = bmux_config::ConfigPaths::default();
    let roots = bundled_plugin_roots();

    match scan_available_plugins(&config, &paths) {
        Ok(registry) => registry
            .iter()
            .filter(|plugin| {
                roots.contains(&plugin.search_root) && registered_plugin_entry_exists(plugin)
            })
            .map(|plugin| plugin.declaration.id.as_str().to_string())
            .collect(),
        Err(e) => {
            tracing::warn!("failed to discover bundled plugins, using empty list: {e:#}");
            Vec::new()
        }
    }
}

fn effective_enabled_plugins(config: &BmuxConfig, registry: &PluginRegistry) -> Vec<String> {
    let disabled = config
        .plugins
        .disabled
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let bundled_roots = bundled_plugin_roots()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut enabled = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    // Auto-enable statically-linked bundled plugins (always available, no
    // entry file to check).
    let mut static_bundled = registry
        .iter()
        .filter(|&plugin| plugin.bundled_static)
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    static_bundled.sort();
    for plugin_id in static_bundled {
        if disabled.contains(plugin_id.as_str()) {
            continue;
        }
        if seen.insert(plugin_id.clone()) {
            enabled.push(plugin_id);
        }
    }

    // Auto-enable filesystem-discovered bundled plugins (from bundled roots
    // whose entry file exists on disk).
    let mut bundled_defaults = registry
        .iter()
        .filter(|&plugin| {
            !plugin.bundled_static
                && bundled_roots.contains(&plugin.search_root)
                && registered_plugin_entry_exists(plugin)
        })
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    bundled_defaults.sort();
    for plugin_id in bundled_defaults {
        if disabled.contains(plugin_id.as_str()) {
            continue;
        }
        if seen.insert(plugin_id.clone()) {
            enabled.push(plugin_id);
        }
    }

    for plugin_id in &config.plugins.enabled {
        if disabled.contains(plugin_id.as_str()) {
            continue;
        }
        if seen.insert(plugin_id.clone()) {
            enabled.push(plugin_id.clone());
        }
    }

    enabled
}

fn validate_enabled_plugins(config: &BmuxConfig, registry: &PluginRegistry) -> Result<()> {
    let disabled = config
        .plugins
        .disabled
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let enabled_plugins = effective_enabled_plugins(config, registry);
    if enabled_plugins.is_empty() {
        return Ok(());
    }

    for plugin_id in &config.plugins.enabled {
        if disabled.contains(plugin_id.as_str()) {
            continue;
        }
        let _ = registry.get(plugin_id).with_context(|| {
            let available = registry.plugin_ids();
            if available.is_empty() {
                format!(
                    "enabled plugin '{plugin_id}' was not found in the configured plugins directory"
                )
            } else {
                format!(
                    "enabled plugin '{plugin_id}' was not found in the configured plugins directory (available: {})",
                    available.join(", ")
                )
            }
        })?;
    }

    let _ = registry
        .activation_order_for(&enabled_plugins)
        .context("enabled plugin dependency graph is invalid")?;

    let mut command_config = config.clone();
    command_config.plugins.enabled = enabled_plugins;
    PluginCommandRegistry::build(&command_config, registry)
        .context("failed building plugin CLI command registry")?;

    Ok(())
}

fn load_enabled_plugins(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<Vec<bmux_plugin::LoadedPlugin>> {
    let enabled_plugins = effective_enabled_plugins(config, registry);
    if enabled_plugins.is_empty() {
        return Ok(Vec::new());
    }

    let disabled = config
        .plugins
        .disabled
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let explicitly_enabled = config
        .plugins
        .enabled
        .iter()
        .filter(|plugin_id| !disabled.contains(plugin_id.as_str()))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();

    for plugin_id in &enabled_plugins {
        if registry.get(plugin_id).is_some() {
            continue;
        }
        if explicitly_enabled.contains(plugin_id) {
            anyhow::bail!("enabled plugin '{plugin_id}' disappeared during native load");
        }
        warn!("skipping bundled plugin '{plugin_id}' because it is no longer discoverable");
    }

    let host = plugin_host_metadata();
    let available_capabilities = available_capability_providers(config, registry)?;
    let ordered_plugins = registry
        .activation_order_for(&enabled_plugins)
        .context("enabled plugin dependency graph is invalid")?;
    let mut loaded_plugins = Vec::with_capacity(ordered_plugins.len());
    for plugin in ordered_plugins {
        let plugin_id = plugin.declaration.id.as_str();
        let loaded = match load_plugin(plugin, &host, &available_capabilities) {
            Ok(loaded) => loaded,
            Err(error) => {
                if explicitly_enabled.contains(plugin_id) {
                    return Err(error)
                        .with_context(|| format!("failed loading enabled plugin '{plugin_id}'"));
                }
                warn!("skipping bundled plugin '{plugin_id}': {error}");
                continue;
            }
        };
        loaded_plugins.push(loaded);
    }

    Ok(loaded_plugins)
}

fn registered_plugin_infos_from_loaded(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
) -> Vec<bmux_plugin::RegisteredPluginInfo> {
    loaded_plugins
        .iter()
        .map(|plugin| bmux_plugin::RegisteredPluginInfo {
            id: plugin.declaration.id.as_str().to_string(),
            display_name: plugin.declaration.display_name.clone(),
            version: plugin.declaration.plugin_version.clone(),
            bundled_static: plugin.registered.bundled_static,
            required_capabilities: plugin
                .declaration
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            provided_capabilities: plugin
                .declaration
                .provided_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            commands: plugin
                .declaration
                .commands
                .iter()
                .map(|c| c.name.clone())
                .collect(),
        })
        .collect()
}

fn registered_plugin_infos_from_registry(
    registry: &PluginRegistry,
) -> Vec<bmux_plugin::RegisteredPluginInfo> {
    registry
        .iter()
        .map(|plugin| bmux_plugin::RegisteredPluginInfo {
            id: plugin.declaration.id.as_str().to_string(),
            display_name: plugin.declaration.display_name.clone(),
            version: plugin.declaration.plugin_version.clone(),
            bundled_static: plugin.bundled_static,
            required_capabilities: plugin
                .declaration
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            provided_capabilities: plugin
                .declaration
                .provided_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            commands: plugin
                .declaration
                .commands
                .iter()
                .map(|c| c.name.clone())
                .collect(),
        })
        .collect()
}

fn plugin_lifecycle_context(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    declaration: &bmux_plugin::PluginDeclaration,
    available_services: Vec<RegisteredService>,
    available_capabilities: Vec<String>,
    enabled_plugins: Vec<String>,
    plugin_search_roots: Vec<String>,
    registered_plugins: Vec<bmux_plugin::RegisteredPluginInfo>,
) -> NativeLifecycleContext {
    let host = plugin_host_for_declaration(declaration, paths, config, available_services.clone());
    NativeLifecycleContext {
        plugin_id: declaration.id.as_str().to_string(),
        required_capabilities: declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: available_services,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        registered_plugins,
        host: plugin_host_metadata(),
        connection: bmux_plugin::PluginHost::connection(&host).clone(),
        settings: config
            .plugins
            .settings
            .get(declaration.id.as_str())
            .cloned(),
        plugin_settings_map: config.plugins.settings.clone(),
        host_kernel_bridge: Some(bmux_plugin::HostKernelBridge::from_fn(host_kernel_bridge)),
    }
}

fn plugin_command_context(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    declaration: &bmux_plugin::PluginDeclaration,
    command: &str,
    arguments: &[String],
    available_services: Vec<RegisteredService>,
    available_capabilities: Vec<String>,
    enabled_plugins: Vec<String>,
    plugin_search_roots: Vec<String>,
    registered_plugins: Vec<bmux_plugin::RegisteredPluginInfo>,
) -> NativeCommandContext {
    let host = plugin_host_for_declaration(declaration, paths, config, available_services.clone());
    NativeCommandContext {
        plugin_id: declaration.id.as_str().to_string(),
        command: command.to_string(),
        arguments: arguments.to_vec(),
        required_capabilities: declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: available_services,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        registered_plugins,
        host: plugin_host_metadata(),
        connection: bmux_plugin::PluginHost::connection(&host).clone(),
        settings: config
            .plugins
            .settings
            .get(declaration.id.as_str())
            .cloned(),
        plugin_settings_map: config.plugins.settings.clone(),
        host_kernel_bridge: Some(bmux_plugin::HostKernelBridge::from_fn(host_kernel_bridge)),
    }
}

fn plugin_system_event(name: &str) -> PluginEvent {
    PluginEvent {
        kind: PluginEventKind::System,
        name: name.to_string(),
        payload: serde_json::json!({
            "product": "bmux",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    }
}

fn activate_loaded_plugins(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<()> {
    let mut activated: Vec<&bmux_plugin::LoadedPlugin> = Vec::new();
    let connection_info = HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let plugin_search_roots = resolve_plugin_search_paths(config, paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let available_capabilities = core_provided_capabilities()
        .into_iter()
        .chain(
            loaded_plugins
                .iter()
                .flat_map(|plugin| plugin.declaration.provided_capabilities.iter().cloned()),
        )
        .map(|capability| capability.to_string())
        .collect::<Vec<_>>();
    let available_services = service_descriptors_from_declarations(
        loaded_plugins.iter().map(|plugin| &plugin.declaration),
    );
    let enabled_plugins = loaded_plugins
        .iter()
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    let registered_plugins = registered_plugin_infos_from_loaded(loaded_plugins);
    for plugin in loaded_plugins {
        if !plugin.declaration.lifecycle.activate_on_startup {
            continue;
        }

        let context = plugin_lifecycle_context(
            config,
            paths,
            &plugin.declaration,
            available_services.clone(),
            available_capabilities.clone(),
            enabled_plugins.clone(),
            plugin_search_roots.clone(),
            registered_plugins.clone(),
        );
        let _host_kernel_connection_guard = enter_host_kernel_connection(connection_info.clone());
        if let Err(error) = plugin.activate(&context) {
            for activated_plugin in activated.into_iter().rev() {
                let context = plugin_lifecycle_context(
                    config,
                    paths,
                    &activated_plugin.declaration,
                    available_services.clone(),
                    available_capabilities.clone(),
                    enabled_plugins.clone(),
                    plugin_search_roots.clone(),
                    registered_plugins.clone(),
                );
                let _host_kernel_connection_guard =
                    enter_host_kernel_connection(connection_info.clone());
                if let Err(deactivate_error) = activated_plugin.deactivate(&context) {
                    warn!(
                        "failed rolling back plugin activation for {}: {deactivate_error}",
                        activated_plugin.declaration.id.as_str()
                    );
                }
            }
            return Err(error).with_context(|| {
                format!(
                    "failed activating plugin '{}'",
                    plugin.declaration.id.as_str()
                )
            });
        }

        activated.push(plugin);
    }

    Ok(())
}

fn deactivate_loaded_plugins(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<()> {
    let connection_info = HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let plugin_search_roots = resolve_plugin_search_paths(config, paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let available_capabilities = core_provided_capabilities()
        .into_iter()
        .chain(
            loaded_plugins
                .iter()
                .flat_map(|plugin| plugin.declaration.provided_capabilities.iter().cloned()),
        )
        .map(|capability| capability.to_string())
        .collect::<Vec<_>>();
    let available_services = service_descriptors_from_declarations(
        loaded_plugins.iter().map(|plugin| &plugin.declaration),
    );
    let enabled_plugins = loaded_plugins
        .iter()
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    let registered_plugins = registered_plugin_infos_from_loaded(loaded_plugins);
    for plugin in loaded_plugins.iter().rev() {
        if !plugin.declaration.lifecycle.activate_on_startup {
            continue;
        }

        let context = plugin_lifecycle_context(
            config,
            paths,
            &plugin.declaration,
            available_services.clone(),
            available_capabilities.clone(),
            enabled_plugins.clone(),
            plugin_search_roots.clone(),
            registered_plugins.clone(),
        );
        let _host_kernel_connection_guard = enter_host_kernel_connection(connection_info.clone());
        let _ = plugin.deactivate(&context).with_context(|| {
            format!(
                "failed deactivating plugin '{}'",
                plugin.declaration.id.as_str()
            )
        })?;
    }

    Ok(())
}

fn dispatch_loaded_plugin_event(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    event: PluginEvent,
) -> Result<()> {
    for plugin in loaded_plugins {
        let _ = plugin.dispatch_event(&event).with_context(|| {
            format!(
                "failed dispatching plugin event '{}' to '{}'",
                event.name,
                plugin.declaration.id.as_str()
            )
        })?;
    }

    Ok(())
}

async fn plugin_event_bridge_loop(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if loaded_plugins.is_empty() {
        return Ok(());
    }

    let mut client = loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }

        match connect_raw("bmux-plugin-event-bridge").await {
            Ok(client) => break client,
            Err(_) => {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        }
    };

    client
        .subscribe_events()
        .await
        .map_err(map_cli_client_error)?;
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            result = client.poll_events(32) => {
                let events = result.map_err(map_cli_client_error)?;
                for event in events {
                    dispatch_loaded_plugin_event(loaded_plugins, plugin_event_from_server_event(&event)?)?;
                }
            }
        }
    }
}

fn plugin_event_from_server_event(event: &bmux_client::ServerEvent) -> Result<PluginEvent> {
    Ok(PluginEvent {
        kind: plugin_event_kind_from_server_event(event),
        name: server_event_name(event).to_string(),
        payload: serde_json::to_value(event).context("failed encoding server event payload")?,
    })
}

const fn plugin_event_kind_from_server_event(event: &bmux_client::ServerEvent) -> PluginEventKind {
    match event {
        bmux_client::ServerEvent::ServerStarted | bmux_client::ServerEvent::ServerStopping => {
            PluginEventKind::System
        }
        bmux_client::ServerEvent::SessionCreated { .. }
        | bmux_client::ServerEvent::SessionRemoved { .. }
        | bmux_client::ServerEvent::FollowStarted { .. }
        | bmux_client::ServerEvent::FollowStopped { .. }
        | bmux_client::ServerEvent::FollowTargetGone { .. }
        | bmux_client::ServerEvent::FollowTargetChanged { .. } => PluginEventKind::Session,
        bmux_client::ServerEvent::ClientAttached { .. }
        | bmux_client::ServerEvent::ClientDetached { .. } => PluginEventKind::Client,
        bmux_client::ServerEvent::AttachViewChanged { .. } => PluginEventKind::Pane,
    }
}

async fn run_plugin_command(plugin_id: &str, command_name: &str, args: &[String]) -> Result<u8> {
    let status = run_plugin_command_internal(plugin_id, command_name, args)?.status;
    Ok(status.clamp(0, i32::from(u8::MAX)) as u8)
}

fn run_plugin_keybinding_command(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> Result<PluginCommandExecution> {
    run_plugin_command_internal(plugin_id, command_name, args)
}

struct PluginCommandExecution {
    status: i32,
    outcome: PluginCommandOutcome,
}

fn run_plugin_command_internal(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> Result<PluginCommandExecution> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let available = registry.plugin_ids();
    let plugin = registry
        .get(plugin_id)
        .with_context(|| format_plugin_not_found_message(plugin_id, &available))?;
    let enabled_plugins = effective_enabled_plugins(&config, &registry);

    if !enabled_plugins.iter().any(|enabled| enabled == plugin_id) {
        anyhow::bail!(format_plugin_not_enabled_message(plugin_id));
    }

    let loaded = load_plugin(
        plugin,
        &plugin_host_metadata(),
        &available_capability_providers(&config, &registry)?,
    )
    .with_context(|| format!("failed loading enabled plugin '{plugin_id}'"))?;
    let plugin_search_roots = resolve_plugin_search_paths(&config, &paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let available_capabilities = available_capability_providers(&config, &registry)?
        .into_keys()
        .map(|capability| capability.to_string())
        .collect::<Vec<_>>();
    let context = plugin_command_context(
        &config,
        &paths,
        &plugin.declaration,
        command_name,
        args,
        available_service_descriptors(&config, &registry)?,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        registered_plugin_infos_from_registry(&registry),
    );
    begin_host_kernel_effect_capture();
    let _host_kernel_connection_guard = enter_host_kernel_connection(context.connection.clone());
    let run_result =
        loaded.run_command_with_context_and_outcome(command_name, args, Some(&context));
    let fallback_effects = finish_host_kernel_effect_capture();
    let (status, mut outcome) = run_result.map_err(|error| {
        anyhow::anyhow!(format_plugin_command_run_error(
            plugin_id,
            command_name,
            &error
        ))
    })?;
    if outcome.effects.is_empty() && !fallback_effects.is_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for effect in fallback_effects {
            match effect {
                PluginCommandEffect::SelectContext { context_id } if seen.insert(context_id) => {
                    outcome
                        .effects
                        .push(PluginCommandEffect::SelectContext { context_id });
                }
                PluginCommandEffect::SelectContext { .. } => {}
            }
        }
    }
    Ok(PluginCommandExecution { status, outcome })
}

fn format_plugin_command_run_error(
    plugin_id: &str,
    command_name: &str,
    error: &dyn std::fmt::Display,
) -> String {
    let base = format!("failed running plugin command '{plugin_id}:{command_name}': {error}");
    if base.contains("session policy denied for this operation") {
        format!(
            "{base}\nHint: operation denied by an active policy provider. Verify policy state or run with an authorized principal."
        )
    } else {
        base
    }
}

fn format_plugin_not_found_message<S: AsRef<str>>(plugin_id: &str, available: &[S]) -> String {
    if available.is_empty() {
        format!("plugin '{plugin_id}' was not found")
    } else {
        let available = available
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect::<Vec<_>>();
        format!(
            "plugin '{plugin_id}' was not found (available: {})",
            available.join(", ")
        )
    }
}

fn format_plugin_not_enabled_message(plugin_id: &str) -> String {
    format!(
        "plugin '{plugin_id}' is not enabled; remove it from plugins.disabled or add it under plugins.enabled to run commands"
    )
}

fn unknown_external_command_message(args: &[String]) -> String {
    format!(
        "unknown command '{}'; run 'bmux plugin list' to inspect available plugins",
        args.join(" ")
    )
}

fn format_plugin_argument_validation_error(
    command_path: &[String],
    error: &dyn std::fmt::Display,
) -> String {
    let base = format!(
        "failed validating plugin command arguments for '{}': {error}",
        command_path.join(" ")
    );
    if base.contains("missing required") {
        format!("{base}\nHint: run '<command> --help' to inspect required plugin options.")
    } else {
        base
    }
}

async fn run_external_plugin_command(args: &[String]) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let mut command_config = config.clone();
    command_config.plugins.enabled = effective_enabled_plugins(&config, &registry);
    let command_registry = PluginCommandRegistry::build(&command_config, &registry)
        .context("failed building plugin CLI command registry")?;
    let resolved = command_registry
        .resolve(args)
        .with_context(|| unknown_external_command_message(args))?;
    let validated_arguments =
        PluginCommandRegistry::validate_arguments(&resolved.schema, &resolved.arguments).map_err(
            |error| anyhow::anyhow!(format_plugin_argument_validation_error(args, &error)),
        )?;
    run_plugin_command(
        &resolved.plugin_id,
        &resolved.command_name,
        &validated_arguments,
    )
    .await
}

#[derive(Debug, serde::Serialize)]
struct ServerStatusJsonPayload {
    running: bool,
    principal_id: Option<Uuid>,
    server_control_principal_id: Option<Uuid>,
    force_local_permitted: bool,
    latest_server_event: Option<String>,
    snapshot: Option<bmux_ipc::ServerSnapshotStatus>,
    server_metadata: Option<ServerRuntimeMetadata>,
    cli_build: Option<String>,
    stale_build: bool,
    stale_warning: Option<String>,
}

async fn run_server_status(as_json: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let status = fetch_server_status().await?;
    let metadata = read_server_runtime_metadata()?;
    let current_build_id = current_cli_build_id().ok();
    let stale_warning = metadata.as_ref().and_then(|entry| {
        current_build_id
            .as_ref()
            .filter(|build| entry.build_id != **build)
            .map(|build| {
                format!(
                    "running server build ({}) differs from current CLI build ({}); restart with `bmux server stop`",
                    entry.build_id, build
                )
            })
    });
    let stale_build = stale_warning.is_some();

    if as_json {
        let latest_event = if matches!(status, Some(ref s) if s.running) {
            latest_server_event_name().await?.map(str::to_string)
        } else {
            None
        };
        let payload = ServerStatusJsonPayload {
            running: matches!(status, Some(ref s) if s.running),
            principal_id: status.as_ref().map(|entry| entry.principal_id),
            server_control_principal_id: status
                .as_ref()
                .map(|entry| entry.server_control_principal_id),
            force_local_permitted: status
                .as_ref()
                .is_some_and(|entry| entry.principal_id == entry.server_control_principal_id),
            latest_server_event: latest_event,
            snapshot: status.as_ref().map(|entry| entry.snapshot.clone()),
            server_metadata: metadata,
            cli_build: current_build_id,
            stale_build,
            stale_warning,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).context("failed encoding server status json")?
        );
        return Ok(u8::from(!payload.running));
    }

    match status {
        Some(status) if status.running => {
            if let Some(event_name) = latest_server_event_name().await? {
                println!("latest server event: {event_name}");
            }
            if let Some(metadata) = metadata.as_ref() {
                println!("server pid: {}", metadata.pid);
                println!("server version: {}", metadata.version);
                println!("server build: {}", metadata.build_id);
                println!("server executable: {}", metadata.executable_path);
                println!("server started_at_ms: {}", metadata.started_at_epoch_ms);
            } else {
                println!("server metadata: missing");
            }
            if let Some(build_id) = current_build_id.as_ref() {
                println!("cli build: {build_id}");
                if let Some(warning) = stale_warning.as_ref() {
                    println!("warning: {warning}");
                }
            }
            println!("principal id: {}", status.principal_id);
            println!(
                "server control principal id: {}",
                status.server_control_principal_id
            );
            println!(
                "force-local permitted: {}",
                if status.principal_id == status.server_control_principal_id {
                    "yes"
                } else {
                    "no"
                }
            );
            println!(
                "snapshot: {}{}",
                if status.snapshot.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status
                    .snapshot
                    .path
                    .as_ref()
                    .map_or(String::new(), |path| format!(" ({path})"))
            );
            if status.snapshot.enabled {
                println!(
                    "snapshot file: {}",
                    if status.snapshot.snapshot_exists {
                        "present"
                    } else {
                        "missing"
                    }
                );
                if let Some(last_write) = status.snapshot.last_write_epoch_ms {
                    println!("snapshot last write (ms): {last_write}");
                }
                if let Some(last_restore) = status.snapshot.last_restore_epoch_ms {
                    println!("snapshot last restore (ms): {last_restore}");
                }
                if let Some(error) = status.snapshot.last_restore_error.as_ref() {
                    println!("snapshot last error: {error}");
                }
            }
            println!("bmux server is running");
            Ok(0)
        }
        _ => {
            println!("bmux server is not running");
            Ok(1)
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct ServerWhoAmIPrincipalJsonPayload {
    principal_id: Uuid,
    server_control_principal_id: Uuid,
    force_local_permitted: bool,
}

async fn run_server_whoami_principal(as_json: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_raw("bmux-cli-server-whoami-principal").await?;
    let identity = client
        .whoami_principal()
        .await
        .map_err(map_cli_client_error)?;

    if as_json {
        let payload = ServerWhoAmIPrincipalJsonPayload {
            principal_id: identity.principal_id,
            server_control_principal_id: identity.server_control_principal_id,
            force_local_permitted: identity.force_local_permitted,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed encoding server whoami-principal json")?
        );
        return Ok(0);
    }

    println!("principal id: {}", identity.principal_id);
    println!(
        "server control principal id: {}",
        identity.server_control_principal_id
    );
    println!(
        "force-local permitted: {}",
        if identity.force_local_permitted {
            "yes"
        } else {
            "no"
        }
    );
    Ok(0)
}

async fn run_server_save() -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-server-save").await?;
    let path = client.server_save().await.map_err(map_cli_client_error)?;

    match path {
        Some(path) => println!("snapshot saved: {path}"),
        None => println!("snapshot save requested"),
    }
    Ok(0)
}

async fn run_server_restore(dry_run: bool, yes: bool) -> Result<u8> {
    if !dry_run && !yes {
        anyhow::bail!("server restore requires either --dry-run or --yes");
    }
    cleanup_stale_pid_file().await?;

    if dry_run {
        let mut client = connect(
            ConnectionPolicyScope::Normal,
            "bmux-cli-server-restore-dry-run",
        )
        .await?;
        let (ok, message) = client
            .server_restore_dry_run()
            .await
            .map_err(map_cli_client_error)?;

        if ok {
            println!("restore dry-run: OK - {message}");
            return Ok(0);
        }
        println!("restore dry-run: FAIL - {message}");
        return Ok(1);
    }

    let mut client = connect(
        ConnectionPolicyScope::Normal,
        "bmux-cli-server-restore-apply",
    )
    .await?;
    let summary = client
        .server_restore_apply()
        .await
        .map_err(map_cli_client_error)?;

    println!(
        "restore applied: sessions={}, follows={}, selected_sessions={}",
        summary.sessions, summary.follows, summary.selected_sessions
    );
    Ok(0)
}

async fn latest_server_event_name() -> Result<Option<&'static str>> {
    let connect =
        tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-status-events")).await;

    let mut client = match connect {
        Ok(Ok(client)) => client,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    let _ = tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.subscribe_events()).await;
    let events = match tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.poll_events(1)).await {
        Ok(Ok(events)) => events,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };
    Ok(events.last().map(server_event_name))
}

const fn server_event_name(event: &bmux_client::ServerEvent) -> &'static str {
    match event {
        bmux_client::ServerEvent::ServerStarted => "server_started",
        bmux_client::ServerEvent::ServerStopping => "server_stopping",
        bmux_client::ServerEvent::SessionCreated { .. } => "session_created",
        bmux_client::ServerEvent::SessionRemoved { .. } => "session_removed",
        bmux_client::ServerEvent::ClientAttached { .. } => "client_attached",
        bmux_client::ServerEvent::ClientDetached { .. } => "client_detached",
        bmux_client::ServerEvent::FollowStarted { .. } => "follow_started",
        bmux_client::ServerEvent::FollowStopped { .. } => "follow_stopped",
        bmux_client::ServerEvent::FollowTargetGone { .. } => "follow_target_gone",
        bmux_client::ServerEvent::FollowTargetChanged { .. } => "follow_target_changed",
        bmux_client::ServerEvent::AttachViewChanged { .. } => "attach_view_changed",
    }
}

async fn run_server_stop() -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let graceful_stopped =
        match tokio::time::timeout(SERVER_STOP_TIMEOUT, connect_raw("bmux-cli-stop")).await {
            Ok(Ok(mut client)) => {
                client.stop_server().await.map_err(map_cli_client_error)?;
                wait_until_server_stopped(SERVER_STOP_TIMEOUT).await?
            }
            Ok(Err(_)) | Err(_) => false,
        };

    if graceful_stopped {
        println!("bmux server stopped gracefully");
        let _ = remove_server_pid_file();
        return Ok(0);
    }

    if let Some(pid) = read_server_pid_file()? {
        if try_kill_pid(pid)? {
            if wait_for_process_exit(pid, SERVER_STOP_TIMEOUT)? {
                println!("bmux server stop fallback succeeded (pid {pid})");
                let _ = remove_server_pid_file();
                return Ok(0);
            }
        } else if !is_pid_running(pid)? {
            let _ = remove_server_pid_file();
        }
    }

    println!("bmux server is not running");
    Ok(1)
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
) -> Result<u8> {
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

fn run_logs_path(as_json: bool) -> Result<u8> {
    let path = active_log_file_path();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "path": path }))
                .context("failed to encode log path json")?
        );
        return Ok(0);
    }
    println!("{}", path.display());
    Ok(0)
}

fn run_logs_level(as_json: bool) -> Result<u8> {
    let level = EFFECTIVE_LOG_LEVEL.get().copied().unwrap_or(Level::INFO);
    let value = match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    };
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "level": value }))
                .context("failed to encode log level json")?
        );
        return Ok(0);
    }
    println!("{value}");
    Ok(0)
}

fn run_logs_tail(lines: usize, since: Option<&str>, follow: bool) -> Result<u8> {
    let path = active_log_file_path();
    if !path.exists() {
        println!(
            "no log file in {} (expected prefix: bmux.log)",
            ConfigPaths::default().logs_dir().display()
        );
        return Ok(0);
    }

    let since_cutoff = match since {
        Some(value) => Some(parse_since_cutoff(value)?),
        None => None,
    };

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed reading log file {}", path.display()))?;
    let all_lines = content
        .lines()
        .filter(|line| line_matches_since(line, since_cutoff))
        .collect::<Vec<_>>();
    let start = all_lines.len().saturating_sub(lines.max(1));
    for line in &all_lines[start..] {
        println!("{line}");
    }

    if !follow {
        return Ok(0);
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .with_context(|| format!("failed opening log file {}", path.display()))?;
    let mut read_offset = file
        .metadata()
        .with_context(|| format!("failed reading metadata for {}", path.display()))?
        .len();

    loop {
        let metadata = file
            .metadata()
            .with_context(|| format!("failed reading metadata for {}", path.display()))?;
        let file_len = metadata.len();
        if file_len < read_offset {
            read_offset = 0;
        }
        if file_len > read_offset {
            file.seek(std::io::SeekFrom::Start(read_offset))
                .with_context(|| format!("failed seeking {}", path.display()))?;
            let mut chunk = String::new();
            file.read_to_string(&mut chunk)
                .with_context(|| format!("failed reading appended logs from {}", path.display()))?;
            if !chunk.is_empty() {
                print!("{chunk}");
                io::stdout().flush().context("failed flushing log output")?;
            }
            read_offset = file_len;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn parse_since_cutoff(raw: &str) -> Result<OffsetDateTime> {
    let duration = parse_since_duration(raw)?;
    let now = OffsetDateTime::now_utc();
    Ok(now - duration)
}

fn parse_since_duration(raw: &str) -> Result<TimeDuration> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("--since must be a non-empty duration like 30s, 10m, 2h, or 1d");
    }

    let split_at = trimmed
        .find(|char: char| !char.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (value_part, unit_part) = trimmed.split_at(split_at);
    if value_part.is_empty() {
        anyhow::bail!("--since must start with a number");
    }

    let amount = value_part
        .parse::<i64>()
        .with_context(|| format!("invalid --since value '{raw}'"))?;
    if amount < 0 {
        anyhow::bail!("--since must be non-negative");
    }

    let duration = match unit_part {
        "" | "s" => TimeDuration::seconds(amount),
        "m" => TimeDuration::minutes(amount),
        "h" => TimeDuration::hours(amount),
        "d" => TimeDuration::days(amount),
        _ => {
            anyhow::bail!(
                "invalid --since unit '{unit_part}' (use s, m, h, d; example: 30s, 10m, 2h, 1d)"
            )
        }
    };
    Ok(duration)
}

fn line_matches_since(line: &str, cutoff: Option<OffsetDateTime>) -> bool {
    let Some(cutoff) = cutoff else {
        return true;
    };
    let Some(timestamp) = line.split_whitespace().next() else {
        return false;
    };
    let Ok(parsed) = OffsetDateTime::parse(timestamp, &Rfc3339) else {
        return false;
    };
    parsed >= cutoff
}

async fn run_session_new(name: Option<String>) -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-new-session").await?;
    let session_id = client
        .new_session(name)
        .await
        .map_err(map_cli_client_error)?;
    println!("created session: {session_id}");
    Ok(0)
}

async fn run_session_list(as_json: bool) -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-list-sessions").await?;
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&sessions).context("failed to encode sessions json")?
        );
        return Ok(0);
    }

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(0);
    }

    println!("ID                                   NAME            CLIENTS");
    for session in sessions {
        let name = session.name.unwrap_or_else(|| "-".to_string());
        println!("{:<36} {:<15} {}", session.id, name, session.client_count);
    }

    Ok(0)
}

async fn run_client_list(as_json: bool) -> Result<u8> {
    let mut api = connect(ConnectionPolicyScope::Normal, "bmux-cli-list-clients").await?;
    let self_id = api.whoami().await.map_err(map_cli_client_error)?;
    let clients = api.list_clients().await.map_err(map_cli_client_error)?;
    let mut clients = clients;
    clients.sort_by_key(|client| (client.id != self_id, client.id));

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&clients).context("failed to encode clients json")?
        );
        return Ok(0);
    }

    if clients.is_empty() {
        println!("no clients");
        return Ok(0);
    }

    let sessions = api.list_sessions().await.map_err(map_cli_client_error)?;
    println!(
        "ID                                   SELF SESSION          CONTEXT      FOLLOWING_CLIENT                     GLOBAL"
    );
    for client_summary in clients {
        let selected_session = client_summary.selected_session_id.map_or_else(
            || "-".to_string(),
            |id| {
                sessions
                    .iter()
                    .find(|session| session.id == id)
                    .map_or_else(
                        || format!("session-{}", short_uuid(id)),
                        session_summary_label,
                    )
            },
        );
        let selected_context = "-".to_string();
        let following_client = client_summary
            .following_client_id
            .map_or_else(|| "-".to_string(), |id| id.to_string());
        println!(
            "{:<36} {:<4} {:<16} {:<12} {:<36} {}",
            client_summary.id,
            if client_summary.id == self_id {
                "yes"
            } else {
                "no"
            },
            selected_session,
            selected_context,
            following_client,
            if client_summary.following_global {
                "yes"
            } else {
                "no"
            }
        );
    }

    Ok(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestructiveOpErrorKind {
    SessionPolicyDenied,
    ForceLocalUnauthorized,
    NotFound,
    Other,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct KillFailureSummary {
    policy_denied: usize,
    not_found: usize,
    other: usize,
}

impl KillFailureSummary {
    const fn record(&mut self, kind: DestructiveOpErrorKind) {
        match kind {
            DestructiveOpErrorKind::SessionPolicyDenied
            | DestructiveOpErrorKind::ForceLocalUnauthorized => {
                self.policy_denied = self.policy_denied.saturating_add(1);
            }
            DestructiveOpErrorKind::NotFound => {
                self.not_found = self.not_found.saturating_add(1);
            }
            DestructiveOpErrorKind::Other => {
                self.other = self.other.saturating_add(1);
            }
        }
    }
}

fn classify_destructive_op_error(error: &ClientError) -> DestructiveOpErrorKind {
    match error {
        ClientError::ServerError { code, message } => match code {
            bmux_ipc::ErrorCode::InvalidRequest
                if message.contains("session policy denied for this operation") =>
            {
                DestructiveOpErrorKind::SessionPolicyDenied
            }
            bmux_ipc::ErrorCode::InvalidRequest
                if message
                    .contains("force-local is only allowed for the server control principal") =>
            {
                DestructiveOpErrorKind::ForceLocalUnauthorized
            }
            bmux_ipc::ErrorCode::NotFound => DestructiveOpErrorKind::NotFound,
            _ => DestructiveOpErrorKind::Other,
        },
        _ => DestructiveOpErrorKind::Other,
    }
}

fn format_destructive_op_error(noun: &str, error: ClientError, force_local: bool) -> String {
    match classify_destructive_op_error(&error) {
        DestructiveOpErrorKind::SessionPolicyDenied => format!(
            "{noun} kill is not permitted by current session policy.{}",
            if force_local {
                " If you intended to override locally, use `--force-local` only from the server control principal."
            } else {
                ""
            }
        ),
        DestructiveOpErrorKind::ForceLocalUnauthorized =>
            "`--force-local` is only available to the server control principal. Check `bmux server whoami-principal`."
                .to_string(),
        DestructiveOpErrorKind::NotFound => map_cli_client_error(error).to_string(),
        DestructiveOpErrorKind::Other => map_cli_client_error(error).to_string(),
    }
}

async fn kill_preflight_identity(
    client: &mut BmuxClient,
    force_local: bool,
) -> Result<Option<bmux_client::PrincipalIdentityInfo>> {
    if !force_local {
        return Ok(None);
    }
    let identity = client
        .whoami_principal()
        .await
        .map_err(map_cli_client_error)?;
    if !identity.force_local_permitted {
        anyhow::bail!(
            "`--force-local` is only available to the server control principal.\ncurrent principal: {}\nserver control principal: {}\nInspect with `bmux server whoami-principal`.",
            identity.principal_id,
            identity.server_control_principal_id
        );
    }
    Ok(Some(identity))
}

async fn print_bulk_kill_preflight(
    client: &mut BmuxClient,
    noun: &str,
    force_local: bool,
) -> Result<Option<bmux_client::PrincipalIdentityInfo>> {
    let identity = client
        .whoami_principal()
        .await
        .map_err(map_cli_client_error)?;
    if force_local {
        if !identity.force_local_permitted {
            anyhow::bail!(
                "`--force-local` is only available to the server control principal.\ncurrent principal: {}\nserver control principal: {}\nInspect with `bmux server whoami-principal`.",
                identity.principal_id,
                identity.server_control_principal_id
            );
        }
        println!(
            "kill-all {noun}: force-local enabled for principal {}",
            identity.principal_id
        );
        return Ok(Some(identity));
    }

    println!(
        "kill-all {noun}: principal {} (server control: {})",
        identity.principal_id, identity.server_control_principal_id
    );
    println!("note: {noun} operations may fail depending on active session policy provider");
    Ok(Some(identity))
}

fn print_bulk_kill_failure_summary(noun: &str, summary: KillFailureSummary) {
    if summary == KillFailureSummary::default() {
        return;
    }
    println!(
        "{noun} kill failures: policy_denied={}, not_found={}, other={}",
        summary.policy_denied, summary.not_found, summary.other
    );
    if summary.policy_denied > 0 {
        println!(
            "hint: inspect active policy provider configuration or identity with `bmux server whoami-principal`"
        );
    }
}

fn attach_quit_failure_status(error: &ClientError) -> &'static str {
    match classify_destructive_op_error(error) {
        DestructiveOpErrorKind::SessionPolicyDenied => "quit blocked by session policy",
        DestructiveOpErrorKind::ForceLocalUnauthorized => {
            "quit requires server control principal for --force-local"
        }
        DestructiveOpErrorKind::NotFound => "quit failed: session not found",
        DestructiveOpErrorKind::Other => "quit failed",
    }
}

async fn run_session_kill(target: &str, force_local: bool) -> Result<u8> {
    let selector = parse_session_selector(target);
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-kill-session").await?;
    let _ = kill_preflight_identity(&mut client, force_local).await?;
    let killed_id = client
        .kill_session_with_options(selector, force_local)
        .await
        .map_err(|error| {
            anyhow::anyhow!(format_destructive_op_error("session", error, force_local))
        })?;
    println!("killed session: {killed_id}");
    Ok(0)
}

async fn run_session_kill_all(force_local: bool) -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-kill-all-sessions").await?;
    let _ = print_bulk_kill_preflight(&mut client, "sessions", force_local).await?;
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(0);
    }

    let mut killed_count = 0usize;
    let mut failed_count = 0usize;
    let mut failure_summary = KillFailureSummary::default();
    for session in sessions {
        match client
            .kill_session_with_options(SessionSelector::ById(session.id), force_local)
            .await
        {
            Ok(killed_id) => {
                println!("killed session: {killed_id}");
                killed_count = killed_count.saturating_add(1);
            }
            Err(error) => {
                failed_count = failed_count.saturating_add(1);
                let kind = classify_destructive_op_error(&error);
                failure_summary.record(kind);
                let mapped_error = format_destructive_op_error("session", error, force_local);
                eprintln!("failed killing session {}: {mapped_error}", session.id);
            }
        }
    }

    println!("kill-all-sessions complete: killed {killed_count}, failed {failed_count}");
    print_bulk_kill_failure_summary("session", failure_summary);
    Ok(u8::from(failed_count != 0))
}

async fn run_session_attach(
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<u8> {
    let client = connect(ConnectionPolicyScope::Normal, "bmux-cli-attach").await?;
    run_session_attach_with_client(client, target, follow, global, None).await
}

async fn run_session_attach_with_client(
    mut client: BmuxClient,
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
    capture_plan: Option<AttachDisplayCapturePlan>,
) -> Result<u8> {
    if target.is_none() && follow.is_none() {
        anyhow::bail!("attach requires a session target or --follow <client-uuid>");
    }
    if target.is_some() && follow.is_some() {
        anyhow::bail!("attach accepts either a session target or --follow, not both");
    }

    let follow_target_id = match follow {
        Some(follow_target) => Some(parse_uuid_value(follow_target, "follow target client id")?),
        None => None,
    };

    let attach_config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "bmux warning: failed loading config for attach keymap, using defaults ({error})"
            );
            BmuxConfig::default()
        }
    };
    let attach_keymap = attach_keymap_from_config(&attach_config);
    let attach_help_lines = build_attach_help_lines(&attach_config);

    if let Some(leader_client_id) = follow_target_id {
        client
            .subscribe_events()
            .await
            .map_err(map_attach_client_error)?;
        client
            .follow_client(leader_client_id, global)
            .await
            .map_err(map_attach_client_error)?;
    }

    let self_client_id = client.whoami().await.map_err(map_attach_client_error)?;
    let mut display_capture = recording::DisplayCaptureWriter::new(capture_plan, self_client_id)?;

    let attach_info = if let Some(leader_client_id) = follow_target_id {
        let context_id = resolve_follow_target_context(&mut client, leader_client_id)
            .await
            .map_err(map_attach_client_error)?;
        open_attach_for_context(&mut client, context_id)
            .await
            .map_err(map_attach_client_error)?
    } else {
        let target = target.expect("target is present when not follow");
        let grant = client
            .attach_grant(parse_session_selector(target))
            .await
            .map_err(map_attach_client_error)?;
        client
            .open_attach_stream_info(&grant)
            .await
            .map_err(map_attach_client_error)?
    };

    if let Some(leader_client_id) = follow_target_id {
        println!(
            "attached to session: {} (following {}{})",
            attach_info.session_id,
            leader_client_id,
            if global { ", global" } else { "" }
        );
    } else {
        println!("attached to session: {}", attach_info.session_id);
    }

    let mut view_state = AttachViewState::new(attach_info);

    update_attach_viewport(&mut client, view_state.attached_id).await?;
    hydrate_attach_state_from_snapshot(&mut client, &mut view_state).await?;
    view_state.set_transient_status(
        initial_attach_status(&attach_keymap, view_state.can_write),
        Instant::now(),
        ATTACH_WELCOME_STATUS_TTL,
    );

    if !view_state.can_write {
        println!("read-only attach: input disabled");
    }
    if let Some(detach_key) = attach_keymap.primary_binding_for_action(&RuntimeAction::Detach) {
        println!("press {detach_key} to detach");
    } else {
        println!("detach is unbound in current keymap");
    }
    client
        .subscribe_events()
        .await
        .map_err(map_attach_client_error)?;
    let _ = client
        .poll_events(256)
        .await
        .map_err(map_attach_client_error)?;

    let raw_mode_guard = RawModeGuard::enable(attach_config.behavior.kitty_keyboard)
        .context("failed to enable raw mode for attach")?;
    let mut attach_input_processor =
        InputProcessor::new(attach_keymap.clone(), raw_mode_guard.keyboard_enhanced);
    let mut exit_reason = AttachExitReason::Detached;

    loop {
        let server_events = client
            .poll_events(16)
            .await
            .map_err(map_attach_client_error)?;
        let terminal_event = poll_attach_terminal_event(ATTACH_IO_POLL_INTERVAL).await?;
        let loop_events = collect_attach_loop_events(server_events, terminal_event);
        let mut should_break = false;
        for loop_event in loop_events {
            if let AttachLoopEvent::Terminal(Event::Resize(cols, rows)) = loop_event
                && let Some(capture) = display_capture.as_mut()
            {
                let _ = capture.record_resize(cols, rows);
            }
            match handle_attach_loop_event(
                loop_event,
                &mut client,
                &mut attach_input_processor,
                follow_target_id,
                Some(self_client_id),
                global,
                &attach_help_lines,
                &mut view_state,
            )
            .await?
            {
                AttachLoopControl::Continue => {}
                AttachLoopControl::Break(reason) => {
                    exit_reason = reason;
                    should_break = true;
                    break;
                }
            }
        }

        if should_break {
            break;
        }

        let _ = view_state.clear_expired_transient_status(Instant::now());
        if let Err(error) =
            refresh_attached_session_from_context(&mut client, &mut view_state).await
        {
            view_state.set_transient_status(
                format!(
                    "context refresh delayed: {}",
                    map_attach_client_error(error)
                ),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }

        let mut frame_needs_render = view_state.dirty.status_needs_redraw
            || view_state.dirty.full_pane_redraw
            || !view_state.dirty.pane_dirty_ids.is_empty();

        if view_state.dirty.layout_needs_refresh || view_state.cached_layout_state.is_none() {
            let previous_layout = view_state.cached_layout_state.clone();
            let layout_state = match client.attach_layout(view_state.attached_id).await {
                Ok(state) => state,
                Err(error) if is_attach_stream_closed_error(&error) => {
                    exit_reason = AttachExitReason::StreamClosed;
                    break;
                }
                Err(error) => return Err(map_attach_client_error(error)),
            };
            if view_state.cached_layout_state.as_ref() != Some(&layout_state) {
                frame_needs_render = true;
                let pane_ids = visible_scene_pane_ids(&layout_state.scene);
                for pane_id in pane_ids {
                    view_state.dirty.pane_dirty_ids.insert(pane_id);
                }
                match previous_layout {
                    None => {
                        view_state.dirty.full_pane_redraw = true;
                    }
                    Some(previous) => {
                        if previous.scene != layout_state.scene {
                            view_state.dirty.full_pane_redraw = true;
                        } else if previous.focused_pane_id != layout_state.focused_pane_id {
                            view_state
                                .dirty
                                .pane_dirty_ids
                                .insert(previous.focused_pane_id);
                            view_state
                                .dirty
                                .pane_dirty_ids
                                .insert(layout_state.focused_pane_id);
                        }
                    }
                }
                view_state.cached_layout_state = Some(layout_state);
            }
            view_state.dirty.layout_needs_refresh = false;
        }

        let Some(layout_state) = view_state.cached_layout_state.clone() else {
            continue;
        };

        resize_attach_parsers_for_scene(&mut view_state.pane_buffers, &layout_state.scene);

        let pane_ids = visible_scene_pane_ids(&layout_state.scene);
        view_state
            .pane_buffers
            .retain(|pane_id, _| pane_ids.iter().any(|id| id == pane_id));

        let chunks = match client
            .attach_pane_output_batch(view_state.attached_id, pane_ids.clone(), 8 * 1024)
            .await
        {
            Ok(chunks) => chunks,
            Err(error) if is_attach_stream_closed_error(&error) => {
                exit_reason = AttachExitReason::StreamClosed;
                break;
            }
            Err(error) => return Err(map_attach_client_error(error)),
        };

        for chunk in chunks {
            if chunk.data.is_empty() {
                continue;
            }
            let buffer = view_state.pane_buffers.entry(chunk.pane_id).or_default();
            append_pane_output(buffer, &chunk.data);
            view_state.dirty.pane_dirty_ids.insert(chunk.pane_id);
            frame_needs_render = true;
        }

        if !frame_needs_render {
            continue;
        }

        let help_scroll = view_state.help_overlay_scroll;
        render_attach_frame(
            &mut client,
            &mut view_state,
            &layout_state,
            follow_target_id,
            global,
            &attach_keymap,
            &attach_help_lines,
            help_scroll,
            display_capture.as_mut(),
        )
        .await?;
    }

    drop(raw_mode_guard);
    restore_terminal_after_attach_ui()?;

    let _ = client.detach().await;
    if follow_target_id.is_some() {
        let _ = client.unfollow().await;
    }
    if let Some(message) = attach_exit_message(exit_reason) {
        println!("{message}");
    }
    if let Some(capture) = display_capture.as_mut() {
        let _ = capture.record_stream_closed();
        let _ = capture.flush();
    }
    Ok(0)
}

async fn handle_attach_runtime_action(
    client: &mut BmuxClient,
    action: RuntimeAction,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    match action {
        RuntimeAction::NewWindow | RuntimeAction::NewSession => {
            let context = client
                .create_context(None, std::collections::BTreeMap::new())
                .await?;
            let attach_info = open_attach_for_context(client, context.id).await?;
            view_state.attached_id = attach_info.session_id;
            view_state.attached_context_id = attach_info.context_id.or(Some(context.id));
            view_state.can_write = attach_info.can_write;
            update_attach_viewport(client, view_state.attached_id).await?;
            hydrate_attach_state_from_snapshot(client, view_state).await?;
            let status = attach_context_status(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
            )
            .await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_WELCOME_STATUS_TTL,
            );
            if !view_state.can_write {
                println!("read-only attach: input disabled");
            }
        }
        _ => {}
    }

    Ok(())
}

async fn apply_plugin_command_outcome(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    outcome: PluginCommandOutcome,
) -> std::result::Result<bool, ClientError> {
    let mut applied = false;
    trace!(
        effect_count = outcome.effects.len(),
        attached_context_id = ?view_state.attached_context_id,
        attached_session_id = %view_state.attached_id,
        "attach.plugin_outcome.received"
    );
    for effect in outcome.effects {
        match effect {
            PluginCommandEffect::SelectContext { context_id } => {
                debug!(
                    target_context_id = %context_id,
                    attached_context_id = ?view_state.attached_context_id,
                    attached_session_id = %view_state.attached_id,
                    "attach.plugin_outcome.select_context"
                );
                retarget_attach_to_context(client, view_state, context_id).await?;
                applied = true;
            }
        }
    }
    Ok(applied)
}

async fn retarget_attach_to_context(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    context_id: Uuid,
) -> std::result::Result<(), ClientError> {
    let started_at = Instant::now();
    debug!(
        from_context_id = ?view_state.attached_context_id,
        from_session_id = %view_state.attached_id,
        to_context_id = %context_id,
        "attach.retarget.start"
    );
    let _ = client
        .select_context(ContextSelector::ById(context_id))
        .await?;
    let attach_info = open_attach_for_context(client, context_id).await?;
    view_state.attached_id = attach_info.session_id;
    view_state.attached_context_id = attach_info.context_id.or(Some(context_id));
    view_state.can_write = attach_info.can_write;
    update_attach_viewport(client, view_state.attached_id).await?;
    hydrate_attach_state_from_snapshot(client, view_state).await?;
    view_state.ui_mode = AttachUiMode::Normal;
    let status = attach_context_status(
        client,
        view_state.attached_context_id,
        view_state.attached_id,
    )
    .await?;
    set_attach_context_status(
        view_state,
        status,
        Instant::now(),
        ATTACH_TRANSIENT_STATUS_TTL,
    );
    debug!(
        to_context_id = ?view_state.attached_context_id,
        to_session_id = %view_state.attached_id,
        can_write = view_state.can_write,
        elapsed_ms = started_at.elapsed().as_millis(),
        "attach.retarget.done"
    );
    Ok(())
}

fn plugin_fallback_retarget_context_id(
    before_context_id: Option<Uuid>,
    after_context_id: Option<Uuid>,
    attached_context_id: Option<Uuid>,
    outcome_applied: bool,
) -> Option<Uuid> {
    if outcome_applied {
        return None;
    }
    after_context_id
        .filter(|after| Some(*after) != before_context_id && Some(*after) != attached_context_id)
}

fn plugin_fallback_new_context_id(
    before_context_ids: Option<&std::collections::BTreeSet<Uuid>>,
    after_context_ids: Option<&std::collections::BTreeSet<Uuid>>,
    attached_context_id: Option<Uuid>,
    after_context_id: Option<Uuid>,
    outcome_applied: bool,
) -> Option<Uuid> {
    if outcome_applied {
        return None;
    }
    let (Some(before), Some(after)) = (before_context_ids, after_context_ids) else {
        return None;
    };

    let mut new_context_ids = after
        .difference(before)
        .copied()
        .filter(|context_id| Some(*context_id) != attached_context_id)
        .collect::<Vec<_>>();

    if new_context_ids.is_empty() {
        return None;
    }
    if new_context_ids.len() == 1 {
        return new_context_ids.pop();
    }

    after_context_id.filter(|context_id| new_context_ids.contains(context_id))
}

async fn handle_attach_ui_action(
    client: &mut BmuxClient,
    action: RuntimeAction,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    match action {
        RuntimeAction::EnterWindowMode => {
            view_state.set_transient_status(
                "workspace mode unavailable in core baseline",
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::ExitMode => {
            let _ = view_state;
        }
        RuntimeAction::EnterScrollMode => {
            if enter_attach_scrollback(view_state) {
            } else {
                view_state.set_transient_status(
                    ATTACH_SCROLLBACK_UNAVAILABLE_STATUS,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        RuntimeAction::ExitScrollMode => {
            if view_state.selection_active() {
                clear_attach_selection(view_state, true);
            } else {
                view_state.exit_scrollback();
            }
        }
        RuntimeAction::ScrollUpLine => {
            step_attach_scrollback(view_state, -1);
        }
        RuntimeAction::ScrollDownLine => {
            step_attach_scrollback(view_state, 1);
        }
        RuntimeAction::ScrollUpPage => {
            step_attach_scrollback(
                view_state,
                -(attach_scrollback_page_size(view_state) as isize),
            );
        }
        RuntimeAction::ScrollDownPage => {
            step_attach_scrollback(view_state, attach_scrollback_page_size(view_state) as isize);
        }
        RuntimeAction::ScrollTop => {
            if view_state.scrollback_active {
                view_state.scrollback_offset = max_attach_scrollback(view_state);
                clamp_attach_scrollback_cursor(view_state);
            }
        }
        RuntimeAction::ScrollBottom => {
            if view_state.scrollback_active {
                view_state.scrollback_offset = 0;
                clamp_attach_scrollback_cursor(view_state);
            }
        }
        RuntimeAction::MoveCursorLeft => {
            move_attach_scrollback_cursor_horizontal(view_state, -1);
        }
        RuntimeAction::MoveCursorRight => {
            move_attach_scrollback_cursor_horizontal(view_state, 1);
        }
        RuntimeAction::MoveCursorUp => {
            move_attach_scrollback_cursor_vertical(view_state, -1);
        }
        RuntimeAction::MoveCursorDown => {
            move_attach_scrollback_cursor_vertical(view_state, 1);
        }
        RuntimeAction::BeginSelection => {
            if begin_attach_selection(view_state) {
                view_state.set_transient_status(
                    ATTACH_SELECTION_STARTED_STATUS,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        RuntimeAction::CopyScrollback => {
            copy_attach_selection(view_state, false);
        }
        RuntimeAction::ConfirmScrollback => {
            confirm_attach_scrollback(view_state);
        }
        RuntimeAction::SessionPrev => {
            view_state.exit_scrollback();
            switch_attach_session_relative(client, view_state, -1).await?;
            let status = attach_context_status(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
            )
            .await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::SessionNext => {
            view_state.exit_scrollback();
            switch_attach_session_relative(client, view_state, 1).await?;
            let status = attach_context_status(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
            )
            .await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowPrev => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowNext => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto1 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto2 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto3 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto4 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto5 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto6 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto7 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto8 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto9 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowClose => {
            view_state.exit_scrollback();
        }
        RuntimeAction::SplitFocusedVertical => {
            let selector = attached_session_selector(client, view_state).await?;
            let _ = client
                .split_pane(Some(selector), PaneSplitDirection::Vertical)
                .await?;
        }
        RuntimeAction::SplitFocusedHorizontal => {
            let selector = attached_session_selector(client, view_state).await?;
            let _ = client
                .split_pane(Some(selector), PaneSplitDirection::Horizontal)
                .await?;
        }
        RuntimeAction::FocusNext
        | RuntimeAction::FocusLeft
        | RuntimeAction::FocusRight
        | RuntimeAction::FocusUp
        | RuntimeAction::FocusDown => {
            let direction = if matches!(action, RuntimeAction::FocusLeft | RuntimeAction::FocusUp) {
                PaneFocusDirection::Prev
            } else {
                PaneFocusDirection::Next
            };
            let selector = attached_session_selector(client, view_state).await?;
            let _ = client.focus_pane(Some(selector), direction).await?;
        }
        RuntimeAction::IncreaseSplit
        | RuntimeAction::DecreaseSplit
        | RuntimeAction::ResizeLeft
        | RuntimeAction::ResizeRight
        | RuntimeAction::ResizeUp
        | RuntimeAction::ResizeDown => {
            let delta = if matches!(
                action,
                RuntimeAction::IncreaseSplit
                    | RuntimeAction::ResizeRight
                    | RuntimeAction::ResizeDown
            ) {
                1
            } else {
                -1
            };
            let selector = attached_session_selector(client, view_state).await?;
            client.resize_pane(Some(selector), delta).await?;
        }
        RuntimeAction::CloseFocusedPane => {
            let selector = attached_session_selector(client, view_state).await?;
            client.close_pane(Some(selector)).await?;
        }
        RuntimeAction::NewWindow | RuntimeAction::NewSession => {
            handle_attach_runtime_action(client, action, view_state).await?;
        }
        _ => {}
    }

    Ok(())
}

fn enter_attach_scrollback(view_state: &mut AttachViewState) -> bool {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return false;
    };
    let Some(buffer) = focused_attach_pane_buffer(view_state) else {
        return false;
    };
    let (row, col) = buffer.parser.screen().cursor_position();
    view_state.scrollback_active = true;
    view_state.scrollback_offset = 0;
    view_state.scrollback_cursor = Some(AttachScrollbackCursor {
        row: usize::from(row).min(inner_h.saturating_sub(1)),
        col: usize::from(col).min(inner_w.saturating_sub(1)),
    });
    view_state.selection_anchor = None;
    true
}

fn begin_attach_selection(view_state: &mut AttachViewState) -> bool {
    if !view_state.scrollback_active {
        return false;
    }
    view_state.selection_anchor = attach_scrollback_cursor_absolute_position(view_state);
    view_state.selection_anchor.is_some()
}

fn clear_attach_selection(view_state: &mut AttachViewState, show_status: bool) {
    view_state.selection_anchor = None;
    if show_status {
        view_state.set_transient_status(
            ATTACH_SELECTION_CLEARED_STATUS,
            Instant::now(),
            ATTACH_TRANSIENT_STATUS_TTL,
        );
    }
}

fn attach_scrollback_cursor_absolute_position(
    view_state: &AttachViewState,
) -> Option<AttachScrollbackPosition> {
    let cursor = view_state.scrollback_cursor?;
    Some(AttachScrollbackPosition {
        row: view_state.scrollback_offset.saturating_add(cursor.row),
        col: cursor.col,
    })
}

fn attach_selection_bounds(
    view_state: &AttachViewState,
) -> Option<(AttachScrollbackPosition, AttachScrollbackPosition)> {
    let anchor = view_state.selection_anchor?;
    let head = attach_scrollback_cursor_absolute_position(view_state)?;
    Some(if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    })
}

fn step_attach_scrollback(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active {
        return;
    }
    let max_offset = max_attach_scrollback(view_state);
    view_state.scrollback_offset =
        adjust_attach_scrollback_offset(view_state.scrollback_offset, delta, max_offset);
    clamp_attach_scrollback_cursor(view_state);
}

fn move_attach_scrollback_cursor_horizontal(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active {
        return;
    }
    let Some((inner_w, _)) = focused_attach_pane_inner_size(view_state) else {
        return;
    };
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };
    cursor.col = adjust_scrollback_cursor_component(cursor.col, delta, inner_w.saturating_sub(1));
}

fn move_attach_scrollback_cursor_vertical(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active || delta == 0 {
        return;
    }
    let Some((_, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return;
    };
    let max_offset = max_attach_scrollback(view_state);
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };

    if delta < 0 {
        for _ in 0..delta.unsigned_abs() {
            if cursor.row > 0 {
                cursor.row -= 1;
            } else if view_state.scrollback_offset < max_offset {
                view_state.scrollback_offset += 1;
            }
        }
    } else {
        for _ in 0..(delta as usize) {
            if cursor.row + 1 < inner_h {
                cursor.row += 1;
            } else if view_state.scrollback_offset > 0 {
                view_state.scrollback_offset -= 1;
            }
        }
    }

    clamp_attach_scrollback_cursor(view_state);
}

fn adjust_scrollback_cursor_component(current: usize, delta: isize, max_value: usize) -> usize {
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(max_value)
    }
}

fn copy_attach_selection(view_state: &mut AttachViewState, exit_after_copy: bool) {
    let Some(text) = selected_attach_text(view_state) else {
        if exit_after_copy {
            view_state.exit_scrollback();
        } else {
            view_state.set_transient_status(
                ATTACH_SELECTION_EMPTY_STATUS,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        return;
    };

    match copy_text_with_clipboard_plugin(&text) {
        Ok(()) => {
            view_state.set_transient_status(
                ATTACH_SELECTION_COPIED_STATUS,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            if exit_after_copy {
                view_state.exit_scrollback();
            }
        }
        Err(error) => {
            view_state.set_transient_status(
                format_clipboard_service_error(&error),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
    }
}

fn confirm_attach_scrollback(view_state: &mut AttachViewState) {
    copy_attach_selection(view_state, true);
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ClipboardWriteRequest {
    text: String,
}

fn copy_text_with_clipboard_plugin(text: &str) -> Result<()> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let services = available_service_descriptors(&config, &registry)?;
    let capability = HostScope::new("bmux.clipboard.write")?;
    let service = services
        .into_iter()
        .find(|entry| {
            entry.capability == capability
                && entry.kind == ServiceKind::Command
                && entry.interface_id == "clipboard-write/v1"
        })
        .context("clipboard service unavailable; ensure a provider is enabled and discoverable")?;

    let provider_plugin_id = match &service.provider {
        bmux_plugin::ProviderId::Plugin(plugin_id) => plugin_id,
        bmux_plugin::ProviderId::Host => {
            anyhow::bail!("clipboard service provider must be plugin-owned")
        }
    };
    let provider = registry.get(provider_plugin_id).with_context(|| {
        format!("clipboard service provider '{provider_plugin_id}' was not found")
    })?;

    let payload = bmux_plugin::encode_service_message(&ClipboardWriteRequest {
        text: text.to_string(),
    })?;
    let enabled_plugins = effective_enabled_plugins(&config, &registry);
    let available_capabilities = available_capability_providers(&config, &registry)?
        .into_keys()
        .map(|entry| entry.to_string())
        .collect::<Vec<_>>();
    let plugin_search_roots = resolve_plugin_search_paths(&config, &paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let loaded = load_plugin(
        provider,
        &plugin_host_metadata(),
        &available_capability_providers(&config, &registry)?,
    )
    .with_context(|| format!("failed loading clipboard service provider '{provider_plugin_id}'"))?;

    let connection = bmux_plugin::HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let _host_kernel_connection_guard = enter_host_kernel_connection(connection.clone());
    let response = loaded.invoke_service(&bmux_plugin::NativeServiceContext {
        plugin_id: provider_plugin_id.clone(),
        request: ServiceRequest {
            caller_plugin_id: "bmux.core".to_string(),
            service,
            operation: "copy_text".to_string(),
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
        services: available_service_descriptors(&config, &registry)?,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        host: plugin_host_metadata(),
        connection,
        settings: std::collections::BTreeMap::new(),
        plugin_settings_map: std::collections::BTreeMap::new(),
        host_kernel_bridge: Some(bmux_plugin::HostKernelBridge::from_fn(host_kernel_bridge)),
    })?;
    if let Some(error) = response.error {
        anyhow::bail!(error.message);
    }

    let _: () = bmux_plugin::decode_service_message(&response.payload)
        .context("failed decoding clipboard service response payload")?;
    Ok(())
}

fn format_clipboard_service_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("clipboard backend unavailable") {
        return "clipboard backend unavailable".to_string();
    }
    if message.starts_with("clipboard copy failed:") {
        return message;
    }
    format!("clipboard copy failed: {message}")
}

fn selected_attach_text(view_state: &mut AttachViewState) -> Option<String> {
    let (start, end) = attach_selection_bounds(view_state)?;
    extract_attach_text(view_state, start, end)
}

fn extract_attach_text(
    view_state: &mut AttachViewState,
    start: AttachScrollbackPosition,
    end: AttachScrollbackPosition,
) -> Option<String> {
    let buffer = focused_attach_pane_buffer(view_state)?;
    let original_scrollback = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(start.row);
    let text = buffer.parser.screen().contents_between(
        0,
        start.col as u16,
        end.row.saturating_sub(start.row) as u16,
        end.col.saturating_add(1) as u16,
    );
    buffer
        .parser
        .screen_mut()
        .set_scrollback(original_scrollback);
    Some(text)
}

fn adjust_attach_scrollback_offset(current: usize, delta: isize, max_offset: usize) -> usize {
    if delta < 0 {
        current.saturating_add(delta.unsigned_abs()).min(max_offset)
    } else {
        current.saturating_sub(delta as usize)
    }
}

fn max_attach_scrollback(view_state: &mut AttachViewState) -> usize {
    let Some(buffer) = focused_attach_pane_buffer(view_state) else {
        return 0;
    };
    let previous = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(usize::MAX);
    let max_offset = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(previous);
    max_offset
}

fn clamp_attach_scrollback_cursor(view_state: &mut AttachViewState) {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        view_state.scrollback_cursor = None;
        return;
    };
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };
    cursor.row = cursor.row.min(inner_h.saturating_sub(1));
    cursor.col = cursor.col.min(inner_w.saturating_sub(1));
}

fn attach_scrollback_page_size(view_state: &AttachViewState) -> usize {
    focused_attach_pane_inner_size(view_state).map_or(10, |(_, inner_h)| inner_h)
}

fn focused_attach_pane_buffer(
    view_state: &mut AttachViewState,
) -> Option<&mut attach::state::PaneRenderBuffer> {
    let focused_pane_id = view_state.cached_layout_state.as_ref()?.focused_pane_id;
    view_state.pane_buffers.get_mut(&focused_pane_id)
}

fn focused_attach_pane_inner_size(view_state: &AttachViewState) -> Option<(usize, usize)> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    layout_state
        .scene
        .surfaces
        .iter()
        .find(|surface| surface.visible && surface.pane_id == Some(layout_state.focused_pane_id))
        .map(|surface| {
            (
                usize::from(surface.rect.w.saturating_sub(2).max(1)),
                usize::from(surface.rect.h.saturating_sub(2).max(1)),
            )
        })
}

async fn switch_attach_session_relative(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    step: isize,
) -> std::result::Result<(), ClientError> {
    if let Some(current_context_id) = view_state.attached_context_id {
        let contexts = client.list_contexts().await?;
        if let Some(target_context_id) = relative_context_id(&contexts, current_context_id, step) {
            let _ = client
                .select_context(ContextSelector::ById(target_context_id))
                .await?;
            let attach_info = open_attach_for_context(client, target_context_id).await?;
            view_state.attached_id = attach_info.session_id;
            view_state.attached_context_id = attach_info.context_id.or(Some(target_context_id));
            view_state.can_write = attach_info.can_write;
            update_attach_viewport(client, view_state.attached_id).await?;
            hydrate_attach_state_from_snapshot(client, view_state).await?;
            return Ok(());
        }
    }

    let sessions = client.list_sessions().await?;
    let Some(target_session_id) = relative_session_id(&sessions, view_state.attached_id, step)
    else {
        return Ok(());
    };

    let attach_info = open_attach_for_session(client, target_session_id).await?;
    view_state.attached_id = attach_info.session_id;
    view_state.attached_context_id = attach_info.context_id;
    view_state.can_write = attach_info.can_write;
    update_attach_viewport(client, view_state.attached_id).await?;
    hydrate_attach_state_from_snapshot(client, view_state).await?;
    Ok(())
}

fn relative_session_id(
    sessions: &[SessionSummary],
    current_session_id: Uuid,
    step: isize,
) -> Option<Uuid> {
    if sessions.is_empty() {
        return None;
    }

    let current_index = sessions
        .iter()
        .position(|session| session.id == current_session_id)
        .unwrap_or(0);
    let len = sessions.len() as isize;
    let mut target_index = current_index as isize + step;
    while target_index < 0 {
        target_index += len;
    }
    target_index %= len;
    sessions
        .get(target_index as usize)
        .map(|session| session.id)
}

fn relative_context_id(
    contexts: &[ContextSummary],
    current_context_id: Uuid,
    step: isize,
) -> Option<Uuid> {
    if contexts.is_empty() {
        return None;
    }

    let current_index = contexts
        .iter()
        .position(|context| context.id == current_context_id)
        .unwrap_or(0);
    let len = contexts.len() as isize;
    let mut target_index = current_index as isize + step;
    while target_index < 0 {
        target_index += len;
    }
    target_index %= len;
    contexts
        .get(target_index as usize)
        .map(|context| context.id)
}

async fn build_attach_status_line_for_draw(
    client: &mut BmuxClient,
    context_id: Option<Uuid>,
    session_id: Uuid,
    can_write: bool,
    ui_mode: AttachUiMode,
    scrollback_active: bool,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    quit_confirmation_pending: bool,
    help_overlay_open: bool,
    transient_status: Option<&str>,
    keymap: &Keymap,
) -> std::result::Result<String, ClientError> {
    let (cols, _) = terminal::size().unwrap_or((0, 0));
    if cols == 0 {
        return Ok(String::new());
    }

    let tabs = build_attach_tabs(client, context_id, session_id).await?;
    let session_label = resolve_attach_session_label(client, session_id).await?;
    let current_context_label =
        resolve_attach_context_label(client, context_id, session_id).await?;
    let mode_label = if help_overlay_open {
        "HELP"
    } else if scrollback_active {
        "SCROLL"
    } else {
        let _ = ui_mode;
        "NORMAL"
    };
    let role_label = if can_write { "write" } else { "read-only" };
    let follow_label = follow_target_id.map(|id| {
        if follow_global {
            format!("following {} (global)", short_uuid(id))
        } else {
            format!("following {}", short_uuid(id))
        }
    });
    let hint = if quit_confirmation_pending {
        "Quit session and all panes? [y/N]".to_string()
    } else if help_overlay_open {
        "Help overlay open | ? toggles | Esc/Enter close".to_string()
    } else if let Some(status) = transient_status {
        status.to_string()
    } else if scrollback_active {
        attach_scrollback_hint(keymap)
    } else {
        attach_mode_hint(ui_mode, keymap)
    };

    let status_line = build_attach_status_line(
        &session_label,
        &current_context_label,
        &tabs,
        mode_label,
        role_label,
        follow_label.as_deref(),
        &hint,
    );

    Ok(format_status_line_for_width(&status_line, cols))
}

fn format_status_line_for_width(status_line: &str, cols: u16) -> String {
    let width = usize::from(cols);
    let mut rendered = status_line.to_string();
    if rendered.len() > width {
        rendered.truncate(width);
    } else {
        rendered.push_str(&" ".repeat(width - rendered.len()));
    }
    rendered
}

fn attach_mode_hint(_ui_mode: AttachUiMode, keymap: &Keymap) -> String {
    let detach = key_hint_or_unbound(keymap, RuntimeAction::Detach);
    let quit = key_hint_or_unbound(keymap, RuntimeAction::Quit);
    let help = key_hint_or_unbound(keymap, RuntimeAction::ShowHelp);
    format!("{detach} detach | {quit} quit | {help} help")
}

fn initial_attach_status(keymap: &Keymap, can_write: bool) -> String {
    let help = key_hint_or_unbound(keymap, RuntimeAction::ShowHelp);
    if can_write {
        format!("{help} help | typing goes to pane")
    } else {
        format!("read-only attach | {help} help")
    }
}

const fn attach_exit_message(reason: AttachExitReason) -> Option<&'static str> {
    match reason {
        AttachExitReason::Detached | AttachExitReason::Quit => None,
        AttachExitReason::StreamClosed => Some("attach ended unexpectedly: server stream closed"),
    }
}

fn attach_scrollback_hint(keymap: &Keymap) -> String {
    let exit = scroll_key_hint_or_unbound(keymap, RuntimeAction::ExitScrollMode);
    let confirm = scroll_key_hint_or_unbound(keymap, RuntimeAction::ConfirmScrollback);
    let left = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorLeft);
    let right = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorRight);
    let up = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorUp);
    let down = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorDown);
    let page_up = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollUpPage);
    let page_down = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollDownPage);
    let top = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollTop);
    let bottom = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollBottom);
    let select = scroll_key_hint_or_unbound(keymap, RuntimeAction::BeginSelection);
    let copy = scroll_key_hint_or_unbound(keymap, RuntimeAction::CopyScrollback);
    format!(
        "{up}/{down} line | {left}/{right} col | {page_up}/{page_down} page | {top}/{bottom} top/bottom | {select} select | {copy} copy | {confirm} copy+exit | {exit} cancel/exit scroll"
    )
}

fn scroll_key_hint_or_unbound(keymap: &Keymap, action: RuntimeAction) -> String {
    keymap
        .primary_scroll_binding_for_action(&action)
        .unwrap_or_else(|| "unbound".to_string())
}

fn key_hint_or_unbound(keymap: &Keymap, action: RuntimeAction) -> String {
    keymap
        .primary_binding_for_action(&action)
        .unwrap_or_else(|| "unbound".to_string())
}

fn queue_attach_status_line(stdout: &mut impl Write, status_line: &str) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    let rendered = format_status_line_for_width(status_line, cols);
    queue!(
        stdout,
        MoveTo(0, 0),
        Print("\x1b[7m"),
        Print(rendered),
        Print("\x1b[0m")
    )
    .context("failed queuing attach status line")
}

fn help_overlay_visible_rows(lines: &[String]) -> usize {
    let (_cols, rows) = terminal::size().unwrap_or((0, 0));
    let max_content_rows = (rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((rows as usize).saturating_sub(2));
    height.saturating_sub(4).max(1)
}

fn adjust_help_overlay_scroll(
    current: usize,
    delta: isize,
    total_lines: usize,
    visible_rows: usize,
) -> usize {
    if total_lines == 0 {
        return 0;
    }
    let max_scroll = total_lines.saturating_sub(visible_rows.max(1));
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize)
    };
    next.min(max_scroll)
}

const fn help_overlay_accepts_key_kind(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn handle_help_overlay_key_event(
    key: &KeyEvent,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> bool {
    if !help_overlay_accepts_key_kind(key.kind) {
        return false;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            view_state.help_overlay_open = false;
            view_state.help_overlay_scroll = 0;
            view_state.dirty.status_needs_redraw = true;
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::PageUp => {
            let page = help_overlay_visible_rows(help_lines) as isize;
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::PageDown => {
            let page = help_overlay_visible_rows(help_lines) as isize;
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Home => {
            view_state.help_overlay_scroll = 0;
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::End => {
            let visible = help_overlay_visible_rows(help_lines);
            view_state.help_overlay_scroll = help_lines.len().saturating_sub(visible);
            view_state.dirty.full_pane_redraw = true;
            true
        }
        _ => false,
    }
}

fn help_overlay_surface(lines: &[String]) -> Option<bmux_ipc::AttachSurface> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols < 20 || rows < 6 {
        return None;
    }

    let content_width = lines
        .iter()
        .map(std::string::String::len)
        .max()
        .unwrap_or(0)
        .min(80);
    let width = (content_width + 4)
        .max(36)
        .min((cols as usize).saturating_sub(2));
    let max_content_rows = (rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((rows as usize).saturating_sub(2));
    let x = ((cols as usize).saturating_sub(width)) / 2;
    let y = ((rows as usize).saturating_sub(height)) / 2;

    Some(bmux_ipc::AttachSurface {
        id: HELP_OVERLAY_SURFACE_ID,
        kind: bmux_ipc::AttachSurfaceKind::Overlay,
        layer: bmux_ipc::AttachLayer::Overlay,
        z: i32::MAX,
        rect: bmux_ipc::AttachRect {
            x: x as u16,
            y: y as u16,
            w: width as u16,
            h: height as u16,
        },
        opaque: true,
        visible: true,
        accepts_input: true,
        cursor_owner: false,
        pane_id: None,
    })
}

fn queue_attach_help_overlay(
    stdout: &mut impl Write,
    surface_meta: &bmux_ipc::AttachSurface,
    lines: &[String],
    scroll: usize,
) -> Result<()> {
    let width = usize::from(surface_meta.rect.w);
    let height = usize::from(surface_meta.rect.h);
    let x = usize::from(surface_meta.rect.x);
    let y = usize::from(surface_meta.rect.y);
    let body_rows = height.saturating_sub(4).max(1);
    let surface = AttachLayerSurface::new(
        PaneRect {
            x: surface_meta.rect.x,
            y: surface_meta.rect.y,
            w: surface_meta.rect.w,
            h: surface_meta.rect.h,
        },
        AttachLayer::Overlay,
        true,
    );
    let text_width = width.saturating_sub(4);

    let top = format!("+{}+", "-".repeat(width.saturating_sub(2)));
    queue!(stdout, MoveTo(x as u16, y as u16), Print(&top))
        .context("failed drawing help overlay top")?;

    let title = " bmux help ";
    let title_x = x + ((width.saturating_sub(title.len())) / 2);
    queue!(stdout, MoveTo(title_x as u16, y as u16), Print(title))
        .context("failed drawing help overlay title")?;

    for row in 1..height.saturating_sub(1) {
        let y_row = (y + row) as u16;
        queue!(
            stdout,
            MoveTo(x as u16, y_row),
            Print("|"),
            MoveTo((x + width - 1) as u16, y_row),
            Print("|")
        )
        .context("failed drawing help overlay border")?;
    }

    queue_layer_fill(stdout, surface).context("failed filling help overlay body")?;

    queue!(
        stdout,
        MoveTo(x as u16, (y + height - 1) as u16),
        Print(&top)
    )
    .context("failed drawing help overlay bottom")?;

    let header = "scope    chord                action";
    let header_rendered = opaque_row_text(header, text_width);
    queue!(
        stdout,
        MoveTo((x + 2) as u16, (y + 1) as u16),
        Print(header_rendered)
    )
    .context("failed drawing help overlay header")?;

    let start = scroll.min(lines.len().saturating_sub(body_rows));
    let end = (start + body_rows).min(lines.len());
    for (idx, line) in lines.iter().skip(start).take(body_rows).enumerate() {
        let rendered = opaque_row_text(line, text_width);
        let row = y + 2 + idx;
        if row >= y + height - 1 {
            break;
        }
        queue!(stdout, MoveTo((x + 2) as u16, row as u16), Print(rendered))
            .context("failed drawing help overlay entry")?;
    }

    let footer = format!(
        "j/k or ↑/↓ scroll | PgUp/PgDn | Esc close | {}-{} / {}",
        if lines.is_empty() { 0 } else { start + 1 },
        end,
        lines.len()
    );
    let footer_rendered = opaque_row_text(&footer, text_width);
    queue!(
        stdout,
        MoveTo((x + 2) as u16, (y + height - 2) as u16),
        Print(footer_rendered)
    )
    .context("failed drawing help overlay footer")?;

    Ok(())
}

async fn render_attach_frame(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    layout_state: &AttachLayoutState,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    keymap: &crate::input::Keymap,
    help_lines: &[String],
    help_scroll: usize,
    display_capture: Option<&mut recording::DisplayCaptureWriter>,
) -> Result<()> {
    if view_state.dirty.status_needs_redraw {
        let now = Instant::now();
        view_state.cached_status_line = Some(
            build_attach_status_line_for_draw(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
                view_state.can_write,
                view_state.ui_mode,
                view_state.scrollback_active,
                follow_target_id,
                follow_global,
                view_state.quit_confirmation_pending,
                view_state.help_overlay_open,
                view_state.transient_status_text(now),
                keymap,
            )
            .await
            .map_err(map_attach_client_error)?,
        );
        view_state.dirty.status_needs_redraw = false;
    }

    let mut frame_bytes = Vec::new();
    queue!(frame_bytes, SavePosition).context("failed queuing cursor save for attach frame")?;
    if let Some(status_line) = view_state.cached_status_line.as_deref() {
        queue_attach_status_line(&mut frame_bytes, status_line)?;
    }
    let cursor_state = render_attach_scene(
        &mut frame_bytes,
        &layout_state.scene,
        &mut view_state.pane_buffers,
        &view_state.dirty.pane_dirty_ids,
        view_state.dirty.full_pane_redraw,
        view_state.scrollback_active,
        view_state.scrollback_offset,
        view_state.scrollback_cursor,
        view_state.selection_anchor,
    )?;
    if view_state.help_overlay_open {
        if let Some(help_surface) = help_overlay_surface(help_lines) {
            queue_attach_help_overlay(&mut frame_bytes, &help_surface, help_lines, help_scroll)?;
        }
        apply_attach_cursor_state(&mut frame_bytes, None, &mut view_state.last_cursor_state)?;
    } else {
        apply_attach_cursor_state(
            &mut frame_bytes,
            cursor_state,
            &mut view_state.last_cursor_state,
        )?;
    }

    if let Some(capture) = display_capture {
        let _ = capture.record_frame_bytes(&frame_bytes);
    }

    let mut stdout = io::stdout();
    stdout
        .write_all(&frame_bytes)
        .context("failed writing attach frame")?;
    stdout.flush().context("failed flushing attach frame")?;
    view_state.dirty.full_pane_redraw = false;
    view_state.dirty.pane_dirty_ids.clear();
    Ok(())
}

async fn build_attach_tabs(
    client: &mut BmuxClient,
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> std::result::Result<Vec<AttachTab>, ClientError> {
    let contexts = client.list_contexts().await?;
    if contexts.is_empty() {
        return Ok(vec![AttachTab {
            label: "terminal".to_string(),
            active: true,
        }]);
    }

    let current_context_id = context_id.or_else(|| {
        contexts
            .iter()
            .find(|context| {
                context
                    .attributes
                    .get("bmux.session_id")
                    .is_some_and(|value| value == &session_id.to_string())
            })
            .map(|context| context.id)
    });

    let tabs = contexts
        .into_iter()
        .take(6)
        .map(|context| AttachTab {
            label: context_summary_label(&context),
            active: current_context_id == Some(context.id),
        })
        .collect();
    Ok(tabs)
}

async fn resolve_attach_context_label(
    client: &mut BmuxClient,
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let contexts = client.list_contexts().await?;
    if let Some(context_id) = context_id
        && let Some(context) = contexts.iter().find(|context| context.id == context_id)
    {
        return Ok(context_summary_label(context));
    }

    if let Some(context) = contexts.iter().find(|context| {
        context
            .attributes
            .get("bmux.session_id")
            .is_some_and(|value| value == &session_id.to_string())
    }) {
        return Ok(context_summary_label(context));
    }

    Ok("terminal".to_string())
}

fn context_summary_label(context: &ContextSummary) -> String {
    context
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map_or_else(
            || format!("context-{}", short_uuid(context.id)),
            ToString::to_string,
        )
}

async fn resolve_attach_session_label(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let sessions = client.list_sessions().await?;
    Ok(sessions
        .into_iter()
        .find(|session| session.id == session_id)
        .map_or_else(
            || format!("session-{}", short_uuid(session_id)),
            |session| session_summary_label(&session),
        ))
}

fn session_summary_label(session: &bmux_ipc::SessionSummary) -> String {
    session
        .name
        .clone()
        .unwrap_or_else(|| format!("session-{}", short_uuid(session.id)))
}

async fn attach_context_status(
    client: &mut BmuxClient,
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let session_label = resolve_attach_session_label(client, session_id).await?;
    let context_label = resolve_attach_context_label(client, context_id, session_id).await?;
    Ok(format!(
        "session: {session_label} | context: {context_label}"
    ))
}

fn set_attach_context_status(
    view_state: &mut AttachViewState,
    status: String,
    now: Instant,
    ttl: Duration,
) {
    view_state.set_transient_status(status, now, ttl);
}

fn short_uuid(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

async fn resolve_follow_target_context(
    client: &mut BmuxClient,
    leader_client_id: Uuid,
) -> std::result::Result<Uuid, ClientError> {
    let clients = client.list_clients().await?;
    let leader = clients
        .into_iter()
        .find(|entry| entry.id == leader_client_id)
        .ok_or(ClientError::UnexpectedResponse("follow target not found"))?;

    if let Some(context_id) = leader.selected_context_id {
        return Ok(context_id);
    }

    if let Some(session_id) = leader.selected_session_id {
        let contexts = client.list_contexts().await?;
        if let Some(context) = contexts.into_iter().find(|context| {
            context
                .attributes
                .get("bmux.session_id")
                .is_some_and(|value| value == &session_id.to_string())
        }) {
            return Ok(context.id);
        }
    }

    Err(ClientError::UnexpectedResponse(
        "follow target has no selected context",
    ))
}

async fn open_attach_for_session(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<bmux_client::AttachOpenInfo, ClientError> {
    let grant = client
        .attach_grant(SessionSelector::ById(session_id))
        .await?;
    client.open_attach_stream_info(&grant).await
}

async fn open_attach_for_context(
    client: &mut BmuxClient,
    context_id: Uuid,
) -> std::result::Result<bmux_client::AttachOpenInfo, ClientError> {
    let grant = client
        .attach_context_grant(ContextSelector::ById(context_id))
        .await?;
    client.open_attach_stream_info(&grant).await
}

async fn attached_session_selector(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<SessionSelector, ClientError> {
    refresh_attached_session_from_context(client, view_state).await?;
    Ok(SessionSelector::ById(view_state.attached_id))
}

async fn refresh_attached_session_from_context(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    if let Some(context_id) = view_state.attached_context_id {
        trace!(
            context_id = %context_id,
            current_session_id = %view_state.attached_id,
            "attach.context_refresh.start"
        );
        let started_at = Instant::now();
        let grant = client
            .attach_context_grant(ContextSelector::ById(context_id))
            .await?;
        let previous_session_id = view_state.attached_id;
        view_state.attached_id = grant.session_id;
        view_state.attached_context_id = grant.context_id.or(Some(context_id));
        trace!(
            context_id = ?view_state.attached_context_id,
            previous_session_id = %previous_session_id,
            refreshed_session_id = %view_state.attached_id,
            elapsed_ms = started_at.elapsed().as_millis(),
            "attach.context_refresh.done"
        );
    }
    Ok(())
}

fn attach_keymap_from_config(config: &BmuxConfig) -> crate::input::Keymap {
    let (runtime_bindings, global_bindings, scroll_bindings) = filtered_attach_keybindings(config);
    let timeout_ms = config
        .keybindings
        .resolve_timeout()
        .map(|timeout| timeout.timeout_ms())
        .unwrap_or(None);
    match crate::input::Keymap::from_parts_with_scroll(
        &config.keybindings.prefix,
        timeout_ms,
        &runtime_bindings,
        &global_bindings,
        &scroll_bindings,
    ) {
        Ok(keymap) => keymap,
        Err(error) => {
            eprintln!("bmux warning: invalid attach keymap config, using defaults ({error})");
            default_attach_keymap()
        }
    }
}

fn filtered_attach_keybindings(
    config: &BmuxConfig,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let (runtime, global, scroll) = merged_runtime_keybindings(config);
    let runtime = normalize_attach_keybindings(runtime, "runtime");
    let mut global = normalize_attach_keybindings(global, "global");
    let scroll = normalize_attach_keybindings(scroll, "scroll");

    inject_attach_global_defaults(&mut global);
    (runtime, global, scroll)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachKeybindingScope {
    Runtime,
    Global,
}

impl AttachKeybindingScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachKeybindingEntry {
    scope: AttachKeybindingScope,
    chord: String,
    action: RuntimeAction,
    action_name: String,
}

fn effective_attach_keybindings(config: &BmuxConfig) -> Vec<AttachKeybindingEntry> {
    let (runtime, global, _) = filtered_attach_keybindings(config);
    let mut entries = Vec::new();

    for (chord, action_name) in runtime {
        if let Ok(action) = crate::input::parse_runtime_action_name(&action_name) {
            entries.push(AttachKeybindingEntry {
                scope: AttachKeybindingScope::Runtime,
                chord,
                action,
                action_name,
            });
        }
    }
    for (chord, action_name) in global {
        if let Ok(action) = crate::input::parse_runtime_action_name(&action_name) {
            entries.push(AttachKeybindingEntry {
                scope: AttachKeybindingScope::Global,
                chord,
                action,
                action_name,
            });
        }
    }

    entries.sort_by(|left, right| {
        left.scope
            .as_str()
            .cmp(right.scope.as_str())
            .then_with(|| left.chord.cmp(&right.chord))
    });
    entries
}

fn build_attach_help_lines(config: &BmuxConfig) -> Vec<String> {
    let keymap = attach_keymap_from_config(config);
    let help = key_hint_or_unbound(&keymap, RuntimeAction::ShowHelp);
    let detach = key_hint_or_unbound(&keymap, RuntimeAction::Detach);
    let scroll = key_hint_or_unbound(&keymap, RuntimeAction::EnterScrollMode);
    let mut groups: Vec<(&str, Vec<AttachKeybindingEntry>)> = vec![
        ("Session", Vec::new()),
        ("Pane", Vec::new()),
        ("Mode", Vec::new()),
        ("Other", Vec::new()),
    ];

    for entry in effective_attach_keybindings(config) {
        let category = match entry.action {
            RuntimeAction::NewSession
            | RuntimeAction::SessionPrev
            | RuntimeAction::SessionNext
            | RuntimeAction::Detach
            | RuntimeAction::Quit => "Session",
            RuntimeAction::NewWindow
            | RuntimeAction::WindowPrev
            | RuntimeAction::WindowNext
            | RuntimeAction::WindowGoto1
            | RuntimeAction::WindowGoto2
            | RuntimeAction::WindowGoto3
            | RuntimeAction::WindowGoto4
            | RuntimeAction::WindowGoto5
            | RuntimeAction::WindowGoto6
            | RuntimeAction::WindowGoto7
            | RuntimeAction::WindowGoto8
            | RuntimeAction::WindowGoto9
            | RuntimeAction::WindowClose => "Other",
            RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane => "Pane",
            RuntimeAction::EnterWindowMode
            | RuntimeAction::ExitMode
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback
            | RuntimeAction::ShowHelp => "Mode",
            _ => "Other",
        };

        if let Some((_, entries)) = groups.iter_mut().find(|(name, _)| *name == category) {
            entries.push(entry);
        }
    }

    let mut lines = Vec::new();
    lines.push("Attach Help".to_string());
    lines.push(format!(
        "Normal mode sends typing to the pane. Use {scroll} for scrollback, {detach} to detach, and {help} to toggle help."
    ));
    lines.push(String::new());
    for (category, mut entries) in groups {
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|left, right| {
            left.scope
                .as_str()
                .cmp(right.scope.as_str())
                .then_with(|| left.chord.cmp(&right.chord))
        });
        lines.push(format!("-- {category} --"));
        for entry in entries {
            lines.push(format!(
                "[{:<7}] {:<20} {}",
                entry.scope.as_str(),
                entry.chord,
                entry.action_name
            ));
        }
        lines.push(String::new());
    }

    if lines.last().is_some_and(String::is_empty) {
        let _ = lines.pop();
    }
    lines
}

fn normalize_attach_keybindings(
    bindings: std::collections::BTreeMap<String, String>,
    scope: &str,
) -> std::collections::BTreeMap<String, String> {
    bindings
        .into_iter()
        .filter_map(
            |(chord, action_name)| match crate::input::parse_runtime_action_name(&action_name) {
                Ok(action) if is_attach_runtime_action(&action) => {
                    Some((chord, action_to_config_name(&action)))
                }
                Ok(_) => None,
                Err(error) => {
                    eprintln!(
                        "bmux warning: dropping invalid {scope} keybinding '{chord}' -> '{action_name}' ({error})"
                    );
                    None
                }
            },
        )
        .collect()
}

fn inject_attach_global_defaults(global: &mut std::collections::BTreeMap<String, String>) {
    let defaults = [
        ("alt+h", RuntimeAction::SessionPrev),
        ("alt+l", RuntimeAction::SessionNext),
    ];

    for (key, action) in defaults {
        global
            .entry(key.to_string())
            .or_insert_with(|| action_to_config_name(&action));
    }
}

const fn is_attach_runtime_action(action: &RuntimeAction) -> bool {
    matches!(
        action,
        RuntimeAction::Detach
            | RuntimeAction::Quit
            | RuntimeAction::NewWindow
            | RuntimeAction::NewSession
            | RuntimeAction::SessionPrev
            | RuntimeAction::SessionNext
            | RuntimeAction::EnterWindowMode
            | RuntimeAction::ExitMode
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback
            | RuntimeAction::WindowPrev
            | RuntimeAction::WindowNext
            | RuntimeAction::WindowGoto1
            | RuntimeAction::WindowGoto2
            | RuntimeAction::WindowGoto3
            | RuntimeAction::WindowGoto4
            | RuntimeAction::WindowGoto5
            | RuntimeAction::WindowGoto6
            | RuntimeAction::WindowGoto7
            | RuntimeAction::WindowGoto8
            | RuntimeAction::WindowGoto9
            | RuntimeAction::WindowClose
            | RuntimeAction::PluginCommand { .. }
            | RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane
            | RuntimeAction::ShowHelp
    )
}

fn default_attach_keymap() -> crate::input::Keymap {
    let defaults = BmuxConfig::default();
    let (runtime_bindings, global_bindings, scroll_bindings) =
        filtered_attach_keybindings(&defaults);
    let timeout_ms = defaults
        .keybindings
        .resolve_timeout()
        .expect("default timeout config must be valid")
        .timeout_ms();
    crate::input::Keymap::from_parts_with_scroll(
        &defaults.keybindings.prefix,
        timeout_ms,
        &runtime_bindings,
        &global_bindings,
        &scroll_bindings,
    )
    .expect("default attach keymap must be valid")
}

fn describe_timeout(timeout: &ResolvedTimeout) -> String {
    match timeout {
        ResolvedTimeout::Indefinite => "indefinite".to_string(),
        ResolvedTimeout::Exact(ms) => format!("exact ({ms}ms)"),
        ResolvedTimeout::Profile { name, ms } => format!("profile:{name} ({ms}ms)"),
    }
}

struct RawModeGuard {
    keyboard_enhanced: bool,
}

impl RawModeGuard {
    fn enable(kitty_keyboard_enabled: bool) -> Result<Self> {
        enable_raw_mode().context("failed enabling raw mode")?;

        #[cfg(feature = "kitty-keyboard")]
        let keyboard_enhanced = kitty_keyboard_enabled
            && crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
        #[cfg(not(feature = "kitty-keyboard"))]
        let keyboard_enhanced = false;

        let _ = kitty_keyboard_enabled; // suppress unused warning when feature is disabled

        if keyboard_enhanced {
            use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
            let mut stdout = io::stdout();
            queue!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )
            .context("failed to push keyboard enhancement flags")?;
            stdout
                .flush()
                .context("failed to flush after pushing keyboard flags")?;
        }

        Ok(Self { keyboard_enhanced })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.keyboard_enhanced {
            use crossterm::event::PopKeyboardEnhancementFlags;
            let mut stdout = io::stdout();
            let _ = queue!(stdout, PopKeyboardEnhancementFlags);
            let _ = stdout.flush();
        }
        let _ = disable_raw_mode();
    }
}

async fn poll_attach_terminal_event(timeout: Duration) -> Result<Option<Event>> {
    tokio::task::spawn_blocking(move || {
        if event::poll(timeout).context("failed polling terminal events")? {
            let event = event::read().context("failed reading terminal event")?;
            return Ok(Some(event));
        }

        Ok(None)
    })
    .await
    .context("failed to join terminal event task")?
}

async fn update_attach_viewport(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<(), ClientError> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    client.attach_set_viewport(session_id, cols, rows).await?;
    Ok(())
}

async fn hydrate_attach_state_from_snapshot(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    let AttachSnapshotState {
        context_id: _,
        session_id,
        focused_pane_id,
        panes,
        layout_root,
        scene,
        chunks,
    } = client
        .attach_snapshot(view_state.attached_id, ATTACH_SNAPSHOT_MAX_BYTES_PER_PANE)
        .await?;

    view_state.cached_layout_state = Some(AttachLayoutState {
        context_id: None,
        session_id,
        focused_pane_id,
        panes,
        layout_root,
        scene,
    });
    view_state.pane_buffers.clear();
    if let Some(layout_state) = view_state.cached_layout_state.as_ref() {
        resize_attach_parsers_for_scene(&mut view_state.pane_buffers, &layout_state.scene);
    }
    for chunk in chunks {
        if chunk.data.is_empty() {
            continue;
        }
        let buffer = view_state.pane_buffers.entry(chunk.pane_id).or_default();
        append_pane_output(buffer, &chunk.data);
        view_state.dirty.pane_dirty_ids.insert(chunk.pane_id);
    }
    view_state.dirty.layout_needs_refresh = false;
    view_state.dirty.full_pane_redraw = true;
    view_state.dirty.status_needs_redraw = true;
    Ok(())
}

fn resize_attach_parsers_for_scene(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &bmux_ipc::AttachScene,
) {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    resize_attach_parsers_for_scene_with_size(pane_buffers, scene, cols, rows);
}

fn resize_attach_parsers_for_scene_with_size(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &bmux_ipc::AttachScene,
    cols: u16,
    rows: u16,
) {
    if cols == 0 || rows <= 1 {
        return;
    }

    for surface in &scene.surfaces {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible {
            continue;
        }
        let rect = PaneRect {
            x: surface.rect.x.min(cols.saturating_sub(1)),
            y: surface.rect.y.min(rows.saturating_sub(1)),
            w: surface.rect.w.min(cols),
            h: surface
                .rect
                .h
                .min(rows.saturating_sub(surface.rect.y.min(rows.saturating_sub(1)))),
        };
        if rect.w < 2 || rect.h < 2 {
            continue;
        }
        let inner_w = rect.w.saturating_sub(2).max(1);
        let inner_h = rect.h.saturating_sub(2).max(1);
        let buffer = pane_buffers.entry(pane_id).or_default();
        buffer.parser.screen_mut().set_size(inner_h, inner_w);
    }
}

async fn handle_attach_loop_event(
    event: AttachLoopEvent,
    client: &mut BmuxClient,
    attach_input_processor: &mut InputProcessor,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    global: bool,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    match event {
        AttachLoopEvent::Server(server_event) => {
            handle_attach_server_event(
                client,
                server_event,
                follow_target_id,
                self_client_id,
                global,
                view_state,
            )
            .await
        }
        AttachLoopEvent::Terminal(terminal_event) => {
            handle_attach_terminal_event(
                client,
                terminal_event,
                attach_input_processor,
                help_lines,
                view_state,
            )
            .await
        }
    }
}

async fn handle_attach_server_event(
    client: &mut BmuxClient,
    server_event: bmux_client::ServerEvent,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    _global: bool,
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    if is_attach_terminal_server_exit_event(&server_event, view_state.attached_id) {
        return Ok(AttachLoopControl::Break(AttachExitReason::StreamClosed));
    }

    match server_event {
        bmux_client::ServerEvent::FollowTargetChanged {
            follower_client_id,
            leader_client_id,
            context_id,
            session_id,
        } => {
            if Some(leader_client_id) != follow_target_id
                || Some(follower_client_id) != self_client_id
            {
                return Ok(AttachLoopControl::Continue);
            }
            let attach_info = if let Some(context_id) = context_id {
                open_attach_for_context(client, context_id)
                    .await
                    .map_err(map_attach_client_error)?
            } else if view_state.attached_context_id.is_none() {
                open_attach_for_session(client, session_id)
                    .await
                    .map_err(map_attach_client_error)?
            } else {
                return Ok(AttachLoopControl::Continue);
            };
            view_state.attached_id = attach_info.session_id;
            view_state.attached_context_id = attach_info.context_id.or(context_id);
            view_state.can_write = attach_info.can_write;
            update_attach_viewport(client, view_state.attached_id).await?;
            hydrate_attach_state_from_snapshot(client, view_state)
                .await
                .map_err(map_attach_client_error)?;
            view_state.ui_mode = AttachUiMode::Normal;
            let status = attach_context_status(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
            )
            .await
            .map_err(map_attach_client_error)?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            if !view_state.can_write {
                println!("read-only attach: input disabled");
            }
        }
        bmux_client::ServerEvent::FollowTargetGone {
            former_leader_client_id,
            ..
        } if Some(former_leader_client_id) == follow_target_id => {
            println!("follow target disconnected; staying on current session");
        }
        bmux_client::ServerEvent::AttachViewChanged {
            context_id,
            session_id,
            components,
            ..
        } if attach_view_event_matches_target(view_state, context_id, session_id) => {
            apply_attach_view_change_components(&components, view_state);
        }
        _ => {}
    }

    Ok(AttachLoopControl::Continue)
}

fn apply_attach_view_change_components(
    components: &[AttachViewComponent],
    view_state: &mut AttachViewState,
) {
    // Components are applied sequentially in server-provided order so future
    // fine-grained refresh behavior can build on earlier invalidation steps
    // without re-sorting or undoing prior effects.
    for component in components {
        match component {
            AttachViewComponent::Scene => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                view_state.dirty.status_needs_redraw = true;
            }
            AttachViewComponent::SurfaceContent => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            AttachViewComponent::Layout => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                view_state.dirty.status_needs_redraw = true;
            }
            AttachViewComponent::Status => {
                view_state.dirty.status_needs_redraw = true;
            }
        }
    }
}

fn is_attach_terminal_server_exit_event(
    event: &bmux_client::ServerEvent,
    attached_id: Uuid,
) -> bool {
    matches!(event, bmux_client::ServerEvent::SessionRemoved { id } if *id == attached_id)
}

fn attach_view_event_matches_target(
    view_state: &AttachViewState,
    event_context_id: Option<Uuid>,
    event_session_id: Uuid,
) -> bool {
    if let Some(attached_context_id) = view_state.attached_context_id {
        return event_context_id == Some(attached_context_id);
    }
    event_session_id == view_state.attached_id
}

async fn handle_attach_terminal_event(
    client: &mut BmuxClient,
    terminal_event: Event,
    attach_input_processor: &mut InputProcessor,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    if matches!(terminal_event, Event::Resize(_, _)) {
        if let Err(error) = refresh_attached_session_from_context(client, view_state).await {
            view_state.set_transient_status(
                format!(
                    "context refresh delayed: {}",
                    map_attach_client_error(error)
                ),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        update_attach_viewport(client, view_state.attached_id).await?;
    }

    let mut skip_attach_key_actions = false;
    if view_state.quit_confirmation_pending
        && let Event::Key(key) = &terminal_event
        && key.kind == KeyEventKind::Press
    {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                let selector = attached_session_selector(client, view_state).await?;
                match client.kill_session(selector).await {
                    Ok(_) => return Ok(AttachLoopControl::Break(AttachExitReason::Quit)),
                    Err(error) => {
                        let status = attach_quit_failure_status(&error);
                        view_state.set_transient_status(
                            status,
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                    }
                }
                view_state.quit_confirmation_pending = false;
                view_state.dirty.status_needs_redraw = true;
                skip_attach_key_actions = true;
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Enter => {
                view_state.quit_confirmation_pending = false;
                view_state.dirty.status_needs_redraw = true;
                skip_attach_key_actions = true;
            }
            _ => {
                skip_attach_key_actions = true;
            }
        }
    }

    if skip_attach_key_actions {
        return Ok(AttachLoopControl::Continue);
    }

    if view_state.help_overlay_open
        && let Event::Key(key) = &terminal_event
        && handle_help_overlay_key_event(key, help_lines, view_state)
    {
        return Ok(AttachLoopControl::Continue);
    }

    for attach_action in
        attach_event_actions(&terminal_event, attach_input_processor, view_state.ui_mode)?
    {
        match attach_action {
            AttachEventAction::Detach => {
                return Ok(AttachLoopControl::Break(AttachExitReason::Detached));
            }
            AttachEventAction::Send(bytes) => {
                if view_state.help_overlay_open {
                    continue;
                }
                if view_state.can_write {
                    if let Err(error) =
                        refresh_attached_session_from_context(client, view_state).await
                    {
                        view_state.set_transient_status(
                            format!(
                                "context refresh delayed: {}",
                                map_attach_client_error(error)
                            ),
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                    }
                    match client.attach_input(view_state.attached_id, bytes).await {
                        Ok(_) => {}
                        Err(error) if is_attach_stream_closed_error(&error) => {
                            return Ok(AttachLoopControl::Break(AttachExitReason::StreamClosed));
                        }
                        Err(error) => return Err(map_attach_client_error(error)),
                    }
                }
            }
            AttachEventAction::Runtime(action) => {
                if view_state.help_overlay_open {
                    continue;
                }
                if let Err(error) = handle_attach_runtime_action(client, action, view_state).await {
                    println!("attach action failed: {}", map_attach_client_error(error));
                } else {
                    view_state.dirty.status_needs_redraw = true;
                    view_state.dirty.layout_needs_refresh = true;
                    view_state.dirty.full_pane_redraw = true;
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
            }
            AttachEventAction::PluginCommand {
                plugin_id,
                command_name,
            } => {
                if view_state.help_overlay_open {
                    continue;
                }
                let before_context_id = match client.current_context().await {
                    Ok(context) => context.map(|entry| entry.id),
                    Err(_) => None,
                };
                let before_context_ids = client.list_contexts().await.ok().map(|contexts| {
                    contexts
                        .into_iter()
                        .map(|context| context.id)
                        .collect::<std::collections::BTreeSet<_>>()
                });
                debug!(
                    plugin_id = %plugin_id,
                    command_name = %command_name,
                    before_context_id = ?before_context_id,
                    attached_context_id = ?view_state.attached_context_id,
                    attached_session_id = %view_state.attached_id,
                    "attach.plugin_command.start"
                );
                match run_plugin_keybinding_command(&plugin_id, &command_name, &[]) {
                    Err(error) => {
                        warn!(
                            plugin_id = %plugin_id,
                            command_name = %command_name,
                            error = %error,
                            "attach.plugin_command.run_failed"
                        );
                        view_state.set_transient_status(
                            format!("plugin action failed: {error}"),
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                    }
                    Ok(execution) => {
                        let status = execution.status;
                        let effect_count = execution.outcome.effects.len();
                        if status != 0 {
                            warn!(
                                plugin_id = %plugin_id,
                                command_name = %command_name,
                                status,
                                effect_count,
                                before_context_id = ?before_context_id,
                                attached_context_id = ?view_state.attached_context_id,
                                attached_session_id = %view_state.attached_id,
                                "attach.plugin_command.nonzero_status"
                            );
                            view_state.set_transient_status(
                                format!(
                                    "plugin action failed ({plugin_id}:{command_name}) exit {status}"
                                ),
                                Instant::now(),
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                            attach_input_processor.set_scroll_mode(view_state.scrollback_active);
                            continue;
                        }

                        let outcome_applied = match apply_plugin_command_outcome(
                            client,
                            view_state,
                            execution.outcome,
                        )
                        .await
                        {
                            Ok(applied) => applied,
                            Err(error) => {
                                view_state.set_transient_status(
                                    format!(
                                        "plugin outcome apply failed: {}",
                                        map_attach_client_error(error)
                                    ),
                                    Instant::now(),
                                    ATTACH_TRANSIENT_STATUS_TTL,
                                );
                                attach_input_processor
                                    .set_scroll_mode(view_state.scrollback_active);
                                continue;
                            }
                        };

                        let after_context_id = match client.current_context().await {
                            Ok(context) => context.map(|entry| entry.id),
                            Err(_) => None,
                        };
                        let after_context_ids = client.list_contexts().await.ok().map(|contexts| {
                            contexts
                                .into_iter()
                                .map(|context| context.id)
                                .collect::<std::collections::BTreeSet<_>>()
                        });
                        debug!(
                            plugin_id = %plugin_id,
                            command_name = %command_name,
                            effect_count,
                            outcome_applied,
                            before_context_id = ?before_context_id,
                            after_context_id = ?after_context_id,
                            attached_context_id = ?view_state.attached_context_id,
                            attached_session_id = %view_state.attached_id,
                            "attach.plugin_command.outcome"
                        );

                        if let Some(fallback_context_id) = plugin_fallback_retarget_context_id(
                            before_context_id,
                            after_context_id,
                            view_state.attached_context_id,
                            outcome_applied,
                        ) {
                            debug!(
                                plugin_id = %plugin_id,
                                command_name = %command_name,
                                fallback_context_id = %fallback_context_id,
                                "attach.plugin_command.fallback_retarget"
                            );
                            if let Err(error) =
                                retarget_attach_to_context(client, view_state, fallback_context_id)
                                    .await
                            {
                                warn!(
                                    plugin_id = %plugin_id,
                                    command_name = %command_name,
                                    fallback_context_id = %fallback_context_id,
                                    error = %error,
                                    "attach.plugin_command.fallback_retarget_failed"
                                );
                                view_state.set_transient_status(
                                    format!(
                                        "plugin fallback retarget failed: {}",
                                        map_attach_client_error(error)
                                    ),
                                    Instant::now(),
                                    ATTACH_TRANSIENT_STATUS_TTL,
                                );
                                attach_input_processor
                                    .set_scroll_mode(view_state.scrollback_active);
                                continue;
                            }
                            view_state.set_transient_status(
                                format!(
                                    "plugin action: {plugin_id}:{command_name} (fallback retarget)"
                                ),
                                Instant::now(),
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                            view_state.dirty.layout_needs_refresh = true;
                            view_state.dirty.full_pane_redraw = true;
                            attach_input_processor.set_scroll_mode(view_state.scrollback_active);
                            continue;
                        }

                        if let Some(fallback_context_id) = plugin_fallback_new_context_id(
                            before_context_ids.as_ref(),
                            after_context_ids.as_ref(),
                            view_state.attached_context_id,
                            after_context_id,
                            outcome_applied,
                        ) {
                            debug!(
                                plugin_id = %plugin_id,
                                command_name = %command_name,
                                fallback_context_id = %fallback_context_id,
                                "attach.plugin_command.new_context_fallback_retarget"
                            );
                            if let Err(error) =
                                retarget_attach_to_context(client, view_state, fallback_context_id)
                                    .await
                            {
                                warn!(
                                    plugin_id = %plugin_id,
                                    command_name = %command_name,
                                    fallback_context_id = %fallback_context_id,
                                    error = %error,
                                    "attach.plugin_command.new_context_fallback_retarget_failed"
                                );
                                view_state.set_transient_status(
                                    format!(
                                        "plugin new-context fallback failed: {}",
                                        map_attach_client_error(error)
                                    ),
                                    Instant::now(),
                                    ATTACH_TRANSIENT_STATUS_TTL,
                                );
                                attach_input_processor
                                    .set_scroll_mode(view_state.scrollback_active);
                                continue;
                            }
                            view_state.set_transient_status(
                                format!(
                                    "plugin action: {plugin_id}:{command_name} (new context retarget)"
                                ),
                                Instant::now(),
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                            view_state.dirty.layout_needs_refresh = true;
                            view_state.dirty.full_pane_redraw = true;
                            attach_input_processor.set_scroll_mode(view_state.scrollback_active);
                            continue;
                        }

                        view_state.set_transient_status(
                            format!("plugin action: {plugin_id}:{command_name}"),
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                        view_state.dirty.layout_needs_refresh = true;
                        view_state.dirty.full_pane_redraw = true;
                    }
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
            }
            AttachEventAction::Ui(action) => {
                if matches!(action, RuntimeAction::ShowHelp) {
                    view_state.help_overlay_open = !view_state.help_overlay_open;
                    if !view_state.help_overlay_open {
                        view_state.help_overlay_scroll = 0;
                    }
                    view_state.dirty.status_needs_redraw = true;
                    view_state.dirty.full_pane_redraw = true;
                    continue;
                }
                if view_state.help_overlay_open {
                    if matches!(action, RuntimeAction::ExitMode)
                        || matches!(action, RuntimeAction::ForwardToPane(_))
                    {
                        view_state.help_overlay_open = false;
                        view_state.help_overlay_scroll = 0;
                        view_state.dirty.status_needs_redraw = true;
                        view_state.dirty.full_pane_redraw = true;
                    }
                    continue;
                }
                if matches!(action, RuntimeAction::Quit) {
                    view_state.quit_confirmation_pending = true;
                    view_state.dirty.status_needs_redraw = true;
                    continue;
                }
                if let Err(error) = handle_attach_ui_action(client, action, view_state).await {
                    println!("attach action failed: {}", map_attach_client_error(error));
                } else {
                    view_state.dirty.layout_needs_refresh = true;
                    view_state.dirty.full_pane_redraw = true;
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
                view_state.dirty.status_needs_redraw = true;
            }
            AttachEventAction::Redraw => {
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            AttachEventAction::Ignore => {}
        }
    }

    Ok(AttachLoopControl::Continue)
}

fn restore_terminal_after_attach_ui() -> Result<()> {
    let mut stdout = io::stdout();
    // Safety net: pop keyboard enhancement flags in case the drop guard didn't run.
    #[cfg(feature = "kitty-keyboard")]
    let _ = queue!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
    queue!(
        stdout,
        Show,
        Print("\x1b[0m"),
        MoveTo(0, 0),
        Clear(ClearType::All),
        MoveTo(0, 0)
    )
    .context("failed restoring terminal after attach ui")?;
    stdout
        .flush()
        .context("failed flushing terminal restoration")
}

fn attach_event_actions(
    event: &Event,
    attach_input_processor: &mut InputProcessor,
    ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    match event {
        Event::Key(key) => attach_key_event_actions(key, attach_input_processor, ui_mode),
        Event::Resize(_, _) => Ok(vec![AttachEventAction::Redraw]),
        Event::Mouse(_) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {
            Ok(vec![AttachEventAction::Ignore])
        }
    }
}

fn attach_key_event_actions(
    key: &KeyEvent,
    attach_input_processor: &mut InputProcessor,
    _ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    // Accept Press and Repeat events. Release events are filtered out here
    // and also inside InputProcessor's crossterm adapter as a safety net.
    if key.kind == KeyEventKind::Release {
        return Ok(vec![AttachEventAction::Ignore]);
    }

    let actions = attach_input_processor.process_terminal_event(Event::Key(*key));
    Ok(actions
        .into_iter()
        .map(|action| match action {
            RuntimeAction::Detach => AttachEventAction::Detach,
            RuntimeAction::ForwardToPane(bytes) => AttachEventAction::Send(bytes),
            RuntimeAction::NewWindow | RuntimeAction::NewSession => {
                AttachEventAction::Runtime(action)
            }
            RuntimeAction::PluginCommand {
                plugin_id,
                command_name,
            } => AttachEventAction::PluginCommand {
                plugin_id,
                command_name,
            },
            RuntimeAction::SessionPrev | RuntimeAction::SessionNext => {
                AttachEventAction::Ui(action)
            }
            RuntimeAction::EnterWindowMode
            | RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane => AttachEventAction::Ui(action),
            RuntimeAction::ExitMode
            | RuntimeAction::WindowPrev
            | RuntimeAction::WindowNext
            | RuntimeAction::WindowGoto1
            | RuntimeAction::WindowGoto2
            | RuntimeAction::WindowGoto3
            | RuntimeAction::WindowGoto4
            | RuntimeAction::WindowGoto5
            | RuntimeAction::WindowGoto6
            | RuntimeAction::WindowGoto7
            | RuntimeAction::WindowGoto8
            | RuntimeAction::WindowGoto9
            | RuntimeAction::WindowClose => AttachEventAction::Ui(action),
            RuntimeAction::Quit => AttachEventAction::Ui(action),
            RuntimeAction::ShowHelp => AttachEventAction::Ui(action),
            RuntimeAction::ToggleSplitDirection
            | RuntimeAction::RestartFocusedPane
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::MoveCursorLeft
            | RuntimeAction::MoveCursorRight
            | RuntimeAction::MoveCursorUp
            | RuntimeAction::MoveCursorDown
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback => AttachEventAction::Ui(action),
        })
        .collect())
}

const fn is_attach_stream_closed_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::ServerError {
            code: bmux_ipc::ErrorCode::NotFound,
            ..
        }
    )
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

async fn run_session_detach() -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-detach").await?;
    client.detach().await.map_err(map_cli_client_error)?;
    println!("detached");
    Ok(0)
}

async fn run_follow(target_client_id: &str, global: bool) -> Result<u8> {
    let target_client_id = parse_uuid_value(target_client_id, "target client id")?;
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-follow").await?;
    client
        .follow_client(target_client_id, global)
        .await
        .map_err(map_cli_client_error)?;
    println!(
        "following client: {}{}",
        target_client_id,
        if global { " (global)" } else { "" }
    );
    Ok(0)
}

async fn run_unfollow() -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-unfollow").await?;
    client.unfollow().await.map_err(map_cli_client_error)?;
    println!("follow stopped");
    Ok(0)
}

fn parse_session_selector(target: &str) -> SessionSelector {
    match Uuid::parse_str(target) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(target.to_string()),
    }
}

fn parse_uuid_value(value: &str, label: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("{label} must be a UUID, got '{value}'"))
}

async fn server_is_running() -> Result<bool> {
    probe_server_running().await
}

async fn probe_server_running() -> Result<bool> {
    Ok(fetch_server_status()
        .await?
        .is_some_and(|status| status.running))
}

async fn fetch_server_status() -> Result<Option<bmux_client::ServerStatusInfo>> {
    let connect = tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-status")).await;

    let mut client = match connect {
        Ok(Ok(client)) => client,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    match tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.server_status()).await {
        Ok(Ok(status)) => Ok(Some(status)),
        Ok(Err(_)) | Err(_) => Ok(None),
    }
}

async fn wait_for_server_running(timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let connect =
            tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-start-wait")).await;
        if let Ok(Ok(mut client)) = connect
            && let Ok(Ok(status)) =
                tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.server_status()).await
            && status.running
        {
            return Ok(true);
        }
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }
    Ok(false)
}

async fn wait_until_server_stopped(timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let reconnect =
            tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-stop-check")).await;
        if reconnect.is_err() || matches!(reconnect, Ok(Err(_))) {
            return Ok(true);
        }
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }

    Ok(false)
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_pid_running(pid)? {
            return Ok(true);
        }
        std::thread::sleep(SERVER_POLL_INTERVAL);
    }
    Ok(!is_pid_running(pid)?)
}

fn server_pid_file_path() -> PathBuf {
    bmux_config::ConfigPaths::default().server_pid_file()
}

fn write_server_pid_file(pid: u32) -> Result<()> {
    let path = server_pid_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating runtime dir {}", parent.display()))?;
    }
    std::fs::write(&path, pid.to_string())
        .with_context(|| format!("failed writing pid file {}", path.display()))
}

fn read_server_pid_file() -> Result<Option<u32>> {
    let path = server_pid_file_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed reading pid file {}", path.display()));
        }
    };

    if let Some(pid) = parse_pid_content(&content) {
        Ok(Some(pid))
    } else {
        let _ = remove_server_pid_file();
        Ok(None)
    }
}

fn remove_server_pid_file() -> Result<()> {
    let path = server_pid_file_path();
    let remove_pid_result = match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed removing pid file {}", path.display()))
        }
    };
    let remove_metadata_result = remove_server_runtime_metadata_file();
    remove_pid_result.and(remove_metadata_result)
}

fn try_kill_pid(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        let status = ProcessCommand::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .context("failed to execute kill command")?;
        Ok(status.success())
    }

    #[cfg(windows)]
    {
        let status = ProcessCommand::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .status()
            .context("failed to execute taskkill command")?;
        return Ok(status.success());
    }
}

fn is_pid_running(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        let status = ProcessCommand::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .context("failed to execute kill -0 command")?;
        Ok(status.success())
    }

    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        let output = ProcessCommand::new("tasklist")
            .arg("/FI")
            .arg(filter)
            .output()
            .context("failed to execute tasklist command")?;
        if !output.status.success() {
            return Ok(false);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(stdout.lines().any(|line| line.contains(&pid.to_string())));
    }
}

async fn cleanup_stale_pid_file() -> Result<()> {
    let Some(pid) = read_server_pid_file()? else {
        return Ok(());
    };

    if !is_pid_running(pid)? && !probe_server_running().await? {
        remove_server_pid_file()?;
    }

    Ok(())
}

fn parse_pid_content(content: &str) -> Option<u32> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u32>().ok().filter(|pid| *pid > 0)
}

fn run_terminal_install_terminfo(yes: bool, check_only: bool) -> Result<u8> {
    let configured = BmuxConfig::load().map_or_else(
        |_| "bmux-256color".to_string(),
        |cfg| cfg.behavior.pane_term,
    );
    let is_installed = check_terminfo_available("bmux-256color") == Some(true);

    if check_only {
        if is_installed {
            println!("bmux-256color terminfo is installed");
            return Ok(0);
        }
        println!("bmux-256color terminfo is not installed");
        return Ok(1);
    }

    if is_installed {
        println!("bmux-256color terminfo is already installed");
        return Ok(0);
    }

    if !yes && io::stdin().is_terminal() {
        println!("bmux-256color terminfo is missing.");
        println!("Install now? [Y/n]");
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("failed reading install confirmation")?;
        let trimmed = answer.trim().to_ascii_lowercase();
        if trimmed == "n" || trimmed == "no" {
            println!("skipped terminfo installation");
            return Ok(0);
        }
    }

    install_bmux_terminfo()?;
    if check_terminfo_available("bmux-256color") == Some(true) {
        println!("installed terminfo entry: bmux-256color");
        if configured != "bmux-256color" {
            println!("note: current config pane_term is '{configured}'");
        }
        Ok(0)
    } else {
        anyhow::bail!("terminfo install completed but bmux-256color is still unavailable")
    }
}

fn run_terminal_doctor(
    as_json: bool,
    include_trace: bool,
    trace_limit: usize,
    trace_family: Option<TraceFamily>,
    trace_pane: Option<u16>,
) -> Result<u8> {
    let config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            println!(
                "bmux terminal doctor warning: failed to load config ({error}); using defaults"
            );
            BmuxConfig::default()
        }
    };

    let configured_term = config.behavior.pane_term.clone();
    let effective = resolve_pane_term(&configured_term);
    let protocol_profile = protocol_profile_for_terminal_profile(effective.profile);
    let last_declined_prompt_epoch_secs = last_prompt_decline_epoch_secs();
    let trace_data = if include_trace {
        load_protocol_trace(10_000)?
    } else {
        ProtocolTraceData::default()
    };
    let trace_events =
        filter_trace_events(&trace_data.events, trace_family, trace_pane, trace_limit);

    if as_json {
        let payload = serde_json::json!({
            "configured_pane_term": configured_term,
            "effective_pane_term": effective.pane_term,
            "terminal_profile": terminal_profile_name(effective.profile),
            "protocol_profile": protocol_profile_name(protocol_profile),
            "primary_da_reply": String::from_utf8_lossy(primary_da_for_profile(protocol_profile)),
            "secondary_da_reply": String::from_utf8_lossy(secondary_da_for_profile(protocol_profile)),
            "supported_queries": supported_query_names(),
            "fallback_chain": effective.fallback_chain,
            "terminfo_check": {
                "attempted": effective.terminfo_checked,
                "available": effective.terminfo_available,
            },
            "terminfo_checks": effective
                .terminfo_checks
                .iter()
                .map(|(term, available)| serde_json::json!({
                    "term": term,
                    "available": available,
                }))
                .collect::<Vec<_>>(),
            "warnings": effective.warnings,
            "terminfo_auto_install": {
                "policy": terminfo_auto_install_name(config.behavior.terminfo_auto_install),
                "prompt_cooldown_days": config.behavior.terminfo_prompt_cooldown_days,
                "last_declined_prompt_epoch_secs": last_declined_prompt_epoch_secs,
            },
            "trace": if include_trace {
                serde_json::json!({
                    "events": trace_events,
                    "limit": trace_limit,
                    "dropped": trace_data.dropped,
                    "applied_filters": {
                        "family": trace_family.map(trace_family_name),
                        "pane": trace_pane,
                    },
                })
            } else {
                serde_json::Value::Null
            },
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed to encode terminal doctor json")?
        );
        return Ok(0);
    }

    println!("bmux terminal doctor");
    println!("configured pane TERM: {configured_term}");
    println!("effective pane TERM: {}", effective.pane_term);
    println!(
        "terminal profile: {}",
        terminal_profile_name(effective.profile)
    );
    println!(
        "protocol profile: {}",
        protocol_profile_name(protocol_profile)
    );
    println!(
        "primary DA reply: {}",
        String::from_utf8_lossy(primary_da_for_profile(protocol_profile))
    );
    println!(
        "secondary DA reply: {}",
        String::from_utf8_lossy(secondary_da_for_profile(protocol_profile))
    );
    println!(
        "terminfo auto-install policy: {} (cooldown {} days)",
        terminfo_auto_install_name(config.behavior.terminfo_auto_install),
        config.behavior.terminfo_prompt_cooldown_days
    );
    if let Some(epoch) = last_declined_prompt_epoch_secs {
        println!("last declined terminfo prompt (epoch secs): {epoch}");
    }
    println!("supported queries: {}", supported_query_names().join(", "));
    println!("fallback chain: {}", effective.fallback_chain.join(" -> "));
    if effective.terminfo_checked {
        println!(
            "terminfo available: {}",
            if effective.terminfo_available {
                "yes"
            } else {
                "no"
            }
        );
        for (term, available) in &effective.terminfo_checks {
            println!(
                "terminfo check {term}: {}",
                match available {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                }
            );
        }
    }
    for warning in effective.warnings {
        println!("warning: {warning}");
    }

    if include_trace {
        println!("trace events (latest {trace_limit}):");
        println!("trace dropped events: {}", trace_data.dropped);
        if trace_family.is_some() || trace_pane.is_some() {
            println!(
                "trace filters: family={} pane={}",
                trace_family.map_or("any", trace_family_name),
                trace_pane.map_or_else(|| "any".to_string(), |pane| pane.to_string())
            );
        }
        if trace_events.is_empty() {
            if trace_data.events.is_empty() {
                println!(
                    "  (no events found; enable behavior.protocol_trace_enabled and run a session)"
                );
            } else {
                println!("  (no events matched active filters)");
            }
        }
        for event in trace_events {
            let pane = event
                .pane_id
                .map_or_else(|| "-".to_string(), |id| id.to_string());
            println!(
                "  [{}] pane={} {}:{} {} {}",
                event.timestamp_ms,
                pane,
                event.family,
                event.name,
                match event.direction {
                    ProtocolDirection::Query => "query",
                    ProtocolDirection::Reply => "reply",
                },
                event.decoded.replace('\u{1b}', "<ESC>")
            );
        }
    }

    Ok(0)
}

fn plugin_keybinding_proposals(
    config: &BmuxConfig,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let paths = ConfigPaths::default();
    let registry = match scan_available_plugins(config, &paths) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!(
                "bmux warning: failed loading plugin keybinding proposals ({error}); continuing without plugin keybinding defaults"
            );
            return (
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
            );
        }
    };
    let enabled_plugins = effective_enabled_plugins(config, &registry)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut runtime = std::collections::BTreeMap::new();
    let mut global = std::collections::BTreeMap::new();
    let mut scroll = std::collections::BTreeMap::new();

    for plugin in registry.iter() {
        if !enabled_plugins.contains(plugin.declaration.id.as_str()) {
            continue;
        }
        for (chord, action) in &plugin.manifest.keybindings.runtime {
            runtime
                .entry(chord.clone())
                .or_insert_with(|| action.clone());
        }
        for (chord, action) in &plugin.manifest.keybindings.global {
            global
                .entry(chord.clone())
                .or_insert_with(|| action.clone());
        }
        for (chord, action) in &plugin.manifest.keybindings.scroll {
            scroll
                .entry(chord.clone())
                .or_insert_with(|| action.clone());
        }
    }

    (runtime, global, scroll)
}

fn merged_runtime_keybindings(
    config: &BmuxConfig,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let defaults = BmuxConfig::default();
    let (plugin_runtime, plugin_global, plugin_scroll) = plugin_keybinding_proposals(config);

    let mut runtime = defaults.keybindings.runtime;
    runtime.extend(plugin_runtime);
    runtime.extend(config.keybindings.runtime.clone());

    let mut global = defaults.keybindings.global;
    global.extend(plugin_global);
    global.extend(config.keybindings.global.clone());

    let mut scroll = defaults.keybindings.scroll;
    scroll.extend(plugin_scroll);
    scroll.extend(config.keybindings.scroll.clone());

    (runtime, global, scroll)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ProtocolTraceFile {
    dropped: usize,
    events: Vec<ProtocolTraceEvent>,
}

#[derive(Debug, Default)]
struct ProtocolTraceData {
    dropped: usize,
    events: Vec<ProtocolTraceEvent>,
}

fn load_protocol_trace(limit: usize) -> Result<ProtocolTraceData> {
    let path = bmux_config::ConfigPaths::default().protocol_trace_file();
    if !path.exists() {
        return Ok(ProtocolTraceData::default());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed reading protocol trace file at {}", path.display()))?;
    let file: ProtocolTraceFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed parsing protocol trace file at {}", path.display()))?;
    if limit == 0 || file.events.len() <= limit {
        return Ok(ProtocolTraceData {
            dropped: file.dropped,
            events: file.events,
        });
    }
    let start = file.events.len().saturating_sub(limit);
    Ok(ProtocolTraceData {
        dropped: file.dropped,
        events: file.events.into_iter().skip(start).collect(),
    })
}

fn filter_trace_events(
    events: &[ProtocolTraceEvent],
    family: Option<TraceFamily>,
    pane: Option<u16>,
    limit: usize,
) -> Vec<ProtocolTraceEvent> {
    let mut filtered: Vec<ProtocolTraceEvent> = events
        .iter()
        .filter(|event| {
            let family_matches =
                family.is_none_or(|value| event.family == trace_family_name(value));
            let pane_matches = pane.is_none_or(|value| event.pane_id == Some(value));
            family_matches && pane_matches
        })
        .cloned()
        .collect();
    if limit > 0 && filtered.len() > limit {
        let start = filtered.len().saturating_sub(limit);
        filtered = filtered.split_off(start);
    }
    filtered
}

const fn trace_family_name(family: TraceFamily) -> &'static str {
    match family {
        TraceFamily::Csi => "csi",
        TraceFamily::Osc => "osc",
        TraceFamily::Dcs => "dcs",
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct TerminfoPromptStateFile {
    last_declined_epoch_secs: Option<u64>,
}

fn install_bmux_terminfo() -> Result<()> {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../terminfo/bmux-256color.terminfo");
    if !source.exists() {
        anyhow::bail!("terminfo source file not found at {}", source.display());
    }

    let output = ProcessCommand::new("tic")
        .arg("-x")
        .arg(&source)
        .output()
        .context("failed to execute tic")?;
    if !output.status.success() {
        anyhow::bail!(
            "tic failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

const fn terminfo_auto_install_name(policy: TerminfoAutoInstall) -> &'static str {
    match policy {
        TerminfoAutoInstall::Ask => "ask",
        TerminfoAutoInstall::Always => "always",
        TerminfoAutoInstall::Never => "never",
    }
}

fn last_prompt_decline_epoch_secs() -> Option<u64> {
    let path = bmux_config::ConfigPaths::default().terminfo_prompt_state_file();
    let bytes = std::fs::read(path).ok()?;
    let state: TerminfoPromptStateFile = serde_json::from_slice(&bytes).ok()?;
    state.last_declined_epoch_secs
}

struct PaneTermResolution {
    pane_term: String,
    profile: TerminalProfile,
    warnings: Vec<String>,
    terminfo_checked: bool,
    terminfo_available: bool,
    fallback_chain: Vec<String>,
    terminfo_checks: Vec<(String, Option<bool>)>,
}

fn resolve_pane_term(configured: &str) -> PaneTermResolution {
    resolve_pane_term_with_checker(configured, check_terminfo_available)
}

fn resolve_pane_term_with_checker<F>(configured: &str, mut checker: F) -> PaneTermResolution
where
    F: FnMut(&str) -> Option<bool>,
{
    let configured_trimmed = configured.trim();
    let configured_normalized = if configured_trimmed.is_empty() {
        "bmux-256color".to_string()
    } else {
        configured_trimmed.to_string()
    };

    let mut warnings = Vec::new();
    if configured_trimmed.is_empty() {
        warnings.push("behavior.pane_term is empty; falling back to bmux-256color".to_string());
    }

    let fallback_chain = vec!["xterm-256color".to_string(), "screen-256color".to_string()];
    let mut terminfo_checks = Vec::new();
    let mut pane_term = configured_normalized;

    let configured_check = checker(&pane_term);
    terminfo_checks.push((pane_term.clone(), configured_check));

    if configured_check == Some(false) {
        let mut selected_fallback = None;
        for candidate in &fallback_chain {
            if candidate == &pane_term {
                continue;
            }
            let check = checker(candidate);
            terminfo_checks.push((candidate.clone(), check));
            if check == Some(true) {
                selected_fallback = Some(candidate.clone());
                break;
            }
        }

        if let Some(fallback) = selected_fallback {
            warnings.push(format!(
                "pane TERM '{}' not installed; using '{}' (fallback chain: {})",
                pane_term,
                fallback,
                fallback_chain.join(", ")
            ));
            if pane_term == "bmux-256color" {
                warnings.push(
                    "install bmux terminfo with scripts/install-terminfo.sh to use bmux-256color"
                        .to_string(),
                );
            }
            pane_term = fallback;
        } else {
            warnings.push(format!(
                "pane TERM '{}' not installed and no fallback available (checked: {})",
                pane_term,
                fallback_chain.join(", ")
            ));
        }
    } else if configured_check.is_none() {
        warnings.push(format!(
            "could not verify terminfo for pane TERM '{pane_term}'; continuing without fallback checks"
        ));
    }

    let profile = profile_for_term(&pane_term);

    let effective_terminfo_available = terminfo_checks
        .iter()
        .find_map(|(term, available)| (term == &pane_term).then_some(*available))
        .flatten();

    if profile == TerminalProfile::Conservative {
        warnings.push(format!(
            "pane TERM '{pane_term}' uses conservative capability profile; compatibility depends on host terminfo"
        ));
    }

    PaneTermResolution {
        pane_term,
        profile,
        warnings,
        terminfo_checked: terminfo_checks
            .iter()
            .any(|(_, available)| available.is_some()),
        terminfo_available: effective_terminfo_available.unwrap_or(false),
        fallback_chain,
        terminfo_checks,
    }
}

fn profile_for_term(term: &str) -> TerminalProfile {
    match term {
        "bmux-256color" => TerminalProfile::Bmux256Color,
        "screen-256color" | "tmux-256color" => TerminalProfile::Screen256Color,
        "xterm-256color" => TerminalProfile::Xterm256Color,
        _ => TerminalProfile::Conservative,
    }
}

const fn terminal_profile_name(profile: TerminalProfile) -> &'static str {
    match profile {
        TerminalProfile::Bmux256Color => "bmux-256color",
        TerminalProfile::Screen256Color => "screen-256color-compatible",
        TerminalProfile::Xterm256Color => "xterm-256color-compatible",
        TerminalProfile::Conservative => "conservative",
    }
}

const fn protocol_profile_for_terminal_profile(profile: TerminalProfile) -> ProtocolProfile {
    match profile {
        TerminalProfile::Bmux256Color => ProtocolProfile::Bmux,
        TerminalProfile::Screen256Color => ProtocolProfile::Screen,
        TerminalProfile::Xterm256Color => ProtocolProfile::Xterm,
        TerminalProfile::Conservative => ProtocolProfile::Conservative,
    }
}

fn check_terminfo_available(term: &str) -> Option<bool> {
    let output = ProcessCommand::new("infocmp").arg(term).output().ok()?;
    Some(output.status.success())
}

fn run_keymap_doctor(as_json: bool) -> Result<u8> {
    let config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            println!("bmux keymap doctor warning: failed to load config ({error}); using defaults");
            BmuxConfig::default()
        }
    };
    let (runtime_bindings, global_bindings, scroll_bindings) = merged_runtime_keybindings(&config);
    let resolved_timeout = config
        .keybindings
        .resolve_timeout()
        .map_err(anyhow::Error::msg)
        .context("failed resolving keymap timeout")?;
    let keymap = crate::input::Keymap::from_parts_with_scroll(
        &config.keybindings.prefix,
        resolved_timeout.timeout_ms(),
        &runtime_bindings,
        &global_bindings,
        &scroll_bindings,
    )
    .context("failed to compile keymap")?;

    let report = keymap.doctor_report();
    let attach_effective = effective_attach_keybindings(&config);

    if as_json {
        let payload = serde_json::json!({
            "prefix": config.keybindings.prefix,
            "timeout_ms": config.keybindings.timeout_ms,
            "timeout_profile": config.keybindings.timeout_profile,
            "timeout_profiles": config.keybindings.merged_timeout_profiles(),
            "resolved_timeout": match &resolved_timeout {
                ResolvedTimeout::Indefinite => serde_json::json!({
                    "mode": "indefinite"
                }),
                ResolvedTimeout::Exact(ms) => serde_json::json!({
                    "mode": "exact",
                    "ms": ms,
                }),
                ResolvedTimeout::Profile { name, ms } => serde_json::json!({
                    "mode": "profile",
                    "name": name,
                    "ms": ms,
                }),
            },
            "global": report
                .global
                .iter()
                .map(|binding| serde_json::json!({
                    "chord": binding.chord,
                    "action": binding.action,
                }))
                .collect::<Vec<_>>(),
            "runtime": report
                .runtime
                .iter()
                .map(|binding| serde_json::json!({
                    "chord": binding.chord,
                    "action": binding.action,
                }))
                .collect::<Vec<_>>(),
            "overlaps": report.overlaps,
            "attach_effective": attach_effective
                .iter()
                .map(|entry| serde_json::json!({
                    "scope": entry.scope.as_str(),
                    "chord": entry.chord,
                    "action": entry.action_name,
                }))
                .collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed to encode keymap doctor json")?
        );
        return Ok(0);
    }

    println!("bmux keymap doctor");
    println!("prefix: {}", config.keybindings.prefix);
    println!("timeout: {}", describe_timeout(&resolved_timeout));
    for line in keymap.doctor_lines() {
        println!("{line}");
    }

    println!("attach_effective:");
    for entry in attach_effective {
        println!(
            "  [{}] {} -> {}",
            entry.scope.as_str(),
            entry.chord,
            entry.action_name
        );
    }

    Ok(0)
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
            LogLevel::Error => moosicbox_log_runtime::init::LogLevel::Error,
            LogLevel::Warn => moosicbox_log_runtime::init::LogLevel::Warn,
            LogLevel::Info => moosicbox_log_runtime::init::LogLevel::Info,
            LogLevel::Debug => moosicbox_log_runtime::init::LogLevel::Debug,
            LogLevel::Trace => moosicbox_log_runtime::init::LogLevel::Trace,
        };
        match moosicbox_log_runtime::init::init(moosicbox_log_runtime::init::InitConfig {
            paths: &paths,
            level: runtime_level,
            with_target: false,
            file_prefix: "bmux.log",
        }) {
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

async fn run_playbook_run(
    source: &str,
    json: bool,
    target_server: bool,
    record: bool,
    export_gif: Option<&str>,
    viewport: Option<&str>,
    timeout: Option<u64>,
    shell: Option<&str>,
) -> Result<u8> {
    let mut playbook = if source == "-" {
        crate::playbook::parse_stdin().context("failed parsing playbook from stdin")?
    } else {
        crate::playbook::parse_file(std::path::Path::new(source))
            .with_context(|| format!("failed parsing playbook from {source}"))?
    };

    // CLI flags override playbook config.
    if record || export_gif.is_some() {
        playbook.config.record = true;
    }
    if let Some(vp) = viewport {
        let (cols, rows) = parse_viewport_string(vp)?;
        playbook.config.viewport.cols = cols;
        playbook.config.viewport.rows = rows;
    }
    if let Some(secs) = timeout {
        playbook.config.timeout = Duration::from_secs(secs);
    }
    if let Some(sh) = shell {
        playbook.config.shell = Some(sh.to_string());
    }

    // Populate bundled plugin IDs so the sandbox can configure plugins.
    playbook.config.bundled_plugin_ids = discover_bundled_plugin_ids();

    let result = crate::playbook::run(playbook, target_server).await?;

    // Export GIF if requested and a recording was produced.
    if let Some(gif_path) = export_gif {
        if let Some(ref rec_id) = result.recording_id {
            let recording_id_str = rec_id.to_string();
            match recording::run_recording_export(
                &recording_id_str,
                RecordingExportFormat::Gif,
                gif_path,
                None,                        // view_client: auto-detect
                1.0,                         // speed
                12,                          // fps
                None,                        // max_duration
                None,                        // max_frames
                RecordingRenderMode::Bitmap, // Use bitmap for headless (no real terminal fonts)
                None,                        // cell_size
                None,                        // cell_width
                None,                        // cell_height
                None,                        // font_family
                None,                        // font_size
                None,                        // line_height
                &[],                         // font_path
            )
            .await
            {
                Ok(_) => {
                    if !json {
                        println!("exported GIF: {gif_path}");
                    }
                }
                Err(e) => {
                    eprintln!("GIF export failed: {e:#}");
                }
            }
        } else if !json {
            eprintln!("GIF export skipped: no recording was produced");
        }
    }

    if json {
        let json_str =
            serde_json::to_string_pretty(&result).context("failed serializing playbook result")?;
        println!("{json_str}");
    } else {
        print!("{}", crate::playbook::format_result(&result));
    }

    Ok(if result.pass { 0 } else { 1 })
}

fn run_playbook_validate(source: &str, json: bool) -> Result<u8> {
    let playbook = if source == "-" {
        crate::playbook::parse_stdin().context("failed parsing playbook from stdin")?
    } else {
        crate::playbook::parse_file(std::path::Path::new(source))
            .with_context(|| format!("failed parsing playbook from {source}"))?
    };

    let errors = crate::playbook::validate(&playbook, false);

    if json {
        let report = serde_json::json!({
            "valid": errors.is_empty(),
            "errors": errors,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if errors.is_empty() {
        println!("playbook is valid");
    } else {
        println!("playbook validation errors:");
        for error in &errors {
            println!("  - {error}");
        }
    }

    Ok(if errors.is_empty() { 0 } else { 1 })
}

async fn run_playbook_interactive(
    socket: Option<&str>,
    record: bool,
    viewport: &str,
    shell: Option<&str>,
    timeout: Option<u64>,
) -> Result<u8> {
    // Parse viewport string "COLSxROWS"
    let (cols, rows) = parse_viewport_string(viewport)?;

    let timeout_duration = timeout.map(Duration::from_secs);

    crate::playbook::interactive::run_interactive(
        socket,
        record,
        cols,
        rows,
        shell,
        timeout_duration,
    )
    .await
}

fn parse_viewport_string(viewport: &str) -> Result<(u16, u16)> {
    let parts: Vec<&str> = viewport.split('x').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid viewport format: expected COLSxROWS (e.g. 80x24), got '{viewport}'");
    }
    let cols: u16 = parts[0]
        .parse()
        .with_context(|| format!("invalid viewport cols: '{}'", parts[0]))?;
    let rows: u16 = parts[1]
        .parse()
        .with_context(|| format!("invalid viewport rows: '{}'", parts[1]))?;
    if cols < 10 || rows < 5 {
        anyhow::bail!("viewport too small (minimum 10x5): {cols}x{rows}");
    }
    Ok((cols, rows))
}

fn run_playbook_from_recording(recording_id: &str, output: Option<&str>) -> Result<u8> {
    let recordings = recording::list_recordings_from_dir(&recording::recordings_root_dir())?;
    let resolved_id = recording::resolve_recording_id_prefix(recording_id, &recordings)?;
    let events = recording::load_recording_events(&resolved_id.to_string())?;
    let playbook_dsl = crate::playbook::from_recording::events_to_playbook(&events)?;

    if let Some(path) = output {
        std::fs::write(path, &playbook_dsl)
            .with_context(|| format!("failed writing playbook to {path}"))?;
        println!("wrote playbook to {path}");
    } else {
        print!("{playbook_dsl}");
    }

    Ok(0)
}

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
    use crate::cli::{Cli, Command};
    use crate::input::InputProcessor;
    use crate::runtime::attach::state::AttachViewState;
    use bmux_client::{AttachLayoutState, AttachOpenInfo, ClientError};
    use bmux_config::{BmuxConfig, ConfigPaths, ResolvedTimeout};
    use bmux_ipc::transport::IpcTransportError;
    use bmux_ipc::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
        AttachViewComponent, ErrorCode, PaneLayoutNode, PaneSummary, RecordingSummary,
        SessionSummary,
    };
    use bmux_plugin::{PluginCommandEffect, PluginManifest, PluginRegistry};
    use crossterm::event::{
        KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
        KeyEventKind as CrosstermKeyEventKind, KeyModifiers,
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
            plugin_api: bmux_plugin::VersionRange::at_least(bmux_plugin::ApiVersion::new(1, 0)),
            native_abi: bmux_plugin::VersionRange::at_least(bmux_plugin::ApiVersion::new(1, 0)),
            entrypoint: bmux_plugin::PluginEntrypoint::Native {
                symbol: bmux_plugin::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            },
            description: None,
            homepage: None,
            provider_priority: 0,
            required_capabilities: std::collections::BTreeSet::from([bmux_plugin::HostScope::new(
                "bmux.commands",
            )
            .expect("capability should parse")]),
            provided_capabilities: std::collections::BTreeSet::from([bmux_plugin::HostScope::new(
                "example.provider.write",
            )
            .expect("capability should parse")]),
            provided_features: std::collections::BTreeSet::new(),
            services: vec![bmux_plugin::PluginService {
                capability: bmux_plugin::HostScope::new("example.provider.write")
                    .expect("capability should parse"),
                kind: bmux_plugin::ServiceKind::Command,
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
            plugin_api: bmux_plugin::VersionRange::at_least(bmux_plugin::ApiVersion::new(1, 0)),
            native_abi: bmux_plugin::VersionRange::at_least(bmux_plugin::ApiVersion::new(1, 0)),
            entrypoint: bmux_plugin::PluginEntrypoint::Native {
                symbol: bmux_plugin::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            },
            description: None,
            homepage: None,
            provider_priority: 0,
            required_capabilities: std::collections::BTreeSet::from([
                bmux_plugin::HostScope::new("bmux.commands").expect("capability should parse"),
                bmux_plugin::HostScope::new("example.base.read").expect("capability should parse"),
            ]),
            provided_capabilities: std::collections::BTreeSet::from([
                bmux_plugin::HostScope::new("example.provider.read")
                    .expect("capability should parse"),
                bmux_plugin::HostScope::new("example.provider.write")
                    .expect("capability should parse"),
            ]),
            provided_features: std::collections::BTreeSet::new(),
            services: vec![
                bmux_plugin::PluginService {
                    capability: bmux_plugin::HostScope::new("example.provider.read")
                        .expect("capability should parse"),
                    kind: bmux_plugin::ServiceKind::Query,
                    interface_id: "provider-query/v1".to_string(),
                },
                bmux_plugin::PluginService {
                    capability: bmux_plugin::HostScope::new("example.provider.write")
                        .expect("capability should parse"),
                    kind: bmux_plugin::ServiceKind::Command,
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
        assert_eq!(event.kind, bmux_plugin::PluginEventKind::System);
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
        assert_eq!(event.kind, bmux_plugin::PluginEventKind::Session);
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
