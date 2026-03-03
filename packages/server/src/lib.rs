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
    ProtocolVersion, Request, Response, ResponsePayload, SessionSelector, SessionSummary, decode,
    encode,
};
use bmux_session::{ClientId, SessionId, SessionManager};
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

        Ok(())
    }
}

async fn handle_connection(
    state: Arc<ServerState>,
    shutdown_tx: watch::Sender<bool>,
    mut stream: LocalIpcStream,
) -> Result<()> {
    let client_id = ClientId::new();
    let mut attached_session: Option<SessionId> = None;

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

        let response = handle_request(
            &state,
            &shutdown_tx,
            client_id,
            &mut attached_session,
            request,
        )?;
        send_response(&mut stream, envelope.request_id, response).await?;
    }

    detach_client_if_attached(&state, client_id, &mut attached_session)?;

    Ok(())
}

fn handle_request(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    client_id: ClientId,
    attached_session: &mut Option<SessionId>,
    request: Request,
) -> Result<Response> {
    let mut manager = state
        .session_manager
        .lock()
        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;

    let response = match request {
        Request::Hello { .. } => Response::Err(ErrorResponse {
            code: ErrorCode::InvalidRequest,
            message: "hello request is only valid during handshake".to_string(),
        }),
        Request::Ping => Response::Ok(ResponsePayload::Pong),
        Request::ServerStatus => Response::Ok(ResponsePayload::ServerStatus { running: true }),
        Request::ServerStop => {
            let _ = shutdown_tx.send(true);
            Response::Ok(ResponsePayload::ServerStopping)
        }
        Request::NewSession { name } => {
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
                Ok(session_id) => Response::Ok(ResponsePayload::SessionCreated {
                    id: session_id.0,
                    name,
                }),
                Err(error) => Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed creating session: {error:#}"),
                }),
            }
        }
        Request::ListSessions => {
            let sessions = manager
                .list_sessions()
                .into_iter()
                .map(|session| SessionSummary {
                    id: session.id.0,
                    name: session.name,
                    window_count: session.window_count,
                    client_count: session.client_count,
                })
                .collect::<Vec<_>>();
            Response::Ok(ResponsePayload::SessionList { sessions })
        }
        Request::KillSession { selector } => {
            let Some(session_id) = resolve_session_id(&manager, &selector) else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session not found for selector {selector:?}"),
                }));
            };

            if manager.remove_session(&session_id).is_err() {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed removing session {}", session_id.0),
                }));
            }
            if *attached_session == Some(session_id) {
                *attached_session = None;
            }
            Response::Ok(ResponsePayload::SessionKilled { id: session_id.0 })
        }
        Request::Attach { selector } => {
            let Some(next_session_id) = resolve_session_id(&manager, &selector) else {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session not found for selector {selector:?}"),
                }));
            };

            if let Some(previous_session_id) = attached_session.take()
                && previous_session_id != next_session_id
                && let Some(previous) = manager.get_session_mut(&previous_session_id)
            {
                previous.remove_client(&client_id);
            }

            match manager.get_session_mut(&next_session_id) {
                Some(session) => {
                    session.add_client(client_id);
                    *attached_session = Some(next_session_id);
                    Response::Ok(ResponsePayload::Attached {
                        id: next_session_id.0,
                    })
                }
                None => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session not found: {}", next_session_id.0),
                }),
            }
        }
        Request::Detach => {
            if let Some(current_session_id) = attached_session.take()
                && let Some(session) = manager.get_session_mut(&current_session_id)
            {
                session.remove_client(&client_id);
            }
            Response::Ok(ResponsePayload::Detached)
        }
    };

    Ok(response)
}

fn detach_client_if_attached(
    state: &Arc<ServerState>,
    client_id: ClientId,
    attached_session: &mut Option<SessionId>,
) -> Result<()> {
    let Some(session_id) = attached_session.take() else {
        return Ok(());
    };

    let mut manager = state
        .session_manager
        .lock()
        .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
    if let Some(session) = manager.get_session_mut(&session_id) {
        session.remove_client(&client_id);
    }

    Ok(())
}

fn resolve_session_id(manager: &SessionManager, selector: &SessionSelector) -> Option<SessionId> {
    match selector {
        SessionSelector::ById(raw_id) => {
            let session_id = SessionId(*raw_id);
            manager.get_session(&session_id).map(|_| session_id)
        }
        SessionSelector::ByName(name) => manager
            .list_sessions()
            .into_iter()
            .find(|session| session.name.as_deref() == Some(name.as_str()))
            .map(|session| session.id),
    }
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
        Response, ResponsePayload, SessionSelector, decode, encode,
    };
    use std::path::Path;
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
        assert_eq!(
            attached,
            Response::Ok(ResponsePayload::Attached { id: session_id })
        );

        let detached = send_request(&mut client, 22, Request::Detach).await;
        assert_eq!(detached, Response::Ok(ResponsePayload::Detached));

        let killed = send_request(
            &mut client,
            23,
            Request::KillSession {
                selector: SessionSelector::ById(session_id),
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
    async fn wait_for_server(endpoint: &IpcEndpoint) {
        for _ in 0..50 {
            if LocalIpcStream::connect(endpoint).await.is_ok() {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("server did not start in time");
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
        let server_clone = server.clone();
        let server_task = tokio::spawn(async move { server_clone.run().await });
        wait_for_server(&endpoint).await;
        (server, endpoint, socket_path, server_task)
    }

    #[cfg(unix)]
    async fn connect_and_handshake(endpoint: &IpcEndpoint) -> LocalIpcStream {
        let mut client = LocalIpcStream::connect(endpoint)
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
        assert_eq!(
            response,
            Response::Ok(ResponsePayload::ServerStatus { running: true })
        );
        client
    }

    #[cfg(unix)]
    async fn send_request(client: &mut LocalIpcStream, request_id: u64, request: Request) -> Response {
        let payload = encode(&request).expect("request should encode");
        let envelope = Envelope::new(request_id, EnvelopeKind::Request, payload);
        client
            .send_envelope(&envelope)
            .await
            .expect("request send should succeed");
        let reply = client
            .recv_envelope()
            .await
            .expect("request reply should be received");
        assert_eq!(reply.request_id, request_id);
        decode(&reply.payload).expect("response decode should succeed")
    }

    #[cfg(unix)]
    async fn stop_server(
        server: BmuxServer,
        server_task: tokio::task::JoinHandle<anyhow::Result<()>>,
        socket_path: &std::path::Path,
    ) {
        server.request_shutdown();
        server_task
            .await
            .expect("server task should join")
            .expect("server should shut down cleanly");
        if socket_path.exists() {
            std::fs::remove_file(socket_path).expect("socket cleanup should succeed");
        }
    }
}
