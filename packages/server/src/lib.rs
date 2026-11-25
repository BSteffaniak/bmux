#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Server component for bmux terminal multiplexer.

use anyhow::{Context, Result};
use bmux_attach_image_protocol::CompressionId;
use bmux_attach_token_state::AttachGrant;
use bmux_client_state::FollowStateHandle;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_context_state::{ContextSelector, ContextStateHandle};
use bmux_ipc::transport::{ErasedIpcStream, IpcTransportError, LocalIpcListener};
use bmux_ipc::{
    CORE_PROTOCOL_CAPABILITIES, Envelope, EnvelopeKind, ErrorCode, ErrorResponse, Event,
    IpcEndpoint, ProtocolContract, Request, Response, ResponsePayload, ServicePipelinePayload,
    ServicePipelineRequest, ServicePipelineStepResult, decode, default_supported_capabilities,
    encode, negotiate_protocol,
};
use bmux_perf_telemetry::{
    PhaseChannel, PhasePayload, PhaseTimer, emit as emit_phase_timing, flush as flush_phase_timing,
};
use bmux_plugin::stream_admission::{
    StreamAdmissionBinding, StreamIngress, StreamIngressError, StreamIngressHandle,
};
use bmux_plugin_sdk::{RemoteAccessAuthorizerHandle, RouteAccess, authorize_remote_access};
use bmux_plugin_sdk::{WireEventSink, WireEventSinkError, WireEventSinkHandle};
use bmux_recording_protocol::{RecordingEventKind, RecordingRollingStartOptions};
use bmux_session_models::{ClientId, SessionId};
use bmux_session_state::SessionManagerHandle;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use tracing::{Instrument as _, debug, info, warn};
use uuid::Uuid;

use bmux_performance_state::{
    PerformanceEventRateLimiter, PerformanceRecordingLevel, PerformanceSettingsHandle,
    PerformanceSettingsReader, PerformanceSettingsStore,
};
use bmux_recording_runtime::RollingRecordingSettings;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_TOKEN_TTL: Duration = Duration::from_secs(10);

const EVENT_PUSH_CHANNEL_CAPACITY: usize = 256;

const RETAINED_RECIPIENT_LIMIT: usize = 4096;
const RETAINED_KIND_LIMIT: usize = 64;
const RETAINED_KIND_BYTES: usize = 256;
const RETAINED_PAYLOAD_BYTES: usize = 256 * 1024;
const RETAINED_TOTAL_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct RetainedWireEvents {
    recipients: Mutex<BTreeMap<Uuid, RetainedRecipient>>,
    /// Plugin sources the push task pulls from for a recipient after a wake.
    pull_sources: Mutex<Vec<(String, bmux_plugin_sdk::WirePullSource)>>,
}

#[derive(Default)]
struct RetainedRecipient {
    values: BTreeMap<String, (Vec<u8>, bool)>,
    bytes: usize,
    ready: Arc<tokio::sync::Notify>,
    /// A pull source asked to be read for this recipient.
    woken: bool,
}

fn retained_error(message: &str) -> WireEventSinkError {
    WireEventSinkError::PublishFailed(message.into())
}

impl RetainedWireEvents {
    fn register(self: &Arc<Self>, recipient: Uuid) -> Result<RetainedRegistration> {
        let mut recipients = self
            .recipients
            .lock()
            .map_err(|_| retained_error("poisoned retention lock"))?;
        anyhow::ensure!(
            recipients.len() < RETAINED_RECIPIENT_LIMIT,
            "retained recipient limit exceeded"
        );
        anyhow::ensure!(
            !recipients.contains_key(&recipient),
            "recipient already registered"
        );
        let entry = RetainedRecipient::default();
        let ready = Arc::clone(&entry.ready);
        recipients.insert(recipient, entry);
        drop(recipients);
        Ok(RetainedRegistration {
            retention: Arc::clone(self),
            recipient,
            ready,
        })
    }

    fn publish(
        &self,
        recipient: Uuid,
        kind: String,
        payload: Vec<u8>,
    ) -> std::result::Result<(), WireEventSinkError> {
        if kind.len() > RETAINED_KIND_BYTES || payload.len() > RETAINED_PAYLOAD_BYTES {
            return Err(retained_error("retained kind or payload limit exceeded"));
        }
        let mut recipients = self
            .recipients
            .lock()
            .map_err(|_| retained_error("poisoned retention lock"))?;
        let entry = recipients
            .get_mut(&recipient)
            .ok_or_else(|| retained_error("recipient disconnected"))?;
        let old_bytes = entry
            .values
            .get(&kind)
            .map_or(0, |(value, _)| kind.len() + value.len());
        let bytes = entry.bytes - old_bytes + kind.len() + payload.len();
        if bytes > RETAINED_TOTAL_BYTES
            || (old_bytes == 0
                && entry.values.len() >= RETAINED_KIND_LIMIT
                && !entry.values.contains_key(&kind))
        {
            return Err(retained_error("retained recipient capacity exceeded"));
        }
        entry.bytes = bytes;
        entry.values.insert(kind, (payload, true));
        entry.ready.notify_one();
        drop(recipients);
        Ok(())
    }

    fn clear(&self, recipient: Uuid, kind: &str) -> std::result::Result<(), WireEventSinkError> {
        let mut recipients = self
            .recipients
            .lock()
            .map_err(|_| retained_error("poisoned retention lock"))?;
        let entry = recipients
            .get_mut(&recipient)
            .ok_or_else(|| retained_error("recipient disconnected"))?;
        if let Some((payload, _)) = entry.values.remove(kind) {
            entry.bytes -= kind.len() + payload.len();
        }
        drop(recipients);
        Ok(())
    }

    fn wake(&self, recipient: Uuid) -> std::result::Result<(), WireEventSinkError> {
        let mut recipients = self
            .recipients
            .lock()
            .map_err(|_| retained_error("poisoned retention lock"))?;
        let entry = recipients
            .get_mut(&recipient)
            .ok_or_else(|| retained_error("recipient disconnected"))?;
        entry.woken = true;
        entry.ready.notify_one();
        drop(recipients);
        Ok(())
    }

    /// Whether the recipient was woken since last asked, clearing it.
    fn take_wake(&self, recipient: Uuid) -> bool {
        self.recipients
            .lock()
            .ok()
            .and_then(|mut recipients| {
                recipients
                    .get_mut(&recipient)
                    .map(|entry| std::mem::take(&mut entry.woken))
            })
            .unwrap_or(false)
    }

    fn pull_sources(&self) -> Vec<(String, bmux_plugin_sdk::WirePullSource)> {
        self.pull_sources
            .lock()
            .map_or_else(|_| Vec::new(), |sources| sources.clone())
    }

    fn take_pending(&self, recipient: Uuid) -> Option<Event> {
        let mut recipients = self.recipients.lock().ok()?;
        let entry = recipients.get_mut(&recipient)?;
        let (kind, (payload, pending)) =
            entry.values.iter_mut().find(|(_, (_, pending))| *pending)?;
        *pending = false;
        let event = Event::PluginBusEvent {
            kind: kind.clone(),
            payload: payload.clone(),
        };
        drop(recipients);
        Some(event)
    }
}

/// Owns the live routing identity; cancellation retires retained values too.
struct RetainedRegistration {
    retention: Arc<RetainedWireEvents>,
    recipient: Uuid,
    ready: Arc<tokio::sync::Notify>,
}

impl Drop for RetainedRegistration {
    fn drop(&mut self) {
        if let Ok(mut recipients) = self.retention.recipients.lock() {
            recipients.remove(&self.recipient);
        }
    }
}

/// Main server implementation.
#[derive(Clone)]
pub struct BmuxServer {
    endpoint: IpcEndpoint,
    ipc_security: bmux_ipc::transport::UnixSecurityPolicy,
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
}

struct ServerState {
    local_terminal_bound: std::sync::OnceLock<()>,
    stream_provenance: bmux_plugin::stream_admission::StreamProvenanceRegistry,
    /// BPDL access classes declared for each routed service endpoint.
    route_access: Mutex<BTreeMap<ServiceEndpoint, DeclaredAccess>>,
    /// Access classes plugins declare for their commands, by (plugin id,
    /// command name). The CLI bridge's `run_plugin` route takes the class of
    /// the command it runs.
    command_access: Mutex<BTreeMap<(String, String), RouteAccess>>,
    /// Connection limit shared by local and admitted streams. Acquired before
    /// a handler task exists, so a full server fails closed.
    transport_admission: Arc<tokio::sync::Semaphore>,
    /// Admitted streams handed in through the registered [`StreamIngressHandle`],
    /// taken by the accept loop when it starts.
    admitted_ingress: Mutex<Option<mpsc::UnboundedReceiver<AdmittedConnection>>>,
    plugin_states: Arc<bmux_plugin::PluginStateRegistry>,
    /// The host's root plugin event bus. Plugins only ever receive scoped
    /// views of it through their activation capabilities.
    plugin_events: Arc<bmux_plugin::EventBus>,
    attach_tokens: Arc<Mutex<AttachTokenManager>>,
    performance_settings: PerformanceSettingsStore,
    operation_lock: AsyncMutex<()>,
    event_hub: Mutex<EventHub>,
    /// Broadcast channel for pushing events to streaming clients.
    event_broadcast: tokio::sync::broadcast::Sender<Event>,
    event_push_clients: AsyncMutex<BTreeSet<ClientId>>,
    retained_wire_events: Arc<RetainedWireEvents>,
    client_principals: Arc<Mutex<BTreeMap<ClientId, Uuid>>>,
    server_control_principal_id: Uuid,
    handshake_timeout: Duration,
    service_registry: Mutex<ServiceRegistry>,
    service_resolver: Mutex<Option<Arc<ServiceResolverHandler>>>,
    plugin_state_replayers: Mutex<Vec<PluginStateReplayer>>,
    device_seal_pending: AsyncMutex<BTreeMap<Uuid, DeviceSealPending>>,
}

struct DeviceSealPending {
    target_client_id: ClientId,
    response_tx: oneshot::Sender<Vec<u8>>,
}

type PluginStateReplayer = Arc<dyn Fn() -> Option<Event> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ServiceRoute {
    pub capability: String,
    pub kind: bmux_ipc::InvokeServiceKind,
    pub interface_id: String,
    pub operation: String,
}

/// A routed service endpoint: every operation of one interface.
type ServiceEndpoint = (String, bmux_ipc::InvokeServiceKind, String);

/// Declared BPDL access classes of one service endpoint.
#[derive(Debug, Clone, Default)]
struct DeclaredAccess {
    interface: Option<RouteAccess>,
    operations: BTreeMap<String, RouteAccess>,
}

impl DeclaredAccess {
    fn for_operation(&self, operation: &str) -> Option<RouteAccess> {
        self.operations.get(operation).copied().or(self.interface)
    }
}

/// One admitted stream waiting for the accept loop, already charged against
/// the connection limit.
struct AdmittedConnection {
    stream: ErasedIpcStream,
    binding: Arc<StreamAdmissionBinding>,
    slot: ConnectionSlot,
}

/// One charged connection slot; dropping it frees the slot. Unlike a held
/// semaphore permit it is plain data, so queued connections carry no guard.
struct ConnectionSlot(Arc<tokio::sync::Semaphore>);

impl ConnectionSlot {
    fn try_acquire(admission: &Arc<tokio::sync::Semaphore>) -> Option<Self> {
        admission.try_acquire().ok()?.forget();
        Some(Self(Arc::clone(admission)))
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

/// The server's [`StreamIngress`]: charges the connection limit synchronously
/// and queues the stream for the accept loop.
struct ServerStreamIngress {
    admission: Arc<tokio::sync::Semaphore>,
    queue: mpsc::UnboundedSender<AdmittedConnection>,
    shutdown: watch::Receiver<bool>,
}

impl StreamIngress for ServerStreamIngress {
    fn accept(
        &self,
        stream: ErasedIpcStream,
        binding: Arc<StreamAdmissionBinding>,
    ) -> std::result::Result<(), StreamIngressError> {
        if *self.shutdown.borrow() {
            return Err(StreamIngressError::Unavailable);
        }
        if binding.is_revoked() {
            return Err(StreamIngressError::Revoked);
        }
        let slot =
            ConnectionSlot::try_acquire(&self.admission).ok_or(StreamIngressError::AtCapacity)?;
        self.queue
            .send(AdmittedConnection {
                stream,
                binding,
                slot,
            })
            .map_err(|_| StreamIngressError::Unavailable)
    }
}

/// Maximum concurrent connections, local and admitted together.
const MAX_CONNECTIONS: usize = 64;

#[derive(Debug, Clone, Default)]
pub struct ServiceInvokeOutput {
    pub payload: Vec<u8>,
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl From<Vec<u8>> for ServiceInvokeOutput {
    fn from(payload: Vec<u8>) -> Self {
        Self {
            payload,
            metadata: BTreeMap::new(),
        }
    }
}

type ServiceInvokeFuture = Pin<Box<dyn Future<Output = Result<ServiceInvokeOutput>> + Send>>;
type ServiceInvokeHandler =
    dyn Fn(ServiceRoute, ServiceInvokeContext, Vec<u8>) -> ServiceInvokeFuture + Send + Sync;
type ServiceResolverHandler = dyn Fn(ServiceRoute, Vec<u8>) -> ServiceInvokeFuture + Send + Sync;

#[derive(Default)]
struct ServiceRegistry {
    handlers: BTreeMap<ServiceRoute, Arc<ServiceInvokeHandler>>,
    checked_handlers: std::collections::BTreeSet<ServiceRoute>,
}

#[derive(Debug)]
struct ServiceCompatibilityError(&'static str);

impl std::fmt::Display for ServiceCompatibilityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ServiceCompatibilityError {}

fn service_invocation_error_code(error: &anyhow::Error) -> ErrorCode {
    if error.downcast_ref::<ServiceCompatibilityError>().is_some()
        || matches!(
            error.downcast_ref::<bmux_plugin_sdk::PluginError>(),
            Some(bmux_plugin_sdk::PluginError::ServiceProtocol { .. })
        )
    {
        ErrorCode::ServiceIncompatible
    } else {
        ErrorCode::Internal
    }
}

impl ServiceRegistry {
    fn dispatch(
        &self,
        route: &ServiceRoute,
        context: ServiceInvokeContext,
        payload: Vec<u8>,
    ) -> Option<ServiceInvokeFuture> {
        let checked = bmux_plugin_sdk::service_contract::is_checked(&payload);
        if checked
            && bmux_plugin_sdk::service_contract::interface_id_for(&route.interface_id, &payload)
                .is_err()
        {
            return Some(Box::pin(async {
                Err(
                    ServiceCompatibilityError("checked frame does not match routed interface")
                        .into(),
                )
            }));
        }
        if let Some(handler) = self.handlers.get(route).cloned() {
            if checked && !self.checked_handlers.contains(route) {
                return Some(Box::pin(async {
                    Err(ServiceCompatibilityError(
                        "service handler has no checked BPDL contract boundary",
                    )
                    .into())
                }));
            }
            return Some(handler(route.clone(), context, payload));
        }
        let mut wildcard = route.clone();
        wildcard.operation = "*".to_string();
        if checked
            && self.handlers.contains_key(&wildcard)
            && !self.checked_handlers.contains(&wildcard)
        {
            return Some(Box::pin(async {
                Err(ServiceCompatibilityError(
                    "service handler has no checked BPDL contract boundary",
                )
                .into())
            }));
        }
        self.handlers
            .get(&wildcard)
            .cloned()
            .map(|handler| handler(route.clone(), context, payload))
    }
}

#[derive(Clone)]
pub struct ServiceInvokeContext {
    host_dispatch_context: Option<Arc<dyn std::any::Any + Send + Sync>>,
    host_internal: bool,
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
    client_id: ClientId,
    client_principal_id: Uuid,
    /// Plugin a host authenticated as the originator of this invocation.
    /// `None` for host-originated calls (CLI, attach, server internals).
    caller_plugin_id: Option<Arc<str>>,
}

impl ServiceInvokeContext {
    /// Retain opaque Rust-local host context without changing transport authority.
    /// This value is never decoded from or serialized into a request.
    #[must_use]
    pub fn with_host_dispatch_context(
        mut self,
        context: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        self.host_dispatch_context = Some(context);
        self
    }

    /// Read a selected host context. A present value of the wrong type is an
    /// error, never permission to fall back to a fresh root authority.
    /// # Errors
    /// Returns an error if the retained host context has a different Rust type.
    pub fn host_dispatch_context<T: std::any::Any + Send + Sync>(&self) -> Result<Option<Arc<T>>> {
        self.host_dispatch_context
            .clone()
            .map(|context| {
                context
                    .downcast::<T>()
                    .map_err(|_| anyhow::anyhow!("host dispatch context type mismatch"))
            })
            .transpose()
    }

    /// Connected-client id on whose behalf this service invocation is
    /// dispatched. Plugin handlers use this to know which client made
    /// a request without a separate `WhoAmI` round-trip.
    #[must_use]
    pub const fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// Connected client this invocation acts for, or `None` when the host
    /// issued the context itself. A host-internal context's client id is a
    /// synthetic per-context identity, never a connected client.
    #[must_use]
    pub const fn connected_client_id(&self) -> Option<ClientId> {
        if self.host_internal {
            None
        } else {
            Some(self.client_id)
        }
    }

    /// Plugin a host authenticated as the originator of this invocation, or
    /// `None` when the host itself originated it.
    ///
    /// Set only from host-derived identity: a host's bound plugin context or
    /// a [`Request::InvokeServiceAsPlugin`] accepted from a local stream.
    #[must_use]
    pub fn caller_plugin_id(&self) -> Option<&str> {
        self.caller_plugin_id.as_deref()
    }

    /// Attribute service invocations dispatched through this context to a
    /// plugin the host process authenticated. Host-only: the host derives
    /// `plugin_id` from a registration it issued, never from plugin input.
    #[must_use]
    pub fn with_caller_plugin_id(&self, plugin_id: &str) -> Self {
        Self {
            caller_plugin_id: Some(Arc::from(plugin_id)),
            ..self.clone()
        }
    }
}

impl ServiceInvokeContext {
    /// Observe the exact selected tuple after checking retained transport access.
    /// This does not invoke a provider or grant authority for a later request.
    /// Opaque fallback resolvers cannot establish authoritative absence.
    ///
    /// # Errors
    /// Returns an error for invalid routing metadata, unsupported service kinds,
    /// denied or invalid retained transport access, unavailable registry locks,
    /// or an opaque fallback resolver that prevents an authoritative answer.
    /// These failures are never reported as provider absence.
    pub fn selected_service_availability(
        &self,
        key: &bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability> {
        use bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability;
        key.validate()?;
        let kind = match key.kind {
            bmux_plugin_sdk::ServiceKind::Query => bmux_ipc::InvokeServiceKind::Query,
            bmux_plugin_sdk::ServiceKind::Command => bmux_ipc::InvokeServiceKind::Command,
            bmux_plugin_sdk::ServiceKind::Event => anyhow::bail!("event availability unsupported"),
        };
        let matches = |route: &ServiceRoute| {
            route.capability == key.capability
                && route.kind == kind
                && route.interface_id == key.interface_id
        };
        if !self.host_internal {
            // Denial is decided before observing registry presence.
            check_stream_origin(&self.state, self.client_id.0)?;
        }
        let registry = self
            .state
            .service_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("service registry unavailable"))?;
        if registry.handlers.keys().any(matches) {
            return Ok(SelectedServiceAvailability::Present);
        }
        drop(registry);
        anyhow::ensure!(
            self.state
                .service_resolver
                .lock()
                .map_err(|_| anyhow::anyhow!("service resolver unavailable"))?
                .is_none(),
            "opaque service resolver cannot establish selected availability"
        );
        Ok(SelectedServiceAvailability::Absent)
    }

    /// Answer the stream-metadata kernel request (current stream provenance),
    /// which never reaches service dispatch.
    /// `None` when the request is not a stream-metadata request.
    fn execute_stream_metadata_request(&self, request: &Request) -> Option<Response> {
        if matches!(request, Request::CurrentStreamProvenance) {
            if self.host_internal {
                return Some(Response::Ok(ResponsePayload::CurrentStreamProvenance {
                    provenance: bmux_ipc::StreamProvenance::HostInternal,
                }));
            }
            return Some(current_stream_provenance(
                &self.state.stream_provenance,
                self.client_id.0,
            ));
        }
        None
    }

    async fn execute_request(&self, request: Request) -> Result<Response> {
        if let Some(response) = self.execute_stream_metadata_request(&request) {
            return Ok(response);
        }
        if self.host_dispatch_context.is_some()
            && let Request::InvokeService {
                capability,
                kind,
                interface_id,
                operation,
                payload,
            } = &request
        {
            let route = ServiceRoute {
                capability: capability.clone(),
                kind: *kind,
                interface_id: interface_id.clone(),
                operation: operation.clone(),
            };
            if !self.host_internal {
                check_stream_origin(&self.state, self.client_id.0)?;
            }
            let invocation = self
                .state
                .service_registry
                .lock()
                .map_err(|_| anyhow::anyhow!("service registry unavailable"))?
                .dispatch(&route, self.clone(), payload.clone())
                .context("bound in-process selected service unavailable")?;
            return Ok(Response::Ok(ResponsePayload::ServiceInvoked {
                payload: invocation.await?.payload,
            }));
        }
        if self.host_internal {
            // Preserve host-internal origin across foundational facade calls;
            // no synthetic local Hello or metadata principal is involved.
            if let Request::InvokeService {
                capability,
                kind,
                interface_id,
                operation,
                payload,
            } = request
            {
                let route = ServiceRoute {
                    capability,
                    kind,
                    interface_id,
                    operation,
                };
                let invocation = self
                    .state
                    .service_registry
                    .lock()
                    .map_err(|_| anyhow::anyhow!("service registry unavailable"))?
                    .dispatch(&route, self.clone(), payload)
                    .context("host internal service unavailable")?;
                return Ok(Response::Ok(ResponsePayload::ServiceInvoked {
                    payload: invocation.await?.payload,
                }));
            }
            anyhow::bail!("unsupported host internal kernel request");
        }
        // Every nested kernel dispatch retains and revalidates the host origin.
        if !self.host_internal
            && matches!(
                self.state.stream_provenance.lookup(self.client_id.0),
                bmux_plugin::stream_admission::StreamProvenance::Invalid
            )
        {
            anyhow::bail!("invalid stream provenance");
        }
        // Selection state is owned by the clients-plugin's FollowState now;
        // plugins that need the caller's selected session should query it
        // through the FollowStateHandle rather than relying on
        // per-invocation locals. We pass an empty selection local here —
        // any handler that still reads it falls back to FollowState via
        // sync_selected_target_from_follow_state.
        if let Some(response) = self.execute_attributed_service_request(&request).await {
            return Ok(response);
        }
        let mut selected_session: Option<SessionId> = None;
        handle_request(
            &self.state,
            &self.shutdown_tx,
            self.client_id,
            self.client_principal_id,
            &mut selected_session,
            request,
        )
        .await
    }

    /// Dispatch an `InvokeService` request under this context's plugin
    /// attribution. `None` when the context has no attribution or the
    /// request is not a service invocation.
    async fn execute_attributed_service_request(&self, request: &Request) -> Option<Response> {
        let (
            Some(caller_plugin_id),
            Request::InvokeService {
                capability,
                kind,
                interface_id,
                operation,
                payload,
            },
        ) = (self.caller_plugin_id.as_ref(), request)
        else {
            return None;
        };
        let route = ServiceRoute {
            capability: capability.clone(),
            kind: *kind,
            interface_id: interface_id.clone(),
            operation: operation.clone(),
        };
        Some(service_output_response(
            invoke_service_output_as(
                &self.state,
                &self.shutdown_tx,
                self.client_id,
                self.client_principal_id,
                route,
                payload.clone(),
                Some(Arc::clone(caller_plugin_id)),
            )
            .await,
        ))
    }

    /// Execute a raw request payload.
    ///
    /// # Errors
    /// Returns an error if decoding, handling, or encoding fails.
    pub async fn execute_raw(&self, request_payload: Vec<u8>) -> Result<Vec<u8>> {
        let request: Request =
            decode(&request_payload).context("failed decoding kernel bridge request payload")?;
        let response = self.execute_request(request).await?;
        encode(&response).context("failed encoding kernel bridge response payload")
    }
}

/// Every dispatch acting for a connection requires that connection's origin to
/// still be valid: unknown, closed and revoked streams are denied. Access
/// classes of admitted streams are checked once, where their requests enter
/// (see [`authorize_admitted_request`]); calls a plugin makes while serving
/// such a request are the plugin's own and are not re-classified.
fn check_stream_origin(state: &ServerState, id: Uuid) -> Result<()> {
    use bmux_plugin::stream_admission::StreamProvenance;
    match state.stream_provenance.lookup(id) {
        StreamProvenance::Local | StreamProvenance::Admitted(_) => Ok(()),
        StreamProvenance::Invalid => anyhow::bail!("invalid stream origin"),
    }
}

/// The fixed access class of a kernel request on an admitted stream, or
/// `None` for service invocations, whose routes declare their own classes.
#[must_use]
pub const fn kernel_request_access(request: &Request) -> Option<RouteAccess> {
    match request {
        Request::Hello { .. }
        | Request::Ping
        | Request::WhoAmIPrincipal
        | Request::ServerStatus
        | Request::SubscribeEvents
        | Request::PollEvents { .. }
        | Request::EnableEventPush => Some(RouteAccess::Observe),
        Request::EmitOnPluginBus { .. } => Some(RouteAccess::Input),
        // Stopping the server, brokering the host's device seal and the
        // host-only kernel bridge are never reachable remotely.
        Request::ServerStop
        | Request::DeviceSealBroker { .. }
        | Request::DeviceSealBrokerResponse { .. }
        | Request::CurrentStreamProvenance => Some(RouteAccess::OwnerLocal),
        Request::InvokeService { .. }
        | Request::InvokeServiceAsPlugin { .. }
        | Request::InvokeServicePipeline { .. } => None,
    }
}

/// Whether a route is the CLI bridge's `run_plugin`, which runs a plugin
/// command on the host and so takes that command's declared class.
fn is_plugin_command_bridge(
    capability: &str,
    kind: bmux_ipc::InvokeServiceKind,
    interface_id: &str,
    operation: &str,
) -> bool {
    capability == bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY
        && kind == bmux_ipc::InvokeServiceKind::Command
        && interface_id == bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1
        && operation == bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1
}

/// The class of the plugin command a `run_plugin` payload names, or `None`
/// (denied remotely) when the payload does not decode or the command
/// declares no class.
fn plugin_command_access(state: &ServerState, payload: &[u8]) -> Option<RouteAccess> {
    let payload = if bmux_plugin_sdk::service_contract::is_checked(payload) {
        bmux_plugin_sdk::service_contract::decode_for(
            bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1,
            bmux_plugin_sdk::cli_bridge::cli_command::SERVICE_CONTRACT,
            payload,
        )
        .ok()?
    } else {
        payload
    };
    let request: bmux_plugin_sdk::PluginCliCommandRequest =
        bmux_plugin_sdk::decode_service_message(payload).ok()?;
    state
        .command_access
        .lock()
        .ok()?
        .get(&(request.plugin_id, request.command_name))
        .copied()
}

fn declared_route_access(
    state: &ServerState,
    capability: &str,
    kind: bmux_ipc::InvokeServiceKind,
    interface_id: &str,
    operation: &str,
    payload: Option<&[u8]>,
) -> Option<RouteAccess> {
    if is_plugin_command_bridge(capability, kind, interface_id, operation) {
        // A templated pipeline payload names no command before it runs.
        return payload.and_then(|payload| plugin_command_access(state, payload));
    }
    state
        .route_access
        .lock()
        .ok()?
        .get(&(capability.to_string(), kind, interface_id.to_string()))
        .and_then(|declared| declared.for_operation(operation))
}

/// Authorize one request read from an admitted stream, before any effect.
/// Every route of a pipeline is checked before its first stage runs.
///
/// # Errors
/// Returns a caller-facing denial.
fn authorize_admitted_request(
    state: &ServerState,
    binding: &StreamAdmissionBinding,
    request: &Request,
) -> std::result::Result<(), String> {
    let authorizer = state
        .plugin_states
        .get::<RemoteAccessAuthorizerHandle>()
        .and_then(|handle| handle.read().ok().map(|handle| handle.clone()));
    let subject = binding.subject_id();
    let check = |access: Option<RouteAccess>, what: &dyn std::fmt::Display| {
        authorize_remote_access(authorizer.as_ref(), subject, access)
            .map_err(|denied| format!("remote access denied for {what}: {denied}"))
    };
    let route = |capability: &str, kind, interface_id: &str, operation: &str, payload| {
        (
            declared_route_access(state, capability, kind, interface_id, operation, payload),
            format!("{interface_id}.{operation}"),
        )
    };
    match request {
        Request::InvokeService {
            capability,
            kind,
            interface_id,
            operation,
            payload,
        }
        | Request::InvokeServiceAsPlugin {
            capability,
            kind,
            interface_id,
            operation,
            payload,
            ..
        } => {
            let (access, name) = route(capability, *kind, interface_id, operation, Some(payload));
            check(access, &name)
        }
        Request::InvokeServicePipeline { pipeline } => {
            for step in &pipeline.steps {
                let payload = match &step.payload {
                    bmux_ipc::ServicePipelinePayload::Encoded { payload } => Some(payload.as_slice()),
                    bmux_ipc::ServicePipelinePayload::JsonTemplate { .. } => None,
                };
                let (access, name) = route(
                    &step.capability,
                    step.kind,
                    &step.interface_id,
                    &step.operation,
                    payload,
                );
                check(access, &name)?;
            }
            Ok(())
        }
        other => check(kernel_request_access(other), &request_kind_name(other)),
    }
}

async fn invoke_service_output(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    client_id: ClientId,
    client_principal_id: Uuid,
    route: ServiceRoute,
    payload: Vec<u8>,
) -> Result<ServiceInvokeOutput> {
    invoke_service_output_as(
        state,
        shutdown_tx,
        client_id,
        client_principal_id,
        route,
        payload,
        None,
    )
    .await
}

fn service_output_response(result: Result<ServiceInvokeOutput>) -> Response {
    match result {
        Ok(output) => Response::Ok(ResponsePayload::ServiceInvoked {
            payload: output.payload,
        }),
        Err(error) => Response::Err(ErrorResponse {
            code: service_invocation_error_code(&error),
            message: format!("service invocation failed: {error:#}"),
        }),
    }
}

/// Plugin attribution a stream may assert with
/// [`Request::InvokeServiceAsPlugin`]: only same-user local streams, which
/// are already inside the host trust boundary. Anything else is dropped so
/// the call is dispatched exactly like a plain, host-attributed request.
fn accepted_caller_plugin_attribution(
    state: &ServerState,
    client_id: ClientId,
    caller_plugin_id: &str,
) -> Option<Arc<str>> {
    let caller_plugin_id = caller_plugin_id.trim();
    (!caller_plugin_id.is_empty()
        && matches!(
            state.stream_provenance.lookup(client_id.0),
            bmux_plugin::stream_admission::StreamProvenance::Local
        ))
    .then(|| Arc::from(caller_plugin_id))
}

async fn invoke_service_output_as(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    client_id: ClientId,
    client_principal_id: Uuid,
    route: ServiceRoute,
    payload: Vec<u8>,
    caller_plugin_id: Option<Arc<str>>,
) -> Result<ServiceInvokeOutput> {
    check_stream_origin(state, client_id.0)?;
    let total_timer = PhaseTimer::start();
    let capability = route.capability.clone();
    let kind = route.kind;
    let interface_id = route.interface_id.clone();
    let operation = route.operation.clone();
    let invoke_context = ServiceInvokeContext {
        host_dispatch_context: None,
        host_internal: false,
        state: Arc::clone(state),
        shutdown_tx: shutdown_tx.clone(),
        client_id,
        client_principal_id,
        caller_plugin_id,
    };
    let registry_timer = PhaseTimer::start();
    let dispatch = {
        let registry = state
            .service_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("service registry lock poisoned"))?;
        registry.dispatch(&route, invoke_context, payload.clone())
    };
    let registry_us = registry_timer.elapsed_us();
    let resolver_timer = PhaseTimer::start();
    let invocation = if let Some(invocation) = dispatch {
        Some(invocation)
    } else {
        if bmux_plugin_sdk::service_contract::is_checked(&payload) {
            return Err(ServiceCompatibilityError(
                "no checked BPDL provider registered for service route",
            )
            .into());
        }
        let resolver = state
            .service_resolver
            .lock()
            .map_err(|_| anyhow::anyhow!("service resolver lock poisoned"))?
            .clone();
        resolver.map(|resolver| resolver(route, payload))
    };
    let resolver_us = resolver_timer.elapsed_us();

    let Some(invocation) = invocation else {
        return Err(anyhow::anyhow!(
            "no provider for service capability='{capability}' kind='{kind:?}' interface='{interface_id}' operation='{operation}'"
        ));
    };
    let invocation_timer = PhaseTimer::start();
    let output = invocation.await?;
    let invocation_us = invocation_timer.elapsed_us();
    emit_phase_timing(
        PhaseChannel::Service,
        &PhasePayload::new("service.server_invoke")
            .service_fields(&capability, format!("{kind:?}"), &interface_id, &operation)
            .field("client_id", client_id)
            .field("registry_us", registry_us)
            .field("resolver_us", resolver_us)
            .field("invocation_us", invocation_us)
            .field("response_payload_len", output.payload.len())
            .field("metadata_keys", output.metadata.len())
            .field("total_us", total_timer.elapsed_us())
            .finish(),
    );
    Ok(output)
}

async fn execute_service_pipeline(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    client_id: ClientId,
    client_principal_id: Uuid,
    pipeline: ServicePipelineRequest,
) -> Result<Vec<ServicePipelineStepResult>> {
    let total_timer = PhaseTimer::start();
    let step_count = pipeline.steps.len();
    let mut results = Vec::with_capacity(pipeline.steps.len());
    let mut metadata_by_step = Vec::with_capacity(pipeline.steps.len());
    for (step_index, step) in pipeline.steps.into_iter().enumerate() {
        let step_timer = PhaseTimer::start();
        let resolve_timer = PhaseTimer::start();
        let payload = resolve_pipeline_payload(&step.payload, &pipeline.inputs, &metadata_by_step)?;
        let payload_resolve_us = resolve_timer.elapsed_us();
        let payload_len = payload.len();
        let capability = step.capability;
        let kind = step.kind;
        let interface_id = step.interface_id;
        let operation = step.operation;
        let route = ServiceRoute {
            capability: capability.clone(),
            kind,
            interface_id: interface_id.clone(),
            operation: operation.clone(),
        };
        let invoke_timer = PhaseTimer::start();
        let output = invoke_service_output(
            state,
            shutdown_tx,
            client_id,
            client_principal_id,
            route,
            payload,
        )
        .await?;
        let invoke_us = invoke_timer.elapsed_us();
        emit_phase_timing(
            PhaseChannel::Service,
            &PhasePayload::new("service_pipeline.step")
                .service_fields(&capability, format!("{kind:?}"), &interface_id, &operation)
                .field("client_id", client_id)
                .field("step_index", step_index)
                .field("payload_resolve_us", payload_resolve_us)
                .field("invoke_us", invoke_us)
                .field("request_payload_len", payload_len)
                .field("response_payload_len", output.payload.len())
                .field("metadata_keys", output.metadata.len())
                .field("total_us", step_timer.elapsed_us())
                .finish(),
        );
        metadata_by_step.push(output.metadata.clone());
        results.push(ServicePipelineStepResult {
            payload: output.payload,
            metadata: output.metadata,
        });
    }
    emit_phase_timing(
        PhaseChannel::Service,
        &PhasePayload::new("service_pipeline.execute")
            .field("client_id", client_id)
            .field("step_count", step_count)
            .field("total_us", total_timer.elapsed_us())
            .finish(),
    );
    Ok(results)
}

fn resolve_pipeline_payload(
    payload: &ServicePipelinePayload,
    inputs: &BTreeMap<String, serde_json::Value>,
    metadata_by_step: &[BTreeMap<String, serde_json::Value>],
) -> Result<Vec<u8>> {
    match payload {
        ServicePipelinePayload::Encoded { payload } => Ok(payload.clone()),
        ServicePipelinePayload::JsonTemplate { value, field_order } => {
            let resolved = resolve_pipeline_template_value(value, inputs, metadata_by_step)?;
            if let Some(field_order) = field_order {
                let serde_json::Value::Object(map) = &resolved else {
                    return Err(anyhow::anyhow!(
                        "pipeline field_order requires a JSON object template"
                    ));
                };
                return bmux_codec::to_positional_vec(&PipelineStructTemplate { map, field_order })
                    .context("failed encoding resolved pipeline struct template");
            }
            bmux_codec::to_vec(&resolved).context("failed encoding resolved pipeline template")
        }
    }
}

struct PipelineStructTemplate<'a> {
    map: &'a serde_json::Map<String, serde_json::Value>,
    field_order: &'a [String],
}

impl serde::Serialize for PipelineStructTemplate<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::{Error, SerializeStruct};

        let mut state =
            serializer.serialize_struct("PipelineStructTemplate", self.field_order.len())?;
        for field in self.field_order {
            let value = self
                .map
                .get(field)
                .ok_or_else(|| Error::custom(format!("pipeline field '{field}' is missing")))?;
            state.serialize_field("", value)?;
        }
        state.end()
    }
}

fn resolve_pipeline_template_value(
    value: &serde_json::Value,
    inputs: &BTreeMap<String, serde_json::Value>,
    metadata_by_step: &[BTreeMap<String, serde_json::Value>],
) -> Result<serde_json::Value> {
    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| resolve_pipeline_template_value(value, inputs, metadata_by_step))
            .collect::<Result<Vec<_>>>()
            .map(serde_json::Value::Array),
        serde_json::Value::Object(map) => {
            if map.len() == 1 {
                if let Some(reference) = map.get("$metadata") {
                    let reference = reference
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("$metadata reference must be a string"))?;
                    return resolve_pipeline_metadata_reference(reference, metadata_by_step);
                }
                if let Some(reference) = map.get("$input") {
                    let reference = reference
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("$input reference must be a string"))?;
                    return inputs
                        .get(reference)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("pipeline input '{reference}' is missing"));
                }
            }
            let resolved = map
                .iter()
                .map(|(key, value)| {
                    resolve_pipeline_template_value(value, inputs, metadata_by_step)
                        .map(|value| (key.clone(), value))
                })
                .collect::<Result<serde_json::Map<_, _>>>()?;
            Ok(serde_json::Value::Object(resolved))
        }
        _ => Ok(value.clone()),
    }
}

fn resolve_pipeline_metadata_reference(
    reference: &str,
    metadata_by_step: &[BTreeMap<String, serde_json::Value>],
) -> Result<serde_json::Value> {
    let (step_index, key) = reference
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("metadata reference '{reference}' must be STEP:KEY"))?;
    let step_index = step_index
        .parse::<usize>()
        .with_context(|| format!("invalid metadata step index in '{reference}'"))?;
    let metadata = metadata_by_step
        .get(step_index)
        .ok_or_else(|| anyhow::anyhow!("metadata step {step_index} has not run"))?;
    metadata
        .get(key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("metadata key '{key}' missing from step {step_index}"))
}

#[derive(Debug)]
struct EventHub {
    events: Vec<EventRecord>,
    subscribers: BTreeMap<ClientId, usize>,
    max_events: usize,
}

#[derive(Debug, Clone)]
struct EventRecord {
    event: Event,
}

impl EventHub {
    const fn new(max_events: usize) -> Self {
        Self {
            events: Vec::new(),
            subscribers: BTreeMap::new(),
            max_events,
        }
    }

    fn emit(&mut self, event: Event) {
        self.events.push(EventRecord { event });
        if self.events.len() > self.max_events {
            let dropped = self.events.len() - self.max_events;
            self.events.drain(..dropped);
            for cursor in self.subscribers.values_mut() {
                *cursor = cursor.saturating_sub(dropped);
            }
        }
    }

    fn subscribe(&mut self, client_id: ClientId) {
        let start = self.events.len().saturating_sub(32);
        self.subscribers.insert(client_id, start);
    }

    fn unsubscribe(&mut self, client_id: ClientId) {
        self.subscribers.remove(&client_id);
    }

    fn poll(&mut self, client_id: ClientId, max_events: usize) -> Option<Vec<Event>> {
        let cursor = self.subscribers.get_mut(&client_id)?;
        let mut index = *cursor;
        let count = max_events.max(1);
        let mut events = Vec::new();
        while index < self.events.len() && events.len() < count {
            events.push(self.events[index].event.clone());
            index = index.saturating_add(1);
        }
        *cursor = index;
        Some(events)
    }
}

#[derive(Debug)]
struct AttachTokenManager {
    ttl: Duration,
    tokens: BTreeMap<Uuid, AttachTokenEntry>,
}

#[derive(Debug, Clone, Copy)]
struct AttachTokenEntry {
    session_id: SessionId,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachTokenValidationError {
    NotFound,
    Expired,
    SessionMismatch,
}

impl AttachTokenManager {
    const fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            tokens: BTreeMap::new(),
        }
    }

    fn issue(&mut self, session_id: SessionId) -> AttachGrant {
        self.prune_expired();

        let attach_token = Uuid::new_v4();
        let expires_at = Instant::now() + self.ttl;
        #[allow(clippy::cast_possible_truncation)]
        let expires_at_epoch_ms = epoch_millis_now().saturating_add(self.ttl.as_millis() as u64);
        self.tokens.insert(
            attach_token,
            AttachTokenEntry {
                session_id,
                expires_at,
            },
        );

        AttachGrant {
            context_id: None,
            session_id: session_id.0,
            attach_token,
            expires_at_epoch_ms,
        }
    }

    fn consume(
        &mut self,
        session_id: SessionId,
        attach_token: Uuid,
    ) -> std::result::Result<(), AttachTokenValidationError> {
        let Some(entry) = self.tokens.get(&attach_token).copied() else {
            return Err(AttachTokenValidationError::NotFound);
        };
        if entry.expires_at <= Instant::now() {
            self.tokens.remove(&attach_token);
            return Err(AttachTokenValidationError::Expired);
        }
        if entry.session_id != session_id {
            return Err(AttachTokenValidationError::SessionMismatch);
        }

        self.tokens.remove(&attach_token);

        Ok(())
    }

    fn remove_for_session(&mut self, session_id: SessionId) {
        self.tokens
            .retain(|_, entry| entry.session_id != session_id);
    }

    fn clear(&mut self) {
        self.tokens.clear();
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        self.tokens.retain(|_, entry| entry.expires_at > now);
    }
}

/// Adapter wrapping `Arc<Mutex<AttachTokenManager>>` and implementing
/// the domain-agnostic [`bmux_attach_token_state::AttachTokenManagerWriter`]
/// trait. Registered during `BmuxServer::new` so plugins reach the
/// attach-token manager through [`AttachTokenManagerHandle`] rather
/// than naming this concrete type.
struct ServerAttachTokenAdapter {
    inner: Arc<Mutex<AttachTokenManager>>,
}

impl ServerAttachTokenAdapter {
    const fn new(inner: Arc<Mutex<AttachTokenManager>>) -> Self {
        Self { inner }
    }
}

impl bmux_attach_token_state::AttachTokenManagerReader for ServerAttachTokenAdapter {
    fn contains(&self, token: Uuid) -> bool {
        self.inner
            .lock()
            .is_ok_and(|guard| guard.tokens.contains_key(&token))
    }
}

impl bmux_attach_token_state::AttachTokenManagerWriter for ServerAttachTokenAdapter {
    fn issue(&self, session_id: SessionId) -> AttachGrant {
        self.inner.lock().map_or_else(
            |_| AttachGrant {
                context_id: None,
                session_id: session_id.0,
                attach_token: Uuid::nil(),
                expires_at_epoch_ms: 0,
            },
            |mut guard| guard.issue(session_id),
        )
    }

    fn consume(
        &self,
        session_id: SessionId,
        token: Uuid,
    ) -> std::result::Result<(), bmux_attach_token_state::AttachTokenValidationError> {
        let Ok(mut guard) = self.inner.lock() else {
            return Err(bmux_attach_token_state::AttachTokenValidationError::NotFound);
        };
        match guard.consume(session_id, token) {
            Ok(()) => Ok(()),
            Err(AttachTokenValidationError::NotFound) => {
                Err(bmux_attach_token_state::AttachTokenValidationError::NotFound)
            }
            Err(AttachTokenValidationError::Expired) => {
                Err(bmux_attach_token_state::AttachTokenValidationError::Expired)
            }
            Err(AttachTokenValidationError::SessionMismatch) => {
                Err(bmux_attach_token_state::AttachTokenValidationError::SessionMismatch)
            }
        }
    }

    fn remove_for_session(&self, session_id: SessionId) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.remove_for_session(session_id);
        }
    }

    fn clear(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.clear();
        }
    }
}

/// Adapter wrapping `Arc<Mutex<BTreeMap<ClientId, Uuid>>>` and
/// implementing [`bmux_client_state::ClientPrincipalLookup`] so
/// plugin handlers can query the principal for a caller client
/// when performing session-policy checks.
struct ServerClientPrincipalAdapter {
    inner: Arc<Mutex<BTreeMap<ClientId, Uuid>>>,
}

impl ServerClientPrincipalAdapter {
    const fn new(inner: Arc<Mutex<BTreeMap<ClientId, Uuid>>>) -> Self {
        Self { inner }
    }
}

impl bmux_client_state::ClientPrincipalLookup for ServerClientPrincipalAdapter {
    fn get(&self, client_id: ClientId) -> Option<Uuid> {
        self.inner.lock().ok()?.get(&client_id).copied()
    }

    fn set(&self, client_id: ClientId, principal_id: Uuid) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(client_id, principal_id);
        }
    }

    fn remove(&self, client_id: ClientId) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.remove(&client_id);
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn epoch_millis_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis() as u64
}

// `FollowState` / `FollowEntry` / `FollowTargetUpdate` are owned by the
// clients plugin. Server reaches follow-state through the
// domain-agnostic `FollowStateHandle` registered by the clients
// plugin's `activate` (or a no-op fallback registered by the server
// at startup when the plugin is absent). The helper below looks up
// the handle from the host's plugin state registry.
fn follow_handle(registry: &bmux_plugin::PluginStateRegistry) -> FollowStateHandle {
    registry
        .get::<FollowStateHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(FollowStateHandle::noop)
}

fn context_handle(registry: &bmux_plugin::PluginStateRegistry) -> ContextStateHandle {
    registry
        .get::<ContextStateHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(ContextStateHandle::noop)
}

fn session_handle(registry: &bmux_plugin::PluginStateRegistry) -> SessionManagerHandle {
    registry
        .get::<SessionManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(SessionManagerHandle::noop)
}

/// Look up the pane-runtime manager handle from the plugin state
/// registry. Returns the noop manager when the pane-runtime plugin
/// isn't loaded or its handle isn't registered yet.
fn session_runtime_handle(
    registry: &bmux_plugin::PluginStateRegistry,
) -> bmux_pane_runtime_state::SessionRuntimeManagerHandle {
    registry
        .get::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(bmux_pane_runtime_state::SessionRuntimeManagerHandle::noop)
}

/// Look up the snapshot orchestrator handle from the plugin state
/// registry. Returns the noop orchestrator when the snapshot plugin
/// isn't loaded or its handle isn't registered yet.
fn snapshot_orchestrator_handle(
    registry: &bmux_plugin::PluginStateRegistry,
) -> bmux_snapshot_runtime::SnapshotOrchestratorHandle {
    registry
        .get::<bmux_snapshot_runtime::SnapshotOrchestratorHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(bmux_snapshot_runtime::SnapshotOrchestratorHandle::noop)
}

/// Look up the shared snapshot dirty flag from the plugin state
/// registry. Returns a fresh detached flag when the snapshot plugin
/// isn't loaded or hasn't registered one yet — flipping it is a
/// harmless no-op in that case.
fn snapshot_dirty_flag(
    registry: &bmux_plugin::PluginStateRegistry,
) -> std::sync::Arc<bmux_snapshot_runtime::SnapshotDirtyFlag> {
    use bmux_snapshot_runtime::{SnapshotDirtyFlag, SnapshotDirtyFlagHandle};
    registry
        .get::<SnapshotDirtyFlagHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| std::sync::Arc::clone(&guard.0)))
        .unwrap_or_else(|| std::sync::Arc::new(SnapshotDirtyFlag::new()))
}

// `ContextState` / `RuntimeContext` are owned by the contexts plugin.
// The types live in
// `bmux_contexts_plugin::context_state`; the runtime handle is
// registered into the host's plugin state registry by
// `bmux_contexts_plugin::ContextsPlugin::activate`. Server code
// accesses the shared handle through `ServerState::context_state`,
// which is an `Arc<RwLock<ContextState>>` obtained from the plugin
// state registry.

fn current_context_session_for_client(
    state: &Arc<ServerState>,
    client_id: ClientId,
) -> Option<SessionId> {
    let _ = state;
    context_handle(&state.plugin_states)
        .0
        .current_session_for_client(client_id)
}

/// Register a no-op `FollowStateHandle` in the plugin state registry
/// **only if** one hasn't already been registered. The clients plugin
/// typically registers its real adapter during `activate` (which
/// happens before server construction), so this fallback only fires
/// in tests or headless scenarios where the plugin is absent.
fn register_noop_follow_state_handle(registry: &bmux_plugin::PluginStateRegistry) {
    if registry.get::<FollowStateHandle>().is_some() {
        return;
    }
    let handle = Arc::new(std::sync::RwLock::new(FollowStateHandle::noop()));
    registry.register::<FollowStateHandle>(&handle);
}

/// Register a no-op `ContextStateHandle` in the plugin state registry
/// only if the contexts plugin hasn't already registered its adapter.
fn register_noop_context_state_handle(registry: &bmux_plugin::PluginStateRegistry) {
    if registry.get::<ContextStateHandle>().is_some() {
        return;
    }
    let handle = Arc::new(std::sync::RwLock::new(ContextStateHandle::noop()));
    registry.register::<ContextStateHandle>(&handle);
}

/// Register a no-op `SessionManagerHandle` in the plugin state registry
/// only if the sessions plugin hasn't already registered its adapter.
fn register_noop_session_manager_handle(registry: &bmux_plugin::PluginStateRegistry) {
    if registry.get::<SessionManagerHandle>().is_some() {
        return;
    }
    let handle = Arc::new(std::sync::RwLock::new(SessionManagerHandle::noop()));
    registry.register::<SessionManagerHandle>(&handle);
}

/// Register the performance plugin's settings handle into the global
/// plugin state registry. Server constructs the initial settings from
/// the static config and shares a `PerformanceSettingsStore` with the
/// plugin so the plugin's typed `SetSettings` handler mutates the
/// record server's event-push rate limiter reads.
///
/// Returns the `PerformanceSettingsStore` so `ServerState` can hold a
/// clone directly for hot-path reads.
fn register_performance_plugin_state(
    registry: &bmux_plugin::PluginStateRegistry,
    config: &BmuxConfig,
) -> PerformanceSettingsStore {
    let store = PerformanceSettingsStore::from_config(config);
    let handle = Arc::new(std::sync::RwLock::new(PerformanceSettingsHandle::from_arc(
        Arc::new(store.clone()),
    )));
    registry.register::<PerformanceSettingsHandle>(&handle);
    store
}

impl BmuxServer {
    /// Record the BPDL access classes a provider declares for one routed
    /// service endpoint. Admitted streams reach only routes whose class is
    /// declared and authorized; registering a handler declares nothing.
    ///
    /// # Errors
    /// Returns an error when the declaration is an event service or the
    /// access bookkeeping is unavailable.
    pub fn declare_service_access(&self, service: &bmux_plugin_sdk::PluginService) -> Result<()> {
        let kind = match service.kind {
            bmux_plugin_sdk::ServiceKind::Query => bmux_ipc::InvokeServiceKind::Query,
            bmux_plugin_sdk::ServiceKind::Command => bmux_ipc::InvokeServiceKind::Command,
            bmux_plugin_sdk::ServiceKind::Event => {
                anyhow::bail!("event services are not invocable routes")
            }
        };
        self.state
            .route_access
            .lock()
            .map_err(|_| anyhow::anyhow!("route access bookkeeping unavailable"))?
            .insert(
                (
                    service.capability.as_str().to_string(),
                    kind,
                    service.interface_id.clone(),
                ),
                DeclaredAccess {
                    interface: service.access,
                    operations: service.operation_access.clone(),
                },
            );
        Ok(())
    }

    /// Record the access class a plugin declares for one of its commands.
    /// An admitted stream may run a plugin command through the CLI bridge
    /// only when the command declares a class its principal is granted.
    ///
    /// # Errors
    /// Returns an error when the access bookkeeping is unavailable.
    pub fn declare_command_access(
        &self,
        plugin_id: &str,
        command_name: &str,
        access: RouteAccess,
    ) -> Result<()> {
        self.state
            .command_access
            .lock()
            .map_err(|_| anyhow::anyhow!("command access bookkeeping unavailable"))?
            .insert((plugin_id.to_string(), command_name.to_string()), access);
        Ok(())
    }

    /// Bind an in-process host caller without inventing a local transport identity.
    #[must_use]
    pub fn host_service_context(&self) -> ServiceInvokeContext {
        ServiceInvokeContext {
            host_dispatch_context: None,
            host_internal: true,
            state: Arc::clone(&self.state),
            shutdown_tx: self.shutdown_tx.clone(),
            client_id: ClientId::new(),
            client_principal_id: Uuid::nil(),
            caller_plugin_id: None,
        }
    }

    /// Actual host-owned generic state context for in-process provider wiring.
    /// Pass this context explicitly; a plugin library's own global registry is
    /// not shared across dynamic-library or process boundaries.
    #[must_use]
    pub fn plugin_state_registry(&self) -> Arc<bmux_plugin::PluginStateRegistry> {
        Arc::clone(&self.state.plugin_states)
    }

    /// The host's root plugin event bus.
    ///
    /// Host code subscribes and relays on it directly; plugin activations
    /// receive scoped views through [`bmux_plugin::event_bus_activation_binder`].
    #[must_use]
    pub fn plugin_event_bus(&self) -> Arc<bmux_plugin::EventBus> {
        Arc::clone(&self.state.plugin_events)
    }
    fn new_with_snapshot(
        endpoint: IpcEndpoint,
        snapshot_manager: Option<()>,
        _shell_integration_root: Option<std::path::PathBuf>,
        server_control_principal_id: Uuid,
    ) -> Self {
        // `snapshot_manager` is retained on the API for backward
        // compatibility but is ignored: snapshot persistence is owned
        // by `bmux_snapshot_plugin` via `SnapshotPluginConfig`
        // registered by CLI bootstrap.
        let _ = snapshot_manager;

        let config = BmuxConfig::load().unwrap_or_default();
        let (shutdown_tx, _) = watch::channel(false);

        // The recording plugin owns construction of `RecordingRuntime`
        // instances and registers its own sink / runtime handles /
        // config during its `activate` callback. CLI bootstrap
        // registered `RecordingPluginConfig` before plugin activation
        // so the plugin has the config paths it needs at that time.

        // Register the performance plugin's settings handle into the
        // plugin state registry. Server's event-push rate limiter
        // reads the registered handle, and the performance plugin's
        // `SetSettings` handler writes into it.
        // The host's root plugin state registry. Plugins only ever receive
        // views of it through their activation capabilities.
        let plugin_states = Arc::new(bmux_plugin::PluginStateRegistry::new());
        let performance_settings = register_performance_plugin_state(&plugin_states, &config);

        // Register a no-op FollowStateHandle so server's handle
        // lookups always succeed. The clients plugin overwrites this
        // during `activate`.
        register_noop_follow_state_handle(&plugin_states);
        register_noop_context_state_handle(&plugin_states);
        register_noop_session_manager_handle(&plugin_states);

        let (event_broadcast_tx, _) =
            tokio::sync::broadcast::channel::<Event>(EVENT_PUSH_CHANNEL_CAPACITY);

        let attach_tokens = Arc::new(Mutex::new(AttachTokenManager::new(ATTACH_TOKEN_TTL)));
        let stream_provenance = bmux_plugin::stream_admission::StreamProvenanceRegistry::default();
        plugin_states.register(&Arc::new(std::sync::RwLock::new(stream_provenance.clone())));
        let client_principals: Arc<Mutex<BTreeMap<ClientId, Uuid>>> =
            Arc::new(Mutex::new(BTreeMap::new()));

        // Remote transports (plugins) hand authenticated streams to this
        // server through the ingress handle, registered before any plugin
        // activates. They share the local connection limit.
        let transport_admission = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel();
        let ingress = StreamIngressHandle::new(Arc::new(ServerStreamIngress {
            admission: Arc::clone(&transport_admission),
            queue: ingress_tx,
            shutdown: shutdown_tx.subscribe(),
        }));
        plugin_states.register::<StreamIngressHandle>(&Arc::new(std::sync::RwLock::new(ingress)));

        // Register server's `AttachTokenManager` so the pane-runtime
        // plugin can issue/consume tokens during attach orchestration
        // without owning its own copy.
        let attach_token_handle = bmux_attach_token_state::AttachTokenManagerHandle::new(
            ServerAttachTokenAdapter::new(Arc::clone(&attach_tokens)),
        );
        plugin_states.register::<bmux_attach_token_state::AttachTokenManagerHandle>(&Arc::new(
            std::sync::RwLock::new(attach_token_handle),
        ));

        // Register server's per-client principal map so plugin
        // handlers can look up the principal for the caller client
        // when performing session-policy checks.
        let principal_handle = bmux_client_state::ClientPrincipalHandle::new(
            ServerClientPrincipalAdapter::new(Arc::clone(&client_principals)),
        );
        plugin_states.register::<bmux_client_state::ClientPrincipalHandle>(&Arc::new(
            std::sync::RwLock::new(principal_handle),
        ));

        Self {
            endpoint,
            ipc_security: bmux_ipc::transport::UnixSecurityPolicy::strict(),
            state: Arc::new(ServerState {
                local_terminal_bound: std::sync::OnceLock::new(),
                attach_tokens,
                performance_settings,
                operation_lock: AsyncMutex::new(()),
                event_hub: Mutex::new(EventHub::new(1024)),
                event_broadcast: event_broadcast_tx,
                event_push_clients: AsyncMutex::new(BTreeSet::new()),
                retained_wire_events: Arc::new(RetainedWireEvents::default()),
                route_access: Mutex::default(),
                command_access: Mutex::default(),
                transport_admission,
                admitted_ingress: Mutex::new(Some(ingress_rx)),
                stream_provenance,
                plugin_states,
                plugin_events: Arc::new(bmux_plugin::EventBus::new()),
                client_principals,
                server_control_principal_id,
                handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
                service_registry: Mutex::new(ServiceRegistry::default()),
                service_resolver: Mutex::new(None),
                plugin_state_replayers: Mutex::new(Vec::new()),
                device_seal_pending: AsyncMutex::new(BTreeMap::new()),
            }),
            shutdown_tx,
        }
    }

    /// Select the generic local transport policy without changing endpoint identity.
    #[must_use]
    pub const fn with_ipc_security(
        mut self,
        policy: bmux_ipc::transport::UnixSecurityPolicy,
    ) -> Self {
        self.ipc_security = policy;
        self
    }

    /// Create a server with an explicit endpoint.
    #[must_use]
    pub fn new(endpoint: IpcEndpoint) -> Self {
        Self::new_with_snapshot(endpoint, None, None, Uuid::new_v4())
    }

    /// Create a server with endpoint derived from config paths.
    #[must_use]
    pub fn from_config_paths(paths: &ConfigPaths) -> Self {
        let config = BmuxConfig::load_from_path(&paths.config_file()).unwrap_or_default();
        let rolling_defaults = rolling_recording_settings_from_config(&config);
        Self::from_config_paths_with_rolling_options(
            paths,
            config.recording.enabled,
            rolling_defaults.window_secs,
            &rolling_defaults.event_kinds,
        )
    }

    /// Create a server with endpoint derived from config paths and explicit
    /// rolling-recording boot options.
    #[must_use]
    pub fn from_config_paths_with_rolling_options(
        paths: &ConfigPaths,
        rolling_recording_auto_start: bool,
        rolling_window_secs: u64,
        rolling_event_kinds: &[RecordingEventKind],
    ) -> Self {
        Self::from_config_paths_with_start_options(
            paths,
            rolling_recording_auto_start,
            rolling_window_secs,
            rolling_event_kinds,
            None,
        )
    }

    /// Create a server with endpoint derived from config paths and
    /// optional shell integration override.
    ///
    /// Rolling-recording auto-start and related options are consumed
    /// by the recording plugin during its `activate` callback; server
    /// no longer owns recording-domain startup.
    #[must_use]
    pub fn from_config_paths_with_start_options(
        paths: &ConfigPaths,
        _rolling_recording_auto_start: bool,
        _rolling_window_secs: u64,
        _rolling_event_kinds: &[RecordingEventKind],
        pane_shell_integration_override: Option<bool>,
    ) -> Self {
        let config = BmuxConfig::load_from_path(&paths.config_file()).unwrap_or_default();
        #[cfg(unix)]
        let endpoint = IpcEndpoint::unix_socket(paths.server_socket());

        #[cfg(windows)]
        let endpoint = IpcEndpoint::windows_named_pipe(paths.server_named_pipe());

        // `snapshot_manager` is no longer used by server — the
        // snapshot plugin owns persistence now. The parameter is
        // retained on the API for backward compatibility until
        // callers are migrated; pass `None` here.
        let snapshot_manager: Option<()> = None;
        let server_control_principal_id =
            load_or_create_principal_id(paths).unwrap_or_else(|error| {
                warn!("failed loading server control principal id: {error}");
                Uuid::new_v4()
            });
        let shell_integration_root = pane_shell_integration_override
            .unwrap_or(config.behavior.pane_shell_integration)
            .then(|| paths.state_dir().join("runtime").join("shell-integration"));
        Self::new_with_snapshot(
            endpoint,
            snapshot_manager,
            shell_integration_root,
            server_control_principal_id,
        )
        .with_ipc_security(ConfigPaths::local_ipc_policy())
    }

    /// Create a server using default bmux config paths.
    #[must_use]
    pub fn from_default_paths() -> Self {
        Self::from_config_paths(&ConfigPaths::default())
    }

    /// Register a generic service invocation handler.
    ///
    /// Handlers are matched by exact `(capability, kind, interface_id, operation)`.
    ///
    /// # Errors
    /// Returns an error if the service registry lock is poisoned.
    pub fn register_service_handler<F, Fut>(
        &self,
        capability: impl Into<String>,
        kind: bmux_ipc::InvokeServiceKind,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(ServiceRoute, ServiceInvokeContext, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<u8>>> + Send + 'static,
    {
        self.register_service_handler_with_metadata(
            capability,
            kind,
            interface_id,
            operation,
            move |route, context, payload| {
                let future = handler(route, context, payload);
                async move { future.await.map(ServiceInvokeOutput::from) }
            },
        )
    }

    /// Register a generic service invocation handler that can return metadata.
    ///
    /// # Errors
    /// Returns an error if the service registry lock is poisoned.
    pub fn register_service_handler_with_metadata<F, Fut>(
        &self,
        capability: impl Into<String>,
        kind: bmux_ipc::InvokeServiceKind,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(ServiceRoute, ServiceInvokeContext, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ServiceInvokeOutput>> + Send + 'static,
    {
        self.register_service_handler_boundary(
            ServiceRoute {
                capability: capability.into(),
                kind,
                interface_id: interface_id.into(),
                operation: operation.into(),
            },
            false,
            handler,
        )
    }

    /// Register a handler that forwards checked frames to a provider-side generated
    /// contract validator before any provider code executes. Do not use this for
    /// legacy/manual handlers. Registration and admission policy share one lock.
    ///
    /// # Errors
    /// Returns an error if the service registry lock is poisoned.
    pub fn register_checked_service_handler_with_metadata<F, Fut>(
        &self,
        capability: impl Into<String>,
        kind: bmux_ipc::InvokeServiceKind,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(ServiceRoute, ServiceInvokeContext, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ServiceInvokeOutput>> + Send + 'static,
    {
        self.register_service_handler_boundary(
            ServiceRoute {
                capability: capability.into(),
                kind,
                interface_id: interface_id.into(),
                operation: operation.into(),
            },
            true,
            handler,
        )
    }

    fn register_service_handler_boundary<F, Fut>(
        &self,
        route: ServiceRoute,
        checked: bool,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(ServiceRoute, ServiceInvokeContext, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ServiceInvokeOutput>> + Send + 'static,
    {
        let wrapped: Arc<ServiceInvokeHandler> =
            Arc::new(move |route, context, payload| Box::pin(handler(route, context, payload)));
        let mut registry = self
            .state
            .service_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("service registry lock poisoned"))?;
        if checked {
            registry.checked_handlers.insert(route.clone());
        } else {
            registry.checked_handlers.remove(&route);
        }
        registry.handlers.insert(route, wrapped);
        drop(registry);
        Ok(())
    }

    /// Register a generic fallback resolver for service routes that are not
    /// explicitly present in the service registry.
    ///
    /// # Errors
    /// Returns an error if the service resolver lock is poisoned.
    pub fn set_service_resolver<F, Fut>(&self, resolver: F) -> Result<()>
    where
        F: Fn(ServiceRoute, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<u8>>> + Send + 'static,
    {
        let wrapped: Arc<ServiceResolverHandler> = Arc::new(move |route, payload| {
            let future = resolver(route, payload);
            Box::pin(async move { future.await.map(ServiceInvokeOutput::from) })
        });
        *self
            .state
            .service_resolver
            .lock()
            .map_err(|_| anyhow::anyhow!("service resolver lock poisoned"))? = Some(wrapped);
        Ok(())
    }

    /// Advertise the host's bound single-terminal transport in local handshakes.
    pub fn advertise_local_terminal(&self) {
        self.state.local_terminal_bound.get_or_init(|| ());
    }

    /// Return the configured endpoint.
    #[must_use]
    pub const fn endpoint(&self) -> &IpcEndpoint {
        &self.endpoint
    }

    /// Request server shutdown.
    pub fn request_shutdown(&self) {
        // Retain shutdown intent even before run() installs its receiver.
        self.shutdown_tx.send_replace(true);
    }

    /// Spawn a forwarder that subscribes to the process-local plugin
    /// event bus for `kind` and broadcasts each emission to every
    /// streaming client as [`Event::PluginBusEvent`].
    ///
    /// Bootstrap invokes this once per plugin declaration that opts
    /// into `forward_to_streaming_clients` in its manifest. The task
    /// holds a [`Weak<ServerState>`] so it terminates cleanly when
    /// the server shuts down.
    ///
    /// # Errors
    ///
    /// Returns an error when the event bus rejects the subscription
    /// request (e.g. no matching channel is registered). Callers may
    /// choose to warn and continue — unregistered channels usually
    /// mean the emitting plugin hasn't activated yet, which is fine
    /// because the bus lets subscribers predate the channel in
    /// `subscribe_waiting` mode. We use the strict `subscribe` here
    /// to surface misconfigurations loudly.
    pub fn spawn_plugin_bus_forwarder<T>(
        &self,
        kind: &bmux_plugin_sdk::PluginEventKind,
    ) -> Result<()>
    where
        T: Clone + Send + Sync + 'static + serde::Serialize + serde::de::DeserializeOwned,
    {
        // Subscribe to the plugin-owned channel with its typed payload.
        // The plugin registers the channel with the typed struct at
        // activation time; consumers (this forwarder included) must
        // pass the matching `T` for the bus to hand out receivers.
        let mut rx = self
            .state
            .plugin_events
            .subscribe::<T>(kind)
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed subscribing to plugin bus kind {kind:?} for streaming forward: {error}"
                )
            })?;
        let kind_string = kind.as_str().to_string();
        let state = Arc::downgrade(&self.state);
        tokio::spawn(async move {
            loop {
                let Ok(payload) = rx.recv().await else {
                    return;
                };
                let Some(state) = state.upgrade() else {
                    return;
                };
                let encoded = match serde_json::to_vec(payload.as_ref()) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(
                            kind = %kind_string,
                            error = %error,
                            "failed encoding plugin bus event payload; dropping",
                        );
                        continue;
                    }
                };
                let event = Event::PluginBusEvent {
                    kind: kind_string.clone(),
                    payload: encoded,
                };
                if let Err(error) = emit_event(&state, event) {
                    tracing::warn!(
                        kind = %kind_string,
                        error = %error,
                        "failed emitting forwarded plugin bus event",
                    );
                }
            }
        });
        Ok(())
    }

    /// Spawn a forwarder for a plugin state channel (the `@state
    /// events` variant in BPDL). Mirrors
    /// [`Self::spawn_plugin_bus_forwarder`] but drives off
    /// [`tokio::sync::watch::Receiver::changed`] so every snapshot
    /// replacement fans out as an [`Event::PluginBusEvent`].
    ///
    /// Registers a replay hook for the state channel's currently-retained value
    /// so late-connecting streaming clients see the current snapshot when they
    /// enable event push, without waiting for the next mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the event bus rejects the state
    /// subscription (unregistered channel, wrong payload type, or
    /// delivery-mode mismatch — a broadcast channel registered under
    /// this kind would fail here loudly rather than silently misroute).
    pub fn spawn_plugin_bus_state_forwarder<T>(
        &self,
        kind: &bmux_plugin_sdk::PluginEventKind,
    ) -> Result<()>
    where
        T: Clone + Send + Sync + 'static + serde::Serialize + serde::de::DeserializeOwned,
    {
        let (_initial, mut rx) = self
            .state
            .plugin_events
            .subscribe_state::<T>(kind)
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed subscribing to plugin bus state kind {kind:?} for streaming forward: \
                     {error}"
                )
            })?;
        let kind_string = kind.as_str().to_string();
        let state = Arc::downgrade(&self.state);
        let replay_kind = kind.clone();
        let replay_kind_string = kind_string.clone();
        let replay_bus = Arc::downgrade(&self.state.plugin_events);
        if let Ok(mut replayers) = self.state.plugin_state_replayers.lock() {
            replayers.push(Arc::new(move || {
                let (current, _rx) = replay_bus
                    .upgrade()?
                    .subscribe_state::<T>(&replay_kind)
                    .ok()?;
                let encoded = match serde_json::to_vec(current.as_ref()) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(
                            kind = %replay_kind_string,
                            error = %error,
                            "failed encoding retained plugin bus state for client replay",
                        );
                        return None;
                    }
                };
                Some(Event::PluginBusEvent {
                    kind: replay_kind_string.clone(),
                    payload: encoded,
                })
            }));
        }

        // Do not emit `_initial` here: this forwarder starts during server
        // bootstrap, before attach clients have connected. Instead, register a
        // replay closure above so each client receives the retained value when
        // it enables event-push delivery, then forward live changes below.

        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(state_ref) = state.upgrade() else {
                    return;
                };
                let snapshot = rx.borrow().clone();
                let encoded = match serde_json::to_vec(snapshot.as_ref()) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(
                            kind = %kind_string,
                            error = %error,
                            "failed encoding plugin bus state payload; dropping",
                        );
                        continue;
                    }
                };
                let event = Event::PluginBusEvent {
                    kind: kind_string.clone(),
                    payload: encoded,
                };
                if let Err(error) = emit_event(&state_ref, event) {
                    tracing::warn!(
                        kind = %kind_string,
                        error = %error,
                        "failed emitting forwarded plugin bus state event",
                    );
                }
            }
        });
        Ok(())
    }

    /// Forward a plugin's serialized (JSON) state channel to streaming
    /// clients, for kinds the server has no typed specialization for.
    ///
    /// Unlike the typed forwarders, this waits for the channel to be
    /// registered and subscribes again whenever it is re-registered (a
    /// plugin reload replaces the channel), so declaring a forwarded kind is
    /// enough for it to reach clients. State that was never published
    /// (`null`) is not sent. Newly attached clients receive the retained
    /// value through the same replay as typed forwarders.
    ///
    /// # Errors
    /// Never; kept fallible for symmetry with the typed forwarders.
    pub fn spawn_plugin_bus_json_state_forwarder(
        &self,
        kind: &bmux_plugin_sdk::PluginEventKind,
    ) -> Result<()> {
        let kind_string = kind.as_str().to_string();
        let replay_kind = kind.clone();
        let replay_kind_string = kind_string.clone();
        let replay_bus = Arc::downgrade(&self.state.plugin_events);
        if let Ok(mut replayers) = self.state.plugin_state_replayers.lock() {
            replayers.push(Arc::new(move || {
                let (current, _rx) = replay_bus
                    .upgrade()?
                    .subscribe_state::<serde_json::Value>(&replay_kind)
                    .ok()?;
                if current.is_null() {
                    return None;
                }
                Some(Event::PluginBusEvent {
                    kind: replay_kind_string.clone(),
                    payload: serde_json::to_vec(current.as_ref()).ok()?,
                })
            }));
        }
        let state = Arc::downgrade(&self.state);
        let bus = Arc::downgrade(&self.state.plugin_events);
        let kind = kind.clone();
        tokio::spawn(async move {
            loop {
                let Some(events) = bus.upgrade() else {
                    return;
                };
                let subscription = events
                    .subscribe_state_when_registered::<serde_json::Value>(&kind)
                    .await;
                drop(events);
                let (initial, mut rx) = match subscription {
                    Ok(subscribed) => subscribed,
                    Err(error) => {
                        tracing::warn!(
                            kind = %kind_string,
                            error = %error,
                            "plugin bus kind is not a serialized state channel; not forwarding",
                        );
                        return;
                    }
                };
                // What was published before this subscription (a plugin may
                // publish as soon as it registers) is forwarded first; a
                // replaced channel drops its sender, so the loop then ends and
                // the next pass subscribes to the new one.
                let mut snapshot = Some(initial);
                loop {
                    if let Some(value) = snapshot.take()
                        && !value.is_null()
                    {
                        let Some(state_ref) = state.upgrade() else {
                            return;
                        };
                        forward_json_state(&state_ref, &kind_string, &value);
                    }
                    if rx.changed().await.is_err() {
                        break;
                    }
                    snapshot = Some(rx.borrow_and_update().clone());
                }
            }
        });
        Ok(())
    }

    /// Run the server accept loop until shutdown is requested.
    ///
    /// # Errors
    ///
    /// Returns an error if binding or accept-loop operations fail.
    pub async fn run(&self) -> Result<()> {
        self.run_impl(None).await
    }

    /// Run with a one-shot readiness notification after initialization and restore.
    /// The notification is not a substitute for an authenticated client handshake.
    ///
    /// # Errors
    /// Returns binding, initialization, and accept-loop failures.
    pub async fn run_with_readiness(
        &self,
        ready: oneshot::Sender<std::result::Result<(), String>>,
    ) -> Result<()> {
        self.run_impl(Some(ready)).await
    }

    async fn run_impl(
        &self,
        mut ready_tx: Option<oneshot::Sender<std::result::Result<(), String>>>,
    ) -> Result<()> {
        let listener = match LocalIpcListener::bind_with_policy(&self.endpoint, self.ipc_security)
            .with_context(|| format!("failed binding server endpoint {:?}", self.endpoint))
        {
            Ok(listener) => listener,
            Err(error) => {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(format!("{error:#}")));
                }
                return Err(error);
            }
        };

        self.run_with_listener(listener, ready_tx).await
    }

    /// Run an already-bound listener through initialization and the accept loop.
    /// The caller can persist bind ownership before initialization begins.
    ///
    /// # Errors
    ///
    /// Returns initialization and accept-loop failures.
    #[allow(clippy::too_many_lines)] // Initialization and shutdown share one listener lifetime.
    pub async fn run_with_listener(
        &self,
        listener: LocalIpcListener,
        mut ready_tx: Option<oneshot::Sender<std::result::Result<(), String>>>,
    ) -> Result<()> {
        // Snapshot restore runs after plugin activation so every participant
        // is registered before the orchestrator walks the envelope.

        // Rolling-recording auto-start and hourly prune loop both
        // moved into the recording plugin's `activate` callback;
        // server no longer constructs `RecordingRuntime` or schedules
        // recording-domain background tasks.

        info!("bmux server listening on {:?}", self.endpoint);
        emit_event(&self.state, Event::ServerStarted)?;
        // Register the wire-event sink so plugins can publish wire
        // events through the `WireEventSinkHandle` trait object in the
        // plugin state registry, without depending on `packages/server`.
        register_wire_event_sink(&self.state);

        // Now that every stateful participant is registered (clients
        // + contexts + sessions + pane-runtime via plugin activate
        // callbacks), ask the snapshot plugin
        // to restore the previous session's state if a snapshot file
        // exists on disk.
        let restore_handle = snapshot_orchestrator_handle(&self.state.plugin_states);
        match restore_handle.as_dyn().restore_if_present_boxed().await {
            Ok(Some(summary)) => {
                info!(
                    "bmux snapshot restored: {} plugins ok, {} failed",
                    summary.restored_plugins, summary.failed_plugins
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    "bmux snapshot restore failed; persisted snapshot writes remain blocked: {error}"
                );
            }
        }

        if let Some(tx) = ready_tx.take() {
            let _ = tx.send(Ok(()));
        }
        // Snapshot debounce + flush loop is owned by the snapshot
        // plugin's activate (see `plugins/snapshot-plugin/src/lib.rs`).
        // Server no longer spawns its own tokio flush task.

        // Admit before spawning: overload must not create an unbounded set of
        // waiting handler tasks, each with its own frame queue and encoder.
        let transport_admission = Arc::clone(&self.state.transport_admission);
        let mut admitted_ingress = self
            .state
            .admitted_ingress
            .lock()
            .map_err(|_| anyhow::anyhow!("admitted ingress lock poisoned"))?
            .take()
            .context("server accept loop already ran")?;
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut connections = tokio::task::JoinSet::new();
        let shutdown_reason = loop {
            if *shutdown_rx.borrow() {
                break "graceful_shutdown_requested";
            }
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        info!("bmux server shutdown requested");
                        break "graceful_shutdown_requested";
                    }
                    if changed.is_err() {
                        break "shutdown_channel_closed";
                    }
                }
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = result { warn!(%error, "connection task panicked"); }
                }
                Some(admitted) = admitted_ingress.recv() => {
                    let AdmittedConnection { stream, binding, slot } = admitted;
                    let state = Arc::clone(&self.state);
                    let shutdown_tx = self.shutdown_tx.clone();
                    connections.spawn(async move {
                        if let Err(error) = handle_connection(state, shutdown_tx, stream, Some(binding), Arc::new(slot)).await {
                            warn!("admitted connection handler failed: {error:#}");
                        }
                    });
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok(stream) => {
                            let Some(slot) = ConnectionSlot::try_acquire(&transport_admission) else {
                                warn!("transport admission exhausted; rejecting connection");
                                continue;
                            };
                            let slot = Arc::new(slot);
                            let state = Arc::clone(&self.state);
                            let shutdown_tx = self.shutdown_tx.clone();
                            connections.spawn(async move {
                                if let Err(error) = handle_connection(state, shutdown_tx, stream.into(), None, slot).await {
                                    warn!("connection handler failed: {error:#}");
                                }
                            });
                        }
                        Err(error) => {
                            warn!(
                                "bmux server listener accept failed on {:?}: {error:#}",
                                self.endpoint
                            );
                            return Err(error).context("accept loop failed");
                        }
                    }
                }
            }
        };

        info!(
            "bmux server listener closing on {:?} (reason: {shutdown_reason})",
            self.endpoint
        );

        let progress = self.state.plugin_states.shutdown_progress();
        if let Err(error) = progress.begin() {
            warn!(%error, "shutdown progress publication failed");
        }
        drop(listener);
        // Queued admitted streams are closed; later ones are refused.
        drop(admitted_ingress);
        info!(
            outstanding = connections.len(),
            "shutdown draining admitted requests"
        );
        while let Some(result) = connections.join_next().await {
            if let Err(error) = progress.advance("admitted request drained") {
                warn!(%error, "shutdown progress publication failed");
            }
            if let Err(error) = result {
                warn!(%error, "connection task failed during shutdown");
            }
        }
        if let Err(error) = progress.advance("admitted requests drained") {
            warn!(%error, "shutdown progress publication failed");
        }
        info!("shutdown admitted requests drained");

        // The host drives only neutral participants. Persistence and resource
        // policy, including ordering, belong to their owning plugins.
        let lifecycle = self
            .state
            .plugin_states
            .get::<bmux_plugin_runtime::shutdown::ShutdownRegistry>()
            .map(|entry| {
                entry
                    .read()
                    .map(|registry| registry.clone())
                    .map_err(|_| anyhow::anyhow!("shutdown registry poisoned"))
            })
            .transpose()?;
        let shutdown_result = if let Some(lifecycle) = lifecycle {
            lifecycle
                .run_with_progress(progress.clone())
                .await
                .map_err(anyhow::Error::msg)
        } else {
            Ok(())
        };
        if let Err(error) = progress.advance("lifecycle participants settled") {
            warn!(%error, "shutdown progress publication failed");
        }
        let _ = emit_event(&self.state, Event::ServerStopping);
        if let Ok(mut attach_tokens) = self.state.attach_tokens.lock() {
            attach_tokens.clear();
        }

        info!("bmux server listener closed on {:?}", self.endpoint);
        flush_phase_timing();

        shutdown_result
    }
}

#[cfg(test)]
#[allow(
    clippy::significant_drop_tightening,
    reason = "Each test holds its server for the whole scenario it drives."
)]
mod admission_tests {
    use super::*;
    use bmux_plugin_sdk::RemoteAccessAuthorizer;

    const CAPABILITY: &str = "example.remote";
    const INTERFACE: &str = "remote-test/v1";

    /// Allows the listed classes for every subject.
    struct Allow(Vec<RouteAccess>);
    impl RemoteAccessAuthorizer for Allow {
        fn allows(&self, _: Uuid, access: RouteAccess) -> bool {
            self.0.contains(&access)
        }
    }

    fn register_authorizer(server: &BmuxServer, allowed: Vec<RouteAccess>) {
        server
            .state
            .plugin_states
            .register::<RemoteAccessAuthorizerHandle>(&Arc::new(std::sync::RwLock::new(
                RemoteAccessAuthorizerHandle::new(Arc::new(Allow(allowed))),
            )));
    }

    /// One query interface: `read` (observe, inherited), `write` (input
    /// override), `admin` (owner-local override), `bare` (undeclared on an
    /// interface that declares nothing is tested separately).
    fn server_with_routes(effects: &Arc<std::sync::atomic::AtomicUsize>) -> BmuxServer {
        let server = BmuxServer::new(IpcEndpoint::unix_socket("remote-access-test"));
        for operation in ["read", "write", "admin"] {
            let effects = Arc::clone(effects);
            server
                .register_service_handler(
                    CAPABILITY,
                    bmux_ipc::InvokeServiceKind::Query,
                    INTERFACE,
                    operation,
                    move |_, _, _| {
                        effects.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        async { Ok(vec![7]) }
                    },
                )
                .unwrap();
        }
        server
            .register_service_handler(
                CAPABILITY,
                bmux_ipc::InvokeServiceKind::Query,
                "undeclared/v1",
                "read",
                |_, _, _| async { Ok(vec![1]) },
            )
            .unwrap();
        server
            .declare_service_access(&bmux_plugin_sdk::PluginService {
                capability: bmux_plugin_sdk::HostScope::new(CAPABILITY).unwrap(),
                kind: bmux_plugin_sdk::ServiceKind::Query,
                interface_id: INTERFACE.into(),
                concurrency: None,
                reentrant_operations: BTreeSet::new(),
                access: Some(RouteAccess::Observe),
                operation_access: BTreeMap::from([
                    ("write".to_string(), RouteAccess::Input),
                    ("admin".to_string(), RouteAccess::OwnerLocal),
                ]),
            })
            .unwrap();
        server
    }

    fn hello(principal_id: Uuid) -> Request {
        Request::Hello {
            contract: ProtocolContract::current(
                CORE_PROTOCOL_CAPABILITIES
                    .iter()
                    .map(|value| (*value).to_string())
                    .collect(),
            ),
            client_name: "admission-test".into(),
            principal_id,
        }
    }

    fn invoke(interface_id: &str, operation: &str) -> Request {
        Request::InvokeService {
            capability: CAPABILITY.into(),
            kind: bmux_ipc::InvokeServiceKind::Query,
            interface_id: interface_id.into(),
            operation: operation.into(),
            payload: Vec::new(),
        }
    }

    async fn call(peer: &mut ErasedIpcStream, id: u64, request: &Request) -> Response {
        peer.send_envelope(&Envelope::new(
            id,
            EnvelopeKind::Request,
            encode(request).unwrap(),
        ))
        .await
        .unwrap();
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), peer.recv_envelope())
                .await
                .expect("response in time")
                .unwrap();
            if envelope.kind == EnvelopeKind::Response {
                assert_eq!(envelope.request_id, id);
                return decode(&envelope.payload).unwrap();
            }
        }
    }

    fn denied(response: &Response) -> bool {
        matches!(
            response,
            Response::Err(ErrorResponse {
                code: ErrorCode::PermissionDenied,
                ..
            })
        )
    }

    /// Serve one admitted in-memory stream directly; returns the client end.
    fn connect_admitted(
        server: &BmuxServer,
        binding: Arc<StreamAdmissionBinding>,
    ) -> (ErasedIpcStream, tokio::task::JoinHandle<Result<()>>) {
        let (client, host) = tokio::io::duplex(1 << 20);
        let permit = ConnectionSlot::try_acquire(&server.state.transport_admission).unwrap();
        let task = tokio::spawn(handle_connection(
            Arc::clone(&server.state),
            server.shutdown_tx.clone(),
            ErasedIpcStream::new(Box::new(host)),
            Some(binding),
            Arc::new(permit),
        ));
        (ErasedIpcStream::new(Box::new(client)), task)
    }

    async fn admitted_peer(server: &BmuxServer, subject: Uuid) -> ErasedIpcStream {
        let (binding, _revocation) = StreamAdmissionBinding::new(subject, Uuid::new_v4());
        let (mut peer, _task) = connect_admitted(server, binding);
        assert!(matches!(
            call(&mut peer, 1, &hello(subject)).await,
            Response::Ok(ResponsePayload::HelloNegotiated { .. })
        ));
        peer
    }

    #[tokio::test]
    async fn admitted_hello_must_name_the_bound_subject() {
        let effects = Arc::default();
        let server = server_with_routes(&effects);
        register_authorizer(&server, vec![RouteAccess::Observe]);
        let subject = Uuid::new_v4();
        let (binding, _revocation) = StreamAdmissionBinding::new(subject, Uuid::new_v4());
        let (mut peer, task) = connect_admitted(&server, binding);
        peer.send_envelope(&Envelope::new(
            1,
            EnvelopeKind::Request,
            encode(&hello(Uuid::new_v4())).unwrap(),
        ))
        .await
        .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("principal mismatch"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn admitted_stream_gets_kernel_requests_by_class_and_no_local_terminal() {
        let effects = Arc::default();
        let server = server_with_routes(&effects);
        server.advertise_local_terminal();
        let subject = Uuid::new_v4();
        // Without an authorizer nothing is allowed, not even the handshake.
        let (binding, _revocation) = StreamAdmissionBinding::new(subject, Uuid::new_v4());
        let (mut refused, task) = connect_admitted(&server, binding);
        assert!(denied(&call(&mut refused, 1, &hello(subject)).await));
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // A grant without observe does not admit the handshake either.
        register_authorizer(&server, vec![RouteAccess::Input]);
        let (binding, _revocation) = StreamAdmissionBinding::new(subject, Uuid::new_v4());
        let (mut refused, _task) = connect_admitted(&server, binding);
        assert!(denied(&call(&mut refused, 1, &hello(subject)).await));

        register_authorizer(&server, vec![RouteAccess::Observe]);
        let (binding, _revocation) = StreamAdmissionBinding::new(subject, Uuid::new_v4());
        let (mut peer, _task) = connect_admitted(&server, binding);
        match call(&mut peer, 1, &hello(subject)).await {
            Response::Ok(ResponsePayload::HelloNegotiated { negotiated }) => assert!(
                !negotiated
                    .capabilities
                    .iter()
                    .any(|capability| capability == bmux_ipc::CAPABILITY_LOCAL_TERMINAL_V1)
            ),
            other => panic!("unexpected hello response: {other:?}"),
        }
        // Grants are read per request: without observe, Ping is refused.
        register_authorizer(&server, vec![RouteAccess::Input]);
        assert!(denied(&call(&mut peer, 2, &Request::Ping).await));

        register_authorizer(&server, RouteAccess::ALL.to_vec());
        assert_eq!(
            call(&mut peer, 3, &Request::Ping).await,
            Response::Ok(ResponsePayload::Pong)
        );
        assert_eq!(
            call(&mut peer, 4, &Request::EnableEventPush).await,
            Response::Ok(ResponsePayload::EventPushEnabled)
        );
        assert!(matches!(
            call(&mut peer, 5, &Request::SubscribeEvents).await,
            Response::Ok(_)
        ));
        // Owner-local whatever the authorizer says: the server keeps running.
        assert!(denied(&call(&mut peer, 6, &Request::ServerStop).await));
        assert!(!*server.shutdown_tx.subscribe().borrow());
        assert!(denied(
            &call(&mut peer, 7, &Request::DeviceSealBroker { payload: vec![] }).await
        ));

        // View-only: observe yes, input (plugin bus emission) no.
        register_authorizer(&server, vec![RouteAccess::Observe]);
        assert_eq!(
            call(&mut peer, 8, &Request::Ping).await,
            Response::Ok(ResponsePayload::Pong)
        );
        assert!(denied(
            &call(
                &mut peer,
                9,
                &Request::EmitOnPluginBus {
                    kind: "example.kind".into(),
                    payload: vec![],
                },
            )
            .await
        ));
    }

    #[test]
    fn kernel_request_classes_are_fixed() {
        assert_eq!(
            kernel_request_access(&Request::Ping),
            Some(RouteAccess::Observe)
        );
        assert_eq!(
            kernel_request_access(&Request::PollEvents { max_events: 1 }),
            Some(RouteAccess::Observe)
        );
        assert_eq!(
            kernel_request_access(&Request::EmitOnPluginBus {
                kind: String::new(),
                payload: vec![],
            }),
            Some(RouteAccess::Input)
        );
        assert_eq!(
            kernel_request_access(&Request::ServerStop),
            Some(RouteAccess::OwnerLocal)
        );
        assert_eq!(kernel_request_access(&invoke(INTERFACE, "read")), None);
    }

    #[tokio::test]
    async fn admitted_service_routes_follow_declared_classes() {
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = server_with_routes(&effects);
        let subject = Uuid::new_v4();
        let ok = Response::Ok(ResponsePayload::ServiceInvoked { payload: vec![7] });
        register_authorizer(&server, vec![RouteAccess::Observe]);
        let mut peer = admitted_peer(&server, subject).await;
        // A grant without observe: even a declared observe route is denied.
        register_authorizer(&server, vec![RouteAccess::Input]);
        assert!(denied(
            &call(&mut peer, 2, &invoke(INTERFACE, "read")).await
        ));

        register_authorizer(&server, vec![RouteAccess::Observe]);
        assert_eq!(call(&mut peer, 3, &invoke(INTERFACE, "read")).await, ok);
        // Per-operation override to input, not granted to view-only.
        assert!(denied(
            &call(&mut peer, 4, &invoke(INTERFACE, "write")).await
        ));
        // A route with no declared class is never reachable remotely.
        assert!(denied(
            &call(&mut peer, 5, &invoke("undeclared/v1", "read")).await
        ));

        register_authorizer(&server, RouteAccess::ALL.to_vec());
        assert_eq!(call(&mut peer, 6, &invoke(INTERFACE, "write")).await, ok);
        // Owner-local even though the authorizer allows everything.
        assert!(denied(
            &call(&mut peer, 7, &invoke(INTERFACE, "admin")).await
        ));
        assert!(denied(
            &call(&mut peer, 8, &invoke("undeclared/v1", "read")).await
        ));
        // Plugin attribution does not change the class check.
        let as_plugin = Request::InvokeServiceAsPlugin {
            caller_plugin_id: "example.plugin".into(),
            capability: CAPABILITY.into(),
            kind: bmux_ipc::InvokeServiceKind::Query,
            interface_id: INTERFACE.into(),
            operation: "admin".into(),
            payload: Vec::new(),
        };
        assert!(denied(&call(&mut peer, 9, &as_plugin).await));

        // Every pipeline stage is checked before the first one runs.
        let before = effects.load(std::sync::atomic::Ordering::SeqCst);
        let step = |operation: &str| bmux_ipc::ServicePipelineStep {
            capability: CAPABILITY.into(),
            kind: bmux_ipc::InvokeServiceKind::Query,
            interface_id: INTERFACE.into(),
            operation: operation.into(),
            payload: bmux_ipc::ServicePipelinePayload::Encoded { payload: vec![] },
        };
        let pipeline = Request::InvokeServicePipeline {
            pipeline: bmux_ipc::ServicePipelineRequest {
                inputs: BTreeMap::new(),
                steps: vec![step("read"), step("admin")],
            },
        };
        assert!(denied(&call(&mut peer, 10, &pipeline).await));
        assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), before);
    }

    #[test]
    fn plugin_commands_run_through_the_cli_bridge_take_their_declared_class() {
        let server = BmuxServer::new(IpcEndpoint::unix_socket("remote-command-test"));
        server
            .declare_command_access("example.tabs", "next-tab", RouteAccess::Input)
            .unwrap();
        let (binding, _revocation) = StreamAdmissionBinding::new(Uuid::new_v4(), Uuid::new_v4());
        let bridge = |operation: &str, command: &str| {
            let request = bmux_plugin_sdk::PluginCliCommandRequest::new(
                "example.tabs".to_string(),
                command.to_string(),
                Vec::new(),
            );
            Request::InvokeService {
                capability: bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY.into(),
                kind: bmux_ipc::InvokeServiceKind::Command,
                interface_id: bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1.into(),
                operation: operation.into(),
                payload: bmux_plugin_sdk::encode_service_message(&request).unwrap(),
            }
        };
        let run = bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1;
        let authorize = |request: &Request| {
            authorize_admitted_request(&server.state, &binding, request).is_ok()
        };

        register_authorizer(&server, vec![RouteAccess::Observe]);
        assert!(!authorize(&bridge(run, "next-tab")));
        register_authorizer(&server, RouteAccess::ALL.to_vec());
        assert!(authorize(&bridge(run, "next-tab")));
        // A command that declares no class never runs for a remote caller.
        assert!(!authorize(&bridge(run, "kill-everything")));
        // Core CLI paths are never reachable remotely.
        assert!(!authorize(&bridge(
            bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
            "next-tab"
        )));
        // A templated pipeline payload names no command before it runs.
        let templated = Request::InvokeServicePipeline {
            pipeline: bmux_ipc::ServicePipelineRequest {
                inputs: BTreeMap::new(),
                steps: vec![bmux_ipc::ServicePipelineStep {
                    capability: bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY.into(),
                    kind: bmux_ipc::InvokeServiceKind::Command,
                    interface_id: bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1.into(),
                    operation: run.into(),
                    payload: bmux_ipc::ServicePipelinePayload::JsonTemplate {
                        value: serde_json::Value::Null,
                        field_order: None,
                    },
                }],
            },
        };
        assert!(!authorize(&templated));
    }

    #[tokio::test]
    async fn local_streams_are_not_class_checked() {
        let effects = Arc::default();
        let server = server_with_routes(&effects);
        let (client, host) = tokio::io::duplex(1 << 20);
        let permit = ConnectionSlot::try_acquire(&server.state.transport_admission).unwrap();
        let _task = tokio::spawn(handle_connection(
            Arc::clone(&server.state),
            server.shutdown_tx.clone(),
            ErasedIpcStream::new(Box::new(host)),
            None,
            Arc::new(permit),
        ));
        let mut peer = ErasedIpcStream::new(Box::new(client));
        assert!(matches!(
            call(&mut peer, 1, &hello(Uuid::new_v4())).await,
            Response::Ok(ResponsePayload::HelloNegotiated { .. })
        ));
        assert_eq!(
            call(&mut peer, 2, &invoke("undeclared/v1", "read")).await,
            Response::Ok(ResponsePayload::ServiceInvoked { payload: vec![1] })
        );
        assert_eq!(
            call(&mut peer, 3, &invoke(INTERFACE, "admin")).await,
            Response::Ok(ResponsePayload::ServiceInvoked { payload: vec![7] })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ingress_serves_admitted_streams_until_revoked() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = IpcEndpoint::UnixSocket(root.path().join("server.sock"));
        let server = BmuxServer::new(endpoint.clone());
        register_authorizer(&server, vec![RouteAccess::Observe]);
        let listener =
            LocalIpcListener::bind_with_policy(&endpoint, ConfigPaths::local_ipc_policy()).unwrap();
        let running = server.clone();
        let run = tokio::spawn(async move { running.run_with_listener(listener, None).await });

        let ingress = StreamIngressHandle::from_host_registry(&server.state.plugin_states).unwrap();
        let subject = Uuid::new_v4();
        let (binding, revocation) = StreamAdmissionBinding::new(subject, Uuid::new_v4());
        let (client, host) = tokio::io::duplex(1 << 20);
        ingress
            .accept(ErasedIpcStream::new(Box::new(host)), Arc::clone(&binding))
            .unwrap();
        let mut peer = ErasedIpcStream::new(Box::new(client));
        assert!(matches!(
            call(&mut peer, 1, &hello(subject)).await,
            Response::Ok(ResponsePayload::HelloNegotiated { .. })
        ));
        assert_eq!(
            call(&mut peer, 2, &Request::Ping).await,
            Response::Ok(ResponsePayload::Pong)
        );
        assert_eq!(
            call(&mut peer, 3, &Request::EnableEventPush).await,
            Response::Ok(ResponsePayload::EventPushEnabled)
        );
        // Revocation closes the idle connection promptly, without polling.
        revocation.revoke();
        let closed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if peer.recv_envelope().await.is_err() {
                    break;
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "revoked stream still open after 1 s");
        // A revoked binding is refused at the door.
        let (client, host) = tokio::io::duplex(1024);
        drop(client);
        assert_eq!(
            ingress.accept(ErasedIpcStream::new(Box::new(host)), binding),
            Err(StreamIngressError::Revoked)
        );

        server.request_shutdown();
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // After shutdown the ingress refuses streams.
        let (fresh, _revocation) = StreamAdmissionBinding::new(subject, Uuid::new_v4());
        let (_client, host) = tokio::io::duplex(1024);
        assert_eq!(
            ingress.accept(ErasedIpcStream::new(Box::new(host)), fresh),
            Err(StreamIngressError::Unavailable)
        );
    }

    #[test]
    fn full_server_refuses_admitted_streams() {
        let server = BmuxServer::new(IpcEndpoint::unix_socket("full-server-test"));
        let ingress = StreamIngressHandle::from_host_registry(&server.state.plugin_states).unwrap();
        let _held = Arc::clone(&server.state.transport_admission)
            .try_acquire_many_owned(u32::try_from(MAX_CONNECTIONS).unwrap())
            .unwrap();
        let (binding, _revocation) = StreamAdmissionBinding::new(Uuid::new_v4(), Uuid::new_v4());
        let (_client, host) = tokio::io::duplex(1024);
        assert_eq!(
            ingress.accept(ErasedIpcStream::new(Box::new(host)), binding),
            Err(StreamIngressError::AtCapacity)
        );
    }

    #[test]
    fn provenance_snapshot_reports_the_binding_until_revoked() {
        let registry = bmux_plugin::stream_admission::StreamProvenanceRegistry::default();
        let subject = Uuid::new_v4();
        let connection = Uuid::new_v4();
        let (binding, revocation) = StreamAdmissionBinding::new(subject, connection);
        let id = Uuid::new_v4();
        let _guard = registry.record(id, Some(binding)).unwrap();
        assert_eq!(
            current_stream_provenance(&registry, id),
            Response::Ok(ResponsePayload::CurrentStreamProvenance {
                provenance: bmux_ipc::StreamProvenance::Admitted {
                    subject_id: subject,
                    connection_id: connection,
                },
            })
        );
        revocation.revoke();
        assert_eq!(
            current_stream_provenance(&registry, id),
            Response::Ok(ResponsePayload::CurrentStreamProvenance {
                provenance: bmux_ipc::StreamProvenance::Invalid,
            })
        );
    }
}

fn current_stream_provenance(
    registry: &bmux_plugin::stream_admission::StreamProvenanceRegistry,
    host_stream_id: Uuid,
) -> Response {
    use bmux_plugin::stream_admission::StreamProvenance as HostProvenance;
    let provenance = match registry.lookup(host_stream_id) {
        HostProvenance::Local => bmux_ipc::StreamProvenance::Local,
        HostProvenance::Admitted(binding) => bmux_ipc::StreamProvenance::Admitted {
            subject_id: binding.subject_id(),
            connection_id: binding.connection_id(),
        },
        HostProvenance::Invalid => bmux_ipc::StreamProvenance::Invalid,
    };
    Response::Ok(ResponsePayload::CurrentStreamProvenance { provenance })
}

/// The principal a connection acts as: the claimed `Hello` principal on a
/// local stream; on an admitted stream it must equal the binding's subject.
fn stream_principal(admitted: Option<&StreamAdmissionBinding>, claimed: Uuid) -> Result<Uuid> {
    if let Some(binding) = admitted {
        anyhow::ensure!(!binding.is_revoked(), "admitted stream revoked");
        let subject = binding.subject_id();
        anyhow::ensure!(claimed == subject, "admitted stream principal mismatch");
        Ok(subject)
    } else {
        Ok(claimed)
    }
}

/// Resolves when `binding` is revoked; never for local streams.
async fn wait_revoked(binding: Option<Arc<StreamAdmissionBinding>>) {
    match binding {
        Some(binding) => binding.revoked().await,
        None => std::future::pending().await,
    }
}

// Keep the serial handshake and connection cleanup visible in one state machine.
#[allow(clippy::too_many_lines)]
async fn handle_connection(
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
    mut stream: ErasedIpcStream,
    admitted: Option<Arc<StreamAdmissionBinding>>,
    admission: Arc<ConnectionSlot>,
) -> Result<()> {
    let client_id = ClientId::new();
    let client_principal_id: Uuid;
    let mut selected_session: Option<SessionId> = None;
    let mut attached_stream_session: Option<SessionId> = None;
    let supports_service_contract_v2: bool;
    let negotiated_frame_codec: Option<
        std::sync::Arc<dyn bmux_ipc::compression::CompressionCodec>,
    >;

    // ── Handshake (serial, before split) ─────────────────────────────────

    // An admitted stream's identity comes only from its in-process binding,
    // pinned for the whole connection; nothing on the wire can install one.
    let mut revoked = std::pin::pin!(wait_revoked(admitted.clone()));
    let first_envelope = tokio::select! {
        biased;
        () = &mut revoked => return Ok(()),
        envelope = tokio::time::timeout(state.handshake_timeout, stream.recv_envelope()) => {
            envelope.context("handshake timed out")??
        }
    };
    let handshake = parse_request(&first_envelope)?;
    // Even the handshake needs the subject's observe grant.
    if let Some(binding) = &admitted
        && let Err(message) = authorize_admitted_request(&state, binding, &handshake)
    {
        send_error(
            &mut stream,
            first_envelope.request_id,
            ErrorCode::PermissionDenied,
            message,
        )
        .await?;
        return Ok(());
    }
    if let Request::Hello {
        contract,
        client_name,
        principal_id,
    } = handshake
    {
        let mut capabilities = default_supported_capabilities();
        // The host's local terminal is never offered to an admitted stream.
        if admitted.is_none() && state.local_terminal_bound.get().is_some() {
            capabilities.push(bmux_ipc::CAPABILITY_LOCAL_TERMINAL_V1.to_owned());
        }
        let server_contract = ProtocolContract::current(capabilities);
        match negotiate_protocol(&contract, &server_contract, CORE_PROTOCOL_CAPABILITIES) {
            Ok(negotiated) => {
                supports_service_contract_v2 = negotiated
                    .capabilities
                    .iter()
                    .any(|capability| capability == bmux_ipc::CAPABILITY_SERVICE_CONTRACT_V2);
                client_principal_id = stream_principal(admitted.as_deref(), principal_id)?;
                debug!(
                    "accepted client handshake: {client_name} revision={} caps={}",
                    negotiated.revision,
                    negotiated.capabilities.join(",")
                );
                // Resolve frame compression codec from negotiated capabilities.
                negotiated_frame_codec =
                    resolve_frame_codec_from_capabilities(&negotiated.capabilities);
                send_ok(
                    &mut stream,
                    first_envelope.request_id,
                    ResponsePayload::HelloNegotiated { negotiated },
                )
                .await?;
            }
            Err(reason) => {
                send_ok(
                    &mut stream,
                    first_envelope.request_id,
                    ResponsePayload::HelloIncompatible { reason },
                )
                .await?;
                return Ok(());
            }
        }
    } else {
        send_error(
            &mut stream,
            first_envelope.request_id,
            ErrorCode::InvalidRequest,
            "first request must be hello".to_string(),
        )
        .await?;
        return Ok(());
    }

    let _provenance = state
        .stream_provenance
        .record(client_id.0, admitted.clone())?;
    follow_handle(&state.plugin_states)
        .0
        .connect_client(client_id);
    {
        let mut principals = state
            .client_principals
            .lock()
            .map_err(|_| anyhow::anyhow!("client principal map lock poisoned"))?;
        principals.insert(client_id, client_principal_id);
    }

    // Register only after the handshake has authenticated this connection.
    let retained_registration = state.retained_wire_events.register(client_id.0)?;

    // ── Split stream for concurrent read/write ───────────────────────────

    let (mut reader, writer) = stream.into_split();

    // Enable frame compression if negotiated.
    if negotiated_frame_codec.is_some() {
        reader.enable_frame_compression();
        // The writer is used via write_raw_frame with pre-encoded frames,
        // so we don't set a codec on it — compression happens at the
        // encode_frame_compressed call site in send_response_via_channel.
    }

    // Channel-based writer: all outgoing frames (responses + pushed events)
    // are sent through this channel to a single writer task. This eliminates
    // mutex contention between the request loop and the event push task.
    // Encoded frames have an IPC size ceiling. Bound queued frames as well:
    // a stalled writer must not retain an unbounded stream of encoded output.
    let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(16);
    let writer_admission = Arc::clone(&admission);
    let (writer_done_tx, mut writer_done_rx) = oneshot::channel();
    let mut connection_shutdown = shutdown_tx.subscribe();
    let writer_task = tokio::spawn(async move {
        // A cancelled handler must not release admission while its writer lives.
        write_queued_frames(writer, frame_rx, Duration::from_secs(5)).await;
        drop(writer_admission);
        let _ = writer_done_tx.send(());
    });

    let _writer_abort = AbortOnDrop(writer_task.abort_handle());
    let mut push_abort = None;
    let mut event_push_task: Option<tokio::task::JoinHandle<()>> = None;

    // ── Request loop ─────────────────────────────────────────────────────

    let request_result: Result<()> = async {
    loop {
        if *connection_shutdown.borrow() {
            break;
        }
        let received = tokio::select! {
            biased;
            () = &mut revoked => break,
            result = reader.recv_envelope_with_timing() => result,
            _ = &mut writer_done_rx => break,
            changed = connection_shutdown.changed() => {
                if changed.is_err() || *connection_shutdown.borrow() {
                    break;
                }
                continue;
            }
        };
        let (envelope, request_read_timing) = match received {
            Ok(result) => result,
            Err(IpcTransportError::Io(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(error).context("failed receiving request envelope"),
        };

        let request_decode_started = Instant::now();
        let request = match parse_request(&envelope) {
            Ok(request) => request,
            Err(error) => {
                send_error_via_channel(
                    &frame_tx,
                    envelope.request_id,
                    ErrorCode::InvalidRequest,
                    format!("failed parsing request: {error:#}"),
                    &mut connection_shutdown,
                    Duration::from_secs(5),
                    negotiated_frame_codec.as_deref(),
                ).await?;
                continue;
            }
        };
        let request_decode_us = request_decode_started.elapsed().as_micros();
        // Admitted streams get the full protocol, each request checked against
        // its access class before any effect. `InvokeServiceAsPlugin` is
        // classed by its route; its plugin attribution is dropped (see
        // `accepted_caller_plugin_attribution`).
        if let Some(binding) = &admitted
            && let Err(message) = authorize_admitted_request(&state, binding, &request)
        {
            send_error_via_channel(
                &frame_tx,
                envelope.request_id,
                ErrorCode::PermissionDenied,
                message,
                &mut connection_shutdown,
                Duration::from_secs(5),
                negotiated_frame_codec.as_deref(),
            ).await?;
            continue;
        }

        // Track whether this request enables event push delivery.
        let is_enable_push = matches!(request, Request::EnableEventPush);

        let request_kind = request_kind_name(&request);
        let service_metadata = ipc_service_request_metadata(&request);
        let exclusive = request_requires_exclusive(&request);
        let request_record_encode_us = 0_u128;
        let started_at = Instant::now();
        debug!(
            client_id = %client_id.0,
            request_id = envelope.request_id,
            request = request_kind,
            exclusive,
            "server.request.start"
        );
        let request_record_us = 0_u128;

        let handle_started = Instant::now();
        let mut response = tokio::select! {
            biased;
            () = &mut revoked => break,
            response = handle_request(
                &state,
                &shutdown_tx,
                client_id,
                client_principal_id,
                &mut selected_session,
                request,
            ) => response?,
        };
        if !supports_service_contract_v2
            && let Response::Err(error) = &mut response
            && matches!(error.code, ErrorCode::UnsupportedRepresentation | ErrorCode::ServiceIncompatible)
        {
            error.code = ErrorCode::Internal;
        }
        let handle_us = handle_started.elapsed().as_micros();
        let elapsed_ms = started_at.elapsed().as_millis();
        let (response_record_encode_us, response_record_us) = match &response {
            Response::Ok(payload) => {
                let response_record_encode_us = 0_u128;
                debug!(
                    client_id = %client_id.0,
                    request_id = envelope.request_id,
                    request = request_kind,
                    response = response_payload_kind_name(payload),
                    elapsed_ms,
                    "server.request.done"
                );
                (response_record_encode_us, 0_u128)
            }
            Response::Err(error) => {
                warn!(
                    client_id = %client_id.0,
                    request_id = envelope.request_id,
                    request = request_kind,
                    error_code = ?error.code,
                    error_message = %error.message,
                    elapsed_ms,
                    "server.request.error"
                );
                (0_u128, 0_u128)
            }
        };
        let response_send_started = Instant::now();
        // ServerStop has already requested shutdown. Its acknowledgement must
        // still be queued (with the same finite capacity deadline), rather than
        // cancelled by the shutdown it initiated.
        let (_ack_shutdown_tx, mut ack_shutdown_rx) = watch::channel(false);
        let response_shutdown = if matches!(response, Response::Ok(ResponsePayload::ServerStopping)) {
            &mut ack_shutdown_rx
        } else {
            &mut connection_shutdown
        };
        let response_send_timing = match send_response_via_channel(
            &frame_tx,
            envelope.request_id,
            &response,
            response_shutdown,
            Duration::from_secs(5),
            negotiated_frame_codec.as_deref(),
        ).await {
            Ok(timing) => timing,
            Err(err) if is_frame_too_large_error(&err) => {
                warn!(
                    client_id = %client_id.0,
                    request_id = envelope.request_id,
                    "response exceeded frame size limit, sending error to client: {err:#}"
                );
                send_error_via_channel(
                    &frame_tx,
                    envelope.request_id,
                    ErrorCode::Internal,
                    "response too large".to_string(),
                    &mut connection_shutdown,
                    Duration::from_secs(5),
                    negotiated_frame_codec.as_deref(),
                ).await?;
                IpcResponseSendTiming::default()
            }
            Err(err) => {
                warn!(client_id = %client_id.0, request_id = envelope.request_id,
                    error = %err, "response delivery failed; closing connection");
                return Err(err);
            }
        };
        let response_send_us = response_send_started.elapsed().as_micros();
        emit_phase_timing(
            PhaseChannel::Ipc,
            &server_ipc_request_phase_payload(
                request_kind,
                service_metadata,
                envelope.request_id,
                &response,
                request_read_timing.socket_read_us,
                request_read_timing.frame_decode_us,
                request_decode_us,
                request_record_encode_us,
                request_record_us,
                handle_us,
                response_record_encode_us,
                response_record_us,
                response_send_timing.response_encode,
                response_send_timing.frame_encode,
                response_send_timing.writer_queue,
                response_send_us,
                started_at.elapsed().as_micros(),
            ),
        );

        // After responding to EnableEventPush, spawn the event push task.
        // It receives events from the broadcast channel and forwards them
        // as serialized frames through the writer channel.
        //
        if is_enable_push && event_push_task.is_none() {
            state.event_push_clients.lock().await.insert(client_id);
            debug!(%client_id, "event_push.client_registered");
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                replay_retained_plugin_state_to_client(
                    &state,
                    &frame_tx,
                    negotiated_frame_codec.as_deref(),
                ),
            ).await.context("writer stalled during retained-state replay")?;
            let mut event_rx = state.event_broadcast.subscribe();
            let retained_ready = Arc::clone(&retained_registration.ready);
            let push_frame_tx = frame_tx.clone();
            let push_frame_codec = negotiated_frame_codec.clone();
            let push_state = Arc::clone(&state);
            let push_client_id = client_id;
            let push_admitted = admitted.is_some();
            event_push_task = Some(tokio::spawn(async move {
                let push_perf_settings_store = push_state.performance_settings.clone();
                let mut push_perf_settings = push_perf_settings_store.current();
                let mut push_perf_rate_limiter =
                    PerformanceEventRateLimiter::new(push_perf_settings);
                let mut push_perf_window = Duration::from_millis(push_perf_settings.window_ms);
                let mut push_window_started_at = Instant::now();
                let mut push_window_sent_events = 0_u64;
                let mut push_window_sent_bytes = 0_u64;
                let mut push_window_lagged_events = 0_u64;
                let mut push_window_lagged_receives = 0_u64;
                loop {
                    let latest_push_perf_settings = push_perf_settings_store.current();
                    if latest_push_perf_settings != push_perf_settings {
                        push_perf_settings = latest_push_perf_settings;
                        push_perf_rate_limiter =
                            PerformanceEventRateLimiter::new(push_perf_settings);
                        push_perf_window = Duration::from_millis(push_perf_settings.window_ms);
                        push_window_started_at = Instant::now();
                        push_window_sent_events = 0;
                        push_window_sent_bytes = 0;
                        push_window_lagged_events = 0;
                        push_window_lagged_receives = 0;
                    }

                    let received = tokio::select! {
                        event = event_rx.recv() => event,
                        () = retained_ready.notified() => {
                            // Reserve before selecting the latest value: a slow writer
                            // must not build a second queue of obsolete retained values.
                            let Ok(permit) = push_frame_tx.reserve().await else {
                                return;
                            };
                            if let Some(event) = push_state.retained_wire_events.take_pending(push_client_id.0) {
                                if let Some(frame) = encode_event_frame(&event, push_frame_codec.as_deref()) {
                                    permit.send(frame);
                                }
                                retained_ready.notify_one();
                            } else if push_state.retained_wire_events.take_wake(push_client_id.0) {
                                // Read at send time: with room to queue a message, pull what the
                                // sources have for this client now.
                                let mut permit = Some(permit);
                                for (kind, source) in push_state.retained_wire_events.pull_sources() {
                                    let Some(payload) = source(push_client_id.0) else {
                                        continue;
                                    };
                                    let event = Event::PluginBusEvent { kind, payload };
                                    let Some(frame) = encode_event_frame(&event, push_frame_codec.as_deref()) else {
                                        continue;
                                    };
                                    let permit = match permit.take() {
                                        Some(permit) => permit,
                                        None => match push_frame_tx.reserve().await {
                                            Ok(permit) => permit,
                                            Err(_) => return,
                                        },
                                    };
                                    permit.send(frame);
                                }
                            }
                            continue;
                        }
                    };
                    match received {
                        // The host's device-seal broker exchange stays on local streams.
                        Ok(Event::DeviceSealBrokerRequest { .. }) if push_admitted => {}
                        Ok(event) => {
                            let Some(frame) =
                                encode_event_frame(&event, push_frame_codec.as_deref())
                            else {
                                continue;
                            };
                            let frame_len = frame.len();
                            if push_frame_tx.send(frame).await.is_err() {
                                return; // writer dropped (client disconnected)
                            }
                            if push_perf_settings.enabled() {
                                push_window_sent_events = push_window_sent_events.saturating_add(1);
                                push_window_sent_bytes = push_window_sent_bytes
                                    .saturating_add(u64::try_from(frame_len).unwrap_or(u64::MAX));

                                let elapsed = push_window_started_at.elapsed();
                                if push_perf_settings
                                    .level_at_least(PerformanceRecordingLevel::Basic)
                                    && elapsed >= push_perf_window
                                    && (push_window_sent_events > 0
                                        || push_window_lagged_events > 0
                                        || push_window_lagged_receives > 0)
                                {
                                    let elapsed_ms =
                                        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                                    let mut payload = serde_json::json!({
                                        "window_elapsed_ms": elapsed_ms,
                                        "events_pushed": push_window_sent_events,
                                        "bytes_pushed": push_window_sent_bytes,
                                        "lagged_events": push_window_lagged_events,
                                    });
                                    if push_perf_settings
                                        .level_at_least(PerformanceRecordingLevel::Detailed)
                                        && let Some(object) = payload.as_object_mut()
                                    {
                                        object.insert(
                                            "lagged_receives".to_string(),
                                            serde_json::Value::from(push_window_lagged_receives),
                                        );
                                    }
                                    if push_perf_settings
                                        .level_at_least(PerformanceRecordingLevel::Trace)
                                        && let Some(object) = payload.as_object_mut()
                                    {
                                        object.insert(
                                            "frame_compression_enabled".to_string(),
                                            serde_json::Value::from(push_frame_codec.is_some()),
                                        );
                                        object.insert(
                                            "event_push_channel_capacity".to_string(),
                                            serde_json::Value::from(EVENT_PUSH_CHANNEL_CAPACITY),
                                        );
                                    }

                                    let _ = push_perf_rate_limiter.encode_payload(payload);
                                    push_window_started_at = Instant::now();
                                    push_window_sent_events = 0;
                                    push_window_sent_bytes = 0;
                                    push_window_lagged_events = 0;
                                    push_window_lagged_receives = 0;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("event push task lagged by {n} events for client");
                            if push_perf_settings.enabled() {
                                push_window_lagged_events =
                                    push_window_lagged_events.saturating_add(n);
                                push_window_lagged_receives =
                                    push_window_lagged_receives.saturating_add(1);
                            }

                            let recovery_events = lag_recovery_attach_view_events_for_client(
                                &push_state,
                                push_client_id,
                            );
                            if !recovery_events.is_empty() {
                                warn!(
                                    "event push lag recovery scheduling {} attach view refresh events",
                                    recovery_events.len()
                                );
                            }
                            for recovery_event in recovery_events {
                                let Some(frame) = encode_event_frame(
                                    &recovery_event,
                                    push_frame_codec.as_deref(),
                                ) else {
                                    continue;
                                };
                                let frame_len = frame.len();
                                if push_frame_tx.send(frame).await.is_err() {
                                    return;
                                }
                                if push_perf_settings.enabled() {
                                    push_window_sent_events =
                                        push_window_sent_events.saturating_add(1);
                                    push_window_sent_bytes = push_window_sent_bytes.saturating_add(
                                        u64::try_from(frame_len).unwrap_or(u64::MAX),
                                    );
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return; // server shutting down
                        }
                    }
                }
            }));
            push_abort = event_push_task.as_ref().map(|task| AbortOnDrop(task.abort_handle()));
        }
    }
    Ok(())
    }.await;

    drop(retained_registration);
    state.event_push_clients.lock().await.remove(&client_id);

    // Abort the event push task if running.
    drop(push_abort);
    if let Some(task) = event_push_task {
        task.abort();
        let _ = task.await;
    }
    // Drop the frame sender so the writer task exits. A revoked stream gets
    // nothing more: its writer is aborted rather than drained.
    drop(frame_tx);
    if admitted
        .as_ref()
        .is_some_and(|binding| binding.is_revoked())
    {
        writer_task.abort();
    }
    finish_writer(writer_task, std::time::Duration::from_secs(5)).await;

    detach_client_state_on_disconnect(
        &state,
        client_id,
        &mut selected_session,
        &mut attached_stream_session,
    );
    disconnect_follow_state(&state, client_id);
    {
        let mut principals = state
            .client_principals
            .lock()
            .map_err(|_| anyhow::anyhow!("client principal map lock poisoned"))?;
        principals.remove(&client_id);
    }
    mark_snapshot_dirty(&state);
    maybe_flush_snapshot(&state, false)?;
    unsubscribe_events(&state, client_id)?;

    request_result
}

async fn write_queued_frames(
    mut writer: bmux_ipc::transport::ErasedIpcStreamWriter,
    mut frames: mpsc::Receiver<Vec<u8>>,
    deadline: Duration,
) {
    while let Some(frame) = frames.recv().await {
        // A timeout may leave a partial frame on the wire. Close the transport
        // and discard the queue; never resume or retry this frame.
        match tokio::time::timeout(deadline, writer.write_raw_frame(&frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(%error, "transport writer failed; closing connection");
                break;
            }
            Err(_) => {
                warn!(
                    deadline_ms = deadline.as_millis(),
                    "transport writer deadline exceeded; closing connection"
                );
                break;
            }
        }
    }
}

// JoinHandle alone detaches on drop. Keep cancellation ownership even while a
// task is being joined, so dropping the parent future cannot strand its IO.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn finish_writer(mut task: tokio::task::JoinHandle<()>, timeout: std::time::Duration) {
    let _abort = AbortOnDrop(task.abort_handle());
    if tokio::time::timeout(timeout, &mut task).await.is_err() {
        task.abort();
        let _ = task.await;
    }
}

async fn select_device_seal_broker_client(state: &Arc<ServerState>) -> Option<ClientId> {
    // Only a local client may broker the host's device seal.
    let event_push_clients = state
        .event_push_clients
        .lock()
        .await
        .iter()
        .copied()
        .filter(|client| {
            matches!(
                state.stream_provenance.lookup(client.0),
                bmux_plugin::stream_admission::StreamProvenance::Local
            )
        })
        .collect::<BTreeSet<_>>();
    let snapshot = follow_handle(&state.plugin_states).0.snapshot();
    let selected =
        snapshot
            .attached_stream_sessions
            .into_iter()
            .find_map(|(client_id, session_id)| {
                (session_id.is_some() && event_push_clients.contains(&client_id))
                    .then_some(client_id)
            });
    if selected.is_none() {
        warn!(
            active_event_push_clients = event_push_clients.len(),
            "device_seal_broker.no_active_attached_client"
        );
    }
    selected
}

fn emit_event(state: &Arc<ServerState>, event: Event) -> Result<()> {
    // Broadcast to streaming clients (ignore errors — no receivers is fine).
    let _ = state.event_broadcast.send(event.clone());
    state
        .event_hub
        .lock()
        .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?
        .emit(event);
    Ok(())
}

/// Send one forwarded JSON state value to streaming clients.
fn forward_json_state(state: &Arc<ServerState>, kind: &str, value: &serde_json::Value) {
    let Ok(payload) = serde_json::to_vec(value) else {
        return;
    };
    let event = Event::PluginBusEvent {
        kind: kind.to_string(),
        payload,
    };
    if let Err(error) = emit_event(state, event) {
        tracing::warn!(%kind, %error, "failed emitting forwarded plugin bus state event");
    }
}

async fn replay_retained_plugin_state_to_client(
    state: &Arc<ServerState>,
    frame_tx: &mpsc::Sender<Vec<u8>>,
    frame_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
) {
    let replayers = state
        .plugin_state_replayers
        .lock()
        .map_or_else(|_| Vec::new(), |guard| guard.clone());
    for replayer in replayers {
        let Some(event) = replayer() else {
            continue;
        };
        let Some(frame) = encode_event_frame(&event, frame_codec) else {
            continue;
        };
        if frame_tx.send(frame).await.is_err() {
            return;
        }
    }
}

/// Concrete `WireEventSink` impl that pipes plugin-published wire
/// events into the server's `emit_event` pipeline (recording fan-out +
/// streaming broadcast + event-hub fan-out). The sink holds an
/// `Arc<ServerState>` so plugins can publish wire events through the
/// registered handle without knowing about `ServerState` at all.
struct ServerWireEventSink {
    state: Arc<ServerState>,
}

impl WireEventSink for ServerWireEventSink {
    fn is_connected(&self, recipient: Uuid) -> Option<bool> {
        let recipients = self.state.retained_wire_events.recipients.lock().ok()?;
        let connected = recipients.contains_key(&recipient);
        drop(recipients);
        Some(connected)
    }
    fn publish(&self, event: Event) -> std::result::Result<(), WireEventSinkError> {
        emit_event(&self.state, event)
            .map_err(|error| WireEventSinkError::PublishFailed(error.to_string()))
    }

    fn publish_retained_to(
        &self,
        recipient: Uuid,
        kind: String,
        payload: Vec<u8>,
    ) -> std::result::Result<(), WireEventSinkError> {
        self.state
            .retained_wire_events
            .publish(recipient, kind, payload)
    }

    fn clear_retained_to(
        &self,
        recipient: Uuid,
        kind: &str,
    ) -> std::result::Result<(), WireEventSinkError> {
        self.state.retained_wire_events.clear(recipient, kind)
    }

    fn register_pull_source(
        &self,
        kind: String,
        source: bmux_plugin_sdk::WirePullSource,
    ) -> std::result::Result<(), WireEventSinkError> {
        if kind.len() > RETAINED_KIND_BYTES {
            return Err(retained_error("pull source kind limit exceeded"));
        }
        self.state
            .retained_wire_events
            .pull_sources
            .lock()
            .map_err(|_| retained_error("poisoned pull source lock"))?
            .push((kind, source));
        Ok(())
    }

    fn wake(&self, recipient: Uuid) -> std::result::Result<(), WireEventSinkError> {
        self.state.retained_wire_events.wake(recipient)
    }
}

/// Register the server's wire-event sink into the plugin state
/// registry. Plugins look up `WireEventSinkHandle` from the registry
/// and call `publish(event)` to emit cross-process wire events
/// without depending on the server at all.
fn register_wire_event_sink(state: &Arc<ServerState>) {
    let sink = Arc::new(std::sync::RwLock::new(WireEventSinkHandle::new(
        ServerWireEventSink {
            state: Arc::clone(state),
        },
    )));
    state.plugin_states.register::<WireEventSinkHandle>(&sink);
}

fn encode_event_frame(
    event: &Event,
    frame_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
) -> Option<Vec<u8>> {
    let payload = encode(event).ok()?;
    let envelope = Envelope::new(0, EnvelopeKind::Event, payload);
    if frame_codec.is_some() {
        bmux_ipc::frame::encode_frame_compressed(&envelope, frame_codec).ok()
    } else {
        bmux_ipc::frame::encode_frame(&envelope).ok()
    }
}

fn lag_recovery_attach_view_events_for_client(
    state: &Arc<ServerState>,
    client_id: ClientId,
) -> Vec<Event> {
    let _ = (state, client_id);

    // Force attach clients to poll the plugin-owned catalog after a
    // dropped event window. The revision is intentionally neutral here:
    // clients treat `full_resync` as authoritative and fetch the typed
    // snapshot from the control-catalog plugin.
    vec![control_catalog_full_resync_event()]
}

fn control_catalog_full_resync_event() -> Event {
    Event::PluginBusEvent {
        kind: "bmux.control_catalog/control-catalog-events".to_string(),
        payload: serde_json::json!({
            "changed": {
                "revision": 0,
                "scopes": ["sessions", "contexts", "bindings"],
                "full_resync": true,
            },
        })
        .to_string()
        .into_bytes(),
    }
}

fn unsubscribe_events(state: &Arc<ServerState>, client_id: ClientId) -> Result<()> {
    state
        .event_hub
        .lock()
        .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?
        .unsubscribe(client_id);
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn sync_selected_target_from_follow_state(
    state: &Arc<ServerState>,
    client_id: ClientId,
    selected_session: &mut Option<SessionId>,
) -> Result<()> {
    let follow_selected = follow_handle(&state.plugin_states)
        .0
        .selected_target(client_id);

    if let Some((follow_selected_context, follow_selected_session)) = follow_selected {
        if let Some(context_id) = follow_selected_context {
            let _ = context_handle(&state.plugin_states)
                .0
                .select_for_client(client_id, &ContextSelector::ById(context_id));
        }

        *selected_session = follow_selected_session
            .or_else(|| current_context_session_for_client(state, client_id));
    }
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn reconcile_selected_session_membership(
    state: &Arc<ServerState>,
    client_id: ClientId,
    previous: Option<SessionId>,
    next: Option<SessionId>,
) -> Result<()> {
    if previous == next {
        return Ok(());
    }

    let manager = session_handle(&state.plugin_states);

    if let Some(previous_session) = previous {
        manager.0.remove_client(previous_session, &client_id);
    }

    if let Some(next_session) = next {
        manager.0.add_client(next_session, client_id);
    }

    Ok(())
}

fn disconnect_follow_state(state: &ServerState, client_id: ClientId) {
    let _ = follow_handle(&state.plugin_states)
        .0
        .disconnect_client(client_id);
}

fn mark_snapshot_dirty(state: &Arc<ServerState>) {
    snapshot_dirty_flag(&state.plugin_states).mark_dirty();
}

/// Opportunistic snapshot flush. The snapshot plugin's debounce
/// thread does the real flushing; this function retains the
/// original signature so call-sites don't churn, but for `force=false`
/// it's a no-op (the dirty flag is already set by the caller's
/// `mark_snapshot_dirty`, and the plugin picks it up). For `force=true`
/// we drive the orchestrator's synchronous `save_now` path via a
/// blocking call on the current tokio runtime.
#[allow(clippy::unnecessary_wraps)] // Signature preserved for call-site ergonomics
fn maybe_flush_snapshot(state: &Arc<ServerState>, force: bool) -> Result<()> {
    let _ = state;
    if !force {
        return Ok(());
    }
    let handle = snapshot_orchestrator_handle(&state.plugin_states);
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        if let Err(error) = tokio::task::block_in_place(|| {
            rt.block_on(async move { handle.as_dyn().save_now_boxed().await })
        }) {
            warn!("forced snapshot flush failed: {error}");
        }
    } else {
        // Headless / test path without a tokio runtime: just flip
        // the dirty flag; nothing else can drive the async path.
        snapshot_dirty_flag(&state.plugin_states).mark_dirty();
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn handle_request(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    client_id: ClientId,
    client_principal_id: Uuid,
    selected_session: &mut Option<SessionId>,
    request: Request,
) -> Result<Response> {
    let _operation_guard = if request_requires_exclusive(&request) {
        Some(state.operation_lock.lock().await)
    } else {
        None
    };

    let previous_selected_session = *selected_session;
    sync_selected_target_from_follow_state(state, client_id, selected_session)?;
    reconcile_selected_session_membership(
        state,
        client_id,
        previous_selected_session,
        *selected_session,
    )?;

    let response = match request {
        Request::CurrentStreamProvenance | Request::Hello { .. } => Response::Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: "hello request is only valid during handshake".to_string(),
        }),
        Request::Ping => Response::Ok(ResponsePayload::Pong),
        Request::WhoAmIPrincipal => Response::Ok(ResponsePayload::PrincipalIdentity {
            principal_id: client_principal_id,
            server_control_principal_id: state.server_control_principal_id,
            force_local_permitted: client_principal_id == state.server_control_principal_id,
        }),
        Request::ServerStatus => Response::Ok(ResponsePayload::ServerStatus {
            running: true,
            principal_id: client_principal_id,
            server_control_principal_id: state.server_control_principal_id,
        }),
        Request::ServerStop => {
            let _ = shutdown_tx.send(true);
            flush_phase_timing();
            Response::Ok(ResponsePayload::ServerStopping)
        }
        Request::InvokeServiceAsPlugin {
            caller_plugin_id,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        } => {
            let caller_plugin_id =
                accepted_caller_plugin_attribution(state, client_id, &caller_plugin_id);
            let route = ServiceRoute {
                capability,
                kind,
                interface_id,
                operation,
            };
            service_output_response(
                invoke_service_output_as(
                    state,
                    shutdown_tx,
                    client_id,
                    client_principal_id,
                    route,
                    payload,
                    caller_plugin_id,
                )
                .await,
            )
        }
        Request::InvokeService {
            capability,
            kind,
            interface_id,
            operation,
            payload,
        } => {
            // Diagnostic span per typed-service call. `Command` kind
            // lands at `info` because mutating dispatches are both
            // rarer and more interesting to audit; `Query` kind lands
            // at `trace` so catalog / listing polls don't flood the
            // log at default levels. Enable queries with
            // `RUST_LOG=bmux_server::invoke_service=trace`.
            let span = match kind {
                bmux_ipc::InvokeServiceKind::Command => tracing::info_span!(
                    target: "bmux_server::invoke_service",
                    "invoke_service",
                    capability = %capability,
                    interface = %interface_id,
                    operation = %operation,
                    kind = "command",
                    client_id = ?client_id,
                ),
                bmux_ipc::InvokeServiceKind::Query => tracing::trace_span!(
                    target: "bmux_server::invoke_service",
                    "invoke_service",
                    capability = %capability,
                    interface = %interface_id,
                    operation = %operation,
                    kind = "query",
                    client_id = ?client_id,
                ),
            };
            // The dispatch below awaits (and may re-enter `handle_request`), so
            // the span must follow the future: an entered guard held across an
            // await corrupts the subscriber's span stack when tasks interleave.
            async {
                let started_at = std::time::Instant::now();
                // Explicit observable entry log for Command-kind typed
                // dispatches only. Query kind fires too frequently
                // (catalog polls, current-context reads) to log per-call
                // at `info`; they still participate in the surrounding
                // `trace_span!` for context when someone enables trace.
                //
                // Counting `invoke command` lines per keystroke answers
                // "did a single user action fan out into the expected
                // number of mutating calls?" without dragging in the
                // query noise.
                if matches!(kind, bmux_ipc::InvokeServiceKind::Command) {
                    tracing::info!(
                        target: "bmux_server::invoke_service",
                        capability = %capability,
                        interface = %interface_id,
                        operation = %operation,
                        client_id = ?client_id,
                        "invoke command",
                    );
                }
                // Typed service calls don't acquire the global operation
                // lock here: many handlers re-enter `handle_request`
                // through `execute_kernel_request`, which takes the lock
                // at the inner `Request::*` level. A dedicated
                // `OperationLockHandle` will gate pure typed mutations
                // once the legacy IPC surface deletes.
                let route = ServiceRoute {
                    capability: capability.clone(),
                    kind,
                    interface_id: interface_id.clone(),
                    operation: operation.clone(),
                };
                check_stream_origin(state, client_id.0)?;
                let invoke_context = ServiceInvokeContext {
                    host_dispatch_context: None,
                    host_internal: false,
                    state: Arc::clone(state),
                    shutdown_tx: shutdown_tx.clone(),
                    client_id,
                    client_principal_id,
                    caller_plugin_id: None,
                };
                let registry_started = std::time::Instant::now();
                let dispatch = {
                    let registry = state
                        .service_registry
                        .lock()
                        .map_err(|_| anyhow::anyhow!("service registry lock poisoned"))?;
                    registry.dispatch(&route, invoke_context.clone(), payload.clone())
                };
                let registry_us = registry_started.elapsed().as_micros();
                let resolver_started = std::time::Instant::now();
                let invocation = if let Some(invocation) = dispatch {
                    Some(invocation)
                } else {
                    let resolver = state
                        .service_resolver
                        .lock()
                        .map_err(|_| anyhow::anyhow!("service resolver lock poisoned"))?
                        .clone();
                    if bmux_plugin_sdk::service_contract::is_checked(&payload) {
                        // A provider exists, but this fallback cannot validate checked
                        // evidence. Never classify that as optional provider absence.
                        resolver.map(|_| -> ServiceInvokeFuture {
                            Box::pin(async {
                                Err(ServiceCompatibilityError(
                                    "fallback provider has no checked contract boundary",
                                )
                                .into())
                            })
                        })
                    } else {
                        resolver.map(|resolver| resolver(route.clone(), payload))
                    }
                };
                let resolver_us = resolver_started.elapsed().as_micros();

                let invocation_started = std::time::Instant::now();
                let response = if let Some(invocation) = invocation {
                    match invocation.await {
                        Ok(output) => Response::Ok(ResponsePayload::ServiceInvoked {
                            payload: output.payload,
                        }),
                        Err(error) => Response::Err(ErrorResponse {
                            code: service_invocation_error_code(&error),
                            message: format!("service invocation failed: {error:#}"),
                        }),
                    }
                } else {
                    Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: format!(
                            "no provider for service capability='{capability}' kind='{kind:?}' interface='{interface_id}' operation='{operation}'"
                        ),
                    })
                };
                let invocation_us = invocation_started.elapsed().as_micros();
                let elapsed = started_at.elapsed();
                #[allow(clippy::cast_possible_truncation)]
                let elapsed_ms = elapsed.as_millis() as u64;
                #[allow(clippy::cast_possible_truncation)]
                let dispatch_micros = elapsed.as_micros() as u64;
                // Companion completion log for Command-kind dispatches so
                // pair-up analysis ("which invoke begin matches which
                // outcome?") is direct. The surrounding span is retained
                // for field context on any nested events the handler
                // emitted; we don't `span.record(...)` the outcome /
                // elapsed_ms here because the explicit `info!` already
                // carries them and double-recording produces a noisy
                // `outcome="ok" outcome="ok"` in the log.
                if elapsed > Duration::from_millis(100) {
                    let level = if elapsed > Duration::from_secs(1) {
                        "severe"
                    } else {
                        "degraded"
                    };
                    tracing::warn!(
                        target: "bmux_server::invoke_service",
                        capability = %capability,
                        interface = %interface_id,
                        operation = %operation,
                        kind = ?kind,
                        client_id = ?client_id,
                        elapsed_ms,
                        dispatch_micros,
                        registry_us,
                        resolver_us,
                        invocation_us,
                        level,
                        "typed service invocation exceeded latency budget",
                    );
                }
                if matches!(kind, bmux_ipc::InvokeServiceKind::Command) {
                    let outcome_str = match &response {
                        Response::Ok(_) => "ok",
                        Response::Err(err) if err.code == ErrorCode::NotFound => "err:not_found",
                        Response::Err(_) => "err:internal",
                    };
                    tracing::info!(
                        target: "bmux_server::invoke_service",
                        capability = %capability,
                        interface = %interface_id,
                        operation = %operation,
                        client_id = ?client_id,
                        elapsed_ms,
                        dispatch_micros,
                        outcome = outcome_str,
                        "invoke command complete",
                    );
                }
                emit_phase_timing(
                    PhaseChannel::Service,
                    &PhasePayload::new("service.server_invoke")
                        .service_fields(capability, format!("{kind:?}"), interface_id, operation)
                        .field("client_id", client_id)
                        .field("registry_us", registry_us)
                        .field("resolver_us", resolver_us)
                        .field("invocation_us", invocation_us)
                        .field("total_us", elapsed.as_micros())
                        .finish(),
                );
                Ok::<_, anyhow::Error>(response)
            }
            .instrument(span)
            .await?
        }
        Request::InvokeServicePipeline { pipeline } => {
            match execute_service_pipeline(
                state,
                shutdown_tx,
                client_id,
                client_principal_id,
                pipeline,
            )
            .await
            {
                Ok(results) => Response::Ok(ResponsePayload::ServicePipelineInvoked { results }),
                Err(error) => Response::Err(ErrorResponse {
                    code: service_invocation_error_code(&error),
                    message: format!("service pipeline failed: {error:#}"),
                }),
            }
        }
        Request::EmitOnPluginBus { kind, payload } => {
            // Relay a wire-encoded payload onto the server's plugin
            // event bus. Decoders registered via
            // `EventBus::register_state_channel_with_decoder` pick it
            // up; channels without a decoder or not yet registered
            // silently drop the payload (returns `emitted: false`) so
            // early wire traffic from a client doesn't fail loudly.
            let event_kind = bmux_plugin_sdk::PluginEventKind::from_owned(kind);
            match state.plugin_events.emit_from_bytes(&event_kind, &payload) {
                Ok(emitted) => Response::Ok(ResponsePayload::PluginBusEmitted { emitted }),
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("emit_on_plugin_bus failed: {error}"),
                }),
            }
        }
        Request::SubscribeEvents => {
            state
                .event_hub
                .lock()
                .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?
                .subscribe(client_id);
            Response::Ok(ResponsePayload::EventsSubscribed)
        }
        Request::PollEvents { max_events } => {
            let mut hub = state
                .event_hub
                .lock()
                .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?;
            hub.poll(client_id, max_events).map_or_else(
                || {
                    Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: "event subscription not found for client".to_string(),
                    })
                },
                |events| Response::Ok(ResponsePayload::EventBatch { events }),
            )
        }
        Request::DeviceSealBroker { payload } => {
            let Some(target_client_id) = select_device_seal_broker_client(state).await else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: "no attached bmux client available for device-seal broker".to_string(),
                }));
            };
            let request_id = Uuid::new_v4();
            let (response_tx, response_rx) = oneshot::channel();
            state.device_seal_pending.lock().await.insert(
                request_id,
                DeviceSealPending {
                    target_client_id,
                    response_tx,
                },
            );
            info!(
                %request_id,
                %target_client_id,
                payload_len = payload.len(),
                "device_seal_broker.forwarding"
            );
            emit_event(
                state,
                Event::DeviceSealBrokerRequest {
                    request_id,
                    target_client_id: target_client_id.0,
                    payload,
                },
            )?;
            match tokio::time::timeout(Duration::from_secs(30), response_rx).await {
                Ok(Ok(payload)) => Response::Ok(ResponsePayload::DeviceSealBrokered { payload }),
                Ok(Err(_)) => Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: "device-seal broker response channel closed".to_string(),
                }),
                Err(_) => {
                    state.device_seal_pending.lock().await.remove(&request_id);
                    Response::Err(ErrorResponse {
                        code: ErrorCode::Timeout,
                        message: "timed out waiting for attached client device-seal broker"
                            .to_string(),
                    })
                }
            }
        }
        Request::DeviceSealBrokerResponse {
            request_id,
            ok,
            payload,
            error,
        } => {
            let mut pending_requests = state.device_seal_pending.lock().await;
            let Some(pending_target_client_id) = pending_requests
                .get(&request_id)
                .map(|pending| pending.target_client_id)
            else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: "device-seal broker request not found".to_string(),
                }));
            };
            if pending_target_client_id != client_id {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "device-seal broker response came from the wrong client".to_string(),
                }));
            }
            let Some(pending) = pending_requests.remove(&request_id) else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: "device-seal broker request not found".to_string(),
                }));
            };
            drop(pending_requests);
            let payload = if ok || !payload.is_empty() {
                payload
            } else {
                error
                    .unwrap_or_else(|| "device-seal broker failed".to_string())
                    .into_bytes()
            };
            let _ = pending.response_tx.send(payload);
            Response::Ok(ResponsePayload::DeviceSealBrokerResponseAccepted)
        }
        // EnableEventPush is handled in handle_connection after the response
        // is sent — the actual push task spawning happens there. Here we just
        // acknowledge the request.
        Request::EnableEventPush => Response::Ok(ResponsePayload::EventPushEnabled),
    };

    // Catalog-changed events are fired by the control-catalog plugin
    // in response to session/context/client events on the event bus.
    // Server no longer emits catalog-changed directly.

    if response_requires_snapshot(&response) {
        mark_snapshot_dirty(state);
        maybe_flush_snapshot(state, false)?;
    }

    Ok(response)
}

const fn request_requires_exclusive(request: &Request) -> bool {
    matches!(request, Request::ServerStop)
}

const fn response_requires_snapshot(_response: &Response) -> bool {
    false
}

const fn request_kind_name(request: &Request) -> &'static str {
    match request {
        Request::CurrentStreamProvenance => "current_stream_provenance",
        Request::Hello { .. } => "hello",
        Request::Ping => "ping",
        Request::WhoAmIPrincipal => "whoami_principal",
        Request::ServerStatus => "server_status",
        Request::ServerStop => "server_stop",
        Request::InvokeService { .. } => "invoke_service",
        Request::InvokeServiceAsPlugin { .. } => "invoke_service_as_plugin",
        Request::InvokeServicePipeline { .. } => "invoke_service_pipeline",
        Request::EmitOnPluginBus { .. } => "emit_on_plugin_bus",
        Request::PollEvents { .. } => "poll_events",
        Request::EnableEventPush => "enable_event_push",
        Request::SubscribeEvents => "subscribe_events",
        Request::DeviceSealBroker { .. } => "device_seal_broker",
        Request::DeviceSealBrokerResponse { .. } => "device_seal_broker_response",
    }
}

const fn response_kind_name(response: &Response) -> &'static str {
    match response {
        Response::Ok(payload) => response_payload_kind_name(payload),
        Response::Err(_) => "error",
    }
}

fn ipc_service_request_metadata(request: &Request) -> serde_json::Map<String, serde_json::Value> {
    if let Request::InvokeService {
        capability,
        kind,
        interface_id,
        operation,
        payload,
    } = request
    {
        return PhasePayload::new("unused")
            .service_fields(capability, format!("{kind:?}"), interface_id, operation)
            .field("service_payload_len", payload.len())
            .into_fields()
            .into_iter()
            .filter(|(key, _)| key.as_str() != "phase")
            .collect();
    }
    serde_json::Map::new()
}

#[allow(clippy::too_many_arguments)]
fn server_ipc_request_phase_payload(
    request_kind: &str,
    service_metadata: serde_json::Map<String, serde_json::Value>,
    request_id: u64,
    response: &Response,
    socket_read_us: u128,
    frame_decode_us: u128,
    request_decode_us: u128,
    request_record_encode_us: u128,
    request_record_us: u128,
    handle_us: u128,
    response_record_encode_us: u128,
    response_record_us: u128,
    response_encode_us: u128,
    response_frame_encode_us: u128,
    writer_queue_us: u128,
    response_send_us: u128,
    total_us: u128,
) -> serde_json::Value {
    PhasePayload::new("ipc.server_request")
        .field("request", request_kind)
        .field("request_id", request_id)
        .field("response", response_kind_name(response))
        .field("socket_read_us", socket_read_us)
        .field("frame_decode_us", frame_decode_us)
        .field("request_decode_us", request_decode_us)
        .field("request_record_encode_us", request_record_encode_us)
        .field("request_record_us", request_record_us)
        .field("handle_us", handle_us)
        .field("response_record_encode_us", response_record_encode_us)
        .field("response_record_us", response_record_us)
        .field("response_encode_us", response_encode_us)
        .field("response_frame_encode_us", response_frame_encode_us)
        .field("writer_queue_us", writer_queue_us)
        .field("response_send_us", response_send_us)
        .field("total_us", total_us)
        .extend(service_metadata)
        .finish()
}

fn all_recording_event_kinds() -> Vec<RecordingEventKind> {
    vec![
        RecordingEventKind::PaneInputRaw,
        RecordingEventKind::PaneOutputRaw,
        RecordingEventKind::ProtocolReplyRaw,
        RecordingEventKind::PaneImage,
        RecordingEventKind::ServerEvent,
        RecordingEventKind::RequestStart,
        RecordingEventKind::RequestDone,
        RecordingEventKind::RequestError,
        RecordingEventKind::Custom,
    ]
}

fn normalize_recording_event_kinds(event_kinds: &[RecordingEventKind]) -> Vec<RecordingEventKind> {
    let mut normalized = Vec::new();
    for kind in all_recording_event_kinds() {
        if event_kinds.contains(&kind) {
            normalized.push(kind);
        }
    }
    normalized
}

#[allow(clippy::fn_params_excessive_bools)]
fn recording_event_kinds_from_flags(
    capture_input: bool,
    capture_output: bool,
    capture_events: bool,
    capture_protocol_replies: bool,
    capture_images: bool,
) -> Vec<RecordingEventKind> {
    let mut event_kinds = Vec::new();
    if capture_input {
        event_kinds.push(RecordingEventKind::PaneInputRaw);
    }
    if capture_output {
        event_kinds.push(RecordingEventKind::PaneOutputRaw);
    }
    if capture_protocol_replies {
        event_kinds.push(RecordingEventKind::ProtocolReplyRaw);
    }
    if capture_images {
        event_kinds.push(RecordingEventKind::PaneImage);
    }
    if capture_events {
        event_kinds.extend([
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ]);
    }
    normalize_recording_event_kinds(&event_kinds)
}

const fn recording_event_kind_from_config(
    kind: bmux_config::RecordingEventKindConfig,
) -> RecordingEventKind {
    match kind {
        bmux_config::RecordingEventKindConfig::PaneInputRaw => RecordingEventKind::PaneInputRaw,
        bmux_config::RecordingEventKindConfig::PaneOutputRaw => RecordingEventKind::PaneOutputRaw,
        bmux_config::RecordingEventKindConfig::ProtocolReplyRaw => {
            RecordingEventKind::ProtocolReplyRaw
        }
        bmux_config::RecordingEventKindConfig::PaneImage => RecordingEventKind::PaneImage,
        bmux_config::RecordingEventKindConfig::ServerEvent => RecordingEventKind::ServerEvent,
        bmux_config::RecordingEventKindConfig::RequestStart => RecordingEventKind::RequestStart,
        bmux_config::RecordingEventKindConfig::RequestDone => RecordingEventKind::RequestDone,
        bmux_config::RecordingEventKindConfig::RequestError => RecordingEventKind::RequestError,
        bmux_config::RecordingEventKindConfig::Custom => RecordingEventKind::Custom,
    }
}

pub fn rolling_recording_settings_from_config(config: &BmuxConfig) -> RollingRecordingSettings {
    let event_kinds = if config.recording.rolling_event_kinds.is_empty() {
        recording_event_kinds_from_flags(
            config
                .recording
                .rolling_capture_input
                .unwrap_or(config.recording.capture_input),
            config
                .recording
                .rolling_capture_output
                .unwrap_or(config.recording.capture_output),
            config
                .recording
                .rolling_capture_events
                .unwrap_or(config.recording.capture_events),
            config
                .recording
                .rolling_capture_protocol_replies
                .unwrap_or(false),
            config.recording.rolling_capture_images.unwrap_or(false),
        )
    } else {
        normalize_recording_event_kinds(
            &config
                .recording
                .rolling_event_kinds
                .iter()
                .copied()
                .map(recording_event_kind_from_config)
                .collect::<Vec<_>>(),
        )
    };
    RollingRecordingSettings {
        window_secs: config.recording.rolling_window_secs,
        event_kinds,
    }
}

fn set_event_kind_enabled(
    event_kinds: &mut Vec<RecordingEventKind>,
    kind: RecordingEventKind,
    enabled: bool,
) {
    event_kinds.retain(|current| *current != kind);
    if enabled {
        event_kinds.push(kind);
    }
}

#[must_use]
pub fn apply_rolling_start_options(
    base: &RollingRecordingSettings,
    options: &RecordingRollingStartOptions,
) -> RollingRecordingSettings {
    let event_kinds = options.event_kinds.as_deref().map_or_else(
        || {
            let mut event_kinds = base.event_kinds.clone();
            if let Some(enabled) = options.capture_input {
                set_event_kind_enabled(&mut event_kinds, RecordingEventKind::PaneInputRaw, enabled);
            }
            if let Some(enabled) = options.capture_output {
                set_event_kind_enabled(
                    &mut event_kinds,
                    RecordingEventKind::PaneOutputRaw,
                    enabled,
                );
            }
            if let Some(enabled) = options.capture_protocol_replies {
                set_event_kind_enabled(
                    &mut event_kinds,
                    RecordingEventKind::ProtocolReplyRaw,
                    enabled,
                );
            }
            if let Some(enabled) = options.capture_images {
                set_event_kind_enabled(&mut event_kinds, RecordingEventKind::PaneImage, enabled);
            }
            if let Some(enabled) = options.capture_events {
                for kind in [
                    RecordingEventKind::ServerEvent,
                    RecordingEventKind::RequestStart,
                    RecordingEventKind::RequestDone,
                    RecordingEventKind::RequestError,
                    RecordingEventKind::Custom,
                ] {
                    set_event_kind_enabled(&mut event_kinds, kind, enabled);
                }
            }
            normalize_recording_event_kinds(&event_kinds)
        },
        normalize_recording_event_kinds,
    );

    RollingRecordingSettings {
        window_secs: options.window_secs.unwrap_or(base.window_secs),
        event_kinds,
    }
}

const fn response_payload_kind_name(payload: &ResponsePayload) -> &'static str {
    match payload {
        ResponsePayload::Pong => "pong",
        ResponsePayload::PrincipalIdentity { .. } => "principal_identity",
        ResponsePayload::CurrentStreamProvenance { .. } => "current_stream_provenance",
        ResponsePayload::HelloNegotiated { .. } => "hello_negotiated",
        ResponsePayload::HelloIncompatible { .. } => "hello_incompatible",
        ResponsePayload::ServerStatus { .. } => "server_status",
        ResponsePayload::ServerStopping => "server_stopping",
        ResponsePayload::ServiceInvoked { .. } => "service_invoked",
        ResponsePayload::ServicePipelineInvoked { .. } => "service_pipeline_invoked",
        ResponsePayload::EventsSubscribed => "events_subscribed",
        ResponsePayload::EventBatch { .. } => "event_batch",
        ResponsePayload::EventPushEnabled => "event_push_enabled",
        ResponsePayload::PluginBusEmitted { .. } => "plugin_bus_emitted",
        ResponsePayload::DeviceSealBrokered { .. } => "device_seal_brokered",
        ResponsePayload::DeviceSealBrokerResponseAccepted => "device_seal_broker_response_accepted",
    }
}

fn detach_client_state_on_disconnect(
    state: &Arc<ServerState>,
    client_id: ClientId,
    selected_session: &mut Option<SessionId>,
    attached_stream_session: &mut Option<SessionId>,
) {
    let previous_selected = selected_session.take();
    let previous_stream = attached_stream_session.take();

    context_handle(&state.plugin_states)
        .0
        .disconnect_client(client_id);

    if previous_selected.is_none() && previous_stream.is_none() {
        return;
    }

    if let Some(session_id) = previous_selected {
        session_handle(&state.plugin_states)
            .0
            .remove_client(session_id, &client_id);
    }

    if let Some(stream_session_id) = previous_stream {
        session_runtime_handle(&state.plugin_states)
            .0
            .end_attach(stream_session_id, client_id);
    }
}

fn parse_request(envelope: &Envelope) -> Result<Request> {
    if envelope.kind != EnvelopeKind::Request {
        anyhow::bail!("expected request envelope kind")
    }
    decode(&envelope.payload).context("failed to decode request payload")
}

fn load_or_create_principal_id(paths: &ConfigPaths) -> Result<Uuid> {
    let path = paths.principal_id_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating principal id dir {}", parent.display()))?;
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let raw = content.trim();
            Uuid::parse_str(raw)
                .with_context(|| format!("invalid principal id in {}: {}", path.display(), raw))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let principal_id = Uuid::new_v4();
            std::fs::write(&path, principal_id.to_string())
                .with_context(|| format!("failed writing principal id file {}", path.display()))?;
            Ok(principal_id)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed reading principal id file {}", path.display())),
    }
}

async fn send_ok(
    stream: &mut ErasedIpcStream,
    request_id: u64,
    payload: ResponsePayload,
) -> Result<()> {
    let response = Response::Ok(payload);
    send_response(stream, request_id, response).await
}

async fn send_error(
    stream: &mut ErasedIpcStream,
    request_id: u64,
    code: ErrorCode,
    message: String,
) -> Result<()> {
    let response = Response::Err(ErrorResponse { code, message });
    send_response(stream, request_id, response).await
}

async fn send_response(
    stream: &mut ErasedIpcStream,
    request_id: u64,
    response: Response,
) -> Result<()> {
    let payload = encode(&response).context("failed encoding response payload")?;
    let envelope = Envelope::new(request_id, EnvelopeKind::Response, payload);
    stream
        .send_envelope(&envelope)
        .await
        .context("failed sending response envelope")
}

async fn send_error_via_channel(
    frame_tx: &mpsc::Sender<Vec<u8>>,
    request_id: u64,
    code: ErrorCode,
    message: String,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Duration,
    frame_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
) -> Result<()> {
    let response = Response::Err(ErrorResponse { code, message });
    send_response_via_channel(
        frame_tx,
        request_id,
        &response,
        shutdown,
        deadline,
        frame_codec,
    )
    .await
    .map(|_| ())
}

#[derive(Debug, Clone, Copy, Default)]
struct IpcResponseSendTiming {
    response_encode: u128,
    frame_encode: u128,
    writer_queue: u128,
}

async fn send_response_via_channel(
    frame_tx: &mpsc::Sender<Vec<u8>>,
    request_id: u64,
    response: &Response,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Duration,
    frame_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
) -> Result<IpcResponseSendTiming> {
    let response_encode_started = Instant::now();
    let payload = encode(response).context("failed encoding response payload")?;
    let response_encode_us = response_encode_started.elapsed().as_micros();
    let envelope = Envelope::new(request_id, EnvelopeKind::Response, payload);
    let frame_encode_started = Instant::now();
    let frame = if frame_codec.is_some() {
        bmux_ipc::frame::encode_frame_compressed(&envelope, frame_codec)
            .context("failed encoding compressed response frame")?
    } else {
        bmux_ipc::frame::encode_frame(&envelope).context("failed encoding response frame")?
    };
    let frame_encode_us = frame_encode_started.elapsed().as_micros();
    let queue_started = Instant::now();
    // Reserve before enqueueing: cancellation/timeout must not publish a frame.
    // Each connection has one serial response producer, so waiting retains at
    // most one extra bounded frame rather than spawning unbounded send tasks.
    let permit = tokio::select! {
        biased;
        () = async {
            loop {
                if *shutdown.borrow_and_update() {
                    break;
                }
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
        } => anyhow::bail!("response delivery cancelled by server shutdown"),
        result = tokio::time::timeout(deadline, frame_tx.reserve()) => {
            result
                .context("response writer queue capacity deadline exceeded")?
                .map_err(|_| anyhow::anyhow!("response writer queue closed"))?
        }
    };
    permit.send(frame);
    Ok(IpcResponseSendTiming {
        response_encode: response_encode_us,
        frame_encode: frame_encode_us,
        writer_queue: queue_started.elapsed().as_micros(),
    })
}

/// Resolve a frame compression codec from negotiated capability strings.
///
/// Prefers lz4 for frames (fastest), falls back to zstd.
fn resolve_frame_codec_from_capabilities(
    capabilities: &[String],
) -> Option<std::sync::Arc<dyn bmux_ipc::compression::CompressionCodec>> {
    use bmux_ipc::compression;
    if capabilities
        .iter()
        .any(|c| c == bmux_ipc::CAPABILITY_COMPRESSION_FRAME_LZ4)
    {
        compression::resolve_codec(CompressionId::Lz4).map(std::sync::Arc::from)
    } else if capabilities
        .iter()
        .any(|c| c == bmux_ipc::CAPABILITY_COMPRESSION_FRAME_ZSTD)
    {
        compression::resolve_codec(CompressionId::Zstd).map(std::sync::Arc::from)
    } else {
        None
    }
}

/// Returns `true` when `err` was caused by the IPC frame payload exceeding the
/// maximum size.  Used to degrade gracefully instead of tearing down the entire
/// client connection.
fn is_frame_too_large_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(IpcTransportError::FrameEncode(
            bmux_ipc::frame::FrameEncodeError::PayloadTooLarge { .. },
        )) = cause.downcast_ref::<IpcTransportError>()
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
#[allow(
    clippy::significant_drop_tightening,
    reason = "Test scaffolding holds guards only for short critical sections; tightening adds churn without runtime benefit."
)]
mod tests {
    use super::*;
    use bmux_ipc::transport::LocalIpcStream;
    use serde::{Deserialize, Serialize};

    async fn next_plugin_bus_event(
        rx: &mut tokio::sync::broadcast::Receiver<Event>,
    ) -> (String, serde_json::Value) {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("forwarded event")
                .expect("broadcast open");
            if let Event::PluginBusEvent { kind, payload } = event {
                return (
                    kind,
                    serde_json::from_slice(&payload).expect("JSON payload"),
                );
            }
        }
    }

    #[tokio::test]
    async fn json_state_forwarder_waits_for_and_follows_re_registered_channels() {
        let server = BmuxServer::new(IpcEndpoint::unix_socket("json-forwarder-test"));
        let kind =
            bmux_plugin_sdk::PluginEventKind::from("example.remote/presentation".to_string());
        let mut events = server.state.event_broadcast.subscribe();
        // Spawned before the plugin registers its channel.
        server.spawn_plugin_bus_json_state_forwarder(&kind).unwrap();
        tokio::task::yield_now().await;
        let bus = server.state.plugin_events.clone();
        bus.register_json_state_channel(kind.clone()).unwrap();
        bus.emit_json(&kind, &serde_json::json!({ "revision": 1 }))
            .unwrap();
        assert_eq!(
            next_plugin_bus_event(&mut events).await,
            (
                kind.as_str().to_string(),
                serde_json::json!({ "revision": 1 })
            )
        );
        // A reloaded plugin replaces its channel; the forwarder follows it.
        bus.register_json_state_channel(kind.clone()).unwrap();
        tokio::task::yield_now().await;
        bus.emit_json(&kind, &serde_json::json!({ "revision": 2 }))
            .unwrap();
        assert_eq!(
            next_plugin_bus_event(&mut events).await.1,
            serde_json::json!({ "revision": 2 })
        );
        // Clients that attach later receive the retained value.
        let replayers = server.state.plugin_state_replayers.lock().unwrap().clone();
        let replayed = replayers
            .iter()
            .find_map(|replay| replay())
            .expect("replayed");
        assert!(matches!(
            replayed,
            Event::PluginBusEvent { kind: replayed_kind, .. } if replayed_kind == kind.as_str()
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supplied_listener_initialization_failure_does_not_rebind() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = IpcEndpoint::UnixSocket(root.path().join("server.sock"));
        let server = BmuxServer::new(endpoint.clone());
        let listener =
            LocalIpcListener::bind_with_policy(&endpoint, ConfigPaths::local_ipc_policy()).unwrap();
        let identity = std::fs::symlink_metadata(root.path().join("server.sock"))
            .unwrap()
            .ino();
        // Poison the first initialization operation to fail before any await.
        let state = Arc::clone(&server.state);
        assert!(
            std::thread::spawn(move || {
                let _guard = state.event_hub.lock().unwrap();
                panic!("injected initialization failure");
            })
            .join()
            .is_err()
        );
        let error = server.run_with_listener(listener, None).await.unwrap_err();
        assert_eq!(error.to_string(), "event hub lock poisoned");
        assert_eq!(
            std::fs::symlink_metadata(root.path().join("server.sock"))
                .unwrap()
                .ino(),
            identity
        );
    }

    #[tokio::test]
    async fn a_wake_reaches_only_its_recipient_once_and_needs_a_connection() {
        let retention = Arc::new(RetainedWireEvents::default());
        let woken = Uuid::new_v4();
        let other = Uuid::new_v4();
        let woken_registration = retention.register(woken).unwrap();
        let _other_registration = retention.register(other).unwrap();
        retention.wake(woken).unwrap();
        retention.wake(woken).unwrap();
        woken_registration.ready.notified().await;
        assert!(!retention.take_wake(other));
        assert!(retention.take_wake(woken));
        assert!(!retention.take_wake(woken));
        assert!(retention.wake(Uuid::new_v4()).is_err());
        drop(woken_registration);
        assert!(retention.wake(woken).is_err());
    }

    #[tokio::test]
    async fn retained_transport_owner_hydration_coalescing_and_isolation() {
        let retention = Arc::new(RetainedWireEvents::default());
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let owner_registration = retention.register(owner).unwrap();
        let other_registration = retention.register(other).unwrap();
        retention.publish(owner, "opaque".into(), vec![1]).unwrap();
        retention.publish(owner, "opaque".into(), vec![2]).unwrap();
        assert!(retention.take_pending(other).is_none());
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                other_registration.ready.notified()
            )
            .await
            .is_err()
        );
        owner_registration.ready.notified().await;
        assert!(
            matches!(retention.take_pending(owner), Some(Event::PluginBusEvent { kind, payload }) if kind == "opaque" && payload == vec![2])
        );
        assert!(retention.take_pending(owner).is_none());
        retention.publish(owner, "opaque".into(), vec![3]).unwrap();
        owner_registration.ready.notified().await;
        assert!(
            matches!(retention.take_pending(owner), Some(Event::PluginBusEvent { payload, .. }) if payload == vec![3])
        );
        retention.publish(owner, "opaque".into(), vec![4]).unwrap();
        retention.clear(owner, "opaque").unwrap();
        assert!(retention.take_pending(owner).is_none());
    }

    #[cfg(unix)]
    async fn retained_transport_request(peer: &mut LocalIpcStream, request: Request) {
        let envelope = Envelope::new(1, EnvelopeKind::Request, encode(&request).unwrap());
        peer.send_envelope(&envelope).await.unwrap();
        let response = peer.recv_envelope().await.unwrap();
        assert_eq!(response.kind, EnvelopeKind::Response);
    }

    #[cfg(unix)]
    async fn retained_transport_recipient(state: &ServerState, principal: Uuid) -> Uuid {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let found = state
                    .client_principals
                    .lock()
                    .unwrap()
                    .iter()
                    .find_map(|(id, value)| (*value == principal).then_some(id.0));
                if let Some(id) = found
                    && state
                        .retained_wire_events
                        .recipients
                        .lock()
                        .unwrap()
                        .contains_key(&id)
                {
                    break id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[test]
    fn shutdown_requested_before_subscription_is_retained() {
        let endpoint = IpcEndpoint::unix_socket(
            std::env::temp_dir().join(format!("bmux-early-shutdown-{}.sock", Uuid::new_v4())),
        );
        let server = BmuxServer::new(endpoint);
        server.request_shutdown();
        assert!(*server.shutdown_tx.subscribe().borrow());
    }

    #[cfg(unix)]
    fn private_socket_path() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp")
            .join(format!("bmux-{}", Uuid::new_v4().simple()))
            .join("ipc.sock")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retained_transport_push_is_private_and_public_flow_is_preserved() {
        let path = private_socket_path();
        let endpoint = IpcEndpoint::unix_socket(path.clone());
        let server = BmuxServer::new(endpoint.clone());
        let sink = ServerWireEventSink {
            state: server.state.clone(),
        };
        let listener = LocalIpcListener::bind(&endpoint).unwrap();
        let pool = Arc::new(tokio::sync::Semaphore::new(2));
        let mut peers = Vec::new();
        let mut tasks = Vec::new();
        let mut aborts = Vec::new();
        let mut recipients = Vec::new();
        for _ in 0..2 {
            let mut peer = LocalIpcStream::connect(&endpoint).await.unwrap();
            let stream = listener.accept().await.unwrap();
            let permit = Arc::new(ConnectionSlot::try_acquire(&pool).unwrap());
            let (state, stop) = (Arc::clone(&server.state), server.shutdown_tx.clone());
            let task = tokio::spawn(handle_connection(state, stop, stream.into(), None, permit));
            aborts.push(AbortOnDrop(task.abort_handle()));
            tasks.push(task);
            let principal = Uuid::new_v4();
            retained_transport_request(
                &mut peer,
                Request::Hello {
                    contract: ProtocolContract::current(
                        CORE_PROTOCOL_CAPABILITIES
                            .iter()
                            .map(|value| (*value).to_string())
                            .collect(),
                    ),
                    client_name: "retained-transport-test".into(),
                    principal_id: principal,
                },
            )
            .await;
            let recipient = retained_transport_recipient(&server.state, principal).await;
            recipients.push(recipient);
            peers.push(peer);
        }
        sink.publish_retained_to(recipients[0], "opaque".into(), vec![2])
            .unwrap();
        let mut public_rx = server.state.event_broadcast.subscribe();
        sink.publish_retained_to(recipients[0], "opaque".into(), vec![3])
            .unwrap();
        assert!(matches!(
            public_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        for peer in &mut peers {
            retained_transport_request(peer, Request::EnableEventPush).await;
        }
        let event = tokio::time::timeout(Duration::from_secs(2), peers[0].recv_envelope())
            .await
            .unwrap()
            .unwrap();
        let event: Event = decode(&event.payload).unwrap();
        assert!(
            matches!(event, Event::PluginBusEvent { kind, payload } if kind == "opaque" && payload == vec![3])
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peers[1].recv_envelope())
                .await
                .is_err()
        );
        sink.publish(Event::ServerStarted).unwrap();
        for peer in &mut peers {
            let event = tokio::time::timeout(Duration::from_secs(2), peer.recv_envelope())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                decode::<Event>(&event.payload).unwrap(),
                Event::ServerStarted
            ));
        }
        drop(peers);
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        assert!(
            server
                .state
                .retained_wire_events
                .recipients
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(
            sink.publish_retained_to(recipients[0], "opaque".into(), vec![4])
                .is_err()
        );
        drop((aborts, listener));
        std::fs::remove_file(path).unwrap(); // This fixture owns the socket pathname.
    }

    #[cfg(unix)]
    #[test]
    fn retained_transport_liveness_tracks_registration_drop_and_poison() {
        let endpoint = IpcEndpoint::unix_socket(
            std::env::temp_dir().join(format!("bmux-liveness-{}.sock", Uuid::new_v4())),
        );
        let server = BmuxServer::new(endpoint);
        let sink = ServerWireEventSink {
            state: Arc::clone(&server.state),
        };
        let recipient = Uuid::new_v4();
        assert_eq!(sink.is_connected(recipient), Some(false));
        let registration = server
            .state
            .retained_wire_events
            .register(recipient)
            .unwrap();
        assert_eq!(sink.is_connected(recipient), Some(true));
        drop(registration);
        assert_eq!(sink.is_connected(recipient), Some(false));
        let retention = &server.state.retained_wire_events;
        let poisoned = std::panic::catch_unwind(|| {
            let _guard = retention.recipients.lock().unwrap();
            panic!("poison isolated retention fixture");
        });
        assert!(poisoned.is_err());
        assert_eq!(sink.is_connected(recipient), None);
    }

    #[test]
    fn retained_transport_retirement_rejects_resurrection() {
        let retention = Arc::new(RetainedWireEvents::default());
        let owner = Uuid::new_v4();
        let registration = retention.register(owner).unwrap();
        retention.publish(owner, "opaque".into(), vec![1]).unwrap();
        drop(registration);
        assert!(retention.recipients.lock().unwrap().is_empty());
        assert!(retention.publish(owner, "opaque".into(), vec![2]).is_err());
        assert!(retention.clear(owner, "opaque").is_err());
    }

    #[test]
    fn retained_transport_enforces_limits_and_allows_replacement() {
        let retention = Arc::new(RetainedWireEvents::default());
        let owner = Uuid::new_v4();
        let _registration = retention.register(owner).unwrap();
        retention.publish(owner, "opaque".into(), vec![1]).unwrap();
        assert!(
            retention
                .publish(owner, "opaque".into(), vec![0; RETAINED_PAYLOAD_BYTES + 1])
                .is_err()
        );
        assert!(
            retention
                .publish(owner, "x".repeat(RETAINED_KIND_BYTES + 1), vec![])
                .is_err()
        );
        for index in 1..RETAINED_KIND_LIMIT {
            retention.publish(owner, index.to_string(), vec![]).unwrap();
        }
        assert!(retention.publish(owner, "overflow".into(), vec![]).is_err());
        retention.publish(owner, "opaque".into(), vec![2]).unwrap();
        retention.clear(owner, "opaque").unwrap();
        retention
            .publish(owner, "replacement".into(), vec![])
            .unwrap();
    }

    #[tokio::test]
    async fn opaque_host_dispatch_context_survives_thread_and_await_without_wire_authority() {
        struct Marker(u64);
        let server = BmuxServer::new(IpcEndpoint::unix_socket(
            std::env::temp_dir().join("bmux-opaque-host-context.sock"),
        ));
        let context = server
            .host_service_context()
            .with_host_dispatch_context(Arc::new(Marker(41)));
        assert!(context.host_dispatch_context::<String>().is_err());
        let moved = tokio::task::spawn_blocking(move || context).await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            moved.host_dispatch_context::<Marker>().unwrap().unwrap().0,
            41
        );
        // A newly authenticated wire entry gets no host context. Neither the
        // request codec nor payload can construct this Rust-local Arc.
        assert!(
            server
                .host_service_context()
                .host_dispatch_context::<Marker>()
                .unwrap()
                .is_none()
        );
        server
            .register_checked_service_handler_with_metadata(
                "example.read",
                bmux_ipc::InvokeServiceKind::Query,
                "example/v1",
                "*",
                |_, context, _| async move {
                    tokio::task::yield_now().await;
                    let marker = context
                        .host_dispatch_context::<Marker>()?
                        .context("expected opaque host dependency")?;
                    Ok(ServiceInvokeOutput::from(vec![u8::try_from(marker.0)?]))
                },
            )
            .unwrap();
        let request = Request::InvokeService {
            capability: "example.read".into(),
            kind: bmux_ipc::InvokeServiceKind::Query,
            interface_id: "example/v1".into(),
            operation: "read".into(),
            payload: vec![],
        };
        let result = moved.execute_raw(encode(&request).unwrap()).await.unwrap();
        assert!(matches!(decode::<Response>(&result).unwrap(),
            Response::Ok(ResponsePayload::ServiceInvoked { payload }) if payload == vec![41]));
    }

    #[test]
    fn selected_availability_reads_exact_registry_and_denial_is_not_absence() {
        use bmux_plugin_sdk::typed_dispatch::{
            SelectedServiceAvailability, ServiceAvailabilityKey,
        };
        let server = BmuxServer::new(IpcEndpoint::unix_socket(
            std::env::temp_dir().join("bmux-availability-unit.sock"),
        ));
        let context = server.host_service_context();
        let key = ServiceAvailabilityKey {
            capability: "example.read".into(),
            kind: bmux_plugin_sdk::ServiceKind::Query,
            interface_id: "example/v1".into(),
        };
        assert_eq!(
            context.selected_service_availability(&key).unwrap(),
            SelectedServiceAvailability::Absent
        );
        server
            .register_checked_service_handler_with_metadata(
                "example.read",
                bmux_ipc::InvokeServiceKind::Query,
                "example/v1",
                "*",
                |_, _, _| async { anyhow::bail!("availability must not execute provider") },
            )
            .unwrap();
        assert_eq!(
            context.selected_service_availability(&key).unwrap(),
            SelectedServiceAvailability::Present
        );
        let mut other = key.clone();
        other.interface_id = "example/v2".into();
        assert_eq!(
            context.selected_service_availability(&other).unwrap(),
            SelectedServiceAvailability::Absent
        );
        let local_id = ClientId::new();
        let mut external = context;
        external.host_internal = false;
        external.client_id = local_id;
        // An invalid principal/stream cannot distinguish present from absent.
        assert!(external.selected_service_availability(&key).is_err());
        assert!(external.selected_service_availability(&other).is_err());
        let _origin = server
            .state
            .stream_provenance
            .record(local_id.0, None)
            .unwrap();
        assert_eq!(
            external.selected_service_availability(&key).unwrap(),
            SelectedServiceAvailability::Present
        );
    }

    #[test]
    fn service_compatibility_errors_are_structured_not_message_matched() {
        let incompatible = anyhow::Error::new(ServiceCompatibilityError("provider mismatch"));
        assert_eq!(
            service_invocation_error_code(&incompatible),
            ErrorCode::ServiceIncompatible
        );
        let protocol = anyhow::Error::new(bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: "mismatch".into(),
        });
        assert_eq!(
            service_invocation_error_code(&protocol),
            ErrorCode::ServiceIncompatible
        );
        let protocol = protocol.context("native provider invocation");
        assert_eq!(
            service_invocation_error_code(&protocol),
            ErrorCode::ServiceIncompatible
        );
        let ordinary_native_failure =
            anyhow::Error::new(bmux_plugin_sdk::PluginError::NativeServiceInvocation {
                plugin_id: "example.provider".into(),
                status: 3,
            })
            .context("native provider invocation");
        assert_eq!(
            service_invocation_error_code(&ordinary_native_failure),
            ErrorCode::Internal
        );
        let unrelated = anyhow::anyhow!("service compatibility mismatch NotFound");
        assert_eq!(
            service_invocation_error_code(&unrelated),
            ErrorCode::Internal
        );
    }

    #[tokio::test]
    async fn plugin_attribution_is_honored_only_from_local_streams() {
        let endpoint =
            IpcEndpoint::unix_socket(std::env::temp_dir().join("bmux-caller-plugin-test.sock"));
        let server = BmuxServer::new(endpoint);
        server
            .register_service_handler(
                "test.whoami",
                bmux_ipc::InvokeServiceKind::Query,
                "whoami/v1",
                "read",
                |_, context, _| async move {
                    Ok(context
                        .caller_plugin_id()
                        .unwrap_or("<host>")
                        .as_bytes()
                        .to_vec())
                },
            )
            .unwrap();
        let attributed = |caller: &str| Request::InvokeServiceAsPlugin {
            caller_plugin_id: caller.into(),
            capability: "test.whoami".into(),
            kind: bmux_ipc::InvokeServiceKind::Query,
            interface_id: "whoami/v1".into(),
            operation: "read".into(),
            payload: Vec::new(),
        };
        let plain = Request::InvokeService {
            capability: "test.whoami".into(),
            kind: bmux_ipc::InvokeServiceKind::Query,
            interface_id: "whoami/v1".into(),
            operation: "read".into(),
            payload: Vec::new(),
        };
        let call = |client_id: ClientId, request: Request| {
            let state = Arc::clone(&server.state);
            let shutdown_tx = server.shutdown_tx.clone();
            async move {
                let mut selected = None;
                handle_request(
                    &state,
                    &shutdown_tx,
                    client_id,
                    Uuid::new_v4(),
                    &mut selected,
                    request,
                )
                .await
                .unwrap()
            }
        };
        let invoked = |caller: &str| {
            Response::Ok(ResponsePayload::ServiceInvoked {
                payload: caller.as_bytes().to_vec(),
            })
        };

        let local_id = ClientId::new();
        let _origin = server
            .state
            .stream_provenance
            .record(local_id.0, None)
            .unwrap();
        assert_eq!(
            call(local_id, attributed("example.native")).await,
            invoked("example.native")
        );
        assert_eq!(call(local_id, plain.clone()).await, invoked("<host>"));
        assert_eq!(call(local_id, attributed("  ")).await, invoked("<host>"));

        // Streams without valid local provenance never get an attribution.
        let unknown = ClientId::new();
        assert!(accepted_caller_plugin_attribution(&server.state, unknown, "x").is_none());
        assert!(!matches!(
            call(unknown, attributed("example.native")).await,
            Response::Ok(_)
        ));

        // Host-attributed kernel contexts carry the identity into dispatch.
        let context = ServiceInvokeContext {
            host_internal: false,
            state: Arc::clone(&server.state),
            shutdown_tx: server.shutdown_tx.clone(),
            client_id: local_id,
            client_principal_id: Uuid::new_v4(),
            caller_plugin_id: None,
            host_dispatch_context: None,
        };
        let raw = encode(&plain).unwrap();
        for (context, expected) in [
            (
                context.with_caller_plugin_id("example.native"),
                "example.native",
            ),
            (context, "<host>"),
        ] {
            assert_eq!(
                decode::<Response>(&context.execute_raw(raw.clone()).await.unwrap()).unwrap(),
                invoked(expected)
            );
        }
    }

    #[tokio::test]
    async fn checked_pipeline_validates_actual_provider_before_execution() {
        let endpoint =
            IpcEndpoint::unix_socket(std::env::temp_dir().join("bmux-pipeline-contract-test.sock"));
        let server = BmuxServer::new(endpoint);
        let local_id = ClientId::new();
        let _origin = server
            .state
            .stream_provenance
            .record(local_id.0, None)
            .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider_calls = Arc::clone(&calls);
        server
            .register_checked_service_handler_with_metadata(
                "example.write",
                bmux_ipc::InvokeServiceKind::Command,
                "example/v1",
                "execute",
                move |route, _, payload| {
                    let calls = Arc::clone(&provider_calls);
                    async move {
                        bmux_plugin_sdk::service_contract::decode_for(
                            &route.interface_id,
                            &[1; 32],
                            &payload,
                        )?;
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(ServiceInvokeOutput::from(
                            bmux_plugin_sdk::service_contract::encode_response(b"done"),
                        ))
                    }
                },
            )
            .unwrap();
        for (hash, succeeds) in [([2; 32], false), ([1; 32], true)] {
            let pipeline = ServicePipelineRequest {
                inputs: BTreeMap::new(),
                steps: vec![bmux_ipc::ServicePipelineStep {
                    capability: "example.write".into(),
                    kind: bmux_ipc::InvokeServiceKind::Command,
                    interface_id: "example/v1".into(),
                    operation: "execute".into(),
                    payload: ServicePipelinePayload::Encoded {
                        payload: bmux_plugin_sdk::service_contract::encode(
                            "example/v1",
                            &hash,
                            b"request",
                        )
                        .unwrap(),
                    },
                }],
            };
            let outcome = execute_service_pipeline(
                &server.state,
                &server.shutdown_tx,
                local_id,
                Uuid::new_v4(),
                pipeline,
            )
            .await;
            assert_eq!(outcome.is_ok(), succeeds);
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(succeeds)
            );
            if let Ok(results) = outcome {
                assert_eq!(
                    bmux_plugin_sdk::service_contract::decode_response(&results[0].payload)
                        .unwrap(),
                    b"done"
                );
            }
        }
    }

    #[tokio::test]
    async fn checked_handler_admission_is_bound_to_registered_handler() {
        let endpoint =
            IpcEndpoint::unix_socket(std::env::temp_dir().join("bmux-contract-test.sock"));
        let server = BmuxServer::new(endpoint);
        let local_id = ClientId::new();
        let _origin = server
            .state
            .stream_provenance
            .record(local_id.0, None)
            .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        server
            .register_service_handler(
                "example.read",
                bmux_ipc::InvokeServiceKind::Query,
                "state/v1",
                "*",
                move |_, _, _| {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async { Ok(vec![]) }
                },
            )
            .unwrap();
        let route = ServiceRoute {
            capability: "example.read".into(),
            kind: bmux_ipc::InvokeServiceKind::Query,
            interface_id: "state/v1".into(),
            operation: "get".into(),
        };
        let payload =
            bmux_plugin_sdk::service_contract::encode("state/v1", &[7; 32], b"request").unwrap();
        let result = invoke_service_output(
            &server.state,
            &server.shutdown_tx,
            local_id,
            Uuid::new_v4(),
            route.clone(),
            payload.clone(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let counted = Arc::clone(&calls);
        server
            .register_checked_service_handler_with_metadata(
                "example.read",
                bmux_ipc::InvokeServiceKind::Query,
                "state/v1",
                "*",
                move |_, _, _| {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async { Ok(ServiceInvokeOutput::from(vec![])) }
                },
            )
            .unwrap();
        invoke_service_output(
            &server.state,
            &server.shutdown_tx,
            local_id,
            Uuid::new_v4(),
            route,
            payload,
        )
        .await
        .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct PipelineTemplateTarget {
        context_id: String,
        can_write: bool,
        cols: u16,
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonreading_socket_terminates_writer_and_releases_admission() {
        let path = std::path::PathBuf::from("/tmp")
            .join(format!("bmux-{}", Uuid::new_v4().simple()))
            .join("ipc.sock");
        let endpoint = IpcEndpoint::UnixSocket(path.clone());
        let listener = LocalIpcListener::bind(&endpoint).unwrap();
        let peer = LocalIpcStream::connect(&endpoint).await.unwrap();
        let stream: ErasedIpcStream = listener.accept().await.unwrap().into();
        let (reader, writer) = stream.into_split();
        let (tx, rx) = mpsc::channel(16);
        let pool = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::clone(&pool).try_acquire_owned().unwrap();
        // Large valid-sized frames exceed the socket buffer while the peer
        // deliberately performs no reads. Keep tx alive to exclude queue EOF.
        for _ in 0..16 {
            tx.try_send(vec![0; bmux_ipc::frame::MAX_FRAME_PAYLOAD_SIZE])
                .unwrap();
        }
        let task = tokio::spawn(async move {
            write_queued_frames(writer, rx, Duration::from_millis(50)).await;
            drop(permit);
        });
        let _abort = AbortOnDrop(task.abort_handle());
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(tx.is_closed());
        assert_eq!(pool.available_permits(), 1);
        drop((reader, peer, listener));
        assert!(
            path.exists(),
            "generic listener drop preserves endpoint ownership"
        );
        std::fs::remove_file(&path).unwrap(); // This isolated fixture owns the socket pathname.
    }

    #[tokio::test]
    async fn cancelling_parent_aborts_owned_task_and_restores_admission() {
        let pool = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::clone(&pool).try_acquire_owned().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let parent = tokio::spawn(async move {
            let child = tokio::spawn(async move {
                std::future::pending::<()>().await;
                drop(permit);
            });
            let _abort = AbortOnDrop(child.abort_handle());
            ready_tx.send(child).unwrap();
            std::future::pending::<()>().await;
        });
        let child = ready_rx.await.unwrap();
        assert_eq!(pool.available_permits(), 0);
        parent.abort();
        assert!(parent.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), child)
                .await
                .unwrap()
                .unwrap_err()
                .is_cancelled()
        );
        assert_eq!(pool.available_permits(), 1);
    }

    #[tokio::test]
    async fn transport_admission_remains_charged_until_writer_finishes() {
        let pool = Arc::new(tokio::sync::Semaphore::new(1));
        let handler = Arc::new(ConnectionSlot::try_acquire(&pool).unwrap());
        let writer = Arc::clone(&handler);
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(writer);
        });
        drop(handler);
        assert!(Arc::clone(&pool).try_acquire_owned().is_err());
        finish_writer(task, std::time::Duration::from_millis(1)).await;
        assert!(Arc::clone(&pool).try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn stalled_writer_is_aborted_and_releases_owned_frames() {
        let owned = Arc::new(vec![0_u8; 1024]);
        let weak = Arc::downgrade(&owned);
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(owned);
        });
        finish_writer(task, std::time::Duration::from_millis(1)).await;
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn response_queue_waits_for_slow_consumer_and_preserves_burst_order() {
        let (tx, mut rx) = mpsc::channel(16);
        let (_shutdown_tx, mut shutdown) = watch::channel(false);
        let response = Response::Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: "test".to_string(),
        });
        for request_id in 0..16 {
            send_response_via_channel(
                &tx,
                request_id,
                &response,
                &mut shutdown,
                Duration::from_secs(1),
                None,
            )
            .await
            .unwrap();
        }
        let pending = send_response_via_channel(
            &tx,
            16,
            &response,
            &mut shutdown,
            Duration::from_secs(1),
            None,
        );
        tokio::pin!(pending);
        // Poll the send while full; it must wait rather than disconnect.
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        let first = rx.recv().await.unwrap();
        assert_eq!(
            bmux_ipc::frame::decode_frame_exact(&first)
                .unwrap()
                .request_id,
            0
        );
        pending.await.unwrap();
        for request_id in 1..17 {
            let frame = rx.recv().await.unwrap();
            assert_eq!(
                bmux_ipc::frame::decode_frame_exact(&frame)
                    .unwrap()
                    .request_id,
                request_id
            );
        }
    }

    #[tokio::test]
    async fn response_queue_deadline_cancellation_and_closure_are_distinct() {
        let (tx, mut rx) = mpsc::channel(1);
        let (shutdown_tx, mut shutdown) = watch::channel(false);
        let response = Response::Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: "test".to_string(),
        });
        send_response_via_channel(
            &tx,
            1,
            &response,
            &mut shutdown,
            Duration::from_secs(1),
            None,
        )
        .await
        .unwrap();
        let error = send_response_via_channel(
            &tx,
            2,
            &response,
            &mut shutdown,
            Duration::from_millis(1),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("capacity deadline exceeded"));
        assert_eq!(rx.len(), 1, "timed out response must not be enqueued");
        shutdown_tx.send(true).unwrap();
        let error = send_response_via_channel(
            &tx,
            3,
            &response,
            &mut shutdown,
            Duration::from_secs(1),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(rx.len(), 1);
        rx.recv().await.unwrap();
        let error = send_response_via_channel(
            &tx,
            4,
            &response,
            &mut shutdown,
            Duration::from_secs(1),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("cancelled"),
            "shutdown wins even with available capacity"
        );
        shutdown_tx.send(false).unwrap();
        drop(rx);
        let error = send_response_via_channel(
            &tx,
            5,
            &response,
            &mut shutdown,
            Duration::from_secs(1),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("queue closed"));
    }

    #[test]
    fn pipeline_json_template_field_order_encodes_struct_payload() {
        let payload = ServicePipelinePayload::JsonTemplate {
            value: serde_json::json!({
                "context_id": { "$metadata": "0:context_id" },
                "can_write": true,
                "cols": { "$input": "cols" },
            }),
            field_order: Some(vec![
                "context_id".to_string(),
                "can_write".to_string(),
                "cols".to_string(),
            ]),
        };
        let inputs = BTreeMap::from([("cols".to_string(), serde_json::json!(120))]);
        let metadata = vec![BTreeMap::from([(
            "context_id".to_string(),
            serde_json::json!("ctx-1"),
        )])];

        let encoded = resolve_pipeline_payload(&payload, &inputs, &metadata)
            .expect("pipeline template should encode");
        let decoded: PipelineTemplateTarget = bmux_codec::from_positional_bytes(&encoded)
            .expect("payload should decode as target struct");

        assert_eq!(
            decoded,
            PipelineTemplateTarget {
                context_id: "ctx-1".to_string(),
                can_write: true,
                cols: 120,
            }
        );
    }

    #[test]
    fn lag_recovery_control_catalog_event_forces_full_resync() {
        let event = control_catalog_full_resync_event();

        let Event::PluginBusEvent { kind, payload } = event else {
            panic!("expected plugin-bus control catalog event");
        };
        assert_eq!(kind, "bmux.control_catalog/control-catalog-events");
        assert_eq!(
            String::from_utf8(payload).expect("valid json"),
            r#"{"changed":{"full_resync":true,"revision":0,"scopes":["sessions","contexts","bindings"]}}"#,
        );
    }
}
