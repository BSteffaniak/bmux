#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Server component for bmux terminal multiplexer.

use anyhow::{Context, Result};
use bmux_config::ConfigPaths;
use bmux_ipc::transport::{IpcTransportError, LocalIpcListener, LocalIpcStream};
use bmux_ipc::{
    CURRENT_PROTOCOL_VERSION, Envelope, EnvelopeKind, ErrorCode, ErrorResponse, IpcEndpoint,
    ProtocolVersion, Request, Response, ResponsePayload, decode, encode,
};
use bmux_session::SessionManager;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Main server implementation.
#[derive(Debug, Clone)]
pub struct BmuxServer {
    endpoint: IpcEndpoint,
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Debug)]
struct ServerState {
    #[allow(dead_code)]
    session_manager: Mutex<SessionManager>,
    handshake_timeout: Duration,
}

impl BmuxServer {
    /// Create a server with an explicit endpoint.
    #[must_use]
    pub fn new(endpoint: IpcEndpoint) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            endpoint,
            state: Arc::new(ServerState {
                session_manager: Mutex::new(SessionManager::new()),
                handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            }),
            shutdown_tx,
        }
    }

    /// Create a server with endpoint derived from config paths.
    #[must_use]
    pub fn from_config_paths(paths: &ConfigPaths) -> Self {
        #[cfg(unix)]
        let endpoint = IpcEndpoint::unix_socket(paths.server_socket());

        #[cfg(windows)]
        let endpoint = IpcEndpoint::windows_named_pipe(paths.server_named_pipe());

        Self::new(endpoint)
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
        let listener = LocalIpcListener::bind(&self.endpoint)
            .await
            .with_context(|| format!("failed binding server endpoint {:?}", self.endpoint))?;
        info!("bmux server listening on {:?}", self.endpoint);

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
                            tokio::spawn(async move {
                                if let Err(error) = handle_connection(state, stream).await {
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

        Ok(())
    }
}

async fn handle_connection(state: Arc<ServerState>, mut stream: LocalIpcStream) -> Result<()> {
    let first_envelope = tokio::time::timeout(state.handshake_timeout, stream.recv_envelope())
        .await
        .context("handshake timed out")??;

    let handshake = parse_request(&first_envelope)?;
    match handshake {
        Request::Hello {
            protocol_version,
            client_name,
        } => {
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
            debug!("accepted client handshake: {client_name}");
            send_ok(
                &mut stream,
                first_envelope.request_id,
                ResponsePayload::ServerStatus { running: true },
            )
            .await?;
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

        if let Request::Ping = request {
            send_ok(&mut stream, envelope.request_id, ResponsePayload::Pong).await?;
            continue;
        }

        send_error(
            &mut stream,
            envelope.request_id,
            ErrorCode::InvalidRequest,
            "request handling not implemented yet".to_string(),
        )
        .await?;
    }

    Ok(())
}

fn parse_request(envelope: &Envelope) -> Result<Request> {
    if envelope.kind != EnvelopeKind::Request {
        anyhow::bail!("expected request envelope kind")
    }
    decode(&envelope.payload).context("failed to decode request payload")
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
    use super::BmuxServer;
    use bmux_ipc::transport::LocalIpcStream;
    use bmux_ipc::{
        Envelope, EnvelopeKind, ErrorCode, ErrorResponse, IpcEndpoint, ProtocolVersion, Request,
        Response, ResponsePayload, decode, encode,
    };
    use std::time::Duration;
    use tokio::time::sleep;
    use uuid::Uuid;

    #[cfg(unix)]
    #[tokio::test]
    async fn handshake_accepts_current_protocol_version() {
        let socket_path = std::env::temp_dir().join(format!("bmux-server-{}.sock", Uuid::new_v4()));
        let endpoint = IpcEndpoint::unix_socket(&socket_path);
        let server = BmuxServer::new(endpoint.clone());

        let server_clone = server.clone();
        let server_task = tokio::spawn(async move { server_clone.run().await });
        wait_for_server(&endpoint).await;

        let mut client = LocalIpcStream::connect(&endpoint)
            .await
            .expect("client should connect");
        let hello_payload = encode(&Request::Hello {
            protocol_version: ProtocolVersion::current(),
            client_name: "test-client".to_string(),
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
        assert_eq!(
            response,
            Response::Ok(ResponsePayload::ServerStatus { running: true })
        );

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

        let server_clone = server.clone();
        let server_task = tokio::spawn(async move { server_clone.run().await });
        wait_for_server(&endpoint).await;

        let mut client = LocalIpcStream::connect(&endpoint)
            .await
            .expect("client should connect");
        let hello_payload = encode(&Request::Hello {
            protocol_version: ProtocolVersion(99),
            client_name: "test-client".to_string(),
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
    async fn wait_for_server(endpoint: &IpcEndpoint) {
        for _ in 0..50 {
            if LocalIpcStream::connect(endpoint).await.is_ok() {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("server did not start in time");
    }
}
