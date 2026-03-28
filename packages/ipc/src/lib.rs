#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Cross-platform IPC protocol models for bmux.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub mod frame;
pub mod transport;

/// Cross-platform local IPC endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", content = "address", rename_all = "snake_case")]
pub enum IpcEndpoint {
    UnixSocket(PathBuf),
    WindowsNamedPipe(String),
}

impl IpcEndpoint {
    /// Construct a Unix domain socket endpoint.
    #[must_use]
    pub fn unix_socket(path: impl Into<PathBuf>) -> Self {
        Self::UnixSocket(path.into())
    }

    /// Construct a Windows named pipe endpoint.
    #[must_use]
    pub fn windows_named_pipe(name: impl Into<String>) -> Self {
        Self::WindowsNamedPipe(name.into())
    }

    /// Return the Unix socket path when this endpoint uses Unix sockets.
    #[must_use]
    pub fn as_unix_socket(&self) -> Option<&Path> {
        match self {
            Self::UnixSocket(path) => Some(path.as_path()),
            Self::WindowsNamedPipe(_) => None,
        }
    }

    /// Return the Windows named pipe when this endpoint uses named pipes.
    #[must_use]
    pub const fn as_windows_named_pipe(&self) -> Option<&str> {
        match self {
            Self::UnixSocket(_) => None,
            Self::WindowsNamedPipe(name) => Some(name.as_str()),
        }
    }
}

/// Current IPC protocol version.
pub const CURRENT_PROTOCOL_VERSION: u16 = 3;

/// Protocol version used in IPC envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion(pub u16);

impl ProtocolVersion {
    /// The currently supported protocol version.
    #[must_use]
    pub const fn current() -> Self {
        Self(CURRENT_PROTOCOL_VERSION)
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::current()
    }
}

/// Envelope discriminant for payload interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeKind {
    Request,
    Response,
    Event,
}

/// Versioned IPC envelope with request correlation support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub version: ProtocolVersion,
    pub request_id: u64,
    pub kind: EnvelopeKind,
    pub payload: Vec<u8>,
}

impl Envelope {
    /// Build a new envelope.
    #[must_use]
    pub const fn new(request_id: u64, kind: EnvelopeKind, payload: Vec<u8>) -> Self {
        Self {
            version: ProtocolVersion::current(),
            request_id,
            kind,
            payload,
        }
    }
}

/// Session selector accepted by commands and protocol requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSelector {
    ById(Uuid),
    ByName(String),
}

/// Generic context selector accepted by context protocol requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextSelector {
    ById(Uuid),
    ByName(String),
}

/// Pane selector accepted by commands and protocol requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneSelector {
    ById(Uuid),
    ByIndex(u32),
    Active,
}

/// Generic service invocation kind for plugin-dispatched RPC calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvokeServiceKind {
    Query,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSplitDirection {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneFocusDirection {
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneLayoutNode {
    Leaf {
        pane_id: Uuid,
    },
    Split {
        direction: PaneSplitDirection,
        ratio_percent: u8,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachViewComponent {
    Scene,
    SurfaceContent,
    Layout,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachLayer {
    Status,
    Pane,
    Overlay,
    FloatingPane,
    Tooltip,
    Cursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachFocusTarget {
    None,
    Pane { pane_id: Uuid },
    Surface { surface_id: Uuid },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachSurfaceKind {
    Pane,
    FloatingPane,
    Modal,
    Overlay,
    Tooltip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachSurface {
    pub id: Uuid,
    pub kind: AttachSurfaceKind,
    pub layer: AttachLayer,
    pub z: i32,
    pub rect: AttachRect,
    pub opaque: bool,
    pub visible: bool,
    pub accepts_input: bool,
    pub cursor_owner: bool,
    pub pane_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachScene {
    pub session_id: Uuid,
    pub focus: AttachFocusTarget,
    pub surfaces: Vec<AttachSurface>,
}

/// Request payload variants for client/server IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol_version: ProtocolVersion,
        client_name: String,
        principal_id: Uuid,
    },
    Ping,
    WhoAmI,
    WhoAmIPrincipal,
    ServerStatus,
    ServerSave,
    ServerRestoreDryRun,
    ServerRestoreApply,
    ServerStop,
    InvokeService {
        capability: String,
        kind: InvokeServiceKind,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    },
    NewSession {
        name: Option<String>,
    },
    ListSessions,
    ListClients,
    CreateContext {
        name: Option<String>,
        #[serde(default)]
        attributes: BTreeMap<String, String>,
    },
    ListContexts,
    SelectContext {
        selector: ContextSelector,
    },
    CloseContext {
        selector: ContextSelector,
        force: bool,
    },
    CurrentContext,
    KillSession {
        selector: SessionSelector,
        force_local: bool,
    },
    SplitPane {
        session: Option<SessionSelector>,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
    },
    FocusPane {
        session: Option<SessionSelector>,
        target: Option<PaneSelector>,
        direction: Option<PaneFocusDirection>,
    },
    ResizePane {
        session: Option<SessionSelector>,
        target: Option<PaneSelector>,
        delta: i16,
    },
    ClosePane {
        session: Option<SessionSelector>,
        target: Option<PaneSelector>,
    },
    ListPanes {
        session: Option<SessionSelector>,
    },
    FollowClient {
        target_client_id: Uuid,
        global: bool,
    },
    Unfollow,
    Attach {
        selector: SessionSelector,
    },
    AttachContext {
        selector: ContextSelector,
    },
    AttachOpen {
        session_id: Uuid,
        attach_token: Uuid,
    },
    AttachInput {
        session_id: Uuid,
        data: Vec<u8>,
    },
    AttachSetViewport {
        session_id: Uuid,
        cols: u16,
        rows: u16,
    },
    AttachOutput {
        session_id: Uuid,
        max_bytes: usize,
    },
    AttachLayout {
        session_id: Uuid,
    },
    AttachPaneOutputBatch {
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    },
    AttachSnapshot {
        session_id: Uuid,
        max_bytes_per_pane: usize,
    },
    SubscribeEvents,
    PollEvents {
        max_events: usize,
    },
    RecordingStart {
        #[serde(default)]
        session_id: Option<Uuid>,
        capture_input: bool,
        #[serde(default)]
        profile: Option<RecordingProfile>,
        #[serde(default)]
        event_kinds: Option<Vec<RecordingEventKind>>,
    },
    RecordingStop {
        #[serde(default)]
        recording_id: Option<Uuid>,
    },
    RecordingStatus,
    RecordingList,
    RecordingDelete {
        recording_id: Uuid,
    },
    RecordingWriteCustomEvent {
        #[serde(default)]
        session_id: Option<Uuid>,
        #[serde(default)]
        pane_id: Option<Uuid>,
        source: String,
        name: String,
        /// Pre-serialized JSON payload bytes.
        payload: Vec<u8>,
    },
    RecordingDeleteAll,
    Detach,
}

/// Attach grant returned by attach control-plane request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachGrant {
    #[serde(default)]
    pub context_id: Option<Uuid>,
    pub session_id: Uuid,
    pub attach_token: Uuid,
    pub expires_at_epoch_ms: u64,
}

/// Summary returned when listing sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub name: Option<String>,
    pub client_count: usize,
}

/// Summary returned when listing generic runtime contexts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSummary {
    pub id: Uuid,
    pub name: Option<String>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

/// Summary returned when listing panes in the active session runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSummary {
    pub id: Uuid,
    pub index: u32,
    pub name: Option<String>,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPaneChunk {
    pub pane_id: Uuid,
    pub data: Vec<u8>,
}

/// Summary returned when listing connected clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSummary {
    pub id: Uuid,
    #[serde(default)]
    pub selected_context_id: Option<Uuid>,
    pub selected_session_id: Option<Uuid>,
    pub following_client_id: Option<Uuid>,
    pub following_global: bool,
}

/// Snapshot persistence status returned by server-status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSnapshotStatus {
    pub enabled: bool,
    pub path: Option<String>,
    pub snapshot_exists: bool,
    pub last_write_epoch_ms: Option<u64>,
    pub last_restore_epoch_ms: Option<u64>,
    pub last_restore_error: Option<String>,
}

/// Recording summary returned by recording APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingSummary {
    pub id: Uuid,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    pub capture_input: bool,
    #[serde(default = "recording_profile_default")]
    pub profile: RecordingProfile,
    #[serde(default = "recording_event_kinds_default")]
    pub event_kinds: Vec<RecordingEventKind>,
    pub started_epoch_ms: u64,
    #[serde(default)]
    pub ended_epoch_ms: Option<u64>,
    pub event_count: u64,
    pub payload_bytes: u64,
    pub path: String,
}

const fn recording_profile_default() -> RecordingProfile {
    RecordingProfile::Full
}

fn recording_event_kinds_default() -> Vec<RecordingEventKind> {
    vec![
        RecordingEventKind::PaneInputRaw,
        RecordingEventKind::PaneOutputRaw,
        RecordingEventKind::ProtocolReplyRaw,
        RecordingEventKind::ServerEvent,
        RecordingEventKind::RequestStart,
        RecordingEventKind::RequestDone,
        RecordingEventKind::RequestError,
    ]
}

/// Recording profile used to choose event verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingProfile {
    Full,
    Functional,
    Visual,
}

/// Recording runtime status details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub active: Option<RecordingSummary>,
    pub queue_len: usize,
}

/// Event kind emitted into a recording timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingEventKind {
    PaneInputRaw,
    PaneOutputRaw,
    ProtocolReplyRaw,
    ServerEvent,
    RequestStart,
    RequestDone,
    RequestError,
    Custom,
}

/// Recording event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingPayload {
    Bytes {
        data: Vec<u8>,
    },
    ServerEvent {
        event: Event,
    },
    RequestStart {
        request_id: u64,
        request_kind: String,
        exclusive: bool,
        /// Full request, postcard-encoded.
        request_data: Vec<u8>,
    },
    RequestDone {
        request_id: u64,
        request_kind: String,
        response_kind: String,
        elapsed_ms: u64,
        /// Full request, postcard-encoded.
        request_data: Vec<u8>,
        /// Full response payload, postcard-encoded.
        response_data: Vec<u8>,
    },
    RequestError {
        request_id: u64,
        request_kind: String,
        error_code: ErrorCode,
        message: String,
        elapsed_ms: u64,
    },
    Custom {
        source: String,
        name: String,
        /// Pre-serialized JSON payload bytes.
        payload: Vec<u8>,
    },
}

/// Timeline event envelope persisted in recordings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingEventEnvelope {
    pub seq: u64,
    pub mono_ns: u64,
    pub wall_epoch_ms: u64,
    pub session_id: Option<Uuid>,
    pub pane_id: Option<Uuid>,
    pub client_id: Option<Uuid>,
    pub kind: RecordingEventKind,
    pub payload: RecordingPayload,
}

/// Successful response payload variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePayload {
    Pong,
    ClientIdentity {
        id: Uuid,
    },
    PrincipalIdentity {
        principal_id: Uuid,
        server_control_principal_id: Uuid,
        force_local_permitted: bool,
    },
    ServerStatus {
        running: bool,
        snapshot: ServerSnapshotStatus,
        principal_id: Uuid,
        server_control_principal_id: Uuid,
    },
    ServerSnapshotSaved {
        path: Option<String>,
    },
    ServerSnapshotRestoreDryRun {
        ok: bool,
        message: String,
    },
    ServerSnapshotRestored {
        sessions: usize,
        follows: usize,
        selected_sessions: usize,
    },
    SessionCreated {
        id: Uuid,
        name: Option<String>,
    },
    SessionList {
        sessions: Vec<SessionSummary>,
    },
    ClientList {
        clients: Vec<ClientSummary>,
    },
    ContextCreated {
        context: ContextSummary,
    },
    ContextList {
        contexts: Vec<ContextSummary>,
    },
    ContextSelected {
        context: ContextSummary,
    },
    ContextClosed {
        id: Uuid,
    },
    CurrentContext {
        context: Option<ContextSummary>,
    },
    SessionKilled {
        id: Uuid,
    },
    PaneSplit {
        id: Uuid,
        session_id: Uuid,
    },
    PaneFocused {
        id: Uuid,
        session_id: Uuid,
    },
    PaneResized {
        session_id: Uuid,
    },
    PaneClosed {
        id: Uuid,
        session_id: Uuid,
        session_closed: bool,
    },
    PaneList {
        panes: Vec<PaneSummary>,
    },
    FollowStarted {
        follower_client_id: Uuid,
        leader_client_id: Uuid,
        global: bool,
    },
    FollowStopped {
        follower_client_id: Uuid,
    },
    Attached {
        grant: AttachGrant,
    },
    AttachReady {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        can_write: bool,
    },
    AttachInputAccepted {
        bytes: usize,
    },
    AttachViewportSet {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    },
    AttachOutput {
        data: Vec<u8>,
    },
    AttachLayout {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        focused_pane_id: Uuid,
        panes: Vec<PaneSummary>,
        layout_root: PaneLayoutNode,
        scene: AttachScene,
    },
    AttachPaneOutputBatch {
        chunks: Vec<AttachPaneChunk>,
    },
    AttachSnapshot {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        focused_pane_id: Uuid,
        panes: Vec<PaneSummary>,
        layout_root: PaneLayoutNode,
        scene: AttachScene,
        chunks: Vec<AttachPaneChunk>,
    },
    EventsSubscribed,
    EventBatch {
        events: Vec<Event>,
    },
    RecordingStarted {
        recording: RecordingSummary,
    },
    RecordingStopped {
        recording_id: Uuid,
    },
    RecordingStatus {
        status: RecordingStatus,
    },
    RecordingList {
        recordings: Vec<RecordingSummary>,
    },
    RecordingDeleted {
        recording_id: Uuid,
    },
    RecordingCustomEventWritten {
        accepted: bool,
    },
    RecordingDeleteAll {
        deleted_count: usize,
    },
    Detached,
    ServerStopping,
    ServiceInvoked {
        payload: Vec<u8>,
    },
}

/// Canonical error codes returned over IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    AlreadyExists,
    InvalidRequest,
    VersionMismatch,
    Timeout,
    Internal,
}

/// Error details returned over IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub code: ErrorCode,
    pub message: String,
}

/// Top-level response message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(ResponsePayload),
    Err(ErrorResponse),
}

/// Event payload variants emitted by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    ServerStarted,
    ServerStopping,
    SessionCreated {
        id: Uuid,
        name: Option<String>,
    },
    SessionRemoved {
        id: Uuid,
    },
    ClientAttached {
        id: Uuid,
    },
    ClientDetached {
        id: Uuid,
    },
    FollowStarted {
        follower_client_id: Uuid,
        leader_client_id: Uuid,
        global: bool,
    },
    FollowStopped {
        follower_client_id: Uuid,
    },
    FollowTargetGone {
        follower_client_id: Uuid,
        former_leader_client_id: Uuid,
    },
    FollowTargetChanged {
        follower_client_id: Uuid,
        leader_client_id: Uuid,
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
    },
    AttachViewChanged {
        #[serde(default)]
        context_id: Option<Uuid>,
        session_id: Uuid,
        revision: u64,
        components: Vec<AttachViewComponent>,
    },
}

/// Serialize any protocol message using postcard.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode<T>(message: &T) -> Result<Vec<u8>, postcard::Error>
where
    T: Serialize,
{
    postcard::to_allocvec(message)
}

/// Deserialize any protocol message using postcard.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode<T>(bytes: &[u8]) -> Result<T, postcard::Error>
where
    T: DeserializeOwned,
{
    postcard::from_bytes(bytes)
}

// ── Shared display track types for recording files ───────────────────────────

/// Display track event — shared type used by both the attach runtime's
/// `DisplayCaptureWriter` and the playbook engine's `PlaybookDisplayTrackWriter`.
///
/// The `terminal_profile` field stores pre-serialized bytes (postcard-encoded
/// `DetectedTerminalProfile`) to avoid cross-crate type dependencies. Use `None`
/// when no terminal profile is available (e.g., headless playbook execution).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayTrackEvent {
    StreamOpened {
        client_id: Uuid,
        recording_id: Uuid,
        cell_width_px: Option<u16>,
        cell_height_px: Option<u16>,
        window_width_px: Option<u16>,
        window_height_px: Option<u16>,
        /// Pre-serialized terminal profile bytes (postcard-encoded), or `None`.
        terminal_profile: Option<Vec<u8>>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    FrameBytes {
        data: Vec<u8>,
    },
    StreamClosed,
}

/// Display track envelope — wraps an event with a monotonic timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayTrackEnvelope {
    pub mono_ns: u64,
    pub event: DisplayTrackEvent,
}

// ── Binary frame utilities for recording files ───────────────────────────────

/// Write a length-prefixed postcard frame to a writer.
///
/// Format: `[u32 little-endian length][postcard bytes]`
///
/// Returns an error if the serialized payload exceeds `u32::MAX` bytes.
pub fn write_frame<W: std::io::Write, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), Box<dyn std::error::Error>> {
    let bytes =
        postcard::to_allocvec(value).map_err(|e| format!("postcard serialize failed: {e}"))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| format!("frame too large: {} bytes exceeds u32::MAX", bytes.len()))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&bytes)?;
    Ok(())
}

/// Result of reading binary frames from a buffer.
pub struct ReadFramesResult<T> {
    /// Successfully deserialized frames.
    pub frames: Vec<T>,
    /// Number of trailing bytes that could not be parsed as a complete frame.
    /// Zero means a clean EOF with no leftover data.
    pub bytes_remaining: usize,
}

/// Read all length-prefixed postcard frames from a byte buffer.
///
/// Returns all successfully-parsed frames plus the count of any trailing bytes
/// that could not form a complete frame (indicating a truncated recording).
pub fn read_frames<T: DeserializeOwned>(
    data: &[u8],
) -> Result<ReadFramesResult<T>, Box<dyn std::error::Error>> {
    let mut results = Vec::new();
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if offset + len > data.len() {
            // Truncated frame — rewind to include the length prefix in remaining bytes.
            offset -= 4;
            break;
        }
        let value: T = postcard::from_bytes(&data[offset..offset + len])
            .map_err(|e| format!("postcard deserialize failed at offset {}: {e}", offset))?;
        results.push(value);
        offset += len;
    }
    Ok(ReadFramesResult {
        frames: results,
        bytes_remaining: data.len() - offset,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
        AttachViewComponent, Envelope, EnvelopeKind, ErrorCode, Event, IpcEndpoint,
        ProtocolVersion, Request, Response, ResponsePayload, SessionSelector, SessionSummary,
        decode, encode,
    };
    use std::path::Path;
    use uuid::Uuid;

    #[test]
    fn serializes_request_roundtrip() {
        let request = Request::KillSession {
            selector: SessionSelector::ByName("dev-shell".to_string()),
            force_local: false,
        };
        let bytes = encode(&request).expect("request should encode");
        let decoded: Request = decode(&bytes).expect("request should decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn serializes_response_roundtrip() {
        let response = Response::Ok(ResponsePayload::SessionList {
            sessions: vec![SessionSummary {
                id: Uuid::new_v4(),
                name: Some("work".to_string()),
                client_count: 1,
            }],
        });
        let bytes = encode(&response).expect("response should encode");
        let decoded: Response = decode(&bytes).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn serializes_attach_scene_response_roundtrip() {
        let pane_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let response = Response::Ok(ResponsePayload::AttachLayout {
            context_id: None,
            session_id,
            focused_pane_id: pane_id,
            panes: vec![],
            layout_root: super::PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 0,
                        y: 1,
                        w: 80,
                        h: 24,
                    },
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(pane_id),
                }],
            },
        });
        let bytes = encode(&response).expect("response should encode");
        let decoded: Response = decode(&bytes).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn serializes_event_roundtrip() {
        let event = Event::SessionCreated {
            id: Uuid::new_v4(),
            name: Some("ops".to_string()),
        };
        let bytes = encode(&event).expect("event should encode");
        let decoded: Event = decode(&bytes).expect("event should decode");
        assert_eq!(decoded, event);
    }

    #[test]
    fn serializes_attach_view_changed_roundtrip() {
        let event = Event::AttachViewChanged {
            context_id: None,
            session_id: Uuid::new_v4(),
            revision: 7,
            components: vec![AttachViewComponent::Layout, AttachViewComponent::Status],
        };
        let bytes = encode(&event).expect("event should encode");
        let decoded: Event = decode(&bytes).expect("event should decode");
        assert_eq!(decoded, event);
    }

    #[test]
    fn serializes_envelope_roundtrip() {
        let payload = encode(&Request::Ping).expect("payload should encode");
        let envelope = Envelope {
            version: ProtocolVersion::current(),
            request_id: 7,
            kind: EnvelopeKind::Request,
            payload,
        };
        let bytes = encode(&envelope).expect("envelope should encode");
        let decoded: Envelope = decode(&bytes).expect("envelope should decode");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn serializes_session_selector_by_id_roundtrip() {
        let selector = SessionSelector::ById(Uuid::new_v4());
        let bytes = encode(&selector).expect("selector should encode");
        let decoded: SessionSelector = decode(&bytes).expect("selector should decode");
        assert_eq!(decoded, selector);
    }

    #[test]
    fn protocol_version_defaults_to_current() {
        assert_eq!(ProtocolVersion::default(), ProtocolVersion::current());
    }

    #[test]
    fn error_code_serializes_roundtrip() {
        let code = ErrorCode::VersionMismatch;
        let bytes = encode(&code).expect("error code should encode");
        let decoded: ErrorCode = decode(&bytes).expect("error code should decode");
        assert_eq!(decoded, code);
    }

    #[test]
    fn endpoint_helpers_report_correct_transport() {
        let unix_endpoint = IpcEndpoint::unix_socket("/tmp/bmux.sock");
        assert_eq!(
            unix_endpoint.as_unix_socket(),
            Some(Path::new("/tmp/bmux.sock"))
        );
        assert_eq!(unix_endpoint.as_windows_named_pipe(), None);

        let pipe_endpoint = IpcEndpoint::windows_named_pipe(r"\\.\pipe\bmux-test");
        assert_eq!(pipe_endpoint.as_unix_socket(), None);
        assert_eq!(
            pipe_endpoint.as_windows_named_pipe(),
            Some(r"\\.\pipe\bmux-test")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoint_exposes_socket_path() {
        let endpoint = IpcEndpoint::unix_socket("/tmp/bmux.sock");
        assert_eq!(endpoint.as_unix_socket(), Some(Path::new("/tmp/bmux.sock")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_exposes_pipe_name() {
        let endpoint = IpcEndpoint::windows_named_pipe(r"\\.\pipe\bmux-test");
        assert_eq!(
            endpoint.as_windows_named_pipe(),
            Some(r"\\.\pipe\bmux-test")
        );
    }
}
