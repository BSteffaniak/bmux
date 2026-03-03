#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Client component for bmux terminal multiplexer.

use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_ipc::transport::{IpcTransportError, LocalIpcStream};
use bmux_ipc::{
    Envelope, EnvelopeKind, ErrorCode, IpcEndpoint, ProtocolVersion, Request, Response,
    ResponsePayload, SessionSelector, SessionSummary, decode, encode,
};
use std::time::Duration;
use thiserror::Error;
use tracing::debug;
use uuid::Uuid;

/// Result type for client operations.
pub type Result<T> = std::result::Result<T, ClientError>;

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
    pub async fn connect_with_paths(paths: &ConfigPaths, client_name: impl Into<String>) -> Result<Self> {
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

    /// Attach client to a session selected by name or UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach(&mut self, selector: SessionSelector) -> Result<Uuid> {
        match self.request(Request::Attach { selector }).await? {
            ResponsePayload::Attached { id } => Ok(id),
            _ => Err(ClientError::UnexpectedResponse("expected attached response")),
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
            _ => Err(ClientError::UnexpectedResponse("expected detached response")),
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
                debug!(
                    "server returned error {:?}: {}",
                    error.code, error.message
                );
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
    use super::BmuxClient;
    use bmux_ipc::{IpcEndpoint, SessionSelector};
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

        let sessions = client
            .list_sessions()
            .await
            .expect("list-sessions should succeed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_id);
        assert_eq!(sessions[0].name.as_deref(), Some("dev"));

        let attached_id = client
            .attach(SessionSelector::ByName("dev".to_string()))
            .await
            .expect("attach should succeed");
        assert_eq!(attached_id, session_id);

        client.detach().await.expect("detach should succeed");

        let killed_id = client
            .kill_session(SessionSelector::ById(session_id))
            .await
            .expect("kill should succeed");
        assert_eq!(killed_id, session_id);
        assert!(client
            .list_sessions()
            .await
            .expect("list after kill should succeed")
            .is_empty());

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
