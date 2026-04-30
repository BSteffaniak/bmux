#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Server component for bmux terminal multiplexer.

use anyhow::{Context, Result};
use bmux_client_state::FollowStateHandle;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_context_state::ContextStateHandle;
use bmux_ipc::transport::{IpcTransportError, LocalIpcListener, LocalIpcStream};
use bmux_ipc::{
    AttachGrant, AttachViewComponent, CORE_PROTOCOL_CAPABILITIES, ContextSelector, Envelope,
    EnvelopeKind, ErrorCode, ErrorResponse, Event, IpcEndpoint, PerformanceRecordingLevel,
    ProtocolContract, RecordingEventKind, RecordingPayload, RecordingRollingStartOptions, Request,
    Response, ResponsePayload, ServerSnapshotStatus, ServicePipelinePayload,
    ServicePipelineRequest, ServicePipelineStepResult, decode, default_supported_capabilities,
    encode, negotiate_protocol,
};
use bmux_perf_telemetry::{
    PhaseChannel, PhasePayload, PhaseTimer, emit as emit_phase_timing, flush as flush_phase_timing,
};
use bmux_plugin_sdk::{WireEventSink, WireEventSinkError, WireEventSinkHandle};
use bmux_session_models::{ClientId, SessionId};
use bmux_session_state::{SessionManagerHandle, SessionManagerSnapshot};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use tracing::{debug, info, warn};
use uuid::Uuid;

use bmux_performance_state::{
    PerformanceEventRateLimiter, PerformanceSettingsHandle, PerformanceSettingsReader,
    PerformanceSettingsStore,
};
use bmux_recording_runtime::{RecordMeta, RecordingSinkHandle, RollingRecordingSettings};

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_TOKEN_TTL: Duration = Duration::from_secs(10);

/// Headroom reserved for envelope framing, layout metadata, pane summaries, and
/// scene data so that the combined output chunks + metadata never exceed the IPC
/// frame limit.  64 KiB is generous for any realistic layout.
const RESPONSE_METADATA_HEADROOM: usize = 65_536;
/// Maximum total bytes of pane output data the server will pack into a single
/// response (snapshot or output-batch).  Computed as `MAX_FRAME_PAYLOAD_SIZE`
/// minus generous metadata headroom, ensuring the serialized response always
/// fits within the frame limit regardless of what the client requests.
const RESPONSE_OUTPUT_BUDGET: usize =
    bmux_ipc::frame::MAX_FRAME_PAYLOAD_SIZE - RESPONSE_METADATA_HEADROOM;
const EVENT_PUSH_CHANNEL_CAPACITY: usize = 256;

/// Main server implementation.
#[derive(Clone)]
pub struct BmuxServer {
    endpoint: IpcEndpoint,
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
}

struct ServerState {
    attach_tokens: Arc<Mutex<AttachTokenManager>>,
    performance_settings: PerformanceSettingsStore,
    operation_lock: AsyncMutex<()>,
    event_hub: Mutex<EventHub>,
    /// Broadcast channel for pushing events to streaming clients.
    event_broadcast: tokio::sync::broadcast::Sender<Event>,
    client_principals: Arc<Mutex<BTreeMap<ClientId, Uuid>>>,
    server_control_principal_id: Uuid,
    handshake_timeout: Duration,
    service_registry: Mutex<ServiceRegistry>,
    service_resolver: Mutex<Option<Arc<ServiceResolverHandler>>>,
    plugin_state_replayers: Mutex<Vec<PluginStateReplayer>>,
}

type PluginStateReplayer = Arc<dyn Fn() -> Option<Event> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ServiceRoute {
    pub capability: String,
    pub kind: bmux_ipc::InvokeServiceKind,
    pub interface_id: String,
    pub operation: String,
}

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
}

impl ServiceRegistry {
    fn dispatch(
        &self,
        route: &ServiceRoute,
        context: ServiceInvokeContext,
        payload: Vec<u8>,
    ) -> Option<ServiceInvokeFuture> {
        if let Some(handler) = self.handlers.get(route).cloned() {
            return Some(handler(route.clone(), context, payload));
        }
        let mut wildcard = route.clone();
        wildcard.operation = "*".to_string();
        self.handlers
            .get(&wildcard)
            .cloned()
            .map(|handler| handler(route.clone(), context, payload))
    }
}

#[derive(Clone)]
pub struct ServiceInvokeContext {
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
    client_id: ClientId,
    client_principal_id: Uuid,
}

impl ServiceInvokeContext {
    /// Connected-client id on whose behalf this service invocation is
    /// dispatched. Plugin handlers use this to know which client made
    /// a request without a separate `WhoAmI` round-trip.
    #[must_use]
    pub const fn client_id(&self) -> ClientId {
        self.client_id
    }
}

impl ServiceInvokeContext {
    async fn execute_request(&self, request: Request) -> Result<Response> {
        // Selection state is owned by the clients-plugin's FollowState now;
        // plugins that need the caller's selected session should query it
        // through the FollowStateHandle rather than relying on
        // per-invocation locals. We pass an empty selection local here —
        // any handler that still reads it falls back to FollowState via
        // sync_selected_target_from_follow_state.
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

async fn invoke_service_output(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    client_id: ClientId,
    client_principal_id: Uuid,
    route: ServiceRoute,
    payload: Vec<u8>,
) -> Result<ServiceInvokeOutput> {
    let total_timer = PhaseTimer::start();
    let capability = route.capability.clone();
    let kind = route.kind;
    let interface_id = route.interface_id.clone();
    let operation = route.operation.clone();
    let invoke_context = ServiceInvokeContext {
        state: Arc::clone(state),
        shutdown_tx: shutdown_tx.clone(),
        client_id,
        client_principal_id,
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
                return bmux_codec::to_vec(&PipelineStructTemplate { map, field_order })
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
// the handle from the global plugin state registry.
fn follow_handle() -> FollowStateHandle {
    bmux_plugin::global_plugin_state_registry()
        .get::<FollowStateHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(FollowStateHandle::noop)
}

fn context_handle() -> ContextStateHandle {
    bmux_plugin::global_plugin_state_registry()
        .get::<ContextStateHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(ContextStateHandle::noop)
}

fn session_handle() -> SessionManagerHandle {
    bmux_plugin::global_plugin_state_registry()
        .get::<SessionManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(SessionManagerHandle::noop)
}

/// Look up the pane-runtime manager handle from the plugin state
/// registry. Returns the noop manager when the pane-runtime plugin
/// isn't loaded or its handle isn't registered yet.
fn session_runtime_handle() -> bmux_pane_runtime_state::SessionRuntimeManagerHandle {
    bmux_plugin::global_plugin_state_registry()
        .get::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(bmux_pane_runtime_state::SessionRuntimeManagerHandle::noop)
}

fn shutdown_runtime_info(info: bmux_pane_runtime_state::RemovedRuntimeInfo) {
    session_runtime_handle().0.shutdown_removed_runtime(info);
}

fn pane_output_event_from_push_read(
    session_id: Uuid,
    pane_id: Uuid,
    read: bmux_pane_runtime_state::OutputRead,
    sync_update_active: bool,
) -> Option<Event> {
    if read.bytes.is_empty() && !read.stream_gap {
        return None;
    }

    Some(Event::PaneOutput {
        session_id,
        pane_id,
        data: read.bytes,
        stream_start: read.stream_start,
        stream_end: read.stream_end,
        stream_gap: read.stream_gap,
        sync_update_active,
    })
}

/// Look up the snapshot orchestrator handle from the plugin state
/// registry. Returns the noop orchestrator when the snapshot plugin
/// isn't loaded or its handle isn't registered yet.
fn snapshot_orchestrator_handle() -> bmux_snapshot_runtime::SnapshotOrchestratorHandle {
    bmux_plugin::global_plugin_state_registry()
        .get::<bmux_snapshot_runtime::SnapshotOrchestratorHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
        .unwrap_or_else(bmux_snapshot_runtime::SnapshotOrchestratorHandle::noop)
}

/// Look up the shared snapshot dirty flag from the plugin state
/// registry. Returns a fresh detached flag when the snapshot plugin
/// isn't loaded or hasn't registered one yet — flipping it is a
/// harmless no-op in that case.
fn snapshot_dirty_flag() -> std::sync::Arc<bmux_snapshot_runtime::SnapshotDirtyFlag> {
    use bmux_snapshot_runtime::{SnapshotDirtyFlag, SnapshotDirtyFlagHandle};
    bmux_plugin::global_plugin_state_registry()
        .get::<SnapshotDirtyFlagHandle>()
        .and_then(|arc| arc.read().ok().map(|guard| std::sync::Arc::clone(&guard.0)))
        .unwrap_or_else(|| std::sync::Arc::new(SnapshotDirtyFlag::new()))
}

// `ContextState` / `RuntimeContext` are owned by the contexts plugin.
// The types live in
// `bmux_contexts_plugin::context_state`; the runtime handle is
// registered into `bmux_plugin::global_plugin_state_registry()` by
// `bmux_contexts_plugin::ContextsPlugin::activate`. Server code
// accesses the shared handle through `ServerState::context_state`,
// which is an `Arc<RwLock<ContextState>>` obtained from the plugin
// state registry.

fn current_context_id_for_session(state: &Arc<ServerState>, session_id: SessionId) -> Option<Uuid> {
    let _ = state;
    context_handle().0.context_for_session(session_id)
}

fn current_context_session_for_client(
    state: &Arc<ServerState>,
    client_id: ClientId,
) -> Option<SessionId> {
    let _ = state;
    context_handle().0.current_session_for_client(client_id)
}

/// Register a no-op `FollowStateHandle` in the plugin state registry
/// **only if** one hasn't already been registered. The clients plugin
/// typically registers its real adapter during `activate` (which
/// happens before server construction), so this fallback only fires
/// in tests or headless scenarios where the plugin is absent.
fn register_noop_follow_state_handle() {
    if bmux_plugin::global_plugin_state_registry()
        .get::<FollowStateHandle>()
        .is_some()
    {
        return;
    }
    let handle = Arc::new(std::sync::RwLock::new(FollowStateHandle::noop()));
    bmux_plugin::global_plugin_state_registry().register::<FollowStateHandle>(&handle);
}

/// Register a no-op `ContextStateHandle` in the plugin state registry
/// only if the contexts plugin hasn't already registered its adapter.
fn register_noop_context_state_handle() {
    if bmux_plugin::global_plugin_state_registry()
        .get::<ContextStateHandle>()
        .is_some()
    {
        return;
    }
    let handle = Arc::new(std::sync::RwLock::new(ContextStateHandle::noop()));
    bmux_plugin::global_plugin_state_registry().register::<ContextStateHandle>(&handle);
}

/// Register a no-op `SessionManagerHandle` in the plugin state registry
/// only if the sessions plugin hasn't already registered its adapter.
fn register_noop_session_manager_handle() {
    if bmux_plugin::global_plugin_state_registry()
        .get::<SessionManagerHandle>()
        .is_some()
    {
        return;
    }
    let handle = Arc::new(std::sync::RwLock::new(SessionManagerHandle::noop()));
    bmux_plugin::global_plugin_state_registry().register::<SessionManagerHandle>(&handle);
}

/// Register the performance plugin's settings handle into the global
/// plugin state registry. Server constructs the initial settings from
/// the static config and shares a `PerformanceSettingsStore` with the
/// plugin so the plugin's typed `SetSettings` handler mutates the
/// record server's event-push rate limiter reads.
///
/// Returns the `PerformanceSettingsStore` so `ServerState` can hold a
/// clone directly for hot-path reads.
fn register_performance_plugin_state(config: &BmuxConfig) -> PerformanceSettingsStore {
    let store = PerformanceSettingsStore::from_config(config);
    let handle = Arc::new(std::sync::RwLock::new(PerformanceSettingsHandle::from_arc(
        Arc::new(store.clone()),
    )));
    bmux_plugin::global_plugin_state_registry().register::<PerformanceSettingsHandle>(&handle);
    store
}

impl BmuxServer {
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
        let performance_settings = register_performance_plugin_state(&config);

        // Register a no-op FollowStateHandle so server's handle
        // lookups always succeed. The clients plugin overwrites this
        // during `activate`.
        register_noop_follow_state_handle();
        register_noop_context_state_handle();
        register_noop_session_manager_handle();

        let (event_broadcast_tx, _) =
            tokio::sync::broadcast::channel::<Event>(EVENT_PUSH_CHANNEL_CAPACITY);

        let attach_tokens = Arc::new(Mutex::new(AttachTokenManager::new(ATTACH_TOKEN_TTL)));
        let client_principals: Arc<Mutex<BTreeMap<ClientId, Uuid>>> =
            Arc::new(Mutex::new(BTreeMap::new()));

        // Register server's `AttachTokenManager` so the pane-runtime
        // plugin can issue/consume tokens during attach orchestration
        // without owning its own copy.
        let attach_token_handle = bmux_attach_token_state::AttachTokenManagerHandle::new(
            ServerAttachTokenAdapter::new(Arc::clone(&attach_tokens)),
        );
        bmux_plugin::global_plugin_state_registry()
            .register::<bmux_attach_token_state::AttachTokenManagerHandle>(&Arc::new(
            std::sync::RwLock::new(attach_token_handle),
        ));

        // Register server's per-client principal map so plugin
        // handlers can look up the principal for the caller client
        // when performing session-policy checks.
        let principal_handle = bmux_client_state::ClientPrincipalHandle::new(
            ServerClientPrincipalAdapter::new(Arc::clone(&client_principals)),
        );
        bmux_plugin::global_plugin_state_registry()
            .register::<bmux_client_state::ClientPrincipalHandle>(&Arc::new(
                std::sync::RwLock::new(principal_handle),
            ));

        Self {
            endpoint,
            state: Arc::new(ServerState {
                attach_tokens,
                performance_settings,
                operation_lock: AsyncMutex::new(()),
                event_hub: Mutex::new(EventHub::new(1024)),
                event_broadcast: event_broadcast_tx,
                client_principals,
                server_control_principal_id,
                handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
                service_registry: Mutex::new(ServiceRegistry::default()),
                service_resolver: Mutex::new(None),
                plugin_state_replayers: Mutex::new(Vec::new()),
            }),
            shutdown_tx,
        }
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
        let route = ServiceRoute {
            capability: capability.into(),
            kind,
            interface_id: interface_id.into(),
            operation: operation.into(),
        };
        let wrapped: Arc<ServiceInvokeHandler> =
            Arc::new(move |route, context, payload| Box::pin(handler(route, context, payload)));

        self.state
            .service_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("service registry lock poisoned"))?
            .handlers
            .insert(route, wrapped);
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

    /// Return the configured endpoint.
    #[must_use]
    pub const fn endpoint(&self) -> &IpcEndpoint {
        &self.endpoint
    }

    /// Request server shutdown.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
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
        let mut rx = bmux_plugin::global_event_bus()
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
        let (_initial, mut rx) = bmux_plugin::global_event_bus()
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
        if let Ok(mut replayers) = self.state.plugin_state_replayers.lock() {
            replayers.push(Arc::new(move || {
                let (current, _rx) = bmux_plugin::global_event_bus()
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

    /// Run the server accept loop until shutdown is requested.
    ///
    /// # Errors
    ///
    /// Returns an error if binding or accept-loop operations fail.
    pub async fn run(&self) -> Result<()> {
        self.run_impl(None).await
    }

    #[allow(clippy::too_many_lines)]
    async fn run_impl(
        &self,
        mut ready_tx: Option<oneshot::Sender<std::result::Result<(), String>>>,
    ) -> Result<()> {
        let listener = match LocalIpcListener::bind(&self.endpoint)
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

        // Snapshot restore runs after plugin activation so every participant
        // is registered before the orchestrator walks the envelope.

        // Rolling-recording auto-start and hourly prune loop both
        // moved into the recording plugin's `activate` callback;
        // server no longer constructs `RecordingRuntime` or schedules
        // recording-domain background tasks.

        info!("bmux server listening on {:?}", self.endpoint);
        emit_event(&self.state, Event::ServerStarted)?;
        if let Some(tx) = ready_tx.take() {
            let _ = tx.send(Ok(()));
        }

        // Register the wire-event sink so plugins can publish wire
        // events through the `WireEventSinkHandle` trait object in the
        // plugin state registry, without depending on `packages/server`.
        register_wire_event_sink(&self.state);

        // Now that every stateful participant is registered (clients
        // + contexts + sessions + pane-runtime via plugin activate
        // callbacks), ask the snapshot plugin
        // to restore the previous session's state if a snapshot file
        // exists on disk.
        let restore_handle = snapshot_orchestrator_handle();
        match restore_handle.as_dyn().restore_if_present_boxed().await {
            Ok(Some(summary)) => {
                info!(
                    "bmux snapshot restored: {} plugins ok, {} failed",
                    summary.restored_plugins, summary.failed_plugins
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!("bmux snapshot restore failed: {error}");
            }
        }

        // Snapshot debounce + flush loop is owned by the snapshot
        // plugin's activate (see `plugins/snapshot-plugin/src/lib.rs`).
        // Server no longer spawns its own tokio flush task.

        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let shutdown_reason = loop {
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
                accepted = listener.accept() => {
                    match accepted {
                        Ok(stream) => {
                            let state = Arc::clone(&self.state);
                            let shutdown_tx = self.shutdown_tx.clone();
                            tokio::spawn(async move {
                                if let Err(error) = handle_connection(state, shutdown_tx, stream).await {
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

        let _ = maybe_flush_snapshot(&self.state, true);

        let removed_runtimes = session_runtime_handle().0.remove_all_runtimes();
        for removed_runtime in removed_runtimes {
            if !removed_runtime.attached_clients.is_empty() {
                let _ = emit_event(
                    &self.state,
                    Event::ClientDetached {
                        id: removed_runtime.session_id.0,
                    },
                );
            }
            shutdown_runtime_info(removed_runtime);
        }
        session_handle()
            .0
            .restore_snapshot(SessionManagerSnapshot::default());
        let _ = emit_event(&self.state, Event::ServerStopping);
        if let Ok(mut attach_tokens) = self.state.attach_tokens.lock() {
            attach_tokens.clear();
        }

        info!("bmux server listener closed on {:?}", self.endpoint);
        flush_phase_timing();

        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_connection(
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
    mut stream: LocalIpcStream,
) -> Result<()> {
    let client_id = ClientId::new();
    let client_principal_id: Uuid;
    let mut selected_session: Option<SessionId> = None;
    let mut attached_stream_session: Option<SessionId> = None;
    let negotiated_frame_codec: Option<
        std::sync::Arc<dyn bmux_ipc::compression::CompressionCodec>,
    >;

    // ── Handshake (serial, before split) ─────────────────────────────────

    let first_envelope = tokio::time::timeout(state.handshake_timeout, stream.recv_envelope())
        .await
        .context("handshake timed out")??;

    let handshake = parse_request(&first_envelope)?;
    if let Request::HelloV2 {
        contract,
        client_name,
        principal_id,
    } = handshake
    {
        let server_contract = ProtocolContract::current(default_supported_capabilities());
        match negotiate_protocol(&contract, &server_contract, CORE_PROTOCOL_CAPABILITIES) {
            Ok(negotiated) => {
                client_principal_id = principal_id;
                debug!(
                    "accepted client handshake (v2): {client_name} revision={} caps={}",
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
            "first request must be hello_v2".to_string(),
        )
        .await?;
        return Ok(());
    }

    follow_handle().0.connect_client(client_id);
    {
        let mut principals = state
            .client_principals
            .lock()
            .map_err(|_| anyhow::anyhow!("client principal map lock poisoned"))?;
        principals.insert(client_id, client_principal_id);
    }

    // ── Split stream for concurrent read/write ───────────────────────────

    let (mut reader, mut writer) = stream.into_split();

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
    let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if writer.write_raw_frame(&frame).await.is_err() {
                return;
            }
        }
    });

    let mut event_push_task: Option<tokio::task::JoinHandle<()>> = None;

    // ── Request loop ─────────────────────────────────────────────────────

    loop {
        let (envelope, request_read_timing) = match reader.recv_envelope_with_timing().await {
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
                    negotiated_frame_codec.as_deref(),
                )?;
                continue;
            }
        };
        let request_decode_us = request_decode_started.elapsed().as_micros();

        // Track whether this request enables event push delivery.
        let is_enable_push = matches!(request, Request::EnableEventPush);

        let request_kind = request_kind_name(&request);
        let service_metadata = ipc_service_request_metadata(&request);
        let exclusive = request_requires_exclusive(&request);
        let request_record_encode_started = Instant::now();
        let request_data = bmux_codec::to_vec(&request).unwrap_or_else(|e| {
            tracing::warn!("failed to serialize request for recording: {e}");
            vec![]
        });
        let request_record_encode_us = request_record_encode_started.elapsed().as_micros();
        let started_at = Instant::now();
        debug!(
            client_id = %client_id.0,
            request_id = envelope.request_id,
            request = request_kind,
            exclusive,
            "server.request.start"
        );
        let request_record_started = Instant::now();
        record_to_all_runtimes(
            RecordingEventKind::RequestStart,
            RecordingPayload::RequestStart {
                request_id: envelope.request_id,
                request_kind: request_kind.to_string(),
                exclusive,
                request_data: request_data.clone(),
            },
            RecordMeta {
                session_id: selected_session.map(|id| id.0),
                pane_id: None,
                client_id: Some(client_id.0),
            },
        );
        let request_record_us = request_record_started.elapsed().as_micros();

        let handle_started = Instant::now();
        let response = handle_request(
            &state,
            &shutdown_tx,
            client_id,
            client_principal_id,
            &mut selected_session,
            request,
        )
        .await?;
        let handle_us = handle_started.elapsed().as_micros();
        let elapsed_ms = started_at.elapsed().as_millis();
        let (response_record_encode_us, response_record_us) = match &response {
            Response::Ok(payload) => {
                let response_record_encode_started = Instant::now();
                let response_data = bmux_codec::to_vec(payload).unwrap_or_else(|e| {
                    tracing::warn!("failed to serialize response for recording: {e}");
                    vec![]
                });
                let response_record_encode_us =
                    response_record_encode_started.elapsed().as_micros();
                debug!(
                    client_id = %client_id.0,
                    request_id = envelope.request_id,
                    request = request_kind,
                    response = response_payload_kind_name(payload),
                    elapsed_ms,
                    "server.request.done"
                );
                let response_record_started = Instant::now();
                record_to_all_runtimes(
                    RecordingEventKind::RequestDone,
                    RecordingPayload::RequestDone {
                        request_id: envelope.request_id,
                        request_kind: request_kind.to_string(),
                        response_kind: response_payload_kind_name(payload).to_string(),
                        #[allow(clippy::cast_possible_truncation)]
                        elapsed_ms: elapsed_ms.min(u128::from(u64::MAX)) as u64,
                        request_data,
                        response_data,
                    },
                    RecordMeta {
                        session_id: selected_session.map(|id| id.0),
                        pane_id: None,
                        client_id: Some(client_id.0),
                    },
                );
                (
                    response_record_encode_us,
                    response_record_started.elapsed().as_micros(),
                )
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
                let response_record_started = Instant::now();
                record_to_all_runtimes(
                    RecordingEventKind::RequestError,
                    RecordingPayload::RequestError {
                        request_id: envelope.request_id,
                        request_kind: request_kind.to_string(),
                        error_code: error.code,
                        message: error.message.clone(),
                        #[allow(clippy::cast_possible_truncation)]
                        elapsed_ms: elapsed_ms.min(u128::from(u64::MAX)) as u64,
                    },
                    RecordMeta {
                        session_id: selected_session.map(|id| id.0),
                        pane_id: None,
                        client_id: Some(client_id.0),
                    },
                );
                (0_u128, response_record_started.elapsed().as_micros())
            }
        };
        let response_send_started = Instant::now();
        let response_send_timing = match send_response_via_channel(
            &frame_tx,
            envelope.request_id,
            &response,
            negotiated_frame_codec.as_deref(),
        ) {
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
                    negotiated_frame_codec.as_deref(),
                )?;
                IpcResponseSendTiming::default()
            }
            Err(err) => return Err(err),
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
        // For `PaneOutputAvailable` events, the task asks the pane-runtime
        // handle to drain the client's cursor and sends the data inline as a
        // `PaneOutput` event. This eliminates the
        // round-trip `AttachPaneOutputBatch` request the client would
        // otherwise need, reducing output latency to a single one-way push.
        if is_enable_push && event_push_task.is_none() {
            replay_retained_plugin_state_to_client(
                &state,
                &frame_tx,
                negotiated_frame_codec.as_deref(),
            );
            let mut event_rx = state.event_broadcast.subscribe();
            let push_frame_tx = frame_tx.clone();
            let push_frame_codec = negotiated_frame_codec.clone();
            let push_state = Arc::clone(&state);
            let push_client_id = client_id;
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

                    match event_rx.recv().await {
                        Ok(event) => {
                            // For pane output notifications, read the actual
                            // data from the buffer and send it inline.
                            let event = match event {
                                Event::PaneOutputAvailable {
                                    session_id,
                                    pane_id,
                                } => {
                                    let Some((read, sync_update_active)) =
                                        session_runtime_handle().0.read_pane_output_for_push(
                                            SessionId(session_id),
                                            pane_id,
                                            push_client_id,
                                            RESPONSE_OUTPUT_BUDGET,
                                        )
                                    else {
                                        continue;
                                    };
                                    let Some(pane_output_event) = pane_output_event_from_push_read(
                                        session_id,
                                        pane_id,
                                        read,
                                        sync_update_active,
                                    ) else {
                                        continue;
                                    };
                                    pane_output_event
                                }
                                other => other,
                            };

                            let Some(frame) =
                                encode_event_frame(&event, push_frame_codec.as_deref())
                            else {
                                continue;
                            };
                            let frame_len = frame.len();
                            if push_frame_tx.send(frame).is_err() {
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

                                    if let Some(encoded_payload) =
                                        push_perf_rate_limiter.encode_payload(payload)
                                    {
                                        record_to_all_runtimes(
                                            RecordingEventKind::Custom,
                                            RecordingPayload::Custom {
                                                source: bmux_ipc::PERF_RECORDING_SOURCE.to_string(),
                                                name: "server.push.window".to_string(),
                                                payload: encoded_payload,
                                            },
                                            RecordMeta {
                                                session_id: None,
                                                pane_id: None,
                                                client_id: Some(push_client_id.0),
                                            },
                                        );
                                    }
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
                                if push_frame_tx.send(frame).is_err() {
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
        }
    }

    // Abort the event push task if running.
    if let Some(task) = event_push_task {
        task.abort();
    }
    // Drop the frame sender so the writer task exits.
    drop(frame_tx);
    let _ = writer_task.await;

    detach_client_state_on_disconnect(
        &state,
        client_id,
        &mut selected_session,
        &mut attached_stream_session,
    )?;
    disconnect_follow_state(&state, client_id)?;
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

    Ok(())
}

fn emit_event(state: &Arc<ServerState>, event: Event) -> Result<()> {
    let session_id = match &event {
        Event::SessionCreated { id, .. }
        | Event::SessionRemoved { id }
        | Event::ClientAttached { id }
        | Event::ClientDetached { id } => Some(*id),
        Event::FollowTargetChanged { session_id, .. }
        | Event::AttachViewChanged { session_id, .. }
        | Event::PaneOutputAvailable { session_id, .. }
        | Event::PaneOutput { session_id, .. }
        | Event::PaneImageAvailable { session_id, .. }
        | Event::PaneExited { session_id, .. }
        | Event::PaneRestarted { session_id, .. } => Some(*session_id),
        Event::ServerStarted
        | Event::ServerStopping
        | Event::FollowStarted { .. }
        | Event::FollowStopped { .. }
        | Event::FollowTargetGone { .. }
        | Event::RecordingStarted { .. }
        | Event::RecordingStopped { .. }
        | Event::PluginBusEvent { .. } => None,
    };
    record_to_all_runtimes(
        RecordingEventKind::ServerEvent,
        RecordingPayload::ServerEvent {
            event: event.clone(),
        },
        RecordMeta {
            session_id,
            pane_id: None,
            client_id: None,
        },
    );
    // Broadcast to streaming clients (ignore errors — no receivers is fine).
    let _ = state.event_broadcast.send(event.clone());
    state
        .event_hub
        .lock()
        .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?
        .emit(event);
    Ok(())
}

fn replay_retained_plugin_state_to_client(
    state: &Arc<ServerState>,
    frame_tx: &mpsc::UnboundedSender<Vec<u8>>,
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
        if frame_tx.send(frame).is_err() {
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
    fn publish(&self, event: Event) -> std::result::Result<(), WireEventSinkError> {
        emit_event(&self.state, event)
            .map_err(|error| WireEventSinkError::PublishFailed(error.to_string()))
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
    bmux_plugin::global_plugin_state_registry().register::<WireEventSinkHandle>(&sink);
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
    let _ = state;
    let revisions = session_runtime_handle()
        .0
        .lag_recovery_bump_attach_view_for_client(client_id);

    if revisions.is_empty() {
        return Vec::new();
    }

    let mut events: Vec<Event> = revisions
        .into_iter()
        .map(|(session_id, revision)| Event::AttachViewChanged {
            context_id: current_context_id_for_session(state, session_id),
            session_id: session_id.0,
            revision,
            components: vec![
                AttachViewComponent::Scene,
                AttachViewComponent::SurfaceContent,
                AttachViewComponent::Layout,
                AttachViewComponent::Status,
            ],
        })
        .collect();

    // Force attach clients to poll the plugin-owned catalog after a
    // dropped event window. The revision is intentionally neutral here:
    // clients treat `full_resync` as authoritative and fetch the typed
    // snapshot from the control-catalog plugin.
    events.push(control_catalog_full_resync_event());

    events
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
    let follow_selected = follow_handle().0.selected_target(client_id);

    if let Some((follow_selected_context, follow_selected_session)) = follow_selected {
        if let Some(context_id) = follow_selected_context {
            let _ = context_handle()
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
    _state: &Arc<ServerState>,
    client_id: ClientId,
    previous: Option<SessionId>,
    next: Option<SessionId>,
) -> Result<()> {
    if previous == next {
        return Ok(());
    }

    let manager = session_handle();

    if let Some(previous_session) = previous {
        manager.0.remove_client(previous_session, &client_id);
    }

    if let Some(next_session) = next {
        manager.0.add_client(next_session, client_id);
    }

    Ok(())
}

fn disconnect_follow_state(state: &Arc<ServerState>, client_id: ClientId) -> Result<()> {
    let events = follow_handle().0.disconnect_client(client_id);

    for event in events {
        emit_event(state, event)?;
    }

    Ok(())
}

fn mark_snapshot_dirty(state: &Arc<ServerState>) {
    let _ = state;
    snapshot_dirty_flag().mark_dirty();
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
    let handle = snapshot_orchestrator_handle();
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        if let Err(error) = tokio::task::block_in_place(|| {
            rt.block_on(async move { handle.as_dyn().save_now_boxed().await })
        }) {
            warn!("forced snapshot flush failed: {error}");
        }
    } else {
        // Headless / test path without a tokio runtime: just flip
        // the dirty flag; nothing else can drive the async path.
        snapshot_dirty_flag().mark_dirty();
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
        Request::Hello { .. } | Request::HelloV2 { .. } => Response::Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: "hello request is only valid during handshake".to_string(),
        }),
        Request::Ping => Response::Ok(ResponsePayload::Pong),
        Request::WhoAmIPrincipal => Response::Ok(ResponsePayload::PrincipalIdentity {
            principal_id: client_principal_id,
            server_control_principal_id: state.server_control_principal_id,
            force_local_permitted: client_principal_id == state.server_control_principal_id,
        }),
        Request::ServerStatus => {
            let report = snapshot_orchestrator_handle().as_dyn().status();
            let snapshot = ServerSnapshotStatus {
                enabled: report.enabled,
                path: report.path,
                snapshot_exists: report.snapshot_exists,
                last_write_epoch_ms: report.last_write_epoch_ms,
                last_restore_epoch_ms: report.last_restore_epoch_ms,
                last_restore_error: report.last_restore_error,
            };
            Response::Ok(ResponsePayload::ServerStatus {
                running: true,
                snapshot,
                principal_id: client_principal_id,
                server_control_principal_id: state.server_control_principal_id,
            })
        }
        Request::ServerSave => {
            let handle = snapshot_orchestrator_handle();
            let path = match handle.as_dyn().save_now_boxed().await {
                Ok(path) => path.map(|p| p.display().to_string()),
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: format!("snapshot save failed: {error}"),
                    }));
                }
            };
            Response::Ok(ResponsePayload::ServerSnapshotSaved { path })
        }
        Request::ServerRestoreDryRun => {
            let handle = snapshot_orchestrator_handle();
            match handle.as_dyn().dry_run_boxed().await {
                Ok(report) => Response::Ok(ResponsePayload::ServerSnapshotRestoreDryRun {
                    ok: report.ok,
                    message: report.message,
                }),
                Err(bmux_snapshot_runtime::SnapshotOrchestratorError::Disabled) => {
                    Response::Ok(ResponsePayload::ServerSnapshotRestoreDryRun {
                        ok: false,
                        message: "snapshot persistence is disabled".to_string(),
                    })
                }
                Err(error) => Response::Ok(ResponsePayload::ServerSnapshotRestoreDryRun {
                    ok: false,
                    message: format!("snapshot dry-run failed: {error}"),
                }),
            }
        }
        Request::ServerRestoreApply => {
            let handle = snapshot_orchestrator_handle();
            let summary = match handle.as_dyn().restore_apply_boxed().await {
                Ok(summary) => summary,
                Err(bmux_snapshot_runtime::SnapshotOrchestratorError::Disabled) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: "snapshot persistence is disabled".to_string(),
                    }));
                }
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: format!("snapshot restore failed: {error}"),
                    }));
                }
            };
            // Wire response retains the legacy `sessions`/`follows`/
            // `selected_sessions` triple. The new orchestrator only
            // knows about participant counts, so we expose
            // `restored_plugins` as `sessions` and zero out the rest.
            // Callers treat these counts as human-readable diagnostics.
            Response::Ok(ResponsePayload::ServerSnapshotRestored {
                sessions: summary.restored_plugins,
                follows: summary.failed_plugins,
                selected_sessions: 0,
            })
        }
        Request::ServerStop => {
            let _ = shutdown_tx.send(true);
            flush_phase_timing();
            Response::Ok(ResponsePayload::ServerStopping)
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
            let _span_guard = span.enter();
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
            let invoke_context = ServiceInvokeContext {
                state: Arc::clone(state),
                shutdown_tx: shutdown_tx.clone(),
                client_id,
                client_principal_id,
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
                resolver.map(|resolver| resolver(route.clone(), payload))
            };
            let resolver_us = resolver_started.elapsed().as_micros();

            let invocation_started = std::time::Instant::now();
            let response = if let Some(invocation) = invocation {
                match invocation.await {
                    Ok(output) => Response::Ok(ResponsePayload::ServiceInvoked {
                        payload: output.payload,
                    }),
                    Err(error) => Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
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
            response
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
                    code: ErrorCode::Internal,
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
            let event_kind =
                bmux_plugin_sdk::PluginEventKind::from_static(Box::leak(kind.into_boxed_str()));
            match bmux_plugin::global_event_bus().emit_from_bytes(&event_kind, &payload) {
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
        // EnableEventPush is handled in handle_connection after the response
        // is sent — the actual push task spawning happens there. Here we just
        // acknowledge the request.
        Request::EnableEventPush => Response::Ok(ResponsePayload::EventPushEnabled),
    };

    // Catalog-changed events are fired by the control-catalog plugin
    // in response to session/context/client events on the event bus.
    // Server no longer emits catalog-changed directly. Attach-side
    // `ClientAttached` events are emitted by the pane-runtime plugin's
    // attach-open handler.

    if response_requires_snapshot(&response) {
        mark_snapshot_dirty(state);
        maybe_flush_snapshot(state, false)?;
    }

    Ok(response)
}

const fn request_requires_exclusive(request: &Request) -> bool {
    matches!(
        request,
        Request::ServerSave | Request::ServerStop | Request::ServerRestoreApply
    )
}

const fn response_requires_snapshot(_response: &Response) -> bool {
    false
}

const fn request_kind_name(request: &Request) -> &'static str {
    match request {
        Request::Hello { .. } => "hello",
        Request::HelloV2 { .. } => "hello_v2",
        Request::Ping => "ping",
        Request::WhoAmIPrincipal => "whoami_principal",
        Request::ServerStatus => "server_status",
        Request::ServerSave => "server_save",
        Request::ServerRestoreDryRun => "server_restore_dry_run",
        Request::ServerRestoreApply => "server_restore_apply",
        Request::ServerStop => "server_stop",
        Request::InvokeService { .. } => "invoke_service",
        Request::InvokeServicePipeline { .. } => "invoke_service_pipeline",
        Request::EmitOnPluginBus { .. } => "emit_on_plugin_bus",
        Request::PollEvents { .. } => "poll_events",
        Request::EnableEventPush => "enable_event_push",
        Request::SubscribeEvents => "subscribe_events",
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
        ResponsePayload::HelloNegotiated { .. } => "hello_negotiated",
        ResponsePayload::HelloIncompatible { .. } => "hello_incompatible",
        ResponsePayload::ServerStatus { .. } => "server_status",
        ResponsePayload::ServerSnapshotSaved { .. } => "server_snapshot_saved",
        ResponsePayload::ServerSnapshotRestoreDryRun { .. } => "server_snapshot_restore_dry_run",
        ResponsePayload::ServerSnapshotRestored { .. } => "server_snapshot_restored",
        ResponsePayload::ServerStopping => "server_stopping",
        ResponsePayload::ServiceInvoked { .. } => "service_invoked",
        ResponsePayload::ServicePipelineInvoked { .. } => "service_pipeline_invoked",
        ResponsePayload::EventsSubscribed => "events_subscribed",
        ResponsePayload::EventBatch { .. } => "event_batch",
        ResponsePayload::EventPushEnabled => "event_push_enabled",
        ResponsePayload::PluginBusEmitted { .. } => "plugin_bus_emitted",
    }
}

fn detach_client_state_on_disconnect(
    state: &Arc<ServerState>,
    client_id: ClientId,
    selected_session: &mut Option<SessionId>,
    attached_stream_session: &mut Option<SessionId>,
) -> Result<()> {
    let previous_selected = selected_session.take();
    let previous_stream = attached_stream_session.take();

    context_handle().0.disconnect_client(client_id);

    if previous_selected.is_none() && previous_stream.is_none() {
        return Ok(());
    }

    if let Some(session_id) = previous_selected {
        session_handle().0.remove_client(session_id, &client_id);
    }

    if let Some(stream_session_id) = previous_stream {
        session_runtime_handle()
            .0
            .end_attach(stream_session_id, client_id);

        emit_event(
            state,
            Event::ClientDetached {
                id: stream_session_id.0,
            },
        )?;
    }

    Ok(())
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
    stream: &mut LocalIpcStream,
    request_id: u64,
    payload: ResponsePayload,
) -> Result<()> {
    let response = Response::Ok(payload);
    send_response(stream, request_id, response).await
}

async fn send_error(
    stream: &mut LocalIpcStream,
    request_id: u64,
    code: ErrorCode,
    message: String,
) -> Result<()> {
    let response = Response::Err(ErrorResponse { code, message });
    send_response(stream, request_id, response).await
}

async fn send_response(
    stream: &mut LocalIpcStream,
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

fn send_error_via_channel(
    frame_tx: &mpsc::UnboundedSender<Vec<u8>>,
    request_id: u64,
    code: ErrorCode,
    message: String,
    frame_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
) -> Result<()> {
    let response = Response::Err(ErrorResponse { code, message });
    send_response_via_channel(frame_tx, request_id, &response, frame_codec).map(|_| ())
}

#[derive(Debug, Clone, Copy, Default)]
struct IpcResponseSendTiming {
    response_encode: u128,
    frame_encode: u128,
    writer_queue: u128,
}

fn send_response_via_channel(
    frame_tx: &mpsc::UnboundedSender<Vec<u8>>,
    request_id: u64,
    response: &Response,
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
    frame_tx
        .send(frame)
        .map_err(|_| anyhow::anyhow!("writer channel closed"))?;
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
        compression::resolve_codec(compression::CompressionId::Lz4).map(std::sync::Arc::from)
    } else if capabilities
        .iter()
        .any(|c| c == bmux_ipc::CAPABILITY_COMPRESSION_FRAME_ZSTD)
    {
        compression::resolve_codec(compression::CompressionId::Zstd).map(std::sync::Arc::from)
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

/// Fan out a recording event to both the manual and rolling recording
/// runtimes by looking up the registered `RecordingSinkHandle` in the
/// plugin state registry. Server code no longer holds `RecordingRuntime`
/// references directly; the recording plugin installs the sink at
/// server construction, and every hot-path write goes through the
/// registered trait object.
fn record_to_all_runtimes(kind: RecordingEventKind, payload: RecordingPayload, meta: RecordMeta) {
    let Some(handle) = bmux_plugin::global_plugin_state_registry().get::<RecordingSinkHandle>()
    else {
        return;
    };
    let Ok(guard) = handle.read() else {
        return;
    };
    guard.0.record(kind, payload, meta);
}

// ── `ServerSessionRuntimeAdapter` ────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::significant_drop_tightening,
    reason = "Test scaffolding holds guards only for short critical sections; tightening adds churn without runtime benefit."
)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct PipelineTemplateTarget {
        context_id: String,
        can_write: bool,
        cols: u16,
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
        let decoded: PipelineTemplateTarget =
            bmux_codec::from_bytes(&encoded).expect("payload should decode as target struct");

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
