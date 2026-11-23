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
    effective_enabled_plugins, plugin_host_metadata, resolve_plugin_search_paths,
    run_keymap_doctor, run_logs_level, run_logs_path, run_plugin_keybinding_command,
    run_plugin_keybinding_command_with_state, run_terminal_install_terminfo,
};

/// Caller id the host uses for invocations it originates itself (CLI,
/// attach, server internals) rather than on a plugin's behalf.
const HOST_CALLER_PLUGIN_ID: &str = "bmux.core";

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

/// Attach a host-authenticated plugin attribution to a typed service request
/// when the connected server negotiated it. Servers that did not negotiate
/// [`bmux_ipc::CAPABILITY_SERVICE_CALLER_PLUGIN_V1`] get the plain request,
/// which they attribute to the host.
///
/// `attribution` must come from host state bound to an admitted invocation
/// scope (the selected instance's declaration), never from plugin payloads.
fn attribute_kernel_request(
    negotiated: Option<&bmux_ipc::NegotiatedProtocol>,
    request: bmux_ipc::Request,
    attribution: Option<&str>,
) -> bmux_ipc::Request {
    let supported = negotiated.is_some_and(|protocol| {
        protocol
            .capabilities
            .iter()
            .any(|capability| capability == bmux_ipc::CAPABILITY_SERVICE_CALLER_PLUGIN_V1)
    });
    match (attribution, request) {
        (
            Some(caller_plugin_id),
            bmux_ipc::Request::InvokeService {
                capability,
                kind,
                interface_id,
                operation,
                payload,
            },
        ) if supported => bmux_ipc::Request::InvokeServiceAsPlugin {
            caller_plugin_id: caller_plugin_id.to_string(),
            capability,
            kind,
            interface_id,
            operation,
            payload,
        },
        (_, request) => request,
    }
}

fn call_host_kernel_via_client(connection: &HostConnectionInfo, payload: &[u8]) -> Result<Vec<u8>> {
    let request: bmux_ipc::Request =
        bmux_ipc::decode(payload).context("failed decoding kernel bridge request payload")?;
    let paths = ConfigPaths::new(
        connection.config_dir.clone().into(),
        connection.runtime_dir.clone().into(),
        connection.data_dir.clone().into(),
        connection.state_dir.clone().into(),
    );
    let request_name = "bmux-cli-host-kernel-bridge".to_string();
    let run = || async {
        let mut client = BmuxClient::connect_with_paths(&paths, &request_name).await?;
        client.request_raw(request.clone()).await
    };
    let response: bmux_ipc::Response = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(run()))
    } else {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed creating kernel bridge runtime")?;
        runtime.block_on(run())
    }?;
    bmux_ipc::encode(&response).context("failed encoding kernel bridge response payload")
}

fn call_host_kernel_via_factory(factory: &KernelClientFactory, payload: &[u8]) -> Result<Vec<u8>> {
    let request: bmux_ipc::Request =
        bmux_ipc::decode(payload).context("failed decoding kernel bridge request payload")?;
    let factory = Arc::clone(factory);
    let run = || async {
        let mut client = factory()
            .await
            .context("remote kernel bridge client factory failed")?;
        client
            .request_raw(request.clone())
            .await
            .context("remote kernel bridge request failed")
    };
    let response: bmux_ipc::Response = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(run()))
    } else {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed creating kernel bridge runtime")?;
        runtime.block_on(run())
    }?;
    bmux_ipc::encode(&response).context("failed encoding kernel bridge response payload")
}

/// Command-local transport authority. The issuing owner pumps this bounded
/// channel on its already-selected authenticated connection; no callback can
/// reconnect, create `HostInternal` authority, or substitute a payload principal.
type CommandScopes = Arc<
    Mutex<
        BTreeMap<
            bmux_plugin_runtime::RuntimeInvocationId,
            (bmux_plugin_runtime::InvocationScope, usize),
        >,
    >,
>;

#[derive(Clone)]
pub(in crate::runtime) struct CommandDispatch {
    sender: tokio::sync::mpsc::Sender<CommandDispatchRequest>,
    slots: Arc<tokio::sync::Semaphore>,
    scopes: CommandScopes,
    state: Option<Arc<super::plugin_runtime::RuntimeCommandState>>,
    // Weak: the nested command that receives this route may retain it after
    // its call ends. A strong lease would keep the issuing command's
    // invocation admitted forever, so the outer command could never settle.
    parent: Option<bmux_plugin::WeakCallbackEntryLease>,
    cancellation: bmux_plugin_sdk::ServiceCancellation,
}
pub(in crate::runtime) struct CommandDispatchOwner {
    scopes: CommandScopes,
    cancellation: bmux_plugin_sdk::ServiceCancellation,
}
impl Drop for CommandDispatchOwner {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let scopes = self
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|(scope, _)| scope.clone())
            .collect::<Vec<_>>();
        for scope in scopes {
            let _ = scope.close();
        }
    }
}
pub(in crate::runtime) struct CommandDispatchRequest {
    pub request: bmux_ipc::Request,
    /// Plugin the selected instance was loaded as. Host state captured from
    /// the admitted invocation's bound declaration; plugins cannot set it.
    caller_plugin_id: Option<String>,
    response: std::sync::mpsc::SyncSender<bmux_plugin_sdk::Result<Vec<u8>>>,
    registered: bool,
    _admission: bmux_plugin_runtime::AdmissionGuard,
    invocation: bmux_plugin_runtime::InvocationScope,
    scopes: CommandScopes,
    _parent: Option<bmux_plugin::CallbackEntryLease>,
    _slot: tokio::sync::OwnedSemaphorePermit,
    cancellation: bmux_plugin_sdk::ServiceCancellation,
}
impl Drop for CommandDispatchRequest {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let mut scopes = self
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, count)) = scopes.get_mut(&self.invocation.id()) {
            *count -= 1;
            if *count == 0 {
                scopes.remove(&self.invocation.id());
            }
        }
    }
}
impl CommandDispatchRequest {
    /// Request to send on the issuing connection: attributed to the calling
    /// plugin when the peer negotiated caller attribution, otherwise the
    /// plain (host-attributed) request.
    pub(in crate::runtime) fn wire_request(
        &self,
        negotiated: Option<&bmux_ipc::NegotiatedProtocol>,
    ) -> bmux_ipc::Request {
        attribute_kernel_request(
            negotiated,
            self.request.clone(),
            self.caller_plugin_id.as_deref(),
        )
    }

    /// Consume the sole request owner into the queue. Its destructor cannot run
    /// on this stack after a successful handoff; rejection drops that same owner.
    fn enqueue(mut self, sender: &tokio::sync::mpsc::Sender<Self>) -> bmux_plugin_sdk::Result<()> {
        {
            let mut scopes = self
                .scopes
                .lock()
                .map_err(|_| command_dispatch_error("command scope registry poisoned".into()))?;
            if self.cancellation.is_cancelled() {
                return Err(command_not_dispatched(
                    "selected command lifetime ended".into(),
                ));
            }
            let (_, count) = scopes
                .entry(self.invocation.id())
                .or_insert_with(|| (self.invocation.clone(), 0));
            *count += 1;
            self.registered = true;
            drop(scopes);
        }
        sender.try_send(self).map_err(|_| {
            command_not_dispatched(
                "selected command queue unavailable; request not dispatched".into(),
            )
        })
    }

    pub(in crate::runtime) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
    // Both rejections are definitive: the request never reached the issuing
    // connection, so no effect began. The typed classification lets callers
    // (including native ports) report them without an uncertain outcome.
    pub(in crate::runtime) fn reject(self) {
        let _ = self.response.clone().send(Err(command_not_dispatched(
            "command ended or cancelled before dispatch".into(),
        )));
        drop(self);
    }
    pub(in crate::runtime) fn reject_unavailable(self, reason: &str) {
        let _ = self
            .response
            .clone()
            .send(Err(command_not_dispatched(reason.to_owned())));
        drop(self);
    }
    pub(in crate::runtime) fn respond(self, result: Result<bmux_ipc::Response>) {
        if result.is_err() {
            // A transport failure is not proof that the server effect settled.
            // Keep cleanup-required scope/code ownership, never authorize finish
            // or replay from a lost response. Definitive server denials below
            // are settled outcomes and do not enter this uncertain state.
            let _ = self
                .invocation
                .fail_cleanup("selected command transport outcome unknown; do not replay".into());
        }
        let result = result
            .map_err(|error| command_dispatch_error(error.to_string()))
            .and_then(|response| match response {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ServiceInvoked { payload }) => {
                    Ok(payload)
                }
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                    provenance,
                }) => bmux_plugin_sdk::encode_service_message(&provenance),
                bmux_ipc::Response::Err(error) => Err(command_dispatch_error(format!(
                    "selected command request denied: {error:?}"
                ))),
                bmux_ipc::Response::Ok(_) => Err(command_dispatch_error(
                    "unexpected selected command response".into(),
                )),
            });
        // Ownership (including admission) remains in this request until the
        // actual response settles, even if the synchronous observer disappeared.
        let _ = self.response.clone().send(result);
        drop(self);
    }
}
const fn command_dispatch_error(details: String) -> bmux_plugin_sdk::PluginError {
    bmux_plugin_sdk::PluginError::ServiceProtocol { details }
}
/// Definitive rejection before the request reached the issuing connection.
const fn command_not_dispatched(details: String) -> bmux_plugin_sdk::PluginError {
    bmux_plugin_sdk::PluginError::RejectedBeforeDispatch { details }
}
impl CommandDispatch {
    pub(in crate::runtime) fn channel() -> (
        Self,
        CommandDispatchOwner,
        tokio::sync::mpsc::Receiver<CommandDispatchRequest>,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let cancellation = bmux_plugin_sdk::ServiceCancellation::new();
        let scopes = Arc::new(Mutex::new(BTreeMap::new()));
        (
            Self {
                sender,
                slots: Arc::new(tokio::sync::Semaphore::new(8)),
                scopes: scopes.clone(),
                state: None,
                parent: None,
                cancellation: cancellation.clone(),
            },
            CommandDispatchOwner {
                scopes,
                cancellation,
            },
            receiver,
        )
    }
    pub(in crate::runtime) fn with_state(
        mut self,
        state: super::plugin_runtime::RuntimeCommandState,
    ) -> Self {
        self.state = Some(Arc::new(state));
        self
    }
    /// The issuing callback entry while it is still running.
    pub(in crate::runtime) fn parent(&self) -> Option<bmux_plugin::CallbackEntryLease> {
        self.parent
            .as_ref()
            .and_then(bmux_plugin::WeakCallbackEntryLease::upgrade)
    }
    pub(in crate::runtime) fn with_parent(
        mut self,
        parent: &bmux_plugin::CallbackEntryLease,
    ) -> Self {
        self.parent = Some(parent.downgrade());
        self
    }
    /// Dispatch one typed service request on the issuing connection.
    ///
    /// `caller` is the plugin the admitted `invocation` belongs to, as bound
    /// by the host when it selected the instance; the pump forwards it as
    /// [`bmux_ipc::Request::InvokeServiceAsPlugin`] attribution.
    pub(in crate::runtime) fn invoke(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        caller: Option<&bmux_plugin::PluginId>,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        if self.cancellation.is_cancelled() {
            return Err(command_not_dispatched(
                "selected command lifetime ended".into(),
            ));
        }
        if request.payload.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
            return Err(command_dispatch_error(
                "selected command request exceeds bound".into(),
            ));
        }
        let kind = invoke_kind_from_service_kind(request.kind)
            .ok_or_else(|| command_dispatch_error("unsupported command service kind".into()))?;
        self.send(
            invocation,
            caller,
            bmux_ipc::Request::InvokeService {
                capability: request.capability.into(),
                kind,
                interface_id: request.interface.into(),
                operation: request.operation.into(),
                payload: request.payload.to_vec(),
            },
        )
    }

    /// Origin of the issuing command connection, answered by the server for
    /// that connection. The host builds the request; plugins cannot alter it.
    pub(in crate::runtime) fn stream_origin(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
    ) -> bmux_plugin_sdk::Result<bmux_ipc::StreamProvenance> {
        if self.cancellation.is_cancelled() {
            return Err(command_not_dispatched(
                "selected command lifetime ended".into(),
            ));
        }
        let encoded = self.send(invocation, None, bmux_ipc::Request::CurrentStreamProvenance)?;
        bmux_plugin_sdk::decode_service_message(&encoded)
    }

    fn send(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        caller: Option<&bmux_plugin::PluginId>,
        request: bmux_ipc::Request,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        let admission = invocation
            .try_admit()
            .map_err(|error| command_dispatch_error(error.to_string()))?;
        let (response, receiver) = std::sync::mpsc::sync_channel(1);
        CommandDispatchRequest {
            request,
            caller_plugin_id: caller.map(|caller| caller.as_str().to_owned()),
            response,
            registered: false,
            _admission: admission,
            invocation: invocation.clone(),
            scopes: self.scopes.clone(),
            _parent: self.parent(),
            _slot: self.slots.clone().try_acquire_owned().map_err(|_| {
                command_not_dispatched(
                    "selected command capacity exhausted; request not dispatched".into(),
                )
            })?,
            cancellation: self.cancellation.clone(),
        }
        .enqueue(&self.sender)?;
        receiver.recv().map_err(|_| {
            command_dispatch_error(
                "selected command response lost; outcome unknown, do not replay".into(),
            )
        })?
    }
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

tokio::task_local! {
    // Set only by the selected host dispatch below, never decoded from requests.
    static SELECTED_PLUGIN_CALLER: String;
    // Marks selected work a host callback thread is blocked waiting for: a callback that work
    // needs must not wait for a pool slot (see `HostCallbackExecutor::submit`). Set only by
    // the selected host dispatch below, never decoded from requests.
    static AWAITED_BY_HOST_CALLBACK: ();
}

/// Server-authenticated authority retained for one provider dispatch. Only the
/// service-handler entrypoint constructs this value; serialized plugin context
/// fields neither create nor replace it. Moving it to a blocking worker retains
/// the original internal/client principal and stream-provenance semantics.
#[derive(Clone)]
pub(in crate::runtime) struct CallbackDependency {
    // Weak on purpose: the server context of a nested call can be retained by
    // the callee (e.g. for a launched pane). A strong lease there would keep
    // the caller's invocation admitted until the callee let go, so the
    // caller's call could never finish draining.
    pub entry: bmux_plugin::WeakCallbackEntryLease,
}

#[derive(Clone)]
pub(in crate::runtime) struct AuthenticatedServiceInvocation {
    caller_plugin: Option<String>,
    context: bmux_server::ServiceInvokeContext,
    runtime: tokio::runtime::Handle,
    cancellation: bmux_plugin_sdk::ServiceCancellation,
}

struct CancelProviderOnDrop(bmux_plugin_sdk::ServiceCancellation);
impl Drop for CancelProviderOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

thread_local! {
    // Execution-location guard only, never a kernel eligibility grant or origin.
    static IN_BLOCKING_PLUGIN_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // Set only at persistent host-thread startup, never by a callback guard.
    // These threads execute synchronous jobs and never poll Tokio futures.
    static ON_HOST_CALLBACK_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct BlockingPluginWorkerGuard(bool);
impl BlockingPluginWorkerGuard {
    fn enter() -> Self {
        Self(IN_BLOCKING_PLUGIN_WORKER.with(|active| active.replace(true)))
    }
}
impl Drop for BlockingPluginWorkerGuard {
    fn drop(&mut self) {
        IN_BLOCKING_PLUGIN_WORKER.with(|active| active.set(self.0));
    }
}

type SelectedRequestJob = Box<dyn FnOnce() + Send + 'static>;

/// The bounded pool host callbacks run on (synchronous plugin callbacks never run on a Tokio
/// worker).
///
/// A callback may synchronously call a service whose provider needs a callback of its own,
/// blocking its slot until that one finishes. So a callback some host callback is waiting for
/// (nested) never waits for a slot: with the pool full of the callbacks waiting for it, it
/// would wait forever. It runs on an overflow thread instead, bounded by the depth of the
/// calls. Any other callback waits for a free slot, at most [`HOST_CALLBACK_SLOT_WAIT`], so a
/// burst (a server restoring every pane at once) queues rather than fails.
struct HostCallbackExecutor {
    sender: std::sync::mpsc::SyncSender<SelectedRequestJob>,
    slots: Arc<tokio::sync::Semaphore>,
}

/// How long a callback no other callback waits for may wait for a free slot.
const HOST_CALLBACK_SLOT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

impl HostCallbackExecutor {
    fn get() -> bmux_plugin_sdk::Result<&'static Self> {
        static EXECUTOR: OnceLock<Result<HostCallbackExecutor, String>> = OnceLock::new();
        EXECUTOR
            .get_or_init(|| Self::start(16))
            .as_ref()
            .map_err(|details| bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: details.clone(),
            })
    }

    fn start(workers: usize) -> Result<Self, String> {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<SelectedRequestJob>(workers);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..workers {
            let receiver = receiver.clone();
            std::thread::Builder::new()
                .name(format!("bmux-host-callback-{index}"))
                .spawn(move || {
                    ON_HOST_CALLBACK_THREAD.with(|location| location.set(true));
                    loop {
                        let job = match receiver.lock() {
                            Ok(receiver) => receiver.recv(),
                            Err(_) => break,
                        };
                        let Ok(job) = job else { break };
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                })
                .map_err(|error| format!("host callback executor startup failed: {error}"))?;
        }
        Ok(Self {
            sender,
            slots: Arc::new(tokio::sync::Semaphore::new(workers)),
        })
    }

    /// Run `job` on a host callback thread: in a free slot, else (if a host callback waits for
    /// it) on an overflow thread, else once a slot frees, waiting on `runtime` at most `wait`.
    async fn submit(
        &self,
        runtime: &tokio::runtime::Handle,
        job: SelectedRequestJob,
        wait: std::time::Duration,
    ) -> bmux_plugin_sdk::Result<()> {
        let protocol = |details| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
        let nested = AWAITED_BY_HOST_CALLBACK.try_with(|()| ()).is_ok();
        let permit = match self.slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) if nested => return Self::overflow(job),
            Err(_) => {
                // Waited on the selected host reactor: the caller may be polling from a
                // runtime without a timer.
                let slots = self.slots.clone();
                runtime
                    .spawn(async move { tokio::time::timeout(wait, slots.acquire_owned()).await })
                    .await
                    .map_err(|_| {
                        protocol(
                            "host callback executor unavailable; callback not dispatched".into(),
                        )
                    })?
                    .map_err(|_| {
                        protocol("host callback executor saturated; callback not dispatched".into())
                    })?
                    .map_err(|_| {
                        protocol(
                            "host callback executor unavailable; callback not dispatched".into(),
                        )
                    })?
            }
        };
        self.sender
            .try_send(Box::new(move || {
                let _permit = permit;
                job();
            }))
            .map_err(|_| {
                protocol("host callback executor unavailable; callback not dispatched".into())
            })
    }

    /// Run `job` on a thread of its own, as a host callback thread.
    fn overflow(job: SelectedRequestJob) -> bmux_plugin_sdk::Result<()> {
        std::thread::Builder::new()
            .name("bmux-host-callback-nested".into())
            .spawn(move || {
                ON_HOST_CALLBACK_THREAD.with(|location| location.set(true));
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            })
            .map(drop)
            .map_err(|error| bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: format!(
                    "host callback overflow thread failed: {error}; callback not dispatched"
                ),
            })
    }
}

/// Build a queue-owned job. An unaccepted job simply drops its unused admission.
/// Once accepted, the admission moves with the effect future, never with the
/// response receiver. Dropping/timing out that receiver cannot cancel the job.
/// The selected runtime must remain alive until its admitted jobs have drained.
fn selected_effect_job(
    runtime: tokio::runtime::Handle,
    admission: Option<bmux_plugin_runtime::AdmissionGuard>,
    work: impl Future<Output = ()> + Send + 'static,
) -> SelectedRequestJob {
    Box::new(move || {
        // Spawn rather than poll here: the dedicated dispatcher never blocks on
        // domain callbacks and cannot deadlock recursive selected dispatch.
        runtime.spawn(async move {
            let _admission = admission;
            work.await;
        });
    })
}

struct SelectedRequestExecutor {
    sender: std::sync::mpsc::SyncSender<SelectedRequestJob>,
    slots: Arc<tokio::sync::Semaphore>,
}

impl SelectedRequestExecutor {
    fn get() -> bmux_plugin_sdk::Result<&'static Self> {
        static EXECUTOR: std::sync::OnceLock<Result<SelectedRequestExecutor, String>> =
            std::sync::OnceLock::new();
        EXECUTOR
            .get_or_init(|| {
                const CAPACITY: usize = 64;
                let (sender, receiver) =
                    std::sync::mpsc::sync_channel::<SelectedRequestJob>(CAPACITY);
                std::thread::Builder::new()
                    .name("bmux-host-dispatch".into())
                    .spawn(move || {
                        while let Ok(job) = receiver.recv() {
                            debug_assert!(tokio::runtime::Handle::try_current().is_err());
                            // Losing a job response is explicitly uncertain, never a
                            // reason to kill the dispatcher or replay another effect.
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                        }
                    })
                    .map_err(|error| format!("selected host executor startup failed: {error}"))?;
                Ok(Self {
                    sender,
                    slots: Arc::new(tokio::sync::Semaphore::new(CAPACITY)),
                })
            })
            .as_ref()
            .map_err(|details| bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: details.clone(),
            })
    }
}

impl bmux_plugin::InvocationKernelDispatch for AuthenticatedServiceInvocation {
    fn execute(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: &[u8],
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        let protocol = |details| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
        if self.cancellation.is_cancelled() {
            return Err(protocol("authenticated kernel invocation cancelled".into()));
        }
        // Execution is handed to dispatch_selected_request's bounded selected
        // executor below. Do not require a CLI thread-local worker marker here:
        // the shared callback executor owns a different blocking thread and
        // already carries explicit scope admission. Placement is validated at
        // the executor seam; caller authority remains the retained server context.
        if request.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
            return Err(protocol(
                "kernel request exceeds negotiated buffer bound".into(),
            ));
        }
        // Caller identity is host-derived from the admitted scope; a plugin
        // kernel request may never assert one.
        if bmux_ipc::decode::<bmux_ipc::Request>(request)
            .as_ref()
            .is_ok_and(plugin_forbidden_kernel_request)
        {
            return Err(protocol(
                "plugins may not assert caller identity in kernel requests".into(),
            ));
        }
        let _admission = invocation
            .try_admit()
            .map_err(|error| protocol(error.to_string()))?;
        // execute_raw uses this exact authenticated server context. It preserves
        // HostInternal only when the server originally issued it, and performs
        // the same authoritative client/stream checks for external requests.
        // No bridge pointer, caller metadata or ambient connection is consulted.
        let encoded = self.dispatch_selected_request(request, Some(invocation))?;
        let response: bmux_ipc::Response =
            bmux_ipc::decode(&encoded).map_err(|error| protocol(error.to_string()))?;
        match response {
            bmux_ipc::Response::Ok(payload) => {
                let bytes =
                    bmux_ipc::encode(&payload).map_err(|error| protocol(error.to_string()))?;
                if bytes.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
                    return Err(protocol(
                        "kernel response exceeds bound; operation must not be replayed".into(),
                    ));
                }
                Ok(bytes)
            }
            bmux_ipc::Response::Err(error) => Err(protocol(format!(
                "authenticated kernel request denied: {error:?}"
            ))),
        }
    }
}

impl AuthenticatedServiceInvocation {
    pub(in crate::runtime) fn with_callback_parent(
        mut self,
        entry: &bmux_plugin::CallbackEntryLease,
    ) -> Self {
        self.context = self
            .context
            .with_host_dispatch_context(Arc::new(CallbackDependency {
                entry: entry.downgrade(),
            }));
        self
    }
    pub(in crate::runtime) fn callback_parent(
        &self,
    ) -> bmux_plugin_sdk::Result<Option<bmux_plugin::CallbackEntryLease>> {
        self.context
            .host_dispatch_context::<CallbackDependency>()
            .map(|context| context.and_then(|context| context.entry.upgrade()))
            .map_err(|error| command_dispatch_error(error.to_string()))
    }

    pub(in crate::runtime) fn selected_service_availability(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        key: &bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability> {
        let protocol = |details| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
        key.validate()?;
        let _admission = invocation
            .try_admit()
            .map_err(|error| protocol(error.to_string()))?;
        if self.cancellation.is_cancelled()
            || invocation
                .is_cancelled()
                .map_err(|error| protocol(error.to_string()))?
        {
            return Err(protocol("availability invocation cancelled".into()));
        }
        self.context
            .selected_service_availability(key)
            .map_err(|error| {
                protocol(format!(
                    "selected availability denied or unavailable: {error:#}"
                ))
            })
    }

    /// Selected transport dispatch for host-owned work outside any plugin
    /// invocation scope (e.g. a host watcher acting for its own plugin). The
    /// placement, cancellation and server authority checks are unchanged.
    pub(in crate::runtime) fn execute_response(
        &self,
        request: &[u8],
    ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
        self.execute_response_with_scope(request, None)
    }

    /// Retain an effect admission in the dispatcher job, independently of the
    /// synchronous response observer. This is the scoped plugin service seam.
    pub(in crate::runtime) fn execute_response_scoped(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: &[u8],
    ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
        self.execute_response_with_scope(request, Some(invocation))
    }

    /// Host-authenticated origin of this invocation's stream. The host builds
    /// the fixed request itself, so this is an invocation fact, never a kernel
    /// grant to the plugin asking.
    pub(in crate::runtime) fn stream_origin(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
    ) -> bmux_plugin_sdk::Result<bmux_ipc::StreamProvenance> {
        let request = bmux_ipc::encode(&bmux_ipc::Request::CurrentStreamProvenance)
            .map_err(|error| command_dispatch_error(error.to_string()))?;
        match self.execute_response_scoped(invocation, &request)? {
            bmux_ipc::ResponsePayload::CurrentStreamProvenance { provenance } => Ok(provenance),
            _ => Err(command_dispatch_error(
                "unexpected stream origin response".into(),
            )),
        }
    }

    fn execute_response_with_scope(
        &self,
        request: &[u8],
        invocation: Option<&bmux_plugin_runtime::InvocationScope>,
    ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
        let protocol = |details| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
        if self.cancellation.is_cancelled() {
            return Err(protocol("authenticated invocation cancelled".into()));
        }
        let bytes = self.dispatch_selected_request(request, invocation)?;
        match bmux_ipc::decode(&bytes).map_err(|error| protocol(error.to_string()))? {
            bmux_ipc::Response::Ok(payload) => Ok(payload),
            bmux_ipc::Response::Err(error) => {
                Err(protocol(format!("authenticated request denied: {error:?}")))
            }
        }
    }

    fn dispatch_selected_request(
        &self,
        request: &[u8],
        invocation: Option<&bmux_plugin_runtime::InvocationScope>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        let protocol = |details| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
        if request.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
            return Err(protocol("authenticated request exceeds bound".into()));
        }
        // Validate placement before admission or dispatch. No worker marker can
        // prove that a thread is outside an active Tokio future poll.
        let current = tokio::runtime::Handle::try_current().ok();
        let host_thread = ON_HOST_CALLBACK_THREAD.with(std::cell::Cell::get);
        if !host_thread
            && current.as_ref().is_some_and(|handle| {
                handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread
            })
        {
            return Err(protocol(
                "synchronous host dispatch requires async port on current-thread runtime".into(),
            ));
        }
        let executor = SelectedRequestExecutor::get()?;
        let permit = executor.slots.clone().try_acquire_owned().map_err(|_| {
            protocol("selected host executor saturated; request not dispatched".into())
        })?;
        // Admission belongs to the accepted job, not to recv_timeout below.
        // Closure racing this acquisition either rejects before enqueue or must
        // wait for the owned effect to settle. Queue rejection drops the unused
        // job and its admission without executing or replaying the request.
        let admission = invocation
            .map(bmux_plugin_runtime::InvocationScope::try_admit)
            .transpose()
            .map_err(|error| protocol(error.to_string()))?;
        let authority = self.clone();
        let request = request.to_vec();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let work = async move {
            let _permit = permit;
            let result = if authority.cancellation.is_cancelled() {
                Err(protocol("authenticated invocation cancelled".into()))
            } else {
                let execution = authority.context.execute_raw(request);
                let result = if let Some(caller) = authority.caller_plugin {
                    SELECTED_PLUGIN_CALLER.scope(caller, execution).await
                } else {
                    execution.await
                };
                result
                    .map_err(|error| protocol(format!("authenticated dispatch failed: {error:#}")))
            };
            let result = result.and_then(|bytes| {
                if bytes.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
                    Err(protocol(
                        "host response exceeds bound; outcome unknown, do not replay".into(),
                    ))
                } else {
                    Ok(bytes)
                }
            });
            let _ = sender.send(result);
        };
        // A host callback thread blocks below until this work ends, so any callback the work
        // needs must not wait for that thread's pool (see `HostCallbackExecutor`).
        let work: std::pin::Pin<Box<dyn Future<Output = ()> + Send>> = if host_thread {
            Box::pin(AWAITED_BY_HOST_CALLBACK.scope((), work))
        } else {
            Box::pin(work)
        };
        executor
            .sender
            .try_send(selected_effect_job(self.runtime.clone(), admission, work))
            .map_err(|_| {
                protocol("selected host executor unavailable; request not dispatched".into())
            })?;
        let wait = move || {
            receiver
                .recv_timeout(std::time::Duration::from_secs(30))
                .map_err(|_| {
                    protocol(
                        "selected host response unconfirmed; outcome unknown, do not replay".into(),
                    )
                })?
        };
        if current.is_some() && !host_thread {
            tokio::task::block_in_place(wait)
        } else {
            wait()
        }
    }

    pub(in crate::runtime) fn with_legacy_process_context<T>(&self, run: impl FnOnce() -> T) -> T {
        let _authority = enter_service_kernel_context(self.context.clone());
        run()
    }

    pub(in crate::runtime) fn with_plugin_caller(mut self, caller: &bmux_plugin::PluginId) -> Self {
        self.caller_plugin = Some(caller.as_str().to_string());
        self
    }

    pub(in crate::runtime) fn new(context: bmux_server::ServiceInvokeContext) -> Self {
        Self {
            caller_plugin: None,
            context,
            runtime: tokio::runtime::Handle::current(),
            cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        }
    }

    /// Trusted asynchronous host entry. This changes execution location only;
    /// the closure must still validate its captured invocation and selected
    /// authority on the blocking worker before performing any effect.
    ///
    /// `runtime` is the explicitly selected host reactor. It must never be
    /// taken from the polling thread: a plugin may poll host calls from its
    /// own short-lived runtime, and work scheduled there dies with it.
    pub(in crate::runtime) async fn dispatch_host_callback<T, F>(
        runtime: tokio::runtime::Handle,
        callback: F,
    ) -> bmux_plugin_sdk::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> bmux_plugin_sdk::Result<T> + Send + 'static,
    {
        let protocol = |details| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
        let executor = HostCallbackExecutor::get()?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let reactor = runtime.clone();
        let job: SelectedRequestJob = Box::new(move || {
            let _entered = runtime.enter();
            let _worker = BlockingPluginWorkerGuard::enter();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback))
                .unwrap_or_else(|_| {
                    Err(protocol(
                        "host callback panicked; outcome unknown, do not replay".into(),
                    ))
                });
            let _ = sender.send(result);
        });
        executor
            .submit(&reactor, job, HOST_CALLBACK_SLOT_WAIT)
            .await?;
        receiver.await.map_err(|_| {
            protocol("host callback response lost; outcome unknown, do not replay".into())
        })?
    }

    pub(in crate::runtime) fn cancellation(&self) -> bmux_plugin_sdk::ServiceCancellation {
        self.cancellation.clone()
    }

    /// Run existing synchronous callbacks only on a blocking worker. The
    /// context guard is worker-local and is never retained across async yields.
    pub(in crate::runtime) async fn dispatch(
        self,
        provider: super::plugin_runtime::ProductionPlugin,
        context: bmux_plugin_sdk::NativeServiceContext,
    ) -> Result<bmux_plugin_sdk::ServiceResponse> {
        let _cancel_on_observer_drop = CancelProviderOnDrop(self.cancellation.clone());
        let runtime = self.runtime.clone();
        Self::dispatch_host_callback(runtime, move || {
            provider.invoke_service_authenticated_sync(&context, self.clone())
        })
        .await
        .map_err(anyhow::Error::from)
    }
}

/// Route a raw kernel request. Bridge requests never carry plugin
/// attribution: plugin identity comes only from admitted invocation scopes on
/// the selected dispatch paths.
pub(super) fn call_host_kernel_bridge_payload(payload: &[u8]) -> Result<Vec<u8>> {
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

/// Kernel requests a plugin may never send through any kernel route: caller
/// identity assertions are constructed only by host code from admitted scope.
const fn plugin_forbidden_kernel_request(request: &bmux_ipc::Request) -> bool {
    matches!(request, bmux_ipc::Request::InvokeServiceAsPlugin { .. })
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

    let payload = match bridge_response_payload(&request.payload) {
        Ok(payload) => payload,
        Err(status) => return status,
    };
    let response = bmux_plugin_sdk::HostKernelBridgeResponse { payload };
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

/// Execute one bridge payload and return the response payload, or the bridge
/// status to fail with.
fn bridge_response_payload(payload: &[u8]) -> std::result::Result<Vec<u8>, i32> {
    if let Ok(Some(command_request)) =
        bmux_plugin_sdk::decode_host_kernel_bridge_cli_command_payload(payload)
    {
        let response = match run_core_built_in_command(&command_request) {
            Ok(exit_code) => bmux_plugin_sdk::CoreCliCommandResponse::new(exit_code),
            Err(error) => bmux_plugin_sdk::CoreCliCommandResponse::failed(1, error.to_string()),
        };
        return bmux_plugin_sdk::encode_service_message(&response).map_err(|_| 5);
    }

    if let Ok(Some(command_request)) =
        bmux_plugin_sdk::decode_host_kernel_bridge_plugin_command_payload(payload)
    {
        let response = match run_plugin_bridge_command(&command_request) {
            Ok(exit_code) => PluginCliCommandResponse::new(exit_code),
            Err(error) => PluginCliCommandResponse::failed(1, error.to_string()),
        };
        return bmux_plugin_sdk::encode_service_message(&response).map_err(|_| 5);
    }

    if bmux_ipc::decode::<bmux_ipc::Request>(payload)
        .as_ref()
        .is_ok_and(plugin_forbidden_kernel_request)
    {
        return Err(2);
    }
    call_host_kernel_bridge_payload(payload).map_err(|_| 5)
}

fn run_selected_core_command(
    state: &super::plugin_runtime::RuntimeCommandState,
    request: &bmux_plugin_sdk::CoreCliCommandRequest,
) -> Result<i32> {
    let mut argv = vec!["bmux".to_string(), "--core-builtins-only".to_string()];
    argv.extend(request.command_path.clone());
    argv.extend(request.arguments.clone());
    let parsed = Cli::try_parse_from(argv).context("failed parsing selected core command")?;
    match parsed.command.context("selected core command missing")? {
        bmux_cli_schema::Command::Terminal {
            command:
                bmux_cli_schema::TerminalCommand::Doctor {
                    json,
                    trace,
                    trace_limit,
                    trace_family,
                    trace_pane,
                },
        } => super::terminal_doctor::run_terminal_doctor_selected(
            state,
            json,
            trace,
            trace_limit,
            trace_family,
            trace_pane,
        )
        .map(i32::from),
        bmux_cli_schema::Command::Keymap {
            command: bmux_cli_schema::KeymapCommand::Doctor { json },
        } => super::terminal_doctor::run_keymap_doctor_selected(json, state).map(i32::from),
        bmux_cli_schema::Command::Terminal {
            command: bmux_cli_schema::TerminalCommand::InstallTerminfo { yes, check },
        } => super::terminal_doctor::run_terminal_install_terminfo_selected(
            yes,
            check,
            &state.config,
        )
        .map(i32::from),
        bmux_cli_schema::Command::Logs {
            command: bmux_cli_schema::LogsCommand::Path { json },
        } => super::logs_cli::run_logs_path_selected(json, &state.paths).map(i32::from),
        bmux_cli_schema::Command::Logs {
            command: bmux_cli_schema::LogsCommand::Level { json },
        } => run_logs_level(json).map(i32::from),
        command @ bmux_cli_schema::Command::Attach { .. } => {
            super::attach_selected::run_selected(state, &command)
        }
        _ => anyhow::bail!(
            "core command '{}' has no pinned runtime executor; ambient routing refused",
            request.command_path.join(" ")
        ),
    }
}

/// Host-private CLI bridge. Captures the already selected command state; this
/// is not a foundational kernel grant and never dispatches an IPC request.
pub(super) fn scoped_cli_executor() -> Result<bmux_plugin_sdk::PluginHostServices> {
    scoped_cli_executor_with_dispatch(None)
}

pub(super) fn scoped_cli_executor_with_dispatch(
    dispatch: Option<CommandDispatch>,
) -> Result<bmux_plugin_sdk::PluginHostServices> {
    let state = if let Some(dispatch) = &dispatch {
        dispatch
            .state
            .as_ref()
            .map(|state| (**state).clone())
            .ok_or_else(|| anyhow::anyhow!("selected command snapshot missing"))?
    } else {
        super::plugin_runtime::selected_command_state()?
    };
    Ok(bmux_plugin_sdk::PluginHostServices::new(move |request| {
        let protocol = |error: anyhow::Error| bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: format!("selected CLI bridge failed: {error:#}"),
        };
        if request.capability != CORE_CLI_COMMAND_CAPABILITY
            || request.kind != ServiceKind::Command
            || request.interface != CORE_CLI_COMMAND_INTERFACE_V1
        {
            return Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "builtin_cli",
            });
        }
        match request.operation {
            bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1 => {
                let command: bmux_plugin_sdk::CoreCliCommandRequest =
                    decode_service_message(request.payload)?;
                if command.protocol_version != bmux_plugin_sdk::CORE_CLI_BRIDGE_PROTOCOL_V1 {
                    return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "unsupported selected core CLI protocol version".into(),
                    });
                }
                let response = match run_selected_core_command(&state, &command) {
                    Ok(status) => bmux_plugin_sdk::CoreCliCommandResponse::new(status),
                    Err(error) => {
                        bmux_plugin_sdk::CoreCliCommandResponse::failed(1, format!("{error:#}"))
                    }
                };
                encode_service_message(&response)
            }
            CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1 => {
                let command: PluginCliCommandRequest = decode_service_message(request.payload)?;
                if command.protocol_version != bmux_plugin_sdk::CORE_CLI_BRIDGE_PROTOCOL_V1 {
                    return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "unsupported selected plugin CLI protocol version".into(),
                    });
                }
                let execution = dispatch
                    .as_ref()
                    .map_or_else(
                        || run_plugin_bridge_command_execution_with_state(&state, &command, None),
                        |dispatch| {
                            super::plugin_runtime::run_plugin_command_with_selected_dispatch(
                                &state,
                                &command,
                                dispatch.clone(),
                            )
                        },
                    )
                    .map_err(protocol)?;
                let response = if execution.status == 0 {
                    PluginCliCommandResponse::new(execution.status)
                } else {
                    PluginCliCommandResponse::failed(
                        execution.status,
                        execution.outcome.error_message.unwrap_or_else(|| {
                            format!(
                                "selected plugin command exited with status {}",
                                execution.status
                            )
                        }),
                    )
                };
                encode_service_message(&response)
            }
            _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "builtin_cli",
            }),
        }
    }))
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
                    None,
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
                    None,
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
    loaded_plugins: &Arc<Vec<super::plugin_runtime::ProductionPlugin>>,
    runtime_owner: Option<&crate::runtime_owner::RuntimeOwner>,
) -> Result<()> {
    let enabled_plugins = super::plugin_runtime::backend_enabled_plugins(config, registry)?;
    let available_capabilities = available_capability_providers(config, registry)?;
    let services = available_service_descriptors(config, registry)?
        .into_iter()
        .filter(|service| match &service.provider {
            bmux_plugin_sdk::ProviderId::Plugin(id) => enabled_plugins.contains(id),
            bmux_plugin_sdk::ProviderId::Host => true,
        })
        .collect::<Vec<_>>();
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
    let command_state = build_runtime_command_state(
        config.clone(),
        paths.clone(),
        Arc::new(registry.clone()),
        runtime_owner,
    )?;
    let command_connection_info = connection_info.clone();
    let command_instances = Arc::clone(loaded_plugins);

    server.register_checked_service_handler_with_metadata(
        CORE_CLI_COMMAND_CAPABILITY,
        InvokeServiceKind::Command,
        CORE_CLI_COMMAND_INTERFACE_V1,
        CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
        move |_route, invoke_context, payload| {
            let command_state = command_state.clone();
            let connection = command_connection_info.clone();
            let command_instances = Arc::clone(&command_instances);
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
                // The server authenticated this invocation; the command's host
                // services dispatch through exactly this context rather than
                // a cached lifecycle authority or ambient kernel context.
                let execution = super::plugin_runtime::run_plugin_command_with_service_authority(
                    &command_state,
                    &request,
                    Some(invoke_context.client_id().0),
                    AuthenticatedServiceInvocation::new(invoke_context.clone()),
                    &command_instances,
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

        let index = loaded_plugins
            .iter()
            .position(|loaded| loaded.declaration.id.as_str() == provider_plugin_id)
            .with_context(|| {
                format!("service provider '{provider_plugin_id}' has no active startup instance")
            })?;
        // Services retain the same startup instance, never a second factory
        // invocation whose state differs from activation and registrations.
        let loaded_provider = Arc::clone(loaded_plugins);
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
                    let authority = AuthenticatedServiceInvocation::new(invoke_context.clone());
                    // The caller is the plugin the server authenticated for this
                    // invocation, the plugin a selected host dispatch is acting
                    // for, or the host itself. Never decoded from the payload.
                    let caller_plugin_id = invoke_context.caller_plugin_id().map_or_else(
                        || {
                            SELECTED_PLUGIN_CALLER
                                .try_with(Clone::clone)
                                .unwrap_or_else(|_| HOST_CALLER_PLUGIN_ID.to_string())
                        },
                        ToString::to_string,
                    );
                    let provider = &loaded_provider[index];
                    let mut service_context = bmux_plugin_sdk::NativeServiceContext {
                        host_service_port: None,
                        plugin_id: provider_declaration.id.as_str().to_string(),
                        request: ServiceRequest {
                            caller_plugin_id,
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
                        // A host-issued context acts for no connected client;
                        // providers must not mistake its synthetic id for one.
                        caller_client_id: invoke_context
                            .connected_client_id()
                            .map(|client| client.0),
                        cancellation: bmux_plugin_sdk::CancellationToken::default(),
                        host_kernel_bridge: None,
                    };
                    let retained = RetainedKernelContext::register(invoke_context.clone())?;
                    service_context.cancellation = retained.cancellation();
                    service_context.host_kernel_bridge = Some(retained.bridge());
                    let response = provider.invoke_service(&service_context, authority).await?;
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

    fn selected_effect_scope() -> (
        bmux_plugin_runtime::InstanceScope,
        bmux_plugin_runtime::ActivationScope,
        bmux_plugin_runtime::InvocationScope,
    ) {
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
        );
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        (instance, activation, invocation)
    }

    async fn retire_selected_effect_scope(
        instance: bmux_plugin_runtime::InstanceScope,
        activation: bmux_plugin_runtime::ActivationScope,
        invocation: bmux_plugin_runtime::InvocationScope,
    ) {
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
        instance.drain().await.unwrap();
        instance.finish().unwrap();
    }

    fn assert_effect_drain_pending(invocation: &bmux_plugin_runtime::InvocationScope) {
        let mut drain = std::pin::pin!(invocation.drain());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            drain.as_mut().poll(&mut context),
            std::task::Poll::Pending
        ));
        // Dropping this drain observer must not drop accepted effect ownership.
    }

    #[tokio::test]
    async fn selected_effect_timeout_before_queue_start_retains_admission() {
        let (instance, activation, invocation) = selected_effect_scope();
        let admission = invocation.try_admit().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let called = calls.clone();
        let (response, observer) = std::sync::mpsc::sync_channel(1);
        let (settled, settlement) = tokio::sync::oneshot::channel();
        let job = selected_effect_job(
            tokio::runtime::Handle::current(),
            Some(admission),
            async move {
                called.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // The response observer may have disappeared; effect is not replayed.
                let _ = response.send(17);
                settled.send(()).unwrap();
            },
        );
        let (queue, queued) = std::sync::mpsc::sync_channel(1);
        assert!(queue.try_send(job).is_ok());
        assert!(matches!(
            observer.recv_timeout(std::time::Duration::ZERO),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(observer);
        assert_effect_drain_pending(&invocation);
        queued.recv().unwrap()();
        settlement.await.unwrap();
        retire_selected_effect_scope(instance, activation, invocation).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn selected_effect_observer_loss_and_cancellation_do_not_release_running_work() {
        let (instance, activation, invocation) = selected_effect_scope();
        let (started, start) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let (response, observer) = std::sync::mpsc::sync_channel(1);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let called = calls.clone();
        let job = selected_effect_job(
            tokio::runtime::Handle::current(),
            Some(invocation.try_admit().unwrap()),
            async move {
                started.send(()).unwrap();
                released.await.unwrap();
                called.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = response.send(23);
            },
        );
        job();
        start.await.unwrap();
        drop(observer);
        invocation.close().unwrap();
        assert!(invocation.is_cancelled().unwrap());
        assert!(matches!(
            invocation.try_admit(),
            Err(bmux_plugin_runtime::ScopeError::Closed)
        ));
        assert_effect_drain_pending(&invocation);
        activation.close().unwrap();
        assert!(matches!(
            activation.finish(),
            Err(bmux_plugin_runtime::ScopeError::Outstanding(_))
        ));
        release.send(()).unwrap();
        retire_selected_effect_scope(instance, activation, invocation).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn selected_effect_closed_scope_rejects_before_queueing() {
        let (instance, activation, invocation) = selected_effect_scope();
        invocation.close().unwrap();
        let (queue, receiver) = std::sync::mpsc::sync_channel::<SelectedRequestJob>(1);
        let admission = invocation.try_admit();
        assert!(matches!(
            admission,
            Err(bmux_plugin_runtime::ScopeError::Closed)
        ));
        // The production seam propagates this admission error before constructing
        // or submitting a job. Neither queue occupancy nor an effect is created.
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(queue);
        retire_selected_effect_scope(instance, activation, invocation).await;
    }

    #[tokio::test]
    async fn selected_effect_success_preserves_the_single_actual_response() {
        let (instance, activation, invocation) = selected_effect_scope();
        let (response, observer) = tokio::sync::oneshot::channel();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let called = calls.clone();
        let job = selected_effect_job(
            tokio::runtime::Handle::current(),
            Some(invocation.try_admit().unwrap()),
            async move {
                called.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                response.send(vec![3, 1, 4]).unwrap();
            },
        );
        job();
        assert_eq!(observer.await.unwrap(), vec![3, 1, 4]);
        retire_selected_effect_scope(instance, activation, invocation).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn selected_effect_queue_rejection_releases_only_unused_admission() {
        for disconnected in [false, true] {
            let (instance, activation, invocation) = selected_effect_scope();
            let (queue, receiver) = std::sync::mpsc::sync_channel::<SelectedRequestJob>(1);
            if disconnected {
                drop(receiver);
            } else {
                assert!(queue.try_send(Box::new(|| {})).is_ok());
                // Keep this receiver alive through rejection below.
                let job = selected_effect_job(
                    tokio::runtime::Handle::current(),
                    Some(invocation.try_admit().unwrap()),
                    async { panic!("rejected effect ran") },
                );
                assert!(matches!(
                    queue.try_send(job),
                    Err(std::sync::mpsc::TrySendError::Full(_))
                ));
                retire_selected_effect_scope(instance, activation, invocation).await;
                continue;
            }
            let job = selected_effect_job(
                tokio::runtime::Handle::current(),
                Some(invocation.try_admit().unwrap()),
                async { panic!("rejected effect ran") },
            );
            assert!(matches!(
                queue.try_send(job),
                Err(std::sync::mpsc::TrySendError::Disconnected(_))
            ));
            retire_selected_effect_scope(instance, activation, invocation).await;
        }
    }

    #[tokio::test]
    async fn selected_availability_closed_and_cancelled_invocations_are_errors() {
        let server = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            std::env::temp_dir().join("bmux-selected-availability-unit.sock"),
        ));
        let authority = AuthenticatedServiceInvocation::new(server.host_service_context());
        let key = bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey {
            capability: "example.read".into(),
            kind: ServiceKind::Query,
            interface_id: "example/v1".into(),
        };
        let (instance, activation, invocation) = selected_effect_scope();
        assert_eq!(
            authority
                .selected_service_availability(&invocation, &key)
                .unwrap(),
            bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability::Absent
        );
        authority.cancellation.cancel();
        assert!(
            authority
                .selected_service_availability(&invocation, &key)
                .is_err()
        );
        let authority = AuthenticatedServiceInvocation::new(server.host_service_context());
        invocation.close().unwrap();
        assert!(
            authority
                .selected_service_availability(&invocation, &key)
                .is_err()
        );
        retire_selected_effect_scope(instance, activation, invocation).await;
    }

    #[tokio::test]
    async fn command_local_dispatch_retains_delayed_effect_after_owner_cancellation() {
        let (instance, activation, invocation) = selected_effect_scope();
        let (dispatch, owner, mut requests) = CommandDispatch::channel();
        let scope = invocation.clone();
        let caller = tokio::task::spawn_blocking(move || {
            dispatch.invoke(
                &scope,
                None,
                &bmux_plugin_sdk::HostServiceRequest {
                    capability: "example.write",
                    kind: ServiceKind::Command,
                    interface: "example/v1",
                    operation: "mutate",
                    payload: b"once",
                },
            )
        });
        {
            let request = requests.recv().await.unwrap();
            assert!(!request.is_cancelled());
            assert!(
                matches!(&request.request, bmux_ipc::Request::InvokeService { payload, .. } if payload == b"once")
            );
            // The request is now owned by the actual connection dispatcher. Losing
            // its command observer may reject new input, never discard this effect.
            drop(owner);
            assert!(request.is_cancelled());
            assert_effect_drain_pending(&invocation);
            request.respond(Ok(bmux_ipc::Response::Ok(
                bmux_ipc::ResponsePayload::ServiceInvoked { payload: vec![9] },
            )));
        }
        assert_eq!(caller.await.unwrap().unwrap(), vec![9]);
        retire_selected_effect_scope(instance, activation, invocation).await;
    }

    #[tokio::test]
    async fn command_local_dispatch_predispatch_rejection_is_not_replayed_or_success() {
        let (instance, activation, invocation) = selected_effect_scope();
        let (dispatch, _owner, mut requests) = CommandDispatch::channel();
        let scope = invocation.clone();
        let caller = tokio::task::spawn_blocking(move || {
            dispatch.invoke(
                &scope,
                None,
                &bmux_plugin_sdk::HostServiceRequest {
                    capability: "example.write",
                    kind: ServiceKind::Command,
                    interface: "example/v1",
                    operation: "denied",
                    payload: &[],
                },
            )
        });
        {
            let request = requests.recv().await.unwrap();
            request.reject();
        }
        assert!(caller.await.unwrap().is_err());
        assert!(requests.try_recv().is_err());
        retire_selected_effect_scope(instance, activation, invocation).await;
    }

    #[tokio::test]
    async fn command_local_dispatch_unknown_transport_outcome_blocks_retirement() {
        let (instance, activation, invocation) = selected_effect_scope();
        let (dispatch, _owner, mut requests) = CommandDispatch::channel();
        let scope = invocation.clone();
        let caller = tokio::task::spawn_blocking(move || {
            dispatch.invoke(
                &scope,
                None,
                &bmux_plugin_sdk::HostServiceRequest {
                    capability: "example.write",
                    kind: ServiceKind::Command,
                    interface: "example/v1",
                    operation: "mutate",
                    payload: &[],
                },
            )
        });
        requests
            .recv()
            .await
            .unwrap()
            .respond(Err(anyhow::anyhow!("connection lost after dispatch")));
        assert!(caller.await.unwrap().is_err());
        assert!(matches!(
            invocation.drain().await,
            Err(bmux_plugin_runtime::ScopeError::Cleanup(_))
        ));
        assert!(matches!(
            invocation.finish(),
            Err(bmux_plugin_runtime::ScopeError::Cleanup(_))
        ));
        // Only this test knows the injected effect never ran; production cannot
        // infer this acknowledgement from a transport failure.
        invocation.resolve_cleanup().unwrap();
        retire_selected_effect_scope(instance, activation, invocation).await;
    }

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
            // SAFETY: the bridge came from this process's host; the request buffer is
            // readable for its length, `output` is writable for `output.len()` bytes and
            // the length out-parameter is a live local, all valid for the call.
            unsafe {
                bridge.invoke(
                    bytes.as_ptr(),
                    bytes.len(),
                    output.as_mut_ptr(),
                    output.len(),
                    &raw mut len,
                )
            } == 0,
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

    #[tokio::test]
    async fn selected_plugin_origin_is_scoped_and_not_request_metadata() {
        assert!(SELECTED_PLUGIN_CALLER.try_with(Clone::clone).is_err());
        SELECTED_PLUGIN_CALLER
            .scope("test.owner".to_string(), async {
                assert_eq!(SELECTED_PLUGIN_CALLER.with(Clone::clone), "test.owner");
                let nested = SELECTED_PLUGIN_CALLER
                    .scope("test.nested".to_string(), async {
                        SELECTED_PLUGIN_CALLER.with(Clone::clone)
                    })
                    .await;
                assert_eq!(nested, "test.nested");
                assert_eq!(SELECTED_PLUGIN_CALLER.with(Clone::clone), "test.owner");
            })
            .await;
        assert!(SELECTED_PLUGIN_CALLER.try_with(Clone::clone).is_err());
    }

    #[tokio::test]
    async fn selected_host_dispatch_preserves_owner_and_restores_task_origin() {
        let directory = private_scope_directory();
        let server = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            directory.path().join("s"),
        ));
        server
            .register_service_handler(
                "test.origin",
                InvokeServiceKind::Query,
                "origin/v1",
                "read",
                |_, _, _| async {
                    Ok(SELECTED_PLUGIN_CALLER
                        .try_with(Clone::clone)
                        .unwrap_or_else(|_| "unbound".to_string())
                        .into_bytes())
                },
            )
            .unwrap();
        let authority = AuthenticatedServiceInvocation::new(server.host_service_context())
            .with_plugin_caller(&bmux_plugin::PluginId::new("test.owner").unwrap());
        let request = bmux_ipc::encode(&bmux_ipc::Request::InvokeService {
            capability: "test.origin".into(),
            kind: InvokeServiceKind::Query,
            interface_id: "origin/v1".into(),
            operation: "read".into(),
            payload: vec![],
        })
        .unwrap();
        let response = AuthenticatedServiceInvocation::dispatch_host_callback(
            tokio::runtime::Handle::current(),
            move || authority.execute_response(&request),
        )
        .await
        .unwrap();
        assert!(
            matches!(response, bmux_ipc::ResponsePayload::ServiceInvoked { payload } if payload == b"test.owner")
        );
        assert!(SELECTED_PLUGIN_CALLER.try_with(Clone::clone).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn selected_kernel_scope_dispatch_does_not_require_cli_worker_tls() {
        use bmux_plugin::InvocationKernelDispatch;
        let directory = private_scope_directory();
        let server = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            directory.path().join("s"),
        ));
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = effects.clone();
        server
            .register_service_handler(
                "test.effect",
                InvokeServiceKind::Command,
                "effect/v1",
                "once",
                move |_, _, _| {
                    let observed = observed.clone();
                    async move {
                        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(vec![7])
                    }
                },
            )
            .unwrap();
        let authority = AuthenticatedServiceInvocation::new(server.host_service_context());
        let (instance, activation, invocation) = selected_effect_scope();
        let scope = invocation.clone();
        let request = bmux_ipc::encode(&bmux_ipc::Request::InvokeService {
            capability: "test.effect".into(),
            kind: InvokeServiceKind::Command,
            interface_id: "effect/v1".into(),
            operation: "once".into(),
            payload: vec![],
        })
        .unwrap();
        tokio::task::spawn_blocking(move || {
            assert!(!IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
            let result = authority.execute(&scope, &request).unwrap();
            assert!(
                matches!(bmux_ipc::decode::<bmux_ipc::ResponsePayload>(&result).unwrap(),
                bmux_ipc::ResponsePayload::ServiceInvoked { payload } if payload == vec![7])
            );
            scope.close().unwrap();
            assert!(authority.execute(&scope, &request).is_err());
        })
        .await
        .unwrap();
        assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 1);
        retire_selected_effect_scope(instance, activation, invocation).await;
    }

    #[tokio::test]
    async fn synchronous_host_dispatch_rejects_current_thread_before_submission() {
        let directory = private_scope_directory();
        let server = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            directory.path().join("s"),
        ));
        let authority = AuthenticatedServiceInvocation::new(server.host_service_context());
        // The callback guard is not physical placement evidence.
        let _misleading_worker = BlockingPluginWorkerGuard::enter();
        assert!(!ON_HOST_CALLBACK_THREAD.with(std::cell::Cell::get));
        let error = authority
            .execute_response(&bmux_ipc::encode(&bmux_ipc::Request::Ping).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("current-thread runtime"));
    }

    #[tokio::test]
    async fn persistent_callback_dispatch_supports_selected_current_thread_runtime() {
        let directory = private_scope_directory();
        let server = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            directory.path().join("s"),
        ));
        let authority = AuthenticatedServiceInvocation::new(server.host_service_context());
        let request = bmux_ipc::encode(&bmux_ipc::Request::CurrentStreamProvenance).unwrap();
        let response = AuthenticatedServiceInvocation::dispatch_host_callback(
            tokio::runtime::Handle::current(),
            move || {
                assert!(ON_HOST_CALLBACK_THREAD.with(std::cell::Cell::get));
                assert!(IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
                assert_eq!(
                    tokio::runtime::Handle::current().runtime_flavor(),
                    tokio::runtime::RuntimeFlavor::CurrentThread
                );
                authority.execute_response(&request)
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            response,
            bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                provenance: bmux_ipc::StreamProvenance::HostInternal,
            }
        ));
        assert!(!ON_HOST_CALLBACK_THREAD.with(std::cell::Cell::get));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_service_called_from_a_callback_gets_a_callback_with_the_pool_full() {
        // A server restoring its panes: every slot but one is busy, and the last callback
        // synchronously invokes a service whose provider needs a callback of its own.
        let directory = private_scope_directory();
        let server = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            directory.path().join("s"),
        ));
        server
            .register_service_handler(
                "test.nested",
                InvokeServiceKind::Query,
                "nested/v1",
                "read",
                |_, _, _| async {
                    AuthenticatedServiceInvocation::dispatch_host_callback(
                        tokio::runtime::Handle::current(),
                        || Ok(vec![9]),
                    )
                    .await
                    .map_err(anyhow::Error::from)
                },
            )
            .unwrap();
        let executor = HostCallbackExecutor::get().unwrap();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(Mutex::new(held));
        while executor.slots.available_permits() > 1 {
            let permit = executor.slots.clone().try_acquire_owned().unwrap();
            let held = held.clone();
            executor
                .sender
                .try_send(Box::new(move || {
                    let _permit = permit;
                    let _ = held.lock().unwrap().recv();
                }))
                .unwrap();
        }
        let authority = AuthenticatedServiceInvocation::new(server.host_service_context());
        let request = bmux_ipc::encode(&bmux_ipc::Request::InvokeService {
            capability: "test.nested".into(),
            kind: InvokeServiceKind::Query,
            interface_id: "nested/v1".into(),
            operation: "read".into(),
            payload: vec![],
        })
        .unwrap();
        let response = AuthenticatedServiceInvocation::dispatch_host_callback(
            tokio::runtime::Handle::current(),
            move || authority.execute_response(&request),
        )
        .await;
        drop(release);
        assert!(
            matches!(response, Ok(bmux_ipc::ResponsePayload::ServiceInvoked { ref payload }) if payload == &[9]),
            "{response:?}"
        );
    }

    /// A one-slot pool with its slot held until the returned sender is dropped.
    fn held_pool() -> (HostCallbackExecutor, std::sync::mpsc::Sender<()>) {
        let pool = HostCallbackExecutor::start(1).unwrap();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let permit = pool.slots.clone().try_acquire_owned().unwrap();
        pool.sender
            .try_send(Box::new(move || {
                let _permit = permit;
                let _ = held.recv();
            }))
            .unwrap();
        (pool, release)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_callback_waits_for_a_slot_rather_than_fail() {
        let (pool, release) = held_pool();
        let (ran, done) = tokio::sync::oneshot::channel();
        let job: SelectedRequestJob = Box::new(move || {
            let _ = ran.send(ON_HOST_CALLBACK_THREAD.with(std::cell::Cell::get));
        });
        let runtime = tokio::runtime::Handle::current();
        let submitted = pool.submit(&runtime, job, std::time::Duration::from_secs(30));
        let released = async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop(release);
        };
        let (submitted, ()) = tokio::join!(submitted, released);
        submitted.unwrap();
        assert!(done.await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_callback_fails_once_its_wait_for_a_slot_runs_out() {
        let (pool, _release) = held_pool();
        let error = pool
            .submit(
                &tokio::runtime::Handle::current(),
                Box::new(|| {}),
                std::time::Duration::from_millis(20),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("saturated"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_callback_a_host_callback_waits_for_never_waits_for_a_slot() {
        // The pool's only slot is held by a callback waiting for this one: waiting for a slot
        // would wait forever.
        let (pool, release) = held_pool();
        let (ran, done) = tokio::sync::oneshot::channel();
        let job: SelectedRequestJob = Box::new(move || {
            let _ = ran.send(ON_HOST_CALLBACK_THREAD.with(std::cell::Cell::get));
        });
        AWAITED_BY_HOST_CALLBACK
            .scope(
                (),
                pool.submit(
                    &tokio::runtime::Handle::current(),
                    job,
                    std::time::Duration::ZERO,
                ),
            )
            .await
            .unwrap();
        assert!(done.await.unwrap());
        drop(release);
    }

    #[test]
    fn selected_dispatcher_is_reused_and_runs_outside_tokio() {
        let executor = SelectedRequestExecutor::get().unwrap();
        assert!(std::ptr::eq(
            executor,
            SelectedRequestExecutor::get().unwrap()
        ));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        assert!(
            executor
                .sender
                .try_send(Box::new(move || {
                    assert!(tokio::runtime::Handle::try_current().is_err());
                    assert!(!IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
                    sender.send(std::thread::current().id()).unwrap();
                }))
                .is_ok()
        );
        let first = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        assert!(
            executor
                .sender
                .try_send(Box::new(move || {
                    sender.send(std::thread::current().id()).unwrap();
                }))
                .is_ok()
        );
        assert_eq!(
            first,
            receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        );
    }

    async fn check_nested_provenance_callbacks(context: ServiceInvokeContext) -> Result<()> {
        // Exercise a synchronous typed callback both in this service poll and
        // in a separately scheduled task. Neither may inherit worker TLS.
        let request = bmux_ipc::encode(&bmux_ipc::Request::CurrentStreamProvenance)?;
        assert!(!IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
        let direct =
            AuthenticatedServiceInvocation::new(context.clone()).execute_response(&request)?;
        assert!(matches!(
            direct,
            bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                provenance: bmux_ipc::StreamProvenance::HostInternal,
            }
        ));
        assert!(!IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
        let nested_context = context.clone();
        let nested_request = request.clone();
        let nested = tokio::spawn(async move {
            assert!(!IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
            let result = AuthenticatedServiceInvocation::new(nested_context)
                .execute_response(&nested_request);
            assert!(!IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
            result
        })
        .await??;
        assert!(matches!(
            nested,
            bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                provenance: bmux_ipc::StreamProvenance::HostInternal,
            }
        ));
        let origin = context.execute_raw(request).await?;
        assert!(matches!(
            bmux_ipc::decode::<bmux_ipc::Response>(&origin)?,
            bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                provenance: bmux_ipc::StreamProvenance::HostInternal,
            })
        ));
        Ok(())
    }

    /// Synchronous dispatch from a current-thread runtime must be rejected
    /// before dispatch: it cannot block in place without stalling its reactor.
    fn assert_current_thread_dispatch_rejected(
        authority: &AuthenticatedServiceInvocation,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: &[u8],
    ) {
        use bmux_plugin::InvocationKernelDispatch;
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let current_thread = tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap();
                    let rejected =
                        current_thread.block_on(async { authority.execute(invocation, request) });
                    assert!(
                        rejected.is_err(),
                        "current-thread executor must not perform synchronous dispatch"
                    );
                })
                .join()
                .unwrap();
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_worker_kernel_dispatch_retains_server_context_without_tls() {
        use bmux_plugin::InvocationKernelDispatch;
        use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
        let first_dir = private_scope_directory();
        let second_dir = private_scope_directory();
        let first = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            first_dir.path().join("private/s"),
        ));
        let second = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            second_dir.path().join("private/s"),
        ));
        for (server, value) in [(&first, 31_u8), (&second, 47_u8)] {
            server
                .register_service_handler(
                    "test.explicit",
                    InvokeServiceKind::Query,
                    "explicit/v1",
                    "read",
                    move |_, context, _| async move {
                        check_nested_provenance_callbacks(context).await?;
                        Ok(vec![value])
                    },
                )
                .unwrap();
        }
        let request = bmux_ipc::encode(&bmux_ipc::Request::InvokeService {
            capability: "test.explicit".into(),
            kind: InvokeServiceKind::Query,
            interface_id: "explicit/v1".into(),
            operation: "read".into(),
            payload: Vec::new(),
        })
        .unwrap();
        for (server, expected) in [(&first, 31_u8), (&second, 47_u8)] {
            let instance = InstanceScope::new(PluginRuntimeScopeId::new());
            let activation = instance
                .begin_activation(PluginConcurrencyConfig::Concurrent)
                .unwrap();
            activation.mark_active().unwrap();
            let invocation = activation.begin_invocation().unwrap();
            let authority = AuthenticatedServiceInvocation::new(server.host_service_context());
            let nested_authority = authority.clone();
            let nested_request = request.clone();
            let nested = tokio::task::spawn_blocking(move || {
                assert!(!IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
                let response = nested_authority.execute_response(&nested_request);
                assert!(!IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
                response
            })
            .await
            .unwrap()
            .unwrap();
            assert!(
                matches!(nested, bmux_ipc::ResponsePayload::ServiceInvoked { payload } if payload == vec![expected])
            );
            // Placement contract of dispatch_selected_request: a multi-thread
            // executor thread dispatches through block_in_place, while a
            // current-thread runtime cannot and must reject before dispatch.
            let in_place = authority.execute(&invocation, &request).unwrap();
            assert!(
                matches!(bmux_ipc::decode::<bmux_ipc::ResponsePayload>(&in_place).unwrap(),
                bmux_ipc::ResponsePayload::ServiceInvoked { payload } if payload == vec![expected])
            );
            assert_current_thread_dispatch_rejected(&authority, &invocation, &request);
            let request = request.clone();
            let cancelled_authority = authority.clone();
            let cancelled_request = request.clone();
            let output = tokio::task::spawn_blocking(move || {
                assert!(SERVICE_KERNEL_CONTEXT.with(|slot| slot.borrow().is_none()));
                let _worker = BlockingPluginWorkerGuard::enter();
                let result = authority.execute(&invocation, &request);
                assert!(IN_BLOCKING_PLUGIN_WORKER.with(std::cell::Cell::get));
                invocation.close().unwrap();
                invocation.finish().unwrap();
                result
            })
            .await
            .unwrap()
            .unwrap();
            assert!(
                matches!(bmux_ipc::decode::<bmux_ipc::ResponsePayload>(&output).unwrap(),
                bmux_ipc::ResponsePayload::ServiceInvoked { payload } if payload == vec![expected])
            );
            cancelled_authority.cancellation.cancel();
            assert!(
                cancelled_authority
                    .execute_response(&cancelled_request)
                    .is_err()
            );
            activation.close().unwrap();
            activation.drain().await.unwrap();
            activation.finish().unwrap();
        }
    }

    // One end-to-end scenario; splitting it would scatter the scope setup.
    #[allow(clippy::too_many_lines)]
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

    // ── Plugin identity: attribution and forgery ─────────────────────────

    fn caller_negotiated(supported: bool) -> bmux_ipc::NegotiatedProtocol {
        let mut capabilities = vec![bmux_ipc::CORE_CAPABILITY_SERVICE_INVOCATION.to_string()];
        if supported {
            capabilities.push(bmux_ipc::CAPABILITY_SERVICE_CALLER_PLUGIN_V1.to_string());
        }
        bmux_ipc::NegotiatedProtocol {
            wire_epoch: 0,
            revision: 0,
            capabilities,
        }
    }

    fn whoami_request() -> bmux_plugin_sdk::HostServiceRequest<'static> {
        bmux_plugin_sdk::HostServiceRequest {
            capability: "test.whoami",
            kind: ServiceKind::Query,
            interface: "whoami/v1",
            operation: "read",
            payload: b"payload",
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_route_attributes_service_calls_to_the_admitted_caller() {
        let (instance, activation, invocation) = selected_effect_scope();
        let (dispatch, _owner, mut requests) = CommandDispatch::channel();
        let caller = bmux_plugin::PluginId::new("example.native").unwrap();
        let worker = {
            let scope = invocation.clone();
            tokio::task::spawn_blocking(move || {
                dispatch.invoke(&scope, Some(&caller), &whoami_request())?;
                dispatch.invoke(&scope, None, &whoami_request())
            })
        };
        let mut seen = Vec::new();
        for _ in 0..2 {
            let request = requests.recv().await.expect("dispatched request");
            seen.push((
                request.wire_request(Some(&caller_negotiated(true))),
                request.wire_request(Some(&caller_negotiated(false))),
                request.wire_request(None),
            ));
            request.respond(Ok(bmux_ipc::Response::Ok(
                bmux_ipc::ResponsePayload::ServiceInvoked {
                    payload: Vec::new(),
                },
            )));
        }
        worker.await.unwrap().unwrap();
        let plain = |request: &bmux_ipc::Request| matches!(request, bmux_ipc::Request::InvokeService { capability, .. } if capability == "test.whoami");
        // Attributed only to peers that negotiated caller attribution.
        let (negotiated, unsupported, unknown) = &seen[0];
        assert!(
            matches!(negotiated, bmux_ipc::Request::InvokeServiceAsPlugin { caller_plugin_id, capability, payload, .. }
                if caller_plugin_id == "example.native" && capability == "test.whoami" && payload == b"payload"),
            "{negotiated:?}"
        );
        assert!(plain(unsupported) && plain(unknown));
        // A host-originated route carries no plugin identity at all.
        let (negotiated, unsupported, unknown) = &seen[1];
        assert!(plain(negotiated) && plain(unsupported) && plain(unknown));
        retire_selected_effect_scope(instance, activation, invocation).await;
    }

    #[test]
    fn plugins_cannot_forge_identity_through_the_bridge() {
        let forged = bmux_ipc::encode(&bmux_ipc::Request::InvokeServiceAsPlugin {
            caller_plugin_id: "plugin.victim".into(),
            capability: "test.whoami".into(),
            kind: bmux_ipc::InvokeServiceKind::Query,
            interface_id: "whoami/v1".into(),
            operation: "read".into(),
            payload: Vec::new(),
        })
        .unwrap();
        assert_eq!(bridge_response_payload(&forged), Err(2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plugins_cannot_forge_identity_through_the_kernel_port() {
        let directory = private_scope_directory();
        let server = BmuxServer::new(bmux_ipc::IpcEndpoint::unix_socket(
            directory.path().join("s"),
        ));
        let authority = AuthenticatedServiceInvocation::new(server.host_service_context());
        let (instance, activation, invocation) = selected_effect_scope();
        let scope = invocation.clone();
        let forged = bmux_ipc::encode(&bmux_ipc::Request::InvokeServiceAsPlugin {
            caller_plugin_id: "plugin.victim".into(),
            capability: "test.whoami".into(),
            kind: bmux_ipc::InvokeServiceKind::Query,
            interface_id: "whoami/v1".into(),
            operation: "read".into(),
            payload: Vec::new(),
        })
        .unwrap();
        let error = tokio::task::spawn_blocking(move || {
            bmux_plugin::InvocationKernelDispatch::execute(&authority, &scope, &forged)
        })
        .await
        .unwrap()
        .expect_err("identity assertion must be refused");
        assert!(error.to_string().contains("caller identity"), "{error}");
        retire_selected_effect_scope(instance, activation, invocation).await;
    }
}
