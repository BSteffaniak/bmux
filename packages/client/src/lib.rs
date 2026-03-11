#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Client component for bmux terminal multiplexer.

use bmux_config::{BmuxConfig, ConfigPaths};
pub use bmux_ipc::Event as ServerEvent;
use bmux_ipc::transport::{IpcTransportError, LocalIpcStream};
use bmux_ipc::{
    AttachGrant, AttachPaneChunk, AttachScene, ClientSummary, Envelope, EnvelopeKind, ErrorCode,
    InvokeServiceKind, IpcEndpoint, PaneFocusDirection, PaneLayoutNode, PaneSelector,
    PaneSplitDirection, PaneSummary, ProtocolVersion, Request, Response, ResponsePayload,
    ServerSnapshotStatus, SessionPermissionSummary, SessionRole, SessionSelector, SessionSummary,
    WindowSelector, WindowSummary, decode, encode,
};
use std::time::Duration;
use thiserror::Error;
use tracing::debug;
use uuid::Uuid;

/// Result type for client operations.
pub type Result<T> = std::result::Result<T, ClientError>;

/// Details returned when opening an attach stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachOpenInfo {
    pub session_id: Uuid,
    pub can_write: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachLayoutState {
    pub session_id: Uuid,
    pub window_id: Uuid,
    pub focused_pane_id: Uuid,
    pub panes: Vec<PaneSummary>,
    pub layout_root: PaneLayoutNode,
    pub scene: AttachScene,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSnapshotState {
    pub session_id: Uuid,
    pub window_id: Uuid,
    pub focused_pane_id: Uuid,
    pub panes: Vec<PaneSummary>,
    pub layout_root: PaneLayoutNode,
    pub scene: AttachScene,
    pub chunks: Vec<AttachPaneChunk>,
}

/// Server status details returned by status RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatusInfo {
    pub running: bool,
    pub snapshot: ServerSnapshotStatus,
    pub principal_id: Uuid,
    pub server_owner_principal_id: Uuid,
}

/// Principal identity details returned by whoami-principal RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrincipalIdentityInfo {
    pub principal_id: Uuid,
    pub server_owner_principal_id: Uuid,
    pub force_local_authorized: bool,
}

/// Summary returned by apply-restore operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerRestoreSummary {
    pub sessions: usize,
    pub windows: usize,
    pub roles: usize,
    pub follows: usize,
    pub selected_sessions: usize,
}

/// Typed client errors.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("transport error: {0}")]
    Transport(#[from] IpcTransportError),
    #[error("serialization error: {0}")]
    Serialization(#[from] postcard::Error),
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("request id mismatch (expected {expected}, got {actual})")]
    RequestIdMismatch { expected: u64, actual: u64 },
    #[error("unexpected envelope kind: expected {expected:?}, got {actual:?}")]
    UnexpectedEnvelopeKind {
        expected: EnvelopeKind,
        actual: EnvelopeKind,
    },
    #[error("server returned error {code:?}: {message}")]
    ServerError { code: ErrorCode, message: String },
    #[error("unexpected response payload: {0}")]
    UnexpectedResponse(&'static str),
    #[error("failed loading config: {0}")]
    ConfigLoad(#[from] bmux_config::ConfigError),
    #[error("failed reading principal id file {path}: {source}")]
    PrincipalIdRead {
        path: String,
        source: std::io::Error,
    },
    #[error("failed writing principal id file {path}: {source}")]
    PrincipalIdWrite {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid principal id in {path}: {value}")]
    PrincipalIdParse { path: String, value: String },
}

/// Main client API for communicating with bmux server.
#[derive(Debug)]
pub struct BmuxClient {
    stream: LocalIpcStream,
    timeout: Duration,
    next_request_id: u64,
    principal_id: Uuid,
}

impl BmuxClient {
    /// Connect to a server endpoint and complete protocol handshake.
    ///
    /// # Errors
    ///
    /// Returns an error if connection or handshake fails.
    pub async fn connect(
        endpoint: &IpcEndpoint,
        timeout: Duration,
        client_name: impl Into<String>,
    ) -> Result<Self> {
        Self::connect_with_principal(endpoint, timeout, client_name, Uuid::new_v4()).await
    }

    /// Connect to a server endpoint and complete protocol handshake using a caller-provided
    /// principal identity.
    ///
    /// # Errors
    ///
    /// Returns an error if connection or handshake fails.
    pub async fn connect_with_principal(
        endpoint: &IpcEndpoint,
        timeout: Duration,
        client_name: impl Into<String>,
        principal_id: Uuid,
    ) -> Result<Self> {
        let stream = LocalIpcStream::connect(endpoint).await?;
        let mut client = Self {
            stream,
            timeout,
            next_request_id: 1,
            principal_id,
        };

        let hello_response = client
            .request(Request::Hello {
                protocol_version: ProtocolVersion::current(),
                client_name: client_name.into(),
                principal_id,
            })
            .await?;

        match hello_response {
            ResponsePayload::ServerStatus { running: true, .. } => Ok(client),
            _ => Err(ClientError::UnexpectedResponse(
                "handshake expected running server status",
            )),
        }
    }

    /// Connect using endpoint derived from provided config paths.
    ///
    /// # Errors
    ///
    /// Returns an error if connection or handshake fails.
    pub async fn connect_with_paths(
        paths: &ConfigPaths,
        client_name: impl Into<String>,
    ) -> Result<Self> {
        let timeout = Duration::from_millis(BmuxConfig::load()?.general.server_timeout.max(1));
        let endpoint = endpoint_from_paths(paths);
        let principal_id = load_or_create_principal_id(paths)?;
        Self::connect_with_principal(&endpoint, timeout, client_name, principal_id).await
    }

    /// Connect using default config paths.
    ///
    /// # Errors
    ///
    /// Returns an error if connection or handshake fails.
    pub async fn connect_default(client_name: impl Into<String>) -> Result<Self> {
        Self::connect_with_paths(&ConfigPaths::default(), client_name).await
    }

    /// Ping the server.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn ping(&mut self) -> Result<()> {
        match self.request(Request::Ping).await? {
            ResponsePayload::Pong => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected pong")),
        }
    }

    /// Return the server-assigned client UUID for this connection.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn whoami(&mut self) -> Result<Uuid> {
        match self.request(Request::WhoAmI).await? {
            ResponsePayload::ClientIdentity { id } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected client identity")),
        }
    }

    /// Return this connection's profile-scoped principal identity.
    #[must_use]
    pub const fn principal_id(&self) -> Uuid {
        self.principal_id
    }

    /// Return principal identity information for this client and server owner.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn whoami_principal(&mut self) -> Result<PrincipalIdentityInfo> {
        match self.request(Request::WhoAmIPrincipal).await? {
            ResponsePayload::PrincipalIdentity {
                principal_id,
                server_owner_principal_id,
                force_local_authorized,
            } => Ok(PrincipalIdentityInfo {
                principal_id,
                server_owner_principal_id,
                force_local_authorized,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected principal identity",
            )),
        }
    }

    /// Retrieve server status.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn server_status(&mut self) -> Result<ServerStatusInfo> {
        match self.request(Request::ServerStatus).await? {
            ResponsePayload::ServerStatus {
                running,
                snapshot,
                principal_id,
                server_owner_principal_id,
            } => Ok(ServerStatusInfo {
                running,
                snapshot,
                principal_id,
                server_owner_principal_id,
            }),
            _ => Err(ClientError::UnexpectedResponse("expected server status")),
        }
    }

    /// Invoke a generic service request over IPC.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails transport/protocol validation.
    pub async fn invoke_service_raw(
        &mut self,
        capability: impl Into<String>,
        kind: InvokeServiceKind,
        interface_id: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        match self
            .request(Request::InvokeService {
                capability: capability.into(),
                kind,
                interface_id: interface_id.into(),
                operation: operation.into(),
                payload,
            })
            .await?
        {
            ResponsePayload::ServiceInvoked { payload } => Ok(payload),
            _ => Err(ClientError::UnexpectedResponse("expected service invoked")),
        }
    }

    /// Execute a raw kernel request and return the full response envelope payload.
    ///
    /// # Errors
    ///
    /// Returns an error if transport/protocol validation fails.
    pub async fn request_raw(&mut self, request: Request) -> Result<Response> {
        let request_id = self.take_request_id();
        let payload = encode(&request)?;
        let envelope = Envelope::new(request_id, EnvelopeKind::Request, payload);

        tokio::time::timeout(self.timeout, self.stream.send_envelope(&envelope))
            .await
            .map_err(|_| ClientError::Timeout(self.timeout))??;

        let response_envelope = tokio::time::timeout(self.timeout, self.stream.recv_envelope())
            .await
            .map_err(|_| ClientError::Timeout(self.timeout))??;

        if response_envelope.request_id != request_id {
            return Err(ClientError::RequestIdMismatch {
                expected: request_id,
                actual: response_envelope.request_id,
            });
        }
        if response_envelope.kind != EnvelopeKind::Response {
            return Err(ClientError::UnexpectedEnvelopeKind {
                expected: EnvelopeKind::Response,
                actual: response_envelope.kind,
            });
        }

        decode(&response_envelope.payload).map_err(ClientError::from)
    }

    /// Return whether this principal can use force-local kill bypass.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn force_local_authorized(&mut self) -> Result<bool> {
        match self.request(Request::WhoAmIPrincipal).await? {
            ResponsePayload::PrincipalIdentity {
                force_local_authorized,
                ..
            } => Ok(force_local_authorized),
            _ => Err(ClientError::UnexpectedResponse(
                "expected principal identity",
            )),
        }
    }

    /// Trigger immediate server snapshot save.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn server_save(&mut self) -> Result<Option<String>> {
        match self.request(Request::ServerSave).await? {
            ResponsePayload::ServerSnapshotSaved { path } => Ok(path),
            _ => Err(ClientError::UnexpectedResponse(
                "expected server snapshot saved",
            )),
        }
    }

    /// Validate snapshot readability and schema without mutating runtime state.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn server_restore_dry_run(&mut self) -> Result<(bool, String)> {
        match self.request(Request::ServerRestoreDryRun).await? {
            ResponsePayload::ServerSnapshotRestoreDryRun { ok, message } => Ok((ok, message)),
            _ => Err(ClientError::UnexpectedResponse(
                "expected server snapshot restore dry-run",
            )),
        }
    }

    /// Apply snapshot restore, replacing current in-memory server state.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn server_restore_apply(&mut self) -> Result<ServerRestoreSummary> {
        match self.request(Request::ServerRestoreApply).await? {
            ResponsePayload::ServerSnapshotRestored {
                sessions,
                windows,
                roles,
                follows,
                selected_sessions,
            } => Ok(ServerRestoreSummary {
                sessions,
                windows,
                roles,
                follows,
                selected_sessions,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected server snapshot restored",
            )),
        }
    }

    /// Ask server to stop gracefully.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn stop_server(&mut self) -> Result<()> {
        match self.request(Request::ServerStop).await? {
            ResponsePayload::ServerStopping => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected server stopping")),
        }
    }

    /// Create a new session.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn new_session(&mut self, name: Option<String>) -> Result<Uuid> {
        match self.request(Request::NewSession { name }).await? {
            ResponsePayload::SessionCreated { id, .. } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected session created")),
        }
    }

    /// List sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn list_sessions(&mut self) -> Result<Vec<SessionSummary>> {
        match self.request(Request::ListSessions).await? {
            ResponsePayload::SessionList { sessions } => Ok(sessions),
            _ => Err(ClientError::UnexpectedResponse("expected session list")),
        }
    }

    /// List currently connected clients.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn list_clients(&mut self) -> Result<Vec<ClientSummary>> {
        match self.request(Request::ListClients).await? {
            ResponsePayload::ClientList { clients } => Ok(clients),
            _ => Err(ClientError::UnexpectedResponse("expected client list")),
        }
    }

    /// List explicit role assignments for a session.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn list_permissions(
        &mut self,
        session: SessionSelector,
    ) -> Result<Vec<SessionPermissionSummary>> {
        match self.request(Request::ListPermissions { session }).await? {
            ResponsePayload::PermissionsList { permissions, .. } => Ok(permissions),
            _ => Err(ClientError::UnexpectedResponse("expected permissions list")),
        }
    }

    /// Grant a role to a client within a session.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn grant_role(
        &mut self,
        session: SessionSelector,
        client_id: Uuid,
        role: SessionRole,
    ) -> Result<()> {
        match self
            .request(Request::GrantRole {
                session,
                client_id,
                role,
            })
            .await?
        {
            ResponsePayload::RoleGranted { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected role granted")),
        }
    }

    /// Revoke a client's explicit role assignment (fallback to observer).
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn revoke_role(&mut self, session: SessionSelector, client_id: Uuid) -> Result<()> {
        match self
            .request(Request::RevokeRole { session, client_id })
            .await?
        {
            ResponsePayload::RoleRevoked { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected role revoked")),
        }
    }

    /// Kill a session selected by name or UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn kill_session(&mut self, selector: SessionSelector) -> Result<Uuid> {
        self.kill_session_with_options(selector, false).await
    }

    /// Kill a session selected by name or UUID with explicit force-local option.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn kill_session_with_options(
        &mut self,
        selector: SessionSelector,
        force_local: bool,
    ) -> Result<Uuid> {
        match self
            .request(Request::KillSession {
                selector,
                force_local,
            })
            .await?
        {
            ResponsePayload::SessionKilled { id } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected session killed")),
        }
    }

    /// Create a new window in a session.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn new_window(
        &mut self,
        session: Option<SessionSelector>,
        name: Option<String>,
    ) -> Result<Uuid> {
        match self.request(Request::NewWindow { session, name }).await? {
            ResponsePayload::WindowCreated { id, .. } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected window created")),
        }
    }

    /// List windows in a session. If `session` is `None`, uses attached session context.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn list_windows(
        &mut self,
        session: Option<SessionSelector>,
    ) -> Result<Vec<WindowSummary>> {
        match self.request(Request::ListWindows { session }).await? {
            ResponsePayload::WindowList { windows } => Ok(windows),
            _ => Err(ClientError::UnexpectedResponse("expected window list")),
        }
    }

    /// Kill a window selected by id/name/active.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn kill_window(
        &mut self,
        session: Option<SessionSelector>,
        target: WindowSelector,
    ) -> Result<Uuid> {
        self.kill_window_with_options(session, target, false).await
    }

    /// Kill a window selected by id/name/active with explicit force-local option.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn kill_window_with_options(
        &mut self,
        session: Option<SessionSelector>,
        target: WindowSelector,
        force_local: bool,
    ) -> Result<Uuid> {
        match self
            .request(Request::KillWindow {
                session,
                target,
                force_local,
            })
            .await?
        {
            ResponsePayload::WindowKilled { id, .. } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected window killed")),
        }
    }

    /// Switch active window selected by id/name/active.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn switch_window(
        &mut self,
        session: Option<SessionSelector>,
        target: WindowSelector,
    ) -> Result<Uuid> {
        match self
            .request(Request::SwitchWindow { session, target })
            .await?
        {
            ResponsePayload::WindowSwitched { id, .. } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected window switched")),
        }
    }

    pub async fn split_pane(
        &mut self,
        session: Option<SessionSelector>,
        direction: PaneSplitDirection,
    ) -> Result<Uuid> {
        match self
            .request(Request::SplitPane {
                session,
                target: None,
                direction,
            })
            .await?
        {
            ResponsePayload::PaneSplit { id, .. } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected pane split")),
        }
    }

    pub async fn split_pane_target(
        &mut self,
        session: Option<SessionSelector>,
        target: PaneSelector,
        direction: PaneSplitDirection,
    ) -> Result<Uuid> {
        match self
            .request(Request::SplitPane {
                session,
                target: Some(target),
                direction,
            })
            .await?
        {
            ResponsePayload::PaneSplit { id, .. } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected pane split")),
        }
    }

    pub async fn focus_pane(
        &mut self,
        session: Option<SessionSelector>,
        direction: PaneFocusDirection,
    ) -> Result<Uuid> {
        match self
            .request(Request::FocusPane {
                session,
                target: None,
                direction: Some(direction),
            })
            .await?
        {
            ResponsePayload::PaneFocused { id, .. } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected pane focused")),
        }
    }

    pub async fn focus_pane_target(
        &mut self,
        session: Option<SessionSelector>,
        target: PaneSelector,
    ) -> Result<Uuid> {
        match self
            .request(Request::FocusPane {
                session,
                target: Some(target),
                direction: None,
            })
            .await?
        {
            ResponsePayload::PaneFocused { id, .. } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected pane focused")),
        }
    }

    pub async fn resize_pane(
        &mut self,
        session: Option<SessionSelector>,
        delta: i16,
    ) -> Result<()> {
        match self
            .request(Request::ResizePane {
                session,
                target: None,
                delta,
            })
            .await?
        {
            ResponsePayload::PaneResized { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected pane resized")),
        }
    }

    pub async fn resize_pane_target(
        &mut self,
        session: Option<SessionSelector>,
        target: PaneSelector,
        delta: i16,
    ) -> Result<()> {
        match self
            .request(Request::ResizePane {
                session,
                target: Some(target),
                delta,
            })
            .await?
        {
            ResponsePayload::PaneResized { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected pane resized")),
        }
    }

    pub async fn close_pane(&mut self, session: Option<SessionSelector>) -> Result<()> {
        match self
            .request(Request::ClosePane {
                session,
                target: None,
            })
            .await?
        {
            ResponsePayload::PaneClosed { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected pane closed")),
        }
    }

    pub async fn close_pane_target(
        &mut self,
        session: Option<SessionSelector>,
        target: PaneSelector,
    ) -> Result<()> {
        match self
            .request(Request::ClosePane {
                session,
                target: Some(target),
            })
            .await?
        {
            ResponsePayload::PaneClosed { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected pane closed")),
        }
    }

    pub async fn list_panes(
        &mut self,
        session: Option<SessionSelector>,
    ) -> Result<Vec<PaneSummary>> {
        match self.request(Request::ListPanes { session }).await? {
            ResponsePayload::PaneList { panes } => Ok(panes),
            _ => Err(ClientError::UnexpectedResponse("expected pane list")),
        }
    }

    /// Follow another client's active session focus.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn follow_client(&mut self, target_client_id: Uuid, global: bool) -> Result<()> {
        match self
            .request(Request::FollowClient {
                target_client_id,
                global,
            })
            .await?
        {
            ResponsePayload::FollowStarted { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected follow started")),
        }
    }

    /// Stop following any current follow target.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn unfollow(&mut self) -> Result<()> {
        match self.request(Request::Unfollow).await? {
            ResponsePayload::FollowStopped { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse("expected follow stopped")),
        }
    }

    /// Attach client to a session selected by name or UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach(&mut self, selector: SessionSelector) -> Result<Uuid> {
        let grant = self.attach_grant(selector).await?;
        Ok(grant.session_id)
    }

    /// Request attach grant token for a session.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_grant(&mut self, selector: SessionSelector) -> Result<AttachGrant> {
        match self.request(Request::Attach { selector }).await? {
            ResponsePayload::Attached { grant } => Ok(grant),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attached response",
            )),
        }
    }

    /// Validate and consume attach grant token.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn open_attach_stream(&mut self, grant: &AttachGrant) -> Result<Uuid> {
        let info = self.open_attach_stream_info(grant).await?;
        Ok(info.session_id)
    }

    /// Validate and consume attach grant token and return role metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn open_attach_stream_info(&mut self, grant: &AttachGrant) -> Result<AttachOpenInfo> {
        match self
            .request(Request::AttachOpen {
                session_id: grant.session_id,
                attach_token: grant.attach_token,
            })
            .await?
        {
            ResponsePayload::AttachReady {
                session_id,
                can_write,
            } => Ok(AttachOpenInfo {
                session_id,
                can_write,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach ready response",
            )),
        }
    }

    /// Detach from currently attached session.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn detach(&mut self) -> Result<()> {
        match self.request(Request::Detach).await? {
            ResponsePayload::Detached => Ok(()),
            _ => Err(ClientError::UnexpectedResponse(
                "expected detached response",
            )),
        }
    }

    /// Send bytes to an attached session runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_input(&mut self, session_id: Uuid, data: Vec<u8>) -> Result<usize> {
        match self
            .request(Request::AttachInput { session_id, data })
            .await?
        {
            ResponsePayload::AttachInputAccepted { bytes } => Ok(bytes),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach input accepted response",
            )),
        }
    }

    /// Update attached viewport dimensions for pane PTY sizing.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<(u16, u16)> {
        match self
            .request(Request::AttachSetViewport {
                session_id,
                cols,
                rows,
            })
            .await?
        {
            ResponsePayload::AttachViewportSet { cols, rows, .. } => Ok((cols, rows)),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach viewport set response",
            )),
        }
    }

    /// Read bytes from an attached session runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_output(&mut self, session_id: Uuid, max_bytes: usize) -> Result<Vec<u8>> {
        match self
            .request(Request::AttachOutput {
                session_id,
                max_bytes,
            })
            .await?
        {
            ResponsePayload::AttachOutput { data } => Ok(data),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach output response",
            )),
        }
    }

    pub async fn attach_layout(&mut self, session_id: Uuid) -> Result<AttachLayoutState> {
        match self.request(Request::AttachLayout { session_id }).await? {
            ResponsePayload::AttachLayout {
                session_id,
                window_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
            } => Ok(AttachLayoutState {
                session_id,
                window_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach layout response",
            )),
        }
    }

    pub async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> Result<Vec<AttachPaneChunk>> {
        match self
            .request(Request::AttachPaneOutputBatch {
                session_id,
                pane_ids,
                max_bytes,
            })
            .await?
        {
            ResponsePayload::AttachPaneOutputBatch { chunks } => Ok(chunks),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach pane output batch response",
            )),
        }
    }

    pub async fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> Result<AttachSnapshotState> {
        match self
            .request(Request::AttachSnapshot {
                session_id,
                max_bytes_per_pane,
            })
            .await?
        {
            ResponsePayload::AttachSnapshot {
                session_id,
                window_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
                chunks,
            } => Ok(AttachSnapshotState {
                session_id,
                window_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
                chunks,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach snapshot response",
            )),
        }
    }

    /// Subscribe this client to server lifecycle events.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn subscribe_events(&mut self) -> Result<()> {
        match self.request(Request::SubscribeEvents).await? {
            ResponsePayload::EventsSubscribed => Ok(()),
            _ => Err(ClientError::UnexpectedResponse(
                "expected events subscribed response",
            )),
        }
    }

    /// Poll server lifecycle events for this client subscription.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn poll_events(&mut self, max_events: usize) -> Result<Vec<ServerEvent>> {
        match self.request(Request::PollEvents { max_events }).await? {
            ResponsePayload::EventBatch { events } => Ok(events),
            _ => Err(ClientError::UnexpectedResponse(
                "expected event batch response",
            )),
        }
    }

    async fn request(&mut self, request: Request) -> Result<ResponsePayload> {
        let response = self.request_raw(request).await?;
        match response {
            Response::Ok(payload) => Ok(payload),
            Response::Err(error) => {
                debug!("server returned error {:?}: {}", error.code, error.message);
                Err(ClientError::ServerError {
                    code: error.code,
                    message: error.message,
                })
            }
        }
    }

    fn take_request_id(&mut self) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        request_id
    }
}

fn endpoint_from_paths(paths: &ConfigPaths) -> IpcEndpoint {
    #[cfg(unix)]
    {
        IpcEndpoint::unix_socket(paths.server_socket())
    }

    #[cfg(windows)]
    {
        IpcEndpoint::windows_named_pipe(paths.server_named_pipe())
    }
}

fn load_or_create_principal_id(paths: &ConfigPaths) -> Result<Uuid> {
    let path = paths.principal_id_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ClientError::PrincipalIdWrite {
            path: path.display().to_string(),
            source,
        })?;
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let raw = content.trim();
            Uuid::parse_str(raw).map_err(|_| ClientError::PrincipalIdParse {
                path: path.display().to_string(),
                value: raw.to_string(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let principal_id = Uuid::new_v4();
            std::fs::write(&path, principal_id.to_string()).map_err(|source| {
                ClientError::PrincipalIdWrite {
                    path: path.display().to_string(),
                    source,
                }
            })?;
            Ok(principal_id)
        }
        Err(source) => Err(ClientError::PrincipalIdRead {
            path: path.display().to_string(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{BmuxClient, ClientError, ServerEvent};
    use bmux_ipc::{
        ErrorCode, InvokeServiceKind, IpcEndpoint, PaneFocusDirection, PaneSelector,
        PaneSplitDirection, SessionRole, SessionSelector, WindowSelector,
    };
    use bmux_server::BmuxServer;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::time::sleep;
    use uuid::Uuid;

    #[cfg(unix)]
    #[tokio::test]
    async fn client_invoke_service_raw_uses_generic_service_dispatch() {
        let socket_path = std::env::temp_dir().join(format!("bmux-client-{}.sock", Uuid::new_v4()));
        let endpoint = IpcEndpoint::unix_socket(&socket_path);
        let server = BmuxServer::new(endpoint.clone());
        server
            .register_service_handler(
                "example.echo",
                InvokeServiceKind::Query,
                "echo-query/v1",
                "ping",
                |_route, _ctx, payload| async move { Ok(payload) },
            )
            .expect("service registration should succeed");
        let server_task = tokio::spawn(async move { server.run().await });
        wait_for_server(&endpoint).await;

        let mut client = BmuxClient::connect(&endpoint, Duration::from_secs(2), "invoke-test")
            .await
            .expect("client should connect");
        let payload = b"hello-service".to_vec();
        let response = client
            .invoke_service_raw(
                "example.echo",
                InvokeServiceKind::Query,
                "echo-query/v1",
                "ping",
                payload.clone(),
            )
            .await
            .expect("invoke-service should succeed");
        assert_eq!(response, payload);

        client.stop_server().await.expect("stop should succeed");
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_can_create_list_attach_detach_and_kill_session() {
        let (server_task, socket_path, endpoint) = start_server().await;
        let mut client = BmuxClient::connect(&endpoint, Duration::from_secs(2), "client-test")
            .await
            .expect("client should connect");

        client.ping().await.expect("ping should pass");
        assert!(
            client
                .server_status()
                .await
                .expect("status should succeed")
                .running
        );

        let session_id = client
            .new_session(Some("dev".to_string()))
            .await
            .expect("new-session should succeed");

        client
            .subscribe_events()
            .await
            .expect("event subscribe should succeed");

        let sessions = client
            .list_sessions()
            .await
            .expect("list-sessions should succeed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_id);
        assert_eq!(sessions[0].name.as_deref(), Some("dev"));

        let initial_windows = client
            .list_windows(Some(SessionSelector::ById(session_id)))
            .await
            .expect("list windows should succeed");
        assert_eq!(initial_windows.len(), 1);
        let primary_window = initial_windows
            .iter()
            .find(|window| window.active)
            .expect("expected active window")
            .id;

        let secondary_window = client
            .new_window(
                Some(SessionSelector::ById(session_id)),
                Some("secondary".to_string()),
            )
            .await
            .expect("new window should succeed");

        let switched = client
            .switch_window(
                Some(SessionSelector::ById(session_id)),
                WindowSelector::ById(secondary_window),
            )
            .await
            .expect("switch window should succeed");
        assert_eq!(switched, secondary_window);

        let windows_after_switch = client
            .list_windows(Some(SessionSelector::ById(session_id)))
            .await
            .expect("list windows after switch should succeed");
        assert_eq!(windows_after_switch.len(), 2);
        assert!(
            windows_after_switch
                .iter()
                .any(|window| window.id == secondary_window && window.active)
        );

        let removed = client
            .kill_window(
                Some(SessionSelector::ById(session_id)),
                WindowSelector::ById(secondary_window),
            )
            .await
            .expect("kill window should succeed");
        assert_eq!(removed, secondary_window);

        let windows_after_kill = client
            .list_windows(Some(SessionSelector::ById(session_id)))
            .await
            .expect("list windows after kill should succeed");
        assert_eq!(windows_after_kill.len(), 1);
        assert!(
            windows_after_kill
                .iter()
                .any(|window| window.id == primary_window && window.active)
        );

        let grant = client
            .attach_grant(SessionSelector::ByName("dev".to_string()))
            .await
            .expect("attach should succeed");
        let attached_id = client
            .open_attach_stream(&grant)
            .await
            .expect("attach open should succeed");
        assert_eq!(attached_id, session_id);

        let layout = client
            .attach_layout(session_id)
            .await
            .expect("attach layout should succeed");
        assert_eq!(layout.session_id, session_id);
        assert_eq!(layout.panes.len(), 1);
        let first_pane_id = layout.panes[0].id;

        let initial_chunks = client
            .attach_pane_output_batch(session_id, vec![first_pane_id], 1024)
            .await
            .expect("attach pane output batch should succeed");
        assert_eq!(initial_chunks.len(), 1);
        assert_eq!(initial_chunks[0].pane_id, first_pane_id);

        let marker = format!("bmux-marker-{}", Uuid::new_v4());
        let command = format!("printf '{marker}\\n'\\n");
        let bytes_sent = client
            .attach_input(session_id, command.as_bytes().to_vec())
            .await
            .expect("attach input should succeed");
        assert_eq!(bytes_sent, command.len());

        let mut collected = Vec::new();
        for _ in 0..20 {
            let output = client
                .attach_output(session_id, 4096)
                .await
                .expect("attach output should succeed");
            if !output.is_empty() {
                collected.extend_from_slice(&output);
                if String::from_utf8_lossy(&collected).contains(&marker) {
                    break;
                }
            }
            sleep(Duration::from_millis(25)).await;
        }
        assert!(
            String::from_utf8_lossy(&collected).contains(&marker),
            "expected marker in PTY output"
        );

        let events = client
            .poll_events(10)
            .await
            .expect("event poll should succeed");
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ServerEvent::ClientAttached { id } if *id == session_id
            )
        }));

        client.detach().await.expect("detach should succeed");

        let killed_id = client
            .kill_session(SessionSelector::ById(session_id))
            .await
            .expect("kill should succeed");
        assert_eq!(killed_id, session_id);
        assert!(
            client
                .list_sessions()
                .await
                .expect("list after kill should succeed")
                .is_empty()
        );

        client.stop_server().await.expect("stop should succeed");
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attach_batch_output_detects_closed_active_pane_and_reaps_session() {
        let (server_task, socket_path, endpoint) = start_server().await;
        let mut client = BmuxClient::connect(&endpoint, Duration::from_secs(2), "attach-exit-test")
            .await
            .expect("client should connect");

        let session_id = client
            .new_session(Some("exit-session".to_string()))
            .await
            .expect("new-session should succeed");
        let grant = client
            .attach_grant(SessionSelector::ById(session_id))
            .await
            .expect("attach grant should succeed");
        let attached_id = client
            .open_attach_stream(&grant)
            .await
            .expect("attach open should succeed");
        assert_eq!(attached_id, session_id);

        let pane_id = client
            .attach_layout(session_id)
            .await
            .expect("attach layout should succeed")
            .panes
            .first()
            .expect("single pane should exist")
            .id;

        client
            .close_pane(Some(SessionSelector::ById(session_id)))
            .await
            .expect("close active pane should succeed");

        let mut closed = false;
        for _ in 0..80 {
            match client
                .attach_pane_output_batch(session_id, vec![pane_id], 1024)
                .await
            {
                Ok(_) => {}
                Err(ClientError::ServerError { code, .. }) if code == ErrorCode::NotFound => {
                    closed = true;
                    break;
                }
                Err(error) => panic!("unexpected attach batch error: {error}"),
            }
            sleep(Duration::from_millis(25)).await;
        }
        assert!(closed, "closed active pane should close attach stream");

        let mut removed = false;
        for _ in 0..40 {
            let sessions = client
                .list_sessions()
                .await
                .expect("list sessions should succeed");
            if sessions.is_empty() {
                removed = true;
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
        assert!(removed, "session should be removed after last pane exits");

        client.stop_server().await.expect("stop should succeed");
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_follow_and_unfollow_succeeds() {
        let (server_task, socket_path, endpoint) = start_server().await;
        let mut leader = BmuxClient::connect(&endpoint, Duration::from_secs(2), "leader-test")
            .await
            .expect("leader should connect");
        let mut follower = BmuxClient::connect(&endpoint, Duration::from_secs(2), "follower-test")
            .await
            .expect("follower should connect");

        follower
            .subscribe_events()
            .await
            .expect("event subscribe should succeed");

        let session_id = leader
            .new_session(Some("follow-leader".to_string()))
            .await
            .expect("leader session should be created");

        leader
            .attach_grant(SessionSelector::ById(session_id))
            .await
            .expect("leader attach grant should succeed");

        let leader_client_id = leader
            .list_clients()
            .await
            .expect("list clients should succeed")
            .into_iter()
            .find(|client| client.selected_session_id == Some(session_id))
            .map(|client| client.id)
            .expect("leader client id should be listed");

        follower
            .follow_client(leader_client_id, true)
            .await
            .expect("follow should succeed");
        follower.unfollow().await.expect("unfollow should succeed");

        leader.stop_server().await.expect("stop should succeed");
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_attach_stream_info_reports_read_only_for_secondary_attacher() {
        let (server_task, socket_path, endpoint) = start_server().await;
        let mut owner = BmuxClient::connect(&endpoint, Duration::from_secs(2), "owner-test")
            .await
            .expect("owner should connect");
        let mut observer = BmuxClient::connect(&endpoint, Duration::from_secs(2), "observer-test")
            .await
            .expect("observer should connect");

        let session_id = owner
            .new_session(Some("attach-role".to_string()))
            .await
            .expect("session should be created");

        let owner_grant = owner
            .attach_grant(SessionSelector::ById(session_id))
            .await
            .expect("owner grant should succeed");
        let owner_info = owner
            .open_attach_stream_info(&owner_grant)
            .await
            .expect("owner open should succeed");
        assert_eq!(owner_info.session_id, session_id);
        assert!(owner_info.can_write);

        let observer_grant = observer
            .attach_grant(SessionSelector::ById(session_id))
            .await
            .expect("observer grant should succeed");
        let observer_info = observer
            .open_attach_stream_info(&observer_grant)
            .await
            .expect("observer open should succeed");
        assert_eq!(observer_info.session_id, session_id);
        assert!(!observer_info.can_write);

        owner.stop_server().await.expect("stop should succeed");
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_can_grant_list_and_revoke_roles() {
        let (server_task, socket_path, endpoint) = start_server().await;
        let mut owner = BmuxClient::connect(&endpoint, Duration::from_secs(2), "owner-perm")
            .await
            .expect("owner should connect");
        let mut member = BmuxClient::connect(&endpoint, Duration::from_secs(2), "member-perm")
            .await
            .expect("member should connect");

        let session_id = owner
            .new_session(Some("perm-session".to_string()))
            .await
            .expect("session should be created");
        owner
            .subscribe_events()
            .await
            .expect("owner event subscribe should succeed");

        let member_id = member.whoami().await.expect("member whoami should succeed");
        owner
            .grant_role(
                SessionSelector::ById(session_id),
                member_id,
                SessionRole::Writer,
            )
            .await
            .expect("grant role should succeed");

        let permissions = owner
            .list_permissions(SessionSelector::ById(session_id))
            .await
            .expect("list permissions should succeed");
        assert!(
            permissions
                .iter()
                .any(|entry| entry.client_id == member_id && entry.role == SessionRole::Writer)
        );

        owner
            .revoke_role(SessionSelector::ById(session_id), member_id)
            .await
            .expect("revoke role should succeed");

        let events = owner
            .poll_events(20)
            .await
            .expect("poll role events should succeed");
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ServerEvent::RoleChanged {
                    session_id: changed_session,
                    client_id: changed_client,
                    role: SessionRole::Writer,
                    ..
                } if *changed_session == session_id && *changed_client == member_id
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ServerEvent::RoleChanged {
                    session_id: changed_session,
                    client_id: changed_client,
                    role: SessionRole::Observer,
                    ..
                } if *changed_session == session_id && *changed_client == member_id
            )
        }));

        owner.stop_server().await.expect("stop should succeed");
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_pane_selector_operations_target_active_index_and_id() {
        let (server_task, socket_path, endpoint) = start_server().await;
        let mut client = BmuxClient::connect(&endpoint, Duration::from_secs(2), "pane-selector")
            .await
            .expect("client should connect");

        let session_id = client
            .new_session(Some("pane-session".to_string()))
            .await
            .expect("session should be created");
        let session = Some(SessionSelector::ById(session_id));

        let second_pane = client
            .split_pane(session.clone(), PaneSplitDirection::Vertical)
            .await
            .expect("split active pane should succeed");
        let third_pane = client
            .split_pane_target(
                session.clone(),
                PaneSelector::ByIndex(1),
                PaneSplitDirection::Horizontal,
            )
            .await
            .expect("split indexed pane should succeed");

        let panes = client
            .list_panes(session.clone())
            .await
            .expect("list panes should succeed");
        assert_eq!(panes.len(), 3);
        assert!(panes.iter().any(|pane| pane.id == second_pane));
        assert!(
            panes
                .iter()
                .any(|pane| pane.id == third_pane && pane.focused)
        );

        let focused_by_id = client
            .focus_pane_target(session.clone(), PaneSelector::ById(second_pane))
            .await
            .expect("focus by id should succeed");
        assert_eq!(focused_by_id, second_pane);

        let focused_prev = client
            .focus_pane(session.clone(), PaneFocusDirection::Prev)
            .await
            .expect("focus by direction should succeed");
        assert_ne!(focused_prev, second_pane);

        client
            .resize_pane_target(session.clone(), PaneSelector::ByIndex(1), 1)
            .await
            .expect("resize by index should succeed");
        client
            .close_pane_target(session.clone(), PaneSelector::ById(third_pane))
            .await
            .expect("close by id should succeed");

        let panes_after_close = client
            .list_panes(session)
            .await
            .expect("list panes after close should succeed");
        assert_eq!(panes_after_close.len(), 2);
        assert!(!panes_after_close.iter().any(|pane| pane.id == third_pane));

        client.stop_server().await.expect("stop should succeed");
        server_task
            .await
            .expect("server task should join")
            .expect("server should stop cleanly");

        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[cfg(unix)]
    async fn start_server() -> (
        tokio::task::JoinHandle<anyhow::Result<()>>,
        PathBuf,
        IpcEndpoint,
    ) {
        let socket_path = std::env::temp_dir().join(format!("bmux-client-{}.sock", Uuid::new_v4()));
        let endpoint = IpcEndpoint::unix_socket(&socket_path);
        let server = BmuxServer::new(endpoint.clone());
        let server_task = tokio::spawn(async move { server.run().await });
        wait_for_server(&endpoint).await;
        (server_task, socket_path, endpoint)
    }

    #[cfg(unix)]
    async fn wait_for_server(endpoint: &IpcEndpoint) {
        for _ in 0..100 {
            if bmux_ipc::transport::LocalIpcStream::connect(endpoint)
                .await
                .is_ok()
            {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("server failed to start in time");
    }
}
