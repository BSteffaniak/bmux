use crate::error::{MobileCoreError, Result};
use crate::target::{TargetRecord, TargetTransport};
use bmux_client::{BmuxClient, ClientError};
use bmux_ipc::compressed_stream::CompressedStream;
use bmux_ipc::transport::{ErasedIpcStream, IpcTransportError};
use bmux_ipc::{ErrorCode, SessionSelector};
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::presets};
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsConnector;
use url::Url;
use uuid::Uuid;

const BMUX_IROH_ALPN: &[u8] = b"bmux/gateway/iroh/1";
const SESSION_COMMAND_BUFFER: usize = 64;
const DEFAULT_IROH_CONNECT_TIMEOUT_MS: u64 = 30_000;
const IROH_COMPRESSED_RETRY_COUNT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IrohCompressionMode {
    Auto,
    None,
    Zstd,
}

impl IrohCompressionMode {
    fn parse(value: &str) -> Result<Self> {
        if value.eq_ignore_ascii_case("auto") {
            Ok(Self::Auto)
        } else if value.eq_ignore_ascii_case("none") {
            Ok(Self::None)
        } else if value.eq_ignore_ascii_case("zstd") {
            Ok(Self::Zstd)
        } else {
            Err(MobileCoreError::InvalidTarget(format!(
                "unsupported iroh compression mode '{value}' (expected auto|none|zstd)"
            )))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IrohConnectAttemptErrorKind {
    Timeout,
    Protocol,
    Other,
}

#[derive(Debug, Clone)]
struct IrohConnectAttemptError {
    kind: IrohConnectAttemptErrorKind,
    message: String,
}

impl IrohConnectAttemptError {
    const fn timeout(message: String) -> Self {
        Self {
            kind: IrohConnectAttemptErrorKind::Timeout,
            message,
        }
    }

    const fn protocol(message: String) -> Self {
        Self {
            kind: IrohConnectAttemptErrorKind::Protocol,
            message,
        }
    }

    const fn other(message: String) -> Self {
        Self {
            kind: IrohConnectAttemptErrorKind::Other,
            message,
        }
    }

    const fn is_timeout(&self) -> bool {
        matches!(self.kind, IrohConnectAttemptErrorKind::Timeout)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendSessionHandle {
    pub id: Uuid,
    pub session_id: Uuid,
    pub can_write: bool,
}

pub trait TerminalBackend: Send + Sync {
    /// Opens or attaches a terminal session for a target.
    ///
    /// # Errors
    ///
    /// Returns an error if target parsing fails, transport is unsupported,
    /// or the backend connection/attach flow cannot be completed.
    fn open(
        &self,
        target: &TargetRecord,
        session: Option<String>,
        rows: u16,
        cols: u16,
    ) -> Result<BackendSessionHandle>;

    /// Reads terminal output bytes for an open session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session handle is unknown or the remote output
    /// call fails.
    fn poll_output(&self, handle_id: Uuid, max_bytes: usize) -> Result<Vec<u8>>;

    /// Writes terminal input bytes for an open session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session handle is unknown or the remote input
    /// call fails.
    fn write_input(&self, handle_id: Uuid, bytes: &[u8]) -> Result<()>;

    /// Updates the terminal viewport size for an open session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session handle is unknown or the remote resize
    /// call fails.
    fn resize(&self, handle_id: Uuid, rows: u16, cols: u16) -> Result<()>;

    /// Closes an open session and detaches from the remote terminal.
    ///
    /// # Errors
    ///
    /// Returns an error if the session handle is unknown.
    fn close(&self, handle_id: Uuid) -> Result<()>;
}

struct RemoteTerminalSession {
    command_tx: mpsc::Sender<SessionCommand>,
}

enum SessionCommand {
    PollOutput {
        max_bytes: usize,
        response: oneshot::Sender<Result<Vec<u8>>>,
    },
    WriteInput {
        bytes: Vec<u8>,
        response: oneshot::Sender<Result<()>>,
    },
    Resize {
        rows: u16,
        cols: u16,
        response: oneshot::Sender<Result<()>>,
    },
    Close {
        response: oneshot::Sender<Result<()>>,
    },
}

pub struct RemoteTerminalBackend {
    runtime: tokio::runtime::Runtime,
    sessions: Mutex<BTreeMap<Uuid, RemoteTerminalSession>>,
}

impl Default for RemoteTerminalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteTerminalBackend {
    #[must_use]
    /// Creates a runtime-backed remote terminal backend.
    ///
    /// # Panics
    ///
    /// Panics if a Tokio runtime cannot be initialized.
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should initialize for mobile terminal backend");
        Self {
            runtime,
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    fn parse_target(target: &TargetRecord) -> Result<ParsedRemoteTarget> {
        match target.transport {
            TargetTransport::Iroh => parse_iroh_target(&target.canonical_target.value),
            TargetTransport::Tls => parse_tls_target(&target.canonical_target.value),
            TargetTransport::Local | TargetTransport::Ssh => {
                Err(MobileCoreError::UnsupportedTerminalTransport(
                    target.canonical_target.value.clone(),
                ))
            }
        }
    }

    fn connect_client(&self, target: &ParsedRemoteTarget) -> Result<BmuxClient> {
        match target {
            ParsedRemoteTarget::Iroh(iroh_target) => self.connect_iroh_client(iroh_target),
            ParsedRemoteTarget::Tls(tls_target) => self.connect_tls_client(tls_target),
        }
    }

    fn lock_sessions(&self) -> Result<MutexGuard<'_, BTreeMap<Uuid, RemoteTerminalSession>>> {
        self.sessions.lock().map_err(|_| {
            MobileCoreError::TerminalBackendFailure(
                "terminal backend sessions lock poisoned".to_string(),
            )
        })
    }

    fn session_command_tx(&self, handle_id: Uuid) -> Result<mpsc::Sender<SessionCommand>> {
        self.lock_sessions()?
            .get(&handle_id)
            .map(|session| session.command_tx.clone())
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(handle_id.to_string()))
    }

    fn remove_session_if_present(&self, handle_id: Uuid) {
        if let Ok(mut sessions) = self.lock_sessions() {
            sessions.remove(&handle_id);
        }
    }

    fn call_session<T>(
        &self,
        handle_id: Uuid,
        command_builder: impl FnOnce(oneshot::Sender<Result<T>>) -> SessionCommand,
    ) -> Result<T> {
        let command_tx = self.session_command_tx(handle_id)?;
        let response = self.runtime.block_on(async {
            let (response_tx, response_rx) = oneshot::channel();
            command_tx
                .send(command_builder(response_tx))
                .await
                .map_err(|_| {
                    MobileCoreError::TerminalBackendFailure(
                        "terminal backend session actor unavailable".to_string(),
                    )
                })?;
            response_rx.await.map_err(|_| {
                MobileCoreError::TerminalBackendFailure(
                    "terminal backend session actor dropped response".to_string(),
                )
            })
        });

        match response {
            Ok(result) => result,
            Err(error) => {
                self.remove_session_if_present(handle_id);
                Err(error)
            }
        }
    }

    fn connect_tls_client(&self, target: &TlsTarget) -> Result<BmuxClient> {
        let timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
        self.runtime.block_on(async {
            let mut attempts = Vec::new();
            for use_compression in [true, false] {
                match Self::connect_tls_client_once(target, timeout, use_compression).await {
                    Ok(client) => return Ok(client),
                    Err(error) => attempts.push((use_compression, error)),
                }
            }
            let errors = attempts
                .iter()
                .map(|(use_compression, error)| {
                    format!(
                        "{} mode: {error}",
                        if *use_compression {
                            "compressed"
                        } else {
                            "raw"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            Err(MobileCoreError::TerminalBackendFailure(format!(
                "failed connecting tls target '{}': {errors}",
                target.label
            )))
        })
    }

    async fn connect_tls_client_once(
        target: &TlsTarget,
        timeout: Duration,
        use_compression: bool,
    ) -> Result<BmuxClient> {
        let connector = build_tls_connector(target)?;
        let address = format!("{}:{}", target.host, target.port);
        let tcp_stream = tokio::time::timeout(timeout, TcpStream::connect(&address))
            .await
            .map_err(|_| {
                MobileCoreError::TerminalBackendFailure(format!(
                    "timed out connecting tls target '{}'",
                    target.label
                ))
            })?
            .map_err(|error| {
                MobileCoreError::TerminalBackendFailure(format!(
                    "failed connecting tls target '{}': {error}",
                    target.label
                ))
            })?;
        let server_name = ServerName::try_from(target.server_name.clone()).map_err(|_| {
            MobileCoreError::TerminalBackendFailure(format!(
                "invalid tls server name '{}'",
                target.server_name
            ))
        })?;
        let tls_stream = connector
            .connect(server_name, tcp_stream)
            .await
            .map_err(|error| {
                MobileCoreError::TerminalBackendFailure(format!(
                    "tls handshake failed for '{}': {error}",
                    target.label
                ))
            })?;
        let erased = if use_compression {
            ErasedIpcStream::new(Box::new(CompressedStream::new(tls_stream, 1)))
        } else {
            ErasedIpcStream::new(Box::new(tls_stream))
        };
        BmuxClient::connect_with_bridge_stream(
            erased,
            timeout,
            "bmux-mobile-terminal-tls".to_string(),
            Uuid::new_v4(),
        )
        .await
        .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))
    }

    fn connect_iroh_client(&self, target: &IrohTarget) -> Result<BmuxClient> {
        if target.require_ssh_auth {
            return Err(MobileCoreError::TerminalBackendFailure(
                "iroh auth=ssh targets are not supported on mobile yet".to_string(),
            ));
        }

        let timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
        self.runtime.block_on(async {
            let mut compressed_errors = Vec::new();
            if !matches!(target.compression_mode, IrohCompressionMode::None) {
                for attempt in 0..=IROH_COMPRESSED_RETRY_COUNT {
                    match Self::connect_iroh_client_once(target, timeout, true).await {
                        Ok(client) => return Ok(client),
                        Err(error) => {
                            compressed_errors
                                .push(format!("attempt {}: {}", attempt + 1, error.message));
                            if matches!(target.compression_mode, IrohCompressionMode::Zstd)
                                && !error.is_timeout()
                            {
                                break;
                            }
                        }
                    }
                }
            }

            if matches!(target.compression_mode, IrohCompressionMode::Zstd) {
                let errors = compressed_errors.join("; ");
                return Err(MobileCoreError::TerminalBackendFailure(format!(
                    "failed connecting iroh target '{}': compressed mode: {errors}",
                    target.label
                )));
            }

            match Self::connect_iroh_client_once(target, timeout, false).await {
                Ok(client) => Ok(client),
                Err(raw_error) => {
                    if compressed_errors.is_empty() {
                        Err(MobileCoreError::TerminalBackendFailure(format!(
                            "failed connecting iroh target '{}': raw mode: {}",
                            target.label, raw_error.message
                        )))
                    } else {
                        let compressed_summary = compressed_errors.join("; ");
                        Err(MobileCoreError::TerminalBackendFailure(format!(
                            "failed connecting iroh target '{}': compressed mode: {compressed_summary}; raw mode: {}",
                            target.label, raw_error.message
                        )))
                    }
                }
            }
        })
    }

    async fn connect_iroh_client_once(
        target: &IrohTarget,
        timeout: Duration,
        use_compression: bool,
    ) -> std::result::Result<BmuxClient, IrohConnectAttemptError> {
        let endpoint = Endpoint::builder(presets::N0)
            .alpns(vec![BMUX_IROH_ALPN.to_vec()])
            .bind()
            .await
            .map_err(|error| {
                IrohConnectAttemptError::other(format!("failed binding iroh endpoint: {error}"))
            })?;

        let endpoint_id: EndpointId = target.endpoint_id.parse().map_err(|error| {
            IrohConnectAttemptError::other(format!(
                "invalid iroh endpoint '{}': {error}",
                target.endpoint_id
            ))
        })?;
        let remote_addr = if let Some(relay_url) = target.relay_url.as_deref() {
            let relay = relay_url.parse().map_err(|error| {
                IrohConnectAttemptError::other(format!(
                    "invalid iroh relay url '{relay_url}': {error}"
                ))
            })?;
            EndpointAddr::new(endpoint_id).with_relay_url(relay)
        } else {
            EndpointAddr::new(endpoint_id)
        };

        let connection =
            tokio::time::timeout(timeout, endpoint.connect(remote_addr, BMUX_IROH_ALPN))
                .await
                .map_err(|_| {
                    IrohConnectAttemptError::timeout(format!(
                        "timed out connecting iroh target '{}'",
                        target.label
                    ))
                })?
                .map_err(|error| {
                    IrohConnectAttemptError::other(format!(
                        "failed connecting iroh target '{}': {error}",
                        target.label
                    ))
                })?;
        let (mut send, mut recv) = connection.open_bi().await.map_err(|error| {
            IrohConnectAttemptError::other(format!(
                "failed opening iroh stream for '{}': {error}",
                target.label
            ))
        })?;

        let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
        let (mut bridge_read, mut bridge_write) = io::split(bridge_stream);
        tokio::spawn(async move {
            let _ = io::copy(&mut recv, &mut bridge_write).await;
            let _ = bridge_write.shutdown().await;
        });
        tokio::spawn(async move {
            let _endpoint_keepalive = endpoint;
            let _connection_keepalive = connection;
            let _ = io::copy(&mut bridge_read, &mut send).await;
            let _ = send.finish();
        });

        let erased = if use_compression {
            ErasedIpcStream::new(Box::new(CompressedStream::new(client_stream, 1)))
        } else {
            ErasedIpcStream::new(Box::new(client_stream))
        };

        BmuxClient::connect_with_bridge_stream(
            erased,
            timeout,
            "bmux-mobile-terminal-iroh".to_string(),
            Uuid::new_v4(),
        )
        .await
        .map_err(Self::classify_iroh_connect_error)
    }

    fn classify_iroh_connect_error(error: ClientError) -> IrohConnectAttemptError {
        match error {
            ClientError::Timeout(timeout) => {
                IrohConnectAttemptError::timeout(format!("request timed out after {timeout:?}"))
            }
            ClientError::ProtocolIncompatible { reason } => IrohConnectAttemptError::protocol(
                format!("protocol negotiation failed: {reason:?}"),
            ),
            ClientError::UnexpectedEnvelopeKind { expected, actual } => {
                IrohConnectAttemptError::protocol(format!(
                    "unexpected envelope kind: expected {expected:?}, got {actual:?}"
                ))
            }
            ClientError::UnexpectedResponse(kind) => {
                IrohConnectAttemptError::protocol(format!("unexpected response payload: {kind}"))
            }
            ClientError::RequestIdMismatch { expected, actual } => {
                IrohConnectAttemptError::protocol(format!(
                    "request id mismatch (expected {expected}, got {actual})"
                ))
            }
            ClientError::Transport(transport_error) => {
                Self::classify_iroh_transport_error(transport_error)
            }
            other => IrohConnectAttemptError::other(other.to_string()),
        }
    }

    fn classify_iroh_transport_error(error: IpcTransportError) -> IrohConnectAttemptError {
        match error {
            IpcTransportError::Io(io_error)
                if io_error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                IrohConnectAttemptError::protocol(format!("transport error: I/O error: {io_error}"))
            }
            IpcTransportError::FrameDecode(decode_error) => IrohConnectAttemptError::protocol(
                format!("transport error: frame decoding failed: {decode_error}"),
            ),
            IpcTransportError::FrameEncode(encode_error) => IrohConnectAttemptError::protocol(
                format!("transport error: frame encoding failed: {encode_error}"),
            ),
            IpcTransportError::UnsupportedEndpoint => IrohConnectAttemptError::other(
                "transport error: unsupported endpoint for this platform".to_string(),
            ),
            IpcTransportError::Io(io_error) => {
                IrohConnectAttemptError::other(format!("transport error: I/O error: {io_error}"))
            }
        }
    }

    fn session_selector_from_value(value: &str) -> SessionSelector {
        Uuid::parse_str(value).map_or_else(
            |_| SessionSelector::ByName(value.to_string()),
            SessionSelector::ById,
        )
    }

    fn normalize_session_value(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
    }

    const fn is_session_not_found(error: &ClientError) -> bool {
        matches!(
            error,
            ClientError::ServerError {
                code: ErrorCode::NotFound,
                ..
            }
        )
    }

    async fn attach_with_selector(
        client: &mut BmuxClient,
        selector: SessionSelector,
    ) -> std::result::Result<(Uuid, bool), ClientError> {
        let grant = client.attach_grant(selector).await?;
        let info = client.open_attach_stream_info(&grant).await?;
        Ok((info.session_id, info.can_write))
    }

    async fn attach_auto_session(client: &mut BmuxClient) -> Result<(Uuid, bool)> {
        let sessions = client
            .list_sessions()
            .await
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;

        let selector =
            if let Some(entry) = sessions.into_iter().max_by_key(|entry| entry.client_count) {
                SessionSelector::ById(entry.id)
            } else {
                let session_id = client
                    .new_session(None)
                    .await
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
                SessionSelector::ById(session_id)
            };

        Self::attach_with_selector(client, selector)
            .await
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))
    }
}

impl TerminalBackend for RemoteTerminalBackend {
    fn open(
        &self,
        target: &TargetRecord,
        session: Option<String>,
        rows: u16,
        cols: u16,
    ) -> Result<BackendSessionHandle> {
        let parsed = Self::parse_target(target)?;
        let mut client = self.connect_client(&parsed)?;

        let explicit_session = Self::normalize_session_value(session.as_deref());
        let preferred_session = explicit_session
            .clone()
            .or_else(|| Self::normalize_session_value(target.default_session.as_deref()));

        let (handle_id, session_id, can_write) = self.runtime.block_on(async {
            let attach_info = if let Some(preferred) = preferred_session {
                let selector = Self::session_selector_from_value(&preferred);
                match Self::attach_with_selector(&mut client, selector).await {
                    Ok(info) => Ok(info),
                    Err(error)
                        if explicit_session.is_none() && Self::is_session_not_found(&error) =>
                    {
                        Self::attach_auto_session(&mut client).await
                    }
                    Err(error) => Err(MobileCoreError::TerminalBackendFailure(error.to_string())),
                }
            } else {
                Self::attach_auto_session(&mut client).await
            }?;

            let _ = client
                .attach_set_viewport(attach_info.0, cols, rows)
                .await
                .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;

            Ok::<(Uuid, Uuid, bool), MobileCoreError>((
                Uuid::new_v4(),
                attach_info.0,
                attach_info.1,
            ))
        })?;

        let (command_tx, command_rx) = mpsc::channel(SESSION_COMMAND_BUFFER);
        drop(
            self.runtime
                .spawn(run_remote_terminal_session(client, session_id, command_rx)),
        );
        self.lock_sessions()?
            .insert(handle_id, RemoteTerminalSession { command_tx });

        Ok(BackendSessionHandle {
            id: handle_id,
            session_id,
            can_write,
        })
    }

    fn poll_output(&self, handle_id: Uuid, max_bytes: usize) -> Result<Vec<u8>> {
        self.call_session(handle_id, |response| SessionCommand::PollOutput {
            max_bytes,
            response,
        })
    }

    fn write_input(&self, handle_id: Uuid, bytes: &[u8]) -> Result<()> {
        self.call_session(handle_id, |response| SessionCommand::WriteInput {
            bytes: bytes.to_vec(),
            response,
        })
    }

    fn resize(&self, handle_id: Uuid, rows: u16, cols: u16) -> Result<()> {
        self.call_session(handle_id, |response| SessionCommand::Resize {
            rows,
            cols,
            response,
        })
    }

    fn close(&self, handle_id: Uuid) -> Result<()> {
        let session = self
            .lock_sessions()?
            .remove(&handle_id)
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(handle_id.to_string()))?;

        self.runtime.block_on(async {
            let (response_tx, response_rx) = oneshot::channel();
            if session
                .command_tx
                .send(SessionCommand::Close {
                    response: response_tx,
                })
                .await
                .is_err()
            {
                return Ok(());
            }
            response_rx.await.ok().map_or(Ok(()), |result| result)
        })
    }
}

async fn run_remote_terminal_session(
    mut client: BmuxClient,
    session_id: Uuid,
    mut command_rx: mpsc::Receiver<SessionCommand>,
) {
    while let Some(command) = command_rx.recv().await {
        match command {
            SessionCommand::PollOutput {
                max_bytes,
                response,
            } => {
                let result = client
                    .attach_output(session_id, max_bytes)
                    .await
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
                let _ = response.send(result);
            }
            SessionCommand::WriteInput { bytes, response } => {
                let result = client
                    .attach_input(session_id, bytes)
                    .await
                    .map(|_| ())
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
                let _ = response.send(result);
            }
            SessionCommand::Resize {
                rows,
                cols,
                response,
            } => {
                let result = client
                    .attach_set_viewport(session_id, cols, rows)
                    .await
                    .map(|_| ())
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
                let _ = response.send(result);
            }
            SessionCommand::Close { response } => {
                let result = client
                    .detach()
                    .await
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
                let _ = response.send(result);
                return;
            }
        }
    }

    let _ = client.detach().await;
}

#[derive(Debug, Clone)]
enum ParsedRemoteTarget {
    Iroh(IrohTarget),
    Tls(TlsTarget),
}

#[derive(Debug, Clone)]
struct TlsTarget {
    label: String,
    host: String,
    port: u16,
    server_name: String,
    connect_timeout_ms: u64,
}

#[derive(Debug, Clone)]
struct IrohTarget {
    label: String,
    endpoint_id: String,
    relay_url: Option<String>,
    require_ssh_auth: bool,
    compression_mode: IrohCompressionMode,
    connect_timeout_ms: u64,
}

fn parse_tls_target(value: &str) -> Result<ParsedRemoteTarget> {
    if value.starts_with("https://") {
        let parsed = Url::parse(value).map_err(|error| {
            MobileCoreError::InvalidTarget(format!("invalid https target '{value}': {error}"))
        })?;
        let host = parsed.host_str().ok_or_else(|| {
            MobileCoreError::InvalidTarget(format!("https target '{value}' must include a host"))
        })?;
        return Ok(ParsedRemoteTarget::Tls(TlsTarget {
            label: value.to_string(),
            host: host.to_string(),
            port: parsed.port().unwrap_or(443),
            server_name: host.to_string(),
            connect_timeout_ms: 8_000,
        }));
    }

    let raw = value.strip_prefix("tls://").ok_or_else(|| {
        MobileCoreError::InvalidTarget(format!("tls target '{value}' must start with tls://"))
    })?;
    let (host, port) = parse_host_port_with_default(raw, 443)?;
    Ok(ParsedRemoteTarget::Tls(TlsTarget {
        label: value.to_string(),
        host: host.clone(),
        port,
        server_name: host,
        connect_timeout_ms: 8_000,
    }))
}

fn parse_iroh_target(value: &str) -> Result<ParsedRemoteTarget> {
    let parsed = Url::parse(value).map_err(|error| {
        MobileCoreError::InvalidTarget(format!("invalid iroh target '{value}': {error}"))
    })?;
    if parsed.scheme() != "iroh" {
        return Err(MobileCoreError::InvalidTarget(format!(
            "iroh target '{value}' must start with iroh://"
        )));
    }
    let endpoint_id = parsed.host_str().ok_or_else(|| {
        MobileCoreError::InvalidTarget(format!("iroh target '{value}' must include endpoint id"))
    })?;

    let mut relay_url = None;
    let mut require_ssh_auth = false;
    let mut compression_mode = IrohCompressionMode::Auto;
    for (key, value) in parsed.query_pairs() {
        if key.eq_ignore_ascii_case("relay") && !value.is_empty() {
            relay_url = Some(value.to_string());
        }
        if key.eq_ignore_ascii_case("auth") && value.eq_ignore_ascii_case("ssh") {
            require_ssh_auth = true;
        }
        if key.eq_ignore_ascii_case("compression") {
            compression_mode = IrohCompressionMode::parse(&value)?;
        }
    }

    Ok(ParsedRemoteTarget::Iroh(IrohTarget {
        label: value.to_string(),
        endpoint_id: endpoint_id.to_string(),
        relay_url,
        require_ssh_auth,
        compression_mode,
        connect_timeout_ms: DEFAULT_IROH_CONNECT_TIMEOUT_MS,
    }))
}

fn parse_host_port_with_default(value: &str, default_port: u16) -> Result<(String, u16)> {
    let raw = value.trim();
    if let Some((host, port_raw)) = raw.rsplit_once(':') {
        if port_raw.is_empty() {
            return Ok((raw.to_string(), default_port));
        }
        let port = port_raw.parse::<u16>().map_err(|error| {
            MobileCoreError::InvalidTarget(format!("invalid port in '{value}': {error}"))
        })?;
        if host.trim().is_empty() {
            return Err(MobileCoreError::InvalidTarget(format!(
                "tls target '{value}' must include host"
            )));
        }
        return Ok((host.to_string(), port));
    }
    if raw.is_empty() {
        return Err(MobileCoreError::InvalidTarget(format!(
            "tls target '{value}' must include host"
        )));
    }
    Ok((raw.to_string(), default_port))
}

fn build_tls_connector(target: &TlsTarget) -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(MobileCoreError::TerminalBackendFailure(format!(
            "no tls trust roots available for target '{}'",
            target.label
        )));
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iroh_target_extracts_auth_and_relay_query() {
        let ParsedRemoteTarget::Iroh(target) = parse_iroh_target(
            "iroh://abcd1234?relay=https://relay.example.com&auth=ssh&compression=zstd",
        )
        .expect("iroh target should parse") else {
            panic!("expected iroh target")
        };

        assert_eq!(target.endpoint_id, "abcd1234");
        assert_eq!(
            target.relay_url.as_deref(),
            Some("https://relay.example.com")
        );
        assert!(target.require_ssh_auth);
        assert!(matches!(target.compression_mode, IrohCompressionMode::Zstd));
    }
}
