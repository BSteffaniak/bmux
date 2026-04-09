#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Server component for bmux terminal multiplexer.

mod persistence;
pub mod recording;

use anyhow::{Context, Result};
use bmux_config::{
    BmuxConfig, ConfigPaths, PerformanceRecordingLevel as ConfigPerformanceRecordingLevel,
};
use bmux_ipc::transport::{IpcTransportError, LocalIpcListener, LocalIpcStream};
use bmux_ipc::{
    AttachFocusTarget, AttachGrant, AttachInputModeState, AttachLayer, AttachMouseProtocolEncoding,
    AttachMouseProtocolMode, AttachMouseProtocolState, AttachPaneChunk, AttachPaneInputMode,
    AttachPaneMouseProtocol, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
    AttachViewComponent, CAPABILITY_CONTROL_CATALOG_SYNC, CORE_PROTOCOL_CAPABILITIES,
    ClientSummary, ContextSelector, ContextSessionBindingSummary, ContextSummary,
    ControlCatalogScope, ControlCatalogSnapshot, Envelope, EnvelopeKind, ErrorCode, ErrorResponse,
    Event, IpcEndpoint, PaneFocusDirection, PaneLayoutNode as IpcPaneLayoutNode, PaneSelector,
    PaneSplitDirection, PaneState, PaneSummary, PerformanceRecordingLevel,
    PerformanceRuntimeSettings, ProtocolContract, ProtocolVersion, RecordingEventKind,
    RecordingPayload, RecordingProfile, RecordingRollingClearReport, RecordingRollingStartOptions,
    RecordingRollingStatus, RecordingRollingUsage, RecordingSummary, Request, Response,
    ResponsePayload, ServerSnapshotStatus, SessionSelector, SessionSummary, decode,
    default_supported_capabilities, encode, negotiate_protocol,
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, trace, warn};
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
const EVENT_PUSH_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PerformanceCaptureSettings {
    level: PerformanceRecordingLevel,
    window_ms: u64,
    max_events_per_sec: u32,
    max_payload_bytes_per_sec: usize,
}

impl Default for PerformanceCaptureSettings {
    fn default() -> Self {
        Self::from_config(&BmuxConfig::default())
    }
}

impl PerformanceCaptureSettings {
    const fn from_config_level(
        level: ConfigPerformanceRecordingLevel,
    ) -> PerformanceRecordingLevel {
        match level {
            ConfigPerformanceRecordingLevel::Off => PerformanceRecordingLevel::Off,
            ConfigPerformanceRecordingLevel::Basic => PerformanceRecordingLevel::Basic,
            ConfigPerformanceRecordingLevel::Detailed => PerformanceRecordingLevel::Detailed,
            ConfigPerformanceRecordingLevel::Trace => PerformanceRecordingLevel::Trace,
        }
    }

    fn from_config(config: &BmuxConfig) -> Self {
        let perf = &config.performance;
        Self {
            level: Self::from_config_level(perf.recording_level),
            window_ms: perf.window_ms.max(1),
            max_events_per_sec: perf.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: perf.max_payload_bytes_per_sec.max(1),
        }
    }

    fn from_runtime_settings(settings: &PerformanceRuntimeSettings) -> Self {
        Self {
            level: settings.recording_level,
            window_ms: settings.window_ms.max(1),
            max_events_per_sec: settings.max_events_per_sec.max(1),
            max_payload_bytes_per_sec: settings.max_payload_bytes_per_sec.max(1),
        }
    }

    const fn to_runtime_settings(self) -> PerformanceRuntimeSettings {
        PerformanceRuntimeSettings {
            recording_level: self.level,
            window_ms: self.window_ms,
            max_events_per_sec: self.max_events_per_sec,
            max_payload_bytes_per_sec: self.max_payload_bytes_per_sec,
        }
    }

    const fn level_rank(level: PerformanceRecordingLevel) -> u8 {
        match level {
            PerformanceRecordingLevel::Off => 0,
            PerformanceRecordingLevel::Basic => 1,
            PerformanceRecordingLevel::Detailed => 2,
            PerformanceRecordingLevel::Trace => 3,
        }
    }

    const fn level_at_least(self, level: PerformanceRecordingLevel) -> bool {
        Self::level_rank(self.level) >= Self::level_rank(level)
    }

    const fn enabled(self) -> bool {
        !matches!(self.level, PerformanceRecordingLevel::Off)
    }

    const fn level_label(self) -> &'static str {
        match self.level {
            PerformanceRecordingLevel::Off => "off",
            PerformanceRecordingLevel::Basic => "basic",
            PerformanceRecordingLevel::Detailed => "detailed",
            PerformanceRecordingLevel::Trace => "trace",
        }
    }
}

#[derive(Debug)]
struct PerformanceEventRateLimiter {
    settings: PerformanceCaptureSettings,
    rate_window_started_at: Instant,
    emitted_events_in_window: u32,
    emitted_payload_bytes_in_window: usize,
    dropped_events_since_emit: u64,
    dropped_payload_bytes_since_emit: u64,
}

impl PerformanceEventRateLimiter {
    fn new(settings: PerformanceCaptureSettings) -> Self {
        Self {
            settings,
            rate_window_started_at: Instant::now(),
            emitted_events_in_window: 0,
            emitted_payload_bytes_in_window: 0,
            dropped_events_since_emit: 0,
            dropped_payload_bytes_since_emit: 0,
        }
    }

    fn reset_rate_window_if_needed(&mut self) {
        if self.rate_window_started_at.elapsed() >= Duration::from_secs(1) {
            self.rate_window_started_at = Instant::now();
            self.emitted_events_in_window = 0;
            self.emitted_payload_bytes_in_window = 0;
        }
    }

    fn can_emit_payload(&mut self, payload_len: usize) -> bool {
        if !self.settings.enabled() {
            return false;
        }

        self.reset_rate_window_if_needed();

        let event_limit_hit = self.emitted_events_in_window >= self.settings.max_events_per_sec;
        let payload_limit_hit = self
            .emitted_payload_bytes_in_window
            .saturating_add(payload_len)
            > self.settings.max_payload_bytes_per_sec;
        if event_limit_hit || payload_limit_hit {
            self.dropped_events_since_emit = self.dropped_events_since_emit.saturating_add(1);
            self.dropped_payload_bytes_since_emit = self
                .dropped_payload_bytes_since_emit
                .saturating_add(u64::try_from(payload_len).unwrap_or(u64::MAX));
            return false;
        }

        self.emitted_events_in_window = self.emitted_events_in_window.saturating_add(1);
        self.emitted_payload_bytes_in_window = self
            .emitted_payload_bytes_in_window
            .saturating_add(payload_len);
        true
    }

    fn encode_payload(&mut self, payload: serde_json::Value) -> Option<Vec<u8>> {
        if !self.settings.enabled() {
            return None;
        }

        let mut object = match payload {
            serde_json::Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_string(), other);
                map
            }
        };

        object.insert(
            "schema_version".to_string(),
            serde_json::Value::from(bmux_ipc::PERF_RECORDING_SCHEMA_VERSION),
        );
        object.insert(
            "level".to_string(),
            serde_json::Value::String(self.settings.level_label().to_string()),
        );
        object.insert(
            "runtime".to_string(),
            serde_json::Value::String("server".to_string()),
        );
        object.insert(
            "ts_epoch_ms".to_string(),
            serde_json::Value::from(epoch_millis_now()),
        );

        if self.dropped_events_since_emit > 0 || self.dropped_payload_bytes_since_emit > 0 {
            object.insert(
                "dropped_events_since_emit".to_string(),
                serde_json::Value::from(self.dropped_events_since_emit),
            );
            object.insert(
                "dropped_payload_bytes_since_emit".to_string(),
                serde_json::Value::from(self.dropped_payload_bytes_since_emit),
            );
            self.dropped_events_since_emit = 0;
            self.dropped_payload_bytes_since_emit = 0;
        }

        let encoded = serde_json::to_vec(&serde_json::Value::Object(object)).ok()?;
        if self.can_emit_payload(encoded.len()) {
            Some(encoded)
        } else {
            None
        }
    }
}

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
    manual_recording_runtime: Arc<Mutex<RecordingRuntime>>,
    rolling_recording_runtime: Arc<Mutex<Option<RecordingRuntime>>>,
    rolling_recording_auto_start: bool,
    rolling_recording_defaults: RollingRecordingSettings,
    performance_settings: Arc<Mutex<PerformanceCaptureSettings>>,
    rolling_recordings_dir: std::path::PathBuf,
    rolling_recording_segment_mb: usize,
    operation_lock: AsyncMutex<()>,
    event_hub: Mutex<EventHub>,
    /// Broadcast channel for pushing events to streaming clients.
    event_broadcast: tokio::sync::broadcast::Sender<Event>,
    control_catalog_revision: AtomicU64,
    client_capabilities: Mutex<BTreeMap<ClientId, BTreeSet<String>>>,
    client_principals: Mutex<BTreeMap<ClientId, Uuid>>,
    server_control_principal_id: Uuid,
    handshake_timeout: Duration,
    pane_exit_rx: AsyncMutex<mpsc::UnboundedReceiver<PaneExitEvent>>,
    service_registry: Mutex<ServiceRegistry>,
    service_resolver: Mutex<Option<Arc<ServiceResolverHandler>>>,
    /// Resolved image payload compression codec from config.
    /// Stored here so `handle_connection` can pass it to `delta.to_ipc()`.
    #[cfg(feature = "image-registry")]
    payload_codec: Option<Arc<dyn bmux_ipc::compression::CompressionCodec>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollingRecordingSettings {
    pub window_secs: u64,
    pub event_kinds: Vec<RecordingEventKind>,
}

impl RollingRecordingSettings {
    const fn is_available(&self) -> bool {
        self.window_secs > 0 && !self.event_kinds.is_empty()
    }

    fn capture_input(&self) -> bool {
        self.event_kinds.contains(&RecordingEventKind::PaneInputRaw)
    }
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
        drop(selection);
        Ok(response)
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

    fn poll_with_filter<F>(
        &mut self,
        client_id: ClientId,
        max_events: usize,
        mut include: F,
    ) -> Option<Vec<Event>>
    where
        F: FnMut(&Event) -> bool,
    {
        let cursor = self.subscribers.get_mut(&client_id)?;
        let mut index = *cursor;
        let count = max_events.max(1);
        let mut events = Vec::new();
        while index < self.events.len() && events.len() < count {
            let event = &self.events[index].event;
            if include(event) {
                events.push(event.clone());
            }
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

#[allow(clippy::cast_possible_truncation)]
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

#[allow(clippy::struct_field_names)]
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

        #[allow(clippy::needless_collect)]
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
            && !self.selected_by_client.contains_key(&client_id)
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
        let _ = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?
            .remove_session(&session_id);
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
    const FALLBACKS: &[&str] = &["xterm-256color", "screen-256color"];

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

    if tokio::time::timeout(Duration::from_millis(250), &mut pane.task)
        .await
        .is_ok()
    {
    } else {
        pane.task.abort();
        let _ = pane.task.await;
    }
}

fn push_pane_runtime_notice(
    output_buffer: &Arc<std::sync::Mutex<OutputFanoutBuffer>>,
    message: impl AsRef<str>,
) {
    if let Ok(mut output) = output_buffer.lock() {
        output.push_chunk(message.as_ref().as_bytes());
    }
}

fn format_pane_exit_reason(status: &portable_pty::ExitStatus) -> String {
    if let Some(signal) = status.signal() {
        return format!("process terminated by signal {signal}");
    }
    format!("process exited with status {}", status.exit_code())
}

struct SessionRuntimeManager {
    runtimes: BTreeMap<SessionId, SessionRuntimeHandle>,
    shell: String,
    pane_term: String,
    protocol_profile: ProtocolProfile,
    pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
    manual_recording_runtime: Arc<Mutex<RecordingRuntime>>,
    rolling_recording_runtime: Arc<Mutex<Option<RecordingRuntime>>>,
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

#[allow(clippy::struct_excessive_bools)]
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
    exit_reason: Arc<std::sync::Mutex<Option<String>>>,
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
    /// True while the inner application is inside a DEC mode 2026
    /// synchronized update (`\x1b[?2026h` seen, `\x1b[?2026l` not yet).
    /// Set by the PTY reader thread via the terminal mode tracker.
    sync_update_in_progress: Arc<AtomicBool>,
    mouse_protocol_state: Arc<std::sync::Mutex<AttachMouseProtocolState>>,
    input_mode_state: Arc<std::sync::Mutex<AttachInputModeState>>,
    #[cfg(feature = "image-registry")]
    image_registry: Arc<std::sync::Mutex<bmux_image::ImageRegistry>>,
    /// Cell pixel dimensions reported by the client (width, height).
    #[cfg(feature = "image-registry")]
    cell_pixel_size: Arc<std::sync::Mutex<(u16, u16)>>,
    /// Set to `true` when the image registry has new content.
    #[cfg(feature = "image-registry")]
    image_dirty: Arc<AtomicBool>,
}

enum PaneRuntimeCommand {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalModeParseState {
    Ground,
    Esc,
    Csi,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct PaneTerminalModeTracker {
    parse_state: TerminalModeParseState,
    csi_buffer: Vec<u8>,
    x10_mode: bool,
    press_release_mode: bool,
    button_motion_mode: bool,
    any_motion_mode: bool,
    utf8_encoding: bool,
    sgr_encoding: bool,
    application_cursor: bool,
    application_keypad: bool,
    /// DEC mode 2026: the inner application has begun a synchronized
    /// update (`\x1b[?2026h`) but has not yet ended it (`\x1b[?2026l`).
    sync_update: bool,
}

impl Default for PaneTerminalModeTracker {
    fn default() -> Self {
        Self {
            parse_state: TerminalModeParseState::Ground,
            csi_buffer: Vec::new(),
            x10_mode: false,
            press_release_mode: false,
            button_motion_mode: false,
            any_motion_mode: false,
            utf8_encoding: false,
            sgr_encoding: false,
            application_cursor: false,
            application_keypad: false,
            sync_update: false,
        }
    }
}

impl PaneTerminalModeTracker {
    fn process(&mut self, bytes: &[u8]) {
        for byte in bytes {
            match self.parse_state {
                TerminalModeParseState::Ground => {
                    if *byte == 0x1b {
                        self.parse_state = TerminalModeParseState::Esc;
                    }
                }
                TerminalModeParseState::Esc => {
                    if *byte == b'[' {
                        self.parse_state = TerminalModeParseState::Csi;
                        self.csi_buffer.clear();
                    } else if *byte == b'=' {
                        self.application_keypad = true;
                        self.parse_state = TerminalModeParseState::Ground;
                    } else if *byte == b'>' {
                        self.application_keypad = false;
                        self.parse_state = TerminalModeParseState::Ground;
                    } else if *byte == b'c' {
                        self.reset();
                    } else if *byte == 0x1b {
                        self.parse_state = TerminalModeParseState::Esc;
                    } else {
                        self.parse_state = TerminalModeParseState::Ground;
                    }
                }
                TerminalModeParseState::Csi => {
                    if *byte == 0x1b {
                        self.parse_state = TerminalModeParseState::Esc;
                        self.csi_buffer.clear();
                        continue;
                    }
                    self.csi_buffer.push(*byte);
                    if (0x40..=0x7e).contains(byte) {
                        let sequence = std::mem::take(&mut self.csi_buffer);
                        self.apply_csi_sequence(&sequence);
                        self.parse_state = TerminalModeParseState::Ground;
                    } else if self.csi_buffer.len() > 64 {
                        self.parse_state = TerminalModeParseState::Ground;
                        self.csi_buffer.clear();
                    }
                }
            }
        }
    }

    const fn current_protocol(&self) -> AttachMouseProtocolState {
        let mode = if self.any_motion_mode {
            AttachMouseProtocolMode::AnyMotion
        } else if self.button_motion_mode {
            AttachMouseProtocolMode::ButtonMotion
        } else if self.press_release_mode {
            AttachMouseProtocolMode::PressRelease
        } else if self.x10_mode {
            AttachMouseProtocolMode::Press
        } else {
            AttachMouseProtocolMode::None
        };

        let encoding = if self.sgr_encoding {
            AttachMouseProtocolEncoding::Sgr
        } else if self.utf8_encoding {
            AttachMouseProtocolEncoding::Utf8
        } else {
            AttachMouseProtocolEncoding::Default
        };

        AttachMouseProtocolState { mode, encoding }
    }

    const fn current_input_modes(&self) -> AttachInputModeState {
        AttachInputModeState {
            application_cursor: self.application_cursor,
            application_keypad: self.application_keypad,
        }
    }

    fn reset(&mut self) {
        self.parse_state = TerminalModeParseState::Ground;
        self.csi_buffer.clear();
        self.x10_mode = false;
        self.press_release_mode = false;
        self.button_motion_mode = false;
        self.any_motion_mode = false;
        self.utf8_encoding = false;
        self.sgr_encoding = false;
        self.application_cursor = false;
        self.application_keypad = false;
        self.sync_update = false;
    }

    fn apply_csi_sequence(&mut self, sequence: &[u8]) {
        if sequence == b"!p" {
            self.reset();
            return;
        }

        let Some((&final_byte, params)) = sequence.split_last() else {
            return;
        };

        let enable = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };

        let Some(private_modes) = params.strip_prefix(b"?") else {
            return;
        };

        for mode in private_modes
            .split(|byte| *byte == b';')
            .filter_map(parse_private_mode_number)
        {
            self.apply_private_mode(mode, enable);
        }
    }

    const fn apply_private_mode(&mut self, mode: u16, enable: bool) {
        match mode {
            1 => self.application_cursor = enable,
            9 => self.x10_mode = enable,
            1000 => self.press_release_mode = enable,
            1002 => self.button_motion_mode = enable,
            1003 => self.any_motion_mode = enable,
            1005 => self.utf8_encoding = enable,
            1006 => self.sgr_encoding = enable,
            2026 => self.sync_update = enable,
            _ => {}
        }
    }
}

struct PaneCursorTracker {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    cursor_escape_state: CursorEscapeState,
    /// Cumulative number of lines that have scrolled off the top.
    /// Used by the image registry to shift image positions on scroll.
    #[cfg(feature = "image-registry")]
    total_scrollback: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorEscapeState {
    Ground,
    Esc,
    EscBracket,
}

impl PaneCursorTracker {
    fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = sanitize_pty_size(rows, cols);
        Self {
            // Use scrollback of 1 so we can detect scroll events via
            // screen().scrollback() incrementing from 0 to 1.
            parser: vt100::Parser::new(rows, cols, 1),
            rows,
            cols,
            cursor_escape_state: CursorEscapeState::Ground,
            #[cfg(feature = "image-registry")]
            total_scrollback: 0,
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = sanitize_pty_size(rows, cols);
        if self.rows == rows && self.cols == cols {
            return;
        }
        self.parser.screen_mut().set_size(rows, cols);
        self.rows = rows;
        self.cols = cols;
    }

    fn process(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let mut normalized = Vec::with_capacity(bytes.len());
        for byte in bytes {
            match self.cursor_escape_state {
                CursorEscapeState::Ground => {
                    if *byte == 0x1b {
                        self.cursor_escape_state = CursorEscapeState::Esc;
                    } else {
                        normalized.push(*byte);
                    }
                }
                CursorEscapeState::Esc => {
                    if *byte == b'[' {
                        self.cursor_escape_state = CursorEscapeState::EscBracket;
                    } else if *byte == 0x1b {
                        normalized.push(0x1b);
                        self.cursor_escape_state = CursorEscapeState::Esc;
                    } else {
                        normalized.extend_from_slice(&[0x1b, *byte]);
                        self.cursor_escape_state = CursorEscapeState::Ground;
                    }
                }
                CursorEscapeState::EscBracket => {
                    match *byte {
                        // vt100::Parser reliably restores cursor for ESC 7/8 but
                        // can miss CSI s/u (especially when apps emit save/probe/
                        // restore around alt-screen transitions). Normalize those
                        // short forms to ESC 7/8 before feeding the parser.
                        b's' => normalized.extend_from_slice(b"\x1b7"),
                        b'u' => normalized.extend_from_slice(b"\x1b8"),
                        _ => {
                            normalized.extend_from_slice(b"\x1b[");
                            normalized.push(*byte);
                        }
                    }
                    self.cursor_escape_state = CursorEscapeState::Ground;
                }
            }
        }

        if !normalized.is_empty() {
            self.parser.process(&normalized);
        }
    }

    fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// Consume any scrollback that accumulated since the last call.
    /// Returns the number of lines that scrolled since last drain.
    #[cfg(feature = "image-registry")]
    fn drain_scroll_delta(&mut self) -> u16 {
        #[allow(clippy::cast_possible_truncation)]
        let scrollback = self.parser.screen().scrollback() as u16;
        if scrollback > 0 {
            self.total_scrollback += u64::from(scrollback);
            // Reset scrollback to 0 so we can detect the next scroll.
            self.parser.screen_mut().set_scrollback(0);
            scrollback
        } else {
            0
        }
    }
}

fn sanitize_pty_size(rows: u16, cols: u16) -> (u16, u16) {
    (rows.max(1), cols.max(1))
}

/// Convert an image event to a recording payload.
#[cfg(feature = "image-registry")]
fn image_event_to_recording_payload(event: &bmux_image::ImageEvent) -> RecordingPayload {
    match event {
        #[cfg(feature = "image-registry")]
        bmux_image::ImageEvent::SixelImage {
            data,
            position,
            pixel_size,
            ..
        } => RecordingPayload::Image {
            protocol: 0,
            position_row: position.row,
            position_col: position.col,
            cell_rows: 0,
            cell_cols: 0,
            pixel_width: pixel_size.width,
            pixel_height: pixel_size.height,
            data: data.clone(),
        },
        #[cfg(feature = "image-registry")]
        bmux_image::ImageEvent::KittyCommand { command: cmd, .. } => {
            let mut apc_body = Vec::new();
            match cmd {
                bmux_image::KittyCommand::Transmit {
                    image_id,
                    data,
                    width,
                    height,
                    ..
                } => {
                    apc_body = bmux_image::codec::kitty::encode_transmit(
                        *image_id,
                        bmux_image::KittyFormat::Rgba,
                        data,
                        *width,
                        *height,
                    );
                }
                bmux_image::KittyCommand::Place(placement) => {
                    apc_body = bmux_image::codec::kitty::encode_place(
                        placement.image_id,
                        placement.placement_id,
                        placement.position.row,
                        placement.position.col,
                    );
                }
                _ => {}
            }
            RecordingPayload::Image {
                protocol: 1,
                position_row: 0,
                position_col: 0,
                cell_rows: 0,
                cell_cols: 0,
                pixel_width: 0,
                pixel_height: 0,
                data: apc_body,
            }
        }
        #[cfg(feature = "image-registry")]
        bmux_image::ImageEvent::ITerm2Image { data, position, .. } => RecordingPayload::Image {
            protocol: 2,
            position_row: position.row,
            position_col: position.col,
            cell_rows: 0,
            cell_cols: 0,
            pixel_width: 0,
            pixel_height: 0,
            data: data.clone(),
        },
    }
}

/// Check if a chunk contains a screen-clearing CSI sequence.
/// Looks for `\e[2J` (erase display) or `\e[3J` (erase scrollback + display).
#[cfg(feature = "image-registry")]
fn chunk_contains_screen_clear(chunk: &[u8]) -> bool {
    // Fast scan for the byte patterns.
    for window in chunk.windows(4) {
        if window[0] == 0x1b
            && window[1] == b'['
            && window[3] == b'J'
            && (window[2] == b'2' || window[2] == b'3')
        {
            return true;
        }
    }
    false
}

fn protocol_reply_for_chunk(
    protocol_engine: &mut TerminalProtocolEngine,
    cursor_tracker: &mut PaneCursorTracker,
    chunk: &[u8],
) -> Vec<u8> {
    let mut reply = Vec::new();
    for byte in chunk {
        let byte_slice = std::slice::from_ref(byte);
        cursor_tracker.process(byte_slice);
        let byte_reply =
            protocol_engine.process_output(byte_slice, cursor_tracker.cursor_position());
        if let Some((query_kind, reply_row, reply_col)) = parse_cpr_reply(&byte_reply) {
            let (tracked_row, tracked_col) = cursor_tracker.cursor_position();
            trace!(
                query_kind,
                reply_row,
                reply_col,
                tracked_row = tracked_row.saturating_add(1),
                tracked_col = tracked_col.saturating_add(1),
                pane_rows = cursor_tracker.rows,
                pane_cols = cursor_tracker.cols,
                alternate_screen = cursor_tracker.parser.screen().alternate_screen(),
                "pane protocol reply: cursor position report"
            );
        }
        reply.extend(byte_reply);
    }
    reply
}

fn parse_cpr_reply(reply: &[u8]) -> Option<(&'static str, u16, u16)> {
    if let Some(body) = reply.strip_prefix(b"\x1b[?")
        && let Some((row, col)) = parse_cpr_coords(body, true)
    {
        return Some(("dec_cpr", row, col));
    }
    let body = reply.strip_prefix(b"\x1b[")?;
    parse_cpr_coords(body, false).map(|(row, col)| ("cpr", row, col))
}

fn parse_cpr_coords(body: &[u8], dec: bool) -> Option<(u16, u16)> {
    let body = body.strip_suffix(b"R")?;
    if !dec && body.starts_with(b"?") {
        return None;
    }

    let mut parts = body.splitn(2, |byte| *byte == b';');
    let row = std::str::from_utf8(parts.next()?)
        .ok()?
        .parse::<u16>()
        .ok()?;
    let col = std::str::from_utf8(parts.next()?)
        .ok()?
        .parse::<u16>()
        .ok()?;
    Some((row, col))
}

fn parse_private_mode_number(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u16 = 0;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?;
        value = value.checked_add(u16::from(*byte - b'0'))?;
    }
    Some(value)
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
}

impl SessionRuntimeManager {
    fn bump_attach_view_revision(&mut self, session_id: SessionId) -> Option<u64> {
        let runtime = self.runtimes.get_mut(&session_id)?;
        runtime.attach_view_revision = runtime.attach_view_revision.saturating_add(1);
        Some(runtime.attach_view_revision)
    }
}

/// Lightweight ECMA-48 escape sequence phase tracker.
///
/// Classifies each byte of a terminal output stream as either part of normal
/// ground-state text or inside an escape sequence (CSI, OSC, DCS, etc.).
/// Used by [`OutputFanoutBuffer`] to record safe resume boundaries so that
/// [`OutputFanoutBuffer::read_recent`] never returns bytes starting mid-sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscSeqPhase {
    /// Normal text and C0 controls — safe for a fresh parser to start here.
    Ground,
    /// Saw ESC (0x1B); next byte determines the sequence type.
    Escape,
    /// Inside a CSI sequence (ESC `[` …); ends on a final byte 0x40–0x7E.
    Csi,
    /// Inside an OSC string (ESC `]` …); ends on BEL (0x07) or ST (ESC `\`).
    Osc,
    /// Saw ESC inside an OSC body — looking for `\` to complete ST.
    OscEsc,
    /// Inside a DCS passthrough (ESC `P` …); ends on ST (ESC `\`).
    Dcs,
    /// Saw ESC inside a DCS body — looking for `\` to complete ST.
    DcsEsc,
    /// Inside an SOS, PM, or APC string (ESC `X`/`^`/`_` …); ends on ST.
    Sos,
    /// Saw ESC inside an SOS/PM/APC body — looking for `\` to complete ST.
    SosEsc,
}

impl EscSeqPhase {
    /// Advance the state machine by one byte, returning the new phase.
    #[inline]
    const fn advance(self, byte: u8) -> Self {
        // CAN (0x18) and SUB (0x1A) abort any sequence from any state.
        if byte == 0x18 || byte == 0x1A {
            return Self::Ground;
        }
        match self {
            Self::Ground => {
                if byte == 0x1B {
                    Self::Escape
                } else {
                    Self::Ground
                }
            }
            Self::Escape => match byte {
                b'[' => Self::Csi,
                b']' => Self::Osc,
                b'P' => Self::Dcs,
                b'X' | b'^' | b'_' => Self::Sos,
                // Intermediate bytes (0x20–0x2F) stay in Escape (ESC intermediate
                // sequence).  ESC restarts.
                0x1B | 0x20..=0x2F => Self::Escape,
                // Final bytes (0x30–0x7E) complete a two-byte escape.
                // Everything else also returns to Ground.
                _ => Self::Ground,
            },
            Self::Csi => match byte {
                // Final byte completes the CSI sequence.
                0x40..=0x7E => Self::Ground,
                // ESC inside CSI aborts it and starts a new escape.
                0x1B => Self::Escape,
                // Parameter bytes (0x30–0x3F) and intermediate bytes (0x20–0x2F)
                // continue the sequence.  Anything else also stays in CSI (tolerant
                // parsing, matching xterm behavior for invalid bytes).
                _ => Self::Csi,
            },
            Self::Osc => match byte {
                0x07 => Self::Ground, // BEL terminates OSC
                0x1B => Self::OscEsc,
                _ => Self::Osc,
            },
            Self::OscEsc => match byte {
                b'\\' => Self::Ground, // ST (ESC \) terminates OSC
                0x1B => Self::Escape,  // nested ESC aborts OSC
                _ => Self::Osc,        // false alarm, back to body
            },
            Self::Dcs => match byte {
                0x1B => Self::DcsEsc,
                _ => Self::Dcs,
            },
            Self::DcsEsc => match byte {
                b'\\' => Self::Ground,
                0x1B => Self::Escape,
                _ => Self::Dcs,
            },
            Self::Sos => match byte {
                0x1B => Self::SosEsc,
                _ => Self::Sos,
            },
            Self::SosEsc => match byte {
                b'\\' => Self::Ground,
                0x1B => Self::Escape,
                _ => Self::Sos,
            },
        }
    }

    const fn is_ground(self) -> bool {
        matches!(self, Self::Ground)
    }
}

struct OutputFanoutBuffer {
    max_bytes: usize,
    start_offset: u64,
    data: VecDeque<u8>,
    cursors: BTreeMap<ClientId, u64>,
    /// Running escape-sequence phase at the end of the buffer.
    esc_phase: EscSeqPhase,
    /// Escape-sequence spans: `(esc_start, safe_resume)` pairs where
    /// `esc_start` is the offset of the ESC byte that began a sequence and
    /// `safe_resume` is the first offset after the sequence completed
    /// (Ground state).  An open (incomplete) span has `safe_resume == u64::MAX`.
    /// Sorted ascending by `esc_start`.
    esc_spans: VecDeque<(u64, u64)>,
}

struct OutputRead {
    bytes: Vec<u8>,
    stream_start: u64,
    stream_end: u64,
    stream_gap: bool,
}

fn pane_output_event_from_read(
    session_id: Uuid,
    pane_id: Uuid,
    read: OutputRead,
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

impl OutputFanoutBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes: max_bytes.max(1),
            start_offset: 0,
            data: VecDeque::new(),
            cursors: BTreeMap::new(),
            esc_phase: EscSeqPhase::Ground,
            esc_spans: VecDeque::new(),
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
        let base_offset = self.end_offset();
        self.data.extend(chunk.iter().copied());

        // Track escape-sequence phase for every byte.  Record spans
        // so that `first_safe_offset_at_or_after` can determine whether
        // any position is inside an escape sequence.
        //
        // The ESC byte itself is a safe start — a fresh ground-state parser
        // correctly handles it.  The unsafe region begins at the byte AFTER
        // the ESC (e.g. the `[` in CSI, the `]` in OSC) and extends to the
        // byte after the final/terminator byte.
        for (i, &byte) in chunk.iter().enumerate() {
            let prev = self.esc_phase;
            self.esc_phase = prev.advance(byte);

            if prev.is_ground() && !self.esc_phase.is_ground() {
                // Ground → non-Ground: open a new span starting AFTER the ESC byte.
                self.esc_spans
                    .push_back((base_offset + i as u64 + 1, u64::MAX));
            } else if !prev.is_ground() && self.esc_phase.is_ground() {
                // non-Ground → Ground: close the current span.  The safe
                // resume point is the byte after the final/terminator byte.
                if let Some(last) = self.esc_spans.back_mut()
                    && last.1 == u64::MAX
                {
                    last.1 = base_offset + i as u64 + 1;
                }
            }
        }

        while self.data.len() > self.max_bytes {
            let _ = self.data.pop_front();
            self.start_offset = self.start_offset.saturating_add(1);
        }

        // Prune spans that are entirely before start_offset.
        while let Some(&(_, safe_resume)) = self.esc_spans.front() {
            if safe_resume != u64::MAX && safe_resume <= self.start_offset {
                self.esc_spans.pop_front();
            } else {
                break;
            }
        }

        // Do not mutate per-client cursors here.  `read_for_client` performs
        // clamping and reports `stream_gap` so clients can recover parser
        // continuity with explicit metadata.
    }

    fn read_for_client(&mut self, client_id: ClientId, max_bytes: usize) -> OutputRead {
        let limit = max_bytes.max(1);
        let end = self.end_offset();

        // Pre-compute the safe resume position before borrowing cursors
        // mutably, since first_ground_boundary_at_or_after borrows self
        // immutably.
        let safe_resume = self.first_safe_offset_at_or_after(self.start_offset);

        let cursor = self.cursors.entry(client_id).or_insert(end);

        let stream_gap = if *cursor < self.start_offset {
            // Bytes were evicted before the client could read them.  Advance
            // the cursor to the nearest safe position so the client
            // never receives bytes starting mid-escape-sequence.
            *cursor = safe_resume;
            true
        } else {
            false
        };

        let stream_start = *cursor;

        #[allow(clippy::cast_possible_truncation)]
        let available = end.saturating_sub(*cursor) as usize;
        if available == 0 {
            return OutputRead {
                bytes: Vec::new(),
                stream_start,
                stream_end: stream_start,
                stream_gap,
            };
        }

        let to_read = available.min(limit);
        #[allow(clippy::cast_possible_truncation)]
        let start_index = (*cursor - self.start_offset) as usize;
        let bytes = self
            .data
            .iter()
            .skip(start_index)
            .take(to_read)
            .copied()
            .collect::<Vec<_>>();
        *cursor = cursor.saturating_add(bytes.len() as u64);

        OutputRead {
            bytes,
            stream_start,
            stream_end: *cursor,
            stream_gap,
        }
    }

    #[cfg(test)]
    fn read_recent(&self, max_bytes: usize) -> Vec<u8> {
        self.read_recent_with_offsets(max_bytes).bytes
    }

    fn read_recent_with_offsets(&self, max_bytes: usize) -> OutputRead {
        let end = self.end_offset();
        if self.data.is_empty() {
            return OutputRead {
                bytes: Vec::new(),
                stream_start: end,
                stream_end: end,
                stream_gap: false,
            };
        }
        let to_read = self.data.len().min(max_bytes.max(1));
        let intended_start = end - to_read as u64;
        let safe_start = self.first_safe_offset_at_or_after(intended_start);

        if safe_start >= end {
            return OutputRead {
                bytes: Vec::new(),
                stream_start: end,
                stream_end: end,
                stream_gap: false,
            };
        }

        #[allow(clippy::cast_possible_truncation)]
        let start_index = (safe_start - self.start_offset) as usize;
        OutputRead {
            bytes: self.data.iter().skip(start_index).copied().collect(),
            stream_start: safe_start,
            stream_end: end,
            stream_gap: false,
        }
    }

    /// Return the first stream offset >= `target` where a fresh ground-state
    /// parser can safely start consuming bytes.
    ///
    /// Checks the escape-sequence span list to determine whether `target`
    /// falls inside an open span.  If so, advances to the span's
    /// `safe_resume` offset.
    fn first_safe_offset_at_or_after(&self, target: u64) -> u64 {
        // Find the span that could contain `target`.  We need the latest
        // span whose esc_start <= target.
        //
        // Binary search by esc_start (the first element of each tuple).
        let idx = self
            .esc_spans
            .binary_search_by(|&(esc_start, _)| esc_start.cmp(&target))
            .unwrap_or_else(|insert_point| insert_point.saturating_sub(1));

        // Check a small window of spans around the search result.  Due to
        // binary_search edge cases with saturating_sub, check idx and idx+1.
        for check in idx..self.esc_spans.len().min(idx + 2) {
            let (esc_start, safe_resume) = self.esc_spans[check];
            if esc_start <= target && target < safe_resume {
                // `target` falls inside this escape sequence.
                if safe_resume == u64::MAX {
                    // Sequence is still open (not yet terminated).
                    return self.end_offset();
                }
                return safe_resume;
            }
        }

        // `target` is not inside any escape sequence — it's in Ground state.
        target
    }

    /// Advance an existing client's read cursor to the end of the buffer,
    /// so the next `read_for_client` call only returns data written after
    /// this point. Used after snapshot reads to avoid re-delivering bytes
    /// the client already received via `read_recent`.
    fn advance_client_to_end(&mut self, client_id: ClientId) {
        let end = self.end_offset();
        if let Some(cursor) = self.cursors.get_mut(&client_id) {
            *cursor = end;
        }
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
    pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pane_input_modes: Vec<AttachPaneInputMode>,
    zoomed: bool,
}

struct AttachPaneSnapshotState {
    chunks: Vec<AttachPaneChunk>,
    pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pane_input_modes: Vec<AttachPaneInputMode>,
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
            Self::Leaf { .. } => false,
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
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
    if let Some(zoomed_id) = runtime.zoomed_pane_id
        && runtime.panes.contains_key(&zoomed_id)
    {
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
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
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
    runtime: &SessionRuntimeHandle,
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
        if let Some(pane) = runtime.panes.get(&zoomed_id)
            && !pane.exited.load(Ordering::SeqCst)
        {
            let (zoom_rows, zoom_cols) = pane_pty_size(root);
            pane.resize_pty(zoom_rows, zoom_cols);
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
    const fn new(
        shell: String,
        pane_term: String,
        protocol_profile: ProtocolProfile,
        pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
        manual_recording_runtime: Arc<Mutex<RecordingRuntime>>,
        rolling_recording_runtime: Arc<Mutex<Option<RecordingRuntime>>>,
        event_broadcast: tokio::sync::broadcast::Sender<Event>,
    ) -> Self {
        Self {
            runtimes: BTreeMap::new(),
            shell,
            pane_term,
            protocol_profile,
            pane_exit_tx,
            manual_recording_runtime,
            rolling_recording_runtime,
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
        let first_pane = self.spawn_pane_runtime(session_id, pane_meta);
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
        panes: &[PaneRuntimeMeta],
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
        for pane_meta in panes {
            let pane = self.spawn_pane_runtime(session_id, pane_meta.clone());
            runtime_panes.insert(pane_meta.id, pane);
        }

        let runtime_layout_root = layout_root
            .unwrap_or_else(|| layout_from_panes(panes).expect("restored runtime has panes"));
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

    #[allow(clippy::too_many_lines)]
    fn spawn_pane_runtime(
        &self,
        session_id: SessionId,
        pane_meta: PaneRuntimeMeta,
    ) -> PaneRuntimeHandle {
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
        let manual_recording_runtime = Arc::clone(&self.manual_recording_runtime);
        let rolling_recording_runtime = Arc::clone(&self.rolling_recording_runtime);
        let output_buffer_for_reader = Arc::clone(&output_buffer);
        let process_group_id = Arc::new(std::sync::Mutex::new(None));
        let process_group_id_for_task = Arc::clone(&process_group_id);
        let exit_reason = Arc::new(std::sync::Mutex::new(None::<String>));
        let exit_reason_for_task = Arc::clone(&exit_reason);
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_task = Arc::clone(&exited);
        let event_broadcast_for_reader = self.event_broadcast.clone();
        let output_dirty = Arc::new(AtomicBool::new(false));
        let output_dirty_for_reader = Arc::clone(&output_dirty);
        let last_requested_size_for_reader = Arc::clone(&last_requested_size);
        let sync_update_in_progress = Arc::new(AtomicBool::new(false));
        let sync_update_for_reader = Arc::clone(&sync_update_in_progress);
        let mouse_protocol_state =
            Arc::new(std::sync::Mutex::new(AttachMouseProtocolState::default()));
        let mouse_protocol_state_for_reader = Arc::clone(&mouse_protocol_state);
        let input_mode_state = Arc::new(std::sync::Mutex::new(AttachInputModeState::default()));
        let input_mode_state_for_reader = Arc::clone(&input_mode_state);

        #[cfg(feature = "image-registry")]
        let image_registry = {
            let img_config = bmux_config::BmuxConfig::load()
                .unwrap_or_default()
                .behavior
                .images;
            Arc::new(std::sync::Mutex::new(if img_config.enabled {
                #[allow(clippy::cast_possible_truncation)]
                bmux_image::ImageRegistry::new(
                    img_config.max_images_per_pane as usize,
                    img_config.max_image_bytes as usize,
                )
            } else {
                // Disabled: zero-capacity registry that drops everything.
                bmux_image::ImageRegistry::new(0, 0)
            }))
        };
        #[cfg(feature = "image-registry")]
        let image_registry_for_reader = Arc::clone(&image_registry);
        #[cfg(feature = "image-registry")]
        let cell_pixel_size = Arc::new(std::sync::Mutex::new((0u16, 0u16)));
        #[cfg(feature = "image-registry")]
        let cell_pixel_size_for_reader = Arc::clone(&cell_pixel_size);
        #[cfg(feature = "image-registry")]
        let image_dirty = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "image-registry")]
        let image_dirty_for_reader = Arc::clone(&image_dirty);

        let task = tokio::spawn(async move {
            let pty_system = native_pty_system();
            let Ok(pty_pair) = pty_system.openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }) else {
                if let Ok(mut reason) = exit_reason_for_task.lock() {
                    *reason = Some("failed to allocate PTY".to_string());
                }
                push_pane_runtime_notice(
                    &output_buffer_for_reader,
                    "\r\n[bmux] pane failed to start: failed to allocate PTY\r\n",
                );
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };

            let mut command = CommandBuilder::new(&shell);
            command.env("TERM", &pane_term);
            let Ok(mut child) = pty_pair.slave.spawn_command(command) else {
                if let Ok(mut reason) = exit_reason_for_task.lock() {
                    *reason = Some(format!("failed to spawn shell '{shell}'"));
                }
                push_pane_runtime_notice(
                    &output_buffer_for_reader,
                    format!("\r\n[bmux] pane failed to start: failed to spawn shell '{shell}'\r\n"),
                );
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

            let Ok(mut reader) = master.try_clone_reader() else {
                if let Ok(mut reason) = exit_reason_for_task.lock() {
                    *reason = Some("failed to open PTY reader".to_string());
                }
                push_pane_runtime_notice(
                    &output_buffer_for_reader,
                    "\r\n[bmux] pane failed to start: failed to open PTY reader\r\n",
                );
                let _ = child.kill();
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };
            let Ok(writer) = master.take_writer() else {
                if let Ok(mut reason) = exit_reason_for_task.lock() {
                    *reason = Some("failed to open PTY writer".to_string());
                }
                push_pane_runtime_notice(
                    &output_buffer_for_reader,
                    "\r\n[bmux] pane failed to start: failed to open PTY writer\r\n",
                );
                let _ = child.kill();
                exited_for_task.store(true, Ordering::SeqCst);
                return;
            };
            let writer = Arc::new(std::sync::Mutex::new(writer));

            let (child_exit_tx, mut child_exit_rx) = mpsc::unbounded_channel::<()>();
            let exited_for_waiter = Arc::clone(&exited_for_task);
            let exit_reason_for_waiter = Arc::clone(&exit_reason_for_task);
            let output_buffer_for_waiter = Arc::clone(&output_buffer_for_reader);
            let child_waiter = std::thread::Builder::new()
                .name(format!("bmux-server-pane-{pane_id}-wait"))
                .spawn(move || {
                    let wait_result = child.wait();
                    exited_for_waiter.store(true, Ordering::SeqCst);
                    if let Ok(mut reason) = exit_reason_for_waiter.lock()
                        && reason.is_none()
                    {
                        *reason = Some(match wait_result {
                            Ok(status) => format_pane_exit_reason(&status),
                            Err(error) => format!("process wait failed: {error}"),
                        });
                    }
                    push_pane_runtime_notice(
                        &output_buffer_for_waiter,
                        "\r\n[bmux] pane process exited; layout preserved. Use restart pane or close pane.\r\n",
                    );
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
                    let (initial_rows, initial_cols) = last_requested_size_for_reader
                        .lock()
                        .map(|size| *size)
                        .unwrap_or((24, 80));
                    let mut cursor_tracker = PaneCursorTracker::new(initial_rows, initial_cols);
                    let mut terminal_mode_tracker = PaneTerminalModeTracker::default();

                    // Image interceptor: detects and extracts image escape sequences
                    // (Sixel, Kitty, iTerm2) from PTY output before they reach the
                    // output buffer.  Feature-gated to compile away when no image
                    // protocols are enabled.
                    #[cfg(feature = "image-registry")]
                    let mut image_interceptor = bmux_image::ImageInterceptor::new();

                    loop {
                        match reader.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(bytes_read) => {
                                let chunk = &buffer[..bytes_read];
                                if let Ok((rows, cols)) =
                                    last_requested_size_for_reader.lock().map(|size| *size)
                                {
                                    cursor_tracker.resize(rows, cols);
                                }

                                // When image support is enabled, run the interceptor
                                // to extract image sequences from the byte stream.
                                // The filtered bytes (images stripped) are what gets
                                // pushed to the output buffer for vt100 parsing.
                                #[cfg(feature = "image-registry")]
                                let chunk = {
                                    let mut result = image_interceptor.process(chunk);

                                    if !result.events.is_empty() {
                                        // Resolve cursor positions for each image event.
                                        // Feed filtered bytes up to each event's offset
                                        // to the cursor tracker, then capture position.
                                        let mut cursor_fed_to = 0usize;
                                        for event in &mut result.events {
                                            let offset = event.filtered_byte_offset();
                                            if offset > cursor_fed_to {
                                                cursor_tracker.process(
                                                    &result.filtered[cursor_fed_to..offset],
                                                );
                                                cursor_fed_to = offset;
                                            }
                                            let (row, col) = cursor_tracker.cursor_position();
                                            event.set_position(bmux_image::ImagePosition {
                                                row,
                                                col,
                                            });
                                        }

                                        let (cpw, cph) = cell_pixel_size_for_reader
                                            .lock()
                                            .map(|s| *s)
                                            .unwrap_or((8, 16));
                                        let cpw = if cpw == 0 { 8 } else { cpw };
                                        let cph = if cph == 0 { 16 } else { cph };
                                        if let Ok(mut reg) = image_registry_for_reader.lock() {
                                            for event in &result.events {
                                                reg.handle_event(event.clone(), cpw, cph);
                                            }
                                        }
                                        // Notify streaming clients that image state changed.
                                        // Only emit on false→true transition to coalesce.
                                        if image_dirty_for_reader
                                            .compare_exchange(
                                                false,
                                                true,
                                                Ordering::SeqCst,
                                                Ordering::SeqCst,
                                            )
                                            .is_ok()
                                        {
                                            let _ = event_broadcast_for_reader.send(
                                                Event::PaneImageAvailable {
                                                    session_id: session_id.0,
                                                    pane_id,
                                                },
                                            );
                                        }
                                        for event in &result.events {
                                            let payload = image_event_to_recording_payload(event);
                                            record_to_all_runtimes(
                                                &manual_recording_runtime,
                                                &rolling_recording_runtime,
                                                RecordingEventKind::PaneImage,
                                                payload,
                                                RecordMeta {
                                                    session_id: Some(session_id.0),
                                                    pane_id: Some(pane_id),
                                                    client_id: None,
                                                },
                                            );
                                        }
                                    }
                                    result.filtered
                                };
                                #[cfg(feature = "image-registry")]
                                let chunk = chunk.as_slice();

                                // Detect screen-clearing CSI sequences (\e[2J, \e[3J)
                                // and clear the image registry when they occur.
                                #[cfg(feature = "image-registry")]
                                if chunk_contains_screen_clear(chunk) {
                                    if let Ok(mut reg) = image_registry_for_reader.lock() {
                                        reg.clear();
                                    }
                                    if image_dirty_for_reader
                                        .compare_exchange(
                                            false,
                                            true,
                                            Ordering::SeqCst,
                                            Ordering::SeqCst,
                                        )
                                        .is_ok()
                                    {
                                        let _ = event_broadcast_for_reader.send(
                                            Event::PaneImageAvailable {
                                                session_id: session_id.0,
                                                pane_id,
                                            },
                                        );
                                    }
                                }

                                // Update terminal mode tracking (mouse protocol,
                                // cursor/keypad modes, synchronized update) BEFORE
                                // making the chunk visible in the output buffer.
                                // This ensures per-pane mode snapshots are always
                                // consistent with or ahead of the buffered data.
                                terminal_mode_tracker.process(chunk);
                                if let Ok(mut protocol) = mouse_protocol_state_for_reader.lock() {
                                    *protocol = terminal_mode_tracker.current_protocol();
                                }
                                if let Ok(mut mode_state) = input_mode_state_for_reader.lock() {
                                    *mode_state = terminal_mode_tracker.current_input_modes();
                                }
                                sync_update_for_reader
                                    .store(terminal_mode_tracker.sync_update, Ordering::SeqCst);

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
                                record_to_all_runtimes(
                                    &manual_recording_runtime,
                                    &rolling_recording_runtime,
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
                                let reply = protocol_reply_for_chunk(
                                    &mut protocol_engine,
                                    &mut cursor_tracker,
                                    chunk,
                                );
                                // Detect scroll events and shift image positions.
                                #[cfg(feature = "image-registry")]
                                {
                                    let scroll_delta = cursor_tracker.drain_scroll_delta();
                                    if scroll_delta > 0 {
                                        if let Ok(mut reg) = image_registry_for_reader.lock() {
                                            reg.scroll_up(scroll_delta);
                                        }
                                        // Notify streaming clients that image positions shifted.
                                        if image_dirty_for_reader
                                            .compare_exchange(
                                                false,
                                                true,
                                                Ordering::SeqCst,
                                                Ordering::SeqCst,
                                            )
                                            .is_ok()
                                        {
                                            let _ = event_broadcast_for_reader.send(
                                                Event::PaneImageAvailable {
                                                    session_id: session_id.0,
                                                    pane_id,
                                                },
                                            );
                                        }
                                    }
                                }
                                if !reply.is_empty() {
                                    record_to_all_runtimes(
                                        &manual_recording_runtime,
                                        &rolling_recording_runtime,
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

        PaneRuntimeHandle {
            meta: pane_meta,
            process_group_id,
            exit_reason,
            stop_tx: Some(stop_tx),
            task,
            input_tx,
            output_buffer,
            exited,
            last_requested_size,
            output_dirty,
            sync_update_in_progress,
            mouse_protocol_state,
            input_mode_state,
            #[cfg(feature = "image-registry")]
            image_registry,
            #[cfg(feature = "image-registry")]
            cell_pixel_size,
            #[cfg(feature = "image-registry")]
            image_dirty,
        }
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
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
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
        let handle = self.spawn_pane_runtime(session_id, pane_meta);
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

    fn focus_pane_target(&mut self, session_id: SessionId, target: &PaneSelector) -> Result<Uuid> {
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
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
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

    fn restart_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> Result<Uuid> {
        let pane_meta = {
            let session = self
                .runtimes
                .get(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let pane_id =
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let pane = session
                .panes
                .get(&pane_id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            PaneRuntimeMeta {
                id: pane_id,
                name: pane.meta.name.clone(),
                shell: pane.meta.shell.clone(),
            }
        };

        let old_pane = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            session
                .panes
                .remove(&pane_meta.id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?
        };
        tokio::spawn(async move {
            shutdown_pane_handle(old_pane).await;
        });

        let new_pane = self.spawn_pane_runtime(session_id, pane_meta.clone());
        let client_ids = {
            let session = self
                .runtimes
                .get(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            session.attached_clients.iter().copied().collect::<Vec<_>>()
        };
        for client_id in client_ids {
            if let Ok(mut output) = new_pane.output_buffer.lock() {
                output.register_client_at_tail(client_id);
            }
        }
        if let Ok(mut reason) = new_pane.exit_reason.lock() {
            *reason = None;
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        session.panes.insert(pane_meta.id, new_pane);
        session.focused_pane_id = pane_meta.id;
        self.apply_stored_attach_viewport(session_id);
        Ok(pane_meta.id)
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
            resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
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

    #[allow(clippy::cast_possible_truncation)]
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
                    state: pane_state_for_handle(pane),
                    state_reason: pane_state_reason_for_handle(pane),
                })
            })
            .collect::<Vec<_>>();
        Ok(panes)
    }

    #[allow(clippy::cast_possible_truncation)]
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
                    state: pane_state_for_handle(pane),
                    state_reason: pane_state_reason_for_handle(pane),
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

    #[allow(clippy::cast_possible_truncation)]
    fn attach_snapshot_state(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes_per_pane: usize,
    ) -> Result<AttachSnapshotState, SessionRuntimeError> {
        let session = self
            .runtimes
            .get_mut(&session_id)
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
                    state: pane_state_for_handle(pane),
                    state_reason: pane_state_reason_for_handle(pane),
                })
            })
            .collect::<Vec<_>>();

        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        for pane_id in pane_ids {
            let Some(pane) = session.panes.get_mut(&pane_id) else {
                continue;
            };
            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol { pane_id, protocol });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode { pane_id, mode });
            let allowed = per_pane_budget.min(budget_remaining);
            let mut output = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            let read = output.read_recent_with_offsets(allowed);
            output.advance_client_to_end(client_id);
            drop(output);

            budget_remaining = budget_remaining.saturating_sub(read.bytes.len());
            pane.output_dirty.store(false, Ordering::SeqCst);
            let sync_update_active = pane.sync_update_in_progress.load(Ordering::SeqCst);
            chunks.push(AttachPaneChunk {
                pane_id,
                data: read.bytes,
                stream_start: read.stream_start,
                stream_end: read.stream_end,
                stream_gap: read.stream_gap,
                sync_update_active,
            });
        }

        Ok(AttachSnapshotState {
            session_id,
            focused_pane_id: session.focused_pane_id,
            panes,
            layout_root: ipc_layout_from_runtime(&session.layout_root),
            scene,
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
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
        let chunks = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?;
            if !session.attached_clients.contains(&client_id) {
                return Err(SessionRuntimeError::NotAttached);
            }

            let mut chunks = Vec::new();
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
                let read = output.read_for_client(client_id, allowed);
                drop(output);
                budget_remaining = budget_remaining.saturating_sub(read.bytes.len());
                let sync_update_active = pane.sync_update_in_progress.load(Ordering::SeqCst);
                chunks.push(AttachPaneChunk {
                    pane_id: *pane_id,
                    data: read.bytes,
                    stream_start: read.stream_start,
                    stream_end: read.stream_end,
                    stream_gap: read.stream_gap,
                    sync_update_active,
                });
            }
            chunks
        };

        Ok(chunks)
    }

    fn attach_pane_snapshot_state(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes_per_pane: usize,
    ) -> Result<AttachPaneSnapshotState, SessionRuntimeError> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let mut seen = BTreeSet::new();

        for pane_id in pane_ids {
            if !seen.insert(*pane_id) {
                continue;
            }

            let Some(pane) = session.panes.get_mut(pane_id) else {
                continue;
            };

            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol {
                pane_id: *pane_id,
                protocol,
            });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode {
                pane_id: *pane_id,
                mode,
            });

            let allowed = per_pane_budget.min(budget_remaining);
            let mut output = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            let read = output.read_recent_with_offsets(allowed);
            output.advance_client_to_end(client_id);
            drop(output);

            budget_remaining = budget_remaining.saturating_sub(read.bytes.len());
            pane.output_dirty.store(false, Ordering::SeqCst);
            let sync_update_active = pane.sync_update_in_progress.load(Ordering::SeqCst);
            chunks.push(AttachPaneChunk {
                pane_id: *pane_id,
                data: read.bytes,
                stream_start: read.stream_start,
                stream_end: read.stream_end,
                stream_gap: read.stream_gap,
                sync_update_active,
            });
        }

        Ok(AttachPaneSnapshotState {
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
        })
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

        let _pane = runtime
            .panes
            .get(&runtime.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

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

    #[allow(clippy::too_many_arguments)]
    fn set_attach_viewport(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
        cell_pixel_width: u16,
        cell_pixel_height: u16,
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

        // Update cell pixel dimensions for image placement sizing.
        #[cfg(feature = "image-registry")]
        if cell_pixel_width > 0 && cell_pixel_height > 0 {
            for pane in runtime.panes.values() {
                if let Ok(mut size) = pane.cell_pixel_size.lock() {
                    *size = (cell_pixel_width, cell_pixel_height);
                }
            }
        }
        #[cfg(not(feature = "image-registry"))]
        let _ = (cell_pixel_width, cell_pixel_height);

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
            return Ok(Vec::new());
        }

        let mut output = pane
            .output_buffer
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        let read = output.read_for_client(client_id, max_bytes);
        drop(output);

        Ok(read.bytes)
    }
}

fn resolve_pane_id_from_selector(
    runtime: &SessionRuntimeHandle,
    selector: &PaneSelector,
) -> Option<Uuid> {
    match selector {
        PaneSelector::Active => runtime
            .panes
            .contains_key(&runtime.focused_pane_id)
            .then_some(runtime.focused_pane_id),
        PaneSelector::ById(id) => runtime.panes.contains_key(id).then_some(*id),
        PaneSelector::ByIndex(index) => {
            if *index == 0 {
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

fn pane_state_for_handle(pane: &PaneRuntimeHandle) -> PaneState {
    if pane.exited.load(Ordering::SeqCst) {
        PaneState::Exited
    } else {
        PaneState::Running
    }
}

fn pane_state_reason_for_handle(pane: &PaneRuntimeHandle) -> Option<String> {
    pane.exit_reason
        .lock()
        .ok()
        .and_then(|reason| reason.clone())
}

impl BmuxServer {
    #[allow(clippy::too_many_arguments)]
    fn new_with_snapshot(
        endpoint: IpcEndpoint,
        snapshot_manager: Option<SnapshotManager>,
        server_control_principal_id: Uuid,
        recordings_dir: std::path::PathBuf,
        rolling_recordings_dir: std::path::PathBuf,
        segment_mb: usize,
        retention_days: u64,
        rolling_recording_auto_start: bool,
        rolling_recording_defaults: RollingRecordingSettings,
    ) -> Self {
        let snapshot_runtime =
            snapshot_manager.map_or_else(SnapshotRuntime::disabled, SnapshotRuntime::with_manager);

        let config = BmuxConfig::load().unwrap_or_default();
        let shell = resolve_server_shell(&config);
        let pane_term = resolve_server_pane_term(&config);
        let protocol_profile = protocol_profile_for_term(&pane_term);

        // Resolve image payload compression codec from config.
        #[cfg(feature = "image-registry")]
        let payload_codec: Option<Arc<dyn bmux_ipc::compression::CompressionCodec>> =
            if config.behavior.compression.enabled {
                resolve_payload_codec_from_config(&config.behavior.compression)
            } else {
                None
            };
        let (shutdown_tx, _) = watch::channel(false);
        let (pane_exit_tx, pane_exit_rx) = mpsc::unbounded_channel();
        let rolling_runtime_available = rolling_recording_defaults.is_available();
        let manual_recording_runtime = Arc::new(Mutex::new(RecordingRuntime::new(
            recordings_dir,
            segment_mb,
            retention_days,
        )));
        let rolling_recording_runtime = Arc::new(Mutex::new(if rolling_runtime_available {
            Some(RecordingRuntime::new_rolling(
                rolling_recordings_dir.clone(),
                segment_mb,
                rolling_recording_defaults.window_secs,
            ))
        } else {
            None
        }));
        let (event_broadcast_tx, _) =
            tokio::sync::broadcast::channel::<Event>(EVENT_PUSH_CHANNEL_CAPACITY);
        Self {
            endpoint,
            state: Arc::new(ServerState {
                session_manager: Mutex::new(SessionManager::new()),
                session_runtimes: Mutex::new(SessionRuntimeManager::new(
                    shell,
                    pane_term,
                    protocol_profile,
                    pane_exit_tx,
                    Arc::clone(&manual_recording_runtime),
                    Arc::clone(&rolling_recording_runtime),
                    event_broadcast_tx.clone(),
                )),
                attach_tokens: Mutex::new(AttachTokenManager::new(ATTACH_TOKEN_TTL)),
                follow_state: Mutex::new(FollowState::default()),
                context_state: Mutex::new(ContextState::default()),
                snapshot_runtime: Mutex::new(snapshot_runtime),
                manual_recording_runtime,
                rolling_recording_runtime,
                rolling_recording_auto_start: rolling_recording_auto_start
                    && rolling_runtime_available,
                rolling_recording_defaults,
                performance_settings: Arc::new(Mutex::new(
                    PerformanceCaptureSettings::from_config(&config),
                )),
                rolling_recordings_dir,
                rolling_recording_segment_mb: segment_mb,
                operation_lock: AsyncMutex::new(()),
                event_hub: Mutex::new(EventHub::new(1024)),
                event_broadcast: event_broadcast_tx,
                control_catalog_revision: AtomicU64::new(1),
                client_capabilities: Mutex::new(BTreeMap::new()),
                client_principals: Mutex::new(BTreeMap::new()),
                server_control_principal_id,
                handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
                pane_exit_rx: AsyncMutex::new(pane_exit_rx),
                service_registry: Mutex::new(ServiceRegistry::default()),
                service_resolver: Mutex::new(None),
                #[cfg(feature = "image-registry")]
                payload_codec,
            }),
            shutdown_tx,
        }
    }

    /// Create a server with an explicit endpoint.
    #[must_use]
    pub fn new(endpoint: IpcEndpoint) -> Self {
        let paths = ConfigPaths::default();
        let config = BmuxConfig::load_from_path(&paths.config_file()).unwrap_or_default();
        let rolling_defaults = rolling_recording_settings_from_config(&config);
        Self::new_with_snapshot(
            endpoint,
            None,
            Uuid::new_v4(),
            config.recordings_dir(&paths),
            paths.rolling_recordings_dir(),
            config.recording.segment_mb,
            config.recording.retention_days,
            config.recording.enabled,
            rolling_defaults,
        )
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
        let rolling_defaults = RollingRecordingSettings {
            window_secs: rolling_window_secs,
            event_kinds: normalize_recording_event_kinds(rolling_event_kinds),
        };
        Self::new_with_snapshot(
            endpoint,
            Some(snapshot_manager),
            server_control_principal_id,
            config.recordings_dir(paths),
            paths.rolling_recordings_dir(),
            config.recording.segment_mb,
            config.recording.retention_days,
            rolling_recording_auto_start,
            rolling_defaults,
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
        let wrapped: Arc<ServiceResolverHandler> =
            Arc::new(move |route, payload| Box::pin(resolver(route, payload)));
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

        if let Err(error) = restore_snapshot_if_present(&self.state) {
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Err(format!("{error:#}")));
            }
            return Err(error);
        }

        if let Err(error) = ensure_rolling_recording_started(&self.state) {
            warn!("failed to initialize rolling recording runtime: {error:#}");
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
        let recording_prune_runtime = Arc::clone(&self.state.manual_recording_runtime);
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

        let removed_runtimes = self.state.session_runtimes.lock().map_or_else(
            |_| Vec::new(),
            |mut runtime_manager| runtime_manager.remove_all_runtimes(),
        );
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
    let mut negotiated_frame_codec: Option<
        std::sync::Arc<dyn bmux_ipc::compression::CompressionCodec>,
    > = None;
    let mut negotiated_capabilities: Option<BTreeSet<String>> = None;

    // ── Handshake (serial, before split) ─────────────────────────────────

    let first_envelope = tokio::time::timeout(state.handshake_timeout, stream.recv_envelope())
        .await
        .context("handshake timed out")??;

    let handshake = parse_request(&first_envelope)?;
    match handshake {
        Request::Hello {
            protocol_version,
            client_name,
            principal_id,
        } => {
            if protocol_version != ProtocolVersion::current() {
                send_error(
                    &mut stream,
                    first_envelope.request_id,
                    ErrorCode::VersionMismatch,
                    format!(
                        "unsupported protocol version {}; expected {}",
                        protocol_version.0,
                        ProtocolVersion::current().0
                    ),
                )
                .await?;
                return Ok(());
            }
            client_principal_id = principal_id;
            debug!("accepted client handshake (legacy): {client_name}");
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
        }
        Request::HelloV2 {
            contract,
            client_name,
            principal_id,
        } => {
            let server_contract = ProtocolContract::current(default_supported_capabilities());
            match negotiate_protocol(&contract, &server_contract, CORE_PROTOCOL_CAPABILITIES) {
                Ok(negotiated) => {
                    client_principal_id = principal_id;
                    negotiated_capabilities = Some(
                        negotiated
                            .capabilities
                            .iter()
                            .cloned()
                            .collect::<BTreeSet<_>>(),
                    );
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
        }
        _ => {
            send_error(
                &mut stream,
                first_envelope.request_id,
                ErrorCode::InvalidRequest,
                "first request must be hello".to_string(),
            )
            .await?;
            return Ok(());
        }
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
    let client_capabilities = negotiated_capabilities.unwrap_or_else(|| {
        CORE_PROTOCOL_CAPABILITIES
            .iter()
            .map(|entry| (*entry).to_string())
            .collect::<BTreeSet<_>>()
    });
    {
        let mut capabilities = state
            .client_capabilities
            .lock()
            .map_err(|_| anyhow::anyhow!("client capability map lock poisoned"))?;
        capabilities.insert(client_id, client_capabilities.clone());
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
                    negotiated_frame_codec.as_deref(),
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
        record_to_all_runtimes(
            &state.manual_recording_runtime,
            &state.rolling_recording_runtime,
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
                record_to_all_runtimes(
                    &state.manual_recording_runtime,
                    &state.rolling_recording_runtime,
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
                record_to_all_runtimes(
                    &state.manual_recording_runtime,
                    &state.rolling_recording_runtime,
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
            }
        }
        match send_response_via_channel(
            &frame_tx,
            envelope.request_id,
            &response,
            negotiated_frame_codec.as_deref(),
        ) {
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
                    negotiated_frame_codec.as_deref(),
                )?;
            }
            Err(err) => return Err(err),
        }

        // After responding to EnableEventPush, spawn the event push task.
        // It receives events from the broadcast channel and forwards them
        // as serialized frames through the writer channel.
        //
        // For `PaneOutputAvailable` events, the task reads the actual output
        // data from the per-client cursor in the pane's OutputFanoutBuffer
        // and sends it inline as a `PaneOutput` event.  This eliminates the
        // round-trip `AttachPaneOutputBatch` request the client would
        // otherwise need, reducing output latency to a single one-way push.
        if is_enable_push && event_push_task.is_none() {
            let mut event_rx = state.event_broadcast.subscribe();
            let push_frame_tx = frame_tx.clone();
            let push_frame_codec = negotiated_frame_codec.clone();
            let push_state = Arc::clone(&state);
            let push_client_id = client_id;
            let push_client_capabilities = client_capabilities.clone();
            event_push_task = Some(tokio::spawn(async move {
                let push_perf_settings_state = Arc::clone(&push_state.performance_settings);
                let mut push_perf_settings = push_perf_settings_state
                    .lock()
                    .map_or_else(|_| PerformanceCaptureSettings::default(), |guard| *guard);
                let mut push_perf_rate_limiter =
                    PerformanceEventRateLimiter::new(push_perf_settings);
                let mut push_perf_window = Duration::from_millis(push_perf_settings.window_ms);
                let mut push_window_started_at = Instant::now();
                let mut push_window_sent_events = 0_u64;
                let mut push_window_sent_bytes = 0_u64;
                let mut push_window_lagged_events = 0_u64;
                let mut push_window_lagged_receives = 0_u64;
                loop {
                    let latest_push_perf_settings = push_perf_settings_state
                        .lock()
                        .map_or(push_perf_settings, |guard| *guard);
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
                                    let (read, sync_update_active) = {
                                        let Ok(runtime_mgr) = push_state.session_runtimes.lock()
                                        else {
                                            continue;
                                        };
                                        let Some(runtime) =
                                            runtime_mgr.runtimes.get(&SessionId(session_id))
                                        else {
                                            continue;
                                        };
                                        // Only read output for panes in
                                        // sessions this client is attached to.
                                        if !runtime.attached_clients.contains(&push_client_id) {
                                            continue;
                                        }
                                        let Some(pane) = runtime.panes.get(&pane_id) else {
                                            continue;
                                        };
                                        pane.output_dirty
                                            .store(false, std::sync::atomic::Ordering::SeqCst);
                                        let Ok(mut buf) = pane.output_buffer.lock() else {
                                            continue;
                                        };
                                        let read = buf.read_for_client(
                                            push_client_id,
                                            RESPONSE_OUTPUT_BUDGET,
                                        );
                                        let sync_update_active =
                                            pane.sync_update_in_progress.load(Ordering::SeqCst);
                                        (read, sync_update_active)
                                    };
                                    let Some(pane_output_event) = pane_output_event_from_read(
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

                            if !event_supported_by_capability_set(&push_client_capabilities, &event)
                            {
                                continue;
                            }

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
                                            &push_state.manual_recording_runtime,
                                            &push_state.rolling_recording_runtime,
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
    {
        let mut capabilities = state
            .client_capabilities
            .lock()
            .map_err(|_| anyhow::anyhow!("client capability map lock poisoned"))?;
        capabilities.remove(&client_id);
    }
    mark_snapshot_dirty(&state)?;
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
        | Event::PerformanceSettingsUpdated { .. }
        | Event::ControlCatalogChanged { .. } => None,
    };
    record_to_all_runtimes(
        &state.manual_recording_runtime,
        &state.rolling_recording_runtime,
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

const fn event_required_capability(event: &Event) -> Option<&'static str> {
    match event {
        Event::ControlCatalogChanged { .. } => Some(CAPABILITY_CONTROL_CATALOG_SYNC),
        _ => None,
    }
}

fn event_supported_by_capability_set(capabilities: &BTreeSet<String>, event: &Event) -> bool {
    event_required_capability(event)
        .is_none_or(|required_capability| capabilities.contains(required_capability))
}

fn normalize_control_catalog_scopes(scopes: &[ControlCatalogScope]) -> Vec<ControlCatalogScope> {
    let mut normalized = Vec::new();
    for scope in [
        ControlCatalogScope::Sessions,
        ControlCatalogScope::Contexts,
        ControlCatalogScope::Bindings,
    ] {
        if scopes.contains(&scope) {
            normalized.push(scope);
        }
    }
    normalized
}

fn current_control_catalog_revision(state: &Arc<ServerState>) -> u64 {
    state.control_catalog_revision.load(Ordering::SeqCst)
}

fn emit_control_catalog_changed(
    state: &Arc<ServerState>,
    scopes: &[ControlCatalogScope],
    full_resync: bool,
) -> Result<()> {
    let scopes = normalize_control_catalog_scopes(scopes);
    if scopes.is_empty() {
        return Ok(());
    }
    let revision = state
        .control_catalog_revision
        .fetch_add(1, Ordering::SeqCst)
        .saturating_add(1);
    emit_event(
        state,
        Event::ControlCatalogChanged {
            revision,
            scopes,
            full_resync,
        },
    )
}

fn build_control_catalog_snapshot(state: &Arc<ServerState>) -> Result<ControlCatalogSnapshot> {
    let revision = current_control_catalog_revision(state);
    let sessions = state
        .session_manager
        .lock()
        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?
        .list_sessions()
        .into_iter()
        .map(|session| SessionSummary {
            id: session.id.0,
            name: session.name,
            client_count: session.client_count,
        })
        .collect::<Vec<_>>();

    let (contexts, context_session_bindings) = {
        let context_state = state
            .context_state
            .lock()
            .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
        let mut contexts = context_state.list();
        let context_session_bindings = context_state
            .session_by_context
            .iter()
            .map(|(context_id, session_id)| ContextSessionBindingSummary {
                context_id: *context_id,
                session_id: session_id.0,
            })
            .collect::<Vec<_>>();
        drop(context_state);
        let binding_by_context = context_session_bindings
            .iter()
            .map(|binding| (binding.context_id, binding.session_id))
            .collect::<BTreeMap<_, _>>();
        for context in &mut contexts {
            if let Some(session_id) = binding_by_context.get(&context.id) {
                context.attributes.insert(
                    CONTEXT_SESSION_ID_ATTRIBUTE.to_string(),
                    session_id.to_string(),
                );
            }
        }
        (contexts, context_session_bindings)
    };

    Ok(ControlCatalogSnapshot {
        revision,
        sessions,
        contexts,
        context_session_bindings,
    })
}

fn lag_recovery_attach_view_events_for_client(
    state: &Arc<ServerState>,
    client_id: ClientId,
) -> Vec<Event> {
    let revisions = {
        let Ok(mut runtime_manager) = state.session_runtimes.lock() else {
            return Vec::new();
        };

        runtime_manager
            .runtimes
            .iter_mut()
            .filter_map(|(session_id, runtime)| {
                if !runtime.attached_clients.contains(&client_id) {
                    return None;
                }
                runtime.attach_view_revision = runtime.attach_view_revision.saturating_add(1);
                Some((*session_id, runtime.attach_view_revision))
            })
            .collect::<Vec<_>>()
    };

    if revisions.is_empty() {
        return Vec::new();
    }

    let mut events = revisions
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
        .collect::<Vec<_>>();

    events.push(Event::ControlCatalogChanged {
        revision: current_control_catalog_revision(state),
        scopes: vec![
            ControlCatalogScope::Sessions,
            ControlCatalogScope::Contexts,
            ControlCatalogScope::Bindings,
        ],
        full_resync: true,
    });

    events
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
        let Some(revision) = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?
            .bump_attach_view_revision(session_id)
        else {
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
    state
        .event_hub
        .lock()
        .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?
        .unsubscribe(client_id);
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
    drop(manager);

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
    drop(follow_state);

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
    drop(runtime);
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

#[allow(clippy::too_many_lines)]
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
        drop(follow_state);

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
        drop(context_state);
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

#[derive(Default)]
struct RollingUsageCounters {
    bytes: u64,
    files: u64,
    directories: u64,
}

fn collect_rolling_usage(root: &std::path::Path) -> Result<RecordingRollingUsage> {
    let mut counters = RollingUsageCounters::default();
    if root.exists() {
        collect_rolling_usage_recursive(root, &mut counters)?;
    }
    Ok(RecordingRollingUsage {
        bytes: counters.bytes,
        files: counters.files,
        directories: counters.directories,
        recording_dirs: count_rolling_recording_dirs(root)?,
    })
}

fn collect_rolling_usage_recursive(
    path: &std::path::Path,
    counters: &mut RollingUsageCounters,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed reading metadata for {}", path.display()))?;
    if metadata.is_file() {
        counters.files = counters.files.saturating_add(1);
        counters.bytes = counters.bytes.saturating_add(metadata.len());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    counters.directories = counters.directories.saturating_add(1);
    for entry in std::fs::read_dir(path)
        .with_context(|| format!("failed reading rolling directory {}", path.display()))?
    {
        let entry = entry?;
        collect_rolling_usage_recursive(&entry.path(), counters)?;
    }
    Ok(())
}

fn count_rolling_recording_dirs(root: &std::path::Path) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut count = 0_u64;
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("failed reading rolling directory {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry.path().join("manifest.json").exists() {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

fn clear_rolling_root(root: &std::path::Path) -> Result<()> {
    if root.exists() {
        std::fs::remove_dir_all(root)
            .with_context(|| format!("failed clearing rolling directory {}", root.display()))?;
    }
    std::fs::create_dir_all(root)
        .with_context(|| format!("failed creating rolling directory {}", root.display()))?;
    Ok(())
}

fn rolling_status_snapshot(state: &Arc<ServerState>) -> Result<RecordingRollingStatus> {
    let mut available = state.rolling_recording_defaults.is_available();
    let mut active = None;
    let mut rolling_window_secs = (state.rolling_recording_defaults.window_secs > 0)
        .then_some(state.rolling_recording_defaults.window_secs);
    let mut event_kinds = state.rolling_recording_defaults.event_kinds.clone();

    {
        let runtime = state
            .rolling_recording_runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
        if let Some(runtime) = runtime.as_ref() {
            available = true;
            rolling_window_secs = runtime.rolling_window_secs();
            if let Some(summary) = runtime.status().active {
                event_kinds.clone_from(&summary.event_kinds);
                active = Some(summary);
            }
        }
    }

    Ok(RecordingRollingStatus {
        root_path: state.rolling_recordings_dir.to_string_lossy().to_string(),
        auto_start: state.rolling_recording_auto_start,
        available,
        active,
        rolling_window_secs,
        event_kinds: normalize_recording_event_kinds(&event_kinds),
        usage: collect_rolling_usage(&state.rolling_recordings_dir)?,
    })
}

fn ensure_rolling_recording_started(state: &Arc<ServerState>) -> Result<()> {
    if !state.rolling_recording_auto_start {
        return Ok(());
    }
    if !state.rolling_recording_defaults.is_available() {
        return Ok(());
    }

    let mut guard = state
        .rolling_recording_runtime
        .lock()
        .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
    if guard.is_none() {
        *guard = Some(RecordingRuntime::new_rolling(
            state.rolling_recordings_dir.clone(),
            state.rolling_recording_segment_mb,
            state.rolling_recording_defaults.window_secs,
        ));
    }
    let runtime = guard
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("rolling recording runtime missing after init"))?;
    if runtime.status().active.is_some() {
        return Ok(());
    }

    let summary =
        start_rolling_recording_runtime(state, runtime, &state.rolling_recording_defaults, None)?;
    let window_secs = runtime.rolling_window_secs().unwrap_or(0);
    drop(guard);
    info!(
        "rolling recording started: {} path={} window_secs={}",
        summary.id, summary.path, window_secs
    );
    Ok(())
}

fn start_rolling_recording_runtime(
    _state: &Arc<ServerState>,
    runtime: &mut RecordingRuntime,
    settings: &RollingRecordingSettings,
    name: Option<String>,
) -> Result<RecordingSummary> {
    if let Err(error) = runtime.delete_all() {
        warn!("failed cleaning hidden rolling recordings root: {error:#}");
    }

    runtime.start(
        None,
        settings.capture_input(),
        name,
        RecordingProfile::Functional,
        settings.event_kinds.clone(),
    )
}

fn recover_rolling_runtime_after_missing_cut_path(
    state: &Arc<ServerState>,
    runtime: &mut RecordingRuntime,
    active_before_cut: Option<RecordingSummary>,
) -> Result<Option<RecordingSummary>> {
    let window_secs = runtime
        .rolling_window_secs()
        .unwrap_or(state.rolling_recording_defaults.window_secs);
    let event_kinds = active_before_cut.as_ref().map_or_else(
        || state.rolling_recording_defaults.event_kinds.clone(),
        |summary| summary.event_kinds.clone(),
    );
    let settings = RollingRecordingSettings {
        window_secs,
        event_kinds: normalize_recording_event_kinds(&event_kinds),
    };
    if !settings.is_available() {
        return Ok(None);
    }

    if runtime.rolling_window_secs() != Some(settings.window_secs) {
        *runtime = RecordingRuntime::new_rolling(
            state.rolling_recordings_dir.clone(),
            state.rolling_recording_segment_mb,
            settings.window_secs,
        );
    }

    let name = active_before_cut.and_then(|summary| summary.name);
    let restarted = start_rolling_recording_runtime(state, runtime, &settings, name)?;
    Ok(Some(restarted))
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

#[allow(clippy::too_many_lines)]
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
                &runtime_panes,
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
        drop(session_manager);
        drop(runtime_manager);
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
        drop(follow_state);
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

fn reap_exited_pane(state: &Arc<ServerState>, session_id: SessionId, pane_id: Uuid) -> Result<()> {
    let state_reason = {
        let runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
        runtime_manager
            .runtimes
            .get(&session_id)
            .and_then(|session| session.panes.get(&pane_id))
            .and_then(pane_state_reason_for_handle)
    };
    emit_event(
        state,
        Event::PaneExited {
            session_id: session_id.0,
            pane_id,
            reason: state_reason,
        },
    )?;
    emit_attach_view_changed_for_layout(state, session_id)?;

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
                if let Err(error) = reap_exited_pane(&state, event.session_id, event.pane_id) {
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
        let mut runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
        if !runtime_manager.runtimes.contains_key(&session_id) {
            runtime_manager.start_runtime(session_id).with_context(|| {
                format!(
                    "failed starting missing session runtime for existing session {}",
                    session_id.0
                )
            })?;
        }
        drop(runtime_manager);
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

    let removed_context_ids = prune_context_mappings_for_session(state, session_id)?;
    if !removed_context_ids.is_empty() {
        emit_control_catalog_changed(
            state,
            &[ControlCatalogScope::Contexts, ControlCatalogScope::Bindings],
            false,
        )?;
    }

    Ok(false)
}

#[allow(clippy::too_many_lines)]
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
        Request::Hello { .. } | Request::HelloV2 { .. } => Response::Err(ErrorResponse {
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
                        drop(context_state);
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
            let session_id = match resolve_session_request_session_id(
                &manager,
                session.as_ref(),
                selected_session.as_ref(),
            ) {
                Ok(session_id) => session_id,
                Err(response) => return Ok(Response::Err(response)),
            };
            drop(manager);

            let pane_result = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?
                .list_panes(session_id);
            let panes = match pane_result {
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
                match resolve_session_request_session_id(
                    &manager,
                    session.as_ref(),
                    selected_session.as_ref(),
                ) {
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
                match resolve_session_request_session_id(
                    &manager,
                    session.as_ref(),
                    selected_session.as_ref(),
                ) {
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
                (Some(target), None) => runtime_manager.focus_pane_target(session_id, &target),
                (None, Some(direction)) => runtime_manager.focus_pane(session_id, direction),
                (None, None) => {
                    runtime_manager.focus_pane_target(session_id, &PaneSelector::Active)
                }
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
                match resolve_session_request_session_id(
                    &manager,
                    session.as_ref(),
                    selected_session.as_ref(),
                ) {
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
                match resolve_session_request_session_id(
                    &manager,
                    session.as_ref(),
                    selected_session.as_ref(),
                ) {
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

            let (closed_pane_id, removed_session) = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?
                .close_pane(session_id, target)
                .map_err(|error| anyhow::anyhow!("failed closing pane: {error:#}"))?;

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
        Request::RestartPane { session, target } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                match resolve_session_request_session_id(
                    &manager,
                    session.as_ref(),
                    selected_session.as_ref(),
                ) {
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
                "pane.restart",
            )
            .await
            {
                return Ok(Response::Err(response));
            }

            let pane_id = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager
                    .restart_pane(session_id, target)
                    .map_err(|error| anyhow::anyhow!("failed restarting pane: {error:#}"))?
            };

            emit_event(
                state,
                Event::PaneRestarted {
                    session_id: session_id.0,
                    pane_id,
                },
            )?;
            emit_attach_view_changed_for_layout(state, session_id)?;
            Response::Ok(ResponsePayload::PaneRestarted {
                id: pane_id,
                session_id: session_id.0,
            })
        }
        Request::ZoomPane { session } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                match resolve_session_request_session_id(
                    &manager,
                    session.as_ref(),
                    selected_session.as_ref(),
                ) {
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
            let sessions = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?
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

            let (context, bind_result) = {
                let mut context_state = state
                    .context_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?;
                let context = context_state.create(client_id, name, attributes);
                let bind_result = context_state.bind_session(context.id, session_id);
                drop(context_state);
                (context, bind_result)
            };
            if let Err(message) = bind_result {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: message.to_string(),
                }));
            }
            Response::Ok(ResponsePayload::ContextCreated { context })
        }
        Request::ListContexts => {
            let contexts = state
                .context_state
                .lock()
                .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?
                .list();
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
                        drop(context_state);
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
            let context = state
                .context_state
                .lock()
                .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?
                .current_for_client(client_id);
            Response::Ok(ResponsePayload::CurrentContext { context })
        }
        Request::ControlCatalogSnapshot { since_revision: _ } => {
            let snapshot = build_control_catalog_snapshot(state)?;
            Response::Ok(ResponsePayload::ControlCatalogSnapshot { snapshot })
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

                let mut grant = state
                    .attach_tokens
                    .lock()
                    .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?
                    .issue(next_session_id);
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
                let removed_context_ids =
                    prune_context_mappings_for_session(state, next_session_id)?;
                if !removed_context_ids.is_empty() {
                    emit_control_catalog_changed(
                        state,
                        &[ControlCatalogScope::Contexts, ControlCatalogScope::Bindings],
                        false,
                    )?;
                }
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
                drop(context_state);

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

                let mut grant = state
                    .attach_tokens
                    .lock()
                    .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?
                    .issue(next_session_id);
                grant.context_id = Some(selected_context_id);
                Response::Ok(ResponsePayload::Attached { grant })
            } else {
                drop(manager);
                let removed_context_ids =
                    prune_context_mappings_for_session(state, next_session_id)?;
                if !removed_context_ids.is_empty() {
                    emit_control_catalog_changed(
                        state,
                        &[ControlCatalogScope::Contexts, ControlCatalogScope::Bindings],
                        false,
                    )?;
                }
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

            if !ensure_attach_session_exists(state, session_id).await? {
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
                        match runtime_manager.begin_attach(session_id, client_id) {
                            Ok(()) => Ok(()),
                            Err(SessionRuntimeError::NotFound) => {
                                if let Err(error) = runtime_manager.start_runtime(session_id) {
                                    warn!(
                                        "failed restarting missing session runtime {} before attach-open: {error:#}",
                                        session_id.0
                                    );
                                }
                                runtime_manager.begin_attach(session_id, client_id)
                            }
                            Err(SessionRuntimeError::Closed) => {
                                let _ = runtime_manager.remove_runtime(session_id);
                                if let Err(error) = runtime_manager.start_runtime(session_id) {
                                    warn!(
                                        "failed restarting closed session runtime {} before attach-open: {error:#}",
                                        session_id.0
                                    );
                                }
                                runtime_manager.begin_attach(session_id, client_id)
                            }
                            Err(error) => Err(error),
                        }
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
                        Err(SessionRuntimeError::NotFound | SessionRuntimeError::Closed) => {
                            Response::Err(ErrorResponse {
                                code: ErrorCode::NotFound,
                                message: format!("session runtime not found: {}", session_id.0),
                            })
                        }
                        Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                            code: ErrorCode::Internal,
                            message: "failed opening attach stream".to_string(),
                        }),
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
                    record_to_all_runtimes(
                        &state.manual_recording_runtime,
                        &state.rolling_recording_runtime,
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
                Err(SessionRuntimeError::Closed) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: "active pane is closed".to_string(),
                }),
            }
        }
        Request::AttachSetViewport {
            session_id,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
            cell_pixel_width,
            cell_pixel_height,
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
                    cell_pixel_width,
                    cell_pixel_height,
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
                Err(SessionRuntimeError::Closed) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: "active pane is closed".to_string(),
                }),
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
                Err(SessionRuntimeError::Closed) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: "active pane is closed".to_string(),
                }),
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
                let mut runtime_manager = state
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
                    pane_mouse_protocols: snapshot.pane_mouse_protocols,
                    pane_input_modes: snapshot.pane_input_modes,
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
                Err(SessionRuntimeError::Closed) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: "active pane is closed".to_string(),
                }),
            }
        }
        Request::AttachPaneSnapshot {
            session_id,
            pane_ids,
            max_bytes_per_pane,
        } => {
            let session_id = SessionId(session_id);
            if !ensure_attach_session_exists(state, session_id).await? {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }));
            }

            let pane_snapshot = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.attach_pane_snapshot_state(
                    session_id,
                    client_id,
                    &pane_ids,
                    max_bytes_per_pane,
                )
            };

            match pane_snapshot {
                Ok(snapshot) => Response::Ok(ResponsePayload::AttachPaneSnapshot {
                    chunks: snapshot.chunks,
                    pane_mouse_protocols: snapshot.pane_mouse_protocols,
                    pane_input_modes: snapshot.pane_input_modes,
                }),
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
            let (chunks, output_still_pending) = {
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
                let chunks = runtime_manager
                    .read_pane_output_batch(session_id, client_id, &pane_ids, max_bytes);
                // Re-check output_dirty: if the PTY reader pushed new data
                // between the clear above and this check, the client should
                // keep draining instead of proceeding to render.
                let still_pending = runtime_manager.runtimes.get(&session_id).is_some_and(|rt| {
                    pane_ids.iter().any(|pane_id| {
                        rt.panes
                            .get(pane_id)
                            .is_some_and(|p| p.output_dirty.load(Ordering::SeqCst))
                    })
                });
                drop(runtime_manager);
                (chunks, still_pending)
            };
            match chunks {
                Ok(chunks) => Response::Ok(ResponsePayload::AttachPaneOutputBatch {
                    chunks,
                    output_still_pending,
                }),
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
        Request::AttachPaneImages {
            session_id,
            pane_ids,
            since_sequences,
        } => {
            let session_id = SessionId(session_id);
            if !ensure_attach_session_exists(state, session_id).await? {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {}", session_id.0),
                }));
            }
            let deltas = {
                let runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                let mut result = Vec::new();
                if let Some(runtime) = runtime_manager.runtimes.get(&session_id) {
                    // Clear image_dirty flags so the PTY reader can re-notify
                    // on the next image change (mirrors output_dirty pattern).
                    for pane_id in &pane_ids {
                        if let Some(pane) = runtime.panes.get(pane_id) {
                            #[cfg(feature = "image-registry")]
                            pane.image_dirty.store(false, Ordering::SeqCst);
                            #[cfg(not(feature = "image-registry"))]
                            let _ = pane;
                        }
                    }
                    for (i, pane_id) in pane_ids.iter().enumerate() {
                        let since = since_sequences.get(i).copied().unwrap_or(0);
                        if let Some(pane) = runtime.panes.get(pane_id) {
                            #[cfg(feature = "image-registry")]
                            if let Ok(reg) = pane.image_registry.lock() {
                                let delta = reg.delta_since(since);
                                result.push(delta.to_ipc(*pane_id, state.payload_codec.as_deref()));
                            }
                            #[cfg(not(feature = "image-registry"))]
                            {
                                let _ = (pane, since);
                                result.push(bmux_ipc::AttachPaneImageDelta {
                                    pane_id: *pane_id,
                                    added: Vec::new(),
                                    removed: Vec::new(),
                                    sequence: 0,
                                });
                            }
                        }
                    }
                }
                drop(runtime_manager);
                result
            };
            Response::Ok(ResponsePayload::AttachPaneImages { deltas })
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
                state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?
                    .end_attach(current_stream_session, client_id);
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
            state
                .event_hub
                .lock()
                .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?
                .subscribe(client_id);
            Response::Ok(ResponsePayload::EventsSubscribed)
        }
        Request::PollEvents { max_events } => {
            let capabilities = state
                .client_capabilities
                .lock()
                .map_err(|_| anyhow::anyhow!("client capability map lock poisoned"))?
                .get(&client_id)
                .cloned()
                .unwrap_or_default();
            let mut hub = state
                .event_hub
                .lock()
                .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?;
            hub.poll_with_filter(client_id, max_events, |event| {
                event_supported_by_capability_set(&capabilities, event)
            })
            .map_or_else(
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
        Request::RecordingStart {
            session_id,
            capture_input,
            name,
            profile,
            event_kinds,
        } => {
            let mut runtime = state
                .manual_recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            let profile = profile.unwrap_or(RecordingProfile::Functional);
            let event_kinds = event_kinds
                .unwrap_or_else(|| default_recording_event_kinds(profile, capture_input));
            match runtime.start(session_id, capture_input, name, profile, event_kinds) {
                Ok(recording) => {
                    let event = Event::RecordingStarted {
                        recording_id: recording.id,
                        path: recording.path.clone(),
                    };
                    drop(runtime);
                    let _ = emit_event(state, event);
                    Response::Ok(ResponsePayload::RecordingStarted { recording })
                }
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: format!("failed starting recording: {error}"),
                }),
            }
        }
        Request::RecordingStop { recording_id } => {
            let mut runtime = state
                .manual_recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            match runtime.stop(recording_id) {
                Ok(recording) => {
                    let recording_id = recording.id;
                    drop(runtime);
                    let _ = emit_event(state, Event::RecordingStopped { recording_id });
                    Response::Ok(ResponsePayload::RecordingStopped { recording_id })
                }
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: format!("failed stopping recording: {error}"),
                }),
            }
        }
        Request::RecordingStatus => {
            let runtime = state
                .manual_recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
            Response::Ok(ResponsePayload::RecordingStatus {
                status: runtime.status(),
            })
        }
        Request::RecordingList => {
            let runtime = state
                .manual_recording_runtime
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
                .manual_recording_runtime
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
            let payload = RecordingPayload::Custom {
                source,
                name,
                payload,
            };
            let meta = RecordMeta {
                session_id,
                pane_id,
                client_id: Some(client_id.0),
            };

            let mut accepted = false;
            {
                let runtime = state
                    .manual_recording_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
                match runtime.record(RecordingEventKind::Custom, payload.clone(), meta) {
                    Ok(recorded) => accepted |= recorded,
                    Err(error) => {
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::Internal,
                            message: format!("failed writing custom recording event: {error}"),
                        }));
                    }
                }
            }
            {
                let runtime = state
                    .rolling_recording_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
                if let Some(runtime) = runtime.as_ref() {
                    match runtime.record(RecordingEventKind::Custom, payload, meta) {
                        Ok(recorded) => accepted |= recorded,
                        Err(error) => {
                            return Ok(Response::Err(ErrorResponse {
                                code: ErrorCode::Internal,
                                message: format!(
                                    "failed writing custom recording event to rolling runtime: {error}"
                                ),
                            }));
                        }
                    }
                }
            }

            Response::Ok(ResponsePayload::RecordingCustomEventWritten { accepted })
        }
        Request::RecordingDeleteAll => {
            let mut runtime = state
                .manual_recording_runtime
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
                    .manual_recording_runtime
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
        Request::RecordingCut { last_seconds, name } => {
            let output_root = state
                .manual_recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?
                .root_dir()
                .to_path_buf();
            let mut guard = state
                .rolling_recording_runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
            let Some(rt) = guard.as_mut() else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "rolling recording is not enabled".to_string(),
                }));
            };
            let mut restarted_rolling = None::<RecordingSummary>;
            let active_before_cut = rt.status().active;
            let result = match rt.cut(&output_root, last_seconds, name) {
                Ok(recording) => Ok(recording),
                Err(error) => {
                    recording::cut_missing_active_recording_dir(&error).map_or_else(
                        || Err(error),
                        |missing_path| {
                            let message = match recover_rolling_runtime_after_missing_cut_path(
                                state,
                                rt,
                                active_before_cut,
                            ) {
                                Ok(Some(restarted)) => {
                                    restarted_rolling = Some(restarted);
                                    format!(
                                        "rolling recording buffer at {} disappeared; rolling capture was restarted; retry recording cut",
                                        missing_path.display()
                                    )
                                }
                                Ok(None) => format!(
                                    "rolling recording buffer at {} disappeared; rolling capture state was reset but could not be restarted automatically",
                                    missing_path.display()
                                ),
                                Err(recovery_error) => format!(
                                    "rolling recording buffer at {} disappeared; automatic recovery failed: {recovery_error:#}",
                                    missing_path.display()
                                ),
                            };
                            Err(anyhow::anyhow!(message))
                        },
                    )
                }
            };
            drop(guard);
            if let Some(restarted) = restarted_rolling {
                let _ = emit_event(
                    state,
                    Event::RecordingStarted {
                        recording_id: restarted.id,
                        path: restarted.path,
                    },
                );
            }
            match result {
                Ok(recording) => Response::Ok(ResponsePayload::RecordingCut { recording }),
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: format!("failed cutting rolling recording: {error}"),
                }),
            }
        }
        Request::RecordingRollingStart { options } => {
            if matches!(options.window_secs, Some(0)) {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "rolling window must be greater than 0 seconds".to_string(),
                }));
            }
            let resolved_settings =
                apply_rolling_start_options(&state.rolling_recording_defaults, &options);
            if !resolved_settings.is_available() {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "rolling recording requires a non-zero window and at least one enabled event kind".to_string(),
                }));
            }
            let options_empty = rolling_start_options_is_empty(&options);
            let mut started_now = false;
            // Ensure the rolling runtime Option is initialized before taking the inner &mut.
            {
                let mut guard = state
                    .rolling_recording_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
                if guard.is_none() {
                    *guard = Some(RecordingRuntime::new_rolling(
                        state.rolling_recordings_dir.clone(),
                        state.rolling_recording_segment_mb,
                        resolved_settings.window_secs,
                    ));
                }
                drop(guard);
            }
            let recording = {
                let mut guard = state
                    .rolling_recording_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
                let runtime = guard.as_mut().ok_or_else(|| {
                    anyhow::anyhow!("rolling recording runtime missing after init")
                })?;
                if let Some(active) = runtime.status().active {
                    if !options_empty {
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::InvalidRequest,
                            message:
                                "rolling recording is already active; stop it before applying overrides"
                                    .to_string(),
                        }));
                    }
                    active
                } else {
                    if runtime.rolling_window_secs() != Some(resolved_settings.window_secs) {
                        *runtime = RecordingRuntime::new_rolling(
                            state.rolling_recordings_dir.clone(),
                            state.rolling_recording_segment_mb,
                            resolved_settings.window_secs,
                        );
                    }
                    started_now = true;
                    let result = start_rolling_recording_runtime(
                        state,
                        runtime,
                        &resolved_settings,
                        options.name,
                    )?;
                    drop(guard);
                    result
                }
            };

            if started_now {
                let _ = emit_event(
                    state,
                    Event::RecordingStarted {
                        recording_id: recording.id,
                        path: recording.path.clone(),
                    },
                );
            }

            Response::Ok(ResponsePayload::RecordingStarted { recording })
        }
        Request::RecordingRollingStop => {
            let recording_id = {
                let mut guard = state
                    .rolling_recording_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
                let Some(rt) = guard.as_mut() else {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: "rolling recording is not configured".to_string(),
                    }));
                };
                if rt.status().active.is_none() {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: "rolling recording is not active".to_string(),
                    }));
                }
                let id = rt.stop(None)?.id;
                drop(guard);
                id
            };

            let _ = emit_event(state, Event::RecordingStopped { recording_id });
            Response::Ok(ResponsePayload::RecordingStopped { recording_id })
        }
        Request::RecordingRollingStatus => match rolling_status_snapshot(state) {
            Ok(status) => Response::Ok(ResponsePayload::RecordingRollingStatus { status }),
            Err(error) => Response::Err(ErrorResponse {
                code: ErrorCode::Internal,
                message: format!("failed reading rolling recording status: {error}"),
            }),
        },
        Request::PerformanceStatus => {
            let settings = state
                .performance_settings
                .lock()
                .map_err(|_| anyhow::anyhow!("performance settings lock poisoned"))?
                .to_runtime_settings();
            Response::Ok(ResponsePayload::PerformanceStatus { settings })
        }
        Request::PerformanceSet { settings } => {
            let normalized_capture_settings =
                PerformanceCaptureSettings::from_runtime_settings(&settings);
            let normalized_settings = normalized_capture_settings.to_runtime_settings();
            {
                let mut guard = state
                    .performance_settings
                    .lock()
                    .map_err(|_| anyhow::anyhow!("performance settings lock poisoned"))?;
                *guard = normalized_capture_settings;
            }
            if let Err(error) = emit_event(
                state,
                Event::PerformanceSettingsUpdated {
                    settings: normalized_settings.clone(),
                },
            ) {
                warn!("failed emitting performance settings update event: {error}");
            }
            Response::Ok(ResponsePayload::PerformanceUpdated {
                settings: normalized_settings,
            })
        }
        Request::RecordingRollingClear { restart_if_active } => {
            let root = state.rolling_recordings_dir.clone();
            let usage_before = match collect_rolling_usage(&root) {
                Ok(usage) => usage,
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: format!("failed reading rolling recording usage: {error}"),
                    }));
                }
            };

            let mut was_active = false;
            let mut stopped_recording_id = None;
            let mut restart_settings = None;
            let mut restart_name = None;

            {
                let mut runtime = state
                    .rolling_recording_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
                if let Some(runtime) = runtime.as_mut()
                    && let Some(active) = runtime.status().active
                {
                    was_active = true;
                    restart_name = active.name;
                    restart_settings = Some(RollingRecordingSettings {
                        window_secs: runtime
                            .rolling_window_secs()
                            .unwrap_or(state.rolling_recording_defaults.window_secs),
                        event_kinds: active.event_kinds,
                    });
                    stopped_recording_id = Some(runtime.stop(None)?.id);
                }
            }

            if let Some(recording_id) = stopped_recording_id {
                let _ = emit_event(state, Event::RecordingStopped { recording_id });
            }

            if let Err(error) = clear_rolling_root(&root) {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed clearing rolling recordings: {error}"),
                }));
            }

            #[allow(clippy::useless_let_if_seq)]
            let mut restarted = false;
            #[allow(clippy::useless_let_if_seq)]
            let mut restarted_recording = None;

            if restart_if_active && was_active {
                let settings =
                    restart_settings.unwrap_or_else(|| state.rolling_recording_defaults.clone());
                if !settings.is_available() {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: "cannot restart rolling recording after clear: no valid settings"
                            .to_string(),
                    }));
                }

                let recording = {
                    // Ensure the rolling runtime Option is initialized.
                    {
                        let mut guard = state.rolling_recording_runtime.lock().map_err(|_| {
                            anyhow::anyhow!("rolling recording runtime lock poisoned")
                        })?;
                        if guard.is_none() {
                            *guard = Some(RecordingRuntime::new_rolling(
                                root.clone(),
                                state.rolling_recording_segment_mb,
                                settings.window_secs,
                            ));
                        }
                    }
                    let mut guard = state
                        .rolling_recording_runtime
                        .lock()
                        .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
                    let runtime = guard.as_mut().ok_or_else(|| {
                        anyhow::anyhow!("rolling recording runtime missing after init")
                    })?;
                    if runtime.rolling_window_secs() != Some(settings.window_secs) {
                        *runtime = RecordingRuntime::new_rolling(
                            root.clone(),
                            state.rolling_recording_segment_mb,
                            settings.window_secs,
                        );
                    }
                    let result =
                        start_rolling_recording_runtime(state, runtime, &settings, restart_name)?;
                    drop(guard);
                    result
                };

                restarted = true;
                let _ = emit_event(
                    state,
                    Event::RecordingStarted {
                        recording_id: recording.id,
                        path: recording.path.clone(),
                    },
                );
                restarted_recording = Some(recording);
            }

            let usage_after = match collect_rolling_usage(&root) {
                Ok(usage) => usage,
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: format!(
                            "failed reading rolling recording usage after clear: {error}"
                        ),
                    }));
                }
            };

            Response::Ok(ResponsePayload::RecordingRollingCleared {
                report: RecordingRollingClearReport {
                    root_path: root.to_string_lossy().to_string(),
                    was_active,
                    restarted,
                    stopped_recording_id,
                    restarted_recording,
                    usage_before,
                    usage_after,
                },
            })
        }
        Request::RecordingCaptureTargets => {
            let mut targets = Vec::new();
            {
                let runtime = state
                    .manual_recording_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("recording runtime lock poisoned"))?;
                if let Some((recording_id, path)) = runtime.active_capture_target() {
                    targets.push(bmux_ipc::RecordingCaptureTarget {
                        recording_id,
                        path: path.to_string_lossy().to_string(),
                        rolling_window_secs: None,
                    });
                }
            }
            {
                let runtime = state
                    .rolling_recording_runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("rolling recording runtime lock poisoned"))?;
                if let Some(runtime) = runtime.as_ref()
                    && let Some((recording_id, path)) = runtime.active_capture_target()
                {
                    targets.push(bmux_ipc::RecordingCaptureTarget {
                        recording_id,
                        path: path.to_string_lossy().to_string(),
                        rolling_window_secs: runtime.rolling_window_secs(),
                    });
                }
            }
            Response::Ok(ResponsePayload::RecordingCaptureTargets { targets })
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
                    record_to_all_runtimes(
                        &state.manual_recording_runtime,
                        &state.rolling_recording_runtime,
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
                    message: format!("pane is closed: {pane_id}"),
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
    if let Response::Ok(payload) = &response {
        match payload {
            ResponsePayload::SessionCreated { .. }
            | ResponsePayload::ContextCreated { .. }
            | ResponsePayload::ContextClosed { .. }
            | ResponsePayload::SessionKilled { .. }
            | ResponsePayload::PaneClosed {
                session_closed: true,
                ..
            } => {
                emit_control_catalog_changed(
                    state,
                    &[
                        ControlCatalogScope::Sessions,
                        ControlCatalogScope::Contexts,
                        ControlCatalogScope::Bindings,
                    ],
                    false,
                )?;
            }
            ResponsePayload::ServerSnapshotRestored { .. } => {
                emit_control_catalog_changed(
                    state,
                    &[
                        ControlCatalogScope::Sessions,
                        ControlCatalogScope::Contexts,
                        ControlCatalogScope::Bindings,
                    ],
                    true,
                )?;
            }
            _ => {}
        }
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
            | Request::RestartPane { .. }
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
            | Request::RecordingCut { .. }
            | Request::RecordingRollingStart { .. }
            | Request::RecordingRollingStop
            | Request::PerformanceSet { .. }
            | Request::RecordingRollingClear { .. }
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
                | ResponsePayload::PaneRestarted { .. }
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
        Request::HelloV2 { .. } => "hello_v2",
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
        Request::RestartPane { .. } => "restart_pane",
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
        Request::AttachPaneSnapshot { .. } => "attach_pane_snapshot",
        Request::AttachPaneOutputBatch { .. } => "attach_pane_output_batch",
        Request::AttachPaneImages { .. } => "attach_pane_images",
        Request::RecordingStart { .. } => "recording_start",
        Request::RecordingStop { .. } => "recording_stop",
        Request::RecordingStatus => "recording_status",
        Request::RecordingList => "recording_list",
        Request::RecordingDelete { .. } => "recording_delete",
        Request::RecordingWriteCustomEvent { .. } => "recording_write_custom_event",
        Request::RecordingDeleteAll => "recording_delete_all",
        Request::RecordingCut { .. } => "recording_cut",
        Request::RecordingRollingStart { .. } => "recording_rolling_start",
        Request::RecordingRollingStop => "recording_rolling_stop",
        Request::RecordingRollingStatus => "recording_rolling_status",
        Request::PerformanceStatus => "performance_status",
        Request::PerformanceSet { .. } => "performance_set",
        Request::RecordingRollingClear { .. } => "recording_rolling_clear",
        Request::RecordingCaptureTargets => "recording_capture_targets",
        Request::RecordingPrune { .. } => "recording_prune",
        Request::Detach => "detach",
        Request::SubscribeEvents => "subscribe_events",
        Request::PollEvents { .. } => "poll_events",
        Request::EnableEventPush => "enable_event_push",
        Request::PaneDirectInput { .. } => "pane_direct_input",
        Request::ControlCatalogSnapshot { .. } => "control_catalog_snapshot",
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
            RecordingEventKind::PaneImage,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ],
        RecordingProfile::Functional => vec![
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::PaneImage,
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

const fn rolling_start_options_is_empty(options: &RecordingRollingStartOptions) -> bool {
    options.window_secs.is_none()
        && options.name.is_none()
        && options.event_kinds.is_none()
        && options.capture_input.is_none()
        && options.capture_output.is_none()
        && options.capture_events.is_none()
        && options.capture_protocol_replies.is_none()
        && options.capture_images.is_none()
}

const fn response_payload_kind_name(payload: &ResponsePayload) -> &'static str {
    match payload {
        ResponsePayload::Pong => "pong",
        ResponsePayload::ClientIdentity { .. } => "client_identity",
        ResponsePayload::PrincipalIdentity { .. } => "principal_identity",
        ResponsePayload::HelloNegotiated { .. } => "hello_negotiated",
        ResponsePayload::HelloIncompatible { .. } => "hello_incompatible",
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
        ResponsePayload::PaneRestarted { .. } => "pane_restarted",
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
        ResponsePayload::AttachPaneSnapshot { .. } => "attach_pane_snapshot",
        ResponsePayload::AttachPaneOutputBatch { .. } => "attach_pane_output_batch",
        ResponsePayload::AttachPaneImages { .. } => "attach_pane_images",
        ResponsePayload::RecordingStarted { .. } => "recording_started",
        ResponsePayload::RecordingStopped { .. } => "recording_stopped",
        ResponsePayload::RecordingStatus { .. } => "recording_status",
        ResponsePayload::RecordingList { .. } => "recording_list",
        ResponsePayload::RecordingDeleted { .. } => "recording_deleted",
        ResponsePayload::RecordingCustomEventWritten { .. } => "recording_custom_event_written",
        ResponsePayload::RecordingDeleteAll { .. } => "recording_delete_all",
        ResponsePayload::RecordingCut { .. } => "recording_cut",
        ResponsePayload::RecordingCaptureTargets { .. } => "recording_capture_targets",
        ResponsePayload::RecordingRollingStatus { .. } => "recording_rolling_status",
        ResponsePayload::PerformanceStatus { .. } => "performance_status",
        ResponsePayload::PerformanceUpdated { .. } => "performance_updated",
        ResponsePayload::RecordingRollingCleared { .. } => "recording_rolling_cleared",
        ResponsePayload::RecordingPruned { .. } => "recording_pruned",
        ResponsePayload::Detached => "detached",
        ResponsePayload::PaneDirectInputAccepted { .. } => "pane_direct_input_accepted",
        ResponsePayload::EventsSubscribed => "events_subscribed",
        ResponsePayload::EventBatch { .. } => "event_batch",
        ResponsePayload::EventPushEnabled => "event_push_enabled",
        ResponsePayload::ControlCatalogSnapshot { .. } => "control_catalog_snapshot",
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

/// Kill sessions offline (without a running server) via the snapshot file.
///
/// # Errors
/// Returns an error if the snapshot cannot be read or written.
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

/// On Windows there are no POSIX process groups, but `taskkill /T` kills an
/// entire process tree rooted at a PID.  We use the PID that
/// `resolve_process_group_id_for_pid` stored (it returns the PID itself on
/// Windows) as the tree-kill target.
#[cfg(windows)]
fn terminate_process_group(process_group_id: i32) -> bool {
    if process_group_id <= 0 {
        return false;
    }
    let pid = process_group_id.to_string();

    // Graceful tree kill first.
    let sent_term = std::process::Command::new("taskkill")
        .args(["/PID", &pid, "/T"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    std::thread::sleep(Duration::from_millis(120));

    // Check if the process is still alive.
    let still_alive = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&pid))
        .unwrap_or(false);

    if still_alive {
        // Force-kill the process tree.
        return std::process::Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            || sent_term;
    }

    sent_term
}

#[cfg(not(any(unix, windows)))]
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

/// On Windows there are no POSIX process groups. Return the PID itself so
/// that `terminate_process_group` can use it as the `taskkill /T` target for
/// process-tree termination.
#[cfg(windows)]
fn resolve_process_group_id_for_pid(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|&id| id > 0)
}

#[cfg(not(any(unix, windows)))]
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
    selector: Option<&SessionSelector>,
    selected_session: Option<&SessionId>,
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
    frame_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
) -> Result<()> {
    let response = Response::Err(ErrorResponse { code, message });
    send_response_via_channel(frame_tx, request_id, &response, frame_codec)
}

fn send_response_via_channel(
    frame_tx: &mpsc::UnboundedSender<Vec<u8>>,
    request_id: u64,
    response: &Response,
    frame_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
) -> Result<()> {
    let payload = encode(response).context("failed encoding response payload")?;
    let envelope = Envelope::new(request_id, EnvelopeKind::Response, payload);
    let frame = if frame_codec.is_some() {
        bmux_ipc::frame::encode_frame_compressed(&envelope, frame_codec)
            .context("failed encoding compressed response frame")?
    } else {
        bmux_ipc::frame::encode_frame(&envelope).context("failed encoding response frame")?
    };
    frame_tx
        .send(frame)
        .map_err(|_| anyhow::anyhow!("writer channel closed"))?;
    Ok(())
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

/// Resolve a payload compression codec from the user's compression config.
#[cfg(feature = "image-registry")]
fn resolve_payload_codec_from_config(
    config: &bmux_config::CompressionConfig,
) -> Option<std::sync::Arc<dyn bmux_ipc::compression::CompressionCodec>> {
    #[cfg(feature = "compression")]
    use bmux_ipc::compression;
    match config.images {
        bmux_config::CompressionMode::None => None,
        bmux_config::CompressionMode::Auto => {
            // Prefer zstd for image payloads, fall back to lz4.
            #[cfg(feature = "compression")]
            {
                compression::default_payload_codec().map(|codec| {
                    // Apply user-configured level if zstd.
                    let boxed: Box<dyn compression::CompressionCodec> =
                        if codec.id() == compression::CompressionId::Zstd {
                            Box::new(compression::ZstdCodec::with_level(config.level))
                        } else {
                            codec
                        };
                    std::sync::Arc::from(boxed)
                })
            }
            #[cfg(not(feature = "compression"))]
            None
        }
        bmux_config::CompressionMode::Zstd => {
            #[cfg(feature = "compression")]
            {
                Some(std::sync::Arc::new(compression::ZstdCodec::with_level(
                    config.level,
                )))
            }
            #[cfg(not(feature = "compression"))]
            None
        }
        bmux_config::CompressionMode::Lz4 => {
            #[cfg(feature = "compression")]
            {
                compression::resolve_codec(compression::CompressionId::Lz4)
                    .map(std::sync::Arc::from)
            }
            #[cfg(not(feature = "compression"))]
            None
        }
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

fn record_to_all_runtimes(
    manual_runtime: &Arc<Mutex<RecordingRuntime>>,
    rolling_runtime: &Arc<Mutex<Option<RecordingRuntime>>>,
    kind: RecordingEventKind,
    payload: RecordingPayload,
    meta: RecordMeta,
) {
    if let Ok(runtime) = manual_runtime.lock() {
        let _ = runtime.record(kind, payload.clone(), meta);
    }
    if let Ok(runtime) = rolling_runtime.lock()
        && let Some(runtime) = runtime.as_ref()
    {
        let _ = runtime.record(kind, payload, meta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
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

    fn test_config_paths(test_name: &str) -> (ConfigPaths, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "bmux-server-{test_name}-{}",
            Uuid::new_v4().as_simple()
        ));
        let config_dir = root.join("config");
        let runtime_dir = root.join("runtime");
        let data_dir = root.join("data");
        let state_dir = root.join("state");
        std::fs::create_dir_all(&config_dir).expect("test config dir should be created");
        std::fs::create_dir_all(&runtime_dir).expect("test runtime dir should be created");
        std::fs::create_dir_all(&data_dir).expect("test data dir should be created");
        std::fs::create_dir_all(&state_dir).expect("test state dir should be created");
        (
            ConfigPaths::new(config_dir, runtime_dir, data_dir, state_dir),
            root,
        )
    }

    #[test]
    fn rolling_recordings_root_is_isolated_across_runtime_dirs() {
        let root = std::env::temp_dir().join(format!(
            "bmux-server-rolling-root-isolation-{}",
            Uuid::new_v4().as_simple()
        ));
        let shared_config_dir = root.join("config");
        let shared_data_dir = root.join("data");
        let shared_state_dir = root.join("state");
        std::fs::create_dir_all(&shared_config_dir).expect("shared config dir should be created");
        std::fs::create_dir_all(&shared_data_dir).expect("shared data dir should be created");
        std::fs::create_dir_all(&shared_state_dir).expect("shared state dir should be created");

        let paths_left = ConfigPaths::new(
            shared_config_dir.clone(),
            root.join("runtime-left"),
            shared_data_dir.clone(),
            shared_state_dir.clone(),
        );
        let paths_right = ConfigPaths::new(
            shared_config_dir,
            root.join("runtime-right"),
            shared_data_dir,
            shared_state_dir,
        );
        let rolling_event_kinds = vec![RecordingEventKind::PaneOutputRaw];

        let server_left = BmuxServer::from_config_paths_with_rolling_options(
            &paths_left,
            true,
            120,
            &rolling_event_kinds,
        );
        let server_right = BmuxServer::from_config_paths_with_rolling_options(
            &paths_right,
            true,
            120,
            &rolling_event_kinds,
        );

        assert_ne!(
            server_left.state.rolling_recordings_dir, server_right.state.rolling_recordings_dir,
            "rolling recording root should be runtime-scoped"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn recording_cut_recovers_when_active_rolling_directory_disappears() {
        let (paths, root) = test_config_paths("recording-cut-recovery");
        let rolling_event_kinds = vec![RecordingEventKind::PaneOutputRaw];
        let server = BmuxServer::from_config_paths_with_rolling_options(
            &paths,
            true,
            120,
            &rolling_event_kinds,
        );

        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        let started = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::RecordingRollingStart {
                options: RecordingRollingStartOptions::default(),
            },
        )
        .await;
        match started {
            Response::Ok(ResponsePayload::RecordingStarted { .. }) => {}
            response => panic!("expected rolling start response, got {response:?}"),
        }

        let (stale_id, stale_path) = server
            .state
            .rolling_recording_runtime
            .lock()
            .expect("rolling runtime lock should succeed")
            .as_ref()
            .expect("rolling runtime should be initialized")
            .active_capture_target()
            .expect("rolling runtime should have active capture");
        let orphaned_path = stale_path.with_extension("orphaned");
        std::fs::rename(&stale_path, &orphaned_path)
            .expect("active rolling recording directory should be movable");

        let first_cut = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::RecordingCut {
                last_seconds: Some(30),
                name: None,
            },
        )
        .await;
        match first_cut {
            Response::Err(error) => {
                assert_eq!(error.code, ErrorCode::InvalidRequest);
                assert!(
                    error
                        .message
                        .contains("rolling capture was restarted; retry recording cut"),
                    "expected recovery hint in cut failure, got: {}",
                    error.message
                );
            }
            response @ Response::Ok(_) => {
                panic!("expected first cut to fail with recovery hint, got {response:?}")
            }
        }

        let restarted_id = server
            .state
            .rolling_recording_runtime
            .lock()
            .expect("rolling runtime lock should succeed")
            .as_ref()
            .expect("rolling runtime should still be initialized")
            .status()
            .active
            .expect("rolling runtime should be active after recovery")
            .id;
        assert_ne!(
            restarted_id, stale_id,
            "recovery should start a fresh rolling recording"
        );

        let accepted = server
            .state
            .rolling_recording_runtime
            .lock()
            .expect("rolling runtime lock should succeed")
            .as_ref()
            .expect("rolling runtime should remain initialized")
            .record(
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes {
                    data: b"recovered-cut-event".to_vec(),
                },
                RecordMeta {
                    session_id: None,
                    pane_id: None,
                    client_id: None,
                },
            )
            .expect("record should complete without error");
        assert!(accepted, "rolling runtime should accept test event");

        std::thread::sleep(Duration::from_millis(1200));

        let second_cut = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::RecordingCut {
                last_seconds: Some(30),
                name: None,
            },
        )
        .await;
        match second_cut {
            Response::Ok(ResponsePayload::RecordingCut { recording }) => {
                assert!(recording.event_count >= 1);
            }
            response => panic!("expected second cut to succeed after recovery, got {response:?}"),
        }

        let stopped = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::RecordingRollingStop,
        )
        .await;
        match stopped {
            Response::Ok(ResponsePayload::RecordingStopped { .. }) => {}
            response => panic!("expected rolling stop response, got {response:?}"),
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pane_mouse_protocol_tracker_tracks_dec_private_modes() {
        let mut tracker = PaneTerminalModeTracker::default();

        assert_eq!(
            tracker.current_protocol().mode,
            AttachMouseProtocolMode::None
        );
        assert_eq!(
            tracker.current_protocol().encoding,
            AttachMouseProtocolEncoding::Default
        );

        tracker.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            tracker.current_protocol(),
            AttachMouseProtocolState {
                mode: AttachMouseProtocolMode::PressRelease,
                encoding: AttachMouseProtocolEncoding::Sgr,
            }
        );

        tracker.process(b"\x1b[?1003h");
        assert_eq!(
            tracker.current_protocol().mode,
            AttachMouseProtocolMode::AnyMotion
        );

        tracker.process(b"\x1b[?1003l");
        assert_eq!(
            tracker.current_protocol().mode,
            AttachMouseProtocolMode::PressRelease
        );

        tracker.process(b"\x1b[?1000l\x1b[?1006l");
        assert_eq!(
            tracker.current_protocol(),
            AttachMouseProtocolState {
                mode: AttachMouseProtocolMode::None,
                encoding: AttachMouseProtocolEncoding::Default,
            }
        );
    }

    #[test]
    fn pane_mouse_protocol_tracker_handles_sequences_split_across_chunks() {
        let mut tracker = PaneTerminalModeTracker::default();

        tracker.process(b"\x1b[?10");
        tracker.process(b"03h\x1b[");
        tracker.process(b"?1006h");

        assert_eq!(
            tracker.current_protocol(),
            AttachMouseProtocolState {
                mode: AttachMouseProtocolMode::AnyMotion,
                encoding: AttachMouseProtocolEncoding::Sgr,
            }
        );
    }

    #[test]
    fn pane_mouse_protocol_tracker_resets_on_terminal_resets() {
        let mut tracker = PaneTerminalModeTracker::default();

        tracker.process(b"\x1b[?1002h\x1b[?1005h");
        assert_eq!(
            tracker.current_protocol().mode,
            AttachMouseProtocolMode::ButtonMotion
        );
        assert_eq!(
            tracker.current_protocol().encoding,
            AttachMouseProtocolEncoding::Utf8
        );

        tracker.process(b"\x1bc");
        assert_eq!(
            tracker.current_protocol(),
            AttachMouseProtocolState::default()
        );

        tracker.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            tracker.current_protocol().mode,
            AttachMouseProtocolMode::PressRelease
        );
        tracker.process(b"\x1b[!p");
        assert_eq!(
            tracker.current_protocol(),
            AttachMouseProtocolState::default()
        );
    }

    #[test]
    fn pane_terminal_mode_tracker_tracks_input_modes() {
        let mut tracker = PaneTerminalModeTracker::default();

        assert_eq!(
            tracker.current_input_modes(),
            AttachInputModeState::default()
        );

        tracker.process(b"\x1b[?1h\x1b=");
        assert_eq!(
            tracker.current_input_modes(),
            AttachInputModeState {
                application_cursor: true,
                application_keypad: true,
            }
        );

        tracker.process(b"\x1b[?1l\x1b>");
        assert_eq!(
            tracker.current_input_modes(),
            AttachInputModeState::default()
        );

        tracker.process(b"\x1b[?1h\x1b=");
        tracker.process(b"\x1bc");
        assert_eq!(
            tracker.current_input_modes(),
            AttachInputModeState::default()
        );
    }

    #[test]
    fn protocol_reply_tracks_cursor_position_for_cpr_queries() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(24, 80);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[12;34H");

        let cpr_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n");
        assert_eq!(cpr_reply, b"\x1b[12;34R");

        let dec_cpr_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?6n");
        assert_eq!(dec_cpr_reply, b"\x1b[?12;34R");
    }

    #[test]
    fn protocol_reply_handles_split_cpr_sequences_across_chunks() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(24, 80);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[20;7H");

        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[").is_empty());
        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"6").is_empty());
        let cpr_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"n");
        assert_eq!(cpr_reply, b"\x1b[20;7R");
    }

    #[test]
    fn protocol_reply_reports_saved_cursor_after_alt_screen_exit() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(24, 80);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[12;34H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[12;34R"
        );

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?1049h");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[4;7H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[4;7R"
        );
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?6n"),
            b"\x1b[?4;7R"
        );

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?1049l");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[12;34R"
        );
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?6n"),
            b"\x1b[?12;34R"
        );
    }

    #[test]
    fn protocol_reply_handles_split_dec_cpr_after_alt_screen_exit() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(24, 80);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[20;7H");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?1049h");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[2;2H");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?1049l");

        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[").is_empty());
        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"?6").is_empty());
        let dec_cpr_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"n");
        assert_eq!(dec_cpr_reply, b"\x1b[?20;7R");
    }

    #[test]
    fn protocol_reply_restores_cursor_after_csi_save_restore() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(24, 80);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[20;35H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[20;35R"
        );

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[s");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[1;1R"
        );

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[u");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[20;35R"
        );
    }

    #[test]
    fn protocol_reply_preserves_pre_alt_cursor_after_csi_save_restore_then_1049_cycle() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(60, 120);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[28;35H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[28;35R"
        );

        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker,
            b"\x1b[s\x1b[?1016$p\x1b[H\x1b[6n\x1b[u",
        );
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[28;35R"
        );

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?1049h");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[2;2H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[2;2R"
        );

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?1049l");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[28;35R"
        );
    }

    #[test]
    fn protocol_reply_handles_split_csi_save_restore_across_chunks() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(30, 120);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[15;42H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[15;42R"
        );

        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b").is_empty());
        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"[").is_empty());
        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"s").is_empty());

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[H");
        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[1;1R"
        );

        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b").is_empty());
        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"[").is_empty());
        assert!(protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"u").is_empty());

        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[15;42R"
        );
    }

    #[test]
    fn protocol_reply_does_not_confuse_kitty_query_u_with_restore_cursor() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(30, 120);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[9;17H");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[s");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[?u");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[H");
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[u");

        assert_eq!(
            protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n"),
            b"\x1b[9;17R"
        );
    }

    #[test]
    fn cursor_tracker_resize_updates_cursor_bounds() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(5, 5);

        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[24;80H");
        let clamped_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n");
        assert_eq!(clamped_reply, b"\x1b[5;5R");

        cursor_tracker.resize(40, 120);
        let _ = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[24;80H");
        let resized_reply = protocol_reply_for_chunk(&mut engine, &mut cursor_tracker, b"\x1b[6n");
        assert_eq!(resized_reply, b"\x1b[24;80R");
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
    #[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
    async fn exited_pane_keeps_layout_and_restart_reuses_same_pane_id() {
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
            Response::Ok(ResponsePayload::PaneSplit { .. }) => {}
            response => panic!("expected split response, got {response:?}"),
        }

        let list_before = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::ListPanes {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await;
        let panes_before = match list_before {
            Response::Ok(ResponsePayload::PaneList { panes }) => panes,
            response => panic!("expected pane list response, got {response:?}"),
        };
        assert_eq!(panes_before.len(), 2);
        let target_pane_id = panes_before[0].id;

        {
            let mut runtime_manager = server
                .state
                .session_runtimes
                .lock()
                .expect("session runtime manager lock should succeed");
            let runtime = runtime_manager
                .runtimes
                .get_mut(&SessionId(session_id))
                .expect("session runtime should exist");
            let pane = runtime
                .panes
                .get_mut(&target_pane_id)
                .expect("target pane should exist");
            pane.exited.store(true, Ordering::SeqCst);
            if let Ok(mut reason) = pane.exit_reason.lock() {
                *reason = Some("process exited with status 130".to_string());
            }
        }

        reap_exited_pane(&server.state, SessionId(session_id), target_pane_id)
            .expect("reap should succeed");

        let list_exited = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::ListPanes {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await;
        let panes_exited = match list_exited {
            Response::Ok(ResponsePayload::PaneList { panes }) => panes,
            response => panic!("expected pane list response, got {response:?}"),
        };
        assert_eq!(panes_exited.len(), 2);
        let exited_summary = panes_exited
            .iter()
            .find(|pane| pane.id == target_pane_id)
            .expect("target pane summary should exist");
        assert_eq!(exited_summary.state, PaneState::Exited);

        let restarted = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::RestartPane {
                session: Some(SessionSelector::ById(session_id)),
                target: Some(PaneSelector::ById(target_pane_id)),
            },
        )
        .await;
        match restarted {
            Response::Ok(ResponsePayload::PaneRestarted {
                id,
                session_id: sid,
            }) => {
                assert_eq!(id, target_pane_id);
                assert_eq!(sid, session_id);
            }
            response => panic!("expected pane restarted response, got {response:?}"),
        }

        let list_after = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::ListPanes {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await;
        let panes_after = match list_after {
            Response::Ok(ResponsePayload::PaneList { panes }) => panes,
            response => panic!("expected pane list response, got {response:?}"),
        };
        assert_eq!(panes_after.len(), 2);
        let restarted_summary = panes_after
            .iter()
            .find(|pane| pane.id == target_pane_id)
            .expect("target pane summary should exist");
        assert_eq!(restarted_summary.state, PaneState::Running);
        assert!(restarted_summary.state_reason.is_none());
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
    async fn control_catalog_snapshot_includes_context_session_bindings() {
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
        let created_context_id = match created {
            Response::Ok(ResponsePayload::ContextCreated { context }) => context.id,
            response => panic!("expected context created response, got {response:?}"),
        };

        let snapshot_response = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::ControlCatalogSnapshot {
                since_revision: None,
            },
        )
        .await;

        let snapshot = match snapshot_response {
            Response::Ok(ResponsePayload::ControlCatalogSnapshot { snapshot }) => snapshot,
            response => panic!("expected control catalog snapshot response, got {response:?}"),
        };

        assert!(
            snapshot.revision >= 2,
            "catalog revision should advance after context creation"
        );
        assert!(
            snapshot
                .contexts
                .iter()
                .any(|context| context.id == created_context_id),
            "created context should be present in catalog snapshot"
        );
        assert!(
            snapshot
                .context_session_bindings
                .iter()
                .any(|binding| binding.context_id == created_context_id),
            "created context should have a session binding in catalog snapshot"
        );
    }

    #[tokio::test]
    async fn poll_events_filters_control_catalog_events_without_capability() {
        let server = BmuxServer::new(test_endpoint());
        let client_id = ClientId::new();
        let principal_id = Uuid::new_v4();
        let mut selected_session = None;
        let mut attached_stream_session = None;

        {
            let mut capabilities = server
                .state
                .client_capabilities
                .lock()
                .expect("client capability map lock should succeed");
            capabilities.insert(client_id, BTreeSet::new());
        }

        let subscribed = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::SubscribeEvents,
        )
        .await;
        assert_eq!(subscribed, Response::Ok(ResponsePayload::EventsSubscribed));

        let _ = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CreateContext {
                name: Some("hidden-control-event".to_string()),
                attributes: BTreeMap::new(),
            },
        )
        .await;

        let polled = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::PollEvents { max_events: 16 },
        )
        .await;
        let events = match polled {
            Response::Ok(ResponsePayload::EventBatch { events }) => events,
            response => panic!("expected event batch response, got {response:?}"),
        };
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::ControlCatalogChanged { .. })),
            "poll_events should filter control catalog events without capability"
        );

        {
            let mut capabilities = server
                .state
                .client_capabilities
                .lock()
                .expect("client capability map lock should succeed");
            capabilities.insert(
                client_id,
                BTreeSet::from([CAPABILITY_CONTROL_CATALOG_SYNC.to_string()]),
            );
        }

        let _ = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::CreateContext {
                name: Some("visible-control-event".to_string()),
                attributes: BTreeMap::new(),
            },
        )
        .await;

        let polled_with_capability = execute_request(
            &server,
            client_id,
            principal_id,
            &mut selected_session,
            &mut attached_stream_session,
            Request::PollEvents { max_events: 16 },
        )
        .await;
        let events_with_capability = match polled_with_capability {
            Response::Ok(ResponsePayload::EventBatch { events }) => events,
            response => panic!("expected event batch response, got {response:?}"),
        };
        assert!(
            events_with_capability
                .iter()
                .any(|event| matches!(event, Event::ControlCatalogChanged { .. })),
            "poll_events should include control catalog events with capability"
        );
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
            response @ Response::Ok(_) => {
                panic!("expected attach context not found response, got {response:?}")
            }
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
            response @ Response::Ok(_) => {
                panic!("expected attach context not found response, got {response:?}")
            }
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
            response @ Response::Ok(_) => panic!("expected denied kill response, got {response:?}"),
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
            response @ Response::Ok(_) => {
                panic!("expected denied split response, got {response:?}")
            }
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
            response @ Response::Ok(_) => {
                panic!("expected denied attach input response, got {response:?}")
            }
        }
    }

    // ---- EscSeqPhase state machine tests ----

    #[test]
    fn esc_seq_phase_ground_stays_ground_for_printable() {
        let mut phase = EscSeqPhase::Ground;
        for &b in b"Hello, world! 123" {
            phase = phase.advance(b);
            assert_eq!(phase, EscSeqPhase::Ground);
        }
    }

    #[test]
    fn esc_seq_phase_csi_sgr_round_trip() {
        // \x1b[38;2;10;10;10m  — a 24-bit true-color SGR sequence.
        let seq = b"\x1b[38;2;10;10;10m";
        let mut phase = EscSeqPhase::Ground;

        phase = phase.advance(seq[0]); // ESC
        assert_eq!(phase, EscSeqPhase::Escape);

        phase = phase.advance(seq[1]); // [
        assert_eq!(phase, EscSeqPhase::Csi);

        // Parameters: 38;2;10;10;10  — all stay in CSI
        for &b in &seq[2..seq.len() - 1] {
            phase = phase.advance(b);
            assert_eq!(phase, EscSeqPhase::Csi);
        }

        phase = phase.advance(seq[seq.len() - 1]); // m (final byte)
        assert_eq!(phase, EscSeqPhase::Ground);
    }

    #[test]
    fn esc_seq_phase_osc_bel_terminator() {
        // \x1b]0;title\x07
        let seq = b"\x1b]0;title\x07";
        let mut phase = EscSeqPhase::Ground;

        phase = phase.advance(0x1b); // ESC
        phase = phase.advance(b']'); // -> Osc
        assert_eq!(phase, EscSeqPhase::Osc);

        for &b in b"0;title" {
            phase = phase.advance(b);
            assert_eq!(phase, EscSeqPhase::Osc);
        }

        phase = phase.advance(0x07); // BEL
        assert_eq!(phase, EscSeqPhase::Ground);
        let _ = seq;
    }

    #[test]
    fn esc_seq_phase_osc_st_terminator() {
        // \x1b]0;title\x1b\\
        let mut phase = EscSeqPhase::Ground;
        for &b in b"\x1b]0;title" {
            phase = phase.advance(b);
        }
        assert_eq!(phase, EscSeqPhase::Osc);
        phase = phase.advance(0x1b);
        assert_eq!(phase, EscSeqPhase::OscEsc);
        phase = phase.advance(b'\\');
        assert_eq!(phase, EscSeqPhase::Ground);
    }

    #[test]
    fn esc_seq_phase_dcs_st_terminator() {
        // ESC P data ESC backslash
        let mut phase = EscSeqPhase::Ground;
        phase = phase.advance(0x1b);
        phase = phase.advance(b'P');
        assert_eq!(phase, EscSeqPhase::Dcs);
        for &b in b"some;data" {
            phase = phase.advance(b);
            assert_eq!(phase, EscSeqPhase::Dcs);
        }
        phase = phase.advance(0x1b);
        assert_eq!(phase, EscSeqPhase::DcsEsc);
        phase = phase.advance(b'\\');
        assert_eq!(phase, EscSeqPhase::Ground);
    }

    #[test]
    fn esc_seq_phase_can_aborts_from_any_state() {
        for initial in [
            EscSeqPhase::Escape,
            EscSeqPhase::Csi,
            EscSeqPhase::Osc,
            EscSeqPhase::Dcs,
            EscSeqPhase::Sos,
        ] {
            assert_eq!(
                initial.advance(0x18),
                EscSeqPhase::Ground,
                "CAN from {initial:?}"
            );
            assert_eq!(
                initial.advance(0x1A),
                EscSeqPhase::Ground,
                "SUB from {initial:?}"
            );
        }
    }

    #[test]
    fn esc_seq_phase_esc_inside_csi_restarts() {
        let mut phase = EscSeqPhase::Ground;
        // Start a CSI
        phase = phase.advance(0x1b);
        phase = phase.advance(b'[');
        assert_eq!(phase, EscSeqPhase::Csi);
        // ESC inside CSI aborts it
        phase = phase.advance(0x1b);
        assert_eq!(phase, EscSeqPhase::Escape);
        // New CSI
        phase = phase.advance(b'[');
        assert_eq!(phase, EscSeqPhase::Csi);
        phase = phase.advance(b'H'); // final byte
        assert_eq!(phase, EscSeqPhase::Ground);
    }

    #[test]
    fn esc_seq_phase_two_byte_escape() {
        // ESC 7 (DECSC — save cursor) is a two-byte sequence
        let mut phase = EscSeqPhase::Ground;
        phase = phase.advance(0x1b);
        assert_eq!(phase, EscSeqPhase::Escape);
        phase = phase.advance(b'7');
        assert_eq!(phase, EscSeqPhase::Ground);
    }

    #[test]
    fn esc_seq_phase_sos_pm_apc() {
        for start_byte in [b'X', b'^', b'_'] {
            let mut phase = EscSeqPhase::Ground;
            phase = phase.advance(0x1b);
            phase = phase.advance(start_byte);
            assert_eq!(phase, EscSeqPhase::Sos);
            for &b in b"body data" {
                phase = phase.advance(b);
                assert_eq!(phase, EscSeqPhase::Sos);
            }
            phase = phase.advance(0x1b);
            assert_eq!(phase, EscSeqPhase::SosEsc);
            phase = phase.advance(b'\\');
            assert_eq!(phase, EscSeqPhase::Ground);
        }
    }

    // ---- OutputFanoutBuffer boundary safety tests ----

    #[test]
    fn read_recent_pure_text_returns_full_amount() {
        let mut buf = OutputFanoutBuffer::new(4096);
        let text = b"Hello, world! This is plain text with no escape sequences.";
        buf.push_chunk(text);
        let result = buf.read_recent(text.len());
        assert_eq!(result, text.to_vec());
    }

    #[test]
    fn read_recent_never_starts_mid_csi() {
        // Simulate the exact corruption scenario: 1024-byte chunks that split
        // an SGR sequence like \x1b[48;2;10;10;10m at the boundary.
        let mut buf = OutputFanoutBuffer::new(65536);

        // Build a payload where the SGR sequence straddles a chunk boundary:
        // First chunk ends with \x1b[48;  second chunk starts with 2;10;10;10m
        let mut chunk1 = vec![b' '; 1020]; // padding
        chunk1.extend_from_slice(b"\x1b[48;");
        assert_eq!(chunk1.len(), 1025);

        let mut chunk2 = Vec::new();
        chunk2.extend_from_slice(b"2;10;10;10m");
        chunk2.extend_from_slice(&vec![b' '; 1013]); // padding
        assert_eq!(chunk2.len(), 1024);

        buf.push_chunk(&chunk1);
        buf.push_chunk(&chunk2);

        // Request read_recent with a budget that starts mid-CSI (at the second
        // chunk boundary, where "2;10;10;10m" lives).
        let result = buf.read_recent(chunk2.len());

        // The result must NOT start with "2;10;10;10m".  It should start at
        // the ground boundary AFTER the 'm' — i.e. the first space.
        assert!(
            !result.starts_with(b"2;10;10;10m"),
            "read_recent returned data starting mid-escape-sequence: {:?}",
            String::from_utf8_lossy(&result[..20.min(result.len())])
        );

        // The ground boundary is one past the 'm' byte (which completes the CSI).
        // So the result should start with spaces (the padding after the CSI).
        if !result.is_empty() {
            assert_eq!(result[0], b' ', "first byte should be ground-state text");
        }
    }

    #[test]
    fn read_recent_exact_boundary_hit() {
        // Budget covers the full buffer — intended_start = 0 which is Ground
        // (very start of stream, no preceding non-Ground state), so we get
        // everything.
        let mut buf = OutputFanoutBuffer::new(4096);
        buf.push_chunk(b"\x1b[38;2;255;0;0mRed text here");

        // The full buffer starts at offset 0 in Ground state (start of stream).
        // ground_boundaries is empty for the region before offset 0 — but
        // esc_phase is Ground (the CSI completed), so `first_ground_boundary_at_or_after(0)`
        // returns 0 via the "esc_phase is Ground" fallback.  We get everything.
        let result = buf.read_recent(4096);
        assert_eq!(result, b"\x1b[38;2;255;0;0mRed text here");
    }

    #[test]
    fn read_recent_entire_window_mid_sequence() {
        // Edge case: the entire read window is inside a single long DCS.
        let mut buf = OutputFanoutBuffer::new(4096);
        // DCS start
        buf.push_chunk(b"\x1bP");
        // Large body — no ground boundary
        buf.push_chunk(&vec![b'x'; 2000]);
        // No ST yet

        // read_recent should return empty since there's no ground boundary.
        let result = buf.read_recent(1000);
        assert!(
            result.is_empty(),
            "should return empty when entirely mid-sequence"
        );
    }

    #[test]
    fn read_recent_with_osc_boundary() {
        let mut buf = OutputFanoutBuffer::new(4096);
        // OSC terminated by BEL
        buf.push_chunk(b"\x1b]0;My Title\x07Some text after");
        // Full buffer starts at offset 0 which is Ground (start of stream).
        let result = buf.read_recent(4096);
        assert_eq!(result, b"\x1b]0;My Title\x07Some text after");
    }

    #[test]
    fn read_recent_budget_smaller_than_buffer_skips_mid_seq_start() {
        let mut buf = OutputFanoutBuffer::new(4096);
        // Push text, then an SGR, then more text.
        // "AAAA...aaaa" (30 bytes) + "\x1b[38;2;128;128;128m" (20 bytes) + "BBBBB..." (18 bytes)
        let payload = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAaaaa\x1b[38;2;128;128;128mBBBBBBBBBBBBBBBBBB";
        buf.push_chunk(payload);
        let total = payload.len();
        // SGR starts at offset 30 (ESC byte), unsafe span is [31, 50).
        // Text 'B's start at offset 50.

        // With budget=30, intended_start = total - 30.  This should land
        // inside the escape sequence's unsafe span, so read_recent advances
        // to the safe_resume offset (50).
        let result = buf.read_recent(30);
        assert_eq!(
            result, b"BBBBBBBBBBBBBBBBBB",
            "expected only the text after the SGR, total={total}"
        );
    }

    // ---- read_for_client gap handling tests ----

    #[test]
    fn read_for_client_gap_advances_to_ground_boundary() {
        let mut buf = OutputFanoutBuffer::new(64);
        let client = ClientId(Uuid::new_v4());
        buf.register_client_at_tail(client);
        // Client cursor is now at end_offset = 0.

        // Push a small chunk so the client cursor (0) falls behind after
        // the next large push causes eviction.
        buf.push_chunk(b"\x1b[31mhello");
        // end_offset = 11, cursor still at 0.

        // Large push that overflows the 64-byte buffer.  The start will
        // contain CSI bytes that become the new buffer front.
        let mut big_chunk = Vec::new();
        big_chunk.extend_from_slice(b"\x1b[48;"); // CSI start (not ground)
        big_chunk.extend_from_slice(b"2;10;10;10m"); // completes CSI
        big_chunk.extend_from_slice(b"safe text here."); // ground state
        big_chunk.resize(80, b' ');
        buf.push_chunk(&big_chunk);

        // After push_chunk, the buffer evicted old bytes.  The client cursor
        // was behind start_offset and was clamped to the nearest ground
        // boundary (which is after the "m" in "2;10;10;10m").

        // Verify that the data returned does NOT start with CSI parameter
        // bytes like "2;10;10;10m".
        let read = buf.read_for_client(client, 1024);
        assert!(
            !read.bytes.starts_with(b"2;10;10;10m"),
            "gap read returned mid-sequence data: {:?}",
            String::from_utf8_lossy(&read.bytes[..20.min(read.bytes.len())])
        );
        // The first byte should be 's' from "safe text here." or a space.
        if !read.bytes.is_empty() {
            assert!(
                read.bytes[0] == b's' || read.bytes[0] == b' ',
                "expected ground-state text, got byte {}",
                read.bytes[0]
            );
        }
    }

    #[test]
    fn pane_output_event_from_read_skips_empty_without_gap() {
        let event = pane_output_event_from_read(
            Uuid::new_v4(),
            Uuid::new_v4(),
            OutputRead {
                bytes: Vec::new(),
                stream_start: 10,
                stream_end: 10,
                stream_gap: false,
            },
            false,
        );

        assert!(event.is_none());
    }

    #[test]
    fn pane_output_event_from_read_emits_gap_even_with_empty_bytes() {
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let event = pane_output_event_from_read(
            session_id,
            pane_id,
            OutputRead {
                bytes: Vec::new(),
                stream_start: 128,
                stream_end: 128,
                stream_gap: true,
            },
            true,
        );

        assert_eq!(
            event,
            Some(Event::PaneOutput {
                session_id,
                pane_id,
                data: Vec::new(),
                stream_start: 128,
                stream_end: 128,
                stream_gap: true,
                sync_update_active: true,
            })
        );
    }

    #[test]
    fn esc_spans_pruned_on_eviction() {
        let mut buf = OutputFanoutBuffer::new(32);

        // Push sequences that create esc_spans, then push enough to evict them.
        buf.push_chunk(b"\x1b[mA\x1b[mB\x1b[mC"); // 3 escape sequences
        let pre_evict_count = buf.esc_spans.len();
        assert!(pre_evict_count >= 3);

        // Push enough to evict the earlier data.
        buf.push_chunk(&[b'X'; 40]);

        // All old spans should be pruned.
        for &(esc_start, safe_resume) in &buf.esc_spans {
            assert!(
                safe_resume == u64::MAX || safe_resume > buf.start_offset,
                "stale span not pruned: ({esc_start}, {safe_resume}), start_offset={}",
                buf.start_offset
            );
        }
    }
}
