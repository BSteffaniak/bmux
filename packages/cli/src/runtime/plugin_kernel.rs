use anyhow::{Context, Result};
use bmux_cli_schema::Cli;
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_ipc::InvokeServiceKind;
use bmux_plugin::PluginRegistry;
use bmux_plugin_sdk::{
    CORE_CLI_COMMAND_CAPABILITY, CORE_CLI_COMMAND_INTERFACE_V1, HostConnectionInfo, HostScope,
    PluginCliCommandRequest, PluginCliCommandResponse, RegisteredService, ServiceKind,
    ServiceRequest,
};
use bmux_server::{BmuxServer, ServiceInvokeContext};
use clap::Parser;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use tracing::Level;

/// A factory that produces a connected [`BmuxClient`] on demand.
///
/// Used by the kernel bridge to reach the bmux server when the normal local-IPC
/// path is not viable (e.g. iroh remote connections where the server lives on a
/// different host).  Each invocation should return a fresh, independently-usable
/// client -- callers will run one request and drop it.
pub(super) type KernelClientFactory =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<BmuxClient>> + Send>> + Send + Sync>;

use super::{
    ConnectionContext, dispatch::dispatch_built_in_command, effective_enabled_plugins, load_plugin,
    plugin_host_metadata, resolve_plugin_search_paths, run_keymap_doctor, run_logs_level,
    run_logs_path, run_plugin_keybinding_command, run_recording_path,
    run_terminal_install_terminfo,
};

thread_local! {
    static SERVICE_KERNEL_CONTEXT: RefCell<Option<ServiceInvokeContext>> = const { RefCell::new(None) };
    static HOST_KERNEL_CONNECTION: RefCell<Option<HostConnectionInfo>> = const { RefCell::new(None) };
    static HOST_KERNEL_CLIENT_FACTORY: RefCell<Option<KernelClientFactory>> = const { RefCell::new(None) };
}

pub(super) struct ServiceKernelContextGuard;
pub(super) struct HostKernelConnectionGuard;
pub(super) struct HostKernelClientFactoryGuard;

pub(super) static EFFECTIVE_LOG_LEVEL: OnceLock<Level> = OnceLock::new();

pub(super) static LOG_WRITER_GUARD: OnceLock<moosicbox_log_runtime::init::LoggingHandle> =
    OnceLock::new();

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

impl Drop for HostKernelClientFactoryGuard {
    fn drop(&mut self) {
        HOST_KERNEL_CLIENT_FACTORY.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

pub(super) fn enter_service_kernel_context(
    context: ServiceInvokeContext,
) -> ServiceKernelContextGuard {
    SERVICE_KERNEL_CONTEXT.with(|slot| {
        *slot.borrow_mut() = Some(context);
    });
    ServiceKernelContextGuard
}

pub(super) fn enter_host_kernel_connection(
    connection: HostConnectionInfo,
) -> HostKernelConnectionGuard {
    HOST_KERNEL_CONNECTION.with(|slot| {
        *slot.borrow_mut() = Some(connection);
    });
    HostKernelConnectionGuard
}

pub(super) fn enter_host_kernel_client_factory(
    factory: KernelClientFactory,
) -> HostKernelClientFactoryGuard {
    HOST_KERNEL_CLIENT_FACTORY.with(|slot| {
        *slot.borrow_mut() = Some(factory);
    });
    HostKernelClientFactoryGuard
}

// Cross-domain mutations flow through typed plugin-to-plugin dispatch
// rather than a plugin-command-effect side-channel; the attach runtime
// observes context changes by comparing before/after `current-context`
// around a plugin command invocation.

pub(super) fn call_host_kernel_via_client(
    connection: &HostConnectionInfo,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let request: bmux_ipc::Request =
        bmux_ipc::decode(payload).context("failed decoding kernel bridge request payload")?;
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
    bmux_ipc::encode(&response).context("failed encoding kernel bridge response payload")
}

fn call_host_kernel_via_factory(factory: &KernelClientFactory, payload: &[u8]) -> Result<Vec<u8>> {
    let request: bmux_ipc::Request =
        bmux_ipc::decode(payload).context("failed decoding kernel bridge request payload")?;
    let factory = Arc::clone(factory);
    let response: bmux_ipc::Response = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                let mut client = factory()
                    .await
                    .context("remote kernel bridge client factory failed")?;
                client
                    .request_raw(request.clone())
                    .await
                    .context("remote kernel bridge request failed")
            })
        })
    } else {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed creating kernel bridge runtime")?;
        runtime.block_on(async {
            let mut client = factory()
                .await
                .context("remote kernel bridge client factory failed")?;
            client
                .request_raw(request.clone())
                .await
                .context("remote kernel bridge request failed")
        })
    }?;
    bmux_ipc::encode(&response).context("failed encoding kernel bridge response payload")
}

pub(super) unsafe extern "C" fn host_kernel_bridge(
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

    if let Ok(Some(command_request)) =
        bmux_plugin_sdk::decode_host_kernel_bridge_cli_command_payload(&request.payload)
    {
        let response = match run_core_built_in_command(&command_request) {
            Ok(exit_code) => bmux_plugin_sdk::CoreCliCommandResponse::new(exit_code),
            Err(_) => return 5,
        };
        let Ok(payload) = bmux_plugin_sdk::encode_service_message(&response) else {
            return 5;
        };
        let response = bmux_plugin_sdk::HostKernelBridgeResponse { payload };
        let Ok(encoded) = bmux_plugin_sdk::encode_service_message(&response) else {
            return 5;
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
        return 0;
    }

    if let Ok(Some(command_request)) =
        bmux_plugin_sdk::decode_host_kernel_bridge_plugin_command_payload(&request.payload)
    {
        let response = match run_plugin_bridge_command(&command_request) {
            Ok(exit_code) => PluginCliCommandResponse::new(exit_code),
            Err(error) => PluginCliCommandResponse::failed(1, error.to_string()),
        };
        let Ok(payload) = bmux_plugin_sdk::encode_service_message(&response) else {
            return 5;
        };
        let response = bmux_plugin_sdk::HostKernelBridgeResponse { payload };
        let Ok(encoded) = bmux_plugin_sdk::encode_service_message(&response) else {
            return 5;
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
        return 0;
    }

    let payload = if let Some(context) = SERVICE_KERNEL_CONTEXT.with(|slot| slot.borrow().clone()) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(async { context.execute_raw(request.payload).await })
            })
        } else {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return 5;
            };
            runtime.block_on(async { context.execute_raw(request.payload).await })
        }
    } else if let Some(factory) = HOST_KERNEL_CLIENT_FACTORY.with(|slot| slot.borrow().clone()) {
        call_host_kernel_via_factory(&factory, &request.payload)
    } else if let Some(connection) = HOST_KERNEL_CONNECTION.with(|slot| slot.borrow().clone()) {
        call_host_kernel_via_client(&connection, &request.payload)
    } else {
        return 5;
    };

    let response = match payload {
        Ok(payload) => bmux_plugin_sdk::HostKernelBridgeResponse { payload },
        Err(_) => return 5,
    };

    let Ok(encoded) = bmux_plugin_sdk::encode_service_message(&response) else {
        return 5;
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

fn run_core_built_in_command(request: &bmux_plugin_sdk::CoreCliCommandRequest) -> Result<i32> {
    if let Some(result) = run_core_built_in_command_fast_path(request)? {
        return Ok(result);
    }

    let mut argv = Vec::with_capacity(2 + request.command_path.len() + request.arguments.len());
    argv.push("bmux".to_string());
    argv.push("--core-builtins-only".to_string());
    argv.extend(request.command_path.clone());
    argv.extend(request.arguments.clone());

    let cli = Cli::try_parse_from(argv).context("failed parsing core built-in command")?;
    let command = cli.command.ok_or_else(|| {
        anyhow::anyhow!("core built-in command path did not resolve to a command")
    })?;

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                dispatch_built_in_command(&command, ConnectionContext::new(cli.target.as_deref()))
                    .await
            })
        })
        .map(i32::from)
    } else {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed creating runtime for core built-in command")?;
        runtime
            .block_on(async {
                dispatch_built_in_command(&command, ConnectionContext::new(cli.target.as_deref()))
                    .await
            })
            .map(i32::from)
    }
}

fn run_plugin_bridge_command(request: &PluginCliCommandRequest) -> Result<i32> {
    let status = run_plugin_keybinding_command(
        request.plugin_id.as_str(),
        request.command_name.as_str(),
        &request.arguments,
        None,
    )?
    .status;
    Ok(status)
}

fn run_core_built_in_command_fast_path(
    request: &bmux_plugin_sdk::CoreCliCommandRequest,
) -> Result<Option<i32>> {
    let path = request.command_path.as_slice();
    match path {
        [logs, path] if logs == "logs" && path == "path" => {
            let as_json = parse_json_only_flag(&request.arguments)?;
            return run_sync_built_in(|| run_logs_path(as_json)).map(Some);
        }
        [logs, level] if logs == "logs" && level == "level" => {
            let as_json = parse_json_only_flag(&request.arguments)?;
            return run_sync_built_in(|| run_logs_level(as_json)).map(Some);
        }
        [keymap, doctor] if keymap == "keymap" && doctor == "doctor" => {
            let as_json = parse_json_only_flag(&request.arguments)?;
            return run_sync_built_in(|| run_keymap_doctor(as_json)).map(Some);
        }
        [recording, path] if recording == "recording" && path == "path" => {
            let as_json = parse_json_only_flag(&request.arguments)?;
            return run_sync_built_in(|| run_recording_path(as_json)).map(Some);
        }
        [terminal, install_terminfo]
            if terminal == "terminal" && install_terminfo == "install-terminfo" =>
        {
            let (yes, check_only) = parse_install_terminfo_flags(&request.arguments)?;
            return run_sync_built_in(|| run_terminal_install_terminfo(yes, check_only)).map(Some);
        }
        _ => {}
    }
    Ok(None)
}

fn parse_json_only_flag(arguments: &[String]) -> Result<bool> {
    match arguments {
        [] => Ok(false),
        [flag] if flag == "--json" => Ok(true),
        _ => anyhow::bail!("unsupported arguments for bridged core command"),
    }
}

fn parse_install_terminfo_flags(arguments: &[String]) -> Result<(bool, bool)> {
    let mut yes = false;
    let mut check_only = false;
    for flag in arguments {
        match flag.as_str() {
            "--yes" => yes = true,
            "--check" => check_only = true,
            _ => anyhow::bail!("unsupported arguments for bridged core command"),
        }
    }
    Ok((yes, check_only))
}

fn run_sync_built_in<F>(f: F) -> Result<i32>
where
    F: FnOnce() -> Result<u8>,
{
    f().map(i32::from)
}

pub(super) fn core_provided_capabilities() -> Vec<HostScope> {
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

pub(super) fn core_service_descriptors() -> Vec<RegisteredService> {
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
            capability: HostScope::new(CORE_CLI_COMMAND_CAPABILITY)
                .expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: CORE_CLI_COMMAND_INTERFACE_V1.to_string(),
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

pub(super) fn available_capability_providers(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<std::collections::BTreeMap<HostScope, bmux_plugin::CapabilityProvider>> {
    let enabled_plugins = effective_enabled_plugins(config, registry);
    registry
        .capability_providers_for(&enabled_plugins, &core_provided_capabilities())
        .context("failed resolving capability providers")
}

pub(super) fn available_service_descriptors(
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

pub(super) const fn invoke_kind_from_service_kind(kind: ServiceKind) -> Option<InvokeServiceKind> {
    match kind {
        ServiceKind::Query => Some(InvokeServiceKind::Query),
        ServiceKind::Command => Some(InvokeServiceKind::Command),
        ServiceKind::Event => None,
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn register_plugin_service_handlers(
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
                            caller_client_id: Some(invoke_context.client_id().0),
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

pub(super) fn service_descriptors_from_declarations<'a>(
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
