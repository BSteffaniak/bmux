use anyhow::{Context, Result};
use bmux_cli_schema::Cli;
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_ipc::InvokeServiceKind;
use bmux_plugin::PluginRegistry;
use bmux_plugin_sdk::{
    CORE_CLI_COMMAND_CAPABILITY, CORE_CLI_COMMAND_INTERFACE_V1,
    CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1, HostConnectionInfo, HostScope,
    PluginCliCommandRequest, PluginCliCommandResponse, RegisteredService, ServiceKind,
    ServiceRequest, decode_service_message, encode_service_message,
};
use bmux_server::{BmuxServer, ServiceInvokeContext};
use clap::Parser;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tracing::Level;

/// A factory that produces a connected [`BmuxClient`] on demand.
///
/// Used by the kernel bridge to reach the bmux server when the normal local-IPC
/// path is not viable (e.g. iroh remote connections where the server lives on a
/// different host).  Each invocation should return a fresh, independently-usable
/// client -- callers will run one request and drop it.
pub type KernelClientFactory =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<BmuxClient>> + Send>> + Send + Sync>;

use super::{
    ConnectionContext, build_runtime_command_state, dispatch::dispatch_built_in_command,
    effective_enabled_plugins, load_plugin, plugin_host_metadata, resolve_plugin_search_paths,
    run_keymap_doctor, run_logs_level, run_logs_path, run_plugin_keybinding_command,
    run_plugin_keybinding_command_with_state, run_terminal_install_terminfo,
};

thread_local! {
    // A caller-process command can borrow the invoking transport through a
    // bounded request pump. Never substitute a fresh connection on this route.
    static INVOKING_TRANSPORT: RefCell<Option<tokio::sync::mpsc::Sender<InvokingTransportRequest>>> = const { RefCell::new(None) };
    static SERVICE_KERNEL_CONTEXT: RefCell<Option<ServiceInvokeContext>> = const { RefCell::new(None) };
    static HOST_KERNEL_CONNECTION: RefCell<Option<HostConnectionInfo>> = const { RefCell::new(None) };
    static HOST_KERNEL_CLIENT_FACTORY: RefCell<Option<KernelClientFactory>> = const { RefCell::new(None) };
    static HOST_KERNEL_RESPONSE_REPLAY: RefCell<Option<(u64, Vec<u8>)>> = const { RefCell::new(None) };
}

static HOST_KERNEL_CONNECTION_FALLBACK: OnceLock<Mutex<Option<HostConnectionInfo>>> =
    OnceLock::new();
static HOST_KERNEL_CLIENT_FACTORY_FALLBACK: OnceLock<Mutex<Option<KernelClientFactory>>> =
    OnceLock::new();

struct RetainedInvocation {
    context: ServiceInvokeContext,
    owners: usize,
    cancellation: bmux_plugin_sdk::CancellationToken,
}
type RetainedInvocationRegistry = std::collections::BTreeMap<uuid::Uuid, RetainedInvocation>;
fn retained_kernel_contexts() -> &'static Mutex<RetainedInvocationRegistry> {
    static CONTEXTS: OnceLock<Mutex<RetainedInvocationRegistry>> = OnceLock::new();
    CONTEXTS.get_or_init(Mutex::default)
}

/// A host-owned registration whose authority ends with its owner, even when
/// blocking work still retains a bridge clone. Bound calls never reconnect.
pub(super) struct OwnedKernelContext(RetainedKernelContext);

impl OwnedKernelContext {
    pub(super) fn new(context: ServiceInvokeContext) -> Result<Self> {
        Ok(Self(RetainedKernelContext::register(context)?))
    }

    pub(super) fn bridge(&self) -> bmux_plugin_sdk::HostKernelBridge {
        self.0.bridge()
    }
}

impl Drop for OwnedKernelContext {
    fn drop(&mut self) {
        if let Ok(mut contexts) = retained_kernel_contexts().lock()
            && let Some(entry) = contexts.get_mut(&self.0.0)
        {
            entry.cancellation.cancel();
        }
    }
}

struct RetainedKernelContext(uuid::Uuid);
impl RetainedKernelContext {
    fn register(context: ServiceInvokeContext) -> Result<Self> {
        let token = uuid::Uuid::new_v4();
        let mut contexts = retained_kernel_contexts()
            .lock()
            .map_err(|_| anyhow::anyhow!("retained kernel context unavailable"))?;
        anyhow::ensure!(
            contexts.len() < 4096,
            "retained kernel context capacity exceeded"
        );
        contexts.insert(
            token,
            RetainedInvocation {
                context,
                owners: 1,
                cancellation: bmux_plugin_sdk::CancellationToken::default(),
            },
        );
        drop(contexts);
        Ok(Self(token))
    }
    fn cancellation(&self) -> bmux_plugin_sdk::CancellationToken {
        retained_kernel_contexts()
            .lock()
            .ok()
            .and_then(|contexts| {
                contexts
                    .get(&self.0)
                    .map(|entry| entry.cancellation.clone())
            })
            .unwrap_or_default()
    }
    fn bridge(&self) -> bmux_plugin_sdk::HostKernelBridge {
        bmux_plugin_sdk::HostKernelBridge::bound(
            host_kernel_bridge,
            self.0,
            retained_kernel_ownership,
        )
    }
}
impl Drop for RetainedKernelContext {
    fn drop(&mut self) {
        unsafe {
            retained_kernel_ownership(self.0.as_bytes().as_ptr(), false);
        }
    }
}

unsafe extern "C" fn retained_kernel_ownership(token: *const u8, retain: bool) -> bool {
    if token.is_null() {
        return false;
    }
    let bytes = unsafe { std::slice::from_raw_parts(token, 16) };
    let Ok(token) = uuid::Uuid::from_slice(bytes) else {
        return false;
    };
    let Ok(mut contexts) = retained_kernel_contexts().lock() else {
        return false;
    };
    let Some(entry) = contexts.get_mut(&token) else {
        return false;
    };
    if retain {
        let Some(next) = entry.owners.checked_add(1) else {
            return false;
        };
        entry.owners = next;
    } else {
        entry.owners -= 1;
        if entry.owners == 0 {
            contexts.remove(&token);
        }
    }
    true
}

pub(super) struct ServiceKernelContextGuard(Option<ServiceInvokeContext>);
pub(super) struct HostKernelConnectionGuard;
pub(super) struct HostKernelClientFactoryGuard;

pub(super) static EFFECTIVE_LOG_LEVEL: OnceLock<Level> = OnceLock::new();

pub(super) enum RuntimeLoggingHandle {
    Moosicbox {
        _handle: moosicbox_log_runtime::init::LoggingHandle,
    },
    Diagnostic {
        // Drop the non-blocking guard before finalizing the manifest handle.
        _guard: tracing_appender::non_blocking::WorkerGuard,
        handle: bmux_diagnostic_log::DiagnosticLogHandle,
    },
}

pub(super) static LOG_CONTROL: OnceLock<bmux_diagnostic_log::DiagnosticLogControl> =
    OnceLock::new();

impl Drop for ServiceKernelContextGuard {
    fn drop(&mut self) {
        SERVICE_KERNEL_CONTEXT.with(|slot| {
            *slot.borrow_mut() = self.0.take();
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
    let previous = SERVICE_KERNEL_CONTEXT.with(|slot| slot.borrow_mut().replace(context));
    ServiceKernelContextGuard(previous)
}

pub(super) fn enter_host_kernel_connection(
    connection: HostConnectionInfo,
) -> HostKernelConnectionGuard {
    set_host_kernel_connection_fallback(connection.clone());
    HOST_KERNEL_CONNECTION.with(|slot| {
        *slot.borrow_mut() = Some(connection);
    });
    HostKernelConnectionGuard
}

pub(super) fn enter_host_kernel_client_factory(
    factory: KernelClientFactory,
) -> HostKernelClientFactoryGuard {
    set_host_kernel_client_factory_fallback(Arc::clone(&factory));
    HOST_KERNEL_CLIENT_FACTORY.with(|slot| {
        *slot.borrow_mut() = Some(factory);
    });
    HostKernelClientFactoryGuard
}

fn set_host_kernel_connection_fallback(connection: HostConnectionInfo) {
    if let Ok(mut slot) = HOST_KERNEL_CONNECTION_FALLBACK
        .get_or_init(|| Mutex::new(None))
        .lock()
    {
        *slot = Some(connection);
    }
}

fn host_kernel_connection_fallback() -> Option<HostConnectionInfo> {
    HOST_KERNEL_CONNECTION_FALLBACK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
}

fn set_host_kernel_client_factory_fallback(factory: KernelClientFactory) {
    if let Ok(mut slot) = HOST_KERNEL_CLIENT_FACTORY_FALLBACK
        .get_or_init(|| Mutex::new(None))
        .lock()
    {
        *slot = Some(factory);
    }
}

fn host_kernel_client_factory_fallback() -> Option<KernelClientFactory> {
    HOST_KERNEL_CLIENT_FACTORY_FALLBACK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
}

#[cfg(test)]
fn clear_host_kernel_fallbacks_for_test() {
    if let Ok(mut slot) = HOST_KERNEL_CONNECTION_FALLBACK
        .get_or_init(|| Mutex::new(None))
        .lock()
    {
        *slot = None;
    }
    if let Ok(mut slot) = HOST_KERNEL_CLIENT_FACTORY_FALLBACK
        .get_or_init(|| Mutex::new(None))
        .lock()
    {
        *slot = None;
    }
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

pub(super) struct InvokingTransportRequest {
    pub request: bmux_ipc::Request,
    pub response: std::sync::mpsc::SyncSender<Result<bmux_ipc::Response>>,
}

pub(super) struct InvokingTransportGuard;

impl Drop for InvokingTransportGuard {
    fn drop(&mut self) {
        INVOKING_TRANSPORT.with(|slot| slot.borrow_mut().take());
    }
}

pub(super) fn enter_invoking_transport(
    sender: tokio::sync::mpsc::Sender<InvokingTransportRequest>,
) -> InvokingTransportGuard {
    INVOKING_TRANSPORT.with(|slot| *slot.borrow_mut() = Some(sender));
    InvokingTransportGuard
}

fn call_host_kernel_bridge_payload(payload: &[u8]) -> Result<Vec<u8>> {
    if let Some(sender) = INVOKING_TRANSPORT.with(|slot| slot.borrow().clone()) {
        let request = bmux_ipc::decode(payload).context("invalid invoking transport request")?;
        let (response, receiver) = std::sync::mpsc::sync_channel(1);
        sender
            .blocking_send(InvokingTransportRequest { request, response })
            .map_err(|_| anyhow::anyhow!("invoking transport closed"))?;
        let response = receiver
            .recv()
            .context("invoking transport response closed")??;
        return bmux_ipc::encode(&response).context("invalid invoking transport response");
    }
    if let Some(context) = SERVICE_KERNEL_CONTEXT.with(|slot| slot.borrow().clone()) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
                return tokio::task::block_in_place(|| {
                    handle.block_on(async { context.execute_raw(payload.to_vec()).await })
                });
            }
            let payload = payload.to_vec();
            return std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed creating retained bridge runtime")?;
                runtime.block_on(context.execute_raw(payload))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("retained bridge worker panicked"))?;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed creating kernel bridge runtime")?;
        runtime.block_on(async { context.execute_raw(payload.to_vec()).await })
    } else if let Some(factory) = HOST_KERNEL_CLIENT_FACTORY.with(|slot| slot.borrow().clone()) {
        call_host_kernel_via_factory(&factory, payload)
    } else if let Some(connection) = HOST_KERNEL_CONNECTION.with(|slot| slot.borrow().clone()) {
        call_host_kernel_via_client(&connection, payload)
    } else if let Some(factory) = host_kernel_client_factory_fallback() {
        call_host_kernel_via_factory(&factory, payload)
    } else if let Some(connection) = host_kernel_connection_fallback() {
        call_host_kernel_via_client(&connection, payload)
    } else {
        anyhow::bail!("no host kernel route is available")
    }
}

fn write_host_kernel_bridge_response(
    invocation_id: u64,
    encoded: Vec<u8>,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    unsafe {
        *output_len = encoded.len();
    }
    if invocation_id != 0 && (output_ptr.is_null() || encoded.len() > output_capacity) {
        HOST_KERNEL_RESPONSE_REPLAY.with(|cache| {
            *cache.borrow_mut() = Some((invocation_id, encoded));
        });
        return 4;
    }
    if output_ptr.is_null() || encoded.len() > output_capacity {
        return 4;
    }
    if invocation_id != 0 {
        HOST_KERNEL_RESPONSE_REPLAY.with(|cache| {
            if cache.borrow().as_ref().map(|(id, _)| *id) == Some(invocation_id) {
                cache.borrow_mut().take();
            }
        });
    }
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }
    0
}

fn enter_retained_kernel_context(
    token: Option<uuid::Uuid>,
) -> std::result::Result<Option<ServiceKernelContextGuard>, ()> {
    // Resolve before replay as well: stale registrations must not obtain cached
    // output. Never consult thread-local/fallback state for a bound request.
    let retained = if let Some(token) = token {
        match retained_kernel_contexts().lock().ok().and_then(|contexts| {
            contexts
                .get(&token)
                .filter(|entry| !entry.cancellation.is_cancelled())
                .map(|entry| entry.context.clone())
        }) {
            Some(context) => Some(context),
            None => return Err(()),
        }
    } else {
        None
    };
    let scope = retained.map(enter_service_kernel_context);
    if token.is_some() {
        let valid = bmux_ipc::encode(&bmux_ipc::Request::CurrentStreamProvenance)
            .ok()
            .and_then(|payload| call_host_kernel_bridge_payload(&payload).ok())
            .and_then(|bytes| bmux_ipc::decode::<bmux_ipc::Response>(&bytes).ok());
        if !matches!(
            valid,
            Some(bmux_ipc::Response::Ok(
                bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                    provenance: bmux_ipc::StreamProvenance::Local
                        | bmux_ipc::StreamProvenance::Admitted { .. }
                        | bmux_ipc::StreamProvenance::HostInternal
                }
            ))
        ) {
            return Err(());
        }
    }

    Ok(scope)
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

    let Ok(_retained_scope) = enter_retained_kernel_context(request.context_token) else {
        return 2;
    };

    if let Some(encoded) = HOST_KERNEL_RESPONSE_REPLAY.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|(id, _)| *id == request.invocation_id)
            .map(|(_, encoded)| encoded.clone())
    }) {
        return write_host_kernel_bridge_response(
            request.invocation_id,
            encoded,
            output_ptr,
            output_capacity,
            output_len,
        );
    }

    if let Ok(Some(command_request)) =
        bmux_plugin_sdk::decode_host_kernel_bridge_cli_command_payload(&request.payload)
    {
        let response = match run_core_built_in_command(&command_request) {
            Ok(exit_code) => bmux_plugin_sdk::CoreCliCommandResponse::new(exit_code),
            Err(error) => bmux_plugin_sdk::CoreCliCommandResponse::failed(1, error.to_string()),
        };
        let Ok(payload) = bmux_plugin_sdk::encode_service_message(&response) else {
            return 5;
        };
        let response = bmux_plugin_sdk::HostKernelBridgeResponse { payload };
        let Ok(encoded) = bmux_plugin_sdk::encode_service_message(&response) else {
            return 5;
        };

        return write_host_kernel_bridge_response(
            request.invocation_id,
            encoded,
            output_ptr,
            output_capacity,
            output_len,
        );
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

        return write_host_kernel_bridge_response(
            request.invocation_id,
            encoded,
            output_ptr,
            output_capacity,
            output_len,
        );
    }

    let response = match call_host_kernel_bridge_payload(&request.payload) {
        Ok(payload) => bmux_plugin_sdk::HostKernelBridgeResponse { payload },
        Err(_) => return 5,
    };

    let Ok(encoded) = bmux_plugin_sdk::encode_service_message(&response) else {
        return 5;
    };

    write_host_kernel_bridge_response(
        request.invocation_id,
        encoded,
        output_ptr,
        output_capacity,
        output_len,
    )
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
                Box::pin(dispatch_built_in_command(
                    &command,
                    ConnectionContext::new(cli.target.as_deref()),
                ))
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
                Box::pin(dispatch_built_in_command(
                    &command,
                    ConnectionContext::new(cli.target.as_deref()),
                ))
                .await
            })
            .map(i32::from)
    }
}

fn run_plugin_bridge_command(request: &PluginCliCommandRequest) -> Result<i32> {
    let execution = run_plugin_bridge_command_execution(request, None)?;
    Ok(execution.status)
}

fn run_plugin_bridge_command_execution(
    request: &PluginCliCommandRequest,
    caller_client_id: Option<uuid::Uuid>,
) -> Result<super::plugin_runtime::PluginCommandExecution> {
    run_plugin_keybinding_command(
        request.plugin_id.as_str(),
        request.command_name.as_str(),
        &request.arguments,
        None,
        caller_client_id,
    )
}

fn run_plugin_bridge_command_execution_with_state(
    state: &super::plugin_runtime::RuntimeCommandState,
    request: &PluginCliCommandRequest,
    caller_client_id: Option<uuid::Uuid>,
) -> Result<super::plugin_runtime::PluginCommandExecution> {
    run_plugin_keybinding_command_with_state(
        state,
        request.plugin_id.as_str(),
        request.command_name.as_str(),
        &request.arguments,
        None,
        caller_client_id,
    )
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
            capability: HostScope::new("bmux.storage").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "volatile-state-query/v1".to_string(),
            provider: bmux_plugin_sdk::ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.storage").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "volatile-state-command/v1".to_string(),
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
        config_dir_candidates: paths
            .config_dir_candidates()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let available_capability_names = available_capabilities
        .keys()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let command_state =
        build_runtime_command_state(config.clone(), paths.clone(), Arc::new(registry.clone()))?;
    let mut loaded_provider_cache: BTreeMap<String, Arc<bmux_plugin::LoadedPlugin>> =
        BTreeMap::new();
    let command_connection_info = connection_info.clone();

    server.register_checked_service_handler_with_metadata(
        CORE_CLI_COMMAND_CAPABILITY,
        InvokeServiceKind::Command,
        CORE_CLI_COMMAND_INTERFACE_V1,
        CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
        move |_route, invoke_context, payload| {
            let command_state = command_state.clone();
            let connection = command_connection_info.clone();
            async move {
                let _kernel_context_guard = enter_service_kernel_context(invoke_context.clone());
                let _host_kernel_connection_guard = enter_host_kernel_connection(connection);
                let checked = bmux_plugin_sdk::service_contract::is_checked(&payload);
                let payload = if checked {
                    bmux_plugin_sdk::service_contract::decode_for(
                        CORE_CLI_COMMAND_INTERFACE_V1,
                        bmux_plugin_sdk::cli_bridge::cli_command::SERVICE_CONTRACT,
                        &payload,
                    )?
                } else {
                    &payload
                };
                let request: PluginCliCommandRequest = decode_service_message(payload)?;
                if request.protocol_version != bmux_plugin_sdk::CORE_CLI_BRIDGE_PROTOCOL_V1 {
                    return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "unsupported CLI argv protocol version".into(),
                    }
                    .into());
                }
                let execution = run_plugin_bridge_command_execution_with_state(
                    &command_state,
                    &request,
                    Some(invoke_context.client_id().0),
                )?;
                let response = if execution.status == 0 {
                    PluginCliCommandResponse::new(execution.status)
                } else {
                    PluginCliCommandResponse::failed(
                        execution.status,
                        execution.outcome.error_message.clone().unwrap_or_else(|| {
                            format!("plugin exited with status {}", execution.status)
                        }),
                    )
                };
                let payload = encode_service_message(&response)?;
                let payload = if checked {
                    bmux_plugin_sdk::service_contract::encode_response(&payload)
                } else {
                    payload
                };
                Ok(bmux_server::ServiceInvokeOutput {
                    payload,
                    metadata: execution.outcome.metadata,
                })
            }
        },
    )?;

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

        let loaded_provider = if let Some(loaded) = loaded_provider_cache.get(&provider_plugin_id) {
            Arc::clone(loaded)
        } else {
            let host = plugin_host_metadata();
            let loaded = Arc::new(
                load_plugin(provider, &host, &available_capabilities, config).with_context(
                    || {
                        format!(
                            "failed loading service provider plugin '{}'",
                            provider.declaration.id.as_str()
                        )
                    },
                )?,
            );
            loaded_provider_cache.insert(provider_plugin_id.clone(), Arc::clone(&loaded));
            loaded
        };
        let provider_declaration = provider.declaration.clone();
        let host = plugin_host_metadata();
        let services_for_handler = available_service_descriptors(config, registry)?;
        let capability_names_for_handler = available_capability_names.clone();
        let plugin_search_roots_for_handler = plugin_search_roots.clone();
        let config_for_handler = config.clone();
        let connection_info_for_handler = connection_info.clone();
        let enabled_plugins_for_handler = enabled_plugins.clone();

        server.register_checked_service_handler_with_metadata(
            service.capability.as_str().to_string(),
            invoke_kind,
            service.interface_id.clone(),
            "*",
            move |route, invoke_context, payload| {
                let loaded_provider = Arc::clone(&loaded_provider);
                let provider_declaration = provider_declaration.clone();
                let host = host.clone();
                let services = services_for_handler.clone();
                let capability_names = capability_names_for_handler.clone();
                let plugin_search_roots = plugin_search_roots_for_handler.clone();
                let config = config_for_handler.clone();
                let connection = connection_info_for_handler.clone();
                let enabled_plugins = enabled_plugins_for_handler.clone();
                async move {
                    let started_at = Instant::now();
                    let _kernel_context_guard =
                        enter_service_kernel_context(invoke_context.clone());
                    let _host_kernel_connection_guard =
                        enter_host_kernel_connection(connection.clone());
                    let retained = RetainedKernelContext::register(invoke_context.clone())?;
                    let response =
                        loaded_provider.invoke_service(&bmux_plugin_sdk::NativeServiceContext {
                            plugin_id: provider_declaration.id.as_str().to_string(),
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
                                        provider_declaration.id.as_str().to_string(),
                                    ),
                                },
                                operation: route.operation,
                                payload,
                            },
                            required_capabilities: provider_declaration
                                .required_capabilities
                                .iter()
                                .chain(provider_declaration.optional_capabilities.iter())
                                .map(ToString::to_string)
                                .collect(),
                            provided_capabilities: provider_declaration
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
                                .get(provider_declaration.id.as_str())
                                .cloned(),
                            plugin_settings_map: config.plugins.settings.clone(),
                            caller_client_id: Some(invoke_context.client_id().0),
                            cancellation: retained.cancellation(),
                            host_kernel_bridge: Some(retained.bridge()),
                        })?;
                    let elapsed_us = started_at.elapsed().as_micros();
                    tracing::trace!(
                        target: "bmux_cli::plugin_service",
                        plugin_id = provider_declaration.id.as_str(),
                        elapsed_us,
                        "plugin service handler dispatched"
                    );
                    if let Some(error) = response.error {
                        anyhow::bail!(error.message);
                    }

                    Ok(bmux_server::ServiceInvokeOutput {
                        payload: response.payload,
                        metadata: BTreeMap::new(),
                    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{MutexGuard, OnceLock};

    static HOST_KERNEL_FALLBACK_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn host_kernel_fallback_test_guard() -> MutexGuard<'static, ()> {
        HOST_KERNEL_FALLBACK_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("host kernel fallback test lock poisoned")
    }

    fn connection(label: &str) -> HostConnectionInfo {
        HostConnectionInfo {
            config_dir: format!("/{label}/config"),
            config_dir_candidates: vec![format!("/{label}/config")],
            runtime_dir: format!("/{label}/runtime"),
            data_dir: format!("/{label}/data"),
            state_dir: format!("/{label}/state"),
        }
    }

    fn invoke_test_bridge(
        bridge: Option<bmux_plugin_sdk::HostKernelBridge>,
        request: &bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload> {
        let bridge = bridge.unwrap();
        let mut envelope =
            bmux_plugin_sdk::HostKernelBridgeRequest::new(bmux_ipc::encode(request)?);
        envelope.context_token = bridge.token();
        let bytes = bmux_plugin_sdk::encode_service_message(&envelope)?;
        let mut output = vec![0; 65536];
        let mut len = 0;
        anyhow::ensure!(
            bridge.invoke(
                bytes.as_ptr(),
                bytes.len(),
                output.as_mut_ptr(),
                output.len(),
                &raw mut len
            ) == 0,
            "bridge denied"
        );
        let response: bmux_plugin_sdk::HostKernelBridgeResponse =
            bmux_plugin_sdk::decode_service_message(&output[..len])?;
        match bmux_ipc::decode(&response.payload)? {
            bmux_ipc::Response::Ok(payload) => Ok(payload),
            bmux_ipc::Response::Err(_) => anyhow::bail!("request denied"),
        }
    }

    fn check_retained_worker_lifetime(context: &ServiceInvokeContext) -> Result<()> {
        let retained = RetainedKernelContext::register(context.clone())?;
        let bridge = retained.bridge();
        let token = bridge.token().unwrap();
        let cancellation = retained.cancellation();
        let encoded = bmux_plugin_sdk::encode_service_message(&bridge)?;
        let captured: bmux_plugin_sdk::HostKernelBridge =
            bmux_plugin_sdk::decode_service_message(&encoded)?;
        drop(retained);
        drop(bridge);
        let captured = std::thread::spawn(move || {
            let response = invoke_test_bridge(
                Some(captured.clone()),
                &bmux_ipc::Request::CurrentStreamProvenance,
            )
            .unwrap();
            assert!(matches!(
                response,
                bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                    provenance: bmux_ipc::StreamProvenance::Local
                }
            ));
            captured
        })
        .join()
        .unwrap();
        let _ = cancellation;
        retained_kernel_contexts()
            .lock()
            .unwrap()
            .get_mut(&token)
            .unwrap()
            .cancellation
            .cancel();
        assert!(invoke_test_bridge(Some(captured.clone()), &bmux_ipc::Request::Ping).is_err());
        drop(captured);
        assert!(
            !retained_kernel_contexts()
                .lock()
                .unwrap()
                .contains_key(&token)
        );

        Ok(())
    }

    fn private_scope_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        bmux_ipc::transport::prepare_private_directory(directory.path(), true).unwrap();
        directory
    }

    #[tokio::test]
    async fn owned_host_bridge_is_in_process_isolated_and_revoked_on_drop() {
        let directory = private_scope_directory();
        // Neither server is started: any attempt to use transport would fail.
        let first = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            directory.path().join("a"),
        ));
        let second = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            directory.path().join("b"),
        ));
        for (server, value) in [(&first, 1_u8), (&second, 2_u8)] {
            server
                .register_service_handler(
                    "test.owner",
                    bmux_ipc::InvokeServiceKind::Query,
                    "owner/v1",
                    "read",
                    move |_, _, _| async move { Ok(vec![value]) },
                )
                .unwrap();
        }
        let first = OwnedKernelContext::new(first.host_service_context()).unwrap();
        let second = OwnedKernelContext::new(second.host_service_context()).unwrap();
        let first_bridge = first.bridge();
        let second_bridge = second.bridge();
        tokio::task::spawn_blocking(move || {
            // A bound bridge must ignore a different ambient owner.
            let second_context = retained_kernel_contexts().lock().unwrap()
                .get(&second_bridge.token().unwrap()).unwrap().context.clone();
            let _ambient = enter_service_kernel_context(second_context);
            for (bridge, value) in [(&first_bridge, 1), (&second_bridge, 2)] {
                assert!(matches!(
                    invoke_test_bridge(Some(bridge.clone()), &bmux_ipc::Request::CurrentStreamProvenance).unwrap(),
                    bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                        provenance: bmux_ipc::StreamProvenance::HostInternal
                    }
                ));
                let response = invoke_test_bridge(Some(bridge.clone()), &bmux_ipc::Request::InvokeService {
                    capability: "test.owner".into(),
                    kind: bmux_ipc::InvokeServiceKind::Query,
                    interface_id: "owner/v1".into(),
                    operation: "read".into(),
                    payload: Vec::new(),
                }).unwrap();
                assert!(matches!(response, bmux_ipc::ResponsePayload::ServiceInvoked { payload } if payload == vec![value]));
            }
            drop(first);
            assert!(invoke_test_bridge(Some(first_bridge), &bmux_ipc::Request::CurrentStreamProvenance).is_err());
            assert!(invoke_test_bridge(Some(second_bridge), &bmux_ipc::Request::CurrentStreamProvenance).is_ok());
            drop(second);
        }).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_service_kernel_scope_restores_bound_parent() {
        // Nix's TMPDIR can be long; keep the socket name short enough for
        // macOS's sockaddr_un rather than appending a UUID to the full path.
        let directory = private_scope_directory();
        let endpoint = bmux_ipc::IpcEndpoint::unix_socket(directory.path().join("private/s"));
        let server = BmuxServer::new(endpoint.clone());
        server
            .register_service_handler(
                "test.scope",
                bmux_ipc::InvokeServiceKind::Query,
                "scope/v1",
                "check",
                |_, context, _| async move {
                    assert!(SERVICE_KERNEL_CONTEXT.with(|slot| slot.borrow().is_none()));
                    let id = context.client_id();
                    check_retained_worker_lifetime(&context)?;

                    let parent = enter_service_kernel_context(context.clone());
                    {
                        let child = enter_service_kernel_context(context);
                        assert_eq!(
                            SERVICE_KERNEL_CONTEXT.with(|slot| slot
                                .borrow()
                                .as_ref()
                                .unwrap()
                                .client_id()),
                            id
                        );
                        drop(child);
                    }
                    assert_eq!(
                        SERVICE_KERNEL_CONTEXT.with(|slot| slot
                            .borrow()
                            .as_ref()
                            .unwrap()
                            .client_id()),
                        id
                    );
                    let response = call_host_kernel_bridge_payload(&bmux_ipc::encode(
                        &bmux_ipc::Request::CurrentStreamProvenance,
                    )?)?;
                    assert!(matches!(
                        bmux_ipc::decode::<bmux_ipc::Response>(&response)?,
                        bmux_ipc::Response::Ok(
                            bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                                provenance: bmux_ipc::StreamProvenance::Local
                            }
                        )
                    ));
                    drop(parent);
                    assert!(SERVICE_KERNEL_CONTEXT.with(|slot| slot.borrow().is_none()));
                    Ok(Vec::new())
                },
            )
            .unwrap();
        let mut task = tokio::spawn(async move { server.run().await });
        let mut peer = tokio::select! {
            result = &mut task => panic!("scope test server stopped before connection: {result:?}"),
            result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match bmux_ipc::transport::LocalIpcStream::connect(&endpoint).await {
                        Ok(peer) => break peer,
                        Err(error) => {
                            assert!(matches!(&error, bmux_ipc::transport::IpcTransportError::Io(source) if matches!(source.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)), "scope test connection failed: {error}");
                            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        }
                    }
                }
            }) => result.expect("scope test server did not become ready within five seconds"),
        };
        for (id, request) in [
            bmux_ipc::Request::Hello {
                contract: bmux_ipc::ProtocolContract::current(
                    bmux_ipc::default_supported_capabilities(),
                ),
                client_name: "scope-test".into(),
                principal_id: uuid::Uuid::new_v4(),
            },
            bmux_ipc::Request::InvokeService {
                capability: "test.scope".into(),
                kind: bmux_ipc::InvokeServiceKind::Query,
                interface_id: "scope/v1".into(),
                operation: "check".into(),
                payload: Vec::new(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            peer.send_envelope(&bmux_ipc::Envelope::new(
                id as u64,
                bmux_ipc::EnvelopeKind::Request,
                bmux_ipc::encode(&request).unwrap(),
            ))
            .await
            .unwrap();
            let response = peer.recv_envelope().await.unwrap();
            assert!(matches!(
                bmux_ipc::decode::<bmux_ipc::Response>(&response.payload).unwrap(),
                bmux_ipc::Response::Ok(_)
            ));
        }
        task.abort();
        let _ = task.await;
    }

    // Keep the ordered capture/revoke/cancel/release lifecycle together so ownership
    // and identity assertions can be audited against the same two live streams.
    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overlapping_retained_admitted_identities_remain_isolated() {
        use bmux_plugin::stream_admission::{
            StreamAdmissionAuthority, StreamAdmissionAuthorityHandle, StreamAdmissionBinding,
            StreamAdmissionError,
        };
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Authority {
            bindings: [Arc<StreamAdmissionBinding>; 2],
            revoked: [AtomicBool; 2],
        }
        impl StreamAdmissionAuthority for Authority {
            fn consume(
                &self,
                ticket: &[u8],
            ) -> Result<Arc<StreamAdmissionBinding>, StreamAdmissionError> {
                match ticket {
                    [index @ 0..=1] => Ok(self.bindings[usize::from(*index)].clone()),
                    _ => Err(StreamAdmissionError),
                }
            }
            fn validate(
                &self,
                binding: &StreamAdmissionBinding,
            ) -> Result<(), StreamAdmissionError> {
                self.bindings
                    .iter()
                    .position(|candidate| candidate.as_ref() == binding)
                    .filter(|index| !self.revoked[*index].load(Ordering::SeqCst))
                    .map(|_| ())
                    .ok_or(StreamAdmissionError)
            }
        }

        let directory = tempfile::Builder::new()
            .prefix("bmux-overlap-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = directory.path().join("private/s");
        let endpoint = bmux_ipc::IpcEndpoint::unix_socket(path.clone());
        let server = BmuxServer::new(endpoint.clone());
        let authority = Arc::new(Authority {
            bindings: std::array::from_fn(|_| {
                Arc::new(StreamAdmissionBinding::new(
                    uuid::Uuid::new_v4(),
                    uuid::Uuid::new_v4(),
                ))
            }),
            revoked: std::array::from_fn(|_| AtomicBool::new(false)),
        });
        server
            .plugin_state_registry()
            .register(&Arc::new(std::sync::RwLock::new(
                StreamAdmissionAuthorityHandle::new(authority.clone()),
            )));
        let (captured_tx, mut captured_rx) = tokio::sync::mpsc::unbounded_channel();
        server
            .approve_admitted_route(bmux_server::ServiceRoute {
                capability: "test.overlap".into(),
                kind: bmux_ipc::InvokeServiceKind::Query,
                interface_id: "overlap/v1".into(),
                operation: "capture".into(),
            })
            .unwrap();
        server
            .register_service_handler(
                "test.overlap",
                bmux_ipc::InvokeServiceKind::Query,
                "overlap/v1",
                "capture",
                move |_, context, _| {
                    let captured_tx = captured_tx.clone();
                    async move {
                        let guard = enter_service_kernel_context(context.clone());
                        let retained = RetainedKernelContext::register(context.clone())?;
                        let bridge = retained.bridge();
                        // Exercise the same serialization/retain path used by plugin capture.
                        let captured: bmux_plugin_sdk::HostKernelBridge =
                            bmux_plugin_sdk::decode_service_message(
                                &bmux_plugin_sdk::encode_service_message(&bridge)?,
                            )?;
                        captured_tx.send((captured, context)).unwrap();
                        drop(bridge);
                        drop(retained);
                        drop(guard);
                        Ok(Vec::new())
                    }
                },
            )
            .unwrap();
        let task = tokio::spawn(async move { server.run().await });
        let mut peers = Vec::new();
        let mut captures = Vec::new();
        for index in 0..2_u8 {
            let mut peer = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match bmux_ipc::transport::LocalIpcStream::connect(&endpoint).await {
                        Ok(peer) => break peer,
                        Err(_) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
                    }
                }
            })
            .await
            .unwrap();
            for (id, request) in [
                bmux_ipc::Request::AdmitStream {
                    admission_version: 1,
                    ticket: vec![index],
                },
                bmux_ipc::Request::Hello {
                    contract: bmux_ipc::ProtocolContract::current(
                        bmux_ipc::default_supported_capabilities(),
                    ),
                    client_name: "overlap-test".into(),
                    principal_id: authority.bindings[usize::from(index)].subject_id(),
                },
                bmux_ipc::Request::InvokeService {
                    capability: "test.overlap".into(),
                    kind: bmux_ipc::InvokeServiceKind::Query,
                    interface_id: "overlap/v1".into(),
                    operation: "capture".into(),
                    payload: Vec::new(),
                },
            ]
            .into_iter()
            .enumerate()
            {
                peer.send_envelope(&bmux_ipc::Envelope::new(
                    id as u64,
                    bmux_ipc::EnvelopeKind::Request,
                    bmux_ipc::encode(&request).unwrap(),
                ))
                .await
                .unwrap();
                if id != 0 {
                    let response = peer.recv_envelope().await.unwrap();
                    assert!(matches!(
                        bmux_ipc::decode::<bmux_ipc::Response>(&response.payload).unwrap(),
                        bmux_ipc::Response::Ok(_)
                    ));
                }
            }
            // The handler has returned; only captured ownership keeps its token alive.
            captures.push(captured_rx.recv().await.unwrap());
            peers.push(peer);
        }
        let tokens = captures
            .iter()
            .map(|(bridge, _)| bridge.token().unwrap())
            .collect::<Vec<_>>();
        assert_ne!(tokens[0], tokens[1]);
        assert_ne!(captures[0].1.client_id(), captures[1].1.client_id());
        let mut workers = Vec::new();
        let mut commands = Vec::new();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        for index in 0..2 {
            let own = captures[index].0.clone();
            let other = captures[1 - index].0.clone();
            let own_context = captures[index].1.clone();
            let other_context = captures[1 - index].1.clone();
            let expected = authority.bindings.clone();
            let done_tx = done_tx.clone();
            let (tx, rx) = std::sync::mpsc::channel::<[bool; 2]>();
            commands.push(tx);
            workers.push(std::thread::spawn(move || {
                for valid in rx {
                    assert!(SERVICE_KERNEL_CONTEXT.with(|slot| slot.borrow().is_none()));
                    let outer = enter_service_kernel_context(other_context.clone());
                    {
                        let inner = enter_service_kernel_context(own_context.clone());
                        // A nested bridge call must select its token, not ambient TLS.
                        for (bridge, subject_index) in [
                            (&other, 1 - index),
                            (&own, index),
                            (&other, 1 - index),
                            (&own, index),
                        ] {
                            let result = invoke_test_bridge(
                                Some(bridge.clone()),
                                &bmux_ipc::Request::CurrentStreamProvenance,
                            );
                            if valid[subject_index] {
                                assert_eq!(
                                    result.unwrap(),
                                    bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                                        provenance: bmux_ipc::StreamProvenance::Admitted {
                                            subject_id: expected[subject_index].subject_id(),
                                            lease_id: expected[subject_index].lease_id(),
                                        },
                                    }
                                );
                            } else {
                                assert!(result.is_err());
                            }
                            assert_eq!(
                                SERVICE_KERNEL_CONTEXT.with(|slot| slot
                                    .borrow()
                                    .as_ref()
                                    .unwrap()
                                    .client_id()),
                                own_context.client_id()
                            );
                        }
                        assert!(enter_retained_kernel_context(Some(uuid::Uuid::new_v4())).is_err());
                        drop(inner);
                    }
                    assert_eq!(
                        SERVICE_KERNEL_CONTEXT.with(|slot| slot
                            .borrow()
                            .as_ref()
                            .unwrap()
                            .client_id()),
                        other_context.client_id()
                    );
                    drop(outer);
                    assert!(SERVICE_KERNEL_CONTEXT.with(|slot| slot.borrow().is_none()));
                    done_tx.send(()).unwrap();
                }
            }));
        }
        for valid in [[true, true], [true, true], [false, true], [false, false]] {
            if !valid[0] {
                authority.revoked[0].store(true, Ordering::SeqCst);
            }
            if !valid[1] {
                retained_kernel_contexts()
                    .lock()
                    .unwrap()
                    .get_mut(&tokens[1])
                    .unwrap()
                    .cancellation
                    .cancel();
            }
            // Deterministic worker alternation exercises both identities after return.
            for command in &commands {
                command.send(valid).unwrap();
                done_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
        }
        drop(captures);
        assert!(tokens.iter().all(|token| {
            retained_kernel_contexts()
                .lock()
                .unwrap()
                .contains_key(token)
        }));
        drop(commands);
        for worker in workers {
            worker.join().unwrap();
        }
        for token in tokens {
            assert!(
                !retained_kernel_contexts()
                    .lock()
                    .unwrap()
                    .contains_key(&token)
            );
            assert!(enter_retained_kernel_context(Some(token)).is_err());
        }
        drop(peers);
        task.abort();
        let _ = task.await;
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn host_kernel_response_replay_returns_oversized_response_without_reexecution() {
        HOST_KERNEL_RESPONSE_REPLAY.with(|cache| cache.borrow_mut().take());
        let invocation_id = 42;
        let encoded = vec![7_u8; 2_048];
        let mut required = 0;
        let mut small = vec![0_u8; 32];
        assert_eq!(
            write_host_kernel_bridge_response(
                invocation_id,
                encoded.clone(),
                small.as_mut_ptr(),
                small.len(),
                &raw mut required,
            ),
            4
        );
        assert_eq!(required, encoded.len());
        let cached = HOST_KERNEL_RESPONSE_REPLAY
            .with(|cache| {
                cache
                    .borrow()
                    .as_ref()
                    .filter(|(id, _)| *id == invocation_id)
                    .map(|(_, encoded)| encoded.clone())
            })
            .expect("response should be cached");
        assert_eq!(cached, encoded);

        let mut output = vec![0_u8; required];
        assert_eq!(
            write_host_kernel_bridge_response(
                invocation_id,
                cached,
                output.as_mut_ptr(),
                output.len(),
                &raw mut required,
            ),
            0
        );
        assert_eq!(output, encoded);
        assert!(HOST_KERNEL_RESPONSE_REPLAY.with(|cache| cache.borrow().is_none()));
    }

    #[test]
    fn host_kernel_connection_fallback_outlives_thread_local_guard() {
        let _lock = host_kernel_fallback_test_guard();
        clear_host_kernel_fallbacks_for_test();
        {
            let _guard = enter_host_kernel_connection(connection("async-plugin"));
            assert_eq!(
                HOST_KERNEL_CONNECTION.with(|slot| slot.borrow().clone()),
                Some(connection("async-plugin")),
            );
        }

        assert!(HOST_KERNEL_CONNECTION.with(|slot| slot.borrow().is_none()));
        assert_eq!(
            host_kernel_connection_fallback(),
            Some(connection("async-plugin")),
        );
    }

    #[test]
    fn host_kernel_client_factory_fallback_outlives_thread_local_guard() {
        let _lock = host_kernel_fallback_test_guard();
        clear_host_kernel_fallbacks_for_test();
        let factory: KernelClientFactory = Arc::new(|| Box::pin(async { unreachable!() }));
        let expected = Arc::as_ptr(&factory);

        {
            let _guard = enter_host_kernel_client_factory(Arc::clone(&factory));
            let current = HOST_KERNEL_CLIENT_FACTORY.with(|slot| slot.borrow().clone());
            assert!(current.is_some());
        }

        assert!(HOST_KERNEL_CLIENT_FACTORY.with(|slot| slot.borrow().is_none()));
        let fallback = host_kernel_client_factory_fallback().expect("fallback factory is retained");
        assert!(std::ptr::addr_eq(Arc::as_ptr(&fallback), expected));
    }
}
