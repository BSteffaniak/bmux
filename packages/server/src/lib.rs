#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Server component for bmux terminal multiplexer.

mod persistence;

use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_ipc::transport::{IpcTransportError, LocalIpcListener, LocalIpcStream};
use bmux_ipc::{
    AttachFocusTarget, AttachGrant, AttachLayer, AttachPaneChunk, AttachRect, AttachScene,
    AttachSurface, AttachSurfaceKind, AttachViewComponent, CURRENT_PROTOCOL_VERSION, ClientSummary,
    Envelope, EnvelopeKind, ErrorCode, ErrorResponse, Event, IpcEndpoint, PaneFocusDirection,
    PaneLayoutNode as IpcPaneLayoutNode, PaneSelector, PaneSplitDirection, PaneSummary,
    ProtocolVersion, Request, Response, ResponsePayload, ServerSnapshotStatus,
    SessionPermissionSummary, SessionRole, SessionSelector, SessionSummary, WindowSelector,
    WindowSummary, decode, encode,
};
use bmux_session::{ClientId, Session, SessionId, SessionManager, WindowId};
use bmux_terminal_protocol::{ProtocolProfile, TerminalProtocolEngine, protocol_profile_for_term};
use persistence::{
    ClientSelectedSessionSnapshotV2, FloatingSurfaceSnapshotV3, FollowEdgeSnapshotV2,
    OwnerPrincipalSnapshotV2, PaneLayoutNodeSnapshotV2, PaneSnapshotV2,
    PaneSplitDirectionSnapshotV2, RoleAssignmentSnapshotV2, SessionSnapshotV3, SnapshotManager,
    SnapshotV3, WindowSnapshotV3,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uuid::Uuid;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_TOKEN_TTL: Duration = Duration::from_secs(10);
const MAX_WINDOW_OUTPUT_BUFFER_BYTES: usize = 1_048_576;
const SNAPSHOT_DEBOUNCE_INTERVAL: Duration = Duration::from_millis(300);

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
    permission_state: Mutex<PermissionState>,
    snapshot_runtime: Mutex<SnapshotRuntime>,
    operation_lock: AsyncMutex<()>,
    event_hub: Mutex<EventHub>,
    client_principals: Mutex<BTreeMap<ClientId, Uuid>>,
    server_owner_principal_id: Uuid,
    handshake_timeout: Duration,
    pane_exit_rx: AsyncMutex<mpsc::UnboundedReceiver<PaneExitEvent>>,
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
    windows: usize,
    roles: usize,
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
    session_id: SessionId,
}

#[derive(Debug, Default)]
struct FollowState {
    connected_clients: std::collections::BTreeSet<ClientId>,
    selected_sessions: BTreeMap<ClientId, Option<SessionId>>,
    follows: BTreeMap<ClientId, FollowEntry>,
}

#[derive(Debug, Default)]
struct PermissionState {
    owner_principals: BTreeMap<SessionId, Uuid>,
    roles: BTreeMap<SessionId, BTreeMap<ClientId, SessionRole>>,
}

impl PermissionState {
    fn ensure_owner(&mut self, session_id: SessionId, owner_principal_id: Uuid) {
        self.owner_principals.insert(session_id, owner_principal_id);
    }

    fn owner_principal_for(&self, session_id: SessionId) -> Option<Uuid> {
        self.owner_principals.get(&session_id).copied()
    }

    fn role_for(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        principal_id: Uuid,
    ) -> SessionRole {
        if self.owner_principal_for(session_id) == Some(principal_id) {
            return SessionRole::Owner;
        }
        self.roles
            .get(&session_id)
            .and_then(|session_roles| session_roles.get(&client_id).copied())
            .unwrap_or(SessionRole::Observer)
    }

    fn set_owner_principal(&mut self, session_id: SessionId, principal_id: Uuid) {
        self.owner_principals.insert(session_id, principal_id);
    }

    fn set_role(&mut self, session_id: SessionId, client_id: ClientId, role: SessionRole) {
        let session_roles = self.roles.entry(session_id).or_default();
        session_roles.insert(client_id, role);
    }

    fn clear_to_observer(&mut self, session_id: SessionId, client_id: ClientId) {
        if let Some(session_roles) = self.roles.get_mut(&session_id) {
            session_roles.remove(&client_id);
        }
    }

    fn remove_session(&mut self, session_id: SessionId) {
        self.owner_principals.remove(&session_id);
        self.roles.remove(&session_id);
    }

    fn list_permissions(
        &self,
        session_id: SessionId,
        connected_clients: &BTreeSet<ClientId>,
        client_principals: &BTreeMap<ClientId, Uuid>,
    ) -> Vec<SessionPermissionSummary> {
        let mut permissions = self
            .roles
            .get(&session_id)
            .map(|session_roles| {
                session_roles
                    .iter()
                    .map(|(client_id, role)| SessionPermissionSummary {
                        client_id: client_id.0,
                        role: *role,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if let Some(owner_principal_id) = self.owner_principal_for(session_id) {
            for client_id in connected_clients {
                if client_principals.get(client_id) == Some(&owner_principal_id)
                    && !permissions
                        .iter()
                        .any(|entry| entry.client_id == client_id.0)
                {
                    permissions.push(SessionPermissionSummary {
                        client_id: client_id.0,
                        role: SessionRole::Owner,
                    });
                }
            }
        }

        permissions
    }

    fn clear_client_roles(&mut self, client_id: ClientId) {
        for roles in self.roles.values_mut() {
            roles.remove(&client_id);
        }
    }
}

impl FollowState {
    fn connect_client(&mut self, client_id: ClientId) {
        self.connected_clients.insert(client_id);
        self.selected_sessions.entry(client_id).or_insert(None);
    }

    fn disconnect_client(&mut self, client_id: ClientId) -> Vec<Event> {
        self.connected_clients.remove(&client_id);
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

    fn set_selected_session(&mut self, client_id: ClientId, session_id: Option<SessionId>) {
        if self.connected_clients.contains(&client_id) {
            self.selected_sessions.insert(client_id, session_id);
        }
    }

    fn selected_session(&self, client_id: ClientId) -> Option<Option<SessionId>> {
        self.selected_sessions.get(&client_id).copied()
    }

    fn start_follow(
        &mut self,
        follower_client_id: ClientId,
        leader_client_id: ClientId,
        global: bool,
    ) -> std::result::Result<Option<SessionId>, &'static str> {
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

        if global
            && let Some(leader_selected) = self.selected_sessions.get(&leader_client_id).copied()
        {
            self.selected_sessions
                .insert(follower_client_id, leader_selected);
            return Ok(leader_selected);
        }

        Ok(None)
    }

    fn stop_follow(&mut self, follower_client_id: ClientId) -> bool {
        self.follows.remove(&follower_client_id).is_some()
    }

    fn sync_followers_from_leader(
        &mut self,
        leader_client_id: ClientId,
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
                self.selected_sessions.insert(follower_id, selected_session);
                if let Some(session_id) = selected_session
                    && previous != Some(session_id)
                {
                    updates.push(FollowTargetUpdate {
                        follower_client_id: follower_id,
                        leader_client_id,
                        session_id,
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
                let (following_client_id, following_global) =
                    self.follows.get(client_id).map_or((None, false), |entry| {
                        (Some(entry.leader_client_id.0), entry.global)
                    });

                ClientSummary {
                    id: client_id.0,
                    selected_session_id,
                    following_client_id,
                    following_global,
                    session_role: None,
                }
            })
            .collect()
    }
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
    if configured.is_empty() {
        "xterm-256color".to_string()
    } else {
        configured.to_string()
    }
}

async fn shutdown_runtime_handle(removed: RemovedRuntime) {
    for window in removed.handle.windows.into_values() {
        shutdown_window_handle(window).await;
    }
}

async fn shutdown_window_handle(window: WindowRuntimeHandle) {
    for pane in window.panes.into_values() {
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
}

struct SessionRuntimeHandle {
    windows: BTreeMap<bmux_session::WindowId, WindowRuntimeHandle>,
    active_window: bmux_session::WindowId,
    next_window_number: u32,
    attached_clients: BTreeSet<ClientId>,
    attach_viewport: Option<AttachViewport>,
    attach_view_revision: u64,
}

#[derive(Clone, Copy)]
struct AttachViewport {
    cols: u16,
    rows: u16,
}

struct WindowRuntimeHandle {
    id: bmux_session::WindowId,
    number: u32,
    name: Option<String>,
    panes: BTreeMap<Uuid, PaneRuntimeHandle>,
    layout_root: PaneLayoutNode,
    focused_pane_id: Uuid,
    floating_surfaces: Vec<FloatingSurfaceRuntime>,
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
    stop_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
    input_tx: mpsc::UnboundedSender<PaneRuntimeCommand>,
    output_buffer: Arc<std::sync::Mutex<OutputFanoutBuffer>>,
    exited: Arc<AtomicBool>,
    last_requested_size: Arc<std::sync::Mutex<(u16, u16)>>,
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

    fn reset_attached_clients_to_active_pane_tail(
        &mut self,
        session_id: SessionId,
    ) -> Result<(), SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let attached_clients = runtime.attached_clients.iter().copied().collect::<Vec<_>>();
        let window = runtime
            .windows
            .get_mut(&runtime.active_window)
            .ok_or(SessionRuntimeError::NotFound)?;
        let pane = window
            .panes
            .get_mut(&window.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let mut output = pane
            .output_buffer
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        for client_id in attached_clients {
            output.reset_client_to_tail(client_id);
        }
        Ok(())
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

    fn reset_client_to_tail(&mut self, client_id: ClientId) {
        self.register_client_at_tail(client_id);
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

struct RemovedWindowRuntime {
    session_id: SessionId,
    window_id: bmux_session::WindowId,
    handle: WindowRuntimeHandle,
    session_removed: Option<RemovedRuntime>,
}

struct WindowRuntimeSummary {
    id: bmux_session::WindowId,
    number: u32,
    name: Option<String>,
    active: bool,
}

struct AttachLayoutState {
    window_id: bmux_session::WindowId,
    focused_pane_id: Uuid,
    panes: Vec<PaneSummary>,
    layout_root: IpcPaneLayoutNode,
    scene: AttachScene,
}

struct AttachSnapshotState {
    session_id: SessionId,
    window_id: bmux_session::WindowId,
    focused_pane_id: Uuid,
    panes: Vec<PaneSummary>,
    layout_root: IpcPaneLayoutNode,
    scene: AttachScene,
    chunks: Vec<AttachPaneChunk>,
}

#[derive(Debug, Clone)]
struct RestoreWindowRuntimeSpec {
    id: bmux_session::WindowId,
    number: u32,
    name: Option<String>,
    panes: Vec<PaneRuntimeMeta>,
    layout_root: Option<PaneLayoutNode>,
    focused_pane_id: Uuid,
    floating_surfaces: Vec<FloatingSurfaceRuntime>,
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

#[derive(Debug, Clone)]
enum WindowSelection {
    Active,
    Id(bmux_session::WindowId),
    Number(u32),
    Name(String),
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
                anyhow::bail!("duplicate pane id {} in runtime layout", pane_id)
            }
        }
        PaneLayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !(0.1..=0.9).contains(ratio) {
                anyhow::bail!("runtime split ratio {} out of range [0.1, 0.9]", ratio)
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

fn attach_rect_from_layout_rect(rect: LayoutRect) -> AttachRect {
    AttachRect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    }
}

fn scene_root_from_viewport(viewport: Option<AttachViewport>) -> LayoutRect {
    let (cols, rows) = viewport.map_or((0, 0), |viewport| (viewport.cols, viewport.rows));
    LayoutRect {
        x: 0,
        y: 1,
        w: cols.max(1),
        h: rows.saturating_sub(1).max(1),
    }
}

fn build_attach_scene(
    session_id: SessionId,
    window: &WindowRuntimeHandle,
    viewport: Option<AttachViewport>,
) -> AttachScene {
    let mut rects = BTreeMap::new();
    collect_layout_rects(
        &window.layout_root,
        scene_root_from_viewport(viewport),
        &mut rects,
    );

    let mut pane_ids = Vec::new();
    window.layout_root.pane_order(&mut pane_ids);

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
                cursor_owner: pane_id == window.focused_pane_id,
                pane_id: Some(pane_id),
            })
        })
        .collect::<Vec<_>>();

    surfaces.extend(
        window
            .floating_surfaces
            .iter()
            .filter(|surface| window.panes.contains_key(&surface.pane_id))
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
        window_id: window.id.0,
        focus: AttachFocusTarget::Pane {
            pane_id: window.focused_pane_id,
        },
        surfaces,
    }
}

fn pane_pty_size(layout_rect: LayoutRect) -> (u16, u16) {
    let cols = layout_rect.w.saturating_sub(2).max(1);
    let rows = layout_rect.h.saturating_sub(2).max(1);
    (rows, cols)
}

fn resize_active_window_ptys(runtime: &mut SessionRuntimeHandle, cols: u16, rows: u16) {
    let root = LayoutRect {
        x: 0,
        y: 1,
        w: cols.max(1),
        h: rows.saturating_sub(1).max(1),
    };
    let Some(window) = runtime.windows.get_mut(&runtime.active_window) else {
        return;
    };

    let mut rects = BTreeMap::new();
    collect_layout_rects(&window.layout_root, root, &mut rects);
    for (pane_id, pane) in &window.panes {
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
    ) -> Self {
        Self {
            runtimes: BTreeMap::new(),
            shell,
            pane_term,
            protocol_profile,
            pane_exit_tx,
        }
    }

    fn start_runtime(&mut self, session_id: SessionId) -> Result<()> {
        if self.runtimes.contains_key(&session_id) {
            anyhow::bail!("runtime already exists for session {}", session_id.0);
        }

        let first_window = self.spawn_window_runtime(
            session_id,
            None,
            1,
            Some("window-1".to_string()),
            None,
            Some("pane-1".to_string()),
            None,
        )?;
        let first_window_id = first_window.id;
        let mut windows = BTreeMap::new();
        windows.insert(first_window.id, first_window);

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                windows,
                active_window: first_window_id,
                next_window_number: 2,
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
        windows: Vec<RestoreWindowRuntimeSpec>,
        active_window: bmux_session::WindowId,
        next_window_number: u32,
    ) -> Result<()> {
        if self.runtimes.contains_key(&session_id) {
            anyhow::bail!("runtime already exists for session {}", session_id.0);
        }

        let mut runtime_windows = BTreeMap::new();
        for window in windows {
            if window.panes.is_empty() {
                anyhow::bail!("restored runtime window must include panes");
            }
            if !window
                .panes
                .iter()
                .any(|pane| pane.id == window.focused_pane_id)
            {
                anyhow::bail!("focused pane missing from restored runtime window");
            }
            let handle = self.spawn_window_runtime(
                session_id,
                Some(window.id),
                window.number,
                window.name,
                Some(window.focused_pane_id),
                None,
                None,
            )?;
            let mut handle = handle;
            let existing_focused = handle.focused_pane_id;
            if let Some(existing) = handle.panes.remove(&existing_focused) {
                tokio::spawn(async move {
                    shutdown_pane_handle(existing).await;
                });
            }

            handle.panes.clear();
            for pane_meta in window.panes {
                let pane = self.spawn_pane_runtime(session_id, pane_meta.clone(), window.id)?;
                handle.panes.insert(pane_meta.id, pane);
            }
            handle.layout_root = window.layout_root.unwrap_or_else(|| {
                let metas = handle
                    .panes
                    .values()
                    .map(|pane| pane.meta.clone())
                    .collect::<Vec<_>>();
                layout_from_panes(&metas).expect("restored window has panes")
            });
            validate_runtime_layout_matches_panes(&handle.layout_root, &handle.panes)?;
            handle.focused_pane_id = window.focused_pane_id;
            handle.floating_surfaces = window.floating_surfaces;
            runtime_windows.insert(window.id, handle);
        }

        if !runtime_windows.contains_key(&active_window) {
            anyhow::bail!("active window missing from restore runtime");
        }

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                windows: runtime_windows,
                active_window,
                next_window_number,
                attached_clients: BTreeSet::new(),
                attach_viewport: None,
                attach_view_revision: 0,
            },
        );

        Ok(())
    }

    fn spawn_window_runtime(
        &self,
        session_id: SessionId,
        id: Option<bmux_session::WindowId>,
        number: u32,
        name: Option<String>,
        pane_id: Option<Uuid>,
        pane_name: Option<String>,
        pane_shell: Option<String>,
    ) -> Result<WindowRuntimeHandle> {
        let window_id = id.unwrap_or_default();
        let pane_meta = PaneRuntimeMeta {
            id: pane_id.unwrap_or(window_id.0),
            name: pane_name,
            shell: pane_shell.unwrap_or_else(|| self.shell.clone()),
        };
        let pane = self.spawn_pane_runtime(session_id, pane_meta.clone(), window_id)?;
        let mut panes = BTreeMap::new();
        panes.insert(pane_meta.id, pane);

        Ok(WindowRuntimeHandle {
            id: window_id,
            number,
            name,
            panes,
            layout_root: PaneLayoutNode::Leaf {
                pane_id: pane_meta.id,
            },
            focused_pane_id: pane_meta.id,
            floating_surfaces: Vec::new(),
        })
    }

    fn spawn_pane_runtime(
        &self,
        session_id: SessionId,
        pane_meta: PaneRuntimeMeta,
        window_id: bmux_session::WindowId,
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
        let output_buffer_for_reader = Arc::clone(&output_buffer);
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_task = Arc::clone(&exited);

        let task = tokio::spawn(async move {
            let pty_system = native_pty_system();
            let pty_pair = match pty_system.openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                Ok(pair) => pair,
                Err(_) => return,
            };

            let mut command = CommandBuilder::new(&shell);
            command.env("TERM", &pane_term);
            let mut child = match pty_pair.slave.spawn_command(command) {
                Ok(child) => child,
                Err(_) => return,
            };
            let mut child_killer = child.clone_killer();
            drop(pty_pair.slave);

            let master = pty_pair.master;

            let mut reader = if let Ok(reader) = master.try_clone_reader() {
                reader
            } else {
                let _ = child.kill();
                return;
            };
            let writer = if let Ok(writer) = master.take_writer() {
                writer
            } else {
                let _ = child.kill();
                return;
            };
            let writer = Arc::new(std::sync::Mutex::new(writer));

            let (child_exit_tx, mut child_exit_rx) = mpsc::unbounded_channel::<()>();
            let exited_for_waiter = Arc::clone(&exited_for_task);
            let child_waiter = std::thread::Builder::new()
                .name(format!(
                    "bmux-server-window-{}-pane-{}-wait",
                    window_id.0, pane_id
                ))
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
                .name(format!(
                    "bmux-server-window-{}-pane-{}",
                    window_id.0, pane_id
                ))
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
                                let reply = protocol_engine.process_output(chunk, (0, 0));
                                if !reply.is_empty() {
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
            stop_tx: Some(stop_tx),
            task,
            input_tx,
            output_buffer,
            exited,
            last_requested_size,
        })
    }

    fn new_window(
        &mut self,
        session_id: SessionId,
        name: Option<String>,
    ) -> Result<(bmux_session::WindowId, u32, Option<String>)> {
        let number = self
            .runtimes
            .get(&session_id)
            .map(|session| session.next_window_number)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let resolved_name = name.or_else(|| Some(format!("window-{number}")));
        let window = self.spawn_window_runtime(
            session_id,
            None,
            number,
            resolved_name.clone(),
            None,
            Some("pane-1".to_string()),
            None,
        )?;
        let window_id = window.id;

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        session.windows.insert(window_id, window);
        session.next_window_number = session.next_window_number.saturating_add(1);
        self.apply_stored_attach_viewport(session_id);
        Ok((window_id, number, resolved_name))
    }

    fn list_windows(&self, session_id: SessionId) -> Result<Vec<WindowRuntimeSummary>> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;

        let mut windows = session
            .windows
            .values()
            .map(|window| WindowRuntimeSummary {
                id: window.id,
                number: window.number,
                name: window.name.clone(),
                active: window.id == session.active_window,
            })
            .collect::<Vec<_>>();
        windows.sort_by_key(|window| (window.number, window.id));
        Ok(windows)
    }

    fn split_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
    ) -> Result<Uuid> {
        let (window_id, target_pane_id, next_pane_name, shell, client_ids) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let window = session
                .windows
                .get_mut(&session.active_window)
                .ok_or_else(|| anyhow::anyhow!("active window not found"))?;
            let target_pane_id =
                resolve_pane_id_from_selector(window, target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let focused = window
                .panes
                .get(&target_pane_id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let name_prefix = match direction {
                PaneSplitDirection::Vertical => "v",
                PaneSplitDirection::Horizontal => "h",
            };
            (
                window.id,
                target_pane_id,
                Some(format!("{name_prefix}-pane-{}", window.panes.len() + 1)),
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
        let handle = self.spawn_pane_runtime(session_id, pane_meta, window_id)?;
        for client_id in client_ids {
            if let Ok(mut output) = handle.output_buffer.lock() {
                output.register_client_at_tail(client_id);
            }
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let window = session
            .windows
            .get_mut(&session.active_window)
            .ok_or_else(|| anyhow::anyhow!("active window not found"))?;
        window.panes.insert(pane_id, handle);
        let replaced =
            window
                .layout_root
                .replace_leaf_with_split(target_pane_id, direction, 0.5, pane_id);
        if !replaced {
            anyhow::bail!("failed to apply split to layout tree")
        }
        window.focused_pane_id = pane_id;
        self.apply_stored_attach_viewport(session_id);
        Ok(pane_id)
    }

    fn focus_pane(&mut self, session_id: SessionId, direction: PaneFocusDirection) -> Result<Uuid> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let window = session
            .windows
            .get_mut(&session.active_window)
            .ok_or_else(|| anyhow::anyhow!("active window not found"))?;
        let mut pane_ids = Vec::new();
        window.layout_root.pane_order(&mut pane_ids);
        if pane_ids.is_empty() {
            anyhow::bail!("no panes in active window")
        }
        let current_index = pane_ids
            .iter()
            .position(|id| *id == window.focused_pane_id)
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
        window.focused_pane_id = pane_ids[next_index];
        Ok(window.focused_pane_id)
    }

    fn focus_pane_target(&mut self, session_id: SessionId, target: PaneSelector) -> Result<Uuid> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let window = session
            .windows
            .get_mut(&session.active_window)
            .ok_or_else(|| anyhow::anyhow!("active window not found"))?;
        let pane_id = resolve_pane_id_from_selector(window, target)
            .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
        window.focused_pane_id = pane_id;
        Ok(pane_id)
    }

    fn close_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> Result<(Uuid, Option<RemovedWindowRuntime>)> {
        let (window_id, pane_id, remove_window) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let window_id = session.active_window;
            let window = session
                .windows
                .get_mut(&window_id)
                .ok_or_else(|| anyhow::anyhow!("active window not found"))?;
            let pane_id =
                resolve_pane_id_from_selector(window, target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            (window_id, pane_id, window.panes.len() == 1)
        };

        if remove_window {
            let removed = self.kill_window(session_id, WindowSelection::Id(window_id))?;
            return Ok((pane_id, Some(removed)));
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let window = session
            .windows
            .get_mut(&window_id)
            .ok_or_else(|| anyhow::anyhow!("active window not found"))?;
        let pane = window
            .panes
            .remove(&pane_id)
            .ok_or_else(|| anyhow::anyhow!("focused pane not found"))?;
        let _ = window.layout_root.remove_leaf(pane_id);
        let mut remaining = Vec::new();
        window.layout_root.pane_order(&mut remaining);
        if (window.focused_pane_id == pane_id
            || !window.panes.contains_key(&window.focused_pane_id))
            && let Some(next_id) = remaining.first().copied()
        {
            window.focused_pane_id = next_id;
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
        let window = session
            .windows
            .get_mut(&session.active_window)
            .ok_or_else(|| anyhow::anyhow!("active window not found"))?;
        let pane_id = resolve_pane_id_from_selector(window, target.unwrap_or(PaneSelector::Active))
            .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
        let step = f32::from(delta) * 0.05;
        let _ = window.layout_root.adjust_focused_ratio(pane_id, step);
        self.apply_stored_attach_viewport(session_id);
        Ok(())
    }

    fn list_panes(
        &self,
        session_id: SessionId,
    ) -> Result<(bmux_session::WindowId, Vec<PaneSummary>)> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let window = session
            .windows
            .get(&session.active_window)
            .ok_or_else(|| anyhow::anyhow!("active window not found"))?;
        let mut pane_ids = Vec::new();
        window.layout_root.pane_order(&mut pane_ids);
        let panes = pane_ids
            .iter()
            .enumerate()
            .filter_map(|(index, pane_id)| {
                window.panes.get(pane_id).map(|pane| PaneSummary {
                    id: *pane_id,
                    index: (index + 1) as u32,
                    name: pane.meta.name.clone(),
                    focused: *pane_id == window.focused_pane_id,
                })
            })
            .collect::<Vec<_>>();
        Ok((window.id, panes))
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
        let window = session
            .windows
            .get(&session.active_window)
            .ok_or(SessionRuntimeError::NotFound)?;
        let scene = build_attach_scene(session_id, window, session.attach_viewport);
        let mut pane_ids = Vec::new();
        window.layout_root.pane_order(&mut pane_ids);
        let panes = pane_ids
            .iter()
            .enumerate()
            .filter_map(|(index, pane_id)| {
                window.panes.get(pane_id).map(|pane| PaneSummary {
                    id: *pane_id,
                    index: (index + 1) as u32,
                    name: pane.meta.name.clone(),
                    focused: *pane_id == window.focused_pane_id,
                })
            })
            .collect::<Vec<_>>();
        Ok(AttachLayoutState {
            window_id: window.id,
            focused_pane_id: window.focused_pane_id,
            panes,
            layout_root: ipc_layout_from_runtime(&window.layout_root),
            scene,
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
        let window = session
            .windows
            .get(&session.active_window)
            .ok_or(SessionRuntimeError::NotFound)?;
        let pane = window
            .panes
            .get(&window.focused_pane_id)
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
        let window = session
            .windows
            .get(&session.active_window)
            .ok_or(SessionRuntimeError::NotFound)?;
        let scene = build_attach_scene(session_id, window, session.attach_viewport);
        let mut pane_ids = Vec::new();
        window.layout_root.pane_order(&mut pane_ids);
        let panes = pane_ids
            .iter()
            .enumerate()
            .filter_map(|(index, pane_id)| {
                window.panes.get(pane_id).map(|pane| PaneSummary {
                    id: *pane_id,
                    index: (index + 1) as u32,
                    name: pane.meta.name.clone(),
                    focused: *pane_id == window.focused_pane_id,
                })
            })
            .collect::<Vec<_>>();

        let mut chunks = Vec::new();
        for pane_id in pane_ids {
            let Some(pane) = window.panes.get(&pane_id) else {
                continue;
            };
            let data = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .read_recent(max_bytes_per_pane);
            chunks.push(AttachPaneChunk { pane_id, data });
        }

        Ok(AttachSnapshotState {
            session_id,
            window_id: window.id,
            focused_pane_id: window.focused_pane_id,
            panes,
            layout_root: ipc_layout_from_runtime(&window.layout_root),
            scene,
            chunks,
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
            let window = session
                .windows
                .get_mut(&session.active_window)
                .ok_or(SessionRuntimeError::NotFound)?;

            let mut chunks = Vec::new();
            let mut closed_active = false;
            for pane_id in pane_ids {
                let Some(pane) = window.panes.get_mut(pane_id) else {
                    continue;
                };
                let mut output = pane
                    .output_buffer
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)?;
                let data = output.read_for_client(client_id, max_bytes);
                drop(output);
                if pane.exited.load(Ordering::SeqCst) {
                    if *pane_id == window.focused_pane_id {
                        closed_active = true;
                    }
                }
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

    fn switch_window(
        &mut self,
        session_id: SessionId,
        selector: WindowSelection,
    ) -> Result<bmux_session::WindowId> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let lookup_selector = selector.clone();
        let window_id = resolve_window_id_from_selector(session, selector).ok_or_else(|| {
            anyhow::anyhow!(window_not_found_in_session_message(
                &lookup_selector,
                session_id
            ))
        })?;
        session.active_window = window_id;
        self.reset_attached_clients_to_active_pane_tail(session_id)
            .map_err(|error| anyhow::anyhow!("failed resetting attach cursors: {error:?}"))?;
        self.apply_stored_attach_viewport(session_id);
        Ok(window_id)
    }

    fn kill_window(
        &mut self,
        session_id: SessionId,
        selector: WindowSelection,
    ) -> Result<RemovedWindowRuntime> {
        let (window_id, is_last_window) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let lookup_selector = selector.clone();
            let Some(window_id) = resolve_window_id_from_selector(session, selector) else {
                anyhow::bail!(
                    "{}",
                    window_not_found_in_session_message(&lookup_selector, session_id)
                );
            };
            let is_last_window = session.windows.len() == 1;
            if !is_last_window && session.active_window == window_id {
                let next_active = session
                    .windows
                    .values()
                    .filter(|window| window.id != window_id)
                    .min_by_key(|window| window.number)
                    .map(|window| window.id)
                    .ok_or_else(|| anyhow::anyhow!("failed selecting next active window"))?;
                session.active_window = next_active;
            }
            (window_id, is_last_window)
        };

        if is_last_window {
            let mut removed_session = self.remove_runtime(session_id)?;
            let window = removed_session
                .handle
                .windows
                .remove(&window_id)
                .ok_or_else(|| anyhow::anyhow!("window missing during session removal"))?;
            return Ok(RemovedWindowRuntime {
                session_id,
                window_id,
                handle: window,
                session_removed: Some(removed_session),
            });
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let window = session
            .windows
            .remove(&window_id)
            .ok_or_else(|| anyhow::anyhow!("window not found in session {}", session_id.0))?;
        Ok(RemovedWindowRuntime {
            session_id,
            window_id,
            handle: window,
            session_removed: None,
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

        let window = runtime
            .windows
            .get(&runtime.active_window)
            .ok_or(SessionRuntimeError::NotFound)?;
        let pane = window
            .panes
            .get(&window.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if pane.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }

        runtime.attached_clients.insert(client_id);
        for window in runtime.windows.values_mut() {
            for pane in window.panes.values_mut() {
                let mut output = pane
                    .output_buffer
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)?;
                output.register_client_at_tail(client_id);
            }
        }
        if let Some(viewport) = runtime.attach_viewport {
            resize_active_window_ptys(runtime, viewport.cols, viewport.rows);
        }
        Ok(())
    }

    fn end_attach(&mut self, session_id: SessionId, client_id: ClientId) {
        if let Some(runtime) = self.runtimes.get_mut(&session_id) {
            let removed = runtime.attached_clients.remove(&client_id);
            if removed {
                for window in runtime.windows.values_mut() {
                    for pane in window.panes.values_mut() {
                        if let Ok(mut output) = pane.output_buffer.lock() {
                            output.unregister_client(client_id);
                        }
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
    ) -> Result<(u16, u16), SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let cols = cols.max(1);
        let rows = rows.max(2);
        runtime.attach_viewport = Some(AttachViewport { cols, rows });
        resize_active_window_ptys(runtime, cols, rows);
        Ok((cols, rows))
    }

    fn apply_stored_attach_viewport(&mut self, session_id: SessionId) {
        let Some(runtime) = self.runtimes.get_mut(&session_id) else {
            return;
        };
        let Some(viewport) = runtime.attach_viewport else {
            return;
        };
        resize_active_window_ptys(runtime, viewport.cols, viewport.rows);
    }

    fn write_input(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<usize, SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let window = runtime
            .windows
            .get_mut(&runtime.active_window)
            .ok_or(SessionRuntimeError::NotFound)?;
        let pane = window
            .panes
            .get_mut(&window.focused_pane_id)
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

        let window = runtime
            .windows
            .get_mut(&runtime.active_window)
            .ok_or(SessionRuntimeError::NotFound)?;
        let pane = window
            .panes
            .get_mut(&window.focused_pane_id)
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

    #[cfg(test)]
    fn window_count(&self, session_id: SessionId) -> usize {
        self.runtimes
            .get(&session_id)
            .map_or(0, |runtime| runtime.windows.len())
    }
}

fn resolve_window_id_from_selector(
    session: &SessionRuntimeHandle,
    selector: WindowSelection,
) -> Option<bmux_session::WindowId> {
    let sorted_windows = || {
        let mut windows = session.windows.values().collect::<Vec<_>>();
        windows.sort_by_key(|window| (window.number, window.id));
        windows
    };

    match selector {
        WindowSelection::Active => Some(session.active_window),
        WindowSelection::Id(id) => session.windows.contains_key(&id).then_some(id),
        WindowSelection::Number(number) => session
            .windows
            .values()
            .find(|window| window.number == number)
            .map(|window| window.id),
        WindowSelection::Name(value) => {
            let windows = sorted_windows();

            if let Some(window) = windows
                .iter()
                .find(|window| window.name.as_deref() == Some(value.as_str()))
            {
                return Some(window.id);
            }

            if let Some(window) = windows
                .iter()
                .find(|window| window.id.to_string().eq_ignore_ascii_case(&value))
            {
                return Some(window.id);
            }

            let value_lower = value.to_ascii_lowercase();
            windows
                .iter()
                .find(|window| {
                    window
                        .id
                        .to_string()
                        .to_ascii_lowercase()
                        .starts_with(&value_lower)
                })
                .map(|window| window.id)
        }
    }
}

fn resolve_pane_id_from_selector(
    window: &WindowRuntimeHandle,
    selector: PaneSelector,
) -> Option<Uuid> {
    match selector {
        PaneSelector::Active => window
            .panes
            .contains_key(&window.focused_pane_id)
            .then_some(window.focused_pane_id),
        PaneSelector::ById(id) => window.panes.contains_key(&id).then_some(id),
        PaneSelector::ByIndex(index) => {
            if index == 0 {
                return None;
            }
            let mut pane_ids = Vec::new();
            window.layout_root.pane_order(&mut pane_ids);
            let position = usize::try_from(index.saturating_sub(1)).ok()?;
            let pane_id = pane_ids.get(position).copied()?;
            window.panes.contains_key(&pane_id).then_some(pane_id)
        }
    }
}

fn window_not_found_in_session_message(
    selector: &WindowSelection,
    session_id: SessionId,
) -> String {
    match selector {
        WindowSelection::Name(_) => format!(
            "window not found in session {} for selector {selector:?} (lookup order: exact name -> exact UUID -> UUID prefix)",
            session_id.0
        ),
        _ => format!(
            "window not found in session {} for selector {selector:?}",
            session_id.0
        ),
    }
}

impl BmuxServer {
    fn new_with_snapshot(
        endpoint: IpcEndpoint,
        snapshot_manager: Option<SnapshotManager>,
        server_owner_principal_id: Uuid,
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
        Self {
            endpoint,
            state: Arc::new(ServerState {
                session_manager: Mutex::new(SessionManager::new()),
                session_runtimes: Mutex::new(SessionRuntimeManager::new(
                    shell,
                    pane_term,
                    protocol_profile,
                    pane_exit_tx,
                )),
                attach_tokens: Mutex::new(AttachTokenManager::new(ATTACH_TOKEN_TTL)),
                follow_state: Mutex::new(FollowState::default()),
                permission_state: Mutex::new(PermissionState::default()),
                snapshot_runtime: Mutex::new(snapshot_runtime),
                operation_lock: AsyncMutex::new(()),
                event_hub: Mutex::new(EventHub::new(1024)),
                client_principals: Mutex::new(BTreeMap::new()),
                server_owner_principal_id,
                handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
                pane_exit_rx: AsyncMutex::new(pane_exit_rx),
            }),
            shutdown_tx,
        }
    }

    /// Create a server with an explicit endpoint.
    #[must_use]
    pub fn new(endpoint: IpcEndpoint) -> Self {
        Self::new_with_snapshot(endpoint, None, Uuid::new_v4())
    }

    /// Create a server with endpoint derived from config paths.
    #[must_use]
    pub fn from_config_paths(paths: &ConfigPaths) -> Self {
        #[cfg(unix)]
        let endpoint = IpcEndpoint::unix_socket(paths.server_socket());

        #[cfg(windows)]
        let endpoint = IpcEndpoint::windows_named_pipe(paths.server_named_pipe());

        let snapshot_manager = SnapshotManager::from_paths(paths);
        let server_owner_principal_id =
            load_or_create_principal_id(paths).unwrap_or_else(|error| {
                warn!("failed loading server owner principal id: {error}");
                Uuid::new_v4()
            });
        Self::new_with_snapshot(endpoint, Some(snapshot_manager), server_owner_principal_id)
    }

    /// Create a server using default bmux config paths.
    #[must_use]
    pub fn from_default_paths() -> Self {
        Self::from_config_paths(&ConfigPaths::default())
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

        let mut shutdown_rx = self.shutdown_tx.subscribe();
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        info!("bmux server shutdown requested");
                        break;
                    }
                    if changed.is_err() {
                        break;
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
                            return Err(error).context("accept loop failed");
                        }
                    }
                }
            }
        }

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
        if let Ok(mut permission_state) = self.state.permission_state.lock() {
            *permission_state = PermissionState::default();
        }
        let _ = emit_event(&self.state, Event::ServerStopping);
        if let Ok(mut attach_tokens) = self.state.attach_tokens.lock() {
            attach_tokens.clear();
        }

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
                server_owner_principal_id: state.server_owner_principal_id,
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

    loop {
        let envelope = match stream.recv_envelope().await {
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
                send_error(
                    &mut stream,
                    envelope.request_id,
                    ErrorCode::InvalidRequest,
                    format!("failed parsing request: {error:#}"),
                )
                .await?;
                continue;
            }
        };

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
        send_response(&mut stream, envelope.request_id, response).await?;
    }

    detach_client_state_on_disconnect(
        &state,
        client_id,
        &mut selected_session,
        &mut attached_stream_session,
    )?;
    disconnect_follow_state(&state, client_id)?;
    rebalance_owner_roles_after_disconnect(&state, client_id)?;
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
        AttachViewComponent::Tabs,
        AttachViewComponent::Status,
    ] {
        if components.contains(&component) {
            normalized.push(component);
        }
    }
    normalized
}

fn attach_view_components_for_pane_close(window_closed: bool) -> &'static [AttachViewComponent] {
    if window_closed {
        &[AttachViewComponent::Scene, AttachViewComponent::Tabs]
    } else {
        &[AttachViewComponent::Scene]
    }
}

fn emit_attach_view_changed_for_pane_close(
    state: &Arc<ServerState>,
    session_id: SessionId,
    window_closed: bool,
    session_closed: bool,
) -> Result<()> {
    if session_closed {
        return Ok(());
    }
    emit_attach_view_changed(
        state,
        session_id,
        attach_view_components_for_pane_close(window_closed),
    )
}

fn emit_attach_view_changed_for_layout(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> Result<()> {
    emit_attach_view_changed(state, session_id, &[AttachViewComponent::Scene])
}

fn emit_attach_view_changed_for_window_tabs(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> Result<()> {
    emit_attach_view_changed(state, session_id, &[AttachViewComponent::Tabs])
}

fn emit_attach_view_changed_for_window_switch(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> Result<()> {
    emit_attach_view_changed(
        state,
        session_id,
        &[AttachViewComponent::Scene, AttachViewComponent::Tabs],
    )
}

fn emit_attach_view_changed_for_status(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> Result<()> {
    emit_attach_view_changed(state, session_id, &[AttachViewComponent::Status])
}

fn unsubscribe_events(state: &Arc<ServerState>, client_id: ClientId) -> Result<()> {
    let mut hub = state
        .event_hub
        .lock()
        .map_err(|_| anyhow::anyhow!("event hub lock poisoned"))?;
    hub.unsubscribe(client_id);
    Ok(())
}

fn sync_selected_session_from_follow_state(
    state: &Arc<ServerState>,
    client_id: ClientId,
    selected_session: &mut Option<SessionId>,
) -> Result<()> {
    let follow_state = state
        .follow_state
        .lock()
        .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
    if let Some(follow_selected_session) = follow_state.selected_session(client_id) {
        *selected_session = follow_selected_session;
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
    let updates = {
        let mut follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        follow_state.set_selected_session(client_id, selected_session);
        follow_state.sync_followers_from_leader(client_id, selected_session)
    };

    for update in updates {
        emit_event(
            state,
            Event::FollowTargetChanged {
                follower_client_id: update.follower_client_id.0,
                leader_client_id: update.leader_client_id.0,
                session_id: update.session_id.0,
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
        follow_state.selected_sessions.insert(*client_id, None);
    }
    for client_id in affected_clients {
        let _ = follow_state.sync_followers_from_leader(client_id, None);
    }

    Ok(())
}

fn rebalance_owner_roles_after_disconnect(
    state: &Arc<ServerState>,
    disconnected_client_id: ClientId,
) -> Result<()> {
    let mut permission_state = state
        .permission_state
        .lock()
        .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
    permission_state.clear_client_roles(disconnected_client_id);

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

fn build_snapshot(state: &Arc<ServerState>) -> Result<SnapshotV3> {
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
                let active_window_id = runtime_manager
                    .runtimes
                    .get(&session_info.id)
                    .map(|runtime| runtime.active_window.0);
                let window_snapshots = runtime_manager
                    .runtimes
                    .get(&session_info.id)
                    .map(|runtime| runtime.windows.values())
                    .into_iter()
                    .flatten()
                    .map(|window| {
                        validate_runtime_layout_matches_panes(&window.layout_root, &window.panes)
                            .with_context(|| {
                            format!(
                                "cannot snapshot inconsistent layout for window {} in session {}",
                                window.id.0, session_info.id.0
                            )
                        })?;

                        let mut pane_ids = Vec::new();
                        window.layout_root.pane_order(&mut pane_ids);
                        let panes = pane_ids
                            .into_iter()
                            .map(|pane_id| {
                                window
                                    .panes
                                    .get(&pane_id)
                                    .map(|pane| PaneSnapshotV2 {
                                        id: pane.meta.id,
                                        name: pane.meta.name.clone(),
                                        shell: pane.meta.shell.clone(),
                                    })
                                    .ok_or_else(|| {
                                        anyhow::anyhow!(
                                            "layout references missing pane {} in window {}",
                                            pane_id,
                                            window.id.0
                                        )
                                    })
                            })
                            .collect::<Result<Vec<_>>>()?;

                        Ok(WindowSnapshotV3 {
                            id: window.id.0,
                            number: window.number,
                            name: window.name.clone(),
                            panes,
                            focused_pane_id: Some(window.focused_pane_id),
                            layout_root: Some(snapshot_layout_from_runtime(&window.layout_root)),
                            floating_surfaces: window
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
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;

                let name = manager
                    .get_session(&session_info.id)
                    .and_then(|session| session.name.clone());
                let next_window_number = runtime_manager
                    .runtimes
                    .get(&session_info.id)
                    .map_or(1, |runtime| runtime.next_window_number);

                Ok(SessionSnapshotV3 {
                    id: session_info.id.0,
                    name,
                    windows: window_snapshots,
                    active_window_id,
                    next_window_number,
                })
            })
            .collect::<Result<Vec<_>>>()?
    };

    let (owner_principals, roles) = {
        let permission_state = state
            .permission_state
            .lock()
            .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
        let owner_principals = permission_state
            .owner_principals
            .iter()
            .map(|(session_id, principal_id)| OwnerPrincipalSnapshotV2 {
                session_id: session_id.0,
                principal_id: *principal_id,
            })
            .collect::<Vec<_>>();
        let roles = permission_state
            .roles
            .iter()
            .flat_map(|(session_id, assignments)| {
                assignments
                    .iter()
                    .filter(|(_, role)| **role != SessionRole::Owner)
                    .map(|(client_id, role)| RoleAssignmentSnapshotV2 {
                        session_id: session_id.0,
                        client_id: client_id.0,
                        role: *role,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        (owner_principals, roles)
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

    Ok(SnapshotV3 {
        sessions: session_snapshots,
        owner_principals,
        roles,
        follows,
        selected_sessions,
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

fn apply_snapshot_state(state: &Arc<ServerState>, snapshot: &SnapshotV3) -> Result<RestoreSummary> {
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
            if session_snapshot.windows.is_empty() {
                warn!(
                    "skipping snapshot session {}: no windows to restore",
                    session_snapshot.id
                );
                continue;
            }

            let session_id = SessionId(session_snapshot.id);
            let mut session = Session::new(session_snapshot.name.clone());
            session.id = session_id;
            session.next_window_number = session_snapshot.next_window_number;
            for window_snapshot in &session_snapshot.windows {
                session.add_window(WindowId(window_snapshot.id), window_snapshot.number);
            }

            if let Some(active_window_id) = session_snapshot.active_window_id {
                let _ = session.set_active_window(WindowId(active_window_id));
            }

            if let Err(error) = session_manager.insert_session(session) {
                warn!(
                    "skipping snapshot session {} insertion failure: {error}",
                    session_snapshot.id
                );
                continue;
            }

            let active_window = session_snapshot
                .active_window_id
                .or_else(|| session_snapshot.windows.first().map(|window| window.id))
                .map(WindowId)
                .expect("windows non-empty implies active fallback");
            let runtime_windows = session_snapshot
                .windows
                .iter()
                .map(|window| {
                    let panes = window
                        .panes
                        .iter()
                        .map(|pane| PaneRuntimeMeta {
                            id: pane.id,
                            name: pane.name.clone(),
                            shell: pane.shell.clone(),
                        })
                        .collect::<Vec<_>>();
                    RestoreWindowRuntimeSpec {
                        id: WindowId(window.id),
                        number: window.number,
                        name: window.name.clone(),
                        panes,
                        layout_root: window
                            .layout_root
                            .as_ref()
                            .map(runtime_layout_from_snapshot),
                        focused_pane_id: window
                            .focused_pane_id
                            .or_else(|| window.panes.first().map(|pane| pane.id))
                            .expect("snapshot validation ensures pane exists"),
                        floating_surfaces: window
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
                            .collect(),
                    }
                })
                .collect::<Vec<_>>();

            if let Err(error) = runtime_manager.restore_runtime(
                session_id,
                runtime_windows,
                active_window,
                session_snapshot.next_window_number,
            ) {
                warn!(
                    "failed restoring runtime for session {}: {error}",
                    session_snapshot.id
                );
                let _ = session_manager.remove_session(&session_id);
                continue;
            }

            summary.sessions += 1;
            summary.windows += session_snapshot.windows.len();
        }
    }

    {
        let mut permission_state = state
            .permission_state
            .lock()
            .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
        permission_state.owner_principals.clear();
        permission_state.roles.clear();

        let session_manager = state
            .session_manager
            .lock()
            .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
        for owner in &snapshot.owner_principals {
            let session_id = SessionId(owner.session_id);
            if session_manager.get_session(&session_id).is_some() {
                permission_state.set_owner_principal(session_id, owner.principal_id);
                summary.roles += 1;
            }
        }

        for role in &snapshot.roles {
            let session_id = SessionId(role.session_id);
            if session_manager.get_session(&session_id).is_some() {
                if role.role == SessionRole::Owner {
                    continue;
                }
                permission_state.set_role(session_id, ClientId(role.client_id), role.role);
                summary.roles += 1;
            }
        }
    }

    {
        let mut follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        follow_state.follows.clear();
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
    snapshot: SnapshotV3,
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
        let mut permission_state = state
            .permission_state
            .lock()
            .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
        *permission_state = PermissionState::default();
    }
    {
        let mut follow_state = state
            .follow_state
            .lock()
            .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
        follow_state.follows.clear();
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
    let removed_window = {
        let mut runtime_manager = state
            .session_runtimes
            .lock()
            .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
        match runtime_manager.close_pane(session_id, Some(PaneSelector::Active)) {
            Ok((_, removed_window)) => removed_window,
            Err(_) => None,
        }
    };

    if let Some(removed_window) = removed_window {
        let removed_window_session_id = removed_window.session_id;
        let removed_window_id = removed_window.window_id;
        let removed_window_handle = removed_window.handle;
        let session_removed = removed_window.session_removed;
        shutdown_window_handle(removed_window_handle).await;
        {
            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            if let Some(session_model) = manager.get_session_mut(&removed_window_session_id) {
                session_model.remove_window(&removed_window_id);
            }
        }
        emit_event(
            state,
            Event::WindowRemoved {
                id: removed_window_id.0,
                session_id: removed_window_session_id.0,
            },
        )?;

        if let Some(removed_session) = session_removed {
            if removed_session.had_attached_clients {
                emit_event(
                    state,
                    Event::ClientDetached {
                        id: removed_window_session_id.0,
                    },
                )?;
            }
            shutdown_runtime_handle(removed_session).await;
            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let _ = manager.remove_session(&removed_window_session_id);
            if *selected_session == Some(removed_window_session_id) {
                *selected_session = None;
                persist_selected_session(state, client_id, None)?;
            }
            if *attached_stream_session == Some(removed_window_session_id) {
                *attached_stream_session = None;
            }
            drop(manager);

            let mut attach_tokens = state
                .attach_tokens
                .lock()
                .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
            attach_tokens.remove_for_session(removed_window_session_id);
            drop(attach_tokens);

            clear_selected_session_for_all(state, removed_window_session_id)?;

            let mut permission_state = state
                .permission_state
                .lock()
                .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
            permission_state.remove_session(removed_window_session_id);
            drop(permission_state);

            emit_event(
                state,
                Event::SessionRemoved {
                    id: removed_window_session_id.0,
                },
            )?;
        } else {
            emit_attach_view_changed_for_pane_close(state, removed_window_session_id, true, false)?;
        }
    } else {
        emit_attach_view_changed_for_pane_close(state, session_id, false, false)?;
    }

    Ok(())
}

async fn reap_exited_pane(
    state: &Arc<ServerState>,
    session_id: SessionId,
    pane_id: Uuid,
) -> Result<()> {
    let removed_window = {
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
            Ok((_, removed_window)) => removed_window,
            Err(_) => None,
        }
    };

    if let Some(removed_window) = removed_window {
        let removed_window_session_id = removed_window.session_id;
        let removed_window_id = removed_window.window_id;
        let removed_window_handle = removed_window.handle;
        let session_removed = removed_window.session_removed;
        shutdown_window_handle(removed_window_handle).await;

        {
            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            if let Some(session_model) = manager.get_session_mut(&removed_window_session_id) {
                session_model.remove_window(&removed_window_id);
            }
        }
        emit_event(
            state,
            Event::WindowRemoved {
                id: removed_window_id.0,
                session_id: removed_window_session_id.0,
            },
        )?;

        if let Some(removed_session) = session_removed {
            if removed_session.had_attached_clients {
                emit_event(
                    state,
                    Event::ClientDetached {
                        id: removed_window_session_id.0,
                    },
                )?;
            }
            shutdown_runtime_handle(removed_session).await;

            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let _ = manager.remove_session(&removed_window_session_id);
            drop(manager);

            let mut attach_tokens = state
                .attach_tokens
                .lock()
                .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
            attach_tokens.remove_for_session(removed_window_session_id);
            drop(attach_tokens);

            clear_selected_session_for_all(state, removed_window_session_id)?;

            let mut permission_state = state
                .permission_state
                .lock()
                .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
            permission_state.remove_session(removed_window_session_id);
            drop(permission_state);

            emit_event(
                state,
                Event::SessionRemoved {
                    id: removed_window_session_id.0,
                },
            )?;
        } else {
            emit_attach_view_changed_for_pane_close(state, removed_window_session_id, true, false)?;
        }
    } else {
        emit_attach_view_changed_for_pane_close(state, session_id, false, false)?;
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
    sync_selected_session_from_follow_state(state, client_id, selected_session)?;
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
            server_owner_principal_id: state.server_owner_principal_id,
            force_local_authorized: client_principal_id == state.server_owner_principal_id,
        }),
        Request::ServerStatus => {
            let snapshot = snapshot_status(state)?;
            Response::Ok(ResponsePayload::ServerStatus {
                running: true,
                snapshot,
                principal_id: client_principal_id,
                server_owner_principal_id: state.server_owner_principal_id,
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
                        "snapshot is valid (sessions={}, roles={}, follows={}, selected={})",
                        snapshot.sessions.len(),
                        snapshot.roles.len(),
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
                windows: summary.windows,
                roles: summary.roles,
                follows: summary.follows,
                selected_sessions: summary.selected_sessions,
            })
        }
        Request::ServerStop => {
            let _ = shutdown_tx.send(true);
            Response::Ok(ResponsePayload::ServerStopping)
        }
        Request::NewSession { name } => {
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
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::AlreadyExists,
                    message: format!("session already exists with name '{requested_name}'"),
                }));
            }

            match manager.create_session(name.clone()) {
                Ok(session_id) => {
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
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::Internal,
                            message: format!("failed creating session runtime: {error:#}"),
                        }));
                    }
                    let initial_windows = runtime_manager
                        .list_windows(session_id)
                        .map(|windows| {
                            windows
                                .into_iter()
                                .map(|window| (window.id, window.number))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    drop(runtime_manager);

                    let mut manager = state
                        .session_manager
                        .lock()
                        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                    if let Some(session_model) = manager.get_session_mut(&session_id) {
                        for (window_id, window_number) in initial_windows {
                            session_model.add_window(window_id, window_number);
                        }
                    }
                    drop(manager);

                    let mut permission_state = state
                        .permission_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
                    permission_state.ensure_owner(session_id, client_principal_id);

                    Response::Ok(ResponsePayload::SessionCreated {
                        id: session_id.0,
                        name,
                    })
                }
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed creating session: {error:#}"),
                }),
            }
        }
        Request::NewWindow { session, name } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let session_id =
                match resolve_window_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                };
            drop(manager);

            if let Err(response) =
                ensure_owner_for_session(state, session_id, client_id, client_principal_id)
            {
                return Ok(Response::Err(response));
            }

            let mut runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            let (window_id, window_number, resolved_name) =
                match runtime_manager.new_window(session_id, name) {
                    Ok(created) => created,
                    Err(error) => {
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::Internal,
                            message: format!("failed creating window runtime: {error:#}"),
                        }));
                    }
                };
            drop(runtime_manager);

            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            if let Some(session_model) = manager.get_session_mut(&session_id) {
                session_model.add_window(window_id, window_number);
            }

            Response::Ok(ResponsePayload::WindowCreated {
                id: window_id.0,
                session_id: session_id.0,
                number: window_number,
                name: resolved_name,
            })
        }
        Request::ListWindows { session } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let session_id =
                match resolve_window_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                };
            drop(manager);

            let runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            let windows = match runtime_manager.list_windows(session_id) {
                Ok(windows) => windows,
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: format!("failed listing windows: {error:#}"),
                    }));
                }
            };

            Response::Ok(ResponsePayload::WindowList {
                windows: windows
                    .into_iter()
                    .map(|window| WindowSummary {
                        id: window.id.0,
                        session_id: session_id.0,
                        number: window.number,
                        name: window.name,
                        active: window.active,
                    })
                    .collect(),
            })
        }
        Request::KillWindow {
            session,
            target,
            force_local,
        } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                match resolve_window_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                }
            };

            if force_local && client_principal_id != state.server_owner_principal_id {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "force-local is only allowed for the server owner principal"
                        .to_string(),
                }));
            }

            if !force_local
                && let Err(response) =
                    ensure_owner_for_session(state, session_id, client_id, client_principal_id)
            {
                return Ok(Response::Err(response));
            }

            let selection = window_selection_from_selector(target);
            let removed_window = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                match runtime_manager.kill_window(session_id, selection) {
                    Ok(removed) => removed,
                    Err(error) => {
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::NotFound,
                            message: format!("failed killing window: {error:#}"),
                        }));
                    }
                }
            };

            let removed_window_session_id = removed_window.session_id;
            let removed_window_id = removed_window.window_id;
            let removed_window_handle = removed_window.handle;
            let session_removed = removed_window.session_removed;

            shutdown_window_handle(removed_window_handle).await;

            {
                let mut manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                if let Some(session_model) = manager.get_session_mut(&removed_window_session_id) {
                    session_model.remove_window(&removed_window_id);
                }
            }

            emit_event(
                state,
                Event::WindowRemoved {
                    id: removed_window_id.0,
                    session_id: removed_window_session_id.0,
                },
            )?;
            if session_removed.is_none() {
                emit_attach_view_changed_for_window_switch(state, removed_window_session_id)?;
            }

            if let Some(removed_session) = session_removed {
                if removed_session.had_attached_clients {
                    emit_event(
                        state,
                        Event::ClientDetached {
                            id: removed_window_session_id.0,
                        },
                    )?;
                }
                shutdown_runtime_handle(removed_session).await;

                let mut manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                let _ = manager.remove_session(&removed_window_session_id);
                if *selected_session == Some(removed_window_session_id) {
                    *selected_session = None;
                    persist_selected_session(state, client_id, None)?;
                }
                if *attached_stream_session == Some(removed_window_session_id) {
                    *attached_stream_session = None;
                }

                let mut attach_tokens = state
                    .attach_tokens
                    .lock()
                    .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
                attach_tokens.remove_for_session(removed_window_session_id);
                drop(attach_tokens);

                clear_selected_session_for_all(state, removed_window_session_id)?;

                let mut permission_state = state
                    .permission_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
                permission_state.remove_session(removed_window_session_id);
                drop(permission_state);

                emit_event(
                    state,
                    Event::SessionRemoved {
                        id: removed_window_session_id.0,
                    },
                )?;
            }

            Response::Ok(ResponsePayload::WindowKilled {
                id: removed_window_id.0,
                session_id: removed_window_session_id.0,
            })
        }
        Request::SwitchWindow { session, target } => {
            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let session_id =
                match resolve_window_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                };
            if let Err(response) =
                ensure_owner_for_session(state, session_id, client_id, client_principal_id)
            {
                return Ok(Response::Err(response));
            }
            let selection = window_selection_from_selector(target);

            let mut runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            let switched_id = match runtime_manager.switch_window(session_id, selection) {
                Ok(window_id) => window_id,
                Err(error) => {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: format!("failed switching window: {error:#}"),
                    }));
                }
            };
            let switched_number = runtime_manager
                .runtimes
                .get(&session_id)
                .and_then(|runtime| runtime.windows.get(&switched_id))
                .map(|window| window.number)
                .ok_or_else(|| anyhow::anyhow!("switched window missing from runtime"))?;
            drop(runtime_manager);

            if let Some(session_model) = manager.get_session_mut(&session_id) {
                let _ = session_model.set_active_window(switched_id);
            }

            emit_event(
                state,
                Event::WindowSwitched {
                    id: switched_id.0,
                    session_id: session_id.0,
                    by_client_id: client_id.0,
                },
            )?;
            emit_attach_view_changed_for_window_switch(state, session_id)?;

            Response::Ok(ResponsePayload::WindowSwitched {
                id: switched_id.0,
                session_id: session_id.0,
                number: switched_number,
            })
        }
        Request::ListPanes { session } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let session_id =
                match resolve_window_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                };
            drop(manager);

            let runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            let (_window_id, panes) = match runtime_manager.list_panes(session_id) {
                Ok(result) => result,
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
        } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let session_id =
                match resolve_window_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                };
            drop(manager);
            if let Err(response) =
                ensure_writer_for_session(state, session_id, client_id, client_principal_id)
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
            let window_id = runtime_manager
                .runtimes
                .get(&session_id)
                .map_or(WindowId(Uuid::nil()), |runtime| runtime.active_window);
            drop(runtime_manager);
            emit_attach_view_changed_for_layout(state, session_id)?;
            Response::Ok(ResponsePayload::PaneSplit {
                id: pane_id,
                session_id: session_id.0,
                window_id: window_id.0,
            })
        }
        Request::FocusPane {
            session,
            target,
            direction,
        } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let session_id =
                match resolve_window_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                };
            drop(manager);
            if let Err(response) =
                ensure_writer_for_session(state, session_id, client_id, client_principal_id)
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
            let window_id = runtime_manager
                .runtimes
                .get(&session_id)
                .map_or(WindowId(Uuid::nil()), |runtime| runtime.active_window);
            drop(runtime_manager);
            emit_attach_view_changed_for_layout(state, session_id)?;
            Response::Ok(ResponsePayload::PaneFocused {
                id: pane_id,
                session_id: session_id.0,
                window_id: window_id.0,
            })
        }
        Request::ResizePane {
            session,
            target,
            delta,
        } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let session_id =
                match resolve_window_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                };
            drop(manager);
            if let Err(response) =
                ensure_writer_for_session(state, session_id, client_id, client_principal_id)
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
            let window_id = runtime_manager
                .runtimes
                .get(&session_id)
                .map_or(WindowId(Uuid::nil()), |runtime| runtime.active_window);
            drop(runtime_manager);
            emit_attach_view_changed_for_layout(state, session_id)?;
            Response::Ok(ResponsePayload::PaneResized {
                session_id: session_id.0,
                window_id: window_id.0,
            })
        }
        Request::ClosePane { session, target } => {
            let session_id = {
                let manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                match resolve_window_request_session_id(&manager, &session, selected_session) {
                    Ok(session_id) => session_id,
                    Err(response) => return Ok(Response::Err(response)),
                }
            };
            if let Err(response) =
                ensure_writer_for_session(state, session_id, client_id, client_principal_id)
            {
                return Ok(Response::Err(response));
            }

            let (closed_pane_id, active_window_id, removed_window) = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                let active_window_id = runtime_manager
                    .runtimes
                    .get(&session_id)
                    .map(|runtime| runtime.active_window)
                    .ok_or_else(|| anyhow::anyhow!("runtime not found"))?;
                let (closed_pane_id, removed_window) = runtime_manager
                    .close_pane(session_id, target)
                    .map_err(|error| anyhow::anyhow!("failed closing pane: {error:#}"))?;
                (closed_pane_id, active_window_id, removed_window)
            };

            let mut window_closed = false;
            let mut session_closed = false;
            if let Some(removed_window) = removed_window {
                window_closed = true;
                let removed_window_session_id = removed_window.session_id;
                let removed_window_id = removed_window.window_id;
                let removed_window_handle = removed_window.handle;
                let session_removed = removed_window.session_removed;
                shutdown_window_handle(removed_window_handle).await;
                {
                    let mut manager = state
                        .session_manager
                        .lock()
                        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                    if let Some(session_model) = manager.get_session_mut(&removed_window_session_id)
                    {
                        session_model.remove_window(&removed_window_id);
                    }
                }
                emit_event(
                    state,
                    Event::WindowRemoved {
                        id: removed_window_id.0,
                        session_id: removed_window_session_id.0,
                    },
                )?;
                if let Some(removed_session) = session_removed {
                    session_closed = true;
                    if removed_session.had_attached_clients {
                        emit_event(
                            state,
                            Event::ClientDetached {
                                id: removed_window_session_id.0,
                            },
                        )?;
                    }
                    shutdown_runtime_handle(removed_session).await;
                    let mut manager = state
                        .session_manager
                        .lock()
                        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                    let _ = manager.remove_session(&removed_window_session_id);
                    if *selected_session == Some(removed_window_session_id) {
                        *selected_session = None;
                        persist_selected_session(state, client_id, None)?;
                    }
                    if *attached_stream_session == Some(removed_window_session_id) {
                        *attached_stream_session = None;
                    }
                    drop(manager);

                    let mut attach_tokens = state
                        .attach_tokens
                        .lock()
                        .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
                    attach_tokens.remove_for_session(removed_window_session_id);
                    drop(attach_tokens);

                    clear_selected_session_for_all(state, removed_window_session_id)?;

                    let mut permission_state = state
                        .permission_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
                    permission_state.remove_session(removed_window_session_id);
                    drop(permission_state);

                    emit_event(
                        state,
                        Event::SessionRemoved {
                            id: removed_window_session_id.0,
                        },
                    )?;
                }
            }

            emit_attach_view_changed_for_pane_close(
                state,
                session_id,
                window_closed,
                session_closed,
            )?;

            Response::Ok(ResponsePayload::PaneClosed {
                id: closed_pane_id,
                session_id: session_id.0,
                window_id: active_window_id.0,
                window_closed,
                session_closed,
            })
        }
        Request::ListSessions => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            let sessions = manager
                .list_sessions()
                .into_iter()
                .map(|session| SessionSummary {
                    id: session.id.0,
                    name: session.name,
                    window_count: runtime_manager
                        .list_windows(session.id)
                        .map(|windows| windows.len())
                        .unwrap_or(session.window_count),
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
            let client_principals = state
                .client_principals
                .lock()
                .map_err(|_| anyhow::anyhow!("client principal map lock poisoned"))?;
            let permission_state = state
                .permission_state
                .lock()
                .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
            let clients = follow_state
                .list_clients()
                .into_iter()
                .map(|mut client| {
                    client.session_role = client.selected_session_id.map(|session_id| {
                        let principal_id = client_principals
                            .get(&ClientId(client.id))
                            .copied()
                            .unwrap_or(Uuid::nil());
                        permission_state.role_for(
                            SessionId(session_id),
                            ClientId(client.id),
                            principal_id,
                        )
                    });
                    client
                })
                .collect::<Vec<_>>();
            Response::Ok(ResponsePayload::ClientList { clients })
        }
        Request::ListPermissions { session } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let Some(session_id) = resolve_session_id(&manager, &session) else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: session_not_found_message(&session),
                }));
            };
            drop(manager);

            let permission_state = state
                .permission_state
                .lock()
                .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
            let follow_state = state
                .follow_state
                .lock()
                .map_err(|_| anyhow::anyhow!("follow state lock poisoned"))?;
            let client_principals = state
                .client_principals
                .lock()
                .map_err(|_| anyhow::anyhow!("client principal map lock poisoned"))?;
            let permissions = permission_state.list_permissions(
                session_id,
                &follow_state.connected_clients,
                &client_principals,
            );
            Response::Ok(ResponsePayload::PermissionsList {
                session_id: session_id.0,
                permissions,
            })
        }
        Request::GrantRole {
            session,
            client_id: target_client_id,
            role,
        } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let Some(session_id) = resolve_session_id(&manager, &session) else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: session_not_found_message(&session),
                }));
            };
            drop(manager);

            if let Err(response) =
                ensure_owner_for_session(state, session_id, client_id, client_principal_id)
            {
                return Ok(Response::Err(response));
            }

            let target_client_id = ClientId(target_client_id);
            let target_principal_id = {
                let principals = state
                    .client_principals
                    .lock()
                    .map_err(|_| anyhow::anyhow!("client principal map lock poisoned"))?;
                match principals.get(&target_client_id).copied() {
                    Some(id) => id,
                    None => {
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::NotFound,
                            message: format!("target client not connected: {}", target_client_id.0),
                        }));
                    }
                }
            };

            let mut permission_state = state
                .permission_state
                .lock()
                .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
            if role == SessionRole::Owner {
                permission_state.set_owner_principal(session_id, target_principal_id);
            } else {
                permission_state.set_role(session_id, target_client_id, role);
            }
            if role == SessionRole::Owner && target_client_id != client_id {
                permission_state.clear_to_observer(session_id, client_id);
            }
            drop(permission_state);

            emit_event(
                state,
                Event::RoleChanged {
                    session_id: session_id.0,
                    client_id: target_client_id.0,
                    role,
                    by_client_id: client_id.0,
                },
            )?;
            emit_attach_view_changed_for_status(state, session_id)?;
            if role == SessionRole::Owner && target_client_id != client_id {
                emit_event(
                    state,
                    Event::RoleChanged {
                        session_id: session_id.0,
                        client_id: client_id.0,
                        role: SessionRole::Observer,
                        by_client_id: client_id.0,
                    },
                )?;
                emit_attach_view_changed_for_status(state, session_id)?;
            }

            Response::Ok(ResponsePayload::RoleGranted {
                session_id: session_id.0,
                client_id: target_client_id.0,
                role,
            })
        }
        Request::RevokeRole {
            session,
            client_id: target_client_id,
        } => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let Some(session_id) = resolve_session_id(&manager, &session) else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: session_not_found_message(&session),
                }));
            };
            drop(manager);

            if let Err(response) =
                ensure_owner_for_session(state, session_id, client_id, client_principal_id)
            {
                return Ok(Response::Err(response));
            }

            let target_client_id = ClientId(target_client_id);
            let target_principal_id = {
                let principals = state
                    .client_principals
                    .lock()
                    .map_err(|_| anyhow::anyhow!("client principal map lock poisoned"))?;
                principals.get(&target_client_id).copied()
            };
            let mut permission_state = state
                .permission_state
                .lock()
                .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
            if target_principal_id.is_some_and(|principal_id| {
                permission_state.role_for(session_id, target_client_id, principal_id)
                    == SessionRole::Owner
            }) {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "cannot revoke the current owner role".to_string(),
                }));
            }
            permission_state.clear_to_observer(session_id, target_client_id);
            drop(permission_state);

            emit_event(
                state,
                Event::RoleChanged {
                    session_id: session_id.0,
                    client_id: target_client_id.0,
                    role: SessionRole::Observer,
                    by_client_id: client_id.0,
                },
            )?;
            emit_attach_view_changed_for_status(state, session_id)?;

            Response::Ok(ResponsePayload::RoleRevoked {
                session_id: session_id.0,
                client_id: target_client_id.0,
                role: SessionRole::Observer,
            })
        }
        Request::KillSession {
            selector,
            force_local,
        } => {
            let (session_id, removed_runtime) = {
                let mut manager = state
                    .session_manager
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
                let Some(session_id) = resolve_session_id(&manager, &selector) else {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::NotFound,
                        message: session_not_found_message(&selector),
                    }));
                };

                if force_local && client_principal_id != state.server_owner_principal_id {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::InvalidRequest,
                        message: "force-local is only allowed for the server owner principal"
                            .to_string(),
                    }));
                }

                if !force_local
                    && let Err(response) =
                        ensure_owner_for_session(state, session_id, client_id, client_principal_id)
                {
                    return Ok(Response::Err(response));
                }

                if manager.remove_session(&session_id).is_err() {
                    return Ok(Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: format!("failed removing session {}", session_id.0),
                    }));
                }
                if *selected_session == Some(session_id) {
                    *selected_session = None;
                    persist_selected_session(state, client_id, None)?;
                }
                if *attached_stream_session == Some(session_id) {
                    *attached_stream_session = None;
                }
                drop(manager);

                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                let removed_runtime = match runtime_manager.remove_runtime(session_id) {
                    Ok(removed) => removed,
                    Err(error) => {
                        return Ok(Response::Err(ErrorResponse {
                            code: ErrorCode::Internal,
                            message: format!("failed stopping session runtime: {error:#}"),
                        }));
                    }
                };
                (session_id, removed_runtime)
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

            let mut permission_state = state
                .permission_state
                .lock()
                .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
            permission_state.remove_session(session_id);
            drop(permission_state);

            emit_event(state, Event::SessionRemoved { id: session_id.0 })?;

            Response::Ok(ResponsePayload::SessionKilled { id: session_id.0 })
        }
        Request::FollowClient {
            target_client_id,
            global,
        } => {
            let leader_client_id = ClientId(target_client_id);
            let initial_target_session = {
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

            match manager.get_session_mut(&next_session_id) {
                Some(session) => {
                    session.add_client(client_id);
                    *selected_session = Some(next_session_id);
                    persist_selected_session(state, client_id, *selected_session)?;
                    drop(manager);

                    let mut attach_tokens = state
                        .attach_tokens
                        .lock()
                        .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
                    let grant = attach_tokens.issue(next_session_id);
                    Response::Ok(ResponsePayload::Attached { grant })
                }
                None => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session not found: {}", next_session_id.0),
                }),
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
                    let can_write = {
                        let permission_state = state
                            .permission_state
                            .lock()
                            .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
                        matches!(
                            permission_state.role_for(session_id, client_id, client_principal_id),
                            SessionRole::Owner | SessionRole::Writer
                        )
                    };

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
                            Response::Ok(ResponsePayload::AttachReady {
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
            let role = {
                let permission_state = state
                    .permission_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("permission state lock poisoned"))?;
                permission_state.role_for(session_id, client_id, client_principal_id)
            };
            if role == SessionRole::Observer {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "attach input denied: observer role is read-only".to_string(),
                }));
            }

            let write_result = {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.write_input(session_id, client_id, data)
            };
            match write_result {
                Ok(bytes) => Response::Ok(ResponsePayload::AttachInputAccepted { bytes }),
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
                runtime_manager.set_attach_viewport(session_id, client_id, cols, rows)
            };

            match update_result {
                Ok((cols, rows)) => Response::Ok(ResponsePayload::AttachViewportSet {
                    session_id: session_id.0,
                    cols,
                    rows,
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
                    session_id: session_id.0,
                    window_id: snapshot.window_id.0,
                    focused_pane_id: snapshot.focused_pane_id,
                    panes: snapshot.panes,
                    layout_root: snapshot.layout_root,
                    scene: snapshot.scene,
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
                    session_id: snapshot.session_id.0,
                    window_id: snapshot.window_id.0,
                    focused_pane_id: snapshot.focused_pane_id,
                    panes: snapshot.panes,
                    layout_root: snapshot.layout_root,
                    scene: snapshot.scene,
                    chunks: snapshot.chunks,
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
    if let Response::Ok(ResponsePayload::WindowCreated {
        id,
        session_id,
        name,
        ..
    }) = &response
    {
        emit_event(
            state,
            Event::WindowCreated {
                id: *id,
                session_id: *session_id,
                name: name.clone(),
            },
        )?;
        emit_attach_view_changed_for_window_tabs(state, SessionId(*session_id))?;
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
            | Request::NewWindow { .. }
            | Request::GrantRole { .. }
            | Request::RevokeRole { .. }
            | Request::KillSession { .. }
            | Request::KillWindow { .. }
            | Request::SwitchWindow { .. }
            | Request::SplitPane { .. }
            | Request::FocusPane { .. }
            | Request::ResizePane { .. }
            | Request::ClosePane { .. }
            | Request::FollowClient { .. }
            | Request::Unfollow
            | Request::Attach { .. }
            | Request::AttachOpen { .. }
            | Request::AttachInput { .. }
            | Request::AttachSetViewport { .. }
            | Request::Detach
    )
}

const fn response_requires_snapshot(response: &Response) -> bool {
    matches!(
        response,
        Response::Ok(
            ResponsePayload::SessionCreated { .. }
                | ResponsePayload::WindowCreated { .. }
                | ResponsePayload::WindowKilled { .. }
                | ResponsePayload::WindowSwitched { .. }
                | ResponsePayload::PaneSplit { .. }
                | ResponsePayload::PaneFocused { .. }
                | ResponsePayload::PaneResized { .. }
                | ResponsePayload::PaneClosed { .. }
                | ResponsePayload::SessionKilled { .. }
                | ResponsePayload::RoleGranted { .. }
                | ResponsePayload::RoleRevoked { .. }
                | ResponsePayload::FollowStarted { .. }
                | ResponsePayload::FollowStopped { .. }
                | ResponsePayload::Attached { .. }
                | ResponsePayload::Detached
        )
    )
}

fn detach_client_state_on_disconnect(
    state: &Arc<ServerState>,
    client_id: ClientId,
    selected_session: &mut Option<SessionId>,
    attached_stream_session: &mut Option<SessionId>,
) -> Result<()> {
    let previous_selected = selected_session.take();
    let previous_stream = attached_stream_session.take();

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

fn session_not_found_message(selector: &SessionSelector) -> String {
    format!(
        "session not found for selector {selector:?} (lookup order: exact name -> exact UUID -> UUID prefix)"
    )
}

fn resolve_window_request_session_id(
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

fn ensure_owner_for_session(
    state: &Arc<ServerState>,
    session_id: SessionId,
    _client_id: ClientId,
    client_principal_id: Uuid,
) -> std::result::Result<(), ErrorResponse> {
    let mut permission_state = state.permission_state.lock().map_err(|_| ErrorResponse {
        code: ErrorCode::Internal,
        message: "permission state lock poisoned".to_string(),
    })?;

    match permission_state.owner_principal_for(session_id) {
        Some(owner_principal_id) if owner_principal_id == client_principal_id => Ok(()),
        Some(_) => Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: "owner role required for this operation".to_string(),
        }),
        None => {
            permission_state.set_owner_principal(session_id, client_principal_id);
            Ok(())
        }
    }
}

fn ensure_writer_for_session(
    state: &Arc<ServerState>,
    session_id: SessionId,
    client_id: ClientId,
    client_principal_id: Uuid,
) -> std::result::Result<(), ErrorResponse> {
    let permission_state = state.permission_state.lock().map_err(|_| ErrorResponse {
        code: ErrorCode::Internal,
        message: "permission state lock poisoned".to_string(),
    })?;

    let role = permission_state.role_for(session_id, client_id, client_principal_id);
    if role == SessionRole::Owner || role == SessionRole::Writer {
        Ok(())
    } else {
        Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: "writer or owner role required for this operation".to_string(),
        })
    }
}

fn window_selection_from_selector(selector: WindowSelector) -> WindowSelection {
    match selector {
        WindowSelector::ById(id) => WindowSelection::Id(WindowId(id)),
        WindowSelector::ByNumber(number) => WindowSelection::Number(number),
        WindowSelector::ByName(name) => WindowSelection::Name(name),
        WindowSelector::Active => WindowSelection::Active,
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

#[cfg(test)]
mod tests {
    use super::{BmuxServer, reap_exited_pane, resolve_session_id};
    use bmux_config::ConfigPaths;
    use bmux_ipc::transport::LocalIpcStream;
    use bmux_ipc::{
        AttachViewComponent, Envelope, EnvelopeKind, ErrorCode, ErrorResponse, Event, IpcEndpoint,
        PaneSelector, PaneSplitDirection, ProtocolVersion, Request, Response, ResponsePayload,
        SessionRole, SessionSelector, WindowSelector, decode, encode,
    };
    use bmux_session::{SessionId, SessionManager};
    use std::path::Path;
    use std::time::Duration;
    use tokio::sync::oneshot;
    use tokio::time::sleep;
    use uuid::Uuid;

    const TEST_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

    #[cfg(unix)]
    async fn spawn_server_with_ready(
        server: BmuxServer,
    ) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        let (ready_tx, ready_rx) = oneshot::channel();
        let server_clone = server.clone();
        let server_task = tokio::spawn(async move { server_clone.run_with_ready(ready_tx).await });
        match tokio::time::timeout(TEST_STARTUP_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => panic!("server failed to start: {error}"),
            Ok(Err(_)) => panic!("server ready channel dropped before startup"),
            Err(_) => panic!("timed out waiting for server startup"),
        }
        server_task
    }

    #[test]
    fn resolve_session_id_prefers_exact_name_before_uuid_fallbacks() {
        let mut manager = SessionManager::new();
        let prefix_source = manager
            .create_session(None)
            .expect("session should be created");
        let selector_value = prefix_source.to_string()[..2].to_string();
        let named = manager
            .create_session(Some(selector_value.clone()))
            .expect("named session should be created");

        let resolved = resolve_session_id(&manager, &SessionSelector::ByName(selector_value));
        assert_eq!(resolved, Some(named));
    }

    #[test]
    fn resolve_session_id_matches_exact_uuid_string() {
        let mut manager = SessionManager::new();
        let session_id = manager
            .create_session(None)
            .expect("session should be created");

        let resolved =
            resolve_session_id(&manager, &SessionSelector::ByName(session_id.to_string()));
        assert_eq!(resolved, Some(session_id));
    }

    #[test]
    fn resolve_session_id_allows_short_unique_prefixes_without_minimum() {
        let mut manager = SessionManager::new();
        let session_id = manager
            .create_session(None)
            .expect("session should be created");

        let prefix = session_id.to_string()[..1].to_string();
        let resolved = resolve_session_id(&manager, &SessionSelector::ByName(prefix));
        assert_eq!(resolved, Some(session_id));
    }

    #[test]
    fn resolve_session_id_picks_first_match_for_ambiguous_prefix() {
        let mut manager = SessionManager::new();
        let mut selector = None;

        for _ in 0..512 {
            let _ = manager
                .create_session(None)
                .expect("session should be created");

            let sessions = manager.list_sessions();
            for nibble in "0123456789abcdef".chars() {
                let matches = sessions
                    .iter()
                    .filter(|session| session.id.to_string().starts_with(nibble))
                    .collect::<Vec<_>>();
                if matches.len() >= 2 {
                    selector = Some(nibble.to_string());
                    break;
                }
            }

            if selector.is_some() {
                break;
            }
        }

        let selector = selector.expect("expected to find an ambiguous prefix");
        let expected_first = manager
            .list_sessions()
            .into_iter()
            .find(|session| session.id.to_string().starts_with(&selector))
            .map(|session| session.id)
            .expect("expected at least one matching session");

        let resolved = resolve_session_id(&manager, &SessionSelector::ByName(selector));
        assert_eq!(resolved, Some(expected_first));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handshake_accepts_current_protocol_version() {
        let socket_path = std::env::temp_dir().join(format!("bmux-server-{}.sock", Uuid::new_v4()));
        let endpoint = IpcEndpoint::unix_socket(&socket_path);
        let server = BmuxServer::new(endpoint.clone());

        let server_task = spawn_server_with_ready(server.clone()).await;

        let mut client = LocalIpcStream::connect(&endpoint)
            .await
            .expect("client should connect");
        let hello_payload = encode(&Request::Hello {
            protocol_version: ProtocolVersion::current(),
            client_name: "test-client".to_string(),
            principal_id: Uuid::new_v4(),
        })
        .expect("hello should encode");
        let hello = Envelope::new(1, EnvelopeKind::Request, hello_payload);
        client
            .send_envelope(&hello)
            .await
            .expect("hello send should succeed");

        let reply = client
            .recv_envelope()
            .await
            .expect("hello reply should be received");
        let response: Response = decode(&reply.payload).expect("response should decode");
        assert_eq!(reply.request_id, 1);
        assert!(matches!(
            response,
            Response::Ok(ResponsePayload::ServerStatus { running: true, .. })
        ));

        server.request_shutdown();
        server_task
            .await
            .expect("server task should join")
            .expect("server should shut down cleanly");
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handshake_rejects_version_mismatch() {
        let socket_path = std::env::temp_dir().join(format!("bmux-server-{}.sock", Uuid::new_v4()));
        let endpoint = IpcEndpoint::unix_socket(&socket_path);
        let server = BmuxServer::new(endpoint.clone());

        let server_task = spawn_server_with_ready(server.clone()).await;

        let mut client = LocalIpcStream::connect(&endpoint)
            .await
            .expect("client should connect");
        let hello_payload = encode(&Request::Hello {
            protocol_version: ProtocolVersion(99),
            client_name: "test-client".to_string(),
            principal_id: Uuid::new_v4(),
        })
        .expect("hello should encode");
        let hello = Envelope::new(77, EnvelopeKind::Request, hello_payload);
        client
            .send_envelope(&hello)
            .await
            .expect("hello send should succeed");

        let reply = client
            .recv_envelope()
            .await
            .expect("hello reply should be received");
        let response: Response = decode(&reply.payload).expect("response should decode");
        assert_eq!(reply.request_id, 77);
        assert!(matches!(
            response,
            Response::Err(ErrorResponse {
                code: ErrorCode::VersionMismatch,
                ..
            })
        ));

        server.request_shutdown();
        server_task
            .await
            .expect("server task should join")
            .expect("server should shut down cleanly");
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supports_new_session_and_list_sessions() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            10,
            Request::NewSession {
                name: Some("dev".to_string()),
            },
        )
        .await;
        let created_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, name }) => {
                assert_eq!(name.as_deref(), Some("dev"));
                id
            }
            other => panic!("unexpected new-session response: {other:?}"),
        };

        {
            let runtime_manager = server
                .state
                .session_runtimes
                .lock()
                .expect("runtime manager lock should succeed");
            assert_eq!(runtime_manager.runtime_count(), 1);
            assert!(runtime_manager.has_runtime(SessionId(created_id)));
        }

        let listed = send_request(&mut client, 11, Request::ListSessions).await;
        match listed {
            Response::Ok(ResponsePayload::SessionList { sessions }) => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].id, created_id);
                assert_eq!(sessions[0].name.as_deref(), Some("dev"));
            }
            other => panic!("unexpected list response: {other:?}"),
        }

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_clients_reports_connected_clients() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client_a = connect_and_handshake(&endpoint).await;
        let mut client_b = connect_and_handshake(&endpoint).await;

        let listed = send_request(&mut client_a, 110, Request::ListClients).await;
        let clients = match listed {
            Response::Ok(ResponsePayload::ClientList { clients }) => clients,
            other => panic!("unexpected list clients response: {other:?}"),
        };
        assert_eq!(clients.len(), 2);
        assert!(
            clients
                .iter()
                .all(|client| client.following_client_id.is_none())
        );
        assert!(clients.iter().all(|client| !client.following_global));

        let listed_from_b = send_request(&mut client_b, 111, Request::ListClients).await;
        let clients_from_b = match listed_from_b {
            Response::Ok(ResponsePayload::ClientList { clients }) => clients,
            other => panic!("unexpected list clients response: {other:?}"),
        };
        assert_eq!(clients_from_b.len(), 2);

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_clients_reports_follow_relationships() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut leader = connect_and_handshake(&endpoint).await;
        let mut follower = connect_and_handshake(&endpoint).await;

        let session_id = match send_request(
            &mut leader,
            120,
            Request::NewSession {
                name: Some("clients-follow".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create session response: {other:?}"),
        };
        let _ = send_request(
            &mut leader,
            121,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await;

        let leader_client_id = match send_request(&mut leader, 122, Request::ListClients).await {
            Response::Ok(ResponsePayload::ClientList { clients }) => clients
                .into_iter()
                .find(|client| client.selected_session_id == Some(session_id))
                .map(|client| client.id)
                .expect("leader client should be listed"),
            other => panic!("unexpected list clients response: {other:?}"),
        };

        let followed = send_request(
            &mut follower,
            123,
            Request::FollowClient {
                target_client_id: leader_client_id,
                global: true,
            },
        )
        .await;
        assert!(matches!(
            followed,
            Response::Ok(ResponsePayload::FollowStarted { global: true, .. })
        ));

        let listed = send_request(&mut follower, 124, Request::ListClients).await;
        let clients = match listed {
            Response::Ok(ResponsePayload::ClientList { clients }) => clients,
            other => panic!("unexpected list clients response: {other:?}"),
        };
        let follower_entry = clients
            .iter()
            .find(|client| client.id != leader_client_id)
            .expect("follower client should be listed");
        assert_eq!(follower_entry.following_client_id, Some(leader_client_id));
        assert!(follower_entry.following_global);
        assert_eq!(follower_entry.selected_session_id, Some(session_id));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_owner_cannot_mutate_session_or_windows() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut owner = connect_and_handshake(&endpoint).await;
        let mut observer =
            connect_and_handshake_with_principal(&endpoint, server.state.server_owner_principal_id)
                .await;

        let session_id = match send_request(
            &mut owner,
            130,
            Request::NewSession {
                name: Some("owner-only".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create session response: {other:?}"),
        };

        let switch_attempt = send_request(
            &mut observer,
            131,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::Active,
            },
        )
        .await;
        assert!(matches!(
            switch_attempt,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        let kill_attempt = send_request(
            &mut observer,
            132,
            Request::KillSession {
                selector: SessionSelector::ById(session_id),
                force_local: false,
            },
        )
        .await;
        assert!(matches!(
            kill_attempt,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        let forced_kill_attempt = send_request(
            &mut observer,
            133,
            Request::KillSession {
                selector: SessionSelector::ById(session_id),
                force_local: true,
            },
        )
        .await;
        assert_eq!(
            forced_kill_attempt,
            Response::Ok(ResponsePayload::SessionKilled { id: session_id })
        );

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grant_writer_role_allows_attach_input() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut owner = connect_and_handshake(&endpoint).await;
        let mut writer = connect_and_handshake(&endpoint).await;

        let session_id = match send_request(
            &mut owner,
            140,
            Request::NewSession {
                name: Some("writer-role".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create session response: {other:?}"),
        };

        let writer_client_id = match send_request(&mut writer, 141, Request::WhoAmI).await {
            Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
            other => panic!("unexpected whoami response: {other:?}"),
        };

        let granted = send_request(
            &mut owner,
            142,
            Request::GrantRole {
                session: SessionSelector::ById(session_id),
                client_id: writer_client_id,
                role: SessionRole::Writer,
            },
        )
        .await;
        assert!(matches!(
            granted,
            Response::Ok(ResponsePayload::RoleGranted {
                role: SessionRole::Writer,
                ..
            })
        ));

        let writer_grant = match send_request(
            &mut writer,
            143,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response: {other:?}"),
        };
        let writer_open = send_request(
            &mut writer,
            144,
            Request::AttachOpen {
                session_id,
                attach_token: writer_grant.attach_token,
            },
        )
        .await;
        assert!(matches!(
            writer_open,
            Response::Ok(ResponsePayload::AttachReady {
                session_id: opened_session,
                can_write: true,
            }) if opened_session == session_id
        ));

        let writer_input = send_request(
            &mut writer,
            145,
            Request::AttachInput {
                session_id,
                data: b"printf 'writer-role-ok\\n'\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            writer_input,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));

        let listed_permissions = send_request(
            &mut owner,
            146,
            Request::ListPermissions {
                session: SessionSelector::ById(session_id),
            },
        )
        .await;
        match listed_permissions {
            Response::Ok(ResponsePayload::PermissionsList { permissions, .. }) => {
                assert!(permissions.iter().any(|entry| {
                    entry.client_id == writer_client_id && entry.role == SessionRole::Writer
                }));
            }
            other => panic!("unexpected permissions list response: {other:?}"),
        }

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owner_transfer_allows_new_owner_mutations() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut owner = connect_and_handshake(&endpoint).await;
        let mut successor = connect_and_handshake(&endpoint).await;

        let session_id = match send_request(
            &mut owner,
            170,
            Request::NewSession {
                name: Some("owner-transfer".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let successor_id = match send_request(&mut successor, 171, Request::WhoAmI).await {
            Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
            other => panic!("unexpected whoami response: {other:?}"),
        };

        let transferred = send_request(
            &mut owner,
            172,
            Request::GrantRole {
                session: SessionSelector::ById(session_id),
                client_id: successor_id,
                role: SessionRole::Owner,
            },
        )
        .await;
        assert!(matches!(
            transferred,
            Response::Ok(ResponsePayload::RoleGranted {
                role: SessionRole::Owner,
                ..
            })
        ));

        let old_owner_mutation = send_request(
            &mut owner,
            173,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("should-fail".to_string()),
            },
        )
        .await;
        assert!(matches!(
            old_owner_mutation,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        let new_owner_mutation = send_request(
            &mut successor,
            174,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("allowed".to_string()),
            },
        )
        .await;
        assert!(matches!(
            new_owner_mutation,
            Response::Ok(ResponsePayload::WindowCreated {
                session_id: created_session,
                ..
            }) if created_session == session_id
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owner_disconnect_keeps_principal_owner_and_writer_cannot_mutate() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut owner = connect_and_handshake(&endpoint).await;
        let mut writer = connect_and_handshake(&endpoint).await;

        let session_id = match send_request(
            &mut owner,
            180,
            Request::NewSession {
                name: Some("owner-disconnect".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let writer_id = match send_request(&mut writer, 181, Request::WhoAmI).await {
            Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
            other => panic!("unexpected whoami response: {other:?}"),
        };

        let granted = send_request(
            &mut owner,
            182,
            Request::GrantRole {
                session: SessionSelector::ById(session_id),
                client_id: writer_id,
                role: SessionRole::Writer,
            },
        )
        .await;
        assert!(matches!(
            granted,
            Response::Ok(ResponsePayload::RoleGranted {
                role: SessionRole::Writer,
                ..
            })
        ));

        drop(owner);
        sleep(Duration::from_millis(50)).await;

        let listed = send_request(
            &mut writer,
            183,
            Request::ListPermissions {
                session: SessionSelector::ById(session_id),
            },
        )
        .await;
        match listed {
            Response::Ok(ResponsePayload::PermissionsList { permissions, .. }) => {
                assert!(permissions.iter().any(|entry| {
                    entry.client_id == writer_id && entry.role == SessionRole::Writer
                }));
            }
            other => panic!("unexpected permissions list response: {other:?}"),
        }

        let mutation = send_request(
            &mut writer,
            184,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("post-promotion".to_string()),
            },
        )
        .await;
        assert!(matches!(
            mutation,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn same_principal_reconnect_retains_owner_permissions() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let principal_id = Uuid::new_v4();

        let mut owner_a = connect_and_handshake_with_principal(&endpoint, principal_id).await;
        let session_id = match send_request(
            &mut owner_a,
            185,
            Request::NewSession {
                name: Some("principal-reconnect".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        drop(owner_a);
        sleep(Duration::from_millis(50)).await;

        let mut owner_b = connect_and_handshake_with_principal(&endpoint, principal_id).await;
        let mutation = send_request(
            &mut owner_b,
            186,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("still-owner".to_string()),
            },
        )
        .await;
        assert!(matches!(
            mutation,
            Response::Ok(ResponsePayload::WindowCreated {
                session_id: created_session,
                ..
            }) if created_session == session_id
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rapid_grant_revoke_toggles_attach_input_permissions() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut owner = connect_and_handshake(&endpoint).await;
        let mut member = connect_and_handshake(&endpoint).await;

        let session_id = match send_request(
            &mut owner,
            190,
            Request::NewSession {
                name: Some("rapid-role-toggle".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let member_id = match send_request(&mut member, 191, Request::WhoAmI).await {
            Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
            other => panic!("unexpected whoami response: {other:?}"),
        };

        let grant = match send_request(
            &mut member,
            192,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response: {other:?}"),
        };
        let opened = send_request(
            &mut member,
            193,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert!(matches!(
            opened,
            Response::Ok(ResponsePayload::AttachReady {
                session_id: opened_session,
                can_write: false,
            }) if opened_session == session_id
        ));

        for idx in 0..3u64 {
            let grant_writer = send_request(
                &mut owner,
                194 + idx * 3,
                Request::GrantRole {
                    session: SessionSelector::ById(session_id),
                    client_id: member_id,
                    role: SessionRole::Writer,
                },
            )
            .await;
            assert!(matches!(
                grant_writer,
                Response::Ok(ResponsePayload::RoleGranted {
                    role: SessionRole::Writer,
                    ..
                })
            ));

            let writer_input = send_request(
                &mut member,
                195 + idx * 3,
                Request::AttachInput {
                    session_id,
                    data: b"printf 'writer-allowed\\n'\n".to_vec(),
                },
            )
            .await;
            assert!(matches!(
                writer_input,
                Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
            ));

            let revoke = send_request(
                &mut owner,
                196 + idx * 3,
                Request::RevokeRole {
                    session: SessionSelector::ById(session_id),
                    client_id: member_id,
                },
            )
            .await;
            assert!(matches!(
                revoke,
                Response::Ok(ResponsePayload::RoleRevoked {
                    role: SessionRole::Observer,
                    ..
                })
            ));

            let denied_input = send_request(
                &mut member,
                197 + idx * 3,
                Request::AttachInput {
                    session_id,
                    data: b"printf 'writer-denied\\n'\n".to_vec(),
                },
            )
            .await;
            assert!(matches!(
                denied_input,
                Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    ..
                })
            ));
        }

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owner_transfer_during_follow_attach_preserves_control_rules() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut owner = connect_and_handshake(&endpoint).await;
        let mut successor = connect_and_handshake(&endpoint).await;

        let session_id = match send_request(
            &mut owner,
            230,
            Request::NewSession {
                name: Some("follow-transfer".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let successor_id = match send_request(&mut successor, 231, Request::WhoAmI).await {
            Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
            other => panic!("unexpected whoami response: {other:?}"),
        };

        let follow_started = send_request(
            &mut successor,
            232,
            Request::FollowClient {
                target_client_id: match send_request(&mut owner, 233, Request::WhoAmI).await {
                    Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
                    other => panic!("unexpected owner whoami response: {other:?}"),
                },
                global: true,
            },
        )
        .await;
        assert!(matches!(
            follow_started,
            Response::Ok(ResponsePayload::FollowStarted { global: true, .. })
        ));

        let owner_attach = send_request(
            &mut owner,
            234,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await;
        assert!(matches!(
            owner_attach,
            Response::Ok(ResponsePayload::Attached { .. })
        ));

        let successor_grant = match send_request(
            &mut successor,
            235,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected successor attach response: {other:?}"),
        };
        let successor_open = send_request(
            &mut successor,
            236,
            Request::AttachOpen {
                session_id,
                attach_token: successor_grant.attach_token,
            },
        )
        .await;
        assert!(matches!(
            successor_open,
            Response::Ok(ResponsePayload::AttachReady {
                session_id: opened_session,
                can_write: false,
            }) if opened_session == session_id
        ));

        let transfer = send_request(
            &mut owner,
            237,
            Request::GrantRole {
                session: SessionSelector::ById(session_id),
                client_id: successor_id,
                role: SessionRole::Owner,
            },
        )
        .await;
        assert!(matches!(
            transfer,
            Response::Ok(ResponsePayload::RoleGranted {
                role: SessionRole::Owner,
                ..
            })
        ));

        let old_owner_mutation = send_request(
            &mut owner,
            238,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("old-owner-blocked".to_string()),
            },
        )
        .await;
        assert!(matches!(
            old_owner_mutation,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        let new_owner_mutation = send_request(
            &mut successor,
            239,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("new-owner-allowed".to_string()),
            },
        )
        .await;
        assert!(matches!(
            new_owner_mutation,
            Response::Ok(ResponsePayload::WindowCreated {
                session_id: created_session,
                ..
            }) if created_session == session_id
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn observer_remains_read_only_across_follow_target_changes() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut leader = connect_and_handshake(&endpoint).await;
        let mut observer = connect_and_handshake(&endpoint).await;

        let alpha = match send_request(
            &mut leader,
            260,
            Request::NewSession {
                name: Some("observer-alpha".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected alpha create response: {other:?}"),
        };
        let beta = match send_request(
            &mut leader,
            261,
            Request::NewSession {
                name: Some("observer-beta".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected beta create response: {other:?}"),
        };

        let leader_id = match send_request(&mut leader, 262, Request::WhoAmI).await {
            Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
            other => panic!("unexpected leader whoami response: {other:?}"),
        };

        let leader_attach_alpha = send_request(
            &mut leader,
            263,
            Request::Attach {
                selector: SessionSelector::ById(alpha),
            },
        )
        .await;
        assert!(matches!(
            leader_attach_alpha,
            Response::Ok(ResponsePayload::Attached { .. })
        ));

        let observer_follow = send_request(
            &mut observer,
            264,
            Request::FollowClient {
                target_client_id: leader_id,
                global: true,
            },
        )
        .await;
        assert!(matches!(
            observer_follow,
            Response::Ok(ResponsePayload::FollowStarted { global: true, .. })
        ));

        let observer_alpha_grant = match send_request(
            &mut observer,
            265,
            Request::Attach {
                selector: SessionSelector::ById(alpha),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected observer attach response: {other:?}"),
        };
        let observer_alpha_open = send_request(
            &mut observer,
            266,
            Request::AttachOpen {
                session_id: alpha,
                attach_token: observer_alpha_grant.attach_token,
            },
        )
        .await;
        assert!(matches!(
            observer_alpha_open,
            Response::Ok(ResponsePayload::AttachReady {
                session_id: opened_session,
                can_write: false,
            }) if opened_session == alpha
        ));

        let observer_alpha_input = send_request(
            &mut observer,
            267,
            Request::AttachInput {
                session_id: alpha,
                data: b"printf 'observer-alpha'\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            observer_alpha_input,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        let leader_attach_beta = send_request(
            &mut leader,
            268,
            Request::Attach {
                selector: SessionSelector::ById(beta),
            },
        )
        .await;
        assert!(matches!(
            leader_attach_beta,
            Response::Ok(ResponsePayload::Attached { .. })
        ));

        let observer_beta_grant = match send_request(
            &mut observer,
            269,
            Request::Attach {
                selector: SessionSelector::ById(beta),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected observer beta attach response: {other:?}"),
        };
        let observer_beta_open = send_request(
            &mut observer,
            270,
            Request::AttachOpen {
                session_id: beta,
                attach_token: observer_beta_grant.attach_token,
            },
        )
        .await;
        assert!(matches!(
            observer_beta_open,
            Response::Ok(ResponsePayload::AttachReady {
                session_id: opened_session,
                can_write: false,
            }) if opened_session == beta
        ));

        let observer_beta_input = send_request(
            &mut observer,
            271,
            Request::AttachInput {
                session_id: beta,
                data: b"printf 'observer-beta'\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            observer_beta_input,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn window_lifecycle_supports_create_list_switch_and_kill() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            12,
            Request::NewSession {
                name: Some("windows".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create session response: {other:?}"),
        };

        let created_window = send_request(
            &mut client,
            13,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("logs".to_string()),
            },
        )
        .await;
        let logs_window_id = match created_window {
            Response::Ok(ResponsePayload::WindowCreated {
                id,
                session_id: sid,
                ..
            }) => {
                assert_eq!(sid, session_id);
                id
            }
            other => panic!("unexpected create window response: {other:?}"),
        };

        let listed = send_request(
            &mut client,
            14,
            Request::ListWindows {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await;
        let windows = match listed {
            Response::Ok(ResponsePayload::WindowList { windows }) => windows,
            other => panic!("unexpected list windows response: {other:?}"),
        };
        assert_eq!(windows.len(), 2);

        let switched = send_request(
            &mut client,
            15,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ById(logs_window_id),
            },
        )
        .await;
        assert!(matches!(
            switched,
            Response::Ok(ResponsePayload::WindowSwitched {
                id,
                session_id: switched_session,
                number: 2,
            }) if id == logs_window_id && switched_session == session_id
        ));

        let killed = send_request(
            &mut client,
            16,
            Request::KillWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ById(logs_window_id),
                force_local: false,
            },
        )
        .await;
        assert_eq!(
            killed,
            Response::Ok(ResponsePayload::WindowKilled {
                id: logs_window_id,
                session_id,
            })
        );

        {
            let runtime_manager = server
                .state
                .session_runtimes
                .lock()
                .expect("runtime manager lock should succeed");
            assert_eq!(runtime_manager.window_count(SessionId(session_id)), 1);
        }

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn switch_window_accepts_uuid_prefix_via_name_selector() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            50,
            Request::NewSession {
                name: Some("window-prefix".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create session response: {other:?}"),
        };

        let created_window = send_request(
            &mut client,
            51,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("logs".to_string()),
            },
        )
        .await;
        let logs_window_id = match created_window {
            Response::Ok(ResponsePayload::WindowCreated { id, .. }) => id,
            other => panic!("unexpected create window response: {other:?}"),
        };

        let prefix = logs_window_id.to_string()[..2].to_string();
        let switched = send_request(
            &mut client,
            52,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ByName(prefix),
            },
        )
        .await;
        assert!(matches!(
            switched,
            Response::Ok(ResponsePayload::WindowSwitched {
                id,
                session_id: switched_session,
                number: 2,
            }) if id == logs_window_id && switched_session == session_id
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn switch_window_name_selector_prefers_exact_name() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            53,
            Request::NewSession {
                name: Some("window-name-priority".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create session response: {other:?}"),
        };

        let source_window = send_request(
            &mut client,
            54,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("source".to_string()),
            },
        )
        .await;
        let source_window_id = match source_window {
            Response::Ok(ResponsePayload::WindowCreated { id, .. }) => id,
            other => panic!("unexpected source window response: {other:?}"),
        };
        let selector_value = source_window_id.to_string()[..2].to_string();

        let named_window = send_request(
            &mut client,
            55,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some(selector_value.clone()),
            },
        )
        .await;
        let named_window_id = match named_window {
            Response::Ok(ResponsePayload::WindowCreated { id, .. }) => id,
            other => panic!("unexpected named window response: {other:?}"),
        };

        let switched = send_request(
            &mut client,
            56,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ByName(selector_value),
            },
        )
        .await;
        assert!(matches!(
            switched,
            Response::Ok(ResponsePayload::WindowSwitched {
                id,
                session_id: switched_session,
                number: 3,
            }) if id == named_window_id && switched_session == session_id
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn switch_window_empty_prefix_picks_first_match_deterministically() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            57,
            Request::NewSession {
                name: Some("window-prefix-order".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create session response: {other:?}"),
        };

        let _ = send_request(
            &mut client,
            58,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("alpha".to_string()),
            },
        )
        .await;
        let _ = send_request(
            &mut client,
            59,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("beta".to_string()),
            },
        )
        .await;

        let listed = send_request(
            &mut client,
            60,
            Request::ListWindows {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await;
        let expected_first = match listed {
            Response::Ok(ResponsePayload::WindowList { windows }) => windows
                .first()
                .map(|window| window.id)
                .expect("expected at least one window"),
            other => panic!("unexpected list windows response: {other:?}"),
        };

        let switched = send_request(
            &mut client,
            61,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ByName(String::new()),
            },
        )
        .await;
        assert!(matches!(
            switched,
            Response::Ok(ResponsePayload::WindowSwitched {
                id,
                session_id: switched_session,
                number: 1,
            }) if id == expected_first && switched_session == session_id
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn switch_window_name_not_found_includes_lookup_chain() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            62,
            Request::NewSession {
                name: Some("window-not-found-chain".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create session response: {other:?}"),
        };

        let switched = send_request(
            &mut client,
            63,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ByName("does-not-exist".to_string()),
            },
        )
        .await;
        match switched {
            Response::Err(ErrorResponse {
                code: ErrorCode::NotFound,
                message,
            }) => {
                assert!(message.contains("lookup order"));
                assert!(message.contains("exact name"));
                assert!(message.contains("exact UUID"));
                assert!(message.contains("UUID prefix"));
            }
            other => panic!("unexpected switch response: {other:?}"),
        }

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn killing_last_window_removes_session() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            17,
            Request::NewSession {
                name: Some("single-window".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let listed = send_request(
            &mut client,
            18,
            Request::ListWindows {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await;
        let window_id = match listed {
            Response::Ok(ResponsePayload::WindowList { windows }) if windows.len() == 1 => {
                windows[0].id
            }
            other => panic!("unexpected initial window list: {other:?}"),
        };

        let killed = send_request(
            &mut client,
            19,
            Request::KillWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ById(window_id),
                force_local: false,
            },
        )
        .await;
        assert!(matches!(
            killed,
            Response::Ok(ResponsePayload::WindowKilled { .. })
        ));

        let listed_sessions = send_request(&mut client, 20, Request::ListSessions).await;
        assert_eq!(
            listed_sessions,
            Response::Ok(ResponsePayload::SessionList {
                sessions: Vec::new(),
            })
        );

        {
            let runtime_manager = server
                .state
                .session_runtimes
                .lock()
                .expect("runtime manager lock should succeed");
            assert_eq!(runtime_manager.runtime_count(), 0);
        }

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supports_attach_detach_and_kill() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            20,
            Request::NewSession {
                name: Some("ops".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected new-session response: {other:?}"),
        };

        let attached = send_request(
            &mut client,
            21,
            Request::Attach {
                selector: SessionSelector::ByName("ops".to_string()),
            },
        )
        .await;
        let grant = match attached {
            Response::Ok(ResponsePayload::Attached { grant }) => {
                assert_eq!(grant.session_id, session_id);
                grant
            }
            other => panic!("unexpected attach response: {other:?}"),
        };

        let attach_open = send_request(
            &mut client,
            211,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert_eq!(
            attach_open,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: true,
            })
        );

        let detached = send_request(&mut client, 22, Request::Detach).await;
        assert_eq!(detached, Response::Ok(ResponsePayload::Detached));

        let killed = send_request(
            &mut client,
            23,
            Request::KillSession {
                selector: SessionSelector::ById(session_id),
                force_local: false,
            },
        )
        .await;
        assert_eq!(
            killed,
            Response::Ok(ResponsePayload::SessionKilled { id: session_id })
        );

        let listed = send_request(&mut client, 24, Request::ListSessions).await;
        assert_eq!(
            listed,
            Response::Ok(ResponsePayload::SessionList {
                sessions: Vec::new(),
            })
        );

        {
            let runtime_manager = server
                .state
                .session_runtimes
                .lock()
                .expect("runtime manager lock should succeed");
            assert_eq!(runtime_manager.runtime_count(), 0);
            assert!(!runtime_manager.has_runtime(SessionId(session_id)));
        }

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn allows_second_attach_for_same_session_with_read_only_input() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client_a = connect_and_handshake(&endpoint).await;
        let mut client_b = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client_a,
            60,
            Request::NewSession {
                name: Some("single-attach".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let grant_a = match send_request(
            &mut client_a,
            61,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response for client a: {other:?}"),
        };
        let open_a = send_request(
            &mut client_a,
            62,
            Request::AttachOpen {
                session_id,
                attach_token: grant_a.attach_token,
            },
        )
        .await;
        assert_eq!(
            open_a,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: true,
            })
        );

        let grant_b = match send_request(
            &mut client_b,
            63,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response for client b: {other:?}"),
        };
        let open_b = send_request(
            &mut client_b,
            64,
            Request::AttachOpen {
                session_id,
                attach_token: grant_b.attach_token,
            },
        )
        .await;
        assert_eq!(
            open_b,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: false,
            })
        );

        let owner_write = send_request(
            &mut client_a,
            65,
            Request::AttachInput {
                session_id,
                data: b"printf 'owner-ok\\n'\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            owner_write,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));

        let output_a = collect_attach_output_until(&mut client_a, session_id, "owner-ok", 20).await;
        assert!(output_a.contains("owner-ok"));
        let output_b = collect_attach_output_until(&mut client_b, session_id, "owner-ok", 20).await;
        assert!(output_b.contains("owner-ok"));

        let follower_write = send_request(
            &mut client_b,
            66,
            Request::AttachInput {
                session_id,
                data: b"printf 'follower-write'\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            follower_write,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detach_keeps_runtime_alive_and_allows_reattach() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            70,
            Request::NewSession {
                name: Some("reattach".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let grant = match send_request(
            &mut client,
            71,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response: {other:?}"),
        };
        let open = send_request(
            &mut client,
            72,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert_eq!(
            open,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: true,
            })
        );

        let marker = "resume-check";
        let write_before_detach = send_request(
            &mut client,
            721,
            Request::AttachInput {
                session_id,
                data: format!("printf '{marker}\\n'\\n").into_bytes(),
            },
        )
        .await;
        assert!(matches!(
            write_before_detach,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));

        let output_before_detach =
            collect_attach_output_until(&mut client, session_id, marker, 20).await;
        assert!(
            output_before_detach.contains(marker),
            "expected marker in pre-detach output, got: {output_before_detach:?}"
        );

        let detached = send_request(&mut client, 73, Request::Detach).await;
        assert_eq!(detached, Response::Ok(ResponsePayload::Detached));

        {
            let runtime_manager = server
                .state
                .session_runtimes
                .lock()
                .expect("runtime manager lock should succeed");
            assert_eq!(runtime_manager.runtime_count(), 1);
            assert!(runtime_manager.has_runtime(SessionId(session_id)));
        }

        let regrant = match send_request(
            &mut client,
            74,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected reattach grant response: {other:?}"),
        };
        let reopen = send_request(
            &mut client,
            75,
            Request::AttachOpen {
                session_id,
                attach_token: regrant.attach_token,
            },
        )
        .await;
        assert_eq!(
            reopen,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: true,
            })
        );

        let stream_after_reattach = send_request(
            &mut client,
            751,
            Request::AttachOutput {
                session_id,
                max_bytes: 8192,
            },
        )
        .await;
        assert!(matches!(
            stream_after_reattach,
            Response::Ok(ResponsePayload::AttachOutput { data }) if data.is_empty()
        ));

        let snapshot_after_reattach = send_request(
            &mut client,
            76,
            Request::AttachSnapshot {
                session_id,
                max_bytes_per_pane: 1024 * 1024,
            },
        )
        .await;
        match snapshot_after_reattach {
            Response::Ok(ResponsePayload::AttachSnapshot { chunks, .. }) => {
                let combined = chunks
                    .into_iter()
                    .flat_map(|chunk| chunk.data)
                    .collect::<Vec<_>>();
                let text = String::from_utf8_lossy(&combined);
                assert!(
                    text.contains(marker),
                    "expected marker in attach snapshot after reattach, got: {text:?}"
                );
            }
            other => panic!("unexpected attach snapshot response: {other:?}"),
        }

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attach_set_viewport_resizes_active_pane_pty() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            280,
            Request::NewSession {
                name: Some("viewport-size".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };

        let grant = match send_request(
            &mut client,
            281,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response: {other:?}"),
        };

        let open = send_request(
            &mut client,
            282,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert!(matches!(
            open,
            Response::Ok(ResponsePayload::AttachReady {
                session_id: opened,
                can_write: true,
            }) if opened == session_id
        ));

        let viewport_cols: u16 = 120;
        let viewport_rows: u16 = 50;
        let viewport_set = send_request(
            &mut client,
            283,
            Request::AttachSetViewport {
                session_id,
                cols: viewport_cols,
                rows: viewport_rows,
            },
        )
        .await;
        assert_eq!(
            viewport_set,
            Response::Ok(ResponsePayload::AttachViewportSet {
                session_id,
                cols: viewport_cols,
                rows: viewport_rows,
            })
        );

        let (measured_rows, measured_cols) = {
            let runtime_manager = server
                .state
                .session_runtimes
                .lock()
                .expect("runtime manager lock should succeed");
            let runtime = runtime_manager
                .runtimes
                .get(&SessionId(session_id))
                .expect("runtime should exist for created session");
            let window = runtime
                .windows
                .get(&runtime.active_window)
                .expect("active window should exist");
            let pane = window
                .panes
                .get(&window.focused_pane_id)
                .expect("focused pane should exist");
            pane.last_requested_pty_size()
        };

        let expected_rows = viewport_rows.saturating_sub(3);
        let expected_cols = viewport_cols.saturating_sub(2);
        assert_eq!(
            (measured_rows, measured_cols),
            (expected_rows, expected_cols),
            "expected requested PTY size to match pane inner size"
        );

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attach_io_routes_through_active_window() {
        let shell_path = write_test_shell_script(
            "attach-window-routing",
            "#!/bin/sh\nwhile IFS= read -r line; do\n  eval \"$line\"\ndone\n",
        );
        let (server, endpoint, socket_path, server_task) =
            start_server_with_shell(&shell_path).await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            300,
            Request::NewSession {
                name: Some("window-routing".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };

        let listed = send_request(
            &mut client,
            301,
            Request::ListWindows {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await;
        let primary_window = match listed {
            Response::Ok(ResponsePayload::WindowList { windows }) => windows
                .iter()
                .find(|window| window.active)
                .map(|window| window.id)
                .expect("expected active initial window"),
            other => panic!("unexpected window list response: {other:?}"),
        };

        let secondary_window = match send_request(
            &mut client,
            302,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("secondary".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::WindowCreated { id, .. }) => id,
            other => panic!("unexpected new window response: {other:?}"),
        };

        let grant = match send_request(
            &mut client,
            303,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response: {other:?}"),
        };
        let opened = send_request(
            &mut client,
            304,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert_eq!(
            opened,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: true,
            })
        );

        let switched_primary = send_request(
            &mut client,
            305,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ById(primary_window),
            },
        )
        .await;
        assert!(matches!(
            switched_primary,
            Response::Ok(ResponsePayload::WindowSwitched {
                id,
                session_id: switched_session,
                number: 1,
            }) if id == primary_window && switched_session == session_id
        ));

        let export_primary = send_request(
            &mut client,
            306,
            Request::AttachInput {
                session_id,
                data: b"export BMUX_WINDOW_ROUTE=one; printf '__bmux_route_one__\\n'\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            export_primary,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));
        let _ =
            collect_attach_output_until(&mut client, session_id, "__bmux_route_one__", 20).await;

        let switched_secondary = send_request(
            &mut client,
            307,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ById(secondary_window),
            },
        )
        .await;
        assert!(matches!(
            switched_secondary,
            Response::Ok(ResponsePayload::WindowSwitched {
                id,
                session_id: switched_session,
                number: 2,
            }) if id == secondary_window && switched_session == session_id
        ));

        let print_secondary = send_request(
            &mut client,
            308,
            Request::AttachInput {
                session_id,
                data: b"printf 'W2=[%s]\\n' \"$BMUX_WINDOW_ROUTE\"\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            print_secondary,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));
        let second_output = collect_attach_output_until(&mut client, session_id, "W2=[]", 20).await;
        assert!(second_output.contains("W2=[]"));

        let switched_back = send_request(
            &mut client,
            309,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ById(primary_window),
            },
        )
        .await;
        assert!(matches!(
            switched_back,
            Response::Ok(ResponsePayload::WindowSwitched {
                id,
                session_id: switched_session,
                number: 1,
            }) if id == primary_window && switched_session == session_id
        ));

        let print_primary = send_request(
            &mut client,
            310,
            Request::AttachInput {
                session_id,
                data: b"printf 'W1=[%s]\\n' \"$BMUX_WINDOW_ROUTE\"\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            print_primary,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));
        let first_output =
            collect_attach_output_until(&mut client, session_id, "W1=[one]", 20).await;
        assert!(first_output.contains("W1=[one]"));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reattach_uses_current_active_window() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            320,
            Request::NewSession {
                name: Some("reattach-window".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };

        let listed = send_request(
            &mut client,
            321,
            Request::ListWindows {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await;
        let primary_window = match listed {
            Response::Ok(ResponsePayload::WindowList { windows }) => windows
                .iter()
                .find(|window| window.active)
                .map(|window| window.id)
                .expect("expected active initial window"),
            other => panic!("unexpected window list response: {other:?}"),
        };

        let secondary_window = match send_request(
            &mut client,
            322,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("secondary".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::WindowCreated { id, .. }) => id,
            other => panic!("unexpected new window response: {other:?}"),
        };

        let grant = match send_request(
            &mut client,
            323,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response: {other:?}"),
        };
        let opened = send_request(
            &mut client,
            324,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert_eq!(
            opened,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: true,
            })
        );

        let switched_primary = send_request(
            &mut client,
            325,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ById(primary_window),
            },
        )
        .await;
        assert!(matches!(
            switched_primary,
            Response::Ok(ResponsePayload::WindowSwitched {
                id,
                session_id: switched_session,
                number: 1,
            }) if id == primary_window && switched_session == session_id
        ));

        let export_primary = send_request(
            &mut client,
            326,
            Request::AttachInput {
                session_id,
                data: b"export BMUX_REATTACH=one\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            export_primary,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));
        let _ = collect_attach_output_until(&mut client, session_id, "one", 5).await;

        let switched_secondary = send_request(
            &mut client,
            327,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ById(secondary_window),
            },
        )
        .await;
        assert!(matches!(
            switched_secondary,
            Response::Ok(ResponsePayload::WindowSwitched {
                id,
                session_id: switched_session,
                number: 2,
            }) if id == secondary_window && switched_session == session_id
        ));

        let export_secondary = send_request(
            &mut client,
            328,
            Request::AttachInput {
                session_id,
                data: b"export BMUX_REATTACH=two\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            export_secondary,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));
        let _ = collect_attach_output_until(&mut client, session_id, "two", 5).await;

        let detached = send_request(&mut client, 329, Request::Detach).await;
        assert_eq!(detached, Response::Ok(ResponsePayload::Detached));

        let regrant = match send_request(
            &mut client,
            330,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected reattach grant response: {other:?}"),
        };
        let reopened = send_request(
            &mut client,
            331,
            Request::AttachOpen {
                session_id,
                attach_token: regrant.attach_token,
            },
        )
        .await;
        assert_eq!(
            reopened,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: true,
            })
        );

        let print_output = send_request(
            &mut client,
            332,
            Request::AttachInput {
                session_id,
                data: b"printf 'RA=[%s]\\n' \"$BMUX_REATTACH\"\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            print_output,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));
        let output = collect_attach_output_until(&mut client, session_id, "RA=[two]", 20).await;
        assert!(output.contains("RA=[two]"));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn event_subscription_reports_lifecycle_order() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let subscribed = send_request(&mut client, 80, Request::SubscribeEvents).await;
        assert_eq!(subscribed, Response::Ok(ResponsePayload::EventsSubscribed));

        let created = send_request(
            &mut client,
            81,
            Request::NewSession {
                name: Some("events".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let grant = match send_request(
            &mut client,
            82,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response: {other:?}"),
        };
        let opened = send_request(
            &mut client,
            83,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert_eq!(
            opened,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: true,
            })
        );

        let detached = send_request(&mut client, 84, Request::Detach).await;
        assert_eq!(detached, Response::Ok(ResponsePayload::Detached));

        let killed = send_request(
            &mut client,
            85,
            Request::KillSession {
                selector: SessionSelector::ById(session_id),
                force_local: false,
            },
        )
        .await;
        assert_eq!(
            killed,
            Response::Ok(ResponsePayload::SessionKilled { id: session_id })
        );

        let events = send_request(&mut client, 86, Request::PollEvents { max_events: 10 }).await;
        let events = match events {
            Response::Ok(ResponsePayload::EventBatch { events }) => events,
            other => panic!("unexpected events response: {other:?}"),
        };

        let created_idx = events
            .iter()
            .position(
                |event| matches!(event, Event::SessionCreated { id, .. } if *id == session_id),
            )
            .expect("session_created event should exist");
        let attached_idx = events
            .iter()
            .position(|event| matches!(event, Event::ClientAttached { id } if *id == session_id))
            .expect("client_attached event should exist");
        let detached_idx = events
            .iter()
            .position(|event| matches!(event, Event::ClientDetached { id } if *id == session_id))
            .expect("client_detached event should exist");
        let removed_idx = events
            .iter()
            .position(|event| matches!(event, Event::SessionRemoved { id } if *id == session_id))
            .expect("session_removed event should exist");

        assert!(created_idx < attached_idx);
        assert!(attached_idx < detached_idx);
        assert!(detached_idx < removed_idx);

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn global_follow_updates_control_plane_without_rebinding_attach_stream() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut leader = connect_and_handshake(&endpoint).await;
        let mut follower = connect_and_handshake(&endpoint).await;

        let subscribed = send_request(&mut follower, 500, Request::SubscribeEvents).await;
        assert_eq!(subscribed, Response::Ok(ResponsePayload::EventsSubscribed));

        let alpha_session = match send_request(
            &mut leader,
            501,
            Request::NewSession {
                name: Some("leader-alpha".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected alpha session response: {other:?}"),
        };
        let beta_session = match send_request(
            &mut leader,
            502,
            Request::NewSession {
                name: Some("leader-beta".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected beta session response: {other:?}"),
        };

        let leader_attached_alpha = send_request(
            &mut leader,
            503,
            Request::Attach {
                selector: SessionSelector::ById(alpha_session),
            },
        )
        .await;
        assert!(matches!(
            leader_attached_alpha,
            Response::Ok(ResponsePayload::Attached { .. })
        ));

        let leader_client_id =
            discover_client_id_from_window_switch(&mut follower, &mut leader, alpha_session, 1500)
                .await;

        let follow_started = send_request(
            &mut follower,
            504,
            Request::FollowClient {
                target_client_id: leader_client_id,
                global: true,
            },
        )
        .await;
        assert!(matches!(
            follow_started,
            Response::Ok(ResponsePayload::FollowStarted { global: true, .. })
        ));

        let follower_windows_alpha =
            send_request(&mut follower, 505, Request::ListWindows { session: None }).await;
        match follower_windows_alpha {
            Response::Ok(ResponsePayload::WindowList { windows }) => {
                assert_eq!(windows.len(), 1);
                assert_eq!(windows[0].session_id, alpha_session);
            }
            other => panic!("unexpected follower windows(alpha) response: {other:?}"),
        }

        let follower_client_id = match send_request(&mut follower, 5051, Request::WhoAmI).await {
            Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
            other => panic!("unexpected follower whoami response: {other:?}"),
        };
        let grant_writer = send_request(
            &mut leader,
            5052,
            Request::GrantRole {
                session: SessionSelector::ById(alpha_session),
                client_id: follower_client_id,
                role: SessionRole::Writer,
            },
        )
        .await;
        assert!(matches!(
            grant_writer,
            Response::Ok(ResponsePayload::RoleGranted {
                role: SessionRole::Writer,
                ..
            })
        ));

        let grant = match send_request(
            &mut follower,
            506,
            Request::Attach {
                selector: SessionSelector::ById(alpha_session),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected follower attach response: {other:?}"),
        };
        let opened = send_request(
            &mut follower,
            507,
            Request::AttachOpen {
                session_id: alpha_session,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert_eq!(
            opened,
            Response::Ok(ResponsePayload::AttachReady {
                session_id: alpha_session,
                can_write: true,
            })
        );

        let set_marker = send_request(
            &mut follower,
            508,
            Request::AttachInput {
                session_id: alpha_session,
                data: b"export BMUX_FOLLOW_STREAM=ok; printf '__bmux_follow_ready__\\n'\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            set_marker,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));
        let _ =
            collect_attach_output_until(&mut follower, alpha_session, "__bmux_follow_ready__", 20)
                .await;

        let leader_attached_beta = send_request(
            &mut leader,
            509,
            Request::Attach {
                selector: SessionSelector::ById(beta_session),
            },
        )
        .await;
        assert!(matches!(
            leader_attached_beta,
            Response::Ok(ResponsePayload::Attached { .. })
        ));

        let follower_windows_beta =
            send_request(&mut follower, 510, Request::ListWindows { session: None }).await;
        match follower_windows_beta {
            Response::Ok(ResponsePayload::WindowList { windows }) => {
                assert_eq!(windows.len(), 1);
                assert_eq!(windows[0].session_id, beta_session);
            }
            other => panic!("unexpected follower windows(beta) response: {other:?}"),
        }

        let print_stream = send_request(
            &mut follower,
            511,
            Request::AttachInput {
                session_id: alpha_session,
                data: b"printf 'FS=[%s]\\n' \"$BMUX_FOLLOW_STREAM\"\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            print_stream,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));
        let output = collect_attach_output_until(&mut follower, alpha_session, "FS=[ok]", 20).await;
        assert!(output.contains("FS=[ok]"));

        let unfollowed = send_request(&mut follower, 512, Request::Unfollow).await;
        assert!(matches!(
            unfollowed,
            Response::Ok(ResponsePayload::FollowStopped { .. })
        ));

        let events = poll_events_collect(&mut follower, 513, 10, 4).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::FollowStarted { global: true, .. }))
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                Event::FollowTargetChanged {
                    session_id,
                    ..
                } if *session_id == beta_session
            )
        }));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::FollowStopped { .. }))
        );

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attach_view_changed_events_cover_layout_tabs_and_full_scopes() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut actor = connect_and_handshake(&endpoint).await;
        let mut observer = connect_and_handshake(&endpoint).await;

        let subscribed = send_request(&mut observer, 900, Request::SubscribeEvents).await;
        assert_eq!(subscribed, Response::Ok(ResponsePayload::EventsSubscribed));

        let session_id = match send_request(
            &mut actor,
            901,
            Request::NewSession {
                name: Some("attach-view-events".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };

        let _ = poll_events_collect(&mut observer, 902, 10, 4).await;

        let split = send_request(
            &mut actor,
            910,
            Request::SplitPane {
                session: Some(SessionSelector::ById(session_id)),
                target: None,
                direction: PaneSplitDirection::Vertical,
            },
        )
        .await;
        assert!(matches!(
            split,
            Response::Ok(ResponsePayload::PaneSplit { .. })
        ));

        let split_events = poll_events_collect(&mut observer, 911, 10, 4).await;
        let split_revision = split_events
            .iter()
            .find_map(|event| match event {
                Event::AttachViewChanged {
                    session_id: changed_session,
                    revision,
                    components,
                } if *changed_session == session_id
                    && components == &vec![AttachViewComponent::Scene] =>
                {
                    Some(*revision)
                }
                _ => None,
            })
            .expect("layout attach view change should exist after split");

        let created_window_id = match send_request(
            &mut actor,
            920,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("extra".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::WindowCreated { id, .. }) => id,
            other => panic!("unexpected window create response: {other:?}"),
        };

        let window_create_events = poll_events_collect(&mut observer, 921, 10, 4).await;
        let tabs_revision = window_create_events
            .iter()
            .find_map(|event| match event {
                Event::AttachViewChanged {
                    session_id: changed_session,
                    revision,
                    components,
                } if *changed_session == session_id
                    && components == &vec![AttachViewComponent::Tabs] =>
                {
                    Some(*revision)
                }
                _ => None,
            })
            .expect("tabs attach view change should exist after window create");
        assert!(tabs_revision > split_revision);

        let switched = send_request(
            &mut actor,
            930,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::ById(created_window_id),
            },
        )
        .await;
        assert!(matches!(
            switched,
            Response::Ok(ResponsePayload::WindowSwitched {
                session_id: switched_session,
                ..
            }) if switched_session == session_id
        ));

        let switch_events = poll_events_collect(&mut observer, 931, 10, 4).await;
        let switched_revision = switch_events
            .iter()
            .find_map(|event| match event {
                Event::AttachViewChanged {
                    session_id: changed_session,
                    revision,
                    components,
                } if *changed_session == session_id
                    && components
                        == &vec![AttachViewComponent::Scene, AttachViewComponent::Tabs] =>
                {
                    Some(*revision)
                }
                _ => None,
            })
            .expect("layout+tabs attach view change should exist after window switch");
        assert!(switched_revision > tabs_revision);

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exiting_attached_split_pane_emits_layout_change_and_updates_attach_layout() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut actor = connect_and_handshake(&endpoint).await;
        let mut observer = connect_and_handshake(&endpoint).await;

        let subscribed = send_request(&mut observer, 940, Request::SubscribeEvents).await;
        assert_eq!(subscribed, Response::Ok(ResponsePayload::EventsSubscribed));

        let session_id = match send_request(
            &mut actor,
            941,
            Request::NewSession {
                name: Some("pane-exit-attach-refresh".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };

        let grant = match send_request(
            &mut actor,
            942,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach grant response: {other:?}"),
        };
        let opened = send_request(
            &mut actor,
            943,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert!(matches!(
            opened,
            Response::Ok(ResponsePayload::AttachReady {
                session_id: attached_session,
                ..
            }) if attached_session == session_id
        ));

        let _ = poll_events_collect(&mut observer, 944, 16, 4).await;

        let split = send_request(
            &mut actor,
            945,
            Request::SplitPane {
                session: Some(SessionSelector::ById(session_id)),
                target: None,
                direction: PaneSplitDirection::Horizontal,
            },
        )
        .await;
        let split_pane_id = match split {
            Response::Ok(ResponsePayload::PaneSplit { id, .. }) => id,
            other => panic!("unexpected split response: {other:?}"),
        };

        let split_events = poll_events_collect(&mut observer, 946, 16, 4).await;
        assert!(split_events.iter().any(|event| {
            matches!(
                event,
                Event::AttachViewChanged {
                    session_id: changed_session,
                    components,
                    ..
                } if *changed_session == session_id
                    && components == &vec![AttachViewComponent::Scene]
            )
        }));

        let accepted = send_request(&mut actor, 947, Request::AttachLayout { session_id }).await;
        assert!(matches!(
            accepted,
            Response::Ok(ResponsePayload::AttachLayout { .. })
        ));

        let exited_pane_id = reap_focused_attached_pane(&server, session_id).await;

        let exit_events = poll_events_collect(&mut observer, 948, 16, 1).await;
        let saw_scene_refresh = exit_events.iter().any(|event| {
            matches!(
                event,
                Event::AttachViewChanged {
                    session_id: changed_session,
                    components,
                    ..
                } if *changed_session == session_id
                    && components.contains(&AttachViewComponent::Scene)
            )
        });

        assert!(
            saw_scene_refresh,
            "expected attach scene refresh after exiting split pane"
        );
        let layout = send_request(&mut actor, 949, Request::AttachLayout { session_id }).await;
        let (focused_pane_id, panes) = match layout {
            Response::Ok(ResponsePayload::AttachLayout {
                focused_pane_id,
                panes,
                ..
            }) => (focused_pane_id, panes),
            other => panic!("unexpected attach layout response: {other:?}"),
        };
        assert_eq!(exited_pane_id, split_pane_id);
        assert_eq!(panes[0].id, focused_pane_id);
        assert_eq!(panes.len(), 1);

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attached_split_pane_exit_via_pty_eventually_updates_attach_layout() {
        let shell_path = write_test_shell_script(
            "pty-exit-smoke",
            "#!/bin/sh\nwhile IFS= read -r line; do\n  if [ \"$line\" = \"__bmux_ready__\" ]; then\n    printf '__bmux_ready__\\n'\n  elif [ \"$line\" = \"__bmux_exit__\" ]; then\n    printf '__bmux_exit__\\n'\n    exit 0\n  fi\ndone\n",
        );
        let (server, endpoint, socket_path, server_task) =
            start_server_with_shell(&shell_path).await;
        let mut actor = connect_and_handshake(&endpoint).await;
        let mut observer = connect_and_handshake(&endpoint).await;

        let subscribed = send_request(&mut observer, 979, Request::SubscribeEvents).await;
        assert_eq!(subscribed, Response::Ok(ResponsePayload::EventsSubscribed));

        let session_id = match send_request(
            &mut actor,
            980,
            Request::NewSession {
                name: Some("pane-exit-pty-smoke".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };

        let grant = match send_request(
            &mut actor,
            981,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach grant response: {other:?}"),
        };
        let opened = send_request(
            &mut actor,
            982,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert!(matches!(
            opened,
            Response::Ok(ResponsePayload::AttachReady {
                session_id: attached_session,
                ..
            }) if attached_session == session_id
        ));

        let _ = poll_events_collect(&mut observer, 982, 16, 4).await;

        let split = send_request(
            &mut actor,
            983,
            Request::SplitPane {
                session: Some(SessionSelector::ById(session_id)),
                target: None,
                direction: PaneSplitDirection::Horizontal,
            },
        )
        .await;
        let split_pane_id = match split {
            Response::Ok(ResponsePayload::PaneSplit { id, .. }) => id,
            other => panic!("unexpected split response: {other:?}"),
        };

        let split_events = poll_events_collect(&mut observer, 984, 16, 4).await;
        assert!(split_events.iter().any(|event| {
            matches!(
                event,
                Event::AttachViewChanged {
                    session_id: changed_session,
                    components,
                    ..
                } if *changed_session == session_id
                    && components == &vec![AttachViewComponent::Scene]
            )
        }));

        let ready_input = send_request(
            &mut actor,
            985,
            Request::AttachInput {
                session_id,
                data: b"__bmux_ready__\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            ready_input,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));

        let ready_output =
            collect_attach_output_until(&mut actor, session_id, "__bmux_ready__", 20).await;
        assert!(
            ready_output.contains("__bmux_ready__"),
            "split pane should echo readiness sentinel before exit: {ready_output}"
        );

        let exit_input = send_request(
            &mut actor,
            986,
            Request::AttachInput {
                session_id,
                data: b"__bmux_exit__\n".to_vec(),
            },
        )
        .await;
        assert!(matches!(
            exit_input,
            Response::Ok(ResponsePayload::AttachInputAccepted { bytes }) if bytes > 0
        ));

        let mut final_layout = None;
        for request_id in 1000..=1080 {
            let _ = poll_events_collect(&mut observer, 2000 + request_id as u64, 16, 2).await;

            let layout =
                send_request(&mut actor, request_id, Request::AttachLayout { session_id }).await;
            if let Response::Ok(ResponsePayload::AttachLayout {
                focused_pane_id,
                panes,
                ..
            }) = layout
                && panes.len() == 1
            {
                final_layout = Some((focused_pane_id, panes));
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }

        let (focused_pane_id, panes) =
            final_layout.expect("attach layout should collapse to one pane after pty exit");
        assert_ne!(focused_pane_id, split_pane_id);
        assert_eq!(panes[0].id, focused_pane_id);

        stop_server(server, server_task, &socket_path).await;
        let _ = std::fs::remove_file(shell_path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn role_change_emits_status_attach_view_change() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut owner = connect_and_handshake(&endpoint).await;
        let mut target = connect_and_handshake(&endpoint).await;
        let mut observer = connect_and_handshake(&endpoint).await;

        let subscribed = send_request(&mut observer, 970, Request::SubscribeEvents).await;
        assert_eq!(subscribed, Response::Ok(ResponsePayload::EventsSubscribed));

        let session_id = match send_request(
            &mut owner,
            971,
            Request::NewSession {
                name: Some("role-status-refresh".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };
        let _ = poll_events_collect(&mut observer, 972, 16, 4).await;

        let target_client_id = match send_request(&mut target, 973, Request::WhoAmI).await {
            Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
            other => panic!("unexpected whoami response: {other:?}"),
        };

        let granted = send_request(
            &mut owner,
            974,
            Request::GrantRole {
                session: SessionSelector::ById(session_id),
                client_id: target_client_id,
                role: SessionRole::Writer,
            },
        )
        .await;
        assert!(matches!(
            granted,
            Response::Ok(ResponsePayload::RoleGranted {
                role: SessionRole::Writer,
                ..
            })
        ));

        let role_events = poll_events_collect(&mut observer, 975, 16, 8).await;
        assert!(role_events.iter().any(|event| {
            matches!(
                event,
                Event::AttachViewChanged {
                    session_id: changed_session,
                    components,
                    ..
                } if *changed_session == session_id
                    && components == &vec![AttachViewComponent::Status]
            )
        }));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn leader_disconnect_emits_follow_target_gone() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut leader = connect_and_handshake(&endpoint).await;
        let mut follower = connect_and_handshake(&endpoint).await;

        let subscribed = send_request(&mut follower, 540, Request::SubscribeEvents).await;
        assert_eq!(subscribed, Response::Ok(ResponsePayload::EventsSubscribed));

        let session_id = match send_request(
            &mut leader,
            541,
            Request::NewSession {
                name: Some("follow-disconnect".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };
        let leader_client_id =
            discover_client_id_from_window_switch(&mut follower, &mut leader, session_id, 1600)
                .await;

        let follow_started = send_request(
            &mut follower,
            542,
            Request::FollowClient {
                target_client_id: leader_client_id,
                global: false,
            },
        )
        .await;
        assert!(matches!(
            follow_started,
            Response::Ok(ResponsePayload::FollowStarted { global: false, .. })
        ));

        drop(leader);

        let events = poll_events_collect(&mut follower, 543, 10, 8).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::FollowTargetGone { .. }))
        );

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rapid_leader_session_switches_emit_multiple_follow_target_changes() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut leader = connect_and_handshake(&endpoint).await;
        let mut follower = connect_and_handshake(&endpoint).await;

        let subscribed = send_request(&mut follower, 580, Request::SubscribeEvents).await;
        assert_eq!(subscribed, Response::Ok(ResponsePayload::EventsSubscribed));

        let alpha = match send_request(
            &mut leader,
            581,
            Request::NewSession {
                name: Some("rapid-alpha".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected alpha create response: {other:?}"),
        };
        let beta = match send_request(
            &mut leader,
            582,
            Request::NewSession {
                name: Some("rapid-beta".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected beta create response: {other:?}"),
        };

        let attached_alpha = send_request(
            &mut leader,
            583,
            Request::Attach {
                selector: SessionSelector::ById(alpha),
            },
        )
        .await;
        assert!(matches!(
            attached_alpha,
            Response::Ok(ResponsePayload::Attached { .. })
        ));

        let leader_client_id = match send_request(&mut leader, 584, Request::ListClients).await {
            Response::Ok(ResponsePayload::ClientList { clients }) => clients
                .into_iter()
                .find(|client| client.selected_session_id == Some(alpha))
                .map(|client| client.id)
                .expect("leader client should be listed"),
            other => panic!("unexpected client list response: {other:?}"),
        };

        let follow_started = send_request(
            &mut follower,
            585,
            Request::FollowClient {
                target_client_id: leader_client_id,
                global: true,
            },
        )
        .await;
        assert!(matches!(
            follow_started,
            Response::Ok(ResponsePayload::FollowStarted { global: true, .. })
        ));

        let attached_beta = send_request(
            &mut leader,
            586,
            Request::Attach {
                selector: SessionSelector::ById(beta),
            },
        )
        .await;
        assert!(matches!(
            attached_beta,
            Response::Ok(ResponsePayload::Attached { .. })
        ));

        let attached_alpha_again = send_request(
            &mut leader,
            587,
            Request::Attach {
                selector: SessionSelector::ById(alpha),
            },
        )
        .await;
        assert!(matches!(
            attached_alpha_again,
            Response::Ok(ResponsePayload::Attached { .. })
        ));

        let events = poll_events_collect(&mut follower, 588, 32, 10).await;
        let target_change_sessions = events
            .iter()
            .filter_map(|event| match event {
                Event::FollowTargetChanged {
                    leader_client_id: event_leader,
                    session_id,
                    ..
                } if *event_leader == leader_client_id => Some(*session_id),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(target_change_sessions.contains(&alpha));
        assert!(target_change_sessions.contains(&beta));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detach_uses_active_stream_session_when_follow_updates_selected_session() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut leader = connect_and_handshake(&endpoint).await;
        let mut follower = connect_and_handshake(&endpoint).await;

        let alpha = match send_request(
            &mut leader,
            620,
            Request::NewSession {
                name: Some("detach-alpha".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected alpha create response: {other:?}"),
        };
        let beta = match send_request(
            &mut leader,
            621,
            Request::NewSession {
                name: Some("detach-beta".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected beta create response: {other:?}"),
        };

        let _ = send_request(
            &mut leader,
            622,
            Request::Attach {
                selector: SessionSelector::ById(alpha),
            },
        )
        .await;
        let leader_client_id = match send_request(&mut leader, 623, Request::ListClients).await {
            Response::Ok(ResponsePayload::ClientList { clients }) => clients
                .into_iter()
                .find(|client| client.selected_session_id == Some(alpha))
                .map(|client| client.id)
                .expect("leader client should be listed"),
            other => panic!("unexpected client list response: {other:?}"),
        };

        let _ = send_request(
            &mut follower,
            624,
            Request::FollowClient {
                target_client_id: leader_client_id,
                global: true,
            },
        )
        .await;

        let follower_grant_alpha = match send_request(
            &mut follower,
            625,
            Request::Attach {
                selector: SessionSelector::ById(alpha),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected follower attach response: {other:?}"),
        };
        let follower_open_alpha = send_request(
            &mut follower,
            626,
            Request::AttachOpen {
                session_id: alpha,
                attach_token: follower_grant_alpha.attach_token,
            },
        )
        .await;
        assert!(matches!(
            follower_open_alpha,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                ..
            }) if session_id == alpha
        ));

        let _ = send_request(
            &mut leader,
            627,
            Request::Attach {
                selector: SessionSelector::ById(beta),
            },
        )
        .await;

        let detached = send_request(&mut follower, 628, Request::Detach).await;
        assert_eq!(detached, Response::Ok(ResponsePayload::Detached));

        let output_after_detach = send_request(
            &mut follower,
            629,
            Request::AttachOutput {
                session_id: alpha,
                max_bytes: 1024,
            },
        )
        .await;
        assert!(matches!(
            output_after_detach,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn server_stop_while_attached_cleans_runtime_state() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            90,
            Request::NewSession {
                name: Some("stop-attached".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let grant = match send_request(
            &mut client,
            91,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response: {other:?}"),
        };
        let opened = send_request(
            &mut client,
            92,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert_eq!(
            opened,
            Response::Ok(ResponsePayload::AttachReady {
                session_id,
                can_write: true,
            })
        );

        let stopper = connect_and_handshake(&endpoint).await;
        let mut stopper = stopper;
        let stopped = send_request(&mut stopper, 93, Request::ServerStop).await;
        assert_eq!(stopped, Response::Ok(ResponsePayload::ServerStopping));

        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        {
            let runtime_manager = server
                .state
                .session_runtimes
                .lock()
                .expect("runtime manager lock should succeed");
            assert_eq!(runtime_manager.runtime_count(), 0);
        }
        {
            let session_manager = server
                .state
                .session_manager
                .lock()
                .expect("session manager lock should succeed");
            assert_eq!(session_manager.session_count(), 0);
        }

        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attach_open_rejects_invalid_token() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            40,
            Request::NewSession {
                name: Some("bad-token".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let response = send_request(
            &mut client,
            41,
            Request::AttachOpen {
                session_id,
                attach_token: Uuid::new_v4(),
            },
        )
        .await;
        assert!(matches!(
            response,
            Response::Err(ErrorResponse {
                code: ErrorCode::NotFound,
                ..
            })
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attach_open_rejects_expired_token() {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let created = send_request(
            &mut client,
            50,
            Request::NewSession {
                name: Some("exp-token".to_string()),
            },
        )
        .await;
        let session_id = match created {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected create response: {other:?}"),
        };

        let attached = send_request(
            &mut client,
            51,
            Request::Attach {
                selector: SessionSelector::ById(session_id),
            },
        )
        .await;
        let grant = match attached {
            Response::Ok(ResponsePayload::Attached { grant }) => grant,
            other => panic!("unexpected attach response: {other:?}"),
        };

        {
            let mut token_manager = server
                .state
                .attach_tokens
                .lock()
                .expect("attach token manager lock should succeed");
            force_expire_attach_token(&mut token_manager, grant.attach_token);
        }

        let response = send_request(
            &mut client,
            52,
            Request::AttachOpen {
                session_id,
                attach_token: grant.attach_token,
            },
        )
        .await;
        assert!(matches!(
            response,
            Response::Err(ErrorResponse {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        stop_server(server, server_task, &socket_path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn server_stop_request_triggers_shutdown() {
        let (_server, endpoint, socket_path, server_task) = start_server().await;
        let mut client = connect_and_handshake(&endpoint).await;

        let response = send_request(&mut client, 30, Request::ServerStop).await;
        assert_eq!(response, Response::Ok(ResponsePayload::ServerStopping));

        server_task
            .await
            .expect("server task should join")
            .expect("server should stop gracefully");
        if Path::new(&socket_path).exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restores_sessions_windows_and_roles_from_snapshot() {
        let suffix = Uuid::new_v4().to_string();
        let root = std::path::PathBuf::from(format!("/tmp/bmxr-{}", &suffix[..8]));
        let paths = ConfigPaths::new(root.join("config"), root.join("runtime"), root.join("data"));
        paths.ensure_dirs().expect("paths should be created");

        let endpoint = IpcEndpoint::unix_socket(paths.server_socket());
        let (_server, server_task) = start_server_from_paths(&paths).await;

        let mut owner = connect_and_handshake(&endpoint).await;
        let mut member = connect_and_handshake(&endpoint).await;

        let session_id = match send_request(
            &mut owner,
            300,
            Request::NewSession {
                name: Some("persisted".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };

        let window_id = match send_request(
            &mut owner,
            301,
            Request::NewWindow {
                session: Some(SessionSelector::ById(session_id)),
                name: Some("extra".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::WindowCreated { id, .. }) => id,
            other => panic!("unexpected window create response: {other:?}"),
        };

        let member_id = match send_request(&mut member, 302, Request::WhoAmI).await {
            Response::Ok(ResponsePayload::ClientIdentity { id }) => id,
            other => panic!("unexpected whoami response: {other:?}"),
        };

        let granted = send_request(
            &mut owner,
            303,
            Request::GrantRole {
                session: SessionSelector::ById(session_id),
                client_id: member_id,
                role: SessionRole::Writer,
            },
        )
        .await;
        assert!(matches!(
            granted,
            Response::Ok(ResponsePayload::RoleGranted { .. })
        ));

        let stopped = send_request(&mut owner, 304, Request::ServerStop).await;
        assert_eq!(stopped, Response::Ok(ResponsePayload::ServerStopping));
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        if Path::new(paths.server_socket().as_path()).exists() {
            std::fs::remove_file(paths.server_socket()).expect("socket cleanup should succeed");
        }

        let (_restored_server, restored_task) = start_server_from_paths(&paths).await;

        let mut restored_client = connect_and_handshake(&endpoint).await;
        let sessions = send_request(&mut restored_client, 305, Request::ListSessions).await;
        let restored = match sessions {
            Response::Ok(ResponsePayload::SessionList { sessions }) => sessions,
            other => panic!("unexpected list sessions response: {other:?}"),
        };
        assert!(restored.iter().any(|s| s.id == session_id));

        let windows = send_request(
            &mut restored_client,
            306,
            Request::ListWindows {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await;
        match windows {
            Response::Ok(ResponsePayload::WindowList { windows }) => {
                assert!(windows.iter().any(|window| window.id == window_id));
            }
            other => panic!("unexpected list windows response: {other:?}"),
        }

        let permissions = send_request(
            &mut restored_client,
            307,
            Request::ListPermissions {
                session: SessionSelector::ById(session_id),
            },
        )
        .await;
        match permissions {
            Response::Ok(ResponsePayload::PermissionsList { permissions, .. }) => {
                assert!(permissions.iter().any(|entry| {
                    entry.client_id == member_id && entry.role == SessionRole::Writer
                }));
            }
            other => panic!("unexpected list permissions response: {other:?}"),
        }

        let stop_restored = send_request(&mut restored_client, 308, Request::ServerStop).await;
        assert_eq!(stop_restored, Response::Ok(ResponsePayload::ServerStopping));
        restored_task
            .await
            .expect("restored server task should join")
            .expect("restored server should stop cleanly");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restores_multi_pane_layout_shape_across_restart() {
        let suffix = Uuid::new_v4().to_string();
        let root = std::path::PathBuf::from(format!("/tmp/bmxr-{}", &suffix[..8]));
        let paths = ConfigPaths::new(root.join("config"), root.join("runtime"), root.join("data"));
        paths.ensure_dirs().expect("paths should be created");

        let endpoint = IpcEndpoint::unix_socket(paths.server_socket());
        let (_server, server_task) = start_server_from_paths(&paths).await;

        let mut client = connect_and_handshake(&endpoint).await;
        let session_id = match send_request(
            &mut client,
            360,
            Request::NewSession {
                name: Some("layout-shape".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected session create response: {other:?}"),
        };

        let pane_2 = match send_request(
            &mut client,
            361,
            Request::SplitPane {
                session: Some(SessionSelector::ById(session_id)),
                target: None,
                direction: PaneSplitDirection::Vertical,
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::PaneSplit { id, .. }) => id,
            other => panic!("unexpected first split response: {other:?}"),
        };

        let pane_3 = match send_request(
            &mut client,
            362,
            Request::SplitPane {
                session: Some(SessionSelector::ById(session_id)),
                target: Some(PaneSelector::ByIndex(1)),
                direction: PaneSplitDirection::Horizontal,
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::PaneSplit { id, .. }) => id,
            other => panic!("unexpected second split response: {other:?}"),
        };

        let focused = send_request(
            &mut client,
            363,
            Request::FocusPane {
                session: Some(SessionSelector::ById(session_id)),
                target: Some(PaneSelector::ById(pane_2)),
                direction: None,
            },
        )
        .await;
        assert!(matches!(
            focused,
            Response::Ok(ResponsePayload::PaneFocused { id, .. }) if id == pane_2
        ));

        let pane_4 = match send_request(
            &mut client,
            364,
            Request::SplitPane {
                session: Some(SessionSelector::ById(session_id)),
                target: None,
                direction: PaneSplitDirection::Horizontal,
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::PaneSplit { id, .. }) => id,
            other => panic!("unexpected third split response: {other:?}"),
        };

        let resized_left = send_request(
            &mut client,
            365,
            Request::ResizePane {
                session: Some(SessionSelector::ById(session_id)),
                target: Some(PaneSelector::ByIndex(1)),
                delta: -1,
            },
        )
        .await;
        assert!(matches!(
            resized_left,
            Response::Ok(ResponsePayload::PaneResized { .. })
        ));

        let resized_right = send_request(
            &mut client,
            366,
            Request::ResizePane {
                session: Some(SessionSelector::ById(session_id)),
                target: Some(PaneSelector::ById(pane_4)),
                delta: 1,
            },
        )
        .await;
        assert!(matches!(
            resized_right,
            Response::Ok(ResponsePayload::PaneResized { .. })
        ));

        let active_window_id = match send_request(
            &mut client,
            367,
            Request::ListWindows {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::WindowList { windows }) => windows
                .into_iter()
                .find(|window| window.active)
                .map(|window| window.id)
                .expect("active window should exist"),
            other => panic!("unexpected list windows response: {other:?}"),
        };

        let pane_order_before_restart = match send_request(
            &mut client,
            368,
            Request::ListPanes {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::PaneList { panes }) => {
                panes.into_iter().map(|pane| pane.id).collect::<Vec<_>>()
            }
            other => panic!("unexpected list panes response: {other:?}"),
        };

        let expected_order = vec![active_window_id, pane_3, pane_2, pane_4];
        assert_eq!(pane_order_before_restart, expected_order);

        let saved = send_request(&mut client, 369, Request::ServerSave).await;
        let snapshot_path = match saved {
            Response::Ok(ResponsePayload::ServerSnapshotSaved { path: Some(path) }) => path,
            other => panic!("unexpected save response: {other:?}"),
        };

        let snapshot_before: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&snapshot_path).expect("snapshot should exist"))
                .expect("snapshot json should decode");
        let layout_before = extract_window_layout(&snapshot_before, session_id, active_window_id);

        let stopped = send_request(&mut client, 370, Request::ServerStop).await;
        assert_eq!(stopped, Response::Ok(ResponsePayload::ServerStopping));
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        if Path::new(paths.server_socket().as_path()).exists() {
            std::fs::remove_file(paths.server_socket()).expect("socket cleanup should succeed");
        }

        let (_restored_server, restored_task) = start_server_from_paths(&paths).await;
        let mut restored_client = connect_and_handshake(&endpoint).await;

        let pane_order_after_restart = match send_request(
            &mut restored_client,
            371,
            Request::ListPanes {
                session: Some(SessionSelector::ById(session_id)),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::PaneList { panes }) => {
                panes.into_iter().map(|pane| pane.id).collect::<Vec<_>>()
            }
            other => panic!("unexpected restored list panes response: {other:?}"),
        };
        assert_eq!(pane_order_after_restart, expected_order);

        let resaved = send_request(&mut restored_client, 372, Request::ServerSave).await;
        assert!(matches!(
            resaved,
            Response::Ok(ResponsePayload::ServerSnapshotSaved { .. })
        ));

        let snapshot_after: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&snapshot_path).expect("resaved snapshot should exist"),
        )
        .expect("resaved snapshot json should decode");
        let layout_after = extract_window_layout(&snapshot_after, session_id, active_window_id);

        assert_eq!(layout_before, layout_after);

        let stop_restored = send_request(&mut restored_client, 373, Request::ServerStop).await;
        assert_eq!(stop_restored, Response::Ok(ResponsePayload::ServerStopping));
        restored_task
            .await
            .expect("restored server task should join")
            .expect("restored server should stop cleanly");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_apply_replaces_current_state() {
        let suffix = Uuid::new_v4().to_string();
        let root = std::path::PathBuf::from(format!("/tmp/bmxr-{}", &suffix[..8]));
        let paths = ConfigPaths::new(root.join("config"), root.join("runtime"), root.join("data"));
        paths.ensure_dirs().expect("paths should be created");

        let endpoint = IpcEndpoint::unix_socket(paths.server_socket());
        let (_server, server_task) = start_server_from_paths(&paths).await;

        let mut client = connect_and_handshake(&endpoint).await;
        let baseline_id = match send_request(
            &mut client,
            340,
            Request::NewSession {
                name: Some("baseline".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected baseline create response: {other:?}"),
        };

        let saved = send_request(&mut client, 341, Request::ServerSave).await;
        assert!(matches!(
            saved,
            Response::Ok(ResponsePayload::ServerSnapshotSaved { .. })
        ));

        let transient_id = match send_request(
            &mut client,
            342,
            Request::NewSession {
                name: Some("transient".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected transient create response: {other:?}"),
        };

        let restored = send_request(&mut client, 343, Request::ServerRestoreApply).await;
        assert!(matches!(
            restored,
            Response::Ok(ResponsePayload::ServerSnapshotRestored { sessions, .. }) if sessions >= 1
        ));

        let sessions = send_request(&mut client, 344, Request::ListSessions).await;
        let sessions = match sessions {
            Response::Ok(ResponsePayload::SessionList { sessions }) => sessions,
            other => panic!("unexpected session list response: {other:?}"),
        };
        assert!(sessions.iter().any(|session| session.id == baseline_id));
        assert!(!sessions.iter().any(|session| session.id == transient_id));

        let stopped = send_request(&mut client, 345, Request::ServerStop).await;
        assert_eq!(stopped, Response::Ok(ResponsePayload::ServerStopping));
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_dry_run_reports_checksum_failure() {
        let suffix = Uuid::new_v4().to_string();
        let root = std::path::PathBuf::from(format!("/tmp/bmxr-{}", &suffix[..8]));
        let paths = ConfigPaths::new(root.join("config"), root.join("runtime"), root.join("data"));
        paths.ensure_dirs().expect("paths should be created");

        let endpoint = IpcEndpoint::unix_socket(paths.server_socket());
        let (_server, server_task) = start_server_from_paths(&paths).await;
        let mut client = connect_and_handshake(&endpoint).await;

        let _ = send_request(
            &mut client,
            350,
            Request::NewSession {
                name: Some("checksum".to_string()),
            },
        )
        .await;
        let saved = send_request(&mut client, 351, Request::ServerSave).await;
        let snapshot_path = match saved {
            Response::Ok(ResponsePayload::ServerSnapshotSaved { path: Some(path) }) => path,
            other => panic!("unexpected save response: {other:?}"),
        };

        let mut payload: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&snapshot_path).expect("snapshot file should exist"),
        )
        .expect("snapshot json should decode");
        let checksum = payload["checksum"]
            .as_u64()
            .expect("checksum field should be u64");
        payload["checksum"] = serde_json::json!(checksum.wrapping_add(1));
        std::fs::write(
            &snapshot_path,
            serde_json::to_vec_pretty(&payload).expect("snapshot json should encode"),
        )
        .expect("tampered snapshot should write");

        let dry_run = send_request(&mut client, 352, Request::ServerRestoreDryRun).await;
        assert!(matches!(
            dry_run,
            Response::Ok(ResponsePayload::ServerSnapshotRestoreDryRun { ok: false, message })
                if message.contains("checksum")
        ));

        let stopped = send_request(&mut client, 353, Request::ServerStop).await;
        assert_eq!(stopped, Response::Ok(ResponsePayload::ServerStopping));
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_rejects_layout_root_with_missing_pane_leaf() {
        let suffix = Uuid::new_v4().to_string();
        let root = std::path::PathBuf::from(format!("/tmp/bmxr-{}", &suffix[..8]));
        let paths = ConfigPaths::new(root.join("config"), root.join("runtime"), root.join("data"));
        paths.ensure_dirs().expect("paths should be created");

        let endpoint = IpcEndpoint::unix_socket(paths.server_socket());
        let (_server, server_task) = start_server_from_paths(&paths).await;
        let mut client = connect_and_handshake(&endpoint).await;

        let session_id = match send_request(
            &mut client,
            354,
            Request::NewSession {
                name: Some("layout-invalid".to_string()),
            },
        )
        .await
        {
            Response::Ok(ResponsePayload::SessionCreated { id, .. }) => id,
            other => panic!("unexpected new session response: {other:?}"),
        };

        let split = send_request(
            &mut client,
            355,
            Request::SplitPane {
                session: Some(SessionSelector::ById(session_id)),
                target: None,
                direction: PaneSplitDirection::Vertical,
            },
        )
        .await;
        assert!(matches!(
            split,
            Response::Ok(ResponsePayload::PaneSplit { .. })
        ));

        let saved = send_request(&mut client, 356, Request::ServerSave).await;
        let snapshot_path = match saved {
            Response::Ok(ResponsePayload::ServerSnapshotSaved { path: Some(path) }) => path,
            other => panic!("unexpected save response: {other:?}"),
        };

        let mut payload: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&snapshot_path).expect("snapshot file should exist"),
        )
        .expect("snapshot json should decode");

        payload["snapshot"]["sessions"][0]["windows"][0]["layout_root"] = serde_json::json!({
            "kind": "leaf",
            "pane_id": Uuid::new_v4(),
        });

        let snapshot_model: super::SnapshotV3 =
            serde_json::from_value(payload["snapshot"].clone()).expect("snapshot model decode");
        let snapshot_bytes =
            serde_json::to_vec(&snapshot_model).expect("snapshot json should encode");
        let checksum = {
            let mut hash = 0xcbf29ce484222325u64;
            for byte in snapshot_bytes {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            hash
        };
        payload["checksum"] = serde_json::json!(checksum);

        std::fs::write(
            &snapshot_path,
            serde_json::to_vec_pretty(&payload).expect("snapshot json should encode"),
        )
        .expect("tampered snapshot should write");

        let dry_run = send_request(&mut client, 357, Request::ServerRestoreDryRun).await;
        match dry_run {
            Response::Ok(ResponsePayload::ServerSnapshotRestoreDryRun { ok, message }) => {
                assert!(!ok, "dry-run should fail for malformed layout snapshot");
                assert!(
                    message.contains("layout") || message.contains("pane set"),
                    "unexpected dry-run failure message: {message}"
                );
            }
            other => panic!("unexpected dry-run response: {other:?}"),
        }

        let apply = send_request(&mut client, 358, Request::ServerRestoreApply).await;
        match apply {
            Response::Err(ErrorResponse { code, message }) => {
                assert_eq!(code, ErrorCode::InvalidRequest);
                assert!(
                    message.contains("layout") || message.contains("pane set"),
                    "unexpected restore apply message: {message}"
                );
            }
            other => panic!("unexpected restore apply response: {other:?}"),
        }

        let stopped = send_request(&mut client, 359, Request::ServerStop).await;
        assert_eq!(stopped, Response::Ok(ResponsePayload::ServerStopping));
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");
    }

    #[cfg(unix)]
    async fn start_server() -> (
        BmuxServer,
        IpcEndpoint,
        std::path::PathBuf,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let socket_path = std::env::temp_dir().join(format!("bmux-server-{}.sock", Uuid::new_v4()));
        let endpoint = IpcEndpoint::unix_socket(&socket_path);
        let server = BmuxServer::new(endpoint.clone());
        let server_task = spawn_server_with_ready(server.clone()).await;
        (server, endpoint, socket_path, server_task)
    }

    #[cfg(unix)]
    async fn start_server_with_shell(
        shell: &std::path::Path,
    ) -> (
        BmuxServer,
        IpcEndpoint,
        std::path::PathBuf,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let (server, endpoint, socket_path, server_task) = start_server().await;
        let mut runtime_manager = server
            .state
            .session_runtimes
            .lock()
            .expect("session runtime manager lock should not be poisoned");
        runtime_manager.shell = shell.display().to_string();
        drop(runtime_manager);
        (server, endpoint, socket_path, server_task)
    }

    #[cfg(unix)]
    async fn start_server_from_paths(
        paths: &ConfigPaths,
    ) -> (BmuxServer, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let server = BmuxServer::from_config_paths(paths);
        let server_task = spawn_server_with_ready(server.clone()).await;
        (server, server_task)
    }

    #[cfg(unix)]
    async fn connect_and_handshake(endpoint: &IpcEndpoint) -> LocalIpcStream {
        connect_and_handshake_with_principal(endpoint, Uuid::new_v4()).await
    }

    #[cfg(unix)]
    fn write_test_shell_script(name: &str, contents: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!("bmux-server-{name}-{}.sh", Uuid::new_v4()));
        std::fs::write(&path, contents).expect("test shell script should be written");
        let mut permissions = std::fs::metadata(&path)
            .expect("test shell script metadata should exist")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions)
            .expect("test shell script should be executable");
        path
    }

    #[cfg(unix)]
    async fn connect_and_handshake_with_principal(
        endpoint: &IpcEndpoint,
        principal_id: Uuid,
    ) -> LocalIpcStream {
        let mut client = LocalIpcStream::connect(endpoint)
            .await
            .expect("client should connect");
        let hello_payload = encode(&Request::Hello {
            protocol_version: ProtocolVersion::current(),
            client_name: "test-client".to_string(),
            principal_id,
        })
        .expect("hello should encode");
        let hello = Envelope::new(1, EnvelopeKind::Request, hello_payload);
        client
            .send_envelope(&hello)
            .await
            .expect("hello send should succeed");
        let reply = client
            .recv_envelope()
            .await
            .expect("hello reply should be received");
        let response: Response = decode(&reply.payload).expect("response should decode");
        assert!(matches!(
            response,
            Response::Ok(ResponsePayload::ServerStatus { running: true, .. })
        ));
        client
    }

    #[cfg(unix)]
    async fn send_request(
        client: &mut LocalIpcStream,
        request_id: u64,
        request: Request,
    ) -> Response {
        let request_debug = format!("{request:?}");
        let payload = encode(&request).expect("request should encode");
        let envelope = Envelope::new(request_id, EnvelopeKind::Request, payload);
        tokio::time::timeout(Duration::from_secs(5), client.send_envelope(&envelope))
            .await
            .unwrap_or_else(|_| {
                panic!("timed out sending request {request_id} ({request_debug}) to test server")
            })
            .expect("request send should succeed");
        let reply = tokio::time::timeout(Duration::from_secs(5), client.recv_envelope())
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for reply to request {request_id} ({request_debug})")
            })
            .expect("request reply should be received");
        assert_eq!(reply.request_id, request_id);
        decode(&reply.payload).expect("response decode should succeed")
    }

    #[cfg(unix)]
    fn extract_window_layout(
        snapshot_envelope: &serde_json::Value,
        session_id: Uuid,
        window_id: Uuid,
    ) -> serde_json::Value {
        let sessions = snapshot_envelope["snapshot"]["sessions"]
            .as_array()
            .expect("snapshot sessions should be array");
        let session = sessions
            .iter()
            .find(|session| session["id"] == serde_json::json!(session_id))
            .expect("session should exist in snapshot");
        let windows = session["windows"]
            .as_array()
            .expect("session windows should be array");
        windows
            .iter()
            .find(|window| window["id"] == serde_json::json!(window_id))
            .and_then(|window| window.get("layout_root"))
            .cloned()
            .expect("window layout_root should exist")
    }

    #[cfg(unix)]
    async fn poll_events_collect(
        client: &mut LocalIpcStream,
        request_id_base: u64,
        max_events: usize,
        attempts: usize,
    ) -> Vec<Event> {
        let mut all_events = Vec::new();
        for idx in 0..attempts.max(1) {
            let response = send_request(
                client,
                request_id_base + idx as u64,
                Request::PollEvents { max_events },
            )
            .await;
            if let Response::Ok(ResponsePayload::EventBatch { events }) = response
                && !events.is_empty()
            {
                all_events.extend(events);
            }
            sleep(Duration::from_millis(25)).await;
        }
        all_events
    }

    #[cfg(unix)]
    async fn discover_client_id_from_window_switch(
        observer: &mut LocalIpcStream,
        actor: &mut LocalIpcStream,
        session_id: Uuid,
        request_id_base: u64,
    ) -> Uuid {
        let switched = send_request(
            actor,
            request_id_base,
            Request::SwitchWindow {
                session: Some(SessionSelector::ById(session_id)),
                target: WindowSelector::Active,
            },
        )
        .await;
        assert!(matches!(
            switched,
            Response::Ok(ResponsePayload::WindowSwitched {
                session_id: switched_session,
                ..
            }) if switched_session == session_id
        ));

        let events = poll_events_collect(observer, request_id_base + 1, 10, 6).await;
        events
            .iter()
            .find_map(|event| match event {
                Event::WindowSwitched {
                    session_id: switched_session,
                    by_client_id,
                    ..
                } if *switched_session == session_id => Some(*by_client_id),
                _ => None,
            })
            .expect("window switched event with client id should exist")
    }

    #[cfg(unix)]
    async fn collect_attach_output_until(
        client: &mut LocalIpcStream,
        session_id: uuid::Uuid,
        needle: &str,
        attempts: usize,
    ) -> String {
        let poll_limit = attempts.max(1).saturating_mul(10);
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let mut collected = String::new();
            let mut idx = 0usize;
            while idx < poll_limit {
                let response = send_request(
                    client,
                    4000 + idx as u64,
                    Request::AttachOutput {
                        session_id,
                        max_bytes: 8192,
                    },
                )
                .await;
                if let Response::Ok(ResponsePayload::AttachOutput { data }) = response
                    && !data.is_empty()
                {
                    collected.push_str(&String::from_utf8_lossy(&data));
                    if collected.contains(needle) {
                        return collected;
                    }
                }
                idx += 1;
                sleep(Duration::from_millis(25)).await;
            }
            collected
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out collecting attach output for session {session_id} while waiting for '{needle}'"
            )
        });

        assert!(
            result.contains(needle),
            "attach output did not contain '{needle}' after {poll_limit} polls: {:?}",
            result
        );
        result
    }

    #[cfg(unix)]
    async fn reap_focused_attached_pane(server: &BmuxServer, session_id: Uuid) -> Uuid {
        let pane_id = {
            let runtime_manager = server
                .state
                .session_runtimes
                .lock()
                .expect("session runtime manager lock should not be poisoned");
            let runtime = runtime_manager
                .runtimes
                .get(&SessionId(session_id))
                .expect("session runtime should exist");
            let window = runtime
                .windows
                .get(&runtime.active_window)
                .expect("active window should exist");
            assert!(
                !runtime.attached_clients.is_empty(),
                "session should have attached clients before deterministic pane reap"
            );
            window.focused_pane_id
        };

        reap_exited_pane(&server.state, SessionId(session_id), pane_id)
            .await
            .expect("focused attached pane reap should succeed");
        pane_id
    }

    #[cfg(unix)]
    async fn stop_server(
        server: BmuxServer,
        server_task: tokio::task::JoinHandle<anyhow::Result<()>>,
        socket_path: &std::path::Path,
    ) {
        server.request_shutdown();
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for server task shutdown"))
            .expect("server task should join")
            .expect("server should shut down cleanly");
        if socket_path.exists() {
            std::fs::remove_file(socket_path).expect("socket cleanup should succeed");
        }
    }

    fn force_expire_attach_token(token_manager: &mut super::AttachTokenManager, token: Uuid) {
        if let Some(entry) = token_manager.tokens.get_mut(&token) {
            entry.expires_at = std::time::Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap();
        }
    }
}
