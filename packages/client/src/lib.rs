#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Client component for bmux terminal multiplexer.

use bmux_config::{BmuxConfig, ConfigPaths};
pub use bmux_ipc::Event as ServerEvent;
use bmux_ipc::transport::{IpcTransportError, LocalIpcStream};
use bmux_ipc::{
    AttachGrant, ClientSummary, Envelope, EnvelopeKind, ErrorCode, IpcEndpoint, ProtocolVersion,
    Request, Response, ResponsePayload, SessionSelector, SessionSummary, WindowSelector,
    WindowSummary, decode, encode,
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
}

/// Main client API for communicating with bmux server.
#[derive(Debug)]
pub struct BmuxClient {
    stream: LocalIpcStream,
    timeout: Duration,
    next_request_id: u64,
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
        let stream = LocalIpcStream::connect(endpoint).await?;
        let mut client = Self {
            stream,
            timeout,
            next_request_id: 1,
        };

        let hello_response = client
            .request(Request::Hello {
                protocol_version: ProtocolVersion::current(),
                client_name: client_name.into(),
            })
            .await?;

        match hello_response {
            ResponsePayload::ServerStatus { running: true } => Ok(client),
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
        Self::connect(&endpoint, timeout, client_name).await
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

    /// Retrieve server status.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn server_status(&mut self) -> Result<bool> {
        match self.request(Request::ServerStatus).await? {
            ResponsePayload::ServerStatus { running } => Ok(running),
            _ => Err(ClientError::UnexpectedResponse("expected server status")),
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

    /// Kill a session selected by name or UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn kill_session(&mut self, selector: SessionSelector) -> Result<Uuid> {
        match self.request(Request::KillSession { selector }).await? {
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
        match self
            .request(Request::KillWindow { session, target })
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

        let response: Response = decode(&response_envelope.payload)?;
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

#[cfg(test)]
mod tests {
    use super::{BmuxClient, ServerEvent};
    use bmux_ipc::{IpcEndpoint, SessionSelector, WindowSelector};
    use bmux_server::BmuxServer;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::time::sleep;
    use uuid::Uuid;

    #[cfg(unix)]
    #[tokio::test]
    async fn client_can_create_list_attach_detach_and_kill_session() {
        let (server_task, socket_path, endpoint) = start_server().await;
        let mut client = BmuxClient::connect(&endpoint, Duration::from_secs(2), "client-test")
            .await
            .expect("client should connect");

        client.ping().await.expect("ping should pass");
        assert!(client.server_status().await.expect("status should succeed"));

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
