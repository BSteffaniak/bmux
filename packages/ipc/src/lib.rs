#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Cross-platform IPC protocol models for bmux.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
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
    pub fn as_windows_named_pipe(&self) -> Option<&str> {
        match self {
            Self::UnixSocket(_) => None,
            Self::WindowsNamedPipe(name) => Some(name.as_str()),
        }
    }
}

/// Current IPC protocol version.
pub const CURRENT_PROTOCOL_VERSION: u16 = 2;

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
    pub fn new(request_id: u64, kind: EnvelopeKind, payload: Vec<u8>) -> Self {
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

/// Window selector accepted by commands and protocol requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowSelector {
    ById(Uuid),
    ByName(String),
    Active,
}

/// Pane selector accepted by commands and protocol requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneSelector {
    ById(Uuid),
    ByIndex(u32),
    Active,
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
        first: Box<PaneLayoutNode>,
        second: Box<PaneLayoutNode>,
    },
}

/// Session role used for collaborative permission controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    Owner,
    Writer,
    Observer,
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
    NewSession {
        name: Option<String>,
    },
    NewWindow {
        session: Option<SessionSelector>,
        name: Option<String>,
    },
    ListSessions,
    ListClients,
    ListPermissions {
        session: SessionSelector,
    },
    ListWindows {
        session: Option<SessionSelector>,
    },
    GrantRole {
        session: SessionSelector,
        client_id: Uuid,
        role: SessionRole,
    },
    RevokeRole {
        session: SessionSelector,
        client_id: Uuid,
    },
    KillSession {
        selector: SessionSelector,
        force_local: bool,
    },
    KillWindow {
        session: Option<SessionSelector>,
        target: WindowSelector,
        force_local: bool,
    },
    SwitchWindow {
        session: Option<SessionSelector>,
        target: WindowSelector,
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
    SubscribeEvents,
    PollEvents {
        max_events: usize,
    },
    Detach,
}

/// Attach grant returned by attach control-plane request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachGrant {
    pub session_id: Uuid,
    pub attach_token: Uuid,
    pub expires_at_epoch_ms: u64,
}

/// Summary returned when listing sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub name: Option<String>,
    pub window_count: usize,
    pub client_count: usize,
}

/// Summary returned when listing windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSummary {
    pub id: Uuid,
    pub session_id: Uuid,
    pub name: Option<String>,
    pub active: bool,
}

/// Summary returned when listing panes in the active window.
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
    pub selected_session_id: Option<Uuid>,
    pub following_client_id: Option<Uuid>,
    pub following_global: bool,
    pub session_role: Option<SessionRole>,
}

/// Permission assignment summary for one client in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPermissionSummary {
    pub client_id: Uuid,
    pub role: SessionRole,
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
        server_owner_principal_id: Uuid,
        force_local_authorized: bool,
    },
    ServerStatus {
        running: bool,
        snapshot: ServerSnapshotStatus,
        principal_id: Uuid,
        server_owner_principal_id: Uuid,
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
        windows: usize,
        roles: usize,
        follows: usize,
        selected_sessions: usize,
    },
    SessionCreated {
        id: Uuid,
        name: Option<String>,
    },
    WindowCreated {
        id: Uuid,
        session_id: Uuid,
        name: Option<String>,
    },
    SessionList {
        sessions: Vec<SessionSummary>,
    },
    ClientList {
        clients: Vec<ClientSummary>,
    },
    PermissionsList {
        session_id: Uuid,
        permissions: Vec<SessionPermissionSummary>,
    },
    WindowList {
        windows: Vec<WindowSummary>,
    },
    SessionKilled {
        id: Uuid,
    },
    WindowKilled {
        id: Uuid,
        session_id: Uuid,
    },
    WindowSwitched {
        id: Uuid,
        session_id: Uuid,
    },
    PaneSplit {
        id: Uuid,
        session_id: Uuid,
        window_id: Uuid,
    },
    PaneFocused {
        id: Uuid,
        session_id: Uuid,
        window_id: Uuid,
    },
    PaneResized {
        session_id: Uuid,
        window_id: Uuid,
    },
    PaneClosed {
        id: Uuid,
        session_id: Uuid,
        window_id: Uuid,
        window_closed: bool,
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
    RoleGranted {
        session_id: Uuid,
        client_id: Uuid,
        role: SessionRole,
    },
    RoleRevoked {
        session_id: Uuid,
        client_id: Uuid,
        role: SessionRole,
    },
    Attached {
        grant: AttachGrant,
    },
    AttachReady {
        session_id: Uuid,
        can_write: bool,
    },
    AttachInputAccepted {
        bytes: usize,
    },
    AttachViewportSet {
        session_id: Uuid,
        cols: u16,
        rows: u16,
    },
    AttachOutput {
        data: Vec<u8>,
    },
    AttachLayout {
        session_id: Uuid,
        window_id: Uuid,
        focused_pane_id: Uuid,
        panes: Vec<PaneSummary>,
        layout_root: PaneLayoutNode,
    },
    AttachPaneOutputBatch {
        chunks: Vec<AttachPaneChunk>,
    },
    EventsSubscribed,
    EventBatch {
        events: Vec<Event>,
    },
    Detached,
    ServerStopping,
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
    WindowCreated {
        id: Uuid,
        session_id: Uuid,
        name: Option<String>,
    },
    WindowRemoved {
        id: Uuid,
        session_id: Uuid,
    },
    WindowSwitched {
        id: Uuid,
        session_id: Uuid,
        by_client_id: Uuid,
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
        session_id: Uuid,
    },
    RoleChanged {
        session_id: Uuid,
        client_id: Uuid,
        role: SessionRole,
        by_client_id: Uuid,
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

#[cfg(test)]
mod tests {
    use super::{
        Envelope, EnvelopeKind, ErrorCode, Event, IpcEndpoint, ProtocolVersion, Request, Response,
        ResponsePayload, SessionPermissionSummary, SessionRole, SessionSelector, SessionSummary,
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
                window_count: 2,
                client_count: 1,
            }],
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
    fn session_role_serializes_roundtrip() {
        let role = SessionRole::Writer;
        let bytes = encode(&role).expect("session role should encode");
        let decoded: SessionRole = decode(&bytes).expect("session role should decode");
        assert_eq!(decoded, role);
    }

    #[test]
    fn serializes_permissions_response_roundtrip() {
        let response = Response::Ok(ResponsePayload::PermissionsList {
            session_id: Uuid::new_v4(),
            permissions: vec![SessionPermissionSummary {
                client_id: Uuid::new_v4(),
                role: SessionRole::Owner,
            }],
        });
        let bytes = encode(&response).expect("permissions response should encode");
        let decoded: Response = decode(&bytes).expect("permissions response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn serializes_role_changed_event_roundtrip() {
        let event = Event::RoleChanged {
            session_id: Uuid::new_v4(),
            client_id: Uuid::new_v4(),
            role: SessionRole::Observer,
            by_client_id: Uuid::new_v4(),
        };
        let bytes = encode(&event).expect("role changed event should encode");
        let decoded: Event = decode(&bytes).expect("role changed event should decode");
        assert_eq!(decoded, event);
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
