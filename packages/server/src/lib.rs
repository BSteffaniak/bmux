#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Server component for bmux terminal multiplexer.

use anyhow::{Context, Result};
use bmux_config::ConfigPaths;
use bmux_ipc::transport::{IpcTransportError, LocalIpcListener, LocalIpcStream};
use bmux_ipc::{
    AttachGrant, CURRENT_PROTOCOL_VERSION, Envelope, EnvelopeKind, ErrorCode, ErrorResponse,
    IpcEndpoint, ProtocolVersion, Request, Response, ResponsePayload, SessionSelector,
    SessionSummary, decode, encode,
};
use bmux_session::{ClientId, SessionId, SessionManager};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uuid::Uuid;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_TOKEN_TTL: Duration = Duration::from_secs(10);

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
    session_runtimes: Mutex<SessionRuntimeManager>,
    attach_tokens: Mutex<AttachTokenManager>,
    handshake_timeout: Duration,
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
    fn new(ttl: Duration) -> Self {
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
        self.tokens.retain(|_, entry| entry.session_id != session_id);
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

#[derive(Debug, Default)]
struct SessionRuntimeManager {
    runtimes: BTreeMap<SessionId, SessionRuntimeHandle>,
}

#[derive(Debug)]
struct SessionRuntimeHandle {
    stop_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    output_rx: std::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    active_client: Option<ClientId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionRuntimeError {
    NotFound,
    AlreadyAttached,
    NotAttached,
    Closed,
}

impl SessionRuntimeManager {
    fn start_runtime(&mut self, session_id: SessionId) -> Result<()> {
        if self.runtimes.contains_key(&session_id) {
            anyhow::bail!("runtime already exists for session {}", session_id.0);
        }

        let (stop_tx, mut stop_rx) = oneshot::channel();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (output_tx, output_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        break;
                    }
                    input = input_rx.recv() => {
                        match input {
                            Some(bytes) => {
                                let _ = output_tx.send(bytes);
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                stop_tx: Some(stop_tx),
                task,
                input_tx,
                output_rx: std::sync::Mutex::new(output_rx),
                active_client: None,
            },
        );
        Ok(())
    }

    fn stop_runtime(&mut self, session_id: SessionId) -> Result<()> {
        let mut runtime = self
            .runtimes
            .remove(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;

        if let Some(stop_tx) = runtime.stop_tx.take() {
            let _ = stop_tx.send(());
        }

        runtime.task.abort();

        Ok(())
    }

    fn begin_attach(&mut self, session_id: SessionId, client_id: ClientId) -> Result<(), SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        match runtime.active_client {
            Some(active) if active != client_id => Err(SessionRuntimeError::AlreadyAttached),
            _ => {
                runtime.active_client = Some(client_id);
                Ok(())
            }
        }
    }

    fn end_attach(&mut self, session_id: SessionId, client_id: ClientId) {
        if let Some(runtime) = self.runtimes.get_mut(&session_id)
            && runtime.active_client == Some(client_id)
        {
            runtime.active_client = None;
        }
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

        if runtime.active_client != Some(client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let bytes = data.len();
        runtime
            .input_tx
            .send(data)
            .map_err(|_| SessionRuntimeError::Closed)?;
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

        if runtime.active_client != Some(client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let mut receiver = runtime
            .output_rx
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        let mut output = Vec::new();
        let limit = max_bytes.max(1);

        while output.len() < limit {
            match receiver.try_recv() {
                Ok(chunk) => {
                    let remaining = limit - output.len();
                    if chunk.len() <= remaining {
                        output.extend_from_slice(&chunk);
                    } else {
                        output.extend_from_slice(&chunk[..remaining]);
                        break;
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err(SessionRuntimeError::Closed);
                }
            }
        }

        Ok(output)
    }

    fn stop_all_runtimes(&mut self) {
        for (_, mut runtime) in std::mem::take(&mut self.runtimes) {
            if let Some(stop_tx) = runtime.stop_tx.take() {
                let _ = stop_tx.send(());
            }
            runtime.task.abort();
        }
    }

    #[cfg(test)]
    fn runtime_count(&self) -> usize {
        self.runtimes.len()
    }

    #[cfg(test)]
    fn has_runtime(&self, session_id: SessionId) -> bool {
        self.runtimes.contains_key(&session_id)
    }
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
                session_runtimes: Mutex::new(SessionRuntimeManager::default()),
                attach_tokens: Mutex::new(AttachTokenManager::new(ATTACH_TOKEN_TTL)),
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

        if let Ok(mut runtime_manager) = self.state.session_runtimes.lock() {
            runtime_manager.stop_all_runtimes();
        }
        if let Ok(mut attach_tokens) = self.state.attach_tokens.lock() {
            attach_tokens.clear();
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
        )
        .await?;
        send_response(&mut stream, envelope.request_id, response).await?;
    }

    detach_client_if_attached(&state, client_id, &mut attached_session)?;

    Ok(())
}

async fn handle_request(
    state: &Arc<ServerState>,
    shutdown_tx: &watch::Sender<bool>,
    client_id: ClientId,
    attached_session: &mut Option<SessionId>,
    request: Request,
) -> Result<Response> {
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
        Request::ListSessions => {
            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
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
            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
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

            drop(manager);
            let mut runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            if let Err(error) = runtime_manager.stop_runtime(session_id) {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::Internal,
                    message: format!("failed stopping session runtime: {error:#}"),
                }));
            }
            drop(runtime_manager);

            let mut attach_tokens = state
                .attach_tokens
                .lock()
                .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
            attach_tokens.remove_for_session(session_id);

            Response::Ok(ResponsePayload::SessionKilled { id: session_id.0 })
        }
        Request::Attach { selector } => {
            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
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

            let manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            if manager.get_session(&session_id).is_none() {
                return Ok(Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session not found: {}", session_id.0),
                }));
            }
            drop(manager);

            let mut attach_tokens = state
                .attach_tokens
                .lock()
                .map_err(|_| anyhow::anyhow!("attach token manager lock poisoned"))?;
            match attach_tokens.consume(session_id, attach_token) {
                Ok(()) => {
                    drop(attach_tokens);
                    let mut runtime_manager = state
                        .session_runtimes
                        .lock()
                        .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                    match runtime_manager.begin_attach(session_id, client_id) {
                        Ok(()) => Response::Ok(ResponsePayload::AttachReady {
                            session_id: session_id.0,
                        }),
                        Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                            code: ErrorCode::NotFound,
                            message: format!("session runtime not found: {}", session_id.0),
                        }),
                        Err(SessionRuntimeError::AlreadyAttached) => Response::Err(ErrorResponse {
                            code: ErrorCode::AlreadyExists,
                            message: "session already has an attached client".to_string(),
                        }),
                        Err(SessionRuntimeError::NotAttached | SessionRuntimeError::Closed) => {
                            Response::Err(ErrorResponse {
                                code: ErrorCode::Internal,
                                message: "failed opening attach stream".to_string(),
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
            let mut runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            match runtime_manager.write_input(SessionId(session_id), client_id, data) {
                Ok(bytes) => Response::Ok(ResponsePayload::AttachInputAccepted { bytes }),
                Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {session_id}"),
                }),
                Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "client is not attached to session runtime".to_string(),
                }),
                Err(SessionRuntimeError::AlreadyAttached | SessionRuntimeError::Closed) => {
                    Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: "failed writing attach input".to_string(),
                    })
                }
            }
        }
        Request::AttachOutput {
            session_id,
            max_bytes,
        } => {
            let mut runtime_manager = state
                .session_runtimes
                .lock()
                .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
            match runtime_manager.read_output(SessionId(session_id), client_id, max_bytes) {
                Ok(data) => Response::Ok(ResponsePayload::AttachOutput { data }),
                Err(SessionRuntimeError::NotFound) => Response::Err(ErrorResponse {
                    code: ErrorCode::NotFound,
                    message: format!("session runtime not found: {session_id}"),
                }),
                Err(SessionRuntimeError::NotAttached) => Response::Err(ErrorResponse {
                    code: ErrorCode::InvalidRequest,
                    message: "client is not attached to session runtime".to_string(),
                }),
                Err(SessionRuntimeError::AlreadyAttached | SessionRuntimeError::Closed) => {
                    Response::Err(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: "failed reading attach output".to_string(),
                    })
                }
            }
        }
        Request::Detach => {
            let mut manager = state
                .session_manager
                .lock()
                .map_err(|_| anyhow::anyhow!("session manager lock poisoned"))?;
            let current_session_id = attached_session.take();
            if let Some(current_session_id) = current_session_id
                && let Some(session) = manager.get_session_mut(&current_session_id)
            {
                session.remove_client(&client_id);
            }
            drop(manager);

            if let Some(current_session_id) = current_session_id {
                let mut runtime_manager = state
                    .session_runtimes
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
                runtime_manager.end_attach(current_session_id, client_id);
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
    drop(manager);

    let mut runtime_manager = state
        .session_runtimes
        .lock()
        .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;
    runtime_manager.end_attach(session_id, client_id);

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
    use bmux_session::SessionId;
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
            Response::Ok(ResponsePayload::AttachReady { session_id })
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
    async fn rejects_second_active_attach_for_same_session() {
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
            Response::Ok(ResponsePayload::AttachReady { session_id })
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
        assert!(matches!(
            open_b,
            Response::Err(ErrorResponse {
                code: ErrorCode::AlreadyExists,
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
        assert_eq!(open, Response::Ok(ResponsePayload::AttachReady { session_id }));

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
            Response::Ok(ResponsePayload::AttachReady { session_id })
        );

        stop_server(server, server_task, &socket_path).await;
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

    fn force_expire_attach_token(token_manager: &mut super::AttachTokenManager, token: Uuid) {
        if let Some(entry) = token_manager.tokens.get_mut(&token) {
            entry.expires_at = std::time::Instant::now() - Duration::from_millis(1);
        }
    }
}
