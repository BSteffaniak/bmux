#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Server component for bmux terminal multiplexer.

mod persistence;
pub mod recording;

use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_ipc::transport::{IpcTransportError, LocalIpcListener, LocalIpcStream};
use bmux_ipc::{
    AttachFocusTarget, AttachGrant, AttachLayer, AttachPaneChunk, AttachRect, AttachScene,
    AttachSurface, AttachSurfaceKind, AttachViewComponent, CURRENT_PROTOCOL_VERSION, ClientSummary,
    ContextSelector, ContextSummary, Envelope, EnvelopeKind, ErrorCode, ErrorResponse, Event,
    IpcEndpoint, PaneFocusDirection, PaneLayoutNode as IpcPaneLayoutNode, PaneSelector,
    PaneSplitDirection, PaneSummary, ProtocolVersion, RecordingEventKind, RecordingPayload,
    RecordingProfile, Request, Response, ResponsePayload, ServerSnapshotStatus, SessionSelector,
    SessionSummary, decode, encode,
};
use bmux_session::{ClientId, Session, SessionId, SessionManager};
use bmux_terminal_protocol::{ProtocolProfile, TerminalProtocolEngine, protocol_profile_for_term};
use persistence::{
    ClientSelectedContextSnapshotV1, ClientSelectedSessionSnapshotV2,
    ContextSessionBindingSnapshotV1, ContextSnapshotV1, FloatingSurfaceSnapshotV3,
    FollowEdgeSnapshotV2, PaneLayoutNodeSnapshotV2, PaneSnapshotV2, PaneSplitDirectionSnapshotV2,
    SessionSnapshotV3, SnapshotManager, SnapshotV4,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::recording::{RecordMeta, RecordingRuntime};

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_TOKEN_TTL: Duration = Duration::from_secs(10);
const CONTEXT_SESSION_ID_ATTRIBUTE: &str = "bmux.session_id";
const MAX_WINDOW_OUTPUT_BUFFER_BYTES: usize = 1_048_576;
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
const SNAPSHOT_DEBOUNCE_INTERVAL: Duration = Duration::from_millis(300);
const OFFLINE_SNAPSHOT_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const OFFLINE_SNAPSHOT_LOCK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineSessionKillTarget {
    One(SessionSelector),
    All,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfflineSessionKillReport {
    pub had_snapshot: bool,
    pub removed_session_ids: Vec<Uuid>,
    pub removed_context_ids: Vec<Uuid>,
}

/// Main server implementation.
#[derive(Clone)]
pub struct BmuxServer {
    endpoint: IpcEndpoint,
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
}

struct ServerState {
    session_manager: Mutex<SessionManager>,
    session_runtimes: Mutex<SessionRuntimeManager>,
    attach_tokens: Mutex<AttachTokenManager>,
    follow_state: Mutex<FollowState>,
    context_state: Mutex<ContextState>,
    snapshot_runtime: Mutex<SnapshotRuntime>,
    recording_runtime: Arc<Mutex<RecordingRuntime>>,
    operation_lock: AsyncMutex<()>,
    event_hub: Mutex<EventHub>,
    /// Broadcast channel for pushing events to streaming clients.
    event_broadcast: tokio::sync::broadcast::Sender<Event>,
    client_principals: Mutex<BTreeMap<ClientId, Uuid>>,
    server_control_principal_id: Uuid,
    handshake_timeout: Duration,
    pane_exit_rx: AsyncMutex<mpsc::UnboundedReceiver<PaneExitEvent>>,
    service_registry: Mutex<ServiceRegistry>,
    service_resolver: Mutex<Option<Arc<ServiceResolverHandler>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ServiceRoute {
    pub capability: String,
    pub kind: bmux_ipc::InvokeServiceKind,
    pub interface_id: String,
    pub operation: String,
}

type ServiceInvokeFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send>>;
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
    selection: Arc<AsyncMutex<(Option<SessionId>, Option<SessionId>)>>,
}

impl ServiceInvokeContext {
    async fn execute_request(&self, request: Request) -> Result<Response> {
        let mut selection = self.selection.lock().await;
        let mut selected_session = selection.0;
        let mut attached_stream_session = selection.1;
        let response = handle_request(
            &self.state,
            &self.shutdown_tx,
            self.client_id,
            self.client_principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            request,
        )
        .await?;
        selection.0 = selected_session;
        selection.1 = attached_stream_session;
        Ok(response)
    }

    pub async fn execute_raw(&self, request_payload: Vec<u8>) -> Result<Vec<u8>> {
        let request: Request =
            decode(&request_payload).context("failed decoding kernel bridge request payload")?;
        let response = self.execute_request(request).await?;
        encode(&response).context("failed encoding kernel bridge response payload")
    }
}

#[derive(Debug, Clone, Copy)]
struct PaneExitEvent {
    session_id: SessionId,
    pane_id: Uuid,
}

#[derive(Debug)]
struct SnapshotRuntime {
    manager: Option<SnapshotManager>,
    dirty: bool,
    last_marked_at: Option<Instant>,
    debounce_interval: Duration,
    last_write_epoch_ms: Option<u64>,
    last_restore_epoch_ms: Option<u64>,
    last_restore_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct RestoreSummary {
    sessions: usize,
    follows: usize,
    selected_sessions: usize,
}

impl SnapshotRuntime {
    const fn disabled() -> Self {
        Self {
            manager: None,
            dirty: false,
            last_marked_at: None,
            debounce_interval: SNAPSHOT_DEBOUNCE_INTERVAL,
            last_write_epoch_ms: None,
            last_restore_epoch_ms: None,
            last_restore_error: None,
        }
    }

    const fn with_manager(manager: SnapshotManager) -> Self {
        Self {
            manager: Some(manager),
            dirty: false,
            last_marked_at: None,
            debounce_interval: SNAPSHOT_DEBOUNCE_INTERVAL,
            last_write_epoch_ms: None,
            last_restore_epoch_ms: None,
            last_restore_error: None,
        }
    }
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
        let start = *cursor;
        let count = max_events.max(1);
        let events = self
            .events
            .iter()
            .skip(start)
            .take(count)
            .map(|record| record.event.clone())
            .collect::<Vec<_>>();
        *cursor = start + events.len();
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

fn epoch_millis_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis() as u64
}

#[derive(Debug, Clone, Copy)]
struct FollowEntry {
    leader_client_id: ClientId,
    global: bool,
}

#[derive(Debug, Clone, Copy)]
struct FollowTargetUpdate {
    follower_client_id: ClientId,
    leader_client_id: ClientId,
    context_id: Option<Uuid>,
    session_id: Option<SessionId>,
}

#[derive(Debug, Default)]
struct FollowState {
    connected_clients: std::collections::BTreeSet<ClientId>,
    selected_contexts: BTreeMap<ClientId, Option<Uuid>>,
    selected_sessions: BTreeMap<ClientId, Option<SessionId>>,
    follows: BTreeMap<ClientId, FollowEntry>,
}

impl FollowState {
    fn connect_client(&mut self, client_id: ClientId) {
        self.connected_clients.insert(client_id);
        self.selected_contexts.entry(client_id).or_insert(None);
        self.selected_sessions.entry(client_id).or_insert(None);
    }

    fn disconnect_client(&mut self, client_id: ClientId) -> Vec<Event> {
        self.connected_clients.remove(&client_id);
        self.selected_contexts.remove(&client_id);
        self.selected_sessions.remove(&client_id);
        self.follows.remove(&client_id);

        let impacted_followers = self
            .follows
            .iter()
            .filter_map(|(follower_id, entry)| {
                (entry.leader_client_id == client_id).then_some(*follower_id)
            })
            .collect::<Vec<_>>();

        impacted_followers
            .into_iter()
            .filter_map(|follower_id| {
                self.follows
                    .remove(&follower_id)
                    .map(|entry| Event::FollowTargetGone {
                        follower_client_id: follower_id.0,
                        former_leader_client_id: entry.leader_client_id.0,
                    })
            })
            .collect()
    }

    fn set_selected_target(
        &mut self,
        client_id: ClientId,
        context_id: Option<Uuid>,
        session_id: Option<SessionId>,
    ) {
        if self.connected_clients.contains(&client_id) {
            self.selected_contexts.insert(client_id, context_id);
            self.selected_sessions.insert(client_id, session_id);
        }
    }

    fn selected_target(&self, client_id: ClientId) -> Option<(Option<Uuid>, Option<SessionId>)> {
        Some((
            self.selected_contexts.get(&client_id).copied()?,
            self.selected_sessions.get(&client_id).copied()?,
        ))
    }

    fn start_follow(
        &mut self,
        follower_client_id: ClientId,
        leader_client_id: ClientId,
        global: bool,
    ) -> std::result::Result<(Option<Uuid>, Option<SessionId>), &'static str> {
        if follower_client_id == leader_client_id {
            return Err("cannot follow self");
        }
        if !self.connected_clients.contains(&leader_client_id) {
            return Err("target client not connected");
        }
        if !self.connected_clients.contains(&follower_client_id) {
            return Err("follower client not connected");
        }

        self.follows.insert(
            follower_client_id,
            FollowEntry {
                leader_client_id,
                global,
            },
        );

        if global {
            let leader_context = self
                .selected_contexts
                .get(&leader_client_id)
                .copied()
                .flatten();
            let leader_session = self
                .selected_sessions
                .get(&leader_client_id)
                .copied()
                .flatten();
            self.selected_contexts
                .insert(follower_client_id, leader_context);
            self.selected_sessions
                .insert(follower_client_id, leader_session);
            return Ok((leader_context, leader_session));
        }

        Ok((None, None))
    }

    fn stop_follow(&mut self, follower_client_id: ClientId) -> bool {
        self.follows.remove(&follower_client_id).is_some()
    }

    fn sync_followers_from_leader(
        &mut self,
        leader_client_id: ClientId,
        selected_context: Option<Uuid>,
        selected_session: Option<SessionId>,
    ) -> Vec<FollowTargetUpdate> {
        let followers = self
            .follows
            .iter()
            .filter_map(|(follower_id, entry)| {
                (entry.leader_client_id == leader_client_id && entry.global).then_some(*follower_id)
            })
            .collect::<Vec<_>>();

        let mut updates = Vec::new();
        for follower_id in followers {
            if self.connected_clients.contains(&follower_id) {
                let previous = self.selected_sessions.get(&follower_id).copied().flatten();
                let previous_context = self.selected_contexts.get(&follower_id).copied().flatten();
                self.selected_contexts.insert(follower_id, selected_context);
                self.selected_sessions.insert(follower_id, selected_session);
                let changed = previous != selected_session || previous_context != selected_context;
                if changed {
                    updates.push(FollowTargetUpdate {
                        follower_client_id: follower_id,
                        leader_client_id,
                        context_id: selected_context,
                        session_id: selected_session,
                    });
                }
            }
        }

        updates
    }

    fn list_clients(&self) -> Vec<ClientSummary> {
        self.connected_clients
            .iter()
            .map(|client_id| {
                let selected_session_id = self
                    .selected_sessions
                    .get(client_id)
                    .and_then(|selected| selected.map(|session_id| session_id.0));
                let selected_context_id = self.selected_contexts.get(client_id).copied().flatten();
                let (following_client_id, following_global) =
                    self.follows.get(client_id).map_or((None, false), |entry| {
                        (Some(entry.leader_client_id.0), entry.global)
                    });

                ClientSummary {
                    id: client_id.0,
                    selected_context_id,
                    selected_session_id,
                    following_client_id,
                    following_global,
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeContext {
    id: Uuid,
    name: Option<String>,
    attributes: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct ContextState {
    contexts: BTreeMap<Uuid, RuntimeContext>,
    session_by_context: BTreeMap<Uuid, SessionId>,
    selected_by_client: BTreeMap<ClientId, Uuid>,
    mru_contexts: VecDeque<Uuid>,
}

impl ContextState {
    fn list(&self) -> Vec<ContextSummary> {
        let mut ordered_ids = self.mru_contexts.iter().copied().collect::<Vec<_>>();
        for id in self.contexts.keys().copied() {
            if !ordered_ids.contains(&id) {
                ordered_ids.push(id);
            }
        }

        ordered_ids
            .into_iter()
            .filter_map(|id| self.contexts.get(&id))
            .map(Self::to_summary)
            .collect()
    }

    fn create(
        &mut self,
        client_id: ClientId,
        name: Option<String>,
        attributes: BTreeMap<String, String>,
    ) -> ContextSummary {
        let context = RuntimeContext {
            id: Uuid::new_v4(),
            name,
            attributes,
        };
        let id = context.id;
        self.contexts.insert(id, context.clone());
        self.selected_by_client.insert(client_id, id);
        self.touch_mru(id);
        Self::to_summary(&context)
    }

    fn current_for_client(&self, client_id: ClientId) -> Option<ContextSummary> {
        let selected = self
            .selected_by_client
            .get(&client_id)
            .copied()
            .filter(|id| self.contexts.contains_key(id))
            .or_else(|| {
                self.mru_contexts
                    .iter()
                    .copied()
                    .find(|id| self.contexts.contains_key(id))
            })?;
        self.contexts.get(&selected).map(Self::to_summary)
    }

    fn current_session_for_client(&self, client_id: ClientId) -> Option<SessionId> {
        let selected = self
            .selected_by_client
            .get(&client_id)
            .copied()
            .filter(|id| self.contexts.contains_key(id))
            .or_else(|| {
                self.mru_contexts
                    .iter()
                    .copied()
                    .find(|id| self.contexts.contains_key(id))
            })?;
        self.session_by_context.get(&selected).copied()
    }

    fn context_for_session(&self, session_id: SessionId) -> Option<Uuid> {
        self.session_by_context
            .iter()
            .find_map(|(context_id, mapped_session_id)| {
                (*mapped_session_id == session_id).then_some(*context_id)
            })
    }

    fn select_for_client(
        &mut self,
        client_id: ClientId,
        selector: &ContextSelector,
    ) -> std::result::Result<ContextSummary, &'static str> {
        let id = self.resolve_id(selector)?;
        self.selected_by_client.insert(client_id, id);
        self.touch_mru(id);
        self.contexts
            .get(&id)
            .map(Self::to_summary)
            .ok_or("context not found")
    }

    fn close(
        &mut self,
        client_id: ClientId,
        selector: &ContextSelector,
        _force: bool,
    ) -> std::result::Result<(Uuid, Option<SessionId>), &'static str> {
        let id = self.resolve_id(selector)?;
        self.remove_context_by_id(id, Some(client_id))
            .ok_or("context not found")
    }

    fn remove_contexts_for_session(&mut self, session_id: SessionId) -> Vec<Uuid> {
        let context_ids = self
            .session_by_context
            .iter()
            .filter_map(|(context_id, mapped)| (*mapped == session_id).then_some(*context_id))
            .collect::<Vec<_>>();
        let mut removed = Vec::with_capacity(context_ids.len());
        for context_id in context_ids {
            if let Some((removed_id, _)) = self.remove_context_by_id(context_id, None) {
                removed.push(removed_id);
            }
        }
        removed
    }

    fn bind_session(
        &mut self,
        context_id: Uuid,
        session_id: SessionId,
    ) -> std::result::Result<(), &'static str> {
        let Some(context) = self.contexts.get_mut(&context_id) else {
            return Err("context not found");
        };
        context.attributes.insert(
            CONTEXT_SESSION_ID_ATTRIBUTE.to_string(),
            session_id.0.to_string(),
        );
        self.session_by_context.insert(context_id, session_id);
        Ok(())
    }

    fn disconnect_client(&mut self, client_id: ClientId) {
        self.selected_by_client.remove(&client_id);
    }

    fn resolve_id(&self, selector: &ContextSelector) -> std::result::Result<Uuid, &'static str> {
        match selector {
            ContextSelector::ById(id) => {
                if self.contexts.contains_key(id) {
                    Ok(*id)
                } else {
                    Err("context not found")
                }
            }
            ContextSelector::ByName(name) => {
                let mut matches = self
                    .contexts
                    .values()
                    .filter(|context| context.name.as_deref() == Some(name.as_str()))
                    .map(|context| context.id);
                let Some(first) = matches.next() else {
                    return Err("context not found");
                };
                if matches.next().is_some() {
                    return Err("context selector by name is ambiguous");
                }
                Ok(first)
            }
        }
    }

    fn touch_mru(&mut self, id: Uuid) {
        self.mru_contexts.retain(|entry| *entry != id);
        self.mru_contexts.push_front(id);
    }

    fn remove_context_by_id(
        &mut self,
        context_id: Uuid,
        preferred_client: Option<ClientId>,
    ) -> Option<(Uuid, Option<SessionId>)> {
        let removed = self.contexts.remove(&context_id)?;
        let removed_session = self.session_by_context.remove(&context_id);
        self.mru_contexts.retain(|entry| *entry != context_id);

        let replacement = self
            .mru_contexts
            .iter()
            .copied()
            .find(|candidate| self.contexts.contains_key(candidate));

        let impacted = self
            .selected_by_client
            .iter()
            .filter_map(|(id_key, selected)| (*selected == removed.id).then_some(*id_key))
            .collect::<Vec<_>>();
        for impacted_client in impacted {
            if let Some(next_id) = replacement {
                self.selected_by_client.insert(impacted_client, next_id);
            } else {
                self.selected_by_client.remove(&impacted_client);
            }
        }

        if let Some(client_id) = preferred_client
            && self.selected_by_client.get(&client_id).is_none()
            && let Some(next_id) = replacement
        {
            self.selected_by_client.insert(client_id, next_id);
        }

        Some((removed.id, removed_session))
    }

    fn to_summary(context: &RuntimeContext) -> ContextSummary {
        ContextSummary {
            id: context.id,
            name: context.name.clone(),
            attributes: context.attributes.clone(),
        }
    }
}

fn current_context_id_for_client(state: &Arc<ServerState>, client_id: ClientId) -> Option<Uuid> {
    let context_state = state.context_state.lock().ok()?;
    context_state
        .current_for_client(client_id)
        .map(|context| context.id)
}

fn current_context_id_for_session(state: &Arc<ServerState>, session_id: SessionId) -> Option<Uuid> {
    let context_state = state.context_state.lock().ok()?;
    context_state.context_for_session(session_id)
}

fn current_context_session_for_client(
    state: &Arc<ServerState>,
    client_id: ClientId,
) -> Option<SessionId> {
    let context_state = state.context_state.lock().ok()?;
    context_state.current_session_for_client(client_id)
}

fn prune_context_mappings_for_session(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> Result<Vec<Uuid>> {
    let mut context_state = state
        .context_state
        .lock()
        .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
    Ok(context_state.remove_contexts_for_session(session_id))
}

fn create_session_runtime(state: &Arc<ServerState>, name: Option<String>) -> Result<SessionId> {
    let mut manager = state
        .session_manager
        .lock()
        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
    if let Some(requested_name) = name.as_deref()
        && manager
            .list_sessions()
            .iter()
            .any(|session| session.name.as_deref() == Some(requested_name))
    {
        anyhow::bail!("session already exists with name '{requested_name}'");
    }

    let session_id = manager
        .create_session(name)
        .map_err(|error| anyhow::anyhow!("failed creating session: {error:#}"))?;
    drop(manager);

    let mut runtime_manager = state
        .session_runtimes
        .lock()
        .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
    if let Err(error) = runtime_manager.start_runtime(session_id) {
        drop(runtime_manager);
        let mut rollback_manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
        let _ = rollback_manager.remove_session(&session_id);
        anyhow::bail!("failed creating session runtime: {error:#}");
    }
    drop(runtime_manager);

    Ok(session_id)
}

fn resolve_server_shell(config: &BmuxConfig) -> String {
    if let Some(shell) = config.general.default_shell.as_ref()
        && !shell.trim().is_empty()
    {
        return shell.clone();
    }

    if let Ok(shell) = std::env::var("SHELL")
        && !shell.trim().is_empty()
    {
        return shell;
    }

    if cfg!(windows) {
        "cmd.exe".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

fn resolve_server_pane_term(config: &BmuxConfig) -> String {
    let configured = config.behavior.pane_term.trim();
    let candidate = if configured.is_empty() {
        "bmux-256color"
    } else {
        configured
    };

    if check_terminfo_available(candidate) {
        return candidate.to_string();
    }

    // Configured TERM not found in terminfo; try fallback chain.
    const FALLBACKS: &[&str] = &["xterm-256color", "screen-256color"];
    for fallback in FALLBACKS {
        if *fallback != candidate && check_terminfo_available(fallback) {
            warn!(
                "pane TERM '{}' terminfo not installed; falling back to '{}'",
                candidate, fallback
            );
            return (*fallback).to_string();
        }
    }

    // Nothing in the fallback chain was available either; use the configured
    // value and hope the pane environment can cope.
    warn!(
        "pane TERM '{}' terminfo not installed and no fallback found; using as-is",
        candidate
    );
    candidate.to_string()
}

/// Check whether a terminfo entry is installed by running `infocmp`.
fn check_terminfo_available(term: &str) -> bool {
    std::process::Command::new("infocmp")
        .arg(term)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn shutdown_runtime_handle(removed: RemovedRuntime) {
    for pane in removed.handle.panes.into_values() {
        shutdown_pane_handle(pane).await;
    }
}

async fn shutdown_pane_handle(mut pane: PaneRuntimeHandle) {
    if let Some(stop_tx) = pane.stop_tx.take() {
        let _ = stop_tx.send(());
    }

    if let Ok(_) = tokio::time::timeout(Duration::from_millis(250), &mut pane.task).await {
    } else {
        pane.task.abort();
        let _ = pane.task.await;
    }
}

struct SessionRuntimeManager {
    runtimes: BTreeMap<SessionId, SessionRuntimeHandle>,
    shell: String,
    pane_term: String,
    protocol_profile: ProtocolProfile,
    pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
    recording_runtime: Arc<Mutex<RecordingRuntime>>,
    /// Broadcast sender for pushing pane output notifications to streaming clients.
    event_broadcast: tokio::sync::broadcast::Sender<Event>,
}

struct SessionRuntimeHandle {
    panes: BTreeMap<Uuid, PaneRuntimeHandle>,
    layout_root: PaneLayoutNode,
    focused_pane_id: Uuid,
    zoomed_pane_id: Option<Uuid>,
    floating_surfaces: Vec<FloatingSurfaceRuntime>,
    attached_clients: BTreeSet<ClientId>,
    attach_viewport: Option<AttachViewport>,
    attach_view_revision: u64,
}

#[derive(Clone, Copy)]
struct AttachViewport {
    cols: u16,
    rows: u16,
    status_top_inset: u16,
    status_bottom_inset: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FloatingSurfaceRuntime {
    id: Uuid,
    pane_id: Uuid,
    rect: LayoutRect,
    z: i32,
    visible: bool,
    opaque: bool,
    accepts_input: bool,
    cursor_owner: bool,
}

#[derive(Debug, Clone)]
struct PaneRuntimeMeta {
    id: Uuid,
    name: Option<String>,
    shell: String,
}

struct PaneRuntimeHandle {
    meta: PaneRuntimeMeta,
    process_group_id: Arc<std::sync::Mutex<Option<i32>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
    input_tx: mpsc::UnboundedSender<PaneRuntimeCommand>,
    output_buffer: Arc<std::sync::Mutex<OutputFanoutBuffer>>,
    exited: Arc<AtomicBool>,
    last_requested_size: Arc<std::sync::Mutex<(u16, u16)>>,
    /// Set to `true` by the PTY reader when new output arrives. The broadcast
    /// event is only emitted on the `false→true` transition, coalescing
    /// thousands of per-chunk writes into ~1 event per fetch cycle.
    output_dirty: Arc<AtomicBool>,
}

enum PaneRuntimeCommand {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LayoutRect {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

impl PaneRuntimeHandle {
    fn send_input(&self, data: Vec<u8>) -> std::result::Result<(), SessionRuntimeError> {
        self.input_tx
            .send(PaneRuntimeCommand::Input(data))
            .map_err(|_| SessionRuntimeError::Closed)
    }

    fn resize_pty(&self, rows: u16, cols: u16) {
        if let Ok(mut last) = self.last_requested_size.lock() {
            *last = (rows, cols);
        }
        let _ = self
            .input_tx
            .send(PaneRuntimeCommand::Resize { rows, cols });
    }

    #[cfg(test)]
    fn last_requested_pty_size(&self) -> (u16, u16) {
        self.last_requested_size
            .lock()
            .map(|value| *value)
            .unwrap_or((0, 0))
    }
}

impl SessionRuntimeManager {
    fn bump_attach_view_revision(&mut self, session_id: SessionId) -> Option<u64> {
        let runtime = self.runtimes.get_mut(&session_id)?;
        runtime.attach_view_revision = runtime.attach_view_revision.saturating_add(1);
        Some(runtime.attach_view_revision)
    }
}

struct OutputFanoutBuffer {
    max_bytes: usize,
    start_offset: u64,
    data: VecDeque<u8>,
    cursors: BTreeMap<ClientId, u64>,
}

impl OutputFanoutBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes: max_bytes.max(1),
            start_offset: 0,
            data: VecDeque::new(),
            cursors: BTreeMap::new(),
        }
    }

    fn end_offset(&self) -> u64 {
        self.start_offset + self.data.len() as u64
    }

    fn register_client_at_tail(&mut self, client_id: ClientId) {
        self.cursors.insert(client_id, self.end_offset());
    }

    fn unregister_client(&mut self, client_id: ClientId) {
        self.cursors.remove(&client_id);
    }

    fn push_chunk(&mut self, chunk: &[u8]) {
        self.data.extend(chunk.iter().copied());
        while self.data.len() > self.max_bytes {
            let _ = self.data.pop_front();
            self.start_offset = self.start_offset.saturating_add(1);
        }

        for cursor in self.cursors.values_mut() {
            if *cursor < self.start_offset {
                *cursor = self.start_offset;
            }
        }
    }

    fn read_for_client(&mut self, client_id: ClientId, max_bytes: usize) -> Vec<u8> {
        let limit = max_bytes.max(1);
        let end = self.end_offset();
        let cursor = self.cursors.entry(client_id).or_insert(end);
        if *cursor < self.start_offset {
            *cursor = self.start_offset;
        }

        let available = end.saturating_sub(*cursor) as usize;
        if available == 0 {
            return Vec::new();
        }

        let to_read = available.min(limit);
        let start_index = (*cursor - self.start_offset) as usize;
        let output = self
            .data
            .iter()
            .skip(start_index)
            .take(to_read)
            .copied()
            .collect::<Vec<_>>();
        *cursor = cursor.saturating_add(output.len() as u64);
        output
    }

    fn read_recent(&self, max_bytes: usize) -> Vec<u8> {
        if self.data.is_empty() {
            return Vec::new();
        }
        let to_read = self.data.len().min(max_bytes.max(1));
        let start_index = self.data.len().saturating_sub(to_read);
        self.data
            .iter()
            .skip(start_index)
            .take(to_read)
            .copied()
            .collect()
    }
}

struct RemovedRuntime {
    session_id: SessionId,
    had_attached_clients: bool,
    handle: SessionRuntimeHandle,
}

struct AttachLayoutState {
    focused_pane_id: Uuid,
    panes: Vec<PaneSummary>,
    layout_root: IpcPaneLayoutNode,
    scene: AttachScene,
    zoomed: bool,
}

struct AttachSnapshotState {
    session_id: SessionId,
    focused_pane_id: Uuid,
    panes: Vec<PaneSummary>,
    layout_root: IpcPaneLayoutNode,
    scene: AttachScene,
    chunks: Vec<AttachPaneChunk>,
    zoomed: bool,
}

#[derive(Debug, Clone)]
enum PaneLayoutNode {
    Leaf {
        pane_id: Uuid,
    },
    Split {
        direction: PaneSplitDirection,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionRuntimeError {
    NotFound,
    NotAttached,
    Closed,
}

impl PaneLayoutNode {
    fn pane_order(&self, out: &mut Vec<Uuid>) {
        match self {
            Self::Leaf { pane_id } => out.push(*pane_id),
            Self::Split { first, second, .. } => {
                first.pane_order(out);
                second.pane_order(out);
            }
        }
    }

    fn replace_leaf_with_split(
        &mut self,
        target: Uuid,
        direction: PaneSplitDirection,
        ratio: f32,
        new_pane_id: Uuid,
    ) -> bool {
        match self {
            Self::Leaf { pane_id } if *pane_id == target => {
                *self = Self::Split {
                    direction,
                    ratio,
                    first: Box::new(Self::Leaf { pane_id: target }),
                    second: Box::new(Self::Leaf {
                        pane_id: new_pane_id,
                    }),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.replace_leaf_with_split(target, direction, ratio, new_pane_id)
                    || second.replace_leaf_with_split(target, direction, ratio, new_pane_id)
            }
            _ => false,
        }
    }

    fn remove_leaf(&mut self, target: Uuid) -> bool {
        enum RemoveResult {
            NotFound,
            Removed,
            RemoveThis,
        }

        fn remove_inner(node: &mut PaneLayoutNode, target: Uuid) -> RemoveResult {
            match node {
                PaneLayoutNode::Leaf { pane_id } => {
                    if *pane_id == target {
                        RemoveResult::RemoveThis
                    } else {
                        RemoveResult::NotFound
                    }
                }
                PaneLayoutNode::Split { first, second, .. } => {
                    match remove_inner(first, target) {
                        RemoveResult::NotFound => {}
                        RemoveResult::Removed => return RemoveResult::Removed,
                        RemoveResult::RemoveThis => {
                            *node = *second.clone();
                            return RemoveResult::Removed;
                        }
                    }

                    match remove_inner(second, target) {
                        RemoveResult::NotFound => RemoveResult::NotFound,
                        RemoveResult::Removed => RemoveResult::Removed,
                        RemoveResult::RemoveThis => {
                            *node = *first.clone();
                            RemoveResult::Removed
                        }
                    }
                }
            }
        }

        !matches!(remove_inner(self, target), RemoveResult::NotFound)
    }

    fn adjust_focused_ratio(&mut self, target: Uuid, delta: f32) -> Option<f32> {
        match self {
            Self::Leaf { .. } => None,
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if contains_pane(first, target) || contains_pane(second, target) {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    Some(*ratio)
                } else {
                    first
                        .adjust_focused_ratio(target, delta)
                        .or_else(|| second.adjust_focused_ratio(target, delta))
                }
            }
        }
    }
}

fn contains_pane(node: &PaneLayoutNode, pane_id: Uuid) -> bool {
    match node {
        PaneLayoutNode::Leaf { pane_id: id } => *id == pane_id,
        PaneLayoutNode::Split { first, second, .. } => {
            contains_pane(first, pane_id) || contains_pane(second, pane_id)
        }
    }
}

fn ipc_layout_from_runtime(node: &PaneLayoutNode) -> IpcPaneLayoutNode {
    match node {
        PaneLayoutNode::Leaf { pane_id } => IpcPaneLayoutNode::Leaf { pane_id: *pane_id },
        PaneLayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let percent = (ratio * 100.0).round();
            let ratio_percent = percent.clamp(10.0, 90.0) as u8;
            IpcPaneLayoutNode::Split {
                direction: *direction,
                ratio_percent,
                first: Box::new(ipc_layout_from_runtime(first)),
                second: Box::new(ipc_layout_from_runtime(second)),
            }
        }
    }
}

fn collect_runtime_layout_pane_ids(node: &PaneLayoutNode, out: &mut BTreeSet<Uuid>) -> Result<()> {
    match node {
        PaneLayoutNode::Leaf { pane_id } => {
            if !out.insert(*pane_id) {
                anyhow::bail!("duplicate pane id {pane_id} in runtime layout")
            }
        }
        PaneLayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !(0.1..=0.9).contains(ratio) {
                anyhow::bail!("runtime split ratio {ratio} out of range [0.1, 0.9]")
            }
            collect_runtime_layout_pane_ids(first, out)?;
            collect_runtime_layout_pane_ids(second, out)?;
        }
    }
    Ok(())
}

fn validate_runtime_layout_matches_panes(
    layout_root: &PaneLayoutNode,
    panes: &BTreeMap<Uuid, PaneRuntimeHandle>,
) -> Result<()> {
    let pane_ids = panes.keys().copied().collect::<BTreeSet<_>>();
    let mut layout_ids = BTreeSet::new();
    collect_runtime_layout_pane_ids(layout_root, &mut layout_ids)?;
    if pane_ids != layout_ids {
        anyhow::bail!(
            "runtime layout panes do not match runtime pane map (layout: {}, panes: {})",
            layout_ids.len(),
            pane_ids.len()
        )
    }
    Ok(())
}

fn split_layout_rect(rect: LayoutRect, ratio: f32, vertical: bool) -> (LayoutRect, LayoutRect) {
    let ratio = ratio.clamp(0.1, 0.9);
    if vertical {
        let split = ((f32::from(rect.w) * ratio).round()) as u16;
        let first_w = split.max(1).min(rect.w.saturating_sub(1).max(1));
        let second_w = rect.w.saturating_sub(first_w).max(1);
        (
            LayoutRect {
                x: rect.x,
                y: rect.y,
                w: first_w,
                h: rect.h,
            },
            LayoutRect {
                x: rect.x.saturating_add(first_w),
                y: rect.y,
                w: second_w,
                h: rect.h,
            },
        )
    } else {
        let split = ((f32::from(rect.h) * ratio).round()) as u16;
        let first_h = split.max(1).min(rect.h.saturating_sub(1).max(1));
        let second_h = rect.h.saturating_sub(first_h).max(1);
        (
            LayoutRect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: first_h,
            },
            LayoutRect {
                x: rect.x,
                y: rect.y.saturating_add(first_h),
                w: rect.w,
                h: second_h,
            },
        )
    }
}

fn collect_layout_rects(
    node: &PaneLayoutNode,
    rect: LayoutRect,
    out: &mut BTreeMap<Uuid, LayoutRect>,
) {
    match node {
        PaneLayoutNode::Leaf { pane_id } => {
            out.insert(*pane_id, rect);
        }
        PaneLayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let vertical = matches!(direction, PaneSplitDirection::Vertical);
            let (first_rect, second_rect) = split_layout_rect(rect, *ratio, vertical);
            collect_layout_rects(first, first_rect, out);
            collect_layout_rects(second, second_rect, out);
        }
    }
}

const fn attach_rect_from_layout_rect(rect: LayoutRect) -> AttachRect {
    AttachRect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    }
}

fn scene_root_from_viewport(viewport: Option<AttachViewport>) -> LayoutRect {
    let (cols, rows, status_top_inset, status_bottom_inset) =
        viewport.map_or((0, 0, 0, 0), |viewport| {
            (
                viewport.cols,
                viewport.rows,
                viewport.status_top_inset,
                viewport.status_bottom_inset,
            )
        });
    let y = status_top_inset.min(rows.saturating_sub(1));
    let reserved = status_top_inset.saturating_add(status_bottom_inset);
    let h = rows.saturating_sub(reserved).max(1);
    LayoutRect {
        x: 0,
        y,
        w: cols.max(1),
        h,
    }
}

fn build_attach_scene(
    session_id: SessionId,
    runtime: &SessionRuntimeHandle,
    viewport: Option<AttachViewport>,
) -> AttachScene {
    let scene_root = scene_root_from_viewport(viewport);

    // When a pane is zoomed, produce a single-pane scene that fills the viewport.
    if let Some(zoomed_id) = runtime.zoomed_pane_id {
        if runtime.panes.contains_key(&zoomed_id) {
            let zoomed_surface = AttachSurface {
                id: zoomed_id,
                kind: AttachSurfaceKind::Pane,
                layer: AttachLayer::Pane,
                z: 0,
                rect: attach_rect_from_layout_rect(scene_root),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(zoomed_id),
            };

            let mut surfaces = vec![zoomed_surface];

            // Floating surfaces still render on top of the zoomed pane.
            surfaces.extend(
                runtime
                    .floating_surfaces
                    .iter()
                    .filter(|surface| runtime.panes.contains_key(&surface.pane_id))
                    .map(|surface| AttachSurface {
                        id: surface.id,
                        kind: AttachSurfaceKind::FloatingPane,
                        layer: AttachLayer::FloatingPane,
                        z: surface.z,
                        rect: attach_rect_from_layout_rect(surface.rect),
                        opaque: surface.opaque,
                        visible: surface.visible,
                        accepts_input: surface.accepts_input,
                        cursor_owner: surface.cursor_owner,
                        pane_id: Some(surface.pane_id),
                    }),
            );

            return AttachScene {
                session_id: session_id.0,
                focus: AttachFocusTarget::Pane { pane_id: zoomed_id },
                surfaces,
            };
        }
        // Zoomed pane was removed; fall through to normal rendering.
    }

    let mut rects = BTreeMap::new();
    collect_layout_rects(&runtime.layout_root, scene_root, &mut rects);

    let mut pane_ids = Vec::new();
    runtime.layout_root.pane_order(&mut pane_ids);

    let mut surfaces = pane_ids
        .into_iter()
        .enumerate()
        .filter_map(|(index, pane_id)| {
            rects.get(&pane_id).copied().map(|rect| AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: AttachLayer::Pane,
                z: index as i32,
                rect: attach_rect_from_layout_rect(rect),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: pane_id == runtime.focused_pane_id,
                pane_id: Some(pane_id),
            })
        })
        .collect::<Vec<_>>();

    surfaces.extend(
        runtime
            .floating_surfaces
            .iter()
            .filter(|surface| runtime.panes.contains_key(&surface.pane_id))
            .map(|surface| AttachSurface {
                id: surface.id,
                kind: AttachSurfaceKind::FloatingPane,
                layer: AttachLayer::FloatingPane,
                z: surface.z,
                rect: attach_rect_from_layout_rect(surface.rect),
                opaque: surface.opaque,
                visible: surface.visible,
                accepts_input: surface.accepts_input,
                cursor_owner: surface.cursor_owner,
                pane_id: Some(surface.pane_id),
            }),
    );

    AttachScene {
        session_id: session_id.0,
        focus: AttachFocusTarget::Pane {
            pane_id: runtime.focused_pane_id,
        },
        surfaces,
    }
}

fn pane_pty_size(layout_rect: LayoutRect) -> (u16, u16) {
    let cols = layout_rect.w.saturating_sub(2).max(1);
    let rows = layout_rect.h.saturating_sub(2).max(1);
    (rows, cols)
}

fn resize_session_ptys(
    runtime: &mut SessionRuntimeHandle,
    cols: u16,
    rows: u16,
    status_top_inset: u16,
    status_bottom_inset: u16,
) {
    let y = status_top_inset.min(rows.saturating_sub(1));
    let reserved = status_top_inset.saturating_add(status_bottom_inset);
    let root = LayoutRect {
        x: 0,
        y,
        w: cols.max(1),
        h: rows.saturating_sub(reserved).max(1),
    };

    // When zoomed, only resize the zoomed pane to fill the viewport.
    if let Some(zoomed_id) = runtime.zoomed_pane_id {
        if let Some(pane) = runtime.panes.get(&zoomed_id) {
            if !pane.exited.load(Ordering::SeqCst) {
                let (zoom_rows, zoom_cols) = pane_pty_size(root);
                pane.resize_pty(zoom_rows, zoom_cols);
            }
        }
        return;
    }

    let mut rects = BTreeMap::new();
    collect_layout_rects(&runtime.layout_root, root, &mut rects);
    for (pane_id, pane) in &runtime.panes {
        if pane.exited.load(Ordering::SeqCst) {
            continue;
        }
        if let Some(rect) = rects.get(pane_id).copied() {
            let (rows, cols) = pane_pty_size(rect);
            pane.resize_pty(rows, cols);
        }
    }
}

fn layout_from_panes(panes: &[PaneRuntimeMeta]) -> Option<PaneLayoutNode> {
    let mut iter = panes.iter();
    let first = iter.next()?;
    let mut root = PaneLayoutNode::Leaf { pane_id: first.id };
    for pane in iter {
        root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(root),
            second: Box::new(PaneLayoutNode::Leaf { pane_id: pane.id }),
        };
    }
    Some(root)
}

fn snapshot_layout_from_runtime(node: &PaneLayoutNode) -> PaneLayoutNodeSnapshotV2 {
    match node {
        PaneLayoutNode::Leaf { pane_id } => PaneLayoutNodeSnapshotV2::Leaf { pane_id: *pane_id },
        PaneLayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => PaneLayoutNodeSnapshotV2::Split {
            direction: match direction {
                PaneSplitDirection::Vertical => PaneSplitDirectionSnapshotV2::Vertical,
                PaneSplitDirection::Horizontal => PaneSplitDirectionSnapshotV2::Horizontal,
            },
            ratio: *ratio,
            first: Box::new(snapshot_layout_from_runtime(first)),
            second: Box::new(snapshot_layout_from_runtime(second)),
        },
    }
}

fn runtime_layout_from_snapshot(node: &PaneLayoutNodeSnapshotV2) -> PaneLayoutNode {
    match node {
        PaneLayoutNodeSnapshotV2::Leaf { pane_id } => PaneLayoutNode::Leaf { pane_id: *pane_id },
        PaneLayoutNodeSnapshotV2::Split {
            direction,
            ratio,
            first,
            second,
        } => PaneLayoutNode::Split {
            direction: match direction {
                PaneSplitDirectionSnapshotV2::Vertical => PaneSplitDirection::Vertical,
                PaneSplitDirectionSnapshotV2::Horizontal => PaneSplitDirection::Horizontal,
            },
            ratio: *ratio,
            first: Box::new(runtime_layout_from_snapshot(first)),
            second: Box::new(runtime_layout_from_snapshot(second)),
        },
    }
}

impl SessionRuntimeManager {
    fn new(
        shell: String,
        pane_term: String,
        protocol_profile: ProtocolProfile,
        pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
        recording_runtime: Arc<Mutex<RecordingRuntime>>,
        event_broadcast: tokio::sync::broadcast::Sender<Event>,
    ) -> Self {
        Self {
            runtimes: BTreeMap::new(),
            shell,
            pane_term,
            protocol_profile,
            pane_exit_tx,
            recording_runtime,
            event_broadcast,
        }
    }

    fn start_runtime(&mut self, session_id: SessionId) -> Result<()> {
        if self.runtimes.contains_key(&session_id) {
            anyhow::bail!("runtime already exists for session {}", session_id.0);
        }

        let first_pane_id = Uuid::new_v4();
        let pane_meta = PaneRuntimeMeta {
            id: first_pane_id,
            name: Some("pane-1".to_string()),
            shell: self.shell.clone(),
        };
        let first_pane = self.spawn_pane_runtime(session_id, pane_meta)?;
        let mut panes = BTreeMap::new();
        panes.insert(first_pane_id, first_pane);

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                panes,
                layout_root: PaneLayoutNode::Leaf {
                    pane_id: first_pane_id,
                },
                focused_pane_id: first_pane_id,
                zoomed_pane_id: None,
                floating_surfaces: Vec::new(),
                attached_clients: BTreeSet::new(),
                attach_viewport: None,
                attach_view_revision: 0,
            },
        );
        Ok(())
    }

    fn restore_runtime(
        &mut self,
        session_id: SessionId,
        panes: Vec<PaneRuntimeMeta>,
        layout_root: Option<PaneLayoutNode>,
        focused_pane_id: Uuid,
        floating_surfaces: Vec<FloatingSurfaceRuntime>,
    ) -> Result<()> {
        if self.runtimes.contains_key(&session_id) {
            anyhow::bail!("runtime already exists for session {}", session_id.0);
        }

        if panes.is_empty() {
            anyhow::bail!("restored runtime must include panes");
        }
        if !panes.iter().any(|pane| pane.id == focused_pane_id) {
            anyhow::bail!("focused pane missing from restored runtime");
        }

        let mut runtime_panes = BTreeMap::new();
        for pane_meta in &panes {
            let pane = self.spawn_pane_runtime(session_id, pane_meta.clone())?;
            runtime_panes.insert(pane_meta.id, pane);
        }

        let runtime_layout_root = layout_root
            .unwrap_or_else(|| layout_from_panes(&panes).expect("restored runtime has panes"));
        validate_runtime_layout_matches_panes(&runtime_layout_root, &runtime_panes)?;

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                panes: runtime_panes,
                layout_root: runtime_layout_root,
                focused_pane_id,
                zoomed_pane_id: None,
                floating_surfaces,
                attached_clients: BTreeSet::new(),
                attach_viewport: None,
                attach_view_revision: 0,
            },
        );

        Ok(())
    }

    fn spawn_pane_runtime(
        &self,
        session_id: SessionId,
        pane_meta: PaneRuntimeMeta,
    ) -> Result<PaneRuntimeHandle> {
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<PaneRuntimeCommand>();
        let output_buffer = Arc::new(std::sync::Mutex::new(OutputFanoutBuffer::new(
            MAX_WINDOW_OUTPUT_BUFFER_BYTES,
        )));
        let last_requested_size = Arc::new(std::sync::Mutex::new((24_u16, 80_u16)));
        let shell = pane_meta.shell.clone();
        let pane_term = self.pane_term.clone();
        let protocol_profile = self.protocol_profile;
        let pane_id = pane_meta.id;
        let pane_exit_tx = self.pane_exit_tx.clone();
        let recording_runtime = Arc::clone(&self.recording_runtime);
        let output_buffer_for_reader = Arc::clone(&output_buffer);
        let process_group_id = Arc::new(std::sync::Mutex::new(None));
        let process_group_id_for_task = Arc::clone(&process_group_id);
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_task = Arc::clone(&exited);
        let event_broadcast_for_reader = self.event_broadcast.clone();
        let output_dirty = Arc::new(AtomicBool::new(false));
        let output_dirty_for_reader = Arc::clone(&output_dirty);

        let task = tokio::spawn(async move {
            let pty_system = native_pty_system();
            let pty_pair = if let Ok(pair) = pty_system.openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                pair
            } else {
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };

            let mut command = CommandBuilder::new(&shell);
            command.env("TERM", &pane_term);
            let mut child = if let Ok(child) = pty_pair.slave.spawn_command(command) {
                child
            } else {
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };
            if let Ok(mut pgid) = process_group_id_for_task.lock() {
                *pgid = child
                    .process_id()
                    .and_then(resolve_process_group_id_for_pid);
            }
            let mut child_killer = child.clone_killer();
            drop(pty_pair.slave);

            let master = pty_pair.master;

            let mut reader = if let Ok(reader) = master.try_clone_reader() {
                reader
            } else {
                let _ = child.kill();
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };
            let writer = if let Ok(writer) = master.take_writer() {
                writer
            } else {
                let _ = child.kill();
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };
            let writer = Arc::new(std::sync::Mutex::new(writer));

            let (child_exit_tx, mut child_exit_rx) = mpsc::unbounded_channel::<()>();
            let exited_for_waiter = Arc::clone(&exited_for_task);
            let child_waiter = std::thread::Builder::new()
                .name(format!("bmux-server-pane-{pane_id}-wait"))
                .spawn(move || {
                    let _ = child.wait();
                    exited_for_waiter.store(true, Ordering::SeqCst);
                    let _ = pane_exit_tx.send(PaneExitEvent {
                        session_id,
                        pane_id,
                    });
                    let _ = child_exit_tx.send(());
                })
                .ok();

            let reader_output = Arc::clone(&output_buffer_for_reader);
            let writer_for_reader = Arc::clone(&writer);
            let reader_thread = std::thread::Builder::new()
                .name(format!("bmux-server-pane-{pane_id}"))
                .spawn(move || {
                    let mut buffer = [0_u8; 8192];
                    let mut protocol_engine = TerminalProtocolEngine::new(protocol_profile);
                    loop {
                        match reader.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(bytes_read) => {
                                let chunk = &buffer[..bytes_read];
                                if let Ok(mut output) = reader_output.lock() {
                                    output.push_chunk(chunk);
                                } else {
                                    break;
                                }
                                // Notify streaming clients that new output is available.
                                // Only emit when transitioning false→true to coalesce
                                // thousands of per-chunk writes into ~1 event per fetch cycle.
                                if output_dirty_for_reader
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    )
                                    .is_ok()
                                {
                                    let _ = event_broadcast_for_reader.send(
                                        Event::PaneOutputAvailable {
                                            session_id: session_id.0,
                                            pane_id,
                                        },
                                    );
                                }
                                if let Ok(runtime) = recording_runtime.lock() {
                                    let _ = runtime.record(
                                        RecordingEventKind::PaneOutputRaw,
                                        RecordingPayload::Bytes {
                                            data: chunk.to_vec(),
                                        },
                                        RecordMeta {
                                            session_id: Some(session_id.0),
                                            pane_id: Some(pane_id),
                                            client_id: None,
                                        },
                                    );
                                }
                                let reply = protocol_engine.process_output(chunk, (0, 0));
                                if !reply.is_empty() {
                                    if let Ok(runtime) = recording_runtime.lock() {
                                        let _ = runtime.record(
                                            RecordingEventKind::ProtocolReplyRaw,
                                            RecordingPayload::Bytes {
                                                data: reply.clone(),
                                            },
                                            RecordMeta {
                                                session_id: Some(session_id.0),
                                                pane_id: Some(pane_id),
                                                client_id: None,
                                            },
                                        );
                                    }
                                    if let Ok(mut writer) = writer_for_reader.lock() {
                                        if writer.write_all(&reply).is_err() {
                                            break;
                                        }
                                        let _ = writer.flush();
                                    } else {
                                        break;
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                })
                .ok();

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        let _ = child_killer.kill();
                        break;
                    }
                    child_exit = child_exit_rx.recv() => {
                        if child_exit.is_some() {
                            break;
                        }
                    }
                    input = input_rx.recv() => {
                        match input {
                            Some(PaneRuntimeCommand::Input(bytes)) => {
                                if let Ok(mut writer) = writer.lock()
                                    && writer.write_all(&bytes).is_ok()
                                {
                                    let _ = writer.flush();
                                } else {
                                    break;
                                }
                            }
                            Some(PaneRuntimeCommand::Resize { rows, cols }) => {
                                let _ = master.resize(PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            }
                            None => break,
                        }
                    }
                }
            }

            if let Some(waiter) = child_waiter {
                let _ = waiter.join();
            }
            if let Some(thread) = reader_thread {
                let _ = thread.join();
            }
            exited_for_task.store(true, Ordering::SeqCst);
        });

        Ok(PaneRuntimeHandle {
            meta: pane_meta,
            process_group_id,
            stop_tx: Some(stop_tx),
            task,
            input_tx,
            output_buffer,
            exited,
            last_requested_size,
            output_dirty,
        })
    }

    fn split_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
    ) -> Result<Uuid> {
        // Auto-unzoom on layout mutation.
        if let Some(session) = self.runtimes.get_mut(&session_id) {
            session.zoomed_pane_id = None;
        }
        let (target_pane_id, next_pane_name, shell, client_ids) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let target_pane_id =
                resolve_pane_id_from_selector(session, target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let focused = session
                .panes
                .get(&target_pane_id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let name_prefix = match direction {
                PaneSplitDirection::Vertical => "v",
                PaneSplitDirection::Horizontal => "h",
            };
            (
                target_pane_id,
                Some(format!("{name_prefix}-pane-{}", session.panes.len() + 1)),
                focused.meta.shell.clone(),
                session.attached_clients.iter().copied().collect::<Vec<_>>(),
            )
        };

        let pane_id = Uuid::new_v4();
        let pane_meta = PaneRuntimeMeta {
            id: pane_id,
            name: next_pane_name,
            shell,
        };
        let handle = self.spawn_pane_runtime(session_id, pane_meta)?;
        for client_id in client_ids {
            if let Ok(mut output) = handle.output_buffer.lock() {
                output.register_client_at_tail(client_id);
            }
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        session.panes.insert(pane_id, handle);
        let replaced =
            session
                .layout_root
                .replace_leaf_with_split(target_pane_id, direction, 0.5, pane_id);
        if !replaced {
            anyhow::bail!("failed to apply split to layout tree")
        }
        session.focused_pane_id = pane_id;
        self.apply_stored_attach_viewport(session_id);
        Ok(pane_id)
    }

    fn focus_pane(&mut self, session_id: SessionId, direction: PaneFocusDirection) -> Result<Uuid> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        // If zoomed, stay zoomed but update to the new focused pane.
        let was_zoomed = session.zoomed_pane_id.is_some();
        let mut pane_ids = Vec::new();
        session.layout_root.pane_order(&mut pane_ids);
        if pane_ids.is_empty() {
            anyhow::bail!("no panes in session runtime")
        }
        let current_index = pane_ids
            .iter()
            .position(|id| *id == session.focused_pane_id)
            .unwrap_or(0);
        let len = pane_ids.len();
        let next_index = match direction {
            PaneFocusDirection::Next => (current_index + 1) % len,
            PaneFocusDirection::Prev => {
                if current_index == 0 {
                    len - 1
                } else {
                    current_index - 1
                }
            }
        };
        session.focused_pane_id = pane_ids[next_index];
        if was_zoomed {
            session.zoomed_pane_id = Some(pane_ids[next_index]);
            self.apply_stored_attach_viewport(session_id);
        }
        Ok(self.runtimes[&session_id].focused_pane_id)
    }

    fn focus_pane_target(&mut self, session_id: SessionId, target: PaneSelector) -> Result<Uuid> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        // If zoomed, stay zoomed but update to the new focused pane.
        let was_zoomed = session.zoomed_pane_id.is_some();
        let pane_id = resolve_pane_id_from_selector(session, target)
            .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
        session.focused_pane_id = pane_id;
        if was_zoomed {
            session.zoomed_pane_id = Some(pane_id);
            self.apply_stored_attach_viewport(session_id);
        }
        Ok(pane_id)
    }

    fn close_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> Result<(Uuid, Option<RemovedRuntime>)> {
        // Auto-unzoom on layout mutation.
        if let Some(session) = self.runtimes.get_mut(&session_id) {
            session.zoomed_pane_id = None;
        }
        let (pane_id, remove_runtime) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let pane_id =
                resolve_pane_id_from_selector(session, target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            (pane_id, session.panes.len() == 1)
        };

        if remove_runtime {
            let removed = self.remove_runtime(session_id)?;
            return Ok((pane_id, Some(removed)));
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let pane = session
            .panes
            .remove(&pane_id)
            .ok_or_else(|| anyhow::anyhow!("focused pane not found"))?;
        let _ = session.layout_root.remove_leaf(pane_id);
        let mut remaining = Vec::new();
        session.layout_root.pane_order(&mut remaining);
        if (session.focused_pane_id == pane_id
            || !session.panes.contains_key(&session.focused_pane_id))
            && let Some(next_id) = remaining.first().copied()
        {
            session.focused_pane_id = next_id;
        }

        tokio::spawn(async move {
            shutdown_pane_handle(pane).await;
        });
        self.apply_stored_attach_viewport(session_id);
        Ok((pane_id, None))
    }

    fn resize_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        delta: i16,
    ) -> Result<()> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        // Auto-unzoom on layout mutation.
        session.zoomed_pane_id = None;
        let pane_id =
            resolve_pane_id_from_selector(session, target.unwrap_or(PaneSelector::Active))
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
        let step = f32::from(delta) * 0.05;
        let _ = session.layout_root.adjust_focused_ratio(pane_id, step);
        self.apply_stored_attach_viewport(session_id);
        Ok(())
    }

    fn toggle_zoom(&mut self, session_id: SessionId) -> Result<(Uuid, bool)> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let focused = session.focused_pane_id;
        if session.zoomed_pane_id.is_some() {
            session.zoomed_pane_id = None;
            self.apply_stored_attach_viewport(session_id);
            Ok((focused, false))
        } else {
            // Only zoom if there are at least 2 panes (zooming a single pane is a no-op).
            let mut pane_ids = Vec::new();
            session.layout_root.pane_order(&mut pane_ids);
            if pane_ids.len() < 2 {
                return Ok((focused, false));
            }
            session.zoomed_pane_id = Some(focused);
            self.apply_stored_attach_viewport(session_id);
            Ok((focused, true))
        }
    }

    fn list_panes(&self, session_id: SessionId) -> Result<Vec<PaneSummary>> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let mut pane_ids = Vec::new();
        session.layout_root.pane_order(&mut pane_ids);
        let panes = pane_ids
            .iter()
            .enumerate()
            .filter_map(|(index, pane_id)| {
                session.panes.get(pane_id).map(|pane| PaneSummary {
                    id: *pane_id,
                    index: (index + 1) as u32,
                    name: pane.meta.name.clone(),
                    focused: *pane_id == session.focused_pane_id,
                })
            })
            .collect::<Vec<_>>();
        Ok(panes)
    }

    fn attach_layout_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<AttachLayoutState, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let scene = build_attach_scene(session_id, session, session.attach_viewport);
        let mut pane_ids = Vec::new();
        session.layout_root.pane_order(&mut pane_ids);
        let panes = pane_ids
            .iter()
            .enumerate()
            .filter_map(|(index, pane_id)| {
                session.panes.get(pane_id).map(|pane| PaneSummary {
                    id: *pane_id,
                    index: (index + 1) as u32,
                    name: pane.meta.name.clone(),
                    focused: *pane_id == session.focused_pane_id,
                })
            })
            .collect::<Vec<_>>();
        Ok(AttachLayoutState {
            focused_pane_id: session.focused_pane_id,
            panes,
            layout_root: ipc_layout_from_runtime(&session.layout_root),
            scene,
            zoomed: session.zoomed_pane_id.is_some(),
        })
    }

    fn active_pane_exited(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<bool, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let pane = session
            .panes
            .get(&session.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        Ok(pane.exited.load(Ordering::SeqCst))
    }

    fn attach_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes_per_pane: usize,
    ) -> Result<AttachSnapshotState, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let scene = build_attach_scene(session_id, session, session.attach_viewport);
        let mut pane_ids = Vec::new();
        session.layout_root.pane_order(&mut pane_ids);
        let panes = pane_ids
            .iter()
            .enumerate()
            .filter_map(|(index, pane_id)| {
                session.panes.get(pane_id).map(|pane| PaneSummary {
                    id: *pane_id,
                    index: (index + 1) as u32,
                    name: pane.meta.name.clone(),
                    focused: *pane_id == session.focused_pane_id,
                })
            })
            .collect::<Vec<_>>();

        let mut chunks = Vec::new();
        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        for pane_id in pane_ids {
            let Some(pane) = session.panes.get(&pane_id) else {
                continue;
            };
            let allowed = per_pane_budget.min(budget_remaining);
            let data = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .read_recent(allowed);
            budget_remaining = budget_remaining.saturating_sub(data.len());
            chunks.push(AttachPaneChunk { pane_id, data });
        }

        Ok(AttachSnapshotState {
            session_id,
            focused_pane_id: session.focused_pane_id,
            panes,
            layout_root: ipc_layout_from_runtime(&session.layout_root),
            scene,
            chunks,
            zoomed: session.zoomed_pane_id.is_some(),
        })
    }

    fn read_pane_output_batch(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> Result<Vec<AttachPaneChunk>, SessionRuntimeError> {
        let (chunks, closed_active) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?;
            if !session.attached_clients.contains(&client_id) {
                return Err(SessionRuntimeError::NotAttached);
            }

            let mut chunks = Vec::new();
            let mut closed_active = false;
            let num_panes = pane_ids.len().max(1);
            let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes);
            let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
            for pane_id in pane_ids {
                let Some(pane) = session.panes.get_mut(pane_id) else {
                    continue;
                };
                let allowed = per_pane_budget.min(budget_remaining);
                let mut output = pane
                    .output_buffer
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)?;
                let data = output.read_for_client(client_id, allowed);
                drop(output);
                if pane.exited.load(Ordering::SeqCst) && *pane_id == session.focused_pane_id {
                    closed_active = true;
                }
                budget_remaining = budget_remaining.saturating_sub(data.len());
                chunks.push(AttachPaneChunk {
                    pane_id: *pane_id,
                    data,
                });
            }
            (chunks, closed_active)
        };

        if closed_active {
            return Err(SessionRuntimeError::Closed);
        }

        Ok(chunks)
    }

    fn remove_runtime(&mut self, session_id: SessionId) -> Result<RemovedRuntime> {
        let runtime = self
            .runtimes
            .remove(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;

        Ok(RemovedRuntime {
            session_id,
            had_attached_clients: !runtime.attached_clients.is_empty(),
            handle: runtime,
        })
    }

    fn remove_all_runtimes(&mut self) -> Vec<RemovedRuntime> {
        std::mem::take(&mut self.runtimes)
            .into_iter()
            .map(|(session_id, runtime)| RemovedRuntime {
                session_id,
                had_attached_clients: !runtime.attached_clients.is_empty(),
                handle: runtime,
            })
            .collect()
    }

    fn begin_attach(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<(), SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        let pane = runtime
            .panes
            .get(&runtime.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if pane.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }

        runtime.attached_clients.insert(client_id);
        for pane in runtime.panes.values_mut() {
            let mut output = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            output.register_client_at_tail(client_id);
        }
        if let Some(viewport) = runtime.attach_viewport {
            resize_session_ptys(
                runtime,
                viewport.cols,
                viewport.rows,
                viewport.status_top_inset,
                viewport.status_bottom_inset,
            );
        }
        Ok(())
    }

    fn end_attach(&mut self, session_id: SessionId, client_id: ClientId) {
        if let Some(runtime) = self.runtimes.get_mut(&session_id) {
            let removed = runtime.attached_clients.remove(&client_id);
            if removed {
                for pane in runtime.panes.values_mut() {
                    if let Ok(mut output) = pane.output_buffer.lock() {
                        output.unregister_client(client_id);
                    }
                }
            }
        }
    }

    fn set_attach_viewport(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> Result<(u16, u16, u16, u16), SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let cols = cols.max(1);
        let rows = rows.max(2);
        let mut status_top_inset = status_top_inset.min(1);
        let mut status_bottom_inset = status_bottom_inset.min(1);
        while status_top_inset.saturating_add(status_bottom_inset) >= rows {
            if status_bottom_inset > 0 {
                status_bottom_inset -= 1;
            } else if status_top_inset > 0 {
                status_top_inset -= 1;
            } else {
                break;
            }
        }
        runtime.attach_viewport = Some(AttachViewport {
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
        });
        resize_session_ptys(runtime, cols, rows, status_top_inset, status_bottom_inset);
        Ok((cols, rows, status_top_inset, status_bottom_inset))
    }

    fn apply_stored_attach_viewport(&mut self, session_id: SessionId) {
        let Some(runtime) = self.runtimes.get_mut(&session_id) else {
            return;
        };
        let Some(viewport) = runtime.attach_viewport else {
            return;
        };
        resize_session_ptys(
            runtime,
            viewport.cols,
            viewport.rows,
            viewport.status_top_inset,
            viewport.status_bottom_inset,
        );
    }

    fn write_input(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<(usize, Uuid), SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let focused_pane_id = runtime.focused_pane_id;
        let pane = runtime
            .panes
            .get_mut(&focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if pane.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }

        let bytes = data.len();
        pane.send_input(data)?;
        Ok((bytes, focused_pane_id))
    }

    /// Write input bytes directly to a specific pane by ID, bypassing focus routing.
    fn write_input_to_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> Result<usize, SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        let pane = runtime
            .panes
            .get_mut(&pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if pane.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }

        let bytes = data.len();
        pane.send_input(data)?;
        Ok(bytes)
    }

    fn read_output(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let pane = runtime
            .panes
            .get_mut(&runtime.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if max_bytes == 0 {
            if pane.exited.load(Ordering::SeqCst) {
                return Err(SessionRuntimeError::Closed);
            }
            return Ok(Vec::new());
        }

        let mut output = pane
            .output_buffer
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        let bytes = output.read_for_client(client_id, max_bytes);
        drop(output);

        if bytes.is_empty() && pane.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }

        Ok(bytes)
    }

    #[cfg(test)]
    fn runtime_count(&self) -> usize {
        self.runtimes.len()
    }

    #[cfg(test)]
    fn has_runtime(&self, session_id: SessionId) -> bool {
        self.runtimes.contains_key(&session_id)
    }
}

fn resolve_pane_id_from_selector(
    runtime: &SessionRuntimeHandle,
    selector: PaneSelector,
) -> Option<Uuid> {
    match selector {
        PaneSelector::Active => runtime
            .panes
            .contains_key(&runtime.focused_pane_id)
            .then_some(runtime.focused_pane_id),
        PaneSelector::ById(id) => runtime.panes.contains_key(&id).then_some(id),
        PaneSelector::ByIndex(index) => {
            if index == 0 {
                return None;
            }
            let mut pane_ids = Vec::new();
            runtime.layout_root.pane_order(&mut pane_ids);
            let position = usize::try_from(index.saturating_sub(1)).ok()?;
            let pane_id = pane_ids.get(position).copied()?;
            runtime.panes.contains_key(&pane_id).then_some(pane_id)
        }
    }
}

impl BmuxServer {
    fn new_with_snapshot(
        endpoint: IpcEndpoint,
        snapshot_manager: Option<SnapshotManager>,
        server_control_principal_id: Uuid,
        recordings_dir: std::path::PathBuf,
        segment_mb: usize,
        retention_days: u64,
    ) -> Self {
        let snapshot_runtime = match snapshot_manager {
            Some(manager) => SnapshotRuntime::with_manager(manager),
            None => SnapshotRuntime::disabled(),
        };

        let config = BmuxConfig::load().unwrap_or_default();
        let shell = resolve_server_shell(&config);
        let pane_term = resolve_server_pane_term(&config);
        let protocol_profile = protocol_profile_for_term(&pane_term);
        let (shutdown_tx, _) = watch::channel(false);
        let (pane_exit_tx, pane_exit_rx) = mpsc::unbounded_channel();
        let recording_runtime = Arc::new(Mutex::new(RecordingRuntime::new(
            recordings_dir,
            segment_mb,
            retention_days,
        )));
        let (event_broadcast_tx, _) = tokio::sync::broadcast::channel::<Event>(256);
        Self {
            endpoint,
            state: Arc::new(ServerState {
                session_manager: Mutex::new(SessionManager::new()),
                session_runtimes: Mutex::new(SessionRuntimeManager::new(
                    shell,
                    pane_term,
                    protocol_profile,
                    pane_exit_tx,
                    Arc::clone(&recording_runtime),
                    event_broadcast_tx.clone(),
                )),
                attach_tokens: Mutex::new(AttachTokenManager::new(ATTACH_TOKEN_TTL)),
                follow_state: Mutex::new(FollowState::default()),
                context_state: Mutex::new(ContextState::default()),
                snapshot_runtime: Mutex::new(snapshot_runtime),
                recording_runtime,
                operation_lock: AsyncMutex::new(()),
                event_hub: Mutex::new(EventHub::new(1024)),
                event_broadcast: event_broadcast_tx,
                client_principals: Mutex::new(BTreeMap::new()),
                server_control_principal_id,
                handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
                pane_exit_rx: AsyncMutex::new(pane_exit_rx),
                service_registry: Mutex::new(ServiceRegistry::default()),
                service_resolver: Mutex::new(None),
            }),
            shutdown_tx,
        }
    }

    /// Create a server with an explicit endpoint.
    #[must_use]
    pub fn new(endpoint: IpcEndpoint) -> Self {
        let paths = ConfigPaths::default();
        let config = BmuxConfig::load_from_path(&paths.config_file()).unwrap_or_default();
        Self::new_with_snapshot(
            endpoint,
            None,
            Uuid::new_v4(),
            config.recordings_dir(&paths),
            config.recording.segment_mb,
            config.recording.retention_days,
        )
    }

    /// Create a server with endpoint derived from config paths.
    #[must_use]
    pub fn from_config_paths(paths: &ConfigPaths) -> Self {
        let config = BmuxConfig::load_from_path(&paths.config_file()).unwrap_or_default();
        #[cfg(unix)]
        let endpoint = IpcEndpoint::unix_socket(paths.server_socket());

        #[cfg(windows)]
        let endpoint = IpcEndpoint::windows_named_pipe(paths.server_named_pipe());

        let snapshot_manager = SnapshotManager::from_paths(paths);
        let server_control_principal_id =
            load_or_create_principal_id(paths).unwrap_or_else(|error| {
                warn!("failed loading server control principal id: {error}");
                Uuid::new_v4()
            });
        Self::new_with_snapshot(
            endpoint,
            Some(snapshot_manager),
            server_control_principal_id,
            config.recordings_dir(paths),
            config.recording.segment_mb,
            config.recording.retention_days,
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
        let route = ServiceRoute {
            capability: capability.into(),
            kind,
            interface_id: interface_id.into(),
            operation: operation.into(),
        };
        let wrapped: Arc<ServiceInvokeHandler> =
            Arc::new(move |route, context, payload| Box::pin(handler(route, context, payload)));

        let mut registry = self
            .state
            .service_registry
            .lock()
            .map_err(|_| anyhow::anyhow!("service registry lock poisoned"))?;
        registry.handlers.insert(route, wrapped);
        Ok(())
    }

    /// Register a generic fallback resolver for service routes that are not
    /// explicitly present in the service registry.
    pub fn set_service_resolver<F, Fut>(&self, resolver: F) -> Result<()>
    where
        F: Fn(ServiceRoute, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<u8>>> + Send + 'static,
    {
        let wrapped: Arc<ServiceResolverHandler> =
            Arc::new(move |route, payload| Box::pin(resolver(route, payload)));
        let mut slot = self
            .state
            .service_resolver
            .lock()
            .map_err(|_| anyhow::anyhow!("service resolver lock poisoned"))?;
        *slot = Some(wrapped);
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

    /// Run the server accept loop until shutdown is requested.
    ///
    /// # Errors
    ///
    /// Returns an error if binding or accept-loop operations fail.
    pub async fn run(&self) -> Result<()> {
        self.run_impl(None).await
    }

    async fn run_impl(
        &self,
        mut ready_tx: Option<oneshot::Sender<std::result::Result<(), String>>>,
    ) -> Result<()> {
        let listener = match LocalIpcListener::bind(&self.endpoint)
            .await
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

        if let Err(error) = restore_snapshot_if_present(&self.state) {
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Err(format!("{error:#}")));
            }
            return Err(error);
        }

        info!("bmux server listening on {:?}", self.endpoint);
        emit_event(&self.state, Event::ServerStarted)?;
        if let Some(tx) = ready_tx.take() {
            let _ = tx.send(Ok(()));
        }

        let pane_exit_state = Arc::clone(&self.state);
        let pane_exit_shutdown_rx = self.shutdown_tx.subscribe();
        let pane_exit_task = tokio::spawn(async move {
            process_pane_exit_events(pane_exit_state, pane_exit_shutdown_rx).await;
        });

        // Periodic recording retention enforcement.
        let recording_prune_runtime = Arc::clone(&self.state.recording_runtime);
        let mut prune_shutdown_rx = self.shutdown_tx.subscribe();
        let _prune_task = tokio::spawn(async move {
            // Initial prune on startup.
            if let Ok(runtime) = recording_prune_runtime.lock() {
                match runtime.prune(None) {
                    Ok(0) => {}
                    Ok(n) => info!("startup recording prune: deleted {n} recording(s)"),
                    Err(e) => warn!("startup recording prune failed: {e:#}"),
                }
            }
            // Periodic prune every hour.
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            interval.tick().await; // skip the immediate first tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Ok(runtime) = recording_prune_runtime.lock() {
                            match runtime.prune(None) {
                                Ok(0) => {}
                                Ok(n) => info!("periodic recording prune: deleted {n} recording(s)"),
                                Err(e) => warn!("periodic recording prune failed: {e:#}"),
                            }
                        }
                    }
                    changed = prune_shutdown_rx.changed() => {
                        if changed.is_ok() && *prune_shutdown_rx.borrow() {
                            break;
                        }
                        if changed.is_err() {
                            break;
                        }
                    }
                }
            }
        });

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
        let _ = pane_exit_task.await;

        let removed_runtimes = if let Ok(mut runtime_manager) = self.state.session_runtimes.lock() {
            runtime_manager.remove_all_runtimes()
        } else {
            Vec::new()
        };
        for removed_runtime in removed_runtimes {
            if removed_runtime.had_attached_clients {
                let _ = emit_event(
                    &self.state,
                    Event::ClientDetached {
                        id: removed_runtime.session_id.0,
                    },
                );
            }
            shutdown_runtime_handle(removed_runtime).await;
        }
        if let Ok(mut session_manager) = self.state.session_manager.lock() {
            *session_manager = SessionManager::new();
        }
        let _ = emit_event(&self.state, Event::ServerStopping);
        if let Ok(mut attach_tokens) = self.state.attach_tokens.lock() {
            attach_tokens.clear();
        }

        info!("bmux server listener closed on {:?}", self.endpoint);

        Ok(())
    }

    #[cfg(test)]
    async fn run_with_ready(
        &self,
        ready_tx: oneshot::Sender<std::result::Result<(), String>>,
    ) -> Result<()> {
        self.run_impl(Some(ready_tx)).await
    }
}

async fn handle_connection(
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
    mut stream: LocalIpcStream,
) -> Result<()> {
    let client_id = ClientId::new();
    let client_principal_id: Uuid;
    let mut selected_session: Option<SessionId> = None;
    let mut attached_stream_session: Option<SessionId> = None;

    // ── Handshake (serial, before split) ─────────────────────────────────

    let first_envelope = tokio::time::timeout(state.handshake_timeout, stream.recv_envelope())
        .await
        .context("handshake timed out")??;

    let handshake = parse_request(&first_envelope)?;
    if let Request::Hello {
        protocol_version,
        client_name,
        principal_id,
    } = handshake
    {
        if protocol_version != ProtocolVersion::current() {
            send_error(
                &mut stream,
                first_envelope.request_id,
                ErrorCode::VersionMismatch,
                format!(
                    "unsupported protocol version {}; expected {}",
                    protocol_version.0, CURRENT_PROTOCOL_VERSION
                ),
            )
            .await?;
            return Ok(());
        }
        client_principal_id = principal_id;
        debug!("accepted client handshake: {client_name}");
        let snapshot = snapshot_status(&state)?;
        send_ok(
            &mut stream,
            first_envelope.request_id,
            ResponsePayload::ServerStatus {
                running: true,
                snapshot,
                principal_id,
                server_control_principal_id: state.server_control_principal_id,
            },
        )
        .await?;
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

    {
        let mut follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        follow_state.connect_client(client_id);
    }
    {
        let mut principals = state
            .client_principals
            .lock()
            .map_err(|_| anyhow::anyhow!("client principal map lock poisoned"))?;
        principals.insert(client_id, client_principal_id);
    }

    // ── Split stream for concurrent read/write ───────────────────────────

    let (mut reader, mut writer) = stream.into_split();

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
        let envelope = match reader.recv_envelope().await {
            Ok(envelope) => envelope,
            Err(IpcTransportError::Io(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(error).context("failed receiving request envelope"),
        };

        let request = match parse_request(&envelope) {
            Ok(request) => request,
            Err(error) => {
                send_error_via_channel(
                    &frame_tx,
                    envelope.request_id,
                    ErrorCode::InvalidRequest,
                    format!("failed parsing request: {error:#}"),
                )?;
                continue;
            }
        };

        // Track whether this request enables event push delivery.
        let is_enable_push = matches!(request, Request::EnableEventPush);

        let request_kind = request_kind_name(&request);
        let exclusive = request_requires_exclusive(&request);
        let request_data = bmux_codec::to_vec(&request).unwrap_or_else(|e| {
            tracing::warn!("failed to serialize request for recording: {e}");
            vec![]
        });
        let started_at = Instant::now();
        debug!(
            client_id = %client_id.0,
            request_id = envelope.request_id,
            request = request_kind,
            exclusive,
            "server.request.start"
        );
        if let Ok(runtime) = state.recording_runtime.lock() {
            let _ = runtime.record(
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
        }

        let response = handle_request(
            &state,
            &shutdown_tx,
            client_id,
            client_principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            request,
        )
        .await?;
        let elapsed_ms = started_at.elapsed().as_millis();
        match &response {
            Response::Ok(payload) => {
                let response_data = bmux_codec::to_vec(payload).unwrap_or_else(|e| {
                    tracing::warn!("failed to serialize response for recording: {e}");
                    vec![]
                });
                debug!(
                    client_id = %client_id.0,
                    request_id = envelope.request_id,
                    request = request_kind,
                    response = response_payload_kind_name(payload),
                    elapsed_ms,
                    "server.request.done"
                );
                if let Ok(runtime) = state.recording_runtime.lock() {
                    let _ = runtime.record(
                        RecordingEventKind::RequestDone,
                        RecordingPayload::RequestDone {
                            request_id: envelope.request_id,
                            request_kind: request_kind.to_string(),
                            response_kind: response_payload_kind_name(payload).to_string(),
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
                }
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
                if let Ok(runtime) = state.recording_runtime.lock() {
                    let _ = runtime.record(
                        RecordingEventKind::RequestError,
                        RecordingPayload::RequestError {
                            request_id: envelope.request_id,
                            request_kind: request_kind.to_string(),
                            error_code: error.code,
                            message: error.message.clone(),
                            elapsed_ms: elapsed_ms.min(u128::from(u64::MAX)) as u64,
                        },
                        RecordMeta {
                            session_id: selected_session.map(|id| id.0),
                            pane_id: None,
                            client_id: Some(client_id.0),
                        },
                    );
                }
            }
        }
        match send_response_via_channel(&frame_tx, envelope.request_id, response) {
            Ok(()) => {}
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
                )?;
            }
            Err(err) => return Err(err),
        }

        // After responding to EnableEventPush, spawn the event push task.
        // It receives events from the broadcast channel and forwards them
        // as serialized frames through the writer channel — no mutex needed.
        if is_enable_push && event_push_task.is_none() {
            let mut event_rx = state.event_broadcast.subscribe();
            let push_frame_tx = frame_tx.clone();
            event_push_task = Some(tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            let Ok(payload) = encode(&event) else {
                                continue;
                            };
                            let envelope = Envelope::new(0, EnvelopeKind::Event, payload);
                            let Ok(frame) = bmux_ipc::frame::encode_frame(&envelope) else {
                                continue;
                            };
                            if push_frame_tx.send(frame).is_err() {
                                return; // writer dropped (client disconnected)
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("event push task lagged by {n} events for client");
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
    mark_snapshot_dirty(&state)?;
    maybe_flush_snapshot(&state, false)?;
    unsubscribe_events(&state, client_id)?;

    Ok(())
}

fn emit_event(state: &Arc<ServerState>, event: Event) -> Result<()> {
    if let Ok(runtime) = state.recording_runtime.lock() {
        let session_id = match &event {
            Event::SessionCreated { id, .. }
            | Event::SessionRemoved { id }
            | Event::ClientAttached { id }
            | Event::ClientDetached { id } => Some(*id),
            Event::FollowTargetChanged { session_id, .. }
            | Event::AttachViewChanged { session_id, .. } => Some(*session_id),
            Event::ServerStarted
            | Event::ServerStopping
            | Event::FollowStarted { .. }
            | Event::FollowStopped { .. }
            | Event::FollowTargetGone { .. }
            | Event::PaneOutputAvailable { .. } => None,
        };
        let _ = runtime.record(
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
    }
    // Broadcast to streaming clients (ignore errors — no receivers is fine).
    let _ = state.event_broadcast.send(event.clone());
    let mut hub = state
        .event_hub
        .lock()
        .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?;
    hub.emit(event);
    Ok(())
}

fn emit_attach_view_changed(
    state: &Arc<ServerState>,
    session_id: SessionId,
    components: &[AttachViewComponent],
) -> Result<()> {
    // Attach view change components are server-normalized before emission so every
    // subscriber receives the same deduplicated canonical ordering. Clients apply
    // components in the received order.
    let components = normalize_attach_view_components(components);
    if components.is_empty() {
        return Ok(());
    }
    let revision = {
        let mut runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
        let Some(revision) = runtime_manager.bump_attach_view_revision(session_id) else {
            return Ok(());
        };
        revision
    };
    emit_event(
        state,
        Event::AttachViewChanged {
            context_id: current_context_id_for_session(state, session_id),
            session_id: session_id.0,
            revision,
            components,
        },
    )
}

fn normalize_attach_view_components(
    components: &[AttachViewComponent],
) -> Vec<AttachViewComponent> {
    // The server owns the canonical attach update ordering. Add new components
    // deliberately here so clients can trust and preserve the emitted sequence.
    let mut normalized = Vec::new();
    for component in [
        AttachViewComponent::Scene,
        AttachViewComponent::SurfaceContent,
        AttachViewComponent::Layout,
        AttachViewComponent::Status,
    ] {
        if components.contains(&component) {
            normalized.push(component);
        }
    }
    normalized
}

fn emit_attach_view_changed_for_pane_close(
    state: &Arc<ServerState>,
    session_id: SessionId,
    session_closed: bool,
) -> Result<()> {
    if session_closed {
        return Ok(());
    }
    emit_attach_view_changed(state, session_id, &[AttachViewComponent::Scene])
}

fn emit_attach_view_changed_for_layout(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> Result<()> {
    emit_attach_view_changed(state, session_id, &[AttachViewComponent::Scene])
}

fn unsubscribe_events(state: &Arc<ServerState>, client_id: ClientId) -> Result<()> {
    let mut hub = state
        .event_hub
        .lock()
        .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?;
    hub.unsubscribe(client_id);
    Ok(())
}

fn sync_selected_target_from_follow_state(
    state: &Arc<ServerState>,
    client_id: ClientId,
    selected_session: &mut Option<SessionId>,
) -> Result<()> {
    let follow_selected = {
        let follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        follow_state.selected_target(client_id)
    };

    if let Some((follow_selected_context, follow_selected_session)) = follow_selected {
        if let Some(context_id) = follow_selected_context {
            let mut context_state = state
                .context_state
                .lock()
                .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
            let _ = context_state.select_for_client(client_id, &ContextSelector::ById(context_id));
        }

        *selected_session = follow_selected_session
            .or_else(|| current_context_session_for_client(state, client_id));
    }
    Ok(())
}

fn reconcile_selected_session_membership(
    state: &Arc<ServerState>,
    client_id: ClientId,
    previous: Option<SessionId>,
    next: Option<SessionId>,
) -> Result<()> {
    if previous == next {
        return Ok(());
    }

    let mut manager = state
        .session_manager
        .lock()
        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;

    if let Some(previous_session) = previous
        && let Some(session) = manager.get_session_mut(&previous_session)
    {
        session.remove_client(&client_id);
    }

    if let Some(next_session) = next
        && let Some(session) = manager.get_session_mut(&next_session)
    {
        session.add_client(client_id);
    }

    Ok(())
}

fn persist_selected_session(
    state: &Arc<ServerState>,
    client_id: ClientId,
    selected_session: Option<SessionId>,
) -> Result<()> {
    let selected_context = current_context_id_for_client(state, client_id);
    let updates = {
        let mut follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        follow_state.set_selected_target(client_id, selected_context, selected_session);
        follow_state.sync_followers_from_leader(client_id, selected_context, selected_session)
    };

    for update in updates {
        let Some(session_id) = update.session_id else {
            continue;
        };
        emit_event(
            state,
            Event::FollowTargetChanged {
                follower_client_id: update.follower_client_id.0,
                leader_client_id: update.leader_client_id.0,
                context_id: update
                    .context_id
                    .or_else(|| current_context_id_for_client(state, update.leader_client_id)),
                session_id: session_id.0,
            },
        )?;
    }

    Ok(())
}

fn disconnect_follow_state(state: &Arc<ServerState>, client_id: ClientId) -> Result<()> {
    let events = {
        let mut follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        follow_state.disconnect_client(client_id)
    };

    for event in events {
        emit_event(state, event)?;
    }

    Ok(())
}

fn clear_selected_session_for_all(
    state: &Arc<ServerState>,
    removed_session_id: SessionId,
) -> Result<()> {
    let mut follow_state = state
        .follow_state
        .lock()
        .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;

    let affected_clients = follow_state
        .selected_sessions
        .iter()
        .filter_map(|(client_id, selected)| {
            (*selected == Some(removed_session_id)).then_some(*client_id)
        })
        .collect::<Vec<_>>();

    for client_id in &affected_clients {
        follow_state.selected_contexts.insert(*client_id, None);
        follow_state.selected_sessions.insert(*client_id, None);
    }
    for client_id in affected_clients {
        let _ = follow_state.sync_followers_from_leader(client_id, None, None);
    }

    Ok(())
}

fn mark_snapshot_dirty(state: &Arc<ServerState>) -> Result<()> {
    let mut runtime = state
        .snapshot_runtime
        .lock()
        .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;
    if runtime.manager.is_some() {
        runtime.dirty = true;
        runtime.last_marked_at = Some(Instant::now());
    }
    Ok(())
}

fn maybe_flush_snapshot(state: &Arc<ServerState>, force: bool) -> Result<()> {
    let manager = {
        let mut runtime = state
            .snapshot_runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;

        let should_flush = if force {
            runtime.dirty
        } else {
            runtime.dirty
                && runtime
                    .last_marked_at
                    .is_some_and(|last| last.elapsed() >= runtime.debounce_interval)
        };

        if !should_flush {
            return Ok(());
        }

        runtime.dirty = false;
        runtime.last_marked_at = None;
        runtime.manager.clone()
    };

    let Some(manager) = manager else {
        return Ok(());
    };

    let snapshot = build_snapshot(state)?;
    if let Err(error) = manager.write_snapshot(&snapshot) {
        warn!("failed writing server snapshot: {error}");
        let mut runtime = state
            .snapshot_runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;
        runtime.dirty = true;
        runtime.last_marked_at = Some(Instant::now());
        runtime.last_restore_error = Some(format!("snapshot write failed: {error}"));
    } else {
        let mut runtime = state
            .snapshot_runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;
        runtime.last_write_epoch_ms = Some(epoch_millis_now());
        runtime.last_restore_error = None;
    }

    Ok(())
}

fn snapshot_status(state: &Arc<ServerState>) -> Result<ServerSnapshotStatus> {
    let runtime = state
        .snapshot_runtime
        .lock()
        .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;

    let path = runtime
        .manager
        .as_ref()
        .map(|manager| manager.path().to_string_lossy().to_string());
    let snapshot_exists = runtime
        .manager
        .as_ref()
        .is_some_and(|manager| manager.path().exists());

    Ok(ServerSnapshotStatus {
        enabled: runtime.manager.is_some(),
        path,
        snapshot_exists,
        last_write_epoch_ms: runtime.last_write_epoch_ms,
        last_restore_epoch_ms: runtime.last_restore_epoch_ms,
        last_restore_error: runtime.last_restore_error.clone(),
    })
}

fn build_snapshot(state: &Arc<ServerState>) -> Result<SnapshotV4> {
    let sessions = {
        let manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
        manager.list_sessions()
    };

    let session_snapshots = {
        let manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
        let runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;

        sessions
            .iter()
            .map(|session_info| {
                let pane_snapshots = runtime_manager
                    .runtimes
                    .get(&session_info.id)
                    .map(|runtime| {
                        validate_runtime_layout_matches_panes(&runtime.layout_root, &runtime.panes)
                            .with_context(|| {
                                format!(
                                    "cannot snapshot inconsistent layout for session {}",
                                    session_info.id.0
                                )
                            })?;

                        let mut pane_ids = Vec::new();
                        runtime.layout_root.pane_order(&mut pane_ids);
                        pane_ids
                            .into_iter()
                            .map(|pane_id| {
                                runtime
                                    .panes
                                    .get(&pane_id)
                                    .map(|pane| PaneSnapshotV2 {
                                        id: pane.meta.id,
                                        name: pane.meta.name.clone(),
                                        shell: pane.meta.shell.clone(),
                                        process_group_id: pane
                                            .process_group_id
                                            .lock()
                                            .ok()
                                            .and_then(|value| *value),
                                    })
                                    .ok_or_else(|| {
                                        anyhow::anyhow!(
                                            "layout references missing pane {} in session {}",
                                            pane_id,
                                            session_info.id.0
                                        )
                                    })
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();

                let name = manager
                    .get_session(&session_info.id)
                    .and_then(|session| session.name.clone());
                let (focused_pane_id, layout_root, floating_surfaces) = runtime_manager
                    .runtimes
                    .get(&session_info.id)
                    .map_or((None, None, Vec::new()), |runtime| {
                        (
                            Some(runtime.focused_pane_id),
                            Some(snapshot_layout_from_runtime(&runtime.layout_root)),
                            runtime
                                .floating_surfaces
                                .iter()
                                .map(|surface| FloatingSurfaceSnapshotV3 {
                                    id: surface.id,
                                    pane_id: surface.pane_id,
                                    x: surface.rect.x,
                                    y: surface.rect.y,
                                    w: surface.rect.w,
                                    h: surface.rect.h,
                                    z: surface.z,
                                    visible: surface.visible,
                                    opaque: surface.opaque,
                                    accepts_input: surface.accepts_input,
                                    cursor_owner: surface.cursor_owner,
                                })
                                .collect(),
                        )
                    });

                Ok(SessionSnapshotV3 {
                    id: session_info.id.0,
                    name,
                    panes: pane_snapshots,
                    focused_pane_id,
                    layout_root,
                    floating_surfaces,
                })
            })
            .collect::<Result<Vec<_>>>()?
    };

    let (follows, selected_sessions) = {
        let follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        let follows = follow_state
            .follows
            .iter()
            .map(|(follower_id, entry)| FollowEdgeSnapshotV2 {
                follower_client_id: follower_id.0,
                leader_client_id: entry.leader_client_id.0,
                global: entry.global,
            })
            .collect::<Vec<_>>();

        let selected_sessions = follow_state
            .selected_sessions
            .iter()
            .map(|(client_id, selected)| ClientSelectedSessionSnapshotV2 {
                client_id: client_id.0,
                session_id: selected.map(|session| session.0),
            })
            .collect::<Vec<_>>();

        (follows, selected_sessions)
    };

    let (contexts, context_session_bindings, selected_contexts, mru_contexts) = {
        let context_state = state
            .context_state
            .lock()
            .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;

        let contexts = context_state
            .contexts
            .values()
            .map(|context| ContextSnapshotV1 {
                id: context.id,
                name: context.name.clone(),
                attributes: context.attributes.clone(),
            })
            .collect::<Vec<_>>();
        let context_session_bindings = context_state
            .session_by_context
            .iter()
            .map(|(context_id, session_id)| ContextSessionBindingSnapshotV1 {
                context_id: *context_id,
                session_id: session_id.0,
            })
            .collect::<Vec<_>>();
        let selected_contexts = context_state
            .selected_by_client
            .iter()
            .map(|(client_id, context_id)| ClientSelectedContextSnapshotV1 {
                client_id: client_id.0,
                context_id: Some(*context_id),
            })
            .collect::<Vec<_>>();
        let mru_contexts = context_state
            .mru_contexts
            .iter()
            .copied()
            .collect::<Vec<_>>();
        (
            contexts,
            context_session_bindings,
            selected_contexts,
            mru_contexts,
        )
    };

    Ok(SnapshotV4 {
        sessions: session_snapshots,
        follows,
        selected_sessions,
        contexts,
        context_session_bindings,
        selected_contexts,
        mru_contexts,
    })
}

fn restore_snapshot_if_present(state: &Arc<ServerState>) -> Result<()> {
    let manager = {
        let runtime = state
            .snapshot_runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;
        runtime.manager.clone()
    };
    let Some(snapshot_manager) = manager else {
        return Ok(());
    };

    if let Err(error) = snapshot_manager.cleanup_temp_file() {
        warn!("failed cleaning stale snapshot temp file: {error}");
    }

    if !snapshot_manager.path().exists() {
        return Ok(());
    }

    let snapshot = match snapshot_manager.read_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            warn!("failed reading snapshot; starting clean: {error}");
            let mut runtime = state
                .snapshot_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;
            runtime.last_restore_error = Some(format!("{error}"));
            return Ok(());
        }
    };

    let _ = apply_snapshot_state(state, &snapshot)?;

    {
        let mut runtime = state
            .snapshot_runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;
        runtime.dirty = false;
        runtime.last_marked_at = None;
        runtime.last_restore_epoch_ms = Some(epoch_millis_now());
        runtime.last_restore_error = None;
    }

    Ok(())
}

fn apply_snapshot_state(state: &Arc<ServerState>, snapshot: &SnapshotV4) -> Result<RestoreSummary> {
    let mut summary = RestoreSummary::default();

    {
        let mut session_manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
        let mut runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;

        for session_snapshot in &snapshot.sessions {
            if session_snapshot.panes.is_empty() {
                warn!(
                    "skipping snapshot session {}: no panes to restore",
                    session_snapshot.id
                );
                continue;
            }

            let session_id = SessionId(session_snapshot.id);
            let mut session = Session::new(session_snapshot.name.clone());
            session.id = session_id;

            if let Err(error) = session_manager.insert_session(session) {
                warn!(
                    "skipping snapshot session {} insertion failure: {error}",
                    session_snapshot.id
                );
                continue;
            }

            let runtime_panes = session_snapshot
                .panes
                .iter()
                .map(|pane| PaneRuntimeMeta {
                    id: pane.id,
                    name: pane.name.clone(),
                    shell: pane.shell.clone(),
                })
                .collect::<Vec<_>>();
            let focused_pane_id = session_snapshot
                .focused_pane_id
                .or_else(|| session_snapshot.panes.first().map(|pane| pane.id))
                .expect("snapshot validation ensures pane exists");
            let floating_surfaces = session_snapshot
                .floating_surfaces
                .iter()
                .map(|surface| FloatingSurfaceRuntime {
                    id: surface.id,
                    pane_id: surface.pane_id,
                    rect: LayoutRect {
                        x: surface.x,
                        y: surface.y,
                        w: surface.w,
                        h: surface.h,
                    },
                    z: surface.z,
                    visible: surface.visible,
                    opaque: surface.opaque,
                    accepts_input: surface.accepts_input,
                    cursor_owner: surface.cursor_owner,
                })
                .collect::<Vec<_>>();

            if let Err(error) = runtime_manager.restore_runtime(
                session_id,
                runtime_panes,
                session_snapshot
                    .layout_root
                    .as_ref()
                    .map(runtime_layout_from_snapshot),
                focused_pane_id,
                floating_surfaces,
            ) {
                warn!(
                    "failed restoring runtime for session {}: {error}",
                    session_snapshot.id
                );
                let _ = session_manager.remove_session(&session_id);
                continue;
            }

            summary.sessions += 1;
        }
    }

    let (selected_contexts, context_for_session) = {
        let session_catalog = {
            let session_manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            session_manager
                .list_sessions()
                .into_iter()
                .map(|session| (session.id.0, session.name))
                .collect::<BTreeMap<_, _>>()
        };

        let binding_by_context = snapshot
            .context_session_bindings
            .iter()
            .map(|binding| (binding.context_id, binding.session_id))
            .collect::<BTreeMap<_, _>>();

        let mut context_state = state
            .context_state
            .lock()
            .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
        context_state.contexts.clear();
        context_state.session_by_context.clear();
        context_state.selected_by_client.clear();
        context_state.mru_contexts.clear();

        for context in &snapshot.contexts {
            let Some(session_id) = binding_by_context.get(&context.id) else {
                continue;
            };
            if !session_catalog.contains_key(session_id) {
                continue;
            }
            context_state.contexts.insert(
                context.id,
                RuntimeContext {
                    id: context.id,
                    name: context.name.clone(),
                    attributes: context.attributes.clone(),
                },
            );
            context_state
                .session_by_context
                .insert(context.id, SessionId(*session_id));
        }

        if context_state.contexts.is_empty() {
            for (session_id, name) in &session_catalog {
                let context_id = *session_id;
                context_state.contexts.insert(
                    context_id,
                    RuntimeContext {
                        id: context_id,
                        name: name.clone(),
                        attributes: BTreeMap::from([(
                            CONTEXT_SESSION_ID_ATTRIBUTE.to_string(),
                            session_id.to_string(),
                        )]),
                    },
                );
                context_state
                    .session_by_context
                    .insert(context_id, SessionId(*session_id));
                context_state.mru_contexts.push_back(context_id);
            }
        } else {
            let mut seen = BTreeSet::new();
            for context_id in &snapshot.mru_contexts {
                if context_state.contexts.contains_key(context_id) && seen.insert(*context_id) {
                    context_state.mru_contexts.push_back(*context_id);
                }
            }
            let context_ids = context_state.contexts.keys().copied().collect::<Vec<_>>();
            for context_id in context_ids {
                if seen.insert(context_id) {
                    context_state.mru_contexts.push_back(context_id);
                }
            }
        }

        for selected in &snapshot.selected_contexts {
            if let Some(context_id) = selected.context_id
                && context_state.contexts.contains_key(&context_id)
            {
                context_state
                    .selected_by_client
                    .insert(ClientId(selected.client_id), context_id);
            }
        }

        (
            context_state
                .selected_by_client
                .iter()
                .map(|(client_id, context_id)| (*client_id, Some(*context_id)))
                .collect::<BTreeMap<_, _>>(),
            context_state
                .session_by_context
                .iter()
                .map(|(context_id, session_id)| (*session_id, *context_id))
                .collect::<BTreeMap<_, _>>(),
        )
    };

    {
        let mut follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        follow_state.follows.clear();
        follow_state.selected_contexts.clear();
        follow_state.selected_sessions.clear();

        let session_manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;

        for selected in &snapshot.selected_sessions {
            let selected_session = selected.session_id.map(SessionId);
            if selected_session
                .is_none_or(|session_id| session_manager.get_session(&session_id).is_some())
            {
                let selected_context = selected_contexts
                    .get(&ClientId(selected.client_id))
                    .copied()
                    .flatten()
                    .or_else(|| {
                        selected_session
                            .and_then(|session_id| context_for_session.get(&session_id).copied())
                    });
                follow_state
                    .selected_contexts
                    .insert(ClientId(selected.client_id), selected_context);
                follow_state
                    .selected_sessions
                    .insert(ClientId(selected.client_id), selected_session);
                summary.selected_sessions += 1;
            }
        }

        for follow in &snapshot.follows {
            follow_state.follows.insert(
                ClientId(follow.follower_client_id),
                FollowEntry {
                    leader_client_id: ClientId(follow.leader_client_id),
                    global: follow.global,
                },
            );
            summary.follows += 1;
        }
    }

    Ok(summary)
}

async fn restore_snapshot_replace(
    state: &Arc<ServerState>,
    snapshot: SnapshotV4,
) -> Result<RestoreSummary> {
    let removed_runtimes = {
        let mut runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
        runtime_manager.remove_all_runtimes()
    };
    for removed_runtime in removed_runtimes {
        shutdown_runtime_handle(removed_runtime).await;
    }

    {
        let mut session_manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
        *session_manager = SessionManager::new();
    }
    {
        let mut follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        follow_state.follows.clear();
        follow_state.selected_contexts.clear();
        follow_state.selected_sessions.clear();
    }
    {
        let mut attach_tokens = state
            .attach_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
        attach_tokens.clear();
    }

    apply_snapshot_state(state, &snapshot)
}

async fn reap_closed_active_pane(
    state: &Arc<ServerState>,
    session_id: SessionId,
    client_id: ClientId,
    selected_session: &mut Option<SessionId>,
    attached_stream_session: &mut Option<SessionId>,
) -> Result<()> {
    let removed_session = {
        let mut runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
        match runtime_manager.close_pane(session_id, Some(PaneSelector::Active)) {
            Ok((_, removed_session)) => removed_session,
            Err(_) => None,
        }
    };

    if let Some(removed_session) = removed_session {
        let removed_session_id = removed_session.session_id;
        if removed_session.had_attached_clients {
            emit_event(
                state,
                Event::ClientDetached {
                    id: removed_session_id.0,
                },
            )?;
        }
        shutdown_runtime_handle(removed_session).await;
        let mut manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
        let _ = manager.remove_session(&removed_session_id);
        drop(manager);
        let _ = prune_context_mappings_for_session(state, removed_session_id)?;
        if *selected_session == Some(removed_session_id) {
            *selected_session = None;
            persist_selected_session(state, client_id, None)?;
        }
        if *attached_stream_session == Some(removed_session_id) {
            *attached_stream_session = None;
        }

        let mut attach_tokens = state
            .attach_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
        attach_tokens.remove_for_session(removed_session_id);
        drop(attach_tokens);

        clear_selected_session_for_all(state, removed_session_id)?;

        emit_event(
            state,
            Event::SessionRemoved {
                id: removed_session_id.0,
            },
        )?;
    } else {
        emit_attach_view_changed_for_pane_close(state, session_id, false)?;
    }

    Ok(())
}

async fn reap_exited_pane(
    state: &Arc<ServerState>,
    session_id: SessionId,
    pane_id: Uuid,
) -> Result<()> {
    let removed_session = {
        let mut runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
        let Some(runtime) = runtime_manager.runtimes.get(&session_id) else {
            return Ok(());
        };
        if runtime.attached_clients.is_empty() {
            return Ok(());
        }
        match runtime_manager.close_pane(session_id, Some(PaneSelector::ById(pane_id))) {
            Ok((_, removed_session)) => removed_session,
            Err(_) => None,
        }
    };

    if let Some(removed_session) = removed_session {
        let removed_session_id = removed_session.session_id;
        if removed_session.had_attached_clients {
            emit_event(
                state,
                Event::ClientDetached {
                    id: removed_session_id.0,
                },
            )?;
        }
        shutdown_runtime_handle(removed_session).await;

        let mut manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
        let _ = manager.remove_session(&removed_session_id);
        drop(manager);
        let _ = prune_context_mappings_for_session(state, removed_session_id)?;

        let mut attach_tokens = state
            .attach_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
        attach_tokens.remove_for_session(removed_session_id);
        drop(attach_tokens);

        clear_selected_session_for_all(state, removed_session_id)?;

        emit_event(
            state,
            Event::SessionRemoved {
                id: removed_session_id.0,
            },
        )?;
    } else {
        emit_attach_view_changed_for_pane_close(state, session_id, false)?;
    }

    Ok(())
}

async fn process_pane_exit_events(state: Arc<ServerState>, mut shutdown_rx: watch::Receiver<bool>) {
    loop {
        let recv_event = async {
            let mut rx = state.pane_exit_rx.lock().await;
            rx.recv().await
        };

        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
                if changed.is_err() {
                    break;
                }
            }
            maybe_event = recv_event => {
                let Some(event) = maybe_event else {
                    break;
                };
                if let Err(error) = reap_exited_pane(&state, event.session_id, event.pane_id).await {
                    warn!("failed reaping exited pane {} in session {}: {error:#}", event.pane_id, event.session_id.0);
                }
            }
        }
    }
}

async fn ensure_attach_session_exists(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> Result<bool> {
    let exists = {
        let manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
        manager.get_session(&session_id).is_some()
    };

    if exists {
        return Ok(true);
    }

    let removed_runtime = {
        let mut runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
        runtime_manager.remove_runtime(session_id).ok()
    };
    if let Some(removed_runtime) = removed_runtime {
        shutdown_runtime_handle(removed_runtime).await;
    }

    let _ = prune_context_mappings_for_session(state, session_id)?;

    Ok(false)
}

async fn handle_request(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    client_id: ClientId,
    client_principal_id: Uuid,
    selected_session: &mut Option<SessionId>,
    attached_stream_session: &mut Option<SessionId>,
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
        Request::Hello { .. } => Response::Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: "hello request is only valid during handshake".to_string(),
        }),
        Request::Ping => Response::Ok(ResponsePayload::Pong),
        Request::WhoAmI => Response::Ok(ResponsePayload::ClientIdentity { id: client_id.0 }),
        Request::WhoAmIPrincipal => Response::Ok(ResponsePayload::PrincipalIdentity {
            principal_id: client_principal_id,
            server_control_principal_id: state.server_control_principal_id,
            force_local_permitted: client_principal_id == state.server_control_principal_id,
        }),
        Request::ServerStatus => {
            let snapshot = snapshot_status(state)?;
            Response::Ok(ResponsePayload::ServerStatus {
                running: true,
                snapshot,
                principal_id: client_principal_id,
                server_control_principal_id: state.server_control_principal_id,
            })
        }
        Request::ServerSave => {
            mark_snapshot_dirty(state)?;
            maybe_flush_snapshot(state, true)?;
            let status = snapshot_status(state)?;
            Response::Ok(ResponsePayload::ServerSnapshotSaved { path: status.path })
        }
        Request::ServerRestoreDryRun => {
            let snapshot_runtime = state
                .snapshot_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;
            let Some(manager) = snapshot_runtime.manager.clone() else {
                return Ok(Response::Ok(ResponsePayload::ServerSnapshotRestoreDryRun {
                    ok: false,
                    message: "snapshot persistence is disabled".to_string(),
                }));
            };
            drop(snapshot_runtime);

            match manager.read_snapshot() {
                Ok(snapshot) => Response::Ok(ResponsePayload::ServerSnapshotRestoreDryRun {
                    ok: true,
                    message: format!(
                        "snapshot is valid (sessions={}, follows={}, selected={})",
                        snapshot.sessions.len(),
                        snapshot.follows.len(),
                        snapshot.selected_sessions.len()
                    ),
                }),
                Err(error) => Response::Ok(ResponsePayload::ServerSnapshotRestoreDryRun {
                    ok: false,
                    message: format!("snapshot dry-run failed: {error}"),
                }),
            }
        }
        Request::ServerRestoreApply => {
            let manager = {
                let snapshot_runtime = state
                    .snapshot_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;
                snapshot_runtime.manager.clone()
            };
            let Some(manager) = manager else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "snapshot persistence is disabled".to_string(),
                }));
            };

            let snapshot = match manager.read_snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: format!("snapshot restore failed: {error}"),
                    }));
                }
            };

            let summary = restore_snapshot_replace(state, snapshot).await?;
            {
                let mut runtime = state
                    .snapshot_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("snapshot runtime lock poisoned"))?;
                runtime.last_restore_epoch_ms = Some(epoch_millis_now());
                runtime.last_restore_error = None;
                runtime.dirty = false;
                runtime.last_marked_at = None;
            }

            Response::Ok(ResponsePayload::ServerSnapshotRestored {
                sessions: summary.sessions,
                follows: summary.follows,
                selected_sessions: summary.selected_sessions,
            })
        }
        Request::ServerStop => {
            let _ = shutdown_tx.send(true);
            Response::Ok(ResponsePayload::ServerStopping)
        }
        Request::InvokeService {
            capability,
            kind,
            interface_id,
            operation,
            payload,
        } => {
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
                selection: Arc::new(AsyncMutex::new((
                    *selected_session,
                    *attached_stream_session,
                ))),
            };
            let dispatch = {
                let registry = state
                    .service_registry
                    .lock()
                    .map_err(|_| anyhow::anyhow!("service registry lock poisoned"))?;
                registry.dispatch(&route, invoke_context.clone(), payload.clone())
            };
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

            if let Some(invocation) = invocation {
                match invocation.await {
                    Ok(payload) => Response::Ok(ResponsePayload::ServiceInvoked { payload }),
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
            }
        }
        Request::NewSession { name } => {
            let session_id = match create_session_runtime(state, name.clone()) {
                Ok(session_id) => session_id,
                Err(error) => {
                    let message = error.to_string();
                    let code = if message.starts_with("session already exists with name") {
                        ErrorCode::AlreadyExists
                    } else {
                        ErrorCode::Internal
                    };
                    return Ok(Response::Err(ErrorResponse { code, message }));
                }
            };

            let context_bind_result = {
                let mut context_state = state
                    .context_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
                let context = context_state.create(client_id, name.clone(), BTreeMap::new());
                match context_state.bind_session(context.id, session_id) {
                    Ok(()) => Ok(()),
                    Err(message) => {
                        let _ = context_state.remove_context_by_id(context.id, Some(client_id));
                        Err(message.to_string())
                    }
                }
            };

            if let Err(message) = context_bind_result {
                let removed_runtime = {
                    let mut manager = state
                        .session_manager
                        .lock()
                        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                    let _ = manager.remove_session(&session_id);
                    drop(manager);

                    let mut runtime_manager = state
                        .session_runtimes
                        .lock()
                        .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                    runtime_manager.remove_runtime(session_id).ok()
                };
                if let Some(removed_runtime) = removed_runtime {
                    shutdown_runtime_handle(removed_runtime).await;
                }
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed creating context for new session: {message}"),
                }));
            }

            Response::Ok(ResponsePayload::SessionCreated {
                id: session_id.0,
                name,
            })
        }
        Request::ListPanes { session } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let session_id =
                match resolve_session_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                };
            drop(manager);

            let runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            let panes = match runtime_manager.list_panes(session_id) {
                Ok(panes) => panes,
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: format!("failed listing panes: {error:#}"),
                    }));
                }
            };
            Response::Ok(ResponsePayload::PaneList { panes })
        }
        Request::SplitPane {
            session,
            target,
            direction,
            ratio_pct: _, // TODO: pass to layout system when supported
        } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                match resolve_session_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                }
            };
            if let Err(response) = ensure_session_mutation_allowed(
                state,
                shutdown_tx,
                session_id,
                client_id,
                client_principal_id,
                "pane.split",
            )
            .await
            {
                return Ok(Response::Err(response));
            }
            let mut runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            let pane_id = match runtime_manager.split_pane(session_id, target, direction) {
                Ok(id) => id,
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: format!("failed splitting pane: {error:#}"),
                    }));
                }
            };
            drop(runtime_manager);
            emit_attach_view_changed_for_layout(state, session_id)?;
            Response::Ok(ResponsePayload::PaneSplit {
                id: pane_id,
                session_id: session_id.0,
            })
        }
        Request::FocusPane {
            session,
            target,
            direction,
        } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                match resolve_session_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                }
            };
            if let Err(response) = ensure_session_mutation_allowed(
                state,
                shutdown_tx,
                session_id,
                client_id,
                client_principal_id,
                "pane.focus",
            )
            .await
            {
                return Ok(Response::Err(response));
            }
            let mut runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            let pane_id = match (target, direction) {
                (Some(_), Some(_)) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: "focus-pane cannot use target and direction together".to_string(),
                    }));
                }
                (Some(target), None) => runtime_manager.focus_pane_target(session_id, target),
                (None, Some(direction)) => runtime_manager.focus_pane(session_id, direction),
                (None, None) => runtime_manager.focus_pane_target(session_id, PaneSelector::Active),
            };
            let pane_id = match pane_id {
                Ok(id) => id,
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: format!("failed focusing pane: {error:#}"),
                    }));
                }
            };
            drop(runtime_manager);
            emit_attach_view_changed_for_layout(state, session_id)?;
            Response::Ok(ResponsePayload::PaneFocused {
                id: pane_id,
                session_id: session_id.0,
            })
        }
        Request::ResizePane {
            session,
            target,
            delta,
        } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                match resolve_session_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                }
            };
            if let Err(response) = ensure_session_mutation_allowed(
                state,
                shutdown_tx,
                session_id,
                client_id,
                client_principal_id,
                "pane.resize",
            )
            .await
            {
                return Ok(Response::Err(response));
            }
            let mut runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            if let Err(error) = runtime_manager.resize_pane(session_id, target, delta) {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("failed resizing pane: {error:#}"),
                }));
            }
            drop(runtime_manager);
            emit_attach_view_changed_for_layout(state, session_id)?;
            Response::Ok(ResponsePayload::PaneResized {
                session_id: session_id.0,
            })
        }
        Request::ClosePane { session, target } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                match resolve_session_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                }
            };
            if let Err(response) = ensure_session_mutation_allowed(
                state,
                shutdown_tx,
                session_id,
                client_id,
                client_principal_id,
                "pane.close",
            )
            .await
            {
                return Ok(Response::Err(response));
            }

            let (closed_pane_id, removed_session) = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                let (closed_pane_id, removed_session) = runtime_manager
                    .close_pane(session_id, target)
                    .map_err(|error| anyhow::anyhow!("failed closing pane: {error:#}"))?;
                (closed_pane_id, removed_session)
            };

            let mut session_closed = false;
            if let Some(removed_session) = removed_session {
                session_closed = true;
                let removed_session_id = removed_session.session_id;
                if removed_session.had_attached_clients {
                    emit_event(
                        state,
                        Event::ClientDetached {
                            id: removed_session_id.0,
                        },
                    )?;
                }
                shutdown_runtime_handle(removed_session).await;
                let mut manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                let _ = manager.remove_session(&removed_session_id);
                drop(manager);
                let _ = prune_context_mappings_for_session(state, removed_session_id)?;
                if *selected_session == Some(removed_session_id) {
                    *selected_session = None;
                    persist_selected_session(state, client_id, None)?;
                }
                if *attached_stream_session == Some(removed_session_id) {
                    *attached_stream_session = None;
                }

                let mut attach_tokens = state
                    .attach_tokens
                    .lock()
                    .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
                attach_tokens.remove_for_session(removed_session_id);
                drop(attach_tokens);

                clear_selected_session_for_all(state, removed_session_id)?;

                emit_event(
                    state,
                    Event::SessionRemoved {
                        id: removed_session_id.0,
                    },
                )?;
            }

            emit_attach_view_changed_for_pane_close(state, session_id, session_closed)?;

            Response::Ok(ResponsePayload::PaneClosed {
                id: closed_pane_id,
                session_id: session_id.0,
                session_closed,
            })
        }
        Request::ZoomPane { session } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                match resolve_session_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                }
            };
            if let Err(response) = ensure_session_mutation_allowed(
                state,
                shutdown_tx,
                session_id,
                client_id,
                client_principal_id,
                "pane.zoom",
            )
            .await
            {
                return Ok(Response::Err(response));
            }
            let mut runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            let (pane_id, zoomed) = match runtime_manager.toggle_zoom(session_id) {
                Ok(result) => result,
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: format!("failed toggling zoom: {error:#}"),
                    }));
                }
            };
            drop(runtime_manager);
            emit_attach_view_changed_for_layout(state, session_id)?;
            Response::Ok(ResponsePayload::PaneZoomed {
                session_id: session_id.0,
                pane_id,
                zoomed,
            })
        }
        Request::ListSessions => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let sessions = manager
                .list_sessions()
                .into_iter()
                .map(|session| SessionSummary {
                    id: session.id.0,
                    name: session.name,
                    client_count: session.client_count,
                })
                .collect::<Vec<_>>();
            Response::Ok(ResponsePayload::SessionList { sessions })
        }
        Request::ListClients => {
            let follow_state = state
                .follow_state
                .lock()
                .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
            let mut clients = follow_state.list_clients();
            drop(follow_state);
            for client in &mut clients {
                client.selected_context_id =
                    current_context_id_for_client(state, ClientId(client.id));
            }
            Response::Ok(ResponsePayload::ClientList { clients })
        }
        Request::CreateContext { name, attributes } => {
            let session_id = match create_session_runtime(state, name.clone()) {
                Ok(session_id) => session_id,
                Err(error) => {
                    let message = error.to_string();
                    let code = if message.starts_with("session already exists with name") {
                        ErrorCode::AlreadyExists
                    } else {
                        ErrorCode::Internal
                    };
                    return Ok(Response::Err(ErrorResponse { code, message }));
                }
            };

            let mut context_state = state
                .context_state
                .lock()
                .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
            let context = context_state.create(client_id, name, attributes);
            if let Err(message) = context_state.bind_session(context.id, session_id) {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: message.to_string(),
                }));
            }
            Response::Ok(ResponsePayload::ContextCreated { context })
        }
        Request::ListContexts => {
            let context_state = state
                .context_state
                .lock()
                .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
            let contexts = context_state.list();
            Response::Ok(ResponsePayload::ContextList { contexts })
        }
        Request::SelectContext { selector } => {
            let selection_result = {
                let mut context_state = state
                    .context_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
                match context_state.select_for_client(client_id, &selector) {
                    Ok(context) => {
                        let session_id = context_state.current_session_for_client(client_id);
                        Ok((context, session_id))
                    }
                    Err(message) => Err(message),
                }
            };

            match selection_result {
                Ok((context, Some(session_id))) => {
                    let previous_selected = *selected_session;
                    *selected_session = Some(session_id);
                    reconcile_selected_session_membership(
                        state,
                        client_id,
                        previous_selected,
                        *selected_session,
                    )?;
                    persist_selected_session(state, client_id, *selected_session)?;
                    Response::Ok(ResponsePayload::ContextSelected { context })
                }
                Ok((context, None)) => Response::Ok(ResponsePayload::ContextSelected { context }),
                Err(message) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: message.to_string(),
                }),
            }
        }
        Request::CloseContext { selector, force } => {
            let close_result = {
                let mut context_state = state
                    .context_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
                context_state.close(client_id, &selector, force)
            };
            match close_result {
                Ok((id, session_id)) => {
                    if let Some(session_id) = session_id {
                        let removed_runtime = {
                            let mut manager = state
                                .session_manager
                                .lock()
                                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                            let _ = manager.remove_session(&session_id);
                            if *selected_session == Some(session_id) {
                                *selected_session = None;
                                persist_selected_session(state, client_id, None)?;
                            }
                            if *attached_stream_session == Some(session_id) {
                                *attached_stream_session = None;
                            }
                            drop(manager);

                            let mut runtime_manager =
                                state.session_runtimes.lock().map_err(|_| {
                                    anyhow::anyhow!("session runtime manager lock poisoned")
                                })?;
                            runtime_manager.remove_runtime(session_id).ok()
                        };

                        if let Some(removed_runtime) = removed_runtime {
                            if removed_runtime.had_attached_clients {
                                emit_event(state, Event::ClientDetached { id: session_id.0 })?;
                            }
                            tokio::spawn(async move {
                                shutdown_runtime_handle(removed_runtime).await;
                            });
                        }

                        let mut attach_tokens = state
                            .attach_tokens
                            .lock()
                            .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
                        attach_tokens.remove_for_session(session_id);
                        drop(attach_tokens);

                        clear_selected_session_for_all(state, session_id)?;
                        emit_event(state, Event::SessionRemoved { id: session_id.0 })?;
                    }

                    Response::Ok(ResponsePayload::ContextClosed { id })
                }
                Err(message) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: message.to_string(),
                }),
            }
        }
        Request::CurrentContext => {
            let context_state = state
                .context_state
                .lock()
                .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
            let context = context_state.current_for_client(client_id);
            Response::Ok(ResponsePayload::CurrentContext { context })
        }
        Request::KillSession {
            selector,
            force_local,
        } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                let Some(session_id) = resolve_session_id(&manager, &selector) else {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: session_not_found_message(&selector),
                    }));
                };

                session_id
            };

            if force_local && client_principal_id != state.server_control_principal_id {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "force-local is only allowed for the server control principal"
                        .to_string(),
                }));
            }

            if !force_local
                && let Err(response) = ensure_session_admin_allowed(
                    state,
                    shutdown_tx,
                    session_id,
                    client_id,
                    client_principal_id,
                    "session.kill",
                )
                .await
            {
                return Ok(Response::Err(response));
            }

            let removed_runtime = {
                let mut manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                if manager.remove_session(&session_id).is_err() {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: format!("failed removing session {}", session_id.0),
                    }));
                }
                drop(manager);
                let _ = prune_context_mappings_for_session(state, session_id)?;
                if *selected_session == Some(session_id) {
                    *selected_session = None;
                    persist_selected_session(state, client_id, None)?;
                }
                if *attached_stream_session == Some(session_id) {
                    *attached_stream_session = None;
                }

                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                match runtime_manager.remove_runtime(session_id) {
                    Ok(removed) => removed,
                    Err(error) => {
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::Internal,
                            message: format!("failed stopping session runtime: {error:#}"),
                        }));
                    }
                }
            };

            if removed_runtime.had_attached_clients {
                emit_event(state, Event::ClientDetached { id: session_id.0 })?;
            }
            shutdown_runtime_handle(removed_runtime).await;

            let mut attach_tokens = state
                .attach_tokens
                .lock()
                .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
            attach_tokens.remove_for_session(session_id);
            drop(attach_tokens);

            clear_selected_session_for_all(state, session_id)?;

            emit_event(state, Event::SessionRemoved { id: session_id.0 })?;

            Response::Ok(ResponsePayload::SessionKilled { id: session_id.0 })
        }
        Request::FollowClient {
            target_client_id,
            global,
        } => {
            let leader_client_id = ClientId(target_client_id);
            let (initial_target_context, initial_target_session) = {
                let mut follow_state = state
                    .follow_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
                match follow_state.start_follow(client_id, leader_client_id, global) {
                    Ok(initial) => initial,
                    Err(reason) => {
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::InvalidRequest,
                            message: reason.to_string(),
                        }));
                    }
                }
            };

            if global {
                let previous_selected = *selected_session;
                *selected_session = initial_target_session;
                reconcile_selected_session_membership(
                    state,
                    client_id,
                    previous_selected,
                    *selected_session,
                )?;

                if let Some(initial_target_context) = initial_target_context {
                    let mut context_state = state
                        .context_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
                    let _ = context_state.select_for_client(
                        client_id,
                        &ContextSelector::ById(initial_target_context),
                    );
                }
            }

            emit_event(
                state,
                Event::FollowStarted {
                    follower_client_id: client_id.0,
                    leader_client_id: leader_client_id.0,
                    global,
                },
            )?;

            if let Some(session_id) = initial_target_session {
                emit_event(
                    state,
                    Event::FollowTargetChanged {
                        follower_client_id: client_id.0,
                        leader_client_id: leader_client_id.0,
                        context_id: initial_target_context
                            .or_else(|| current_context_id_for_client(state, leader_client_id)),
                        session_id: session_id.0,
                    },
                )?;
            }

            Response::Ok(ResponsePayload::FollowStarted {
                follower_client_id: client_id.0,
                leader_client_id: leader_client_id.0,
                global,
            })
        }
        Request::Unfollow => {
            let removed = {
                let mut follow_state = state
                    .follow_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
                follow_state.stop_follow(client_id)
            };

            if removed {
                emit_event(
                    state,
                    Event::FollowStopped {
                        follower_client_id: client_id.0,
                    },
                )?;
            }

            Response::Ok(ResponsePayload::FollowStopped {
                follower_client_id: client_id.0,
            })
        }
        Request::Attach { selector } => {
            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let Some(next_session_id) = resolve_session_id(&manager, &selector) else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: session_not_found_message(&selector),
                }));
            };

            if let Some(previous_session_id) = selected_session.take()
                && previous_session_id != next_session_id
                && let Some(previous) = manager.get_session_mut(&previous_session_id)
            {
                previous.remove_client(&client_id);
            }

            if let Some(session) = manager.get_session_mut(&next_session_id) {
                session.add_client(client_id);
                *selected_session = Some(next_session_id);
                persist_selected_session(state, client_id, *selected_session)?;
                drop(manager);

                let mut attach_tokens = state
                    .attach_tokens
                    .lock()
                    .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
                let mut grant = attach_tokens.issue(next_session_id);
                // Prefer the context that maps to the target session so that
                // the client's first `refresh_attached_session_from_context`
                // does not resolve a stale MRU context belonging to a
                // different session (which would cause a session-id mismatch
                // and "client is not attached to session runtime" errors).
                grant.context_id = current_context_id_for_session(state, next_session_id)
                    .or_else(|| current_context_id_for_client(state, client_id));
                Response::Ok(ResponsePayload::Attached { grant })
            } else {
                drop(manager);
                let _ = prune_context_mappings_for_session(state, next_session_id)?;
                Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session not found: {}", next_session_id.0),
                })
            }
        }
        Request::AttachContext { selector } => {
            let (selected_context_id, next_session_id) = {
                let mut context_state = state
                    .context_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
                let context = match context_state.select_for_client(client_id, &selector) {
                    Ok(context) => context,
                    Err(message) => {
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::NotFound,
                            message: message.to_string(),
                        }));
                    }
                };

                let Some(session_id) = context_state.current_session_for_client(client_id) else {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: "context has no attached runtime".to_string(),
                    }));
                };

                (context.id, session_id)
            };

            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            if let Some(previous_session_id) = selected_session.take()
                && previous_session_id != next_session_id
                && let Some(previous) = manager.get_session_mut(&previous_session_id)
            {
                previous.remove_client(&client_id);
            }

            if let Some(session) = manager.get_session_mut(&next_session_id) {
                session.add_client(client_id);
                *selected_session = Some(next_session_id);
                persist_selected_session(state, client_id, *selected_session)?;
                drop(manager);

                let mut attach_tokens = state
                    .attach_tokens
                    .lock()
                    .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
                let mut grant = attach_tokens.issue(next_session_id);
                grant.context_id = Some(selected_context_id);
                Response::Ok(ResponsePayload::Attached { grant })
            } else {
                drop(manager);
                let _ = prune_context_mappings_for_session(state, next_session_id)?;
                Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session not found: {}", next_session_id.0),
                })
            }
        }
        Request::AttachOpen {
            session_id,
            attach_token,
        } => {
            let session_id = SessionId(session_id);

            let session_exists = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                manager.get_session(&session_id).is_some()
            };
            if !session_exists {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session not found: {}", session_id.0),
                }));
            }

            let consumed = {
                let mut attach_tokens = state
                    .attach_tokens
                    .lock()
                    .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
                attach_tokens.consume(session_id, attach_token)
            };

            match consumed {
                Ok(()) => {
                    let can_write = true;

                    let begin_result = {
                        let mut runtime_manager = state.session_runtimes.lock().map_err(|_| {
                            anyhow::anyhow!("session runtime manager lock poisoned")
                        })?;
                        if let Some(previous_stream_session) = *attached_stream_session
                            && previous_stream_session != session_id
                        {
                            runtime_manager.end_attach(previous_stream_session, client_id);
                            emit_event(
                                state,
                                Event::ClientDetached {
                                    id: previous_stream_session.0,
                                },
                            )?;
                        }
                        runtime_manager.begin_attach(session_id, client_id)
                    };

                    match begin_result {
                        Ok(()) => {
                            *attached_stream_session = Some(session_id);
                            let context_id = current_context_id_for_client(state, client_id);
                            Response::Ok(ResponsePayload::AttachReady {
                                context_id,
                                session_id: session_id.0,
                                can_write,
                            })
                        }
                        Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                            code: ErrorCode::NotFound,
                            message: format!("session runtime not found: {}", session_id.0),
                        }),
                        Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                            code: ErrorCode::Internal,
                            message: "failed opening attach stream".to_string(),
                        }),
                        Err(SessionRuntimeError::Closed) => {
                            let _ = reap_closed_active_pane(
                                state,
                                session_id,
                                client_id,
                                selected_session,
                                attached_stream_session,
                            )
                            .await;
                            Response::Err(ErrorResponse {
                                code: ErrorCode::NotFound,
                                message: format!("session runtime not found: {}", session_id.0),
                            })
                        }
                    }
                }
                Err(AttachTokenValidationError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: "attach token not found".to_string(),
                }),
                Err(AttachTokenValidationError::Expired) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "attach token expired".to_string(),
                }),
                Err(AttachTokenValidationError::SessionMismatch) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "attach token does not match requested session".to_string(),
                }),
            }
        }
        Request::AttachInput { session_id, data } => {
            let session_id = SessionId(session_id);
            if !ensure_attach_session_exists(state, session_id).await? {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }));
            }
            if let Err(response) = ensure_session_mutation_allowed(
                state,
                shutdown_tx,
                session_id,
                client_id,
                client_principal_id,
                "attach.input",
            )
            .await
            {
                return Ok(Response::Err(response));
            }
            let captured_input = data.clone();
            let write_result = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.write_input(session_id, client_id, data)
            };
            match write_result {
                Ok((bytes, focused_pane_id)) => {
                    if let Ok(runtime) = state.recording_runtime.lock() {
                        let _ = runtime.record(
                            RecordingEventKind::PaneInputRaw,
                            RecordingPayload::Bytes {
                                data: captured_input,
                            },
                            RecordMeta {
                                session_id: Some(session_id.0),
                                pane_id: Some(focused_pane_id),
                                client_id: Some(client_id.0),
                            },
                        );
                    }
                    Response::Ok(ResponsePayload::AttachInputAccepted { bytes })
                }
                Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }),
                Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "client is not attached to session runtime".to_string(),
                }),
                Err(SessionRuntimeError::Closed) => {
                    let _ = reap_closed_active_pane(
                        state,
                        session_id,
                        client_id,
                        selected_session,
                        attached_stream_session,
                    )
                    .await;
                    Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: "active pane is closed".to_string(),
                    })
                }
            }
        }
        Request::AttachSetViewport {
            session_id,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
        } => {
            let session_id = SessionId(session_id);
            if !ensure_attach_session_exists(state, session_id).await? {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }));
            }

            let update_result = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.set_attach_viewport(
                    session_id,
                    client_id,
                    cols,
                    rows,
                    status_top_inset,
                    status_bottom_inset,
                )
            };

            match update_result {
                Ok((cols, rows, status_top_inset, status_bottom_inset)) => {
                    Response::Ok(ResponsePayload::AttachViewportSet {
                        context_id: current_context_id_for_client(state, client_id),
                        session_id: session_id.0,
                        cols,
                        rows,
                        status_top_inset,
                        status_bottom_inset,
                    })
                }
                Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }),
                Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "client is not attached to session runtime".to_string(),
                }),
                Err(SessionRuntimeError::Closed) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: "active pane is closed".to_string(),
                }),
            }
        }
        Request::AttachOutput {
            session_id,
            max_bytes,
        } => {
            let session_id = SessionId(session_id);
            if !ensure_attach_session_exists(state, session_id).await? {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }));
            }
            let read_result = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.read_output(session_id, client_id, max_bytes)
            };
            match read_result {
                Ok(data) => Response::Ok(ResponsePayload::AttachOutput { data }),
                Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }),
                Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "client is not attached to session runtime".to_string(),
                }),
                Err(SessionRuntimeError::Closed) => {
                    let _ = reap_closed_active_pane(
                        state,
                        session_id,
                        client_id,
                        selected_session,
                        attached_stream_session,
                    )
                    .await;
                    Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: "active pane is closed".to_string(),
                    })
                }
            }
        }
        Request::AttachLayout { session_id } => {
            let session_id = SessionId(session_id);
            if !ensure_attach_session_exists(state, session_id).await? {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }));
            }
            let active_pane_exited = {
                let runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.active_pane_exited(session_id, client_id)
            };
            match active_pane_exited {
                Ok(true) => {
                    let _ = reap_closed_active_pane(
                        state,
                        session_id,
                        client_id,
                        selected_session,
                        attached_stream_session,
                    )
                    .await;
                }
                Ok(false) => {}
                Err(SessionRuntimeError::NotFound) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: format!("session runtime not found: {}", session_id.0),
                    }));
                }
                Err(SessionRuntimeError::NotAttached) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: "client is not attached to session runtime".to_string(),
                    }));
                }
                Err(SessionRuntimeError::Closed) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: "active pane is closed".to_string(),
                    }));
                }
            }
            let state_snapshot = {
                let runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.attach_layout_state(session_id, client_id)
            };
            match state_snapshot {
                Ok(snapshot) => Response::Ok(ResponsePayload::AttachLayout {
                    context_id: current_context_id_for_client(state, client_id),
                    session_id: session_id.0,
                    focused_pane_id: snapshot.focused_pane_id,
                    panes: snapshot.panes,
                    layout_root: snapshot.layout_root,
                    scene: snapshot.scene,
                    zoomed: snapshot.zoomed,
                }),
                Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }),
                Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "client is not attached to session runtime".to_string(),
                }),
                Err(SessionRuntimeError::Closed) => {
                    let _ = reap_closed_active_pane(
                        state,
                        session_id,
                        client_id,
                        selected_session,
                        attached_stream_session,
                    )
                    .await;
                    Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: "active pane is closed".to_string(),
                    })
                }
            }
        }
        Request::AttachSnapshot {
            session_id,
            max_bytes_per_pane,
        } => {
            let session_id = SessionId(session_id);
            if !ensure_attach_session_exists(state, session_id).await? {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }));
            }

            let snapshot = {
                let runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.attach_snapshot_state(session_id, client_id, max_bytes_per_pane)
            };

            match snapshot {
                Ok(snapshot) => Response::Ok(ResponsePayload::AttachSnapshot {
                    context_id: current_context_id_for_client(state, client_id),
                    session_id: snapshot.session_id.0,
                    focused_pane_id: snapshot.focused_pane_id,
                    panes: snapshot.panes,
                    layout_root: snapshot.layout_root,
                    scene: snapshot.scene,
                    chunks: snapshot.chunks,
                    zoomed: snapshot.zoomed,
                }),
                Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }),
                Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "client is not attached to session runtime".to_string(),
                }),
                Err(SessionRuntimeError::Closed) => {
                    let _ = reap_closed_active_pane(
                        state,
                        session_id,
                        client_id,
                        selected_session,
                        attached_stream_session,
                    )
                    .await;
                    Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: "active pane is closed".to_string(),
                    })
                }
            }
        }
        Request::AttachPaneOutputBatch {
            session_id,
            pane_ids,
            max_bytes,
        } => {
            let session_id = SessionId(session_id);
            if !ensure_attach_session_exists(state, session_id).await? {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }));
            }
            let chunks = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                // Clear output_dirty flags for requested panes so the PTY reader
                // can re-notify on the next chunk of output.
                if let Some(runtime) = runtime_manager.runtimes.get(&session_id) {
                    for pane_id in &pane_ids {
                        if let Some(pane) = runtime.panes.get(pane_id) {
                            pane.output_dirty.store(false, Ordering::SeqCst);
                        }
                    }
                }
                runtime_manager.read_pane_output_batch(session_id, client_id, &pane_ids, max_bytes)
            };
            match chunks {
                Ok(chunks) => Response::Ok(ResponsePayload::AttachPaneOutputBatch { chunks }),
                Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }),
                Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "client is not attached to session runtime".to_string(),
                }),
                Err(SessionRuntimeError::Closed) => {
                    let _ = reap_closed_active_pane(
                        state,
                        session_id,
                        client_id,
                        selected_session,
                        attached_stream_session,
                    )
                    .await;
                    Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: "active pane is closed".to_string(),
                    })
                }
            }
        }
        Request::Detach => {
            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let previous_selected_session = selected_session.take();
            if let Some(current_selected_session) = previous_selected_session
                && let Some(session) = manager.get_session_mut(&current_selected_session)
            {
                session.remove_client(&client_id);
            }
            drop(manager);

            if let Some(current_stream_session) = attached_stream_session.take() {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.end_attach(current_stream_session, client_id);
                emit_event(
                    state,
                    Event::ClientDetached {
                        id: current_stream_session.0,
                    },
                )?;
            }
            persist_selected_session(state, client_id, None)?;
            Response::Ok(ResponsePayload::Detached)
        }
        Request::SubscribeEvents => {
            let mut hub = state
                .event_hub
                .lock()
                .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?;
            hub.subscribe(client_id);
            Response::Ok(ResponsePayload::EventsSubscribed)
        }
        Request::PollEvents { max_events } => {
            let mut hub = state
                .event_hub
                .lock()
                .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?;
            match hub.poll(client_id, max_events) {
                Some(events) => Response::Ok(ResponsePayload::EventBatch { events }),
                None => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "event subscription not found for client".to_string(),
                }),
            }
        }
        // EnableEventPush is handled in handle_connection after the response
        // is sent — the actual push task spawning happens there. Here we just
        // acknowledge the request.
        Request::EnableEventPush => Response::Ok(ResponsePayload::EventPushEnabled),
        Request::RecordingStart {
            session_id,
            capture_input,
            profile,
            event_kinds,
        } => {
            let mut runtime = state
                .recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            let profile = profile.unwrap_or(RecordingProfile::Functional);
            let event_kinds = event_kinds
                .unwrap_or_else(|| default_recording_event_kinds(profile, capture_input));
            match runtime.start(session_id, capture_input, profile, event_kinds) {
                Ok(recording) => Response::Ok(ResponsePayload::RecordingStarted { recording }),
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: format!("failed starting recording: {error}"),
                }),
            }
        }
        Request::RecordingStop { recording_id } => {
            let mut runtime = state
                .recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            match runtime.stop(recording_id) {
                Ok(recording) => Response::Ok(ResponsePayload::RecordingStopped {
                    recording_id: recording.id,
                }),
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: format!("failed stopping recording: {error}"),
                }),
            }
        }
        Request::RecordingStatus => {
            let runtime = state
                .recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            Response::Ok(ResponsePayload::RecordingStatus {
                status: runtime.status(),
            })
        }
        Request::RecordingList => {
            let runtime = state
                .recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            match runtime.list() {
                Ok(recordings) => Response::Ok(ResponsePayload::RecordingList { recordings }),
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed listing recordings: {error}"),
                }),
            }
        }
        Request::RecordingDelete { recording_id } => {
            let mut runtime = state
                .recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            match runtime.delete(recording_id) {
                Ok(recording) => Response::Ok(ResponsePayload::RecordingDeleted {
                    recording_id: recording.id,
                }),
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: format!("failed deleting recording: {error}"),
                }),
            }
        }
        Request::RecordingWriteCustomEvent {
            session_id,
            pane_id,
            source,
            name,
            payload,
        } => {
            let runtime = state
                .recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            match runtime.record(
                RecordingEventKind::Custom,
                RecordingPayload::Custom {
                    source,
                    name,
                    payload,
                },
                RecordMeta {
                    session_id,
                    pane_id,
                    client_id: Some(client_id.0),
                },
            ) {
                Ok(accepted) => {
                    Response::Ok(ResponsePayload::RecordingCustomEventWritten { accepted })
                }
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed writing custom recording event: {error}"),
                }),
            }
        }
        Request::RecordingDeleteAll => {
            let mut runtime = state
                .recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            match runtime.delete_all() {
                Ok(deleted_count) => {
                    Response::Ok(ResponsePayload::RecordingDeleteAll { deleted_count })
                }
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed deleting all recordings: {error}"),
                }),
            }
        }
        Request::RecordingPrune { older_than_days } => {
            let result = {
                let runtime = state
                    .recording_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
                runtime.prune(older_than_days)
            };
            match result {
                Ok(deleted_count) => {
                    Response::Ok(ResponsePayload::RecordingPruned { deleted_count })
                }
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed pruning recordings: {error}"),
                }),
            }
        }
        Request::PaneDirectInput {
            session_id,
            pane_id,
            data,
        } => {
            let session_id = SessionId(session_id);
            if !ensure_attach_session_exists(state, session_id).await? {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }));
            }
            if let Err(response) = ensure_session_mutation_allowed(
                state,
                shutdown_tx,
                session_id,
                client_id,
                client_principal_id,
                "pane.direct_input",
            )
            .await
            {
                return Ok(Response::Err(response));
            }
            let captured_input = data.clone();
            let write_result = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.write_input_to_pane(session_id, pane_id, data)
            };
            match write_result {
                Ok(bytes) => {
                    if let Ok(runtime) = state.recording_runtime.lock() {
                        let _ = runtime.record(
                            RecordingEventKind::PaneInputRaw,
                            RecordingPayload::Bytes {
                                data: captured_input,
                            },
                            RecordMeta {
                                session_id: Some(session_id.0),
                                pane_id: Some(pane_id),
                                client_id: Some(client_id.0),
                            },
                        );
                    }
                    Response::Ok(ResponsePayload::PaneDirectInputAccepted { bytes, pane_id })
                }
                Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!(
                        "session or pane not found: session={}, pane={}",
                        session_id.0, pane_id
                    ),
                }),
                Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "client is not attached to session runtime".to_string(),
                }),
                Err(SessionRuntimeError::Closed) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("pane is closed: {}", pane_id),
                }),
            }
        }
    };

    if let Response::Ok(ResponsePayload::SessionCreated { id, name }) = &response {
        emit_event(
            state,
            Event::SessionCreated {
                id: *id,
                name: name.clone(),
            },
        )?;
    }
    if let Response::Ok(ResponsePayload::AttachReady { session_id, .. }) = &response {
        emit_event(state, Event::ClientAttached { id: *session_id })?;
    }

    if response_requires_snapshot(&response) {
        mark_snapshot_dirty(state)?;
        maybe_flush_snapshot(state, false)?;
    }

    Ok(response)
}

const fn request_requires_exclusive(request: &Request) -> bool {
    matches!(
        request,
        Request::ServerSave
            | Request::ServerStop
            | Request::ServerRestoreApply
            | Request::NewSession { .. }
            | Request::CreateContext { .. }
            | Request::SelectContext { .. }
            | Request::CloseContext { .. }
            | Request::KillSession { .. }
            | Request::SplitPane { .. }
            | Request::FocusPane { .. }
            | Request::ResizePane { .. }
            | Request::ClosePane { .. }
            | Request::ZoomPane { .. }
            | Request::FollowClient { .. }
            | Request::Unfollow
            | Request::Attach { .. }
            | Request::AttachContext { .. }
            | Request::AttachOpen { .. }
            | Request::AttachInput { .. }
            | Request::AttachSetViewport { .. }
            | Request::PaneDirectInput { .. }
            | Request::RecordingStart { .. }
            | Request::RecordingStop { .. }
            | Request::RecordingDelete { .. }
            | Request::RecordingWriteCustomEvent { .. }
            | Request::RecordingDeleteAll
            | Request::RecordingPrune { .. }
            | Request::Detach
    )
}

const fn response_requires_snapshot(response: &Response) -> bool {
    matches!(
        response,
        Response::Ok(
            ResponsePayload::SessionCreated { .. }
                | ResponsePayload::ContextCreated { .. }
                | ResponsePayload::ContextSelected { .. }
                | ResponsePayload::ContextClosed { .. }
                | ResponsePayload::PaneSplit { .. }
                | ResponsePayload::PaneFocused { .. }
                | ResponsePayload::PaneResized { .. }
                | ResponsePayload::PaneClosed { .. }
                | ResponsePayload::SessionKilled { .. }
                | ResponsePayload::FollowStarted { .. }
                | ResponsePayload::FollowStopped { .. }
                | ResponsePayload::Attached { .. }
                | ResponsePayload::Detached
        )
    )
}

const fn request_kind_name(request: &Request) -> &'static str {
    match request {
        Request::Hello { .. } => "hello",
        Request::Ping => "ping",
        Request::WhoAmI => "whoami",
        Request::WhoAmIPrincipal => "whoami_principal",
        Request::ServerStatus => "server_status",
        Request::ServerSave => "server_save",
        Request::ServerRestoreDryRun => "server_restore_dry_run",
        Request::ServerRestoreApply => "server_restore_apply",
        Request::ServerStop => "server_stop",
        Request::InvokeService { .. } => "invoke_service",
        Request::NewSession { .. } => "new_session",
        Request::ListSessions => "list_sessions",
        Request::ListClients => "list_clients",
        Request::CreateContext { .. } => "create_context",
        Request::ListContexts => "list_contexts",
        Request::SelectContext { .. } => "select_context",
        Request::CloseContext { .. } => "close_context",
        Request::CurrentContext => "current_context",
        Request::KillSession { .. } => "kill_session",
        Request::ListPanes { .. } => "list_panes",
        Request::SplitPane { .. } => "split_pane",
        Request::FocusPane { .. } => "focus_pane",
        Request::ResizePane { .. } => "resize_pane",
        Request::ClosePane { .. } => "close_pane",
        Request::ZoomPane { .. } => "zoom_pane",
        Request::FollowClient { .. } => "follow_client",
        Request::Unfollow => "unfollow",
        Request::Attach { .. } => "attach",
        Request::AttachContext { .. } => "attach_context",
        Request::AttachOpen { .. } => "attach_open",
        Request::AttachInput { .. } => "attach_input",
        Request::AttachSetViewport { .. } => "attach_set_viewport",
        Request::AttachOutput { .. } => "attach_output",
        Request::AttachLayout { .. } => "attach_layout",
        Request::AttachSnapshot { .. } => "attach_snapshot",
        Request::AttachPaneOutputBatch { .. } => "attach_pane_output_batch",
        Request::RecordingStart { .. } => "recording_start",
        Request::RecordingStop { .. } => "recording_stop",
        Request::RecordingStatus => "recording_status",
        Request::RecordingList => "recording_list",
        Request::RecordingDelete { .. } => "recording_delete",
        Request::RecordingWriteCustomEvent { .. } => "recording_write_custom_event",
        Request::RecordingDeleteAll => "recording_delete_all",
        Request::RecordingPrune { .. } => "recording_prune",
        Request::Detach => "detach",
        Request::SubscribeEvents => "subscribe_events",
        Request::PollEvents { .. } => "poll_events",
        Request::EnableEventPush => "enable_event_push",
        Request::PaneDirectInput { .. } => "pane_direct_input",
    }
}

fn default_recording_event_kinds(
    profile: RecordingProfile,
    capture_input: bool,
) -> Vec<RecordingEventKind> {
    let mut event_kinds = match profile {
        RecordingProfile::Full => vec![
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::ProtocolReplyRaw,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ],
        RecordingProfile::Functional => vec![
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ],
        RecordingProfile::Visual => vec![RecordingEventKind::PaneOutputRaw],
    };
    if capture_input && profile != RecordingProfile::Visual {
        event_kinds.push(RecordingEventKind::PaneInputRaw);
    }
    event_kinds
}

const fn response_payload_kind_name(payload: &ResponsePayload) -> &'static str {
    match payload {
        ResponsePayload::Pong => "pong",
        ResponsePayload::ClientIdentity { .. } => "client_identity",
        ResponsePayload::PrincipalIdentity { .. } => "principal_identity",
        ResponsePayload::ServerStatus { .. } => "server_status",
        ResponsePayload::ServerSnapshotSaved { .. } => "server_snapshot_saved",
        ResponsePayload::ServerSnapshotRestoreDryRun { .. } => "server_snapshot_restore_dry_run",
        ResponsePayload::ServerSnapshotRestored { .. } => "server_snapshot_restored",
        ResponsePayload::ServerStopping => "server_stopping",
        ResponsePayload::ServiceInvoked { .. } => "service_invoked",
        ResponsePayload::SessionCreated { .. } => "session_created",
        ResponsePayload::SessionList { .. } => "session_list",
        ResponsePayload::ClientList { .. } => "client_list",
        ResponsePayload::ContextCreated { .. } => "context_created",
        ResponsePayload::ContextList { .. } => "context_list",
        ResponsePayload::ContextSelected { .. } => "context_selected",
        ResponsePayload::ContextClosed { .. } => "context_closed",
        ResponsePayload::CurrentContext { .. } => "current_context",
        ResponsePayload::SessionKilled { .. } => "session_killed",
        ResponsePayload::PaneList { .. } => "pane_list",
        ResponsePayload::PaneSplit { .. } => "pane_split",
        ResponsePayload::PaneFocused { .. } => "pane_focused",
        ResponsePayload::PaneResized { .. } => "pane_resized",
        ResponsePayload::PaneClosed { .. } => "pane_closed",
        ResponsePayload::PaneZoomed { .. } => "pane_zoomed",
        ResponsePayload::FollowStarted { .. } => "follow_started",
        ResponsePayload::FollowStopped { .. } => "follow_stopped",
        ResponsePayload::Attached { .. } => "attached",
        ResponsePayload::AttachReady { .. } => "attach_ready",
        ResponsePayload::AttachInputAccepted { .. } => "attach_input_accepted",
        ResponsePayload::AttachViewportSet { .. } => "attach_viewport_set",
        ResponsePayload::AttachOutput { .. } => "attach_output",
        ResponsePayload::AttachLayout { .. } => "attach_layout",
        ResponsePayload::AttachSnapshot { .. } => "attach_snapshot",
        ResponsePayload::AttachPaneOutputBatch { .. } => "attach_pane_output_batch",
        ResponsePayload::RecordingStarted { .. } => "recording_started",
        ResponsePayload::RecordingStopped { .. } => "recording_stopped",
        ResponsePayload::RecordingStatus { .. } => "recording_status",
        ResponsePayload::RecordingList { .. } => "recording_list",
        ResponsePayload::RecordingDeleted { .. } => "recording_deleted",
        ResponsePayload::RecordingCustomEventWritten { .. } => "recording_custom_event_written",
        ResponsePayload::RecordingDeleteAll { .. } => "recording_delete_all",
        ResponsePayload::RecordingPruned { .. } => "recording_pruned",
        ResponsePayload::Detached => "detached",
        ResponsePayload::PaneDirectInputAccepted { .. } => "pane_direct_input_accepted",
        ResponsePayload::EventsSubscribed => "events_subscribed",
        ResponsePayload::EventBatch { .. } => "event_batch",
        ResponsePayload::EventPushEnabled => "event_push_enabled",
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

    {
        let mut context_state = state
            .context_state
            .lock()
            .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
        context_state.disconnect_client(client_id);
    }

    if previous_selected.is_none() && previous_stream.is_none() {
        return Ok(());
    }

    let mut manager = state
        .session_manager
        .lock()
        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
    if let Some(session_id) = previous_selected
        && let Some(session) = manager.get_session_mut(&session_id)
    {
        session.remove_client(&client_id);
    }
    drop(manager);

    if let Some(stream_session_id) = previous_stream {
        let mut runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
        runtime_manager.end_attach(stream_session_id, client_id);
        drop(runtime_manager);

        emit_event(
            state,
            Event::ClientDetached {
                id: stream_session_id.0,
            },
        )?;
    }

    Ok(())
}

fn resolve_session_id(manager: &SessionManager, selector: &SessionSelector) -> Option<SessionId> {
    match selector {
        SessionSelector::ById(raw_id) => {
            let session_id = SessionId(*raw_id);
            manager.get_session(&session_id).map(|_| session_id)
        }
        SessionSelector::ByName(value) => {
            let sessions = manager.list_sessions();

            if let Some(session) = sessions
                .iter()
                .find(|session| session.name.as_deref() == Some(value.as_str()))
            {
                return Some(session.id);
            }

            if let Some(session) = sessions
                .iter()
                .find(|session| session.id.to_string().eq_ignore_ascii_case(value))
            {
                return Some(session.id);
            }

            let value_lower = value.to_ascii_lowercase();
            sessions
                .iter()
                .find(|session| {
                    session
                        .id
                        .to_string()
                        .to_ascii_lowercase()
                        .starts_with(&value_lower)
                })
                .map(|session| session.id)
        }
    }
}

pub fn offline_kill_sessions(target: OfflineSessionKillTarget) -> Result<OfflineSessionKillReport> {
    let paths = ConfigPaths::default();
    let snapshot_manager = SnapshotManager::from_paths(&paths);
    let kill_all = matches!(target, OfflineSessionKillTarget::All);
    if !snapshot_manager.path().exists() {
        return Ok(OfflineSessionKillReport {
            had_snapshot: false,
            ..OfflineSessionKillReport::default()
        });
    }

    let _lock = acquire_offline_snapshot_lock(snapshot_manager.path())?;
    let mut snapshot = match snapshot_manager.read_snapshot() {
        Ok(snapshot) => snapshot,
        Err(persistence::SnapshotError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(OfflineSessionKillReport {
                had_snapshot: false,
                ..OfflineSessionKillReport::default()
            });
        }
        Err(error) if kill_all => {
            if let Err(remove_error) = std::fs::remove_file(snapshot_manager.path())
                && remove_error.kind() != std::io::ErrorKind::NotFound
            {
                anyhow::bail!(
                    "failed reading snapshot for offline kill ({error}); failed removing invalid snapshot: {remove_error}"
                );
            }
            return Ok(OfflineSessionKillReport {
                had_snapshot: true,
                ..OfflineSessionKillReport::default()
            });
        }
        Err(error) => anyhow::bail!("failed reading snapshot for offline kill: {error}"),
    };

    let removed_session_ids = match target {
        OfflineSessionKillTarget::All => snapshot
            .sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        OfflineSessionKillTarget::One(selector) => {
            resolve_snapshot_session_id(&snapshot, &selector)
                .into_iter()
                .collect::<Vec<_>>()
        }
    };

    if removed_session_ids.is_empty() {
        return Ok(OfflineSessionKillReport {
            had_snapshot: true,
            ..OfflineSessionKillReport::default()
        });
    }

    let removed_session_set = removed_session_ids.iter().copied().collect::<BTreeSet<_>>();
    kill_removed_snapshot_session_process_groups(&snapshot, &removed_session_set);

    snapshot
        .sessions
        .retain(|session| !removed_session_set.contains(&session.id));

    for selected in &mut snapshot.selected_sessions {
        if selected
            .session_id
            .is_some_and(|session_id| removed_session_set.contains(&session_id))
        {
            selected.session_id = None;
        }
    }

    let removed_context_set = snapshot
        .context_session_bindings
        .iter()
        .filter_map(|binding| {
            removed_session_set
                .contains(&binding.session_id)
                .then_some(binding.context_id)
        })
        .collect::<BTreeSet<_>>();

    snapshot
        .context_session_bindings
        .retain(|binding| !removed_context_set.contains(&binding.context_id));
    snapshot
        .contexts
        .retain(|context| !removed_context_set.contains(&context.id));

    for selected in &mut snapshot.selected_contexts {
        if selected
            .context_id
            .is_some_and(|context_id| removed_context_set.contains(&context_id))
        {
            selected.context_id = None;
        }
    }
    snapshot
        .mru_contexts
        .retain(|context_id| !removed_context_set.contains(context_id));

    snapshot_manager
        .write_snapshot(&snapshot)
        .map_err(|error| anyhow::anyhow!("failed writing snapshot for offline kill: {error}"))?;

    Ok(OfflineSessionKillReport {
        had_snapshot: true,
        removed_session_ids,
        removed_context_ids: removed_context_set.into_iter().collect(),
    })
}

fn resolve_snapshot_session_id(snapshot: &SnapshotV4, selector: &SessionSelector) -> Option<Uuid> {
    match selector {
        SessionSelector::ById(raw_id) => snapshot
            .sessions
            .iter()
            .find(|session| session.id == *raw_id)
            .map(|session| session.id),
        SessionSelector::ByName(value) => {
            if let Some(session) = snapshot
                .sessions
                .iter()
                .find(|session| session.name.as_deref() == Some(value.as_str()))
            {
                return Some(session.id);
            }

            if let Some(session) = snapshot
                .sessions
                .iter()
                .find(|session| session.id.to_string().eq_ignore_ascii_case(value))
            {
                return Some(session.id);
            }

            let value_lower = value.to_ascii_lowercase();
            snapshot
                .sessions
                .iter()
                .find(|session| {
                    session
                        .id
                        .to_string()
                        .to_ascii_lowercase()
                        .starts_with(&value_lower)
                })
                .map(|session| session.id)
        }
    }
}

fn kill_removed_snapshot_session_process_groups(
    snapshot: &SnapshotV4,
    removed_session_set: &BTreeSet<Uuid>,
) {
    let process_groups = snapshot
        .sessions
        .iter()
        .filter(|session| removed_session_set.contains(&session.id))
        .flat_map(|session| {
            session
                .panes
                .iter()
                .filter_map(|pane| pane.process_group_id)
        })
        .filter(|pgid| *pgid > 0)
        .collect::<BTreeSet<_>>();

    for process_group_id in process_groups {
        let _ = terminate_process_group(process_group_id);
    }
}

#[cfg(unix)]
fn terminate_process_group(process_group_id: i32) -> bool {
    if process_group_id <= 0 {
        return false;
    }

    let sent_term = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(format!("-{process_group_id}"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    std::thread::sleep(Duration::from_millis(120));

    let group_still_alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(format!("-{process_group_id}"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if group_still_alive {
        return std::process::Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{process_group_id}"))
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
            || sent_term;
    }

    sent_term
}

#[cfg(not(unix))]
fn terminate_process_group(_process_group_id: i32) -> bool {
    false
}

#[cfg(unix)]
fn resolve_process_group_id_for_pid(pid: u32) -> Option<i32> {
    let output = std::process::Command::new("ps")
        .arg("-o")
        .arg("pgid=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parsed = value.parse::<i32>().ok()?;
    (parsed > 0).then_some(parsed)
}

#[cfg(not(unix))]
fn resolve_process_group_id_for_pid(_pid: u32) -> Option<i32> {
    None
}

struct OfflineSnapshotMutationLock {
    path: std::path::PathBuf,
}

impl Drop for OfflineSnapshotMutationLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_offline_snapshot_lock(
    snapshot_path: &std::path::Path,
) -> Result<OfflineSnapshotMutationLock> {
    let parent = snapshot_path.parent().ok_or_else(|| {
        anyhow::anyhow!("failed acquiring offline snapshot lock: snapshot has no parent directory")
    })?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed creating snapshot directory {}", parent.display()))?;
    let lock_path = parent.join("server-snapshot-v1.lock");
    let started = Instant::now();

    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                return Ok(OfflineSnapshotMutationLock { path: lock_path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if started.elapsed() >= OFFLINE_SNAPSHOT_LOCK_TIMEOUT {
                    anyhow::bail!(
                        "timed out waiting for snapshot lock {}; retry once no other snapshot mutation is in progress",
                        lock_path.display()
                    );
                }
                std::thread::sleep(OFFLINE_SNAPSHOT_LOCK_RETRY_INTERVAL);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed acquiring snapshot lock {}", lock_path.display())
                });
            }
        }
    }
}

fn session_not_found_message(selector: &SessionSelector) -> String {
    format!(
        "session not found for selector {selector:?} (lookup order: exact name -> exact UUID -> UUID prefix)"
    )
}

fn resolve_session_request_session_id(
    manager: &SessionManager,
    selector: &Option<SessionSelector>,
    selected_session: &Option<SessionId>,
) -> std::result::Result<SessionId, ErrorResponse> {
    if let Some(selector) = selector {
        return resolve_session_id(manager, selector).ok_or_else(|| ErrorResponse {
            code: ErrorCode::NotFound,
            message: session_not_found_message(selector),
        });
    }

    if let Some(selected) = selected_session {
        return Ok(*selected);
    }

    let sessions = manager.list_sessions();
    if sessions.len() == 1 {
        return Ok(sessions[0].id);
    }

    Err(ErrorResponse {
        code: ErrorCode::InvalidRequest,
        message: "session selector is required when no attached session is active".to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionPolicyCheckRequest {
    session_id: Uuid,
    #[serde(default)]
    context_id: Option<Uuid>,
    client_id: Uuid,
    principal_id: Uuid,
    action: String,
    #[serde(default)]
    plugin_id: Option<String>,
    #[serde(default)]
    capability: Option<String>,
    #[serde(default)]
    execution_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionPolicyCheckResponse {
    allowed: bool,
    reason: Option<String>,
}

async fn check_session_policy(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    session_id: SessionId,
    client_id: ClientId,
    client_principal_id: Uuid,
    action: &str,
) -> std::result::Result<Option<SessionPolicyCheckResponse>, ErrorResponse> {
    let route = ServiceRoute {
        capability: "bmux.sessions.policy".to_string(),
        kind: bmux_ipc::InvokeServiceKind::Query,
        interface_id: "session-policy-query/v1".to_string(),
        operation: "check".to_string(),
    };
    let payload = encode(&SessionPolicyCheckRequest {
        session_id: session_id.0,
        context_id: None,
        client_id: client_id.0,
        principal_id: client_principal_id,
        action: action.to_string(),
        plugin_id: None,
        capability: None,
        execution_class: None,
    })
    .map_err(|error| ErrorResponse {
        code: ErrorCode::Internal,
        message: format!("failed encoding session policy request: {error}"),
    })?;

    let invoke_context = ServiceInvokeContext {
        state: Arc::clone(state),
        shutdown_tx: shutdown_tx.clone(),
        client_id,
        client_principal_id,
        selection: Arc::new(AsyncMutex::new((Some(session_id), None))),
    };

    let dispatch = {
        let registry = state.service_registry.lock().map_err(|_| ErrorResponse {
            code: ErrorCode::Internal,
            message: "service registry lock poisoned".to_string(),
        })?;
        registry.dispatch(&route, invoke_context.clone(), payload.clone())
    };
    let invocation = if let Some(invocation) = dispatch {
        Some(invocation)
    } else {
        let resolver = state
            .service_resolver
            .lock()
            .map_err(|_| ErrorResponse {
                code: ErrorCode::Internal,
                message: "service resolver lock poisoned".to_string(),
            })?
            .clone();
        resolver.map(|resolver| resolver(route, payload))
    };

    let Some(invocation) = invocation else {
        return Ok(None);
    };
    let payload = invocation.await.map_err(|error| ErrorResponse {
        code: ErrorCode::Internal,
        message: format!("session policy invocation failed: {error:#}"),
    })?;
    let response =
        decode::<SessionPolicyCheckResponse>(&payload).map_err(|error| ErrorResponse {
            code: ErrorCode::Internal,
            message: format!("failed decoding session policy response: {error}"),
        })?;
    Ok(Some(response))
}

async fn ensure_session_admin_allowed(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    session_id: SessionId,
    client_id: ClientId,
    client_principal_id: Uuid,
    action: &str,
) -> std::result::Result<(), ErrorResponse> {
    let decision = check_session_policy(
        state,
        shutdown_tx,
        session_id,
        client_id,
        client_principal_id,
        action,
    )
    .await?;
    if let Some(decision) = decision
        && !decision.allowed
    {
        return Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: decision
                .reason
                .unwrap_or_else(|| "session policy denied for this operation".to_string()),
        });
    }
    Ok(())
}

async fn ensure_session_mutation_allowed(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    session_id: SessionId,
    client_id: ClientId,
    client_principal_id: Uuid,
    action: &str,
) -> std::result::Result<(), ErrorResponse> {
    let decision = check_session_policy(
        state,
        shutdown_tx,
        session_id,
        client_id,
        client_principal_id,
        action,
    )
    .await?;
    if let Some(decision) = decision
        && !decision.allowed
    {
        return Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: decision
                .reason
                .unwrap_or_else(|| "session policy denied for this operation".to_string()),
        });
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
) -> Result<()> {
    let response = Response::Err(ErrorResponse { code, message });
    send_response_via_channel(frame_tx, request_id, response)
}

fn send_response_via_channel(
    frame_tx: &mpsc::UnboundedSender<Vec<u8>>,
    request_id: u64,
    response: Response,
) -> Result<()> {
    let payload = encode(&response).context("failed encoding response payload")?;
    let envelope = Envelope::new(request_id, EnvelopeKind::Response, payload);
    let frame =
        bmux_ipc::frame::encode_frame(&envelope).context("failed encoding response frame")?;
    frame_tx
        .send(frame)
        .map_err(|_| anyhow::anyhow!("writer channel closed"))?;
    Ok(())
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
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    async fn execute_request(
        server: &BmuxServer,
        client_id: ClientId,
        principal_id: Uuid,
        selected_session: &mut Option<SessionId>,
        attached_stream_session: &mut Option<SessionId>,
        request: Request,
    ) -> Response {
        handle_request(
            &server.state,
            &server.shutdown_tx,
            client_id,
            principal_id,
            selected_session,
            attached_stream_session,
            request,
        )
        .await
        .expect("request should complete")
    }

    fn test_endpoint() -> IpcEndpoint {
        #[cfg(unix)]
        {
            IpcEndpoint::unix_socket(PathBuf::from("/tmp/bmux-server-policy-test.sock"))
        }
        #[cfg(windows)]
        {
            IpcEndpoint::windows_named_pipe(PathBuf::from(r"\\.\pipe\bmux-server-policy-test"))
        }
    }

    #[tokio::test]
    async fn session_policy_fallback_is_permissive_without_provider() {
        let server = BmuxServer::new(test_endpoint());
        let result = ensure_session_mutation_allowed(
            &server.state,
            &server.shutdown_tx,
            SessionId::new(),
            ClientId::new(),
            Uuid::new_v4(),
            "pane.split",
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn session_admin_policy_fallback_is_permissive_without_provider() {
        let server = BmuxServer::new(test_endpoint());
        let result = ensure_session_admin_allowed(
            &server.state,
            &server.shutdown_tx,
            SessionId::new(),
            ClientId::new(),
            Uuid::new_v4(),
            "session.kill",
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn split_pane_succeeds_without_policy_provider() {
        let server = BmuxServer::new(test_endpoint());
        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let created = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::NewSession { name: None },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            response => panic!("expected session created response, got {response:?}"),
        };

        let split = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::SplitPane {
                session: Some(SessionSelector::ById(session_id)),
                target: None,
                direction: PaneSplitDirection::Vertical,
                ratio_pct: None,
            },
        )
        .await;

        match split {
            Response::Ok(ResponsePayload::PaneSplit {
                session_id: split_session_id,
                ..
            }) => {
                assert_eq!(split_session_id, session_id);
            }
            response => panic!("expected successful split response, got {response:?}"),
        }
    }

    #[tokio::test]
    async fn create_context_sets_current_context_for_client() {
        let server = BmuxServer::new(test_endpoint());
        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let mut attributes = BTreeMap::new();
        attributes.insert("core.kind".to_string(), "editor".to_string());
        let created = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CreateContext {
                name: Some("alpha".to_string()),
                attributes,
            },
        )
        .await;
        let context_id = match created {
            Response::Ok(ResponsePayload::ContextCreated { context }) => context.id,
            response => panic!("expected context created response, got {response:?}"),
        };

        let current = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CurrentContext,
        )
        .await;
        match current {
            Response::Ok(ResponsePayload::CurrentContext {
                context: Some(context),
            }) => {
                assert_eq!(context.id, context_id);
                assert_eq!(context.name.as_deref(), Some("alpha"));
            }
            response => panic!("expected current context response, got {response:?}"),
        }
    }

    #[tokio::test]
    async fn new_session_creates_bound_context_for_client() {
        let server = BmuxServer::new(test_endpoint());
        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let session_name = "session-window".to_string();
        let created = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::NewSession {
                name: Some(session_name.clone()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated {
                id,
                name: Some(name),
            }) => {
                assert_eq!(name, session_name);
                id
            }
            response => panic!("expected session created response, got {response:?}"),
        };

        let current = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CurrentContext,
        )
        .await;

        let context = match current {
            Response::Ok(ResponsePayload::CurrentContext {
                context: Some(context),
            }) => context,
            response => panic!("expected current context response, got {response:?}"),
        };
        assert_eq!(context.name.as_deref(), Some(session_name.as_str()));

        let mapped_session = {
            let context_state = server
                .state
                .context_state
                .lock()
                .expect("context state lock should succeed");
            context_state
                .session_by_context
                .get(&context.id)
                .copied()
                .expect("new session context should bind session")
        };
        assert_eq!(mapped_session.0, session_id);
    }

    #[tokio::test]
    async fn attach_context_updates_selected_context_and_grant_context_id() {
        let server = BmuxServer::new(test_endpoint());
        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let first = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CreateContext {
                name: Some("first".to_string()),
                attributes: BTreeMap::new(),
            },
        )
        .await;
        let first_id = match first {
            Response::Ok(ResponsePayload::ContextCreated { context }) => context.id,
            response => panic!("expected first context created response, got {response:?}"),
        };

        let second = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CreateContext {
                name: Some("second".to_string()),
                attributes: BTreeMap::new(),
            },
        )
        .await;
        let second_id = match second {
            Response::Ok(ResponsePayload::ContextCreated { context }) => context.id,
            response => panic!("expected second context created response, got {response:?}"),
        };

        let attached = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::AttachContext {
                selector: ContextSelector::ById(first_id),
            },
        )
        .await;
        match attached {
            Response::Ok(ResponsePayload::Attached { grant }) => {
                assert_eq!(grant.context_id, Some(first_id));
            }
            response => panic!("expected attached response, got {response:?}"),
        }

        let current = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CurrentContext,
        )
        .await;
        match current {
            Response::Ok(ResponsePayload::CurrentContext {
                context: Some(context),
            }) => assert_eq!(context.id, first_id),
            response => panic!("expected current context response, got {response:?}"),
        }

        let _ = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CloseContext {
                selector: ContextSelector::ById(first_id),
                force: true,
            },
        )
        .await;
        let _ = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CloseContext {
                selector: ContextSelector::ById(second_id),
                force: true,
            },
        )
        .await;
    }

    #[tokio::test]
    async fn select_context_completes_without_deadlock() {
        let server = BmuxServer::new(test_endpoint());
        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let created = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CreateContext {
                name: Some("alpha".to_string()),
                attributes: BTreeMap::new(),
            },
        )
        .await;
        let context_id = match created {
            Response::Ok(ResponsePayload::ContextCreated { context }) => context.id,
            response => panic!("expected context created response, got {response:?}"),
        };

        let selected = tokio::time::timeout(
            Duration::from_secs(1),
            execute_request(
                &server,
                client_id,
                principal_id,
                &mut selected_session,
                &mut attached_stream_session,
                Request::SelectContext {
                    selector: ContextSelector::ById(context_id),
                },
            ),
        )
        .await
        .expect("select context should not deadlock");

        match selected {
            Response::Ok(ResponsePayload::ContextSelected { context }) => {
                assert_eq!(context.id, context_id);
            }
            response => panic!("expected context selected response, got {response:?}"),
        }

        let _ = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CloseContext {
                selector: ContextSelector::ById(context_id),
                force: true,
            },
        )
        .await;
    }

    #[test]
    fn remove_contexts_for_session_clears_mapping_and_reselects_client() {
        let client_id = ClientId::new();
        let mut context_state = ContextState::default();

        let first = context_state.create(client_id, Some("first".to_string()), BTreeMap::new());
        let first_session_id = SessionId::new();
        context_state
            .bind_session(first.id, first_session_id)
            .expect("first context should bind to session");

        let second = context_state.create(client_id, Some("second".to_string()), BTreeMap::new());
        let second_session_id = SessionId::new();
        context_state
            .bind_session(second.id, second_session_id)
            .expect("second context should bind to session");

        let _ = context_state
            .select_for_client(client_id, &ContextSelector::ById(first.id))
            .expect("selecting first context should succeed");

        let removed = context_state.remove_contexts_for_session(first_session_id);
        assert_eq!(removed, vec![first.id]);
        assert!(
            context_state
                .context_for_session(first_session_id)
                .is_none()
        );
        assert_eq!(
            context_state
                .current_for_client(client_id)
                .map(|context| context.id),
            Some(second.id)
        );
        assert_eq!(
            context_state.current_session_for_client(client_id),
            Some(second_session_id)
        );
    }

    #[tokio::test]
    async fn attach_context_prunes_stale_context_session_mapping() {
        let server = BmuxServer::new(test_endpoint());
        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let created = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CreateContext {
                name: Some("stale".to_string()),
                attributes: BTreeMap::new(),
            },
        )
        .await;
        let context_id = match created {
            Response::Ok(ResponsePayload::ContextCreated { context }) => context.id,
            response => panic!("expected context created response, got {response:?}"),
        };

        let mapped_session_id = {
            let context_state = server
                .state
                .context_state
                .lock()
                .expect("context state lock should succeed");
            context_state
                .session_by_context
                .get(&context_id)
                .copied()
                .expect("created context should have session mapping")
        };

        {
            let mut manager = server
                .state
                .session_manager
                .lock()
                .expect("session manager lock should succeed");
            let _ = manager.remove_session(&mapped_session_id);
        }

        {
            let mut runtimes = server
                .state
                .session_runtimes
                .lock()
                .expect("session runtime manager lock should succeed");
            let _ = runtimes.remove_runtime(mapped_session_id);
        }

        let first_attach = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::AttachContext {
                selector: ContextSelector::ById(context_id),
            },
        )
        .await;
        match first_attach {
            Response::Err(error) => {
                assert_eq!(error.code, ErrorCode::NotFound);
                assert!(error.message.starts_with("session not found:"));
            }
            response => panic!("expected attach context not found response, got {response:?}"),
        }

        let current = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CurrentContext,
        )
        .await;
        match current {
            Response::Ok(ResponsePayload::CurrentContext { context }) => {
                assert!(context.is_none());
            }
            response => panic!("expected current context response, got {response:?}"),
        }

        let second_attach = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::AttachContext {
                selector: ContextSelector::ById(context_id),
            },
        )
        .await;
        match second_attach {
            Response::Err(error) => {
                assert_eq!(error.code, ErrorCode::NotFound);
                assert_eq!(error.message, "context not found");
            }
            response => panic!("expected attach context not found response, got {response:?}"),
        }
    }

    #[tokio::test]
    async fn close_active_context_promotes_most_recent_active_context() {
        let client_id = ClientId::new();
        let mut context_state = ContextState::default();

        let first = context_state.create(client_id, Some("first".to_string()), BTreeMap::new());
        let first_id = first.id;
        context_state
            .bind_session(first_id, SessionId::new())
            .expect("first context should bind to session");

        let second = context_state.create(client_id, Some("second".to_string()), BTreeMap::new());
        let second_id = second.id;
        context_state
            .bind_session(second_id, SessionId::new())
            .expect("second context should bind to session");

        let _ = context_state
            .select_for_client(client_id, &ContextSelector::ById(first_id))
            .expect("selecting first context should succeed");

        let (closed_id, _closed_session) = context_state
            .close(client_id, &ContextSelector::ById(first_id), true)
            .expect("closing first context should succeed");
        assert_eq!(closed_id, first_id);

        let current = context_state
            .current_for_client(client_id)
            .expect("current context should exist after close");
        assert_eq!(current.id, second_id);
    }

    #[tokio::test]
    async fn session_policy_denial_blocks_mutation() {
        let server = BmuxServer::new(test_endpoint());
        server
            .set_service_resolver(|route, payload| async move {
                if route.interface_id == "session-policy-query/v1" && route.operation == "check" {
                    let request: SessionPolicyCheckRequest = decode(&payload)?;
                    assert_eq!(request.action, "pane.split");
                    return encode(&SessionPolicyCheckResponse {
                        allowed: false,
                        reason: Some("session policy denied for this operation".to_string()),
                    })
                    .map_err(anyhow::Error::from);
                }
                anyhow::bail!("unexpected policy route")
            })
            .expect("resolver registration should succeed");

        let result = ensure_session_mutation_allowed(
            &server.state,
            &server.shutdown_tx,
            SessionId::new(),
            ClientId::new(),
            Uuid::new_v4(),
            "pane.split",
        )
        .await;

        let error = result.expect_err("mutation should be denied by policy provider");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("session policy denied"));
    }

    #[tokio::test]
    async fn session_policy_allows_admin_when_provider_approves() {
        let server = BmuxServer::new(test_endpoint());
        server
            .set_service_resolver(|route, payload| async move {
                if route.interface_id == "session-policy-query/v1" && route.operation == "check" {
                    let request: SessionPolicyCheckRequest = decode(&payload)?;
                    assert_eq!(request.action, "session.kill");
                    return encode(&SessionPolicyCheckResponse {
                        allowed: true,
                        reason: None,
                    })
                    .map_err(anyhow::Error::from);
                }
                anyhow::bail!("unexpected policy route")
            })
            .expect("resolver registration should succeed");

        let result = ensure_session_admin_allowed(
            &server.state,
            &server.shutdown_tx,
            SessionId::new(),
            ClientId::new(),
            Uuid::new_v4(),
            "session.kill",
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn kill_session_is_blocked_when_admin_policy_denies() {
        let server = BmuxServer::new(test_endpoint());
        server
            .set_service_resolver(|route, payload| async move {
                if route.interface_id == "session-policy-query/v1" && route.operation == "check" {
                    let request: SessionPolicyCheckRequest = decode(&payload)?;
                    assert_eq!(request.action, "session.kill");
                    return encode(&SessionPolicyCheckResponse {
                        allowed: false,
                        reason: Some("session policy denied for this operation".to_string()),
                    })
                    .map_err(anyhow::Error::from);
                }
                anyhow::bail!("unexpected policy route")
            })
            .expect("resolver registration should succeed");

        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let created = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::NewSession { name: None },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            response => panic!("expected session created response, got {response:?}"),
        };

        let killed = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::KillSession {
                selector: SessionSelector::ById(session_id),
                force_local: false,
            },
        )
        .await;

        match killed {
            Response::Err(error) => {
                assert_eq!(error.code, ErrorCode::InvalidRequest);
                assert!(error.message.contains("session policy denied"));
            }
            response => panic!("expected denied kill response, got {response:?}"),
        }
    }

    #[tokio::test]
    async fn split_pane_is_blocked_when_mutation_policy_denies() {
        let server = BmuxServer::new(test_endpoint());
        server
            .set_service_resolver(|route, payload| async move {
                if route.interface_id == "session-policy-query/v1" && route.operation == "check" {
                    let request: SessionPolicyCheckRequest = decode(&payload)?;
                    assert_eq!(request.action, "pane.split");
                    return encode(&SessionPolicyCheckResponse {
                        allowed: false,
                        reason: Some("session policy denied for this operation".to_string()),
                    })
                    .map_err(anyhow::Error::from);
                }
                anyhow::bail!("unexpected policy route")
            })
            .expect("resolver registration should succeed");

        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let created = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::NewSession { name: None },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            response => panic!("expected session created response, got {response:?}"),
        };

        let split = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::SplitPane {
                session: Some(SessionSelector::ById(session_id)),
                target: None,
                direction: PaneSplitDirection::Vertical,
                ratio_pct: None,
            },
        )
        .await;

        match split {
            Response::Err(error) => {
                assert_eq!(error.code, ErrorCode::InvalidRequest);
                assert!(error.message.contains("session policy denied"));
            }
            response => panic!("expected denied split response, got {response:?}"),
        }
    }

    #[tokio::test]
    async fn attach_input_is_blocked_when_mutation_policy_denies() {
        let server = BmuxServer::new(test_endpoint());
        server
            .set_service_resolver(|route, payload| async move {
                if route.interface_id == "session-policy-query/v1" && route.operation == "check" {
                    let request: SessionPolicyCheckRequest = decode(&payload)?;
                    assert_eq!(request.action, "attach.input");
                    return encode(&SessionPolicyCheckResponse {
                        allowed: false,
                        reason: Some("session policy denied for this operation".to_string()),
                    })
                    .map_err(anyhow::Error::from);
                }
                anyhow::bail!("unexpected policy route")
            })
            .expect("resolver registration should succeed");

        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let created = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::NewSession { name: None },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            response => panic!("expected session created response, got {response:?}"),
        };

        let attach_input = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::AttachInput {
                session_id,
                data: b"echo hi\n".to_vec(),
            },
        )
        .await;

        match attach_input {
            Response::Err(error) => {
                assert_eq!(error.code, ErrorCode::InvalidRequest);
                assert!(error.message.contains("session policy denied"));
            }
            response => panic!("expected denied attach input response, got {response:?}"),
        }
    }
}
