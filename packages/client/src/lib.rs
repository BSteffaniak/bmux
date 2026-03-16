#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Client component for bmux terminal multiplexer.

use bmux_config::{BmuxConfig, ConfigPaths};
pub use bmux_ipc::Event as ServerEvent;
use bmux_ipc::transport::{IpcTransportError, LocalIpcStream};
use bmux_ipc::{
    AttachGrant, AttachPaneChunk, AttachScene, ClientSummary, ContextSelector, ContextSummary,
    Envelope, EnvelopeKind, ErrorCode, InvokeServiceKind, IpcEndpoint, PaneFocusDirection,
    PaneLayoutNode, PaneSelector, PaneSplitDirection, PaneSummary, ProtocolVersion, Request,
    Response, ResponsePayload, ServerSnapshotStatus, SessionSelector, SessionSummary, decode,
    encode,
};
use std::collections::BTreeMap;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, trace, warn};
use uuid::Uuid;

/// Result type for client operations.
pub type Result<T> = std::result::Result<T, ClientError>;

/// Details returned when opening an attach stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachOpenInfo {
    pub context_id: Option<Uuid>,
    pub session_id: Uuid,
    pub can_write: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachLayoutState {
    pub context_id: Option<Uuid>,
    pub session_id: Uuid,
    pub focused_pane_id: Uuid,
    pub panes: Vec<PaneSummary>,
    pub layout_root: PaneLayoutNode,
    pub scene: AttachScene,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSnapshotState {
    pub context_id: Option<Uuid>,
    pub session_id: Uuid,
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
    pub server_control_principal_id: Uuid,
}

/// Principal identity details returned by whoami-principal RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrincipalIdentityInfo {
    pub principal_id: Uuid,
    pub server_control_principal_id: Uuid,
    pub force_local_permitted: bool,
}

/// Summary returned by apply-restore operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerRestoreSummary {
    pub sessions: usize,
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

    /// Return principal identity information for this client and server control principal.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn whoami_principal(&mut self) -> Result<PrincipalIdentityInfo> {
        match self.request(Request::WhoAmIPrincipal).await? {
            ResponsePayload::PrincipalIdentity {
                principal_id,
                server_control_principal_id,
                force_local_permitted,
            } => Ok(PrincipalIdentityInfo {
                principal_id,
                server_control_principal_id,
                force_local_permitted,
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
                server_control_principal_id,
            } => Ok(ServerStatusInfo {
                running,
                snapshot,
                principal_id,
                server_control_principal_id,
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
        let request_kind = request_kind_name(&request);
        let timeout_ms = self.timeout.as_millis();
        let started_at = std::time::Instant::now();
        debug!(
            request_id,
            request = request_kind,
            timeout_ms,
            "ipc.request.start"
        );
        let payload = encode(&request)?;
        let envelope = Envelope::new(request_id, EnvelopeKind::Request, payload);

        tokio::time::timeout(self.timeout, self.stream.send_envelope(&envelope))
            .await
            .map_err(|_| {
                warn!(
                    request_id,
                    request = request_kind,
                    timeout_ms,
                    phase = "send",
                    duration_ms = started_at.elapsed().as_millis(),
                    "ipc.request.timeout"
                );
                ClientError::Timeout(self.timeout)
            })??;

        trace!(
            request_id,
            request = request_kind,
            duration_ms = started_at.elapsed().as_millis(),
            "ipc.request.sent"
        );

        let response_envelope = tokio::time::timeout(self.timeout, self.stream.recv_envelope())
            .await
            .map_err(|_| {
                warn!(
                    request_id,
                    request = request_kind,
                    timeout_ms,
                    phase = "recv",
                    duration_ms = started_at.elapsed().as_millis(),
                    "ipc.request.timeout"
                );
                ClientError::Timeout(self.timeout)
            })??;

        if response_envelope.request_id != request_id {
            warn!(
                request_id,
                request = request_kind,
                actual_request_id = response_envelope.request_id,
                duration_ms = started_at.elapsed().as_millis(),
                "ipc.request.id_mismatch"
            );
            return Err(ClientError::RequestIdMismatch {
                expected: request_id,
                actual: response_envelope.request_id,
            });
        }
        if response_envelope.kind != EnvelopeKind::Response {
            warn!(
                request_id,
                request = request_kind,
                actual_kind = ?response_envelope.kind,
                duration_ms = started_at.elapsed().as_millis(),
                "ipc.request.unexpected_envelope_kind"
            );
            return Err(ClientError::UnexpectedEnvelopeKind {
                expected: EnvelopeKind::Response,
                actual: response_envelope.kind,
            });
        }

        let response: Response = decode(&response_envelope.payload).map_err(ClientError::from)?;
        debug!(
            request_id,
            request = request_kind,
            response = response_kind_name(&response),
            duration_ms = started_at.elapsed().as_millis(),
            "ipc.request.done"
        );
        Ok(response)
    }

    /// Return whether this principal can use force-local kill bypass.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn force_local_permitted(&mut self) -> Result<bool> {
        match self.request(Request::WhoAmIPrincipal).await? {
            ResponsePayload::PrincipalIdentity {
                force_local_permitted,
                ..
            } => Ok(force_local_permitted),
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
                follows,
                selected_sessions,
            } => Ok(ServerRestoreSummary {
                sessions,
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

    /// Create a new generic runtime context.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn create_context(
        &mut self,
        name: Option<String>,
        attributes: BTreeMap<String, String>,
    ) -> Result<ContextSummary> {
        match self
            .request(Request::CreateContext { name, attributes })
            .await?
        {
            ResponsePayload::ContextCreated { context } => Ok(context),
            _ => Err(ClientError::UnexpectedResponse("expected context created")),
        }
    }

    /// List generic runtime contexts.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn list_contexts(&mut self) -> Result<Vec<ContextSummary>> {
        match self.request(Request::ListContexts).await? {
            ResponsePayload::ContextList { contexts } => Ok(contexts),
            _ => Err(ClientError::UnexpectedResponse("expected context list")),
        }
    }

    /// Select an active runtime context for this client.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn select_context(&mut self, selector: ContextSelector) -> Result<ContextSummary> {
        match self.request(Request::SelectContext { selector }).await? {
            ResponsePayload::ContextSelected { context } => Ok(context),
            _ => Err(ClientError::UnexpectedResponse("expected context selected")),
        }
    }

    /// Close a runtime context and optionally force closure.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn close_context(&mut self, selector: ContextSelector, force: bool) -> Result<Uuid> {
        match self
            .request(Request::CloseContext { selector, force })
            .await?
        {
            ResponsePayload::ContextClosed { id } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected context closed")),
        }
    }

    /// Return currently active context for this client when available.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn current_context(&mut self) -> Result<Option<ContextSummary>> {
        match self.request(Request::CurrentContext).await? {
            ResponsePayload::CurrentContext { context } => Ok(context),
            _ => Err(ClientError::UnexpectedResponse("expected current context")),
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

    /// Request attach grant token for a context selected by name or UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_context_grant(&mut self, selector: ContextSelector) -> Result<AttachGrant> {
        match self.request(Request::AttachContext { selector }).await? {
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

    /// Validate and consume attach grant token and return attach metadata.
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
                context_id,
                session_id,
                can_write,
            } => Ok(AttachOpenInfo {
                context_id,
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
                context_id,
                session_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
            } => Ok(AttachLayoutState {
                context_id,
                session_id,
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
                context_id,
                session_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
                chunks,
            } => Ok(AttachSnapshotState {
                context_id,
                session_id,
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

fn request_kind_name(request: &Request) -> &'static str {
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
        Request::Detach => "detach",
        Request::SubscribeEvents => "subscribe_events",
        Request::PollEvents { .. } => "poll_events",
    }
}

fn response_kind_name(response: &Response) -> &'static str {
    match response {
        Response::Ok(payload) => match payload {
            ResponsePayload::Pong => "pong",
            ResponsePayload::ClientIdentity { .. } => "client_identity",
            ResponsePayload::PrincipalIdentity { .. } => "principal_identity",
            ResponsePayload::ServerStatus { .. } => "server_status",
            ResponsePayload::ServerSnapshotSaved { .. } => "server_snapshot_saved",
            ResponsePayload::ServerSnapshotRestoreDryRun { .. } => {
                "server_snapshot_restore_dry_run"
            }
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
            ResponsePayload::Detached => "detached",
            ResponsePayload::EventsSubscribed => "events_subscribed",
            ResponsePayload::EventBatch { .. } => "event_batch",
        },
        Response::Err(_) => "error",
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
    use super::{ConfigPaths, load_or_create_principal_id};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic for test")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bmux-client-test-{nanos}"));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    #[test]
    fn load_or_create_principal_id_creates_and_persists_value() {
        let root = temp_dir();
        let paths = ConfigPaths::new(root.join("config"), root.join("runtime"), root.join("data"));
        let first = load_or_create_principal_id(&paths).expect("principal id should be created");
        let second = load_or_create_principal_id(&paths).expect("principal id should be reused");
        assert_eq!(first, second);
    }

    #[test]
    fn load_or_create_principal_id_rejects_invalid_file_contents() {
        let root = temp_dir();
        let paths = ConfigPaths::new(root.join("config"), root.join("runtime"), root.join("data"));
        let path = paths.principal_id_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("principal parent should exist");
        }
        fs::write(&path, "not-a-uuid").expect("principal file should be written");
        let error = load_or_create_principal_id(&paths).expect_err("invalid principal should fail");
        assert!(error.to_string().contains("invalid principal id"));
    }
}
