use crate::connection::{TerminalMouseButton, TerminalMouseEvent, TerminalMouseEventKind};
use crate::error::{MobileCoreError, Result};
use crate::target::{TargetRecord, TargetTransport};
use bmux_attach_pipeline::mouse as attach_mouse;
use bmux_attach_pipeline::render::visible_scene_pane_ids;
use bmux_attach_pipeline::{AttachChunkApplyOutcome, AttachScenePipeline, AttachViewport};
use bmux_client::{BmuxClient, ClientError, ServerEvent, StreamingBmuxClient};
use bmux_ipc::compressed_stream::CompressedStream;
use bmux_ipc::transport::{ErasedIpcStream, IpcTransportError};
use bmux_ipc::{
    AttachPaneChunk, AttachViewComponent, CAPABILITY_ATTACH_PANE_SNAPSHOT, ErrorCode,
    SessionSelector,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::presets};
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::io;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsConnector;
use url::Url;
use uuid::Uuid;

const BMUX_IROH_ALPN: &[u8] = b"bmux/gateway/iroh/1";
const SESSION_COMMAND_BUFFER: usize = 64;
const DEFAULT_IROH_CONNECT_TIMEOUT_MS: u64 = 7_500;
const DEFAULT_IROH_HELLO_PROBE_TIMEOUT_MS: u64 = 2_500;
const IROH_COMPRESSED_RETRY_COUNT: usize = 0;
const OUTPUT_QUEUE_MAX_BYTES: usize = 4 * 1024 * 1024;
const EVENT_TRIGGER_FETCH_MAX_BYTES: usize = 256 * 1024;
const SESSION_KEEPALIVE_INTERVAL_MS: u64 = 10_000;

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

impl IrohConnectAttemptErrorKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Protocol => "protocol",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IrohConnectStage {
    Connect,
    OpenBi,
    HelloV2,
}

impl IrohConnectStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::OpenBi => "open_bi",
            Self::HelloV2 => "hello_v2",
        }
    }
}

#[derive(Debug, Clone)]
struct IrohConnectAttemptError {
    kind: IrohConnectAttemptErrorKind,
    stage: IrohConnectStage,
    elapsed_ms: u64,
    message: String,
}

impl IrohConnectAttemptError {
    const fn timeout(stage: IrohConnectStage, elapsed_ms: u64, message: String) -> Self {
        Self {
            kind: IrohConnectAttemptErrorKind::Timeout,
            stage,
            elapsed_ms,
            message,
        }
    }

    const fn protocol(stage: IrohConnectStage, elapsed_ms: u64, message: String) -> Self {
        Self {
            kind: IrohConnectAttemptErrorKind::Protocol,
            stage,
            elapsed_ms,
            message,
        }
    }

    const fn other(stage: IrohConnectStage, elapsed_ms: u64, message: String) -> Self {
        Self {
            kind: IrohConnectAttemptErrorKind::Other,
            stage,
            elapsed_ms,
            message,
        }
    }

    const fn is_timeout(&self) -> bool {
        matches!(self.kind, IrohConnectAttemptErrorKind::Timeout)
    }

    fn report_fragment(&self, mode_label: &str, attempt_number: usize) -> String {
        format!(
            "{} attempt {}: phase={} kind={} elapsed={}ms detail={}",
            mode_label,
            attempt_number,
            self.stage.as_str(),
            self.kind.as_str(),
            self.elapsed_ms,
            self.message
        )
    }
}

#[derive(Debug, Clone)]
struct IrohAttemptFailure {
    use_compression: bool,
    attempt_number: usize,
    error: IrohConnectAttemptError,
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

    /// Sends a terminal mouse event for an open session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session handle is unknown or the remote mouse
    /// routing call fails.
    fn mouse_event(&self, handle_id: Uuid, event: &TerminalMouseEvent) -> Result<()>;

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
    actor_state: Arc<Mutex<SessionActorState>>,
}

#[derive(Debug, Default)]
struct SessionActorState {
    terminated_reason: Option<String>,
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
    MouseEvent {
        event: TerminalMouseEvent,
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
    iroh_mode_cache: Mutex<BTreeMap<String, bool>>,
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
            iroh_mode_cache: Mutex::new(BTreeMap::new()),
        }
    }

    fn iroh_mode_cache_key(target: &IrohTarget) -> String {
        format!(
            "{}|{}",
            target.endpoint_id,
            target.relay_url.as_deref().unwrap_or_default()
        )
    }

    fn cached_iroh_mode(&self, target: &IrohTarget) -> Option<bool> {
        let key = Self::iroh_mode_cache_key(target);
        self.iroh_mode_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).copied())
    }

    fn remember_iroh_mode_success(&self, target: &IrohTarget, use_compression: bool) {
        if let Ok(mut cache) = self.iroh_mode_cache.lock() {
            let key = Self::iroh_mode_cache_key(target);
            cache.insert(key, use_compression);
        }
    }

    fn iroh_mode_order(&self, target: &IrohTarget) -> Vec<bool> {
        match target.compression_mode {
            IrohCompressionMode::None => vec![false],
            IrohCompressionMode::Zstd => vec![true],
            IrohCompressionMode::Auto => self
                .cached_iroh_mode(target)
                .map_or_else(|| vec![true, false], |cached| vec![cached, !cached]),
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

    fn session_actor_state(&self, handle_id: Uuid) -> Result<Arc<Mutex<SessionActorState>>> {
        self.lock_sessions()?
            .get(&handle_id)
            .map(|session| Arc::clone(&session.actor_state))
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
        let actor_state = self.session_actor_state(handle_id)?;
        if let Some(reason) = session_terminated_reason(&actor_state) {
            self.remove_session_if_present(handle_id);
            return Err(MobileCoreError::TerminalBackendFailure(reason));
        }

        let response = self.runtime.block_on(async {
            let (response_tx, response_rx) = oneshot::channel();
            command_tx
                .send(command_builder(response_tx))
                .await
                .map_err(|_| {
                    MobileCoreError::TerminalBackendFailure(
                        session_terminated_reason(&actor_state).unwrap_or_else(|| {
                            "terminal backend session actor unavailable".to_string()
                        }),
                    )
                })?;
            response_rx.await.map_err(|_| {
                MobileCoreError::TerminalBackendFailure(
                    session_terminated_reason(&actor_state).unwrap_or_else(|| {
                        "terminal backend session actor dropped response".to_string()
                    }),
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

        let connect_timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
        let hello_probe_timeout = Duration::from_millis(
            target
                .connect_timeout_ms
                .clamp(1, DEFAULT_IROH_HELLO_PROBE_TIMEOUT_MS),
        );
        let hello_retry_timeout = connect_timeout;
        let mode_order = self.iroh_mode_order(target);

        let result = self.runtime.block_on(async {
            let mut failures = Vec::new();
            for use_compression in mode_order {
                let max_attempts = if use_compression {
                    IROH_COMPRESSED_RETRY_COUNT.saturating_add(1)
                } else {
                    1
                };

                let mut mode_attempt_number: usize = 0;
                for _ in 0..max_attempts {
                    mode_attempt_number += 1;
                    match Self::connect_iroh_client_once(
                        target,
                        connect_timeout,
                        hello_probe_timeout,
                        use_compression,
                    )
                    .await
                    {
                        Ok(client) => return Ok((client, use_compression)),
                        Err(error) => {
                            let should_break = use_compression
                                && matches!(target.compression_mode, IrohCompressionMode::Zstd)
                                && !error.is_timeout();
                            let should_retry_with_extended_hello =
                                Self::should_retry_with_extended_hello_timeout(
                                    &error,
                                    hello_probe_timeout,
                                    hello_retry_timeout,
                                );
                            failures.push(IrohAttemptFailure {
                                use_compression,
                                attempt_number: mode_attempt_number,
                                error,
                            });

                            if should_retry_with_extended_hello {
                                mode_attempt_number += 1;
                                match Self::connect_iroh_client_once(
                                    target,
                                    connect_timeout,
                                    hello_retry_timeout,
                                    use_compression,
                                )
                                .await
                                {
                                    Ok(client) => return Ok((client, use_compression)),
                                    Err(retry_error) => {
                                        let retry_should_break = use_compression
                                            && matches!(
                                                target.compression_mode,
                                                IrohCompressionMode::Zstd
                                            )
                                            && !retry_error.is_timeout();
                                        failures.push(IrohAttemptFailure {
                                            use_compression,
                                            attempt_number: mode_attempt_number,
                                            error: retry_error,
                                        });
                                        if retry_should_break {
                                            break;
                                        }
                                    }
                                }
                            } else if should_break {
                                break;
                            }
                        }
                    }
                }
            }

            Err(Self::format_iroh_connect_failures(target, &failures))
        });

        match result {
            Ok((client, use_compression)) => {
                self.remember_iroh_mode_success(target, use_compression);
                Ok(client)
            }
            Err(error_message) => Err(MobileCoreError::TerminalBackendFailure(error_message)),
        }
    }

    async fn connect_iroh_client_once(
        target: &IrohTarget,
        connect_timeout: Duration,
        hello_timeout: Duration,
        use_compression: bool,
    ) -> std::result::Result<BmuxClient, IrohConnectAttemptError> {
        let started = Instant::now();
        let endpoint = Endpoint::builder(presets::N0)
            .alpns(vec![BMUX_IROH_ALPN.to_vec()])
            .bind()
            .await
            .map_err(|error| {
                IrohConnectAttemptError::other(
                    IrohConnectStage::Connect,
                    Self::elapsed_ms(started),
                    format!("failed binding iroh endpoint: {error}"),
                )
            })?;

        let endpoint_id: EndpointId = target.endpoint_id.parse().map_err(|error| {
            IrohConnectAttemptError::other(
                IrohConnectStage::Connect,
                Self::elapsed_ms(started),
                format!("invalid iroh endpoint '{}': {error}", target.endpoint_id),
            )
        })?;
        let remote_addr = if let Some(relay_url) = target.relay_url.as_deref() {
            let relay = relay_url.parse().map_err(|error| {
                IrohConnectAttemptError::other(
                    IrohConnectStage::Connect,
                    Self::elapsed_ms(started),
                    format!("invalid iroh relay url '{relay_url}': {error}"),
                )
            })?;
            EndpointAddr::new(endpoint_id).with_relay_url(relay)
        } else {
            EndpointAddr::new(endpoint_id)
        };

        let connection = tokio::time::timeout(
            connect_timeout,
            endpoint.connect(remote_addr, BMUX_IROH_ALPN),
        )
        .await
        .map_err(|_| {
            IrohConnectAttemptError::timeout(
                IrohConnectStage::Connect,
                Self::elapsed_ms(started),
                format!("timed out connecting iroh target '{}'", target.label),
            )
        })?
        .map_err(|error| {
            IrohConnectAttemptError::other(
                IrohConnectStage::Connect,
                Self::elapsed_ms(started),
                format!("failed connecting iroh target '{}': {error}", target.label),
            )
        })?;
        let (mut send, mut recv) = tokio::time::timeout(hello_timeout, connection.open_bi())
            .await
            .map_err(|_| {
                IrohConnectAttemptError::timeout(
                    IrohConnectStage::OpenBi,
                    Self::elapsed_ms(started),
                    format!("timed out opening iroh stream for '{}'", target.label),
                )
            })?
            .map_err(|error| {
                IrohConnectAttemptError::other(
                    IrohConnectStage::OpenBi,
                    Self::elapsed_ms(started),
                    format!("failed opening iroh stream for '{}': {error}", target.label),
                )
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
            hello_timeout,
            "bmux-mobile-terminal-iroh".to_string(),
            Uuid::new_v4(),
        )
        .await
        .map_err(|error| {
            Self::classify_iroh_connect_error(
                error,
                IrohConnectStage::HelloV2,
                Self::elapsed_ms(started),
            )
        })
    }

    fn classify_iroh_connect_error(
        error: ClientError,
        stage: IrohConnectStage,
        elapsed_ms: u64,
    ) -> IrohConnectAttemptError {
        match error {
            ClientError::Timeout(timeout) => IrohConnectAttemptError::timeout(
                stage,
                elapsed_ms,
                format!("request timed out after {timeout:?}"),
            ),
            ClientError::ProtocolIncompatible { reason } => IrohConnectAttemptError::protocol(
                stage,
                elapsed_ms,
                format!("protocol negotiation failed: {reason:?}"),
            ),
            ClientError::UnexpectedEnvelopeKind { expected, actual } => {
                IrohConnectAttemptError::protocol(
                    stage,
                    elapsed_ms,
                    format!("unexpected envelope kind: expected {expected:?}, got {actual:?}"),
                )
            }
            ClientError::UnexpectedResponse(kind) => IrohConnectAttemptError::protocol(
                stage,
                elapsed_ms,
                format!("unexpected response payload: {kind}"),
            ),
            ClientError::RequestIdMismatch { expected, actual } => {
                IrohConnectAttemptError::protocol(
                    stage,
                    elapsed_ms,
                    format!("request id mismatch (expected {expected}, got {actual})"),
                )
            }
            ClientError::Transport(transport_error) => {
                Self::classify_iroh_transport_error(transport_error, stage, elapsed_ms)
            }
            other => IrohConnectAttemptError::other(stage, elapsed_ms, other.to_string()),
        }
    }

    fn classify_iroh_transport_error(
        error: IpcTransportError,
        stage: IrohConnectStage,
        elapsed_ms: u64,
    ) -> IrohConnectAttemptError {
        match error {
            IpcTransportError::Io(io_error)
                if io_error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                IrohConnectAttemptError::protocol(
                    stage,
                    elapsed_ms,
                    format!("transport error: I/O error: {io_error}"),
                )
            }
            IpcTransportError::FrameDecode(decode_error) => IrohConnectAttemptError::protocol(
                stage,
                elapsed_ms,
                format!("transport error: frame decoding failed: {decode_error}"),
            ),
            IpcTransportError::FrameEncode(encode_error) => IrohConnectAttemptError::protocol(
                stage,
                elapsed_ms,
                format!("transport error: frame encoding failed: {encode_error}"),
            ),
            IpcTransportError::UnsupportedEndpoint => IrohConnectAttemptError::other(
                stage,
                elapsed_ms,
                "transport error: unsupported endpoint for this platform".to_string(),
            ),
            IpcTransportError::Io(io_error) => IrohConnectAttemptError::other(
                stage,
                elapsed_ms,
                format!("transport error: I/O error: {io_error}"),
            ),
        }
    }

    fn format_iroh_connect_failures(
        target: &IrohTarget,
        failures: &[IrohAttemptFailure],
    ) -> String {
        if failures.is_empty() {
            return format!(
                "failed connecting iroh target '{}': no attempts were executed",
                target.label
            );
        }

        let compressed = failures
            .iter()
            .filter(|failure| failure.use_compression)
            .map(|failure| {
                failure
                    .error
                    .report_fragment("compressed", failure.attempt_number)
            })
            .collect::<Vec<_>>();
        let raw = failures
            .iter()
            .filter(|failure| !failure.use_compression)
            .map(|failure| failure.error.report_fragment("raw", failure.attempt_number))
            .collect::<Vec<_>>();
        let compressed_summary = if compressed.is_empty() {
            "not attempted".to_string()
        } else {
            compressed.join("; ")
        };
        let raw_summary = if raw.is_empty() {
            "not attempted".to_string()
        } else {
            raw.join("; ")
        };

        let classification = Self::classify_iroh_open_failure(failures);
        let timeline = failures
            .iter()
            .map(|failure| {
                failure.error.report_fragment(
                    if failure.use_compression {
                        "compressed"
                    } else {
                        "raw"
                    },
                    failure.attempt_number,
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");

        format!(
            "failed connecting iroh target '{}': {classification}; compressed mode: {compressed_summary}; raw mode: {raw_summary}; open timeline: {timeline}",
            target.label
        )
    }

    fn classify_iroh_open_failure(failures: &[IrohAttemptFailure]) -> &'static str {
        let reached_stream = failures
            .iter()
            .any(|failure| !matches!(failure.error.stage, IrohConnectStage::Connect));
        let has_connect_timeout = failures.iter().any(|failure| {
            failure.error.stage == IrohConnectStage::Connect && failure.error.is_timeout()
        });
        let has_hello_timeout = failures.iter().any(|failure| {
            failure.error.stage == IrohConnectStage::HelloV2 && failure.error.is_timeout()
        });
        let has_hello_eof = failures.iter().any(|failure| {
            failure.error.stage == IrohConnectStage::HelloV2
                && failure.error.message.to_ascii_lowercase().contains("eof")
        });

        if reached_stream && has_hello_timeout && has_hello_eof {
            "endpoint reachable, but bmux handshake failed (timeout + early EOF)"
        } else if reached_stream && has_hello_eof {
            "endpoint reachable, but remote closed bmux handshake stream early"
        } else if reached_stream && has_hello_timeout {
            "endpoint reachable, but bmux handshake timed out"
        } else if has_connect_timeout {
            "unable to establish relay route before timeout"
        } else {
            "iroh connect/open handshake failed"
        }
    }

    fn should_retry_with_extended_hello_timeout(
        error: &IrohConnectAttemptError,
        hello_probe_timeout: Duration,
        hello_retry_timeout: Duration,
    ) -> bool {
        hello_retry_timeout > hello_probe_timeout
            && error.is_timeout()
            && matches!(
                error.stage,
                IrohConnectStage::OpenBi | IrohConnectStage::HelloV2
            )
    }

    fn elapsed_ms(started: Instant) -> u64 {
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
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
    #[allow(clippy::too_many_lines)]
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

        let (handle_id, session_id, can_write, initial_snapshot, streaming_client) =
            self.runtime.block_on(async {
                let attach_info = if let Some(preferred) = preferred_session {
                    let selector = Self::session_selector_from_value(&preferred);
                    match Self::attach_with_selector(&mut client, selector).await {
                        Ok(info) => Ok(info),
                        Err(error)
                            if explicit_session.is_none() && Self::is_session_not_found(&error) =>
                        {
                            Self::attach_auto_session(&mut client).await
                        }
                        Err(error) => {
                            Err(MobileCoreError::TerminalBackendFailure(error.to_string()))
                        }
                    }
                } else {
                    Self::attach_auto_session(&mut client).await
                }?;

                let _ = client
                    .attach_set_viewport(attach_info.0, cols, rows)
                    .await
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;

                let mut streaming_client = StreamingBmuxClient::from_client(client)
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
                streaming_client
                    .enable_event_push()
                    .await
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;

                let initial_snapshot = if let Ok(snapshot) = streaming_client
                    .attach_snapshot(attach_info.0, EVENT_TRIGGER_FETCH_MAX_BYTES)
                    .await
                {
                    Some(snapshot)
                } else {
                    streaming_client
                        .attach_layout(attach_info.0)
                        .await
                        .ok()
                        .map(|layout| bmux_client::AttachSnapshotState {
                            context_id: layout.context_id,
                            session_id: layout.session_id,
                            focused_pane_id: layout.focused_pane_id,
                            panes: layout.panes,
                            layout_root: layout.layout_root,
                            scene: layout.scene,
                            chunks: Vec::new(),
                            pane_mouse_protocols: Vec::new(),
                            pane_input_modes: Vec::new(),
                            zoomed: layout.zoomed,
                        })
                };

                Ok::<
                    (
                        Uuid,
                        Uuid,
                        bool,
                        Option<bmux_client::AttachSnapshotState>,
                        StreamingBmuxClient,
                    ),
                    MobileCoreError,
                >((
                    Uuid::new_v4(),
                    attach_info.0,
                    attach_info.1,
                    initial_snapshot,
                    streaming_client,
                ))
            })?;

        let (command_tx, command_rx) = mpsc::channel(SESSION_COMMAND_BUFFER);
        let actor_state = Arc::new(Mutex::new(SessionActorState::default()));
        drop(self.runtime.spawn(run_remote_terminal_session(
            streaming_client,
            session_id,
            initial_snapshot,
            AttachViewport {
                cols,
                rows,
                status_top_inset: 0,
                status_bottom_inset: 0,
            },
            command_rx,
            Arc::clone(&actor_state),
        )));
        self.lock_sessions()?.insert(
            handle_id,
            RemoteTerminalSession {
                command_tx,
                actor_state,
            },
        );

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

    fn mouse_event(&self, handle_id: Uuid, event: &TerminalMouseEvent) -> Result<()> {
        self.call_session(handle_id, |response| SessionCommand::MouseEvent {
            event: *event,
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

fn set_session_terminated_reason(actor_state: &Arc<Mutex<SessionActorState>>, reason: String) {
    if let Ok(mut state) = actor_state.lock()
        && state.terminated_reason.is_none()
    {
        state.terminated_reason = Some(reason);
    }
}

fn session_terminated_reason(actor_state: &Arc<Mutex<SessionActorState>>) -> Option<String> {
    actor_state.lock().ok().and_then(|state| {
        state
            .terminated_reason
            .as_ref()
            .map(|reason| format!("terminal session terminated: {reason}"))
    })
}

async fn run_remote_terminal_session(
    mut client: StreamingBmuxClient,
    session_id: Uuid,
    initial_snapshot: Option<bmux_client::AttachSnapshotState>,
    viewport: AttachViewport,
    mut command_rx: mpsc::Receiver<SessionCommand>,
    actor_state: Arc<Mutex<SessionActorState>>,
) {
    let mut state = StreamOutputState::new(viewport);
    if let Some(snapshot) = initial_snapshot {
        state.hydrate_snapshot(snapshot);
    }

    let mut event_stream_open = true;
    let mut keepalive = tokio::time::interval(Duration::from_millis(SESSION_KEEPALIVE_INTERVAL_MS));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = keepalive.tick().await;

    loop {
        tokio::select! {
            maybe_command = command_rx.recv() => {
                let Some(command) = maybe_command else {
                    break;
                };
                if !handle_session_command(
                    &mut client,
                    &mut state,
                    session_id,
                    command,
                    &actor_state,
                ).await {
                    return;
                }
            }
            maybe_event = client.event_receiver().recv(), if event_stream_open => {
                let Some(event) = maybe_event else {
                    event_stream_open = false;
                    let reason = client.disconnect_reason().unwrap_or_else(|| {
                        "terminal session stream closed by remote peer".to_string()
                    });
                    set_session_terminated_reason(
                        &actor_state,
                        reason,
                    );
                    continue;
                };
                handle_session_event(&mut client, &mut state, session_id, event).await;
            }
            _ = keepalive.tick(), if event_stream_open => {
                if let Err(error) = client.ping().await {
                    event_stream_open = false;
                    set_session_terminated_reason(
                        &actor_state,
                        format!("keepalive ping failed: {error}"),
                    );
                }
            }
        }
    }

    let _ = client.detach().await;
}

async fn handle_session_command(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
    command: SessionCommand,
    actor_state: &Arc<Mutex<SessionActorState>>,
) -> bool {
    if let Some(reason) = session_terminated_reason(actor_state) {
        match command {
            SessionCommand::PollOutput { response, .. } => {
                let _ = response.send(Err(MobileCoreError::TerminalBackendFailure(reason)));
                return true;
            }
            SessionCommand::WriteInput { response, .. } => {
                let _ = response.send(Err(MobileCoreError::TerminalBackendFailure(format!(
                    "{reason}; input rejected"
                ))));
                return true;
            }
            SessionCommand::MouseEvent { response, .. } => {
                let _ = response.send(Err(MobileCoreError::TerminalBackendFailure(format!(
                    "{reason}; mouse event rejected"
                ))));
                return true;
            }
            SessionCommand::Resize { response, .. } => {
                let _ = response.send(Err(MobileCoreError::TerminalBackendFailure(format!(
                    "{reason}; resize rejected"
                ))));
                return true;
            }
            SessionCommand::Close { response } => {
                let _ = response.send(Ok(()));
                return false;
            }
        }
    }

    match command {
        SessionCommand::PollOutput {
            max_bytes,
            response,
        } => {
            let render_result = state.render_if_dirty();
            let _ = match render_result {
                Ok(()) => response.send(Ok(state.drain_output(max_bytes))),
                Err(error) => response.send(Err(error)),
            };
            true
        }
        SessionCommand::WriteInput { bytes, response } => {
            let result = client
                .attach_input(session_id, bytes)
                .await
                .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
            if let Err(error) = &result {
                set_session_terminated_reason(actor_state, error.to_string());
            }
            let _ = response.send(result);
            true
        }
        SessionCommand::MouseEvent { event, response } => {
            let result = handle_session_mouse_event(client, state, session_id, event).await;
            if let Err(error) = &result {
                set_session_terminated_reason(actor_state, error.to_string());
            }
            let _ = response.send(result);
            true
        }
        SessionCommand::Resize {
            rows,
            cols,
            response,
        } => {
            let result = client
                .attach_set_viewport_with_insets(session_id, cols, rows, 0, 0)
                .await
                .map(|(_cols, _rows)| {
                    state.set_viewport(AttachViewport {
                        cols,
                        rows,
                        status_top_inset: 0,
                        status_bottom_inset: 0,
                    });
                })
                .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
            if let Err(error) = &result {
                set_session_terminated_reason(actor_state, error.to_string());
            }
            let _ = response.send(result);
            true
        }
        SessionCommand::Close { response } => {
            let result = client
                .detach()
                .await
                .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
            let _ = response.send(result);
            false
        }
    }
}

async fn handle_session_mouse_event(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
    event: TerminalMouseEvent,
) -> Result<()> {
    let Some((target_pane, pane_content_rect)) =
        state.target_pane_and_rect_at(event.col, event.row)
    else {
        return Ok(());
    };

    let in_focused_pane = state
        .focused_pane_id()
        .is_some_and(|focused_pane| focused_pane == target_pane);
    let focus_before_forward = matches!(
        event.kind,
        TerminalMouseEventKind::Down | TerminalMouseEventKind::Up | TerminalMouseEventKind::Drag
    ) && matches!(event.button, Some(TerminalMouseButton::Left));

    if focus_before_forward && !in_focused_pane {
        #[derive(serde::Serialize)]
        struct FocusPaneBySelectorArgs {
            session: bmux_windows_plugin_api::windows_commands::Selector,
            target: bmux_windows_plugin_api::windows_commands::Selector,
        }
        let args = FocusPaneBySelectorArgs {
            session: bmux_windows_plugin_api::windows_commands::Selector {
                id: Some(session_id),
                name: None,
                index: None,
            },
            target: bmux_windows_plugin_api::windows_commands::Selector {
                id: Some(target_pane),
                name: None,
                index: None,
            },
        };
        let encoded = bmux_codec::to_vec(&args).map_err(|error| {
            MobileCoreError::TerminalBackendFailure(format!(
                "encoding focus-pane-by-selector args: {error}"
            ))
        })?;
        let _response_bytes = client
            .invoke_service_raw(
                "bmux.windows.write",
                bmux_ipc::InvokeServiceKind::Command,
                "windows-commands",
                "focus-pane-by-selector",
                encoded,
            )
            .await
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
        if let Ok(layout) = client.attach_layout(session_id).await {
            let _ = state.apply_layout_state(layout);
        }
    } else if !in_focused_pane {
        return Ok(());
    }

    let Some(protocol) = state.pane_protocol(target_pane) else {
        return Ok(());
    };
    let Some(mouse_event) = terminal_mouse_event_to_attach_mouse_event(event) else {
        return Ok(());
    };
    // Programs inside the pane expect pane-local coordinates. See the
    // matching translation in `attach_mouse_forward_bytes_for_target` for
    // the attach CLI path.
    let Some(local_event) =
        attach_mouse::translate_event_to_pane_local(mouse_event, pane_content_rect)
    else {
        return Ok(());
    };
    let Some(bytes) = attach_mouse::encode_for_protocol(local_event, protocol) else {
        return Ok(());
    };

    client
        .attach_input(session_id, bytes)
        .await
        .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))
}

const fn terminal_mouse_button_to_attach(button: TerminalMouseButton) -> attach_mouse::Button {
    match button {
        TerminalMouseButton::Left => attach_mouse::Button::Left,
        TerminalMouseButton::Middle => attach_mouse::Button::Middle,
        TerminalMouseButton::Right => attach_mouse::Button::Right,
    }
}

fn terminal_mouse_event_to_attach_mouse_event(
    event: TerminalMouseEvent,
) -> Option<attach_mouse::Event> {
    let kind = match event.kind {
        TerminalMouseEventKind::Down => {
            attach_mouse::EventKind::Down(terminal_mouse_button_to_attach(event.button?))
        }
        TerminalMouseEventKind::Up => {
            attach_mouse::EventKind::Up(terminal_mouse_button_to_attach(event.button?))
        }
        TerminalMouseEventKind::Drag => {
            attach_mouse::EventKind::Drag(terminal_mouse_button_to_attach(event.button?))
        }
        TerminalMouseEventKind::Move => attach_mouse::EventKind::Moved,
        TerminalMouseEventKind::ScrollUp => attach_mouse::EventKind::ScrollUp,
        TerminalMouseEventKind::ScrollDown => attach_mouse::EventKind::ScrollDown,
        TerminalMouseEventKind::ScrollLeft => attach_mouse::EventKind::ScrollLeft,
        TerminalMouseEventKind::ScrollRight => attach_mouse::EventKind::ScrollRight,
    };

    Some(attach_mouse::Event {
        kind,
        column: event.col,
        row: event.row,
        modifiers: attach_mouse::Modifiers {
            shift: event.shift,
            alt: event.alt,
            control: event.control,
        },
    })
}

async fn handle_session_event(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
    event: ServerEvent,
) {
    match event {
        ServerEvent::PaneOutput {
            session_id: event_session_id,
            pane_id,
            data,
            stream_start,
            stream_end,
            stream_gap,
            sync_update_active,
        } if event_session_id == session_id => {
            let outcome = state.apply_chunk(&AttachPaneChunk {
                pane_id,
                data,
                stream_start,
                stream_end,
                stream_gap,
                sync_update_active,
            });
            if matches!(outcome, AttachChunkApplyOutcome::Desync) {
                let _ = recover_desynced_pane(client, state, session_id, pane_id).await;
            }
        }
        ServerEvent::PaneOutputAvailable {
            session_id: event_session_id,
            ..
        } if event_session_id == session_id => {
            let pane_ids = state.visible_pane_ids();
            if !pane_ids.is_empty()
                && let Ok(batch) = client
                    .attach_pane_output_batch(session_id, pane_ids, EVENT_TRIGGER_FETCH_MAX_BYTES)
                    .await
            {
                for chunk in batch.chunks {
                    let pane_id = chunk.pane_id;
                    let outcome = state.apply_chunk(&chunk);
                    if matches!(outcome, AttachChunkApplyOutcome::Desync) {
                        let _ = recover_desynced_pane(client, state, session_id, pane_id).await;
                    }
                }
            }
        }
        ServerEvent::AttachViewChanged {
            session_id: event_session_id,
            components,
            ..
        } if event_session_id == session_id => {
            let component_hydration_requested = state.apply_view_change_components(&components);
            if let Ok(layout) = client.attach_layout(session_id).await {
                let layout_hydration_requested = state.apply_layout_state(layout);
                if component_hydration_requested || layout_hydration_requested {
                    let _ = hydrate_full_scene_snapshot(client, state, session_id).await;
                }
            } else if component_hydration_requested {
                let _ = hydrate_full_scene_snapshot(client, state, session_id).await;
            }
        }
        _ => {}
    }
}

async fn hydrate_full_scene_snapshot(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
) -> Result<()> {
    let snapshot = client
        .attach_snapshot(session_id, EVENT_TRIGGER_FETCH_MAX_BYTES)
        .await
        .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
    state.hydrate_snapshot(snapshot);
    Ok(())
}

async fn recover_desynced_pane(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
    pane_id: Uuid,
) -> Result<()> {
    if client.supports_capability(CAPABILITY_ATTACH_PANE_SNAPSHOT)
        && let Ok(snapshot) = client
            .attach_pane_snapshot(session_id, vec![pane_id], EVENT_TRIGGER_FETCH_MAX_BYTES)
            .await
    {
        state.hydrate_pane_snapshot(&[pane_id], snapshot);
        return Ok(());
    }

    hydrate_full_scene_snapshot(client, state, session_id).await
}

struct StreamOutputState {
    output_queue: OutputQueue,
    pipeline: AttachScenePipeline,
}

impl StreamOutputState {
    fn new(viewport: AttachViewport) -> Self {
        Self {
            output_queue: OutputQueue::new(OUTPUT_QUEUE_MAX_BYTES),
            pipeline: AttachScenePipeline::new(viewport),
        }
    }

    fn set_viewport(&mut self, viewport: AttachViewport) {
        self.pipeline.set_viewport(viewport);
    }

    fn hydrate_snapshot(&mut self, snapshot: bmux_client::AttachSnapshotState) {
        self.pipeline.hydrate_snapshot(snapshot);
    }

    fn hydrate_pane_snapshot(
        &mut self,
        pane_ids: &[Uuid],
        snapshot: bmux_client::AttachPaneSnapshotState,
    ) {
        self.pipeline.hydrate_pane_snapshot(pane_ids, snapshot);
    }

    fn apply_layout_state(&mut self, layout_state: bmux_client::AttachLayoutState) -> bool {
        self.pipeline.apply_layout_state(layout_state)
    }

    fn apply_view_change_components(&mut self, components: &[AttachViewComponent]) -> bool {
        self.pipeline.apply_view_change_components(components)
    }

    fn apply_chunk(&mut self, chunk: &AttachPaneChunk) -> AttachChunkApplyOutcome {
        self.pipeline.apply_chunk(chunk)
    }

    fn visible_pane_ids(&self) -> Vec<Uuid> {
        self.pipeline
            .layout_state
            .as_ref()
            .map_or_else(Vec::new, |layout_state| {
                visible_scene_pane_ids(&layout_state.scene)
            })
    }

    fn focused_pane_id(&self) -> Option<Uuid> {
        self.pipeline
            .layout_state
            .as_ref()
            .map(|layout_state| layout_state.focused_pane_id)
    }

    fn target_pane_and_rect_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<(Uuid, bmux_ipc::AttachRect)> {
        let layout_state = self.pipeline.layout_state.as_ref()?;
        attach_mouse::pane_and_rect_at(&layout_state.scene, column, row)
    }

    fn pane_protocol(&self, pane_id: Uuid) -> Option<attach_mouse::PaneProtocol> {
        attach_mouse::pane_protocol(
            &self.pipeline.pane_buffers,
            self.pipeline.pane_mouse_protocol_hints(),
            pane_id,
        )
    }

    fn render_if_dirty(&mut self) -> Result<()> {
        if let Some(frame) = self
            .pipeline
            .render_frame()
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?
        {
            self.output_queue.push_bytes(frame);
        }
        Ok(())
    }

    fn drain_output(&mut self, max_bytes: usize) -> Vec<u8> {
        self.output_queue.drain(max_bytes)
    }
}

#[derive(Debug)]
struct OutputQueue {
    bytes: VecDeque<u8>,
    max_bytes: usize,
}

impl OutputQueue {
    const fn new(max_bytes: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            max_bytes,
        }
    }

    fn push_bytes(&mut self, data: Vec<u8>) {
        self.bytes.extend(data);
        while self.bytes.len() > self.max_bytes {
            let _ = self.bytes.pop_front();
        }
    }

    fn drain(&mut self, max_bytes: usize) -> Vec<u8> {
        let to_read = self.bytes.len().min(max_bytes.max(1));
        self.bytes.drain(..to_read).collect()
    }
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

    #[test]
    fn retry_with_extended_hello_timeout_for_hello_probe_timeout() {
        let error = IrohConnectAttemptError::timeout(
            IrohConnectStage::HelloV2,
            2_500,
            "request timed out after 2.5s".to_string(),
        );

        assert!(
            RemoteTerminalBackend::should_retry_with_extended_hello_timeout(
                &error,
                std::time::Duration::from_millis(2_500),
                std::time::Duration::from_millis(7_500),
            )
        );
    }

    #[test]
    fn no_retry_with_extended_hello_timeout_for_connect_stage_timeout() {
        let error = IrohConnectAttemptError::timeout(
            IrohConnectStage::Connect,
            2_500,
            "timed out connecting iroh target".to_string(),
        );

        assert!(
            !RemoteTerminalBackend::should_retry_with_extended_hello_timeout(
                &error,
                std::time::Duration::from_millis(2_500),
                std::time::Duration::from_millis(7_500),
            )
        );
    }

    #[test]
    fn no_retry_with_extended_hello_timeout_when_retry_not_longer_than_probe() {
        let error = IrohConnectAttemptError::timeout(
            IrohConnectStage::OpenBi,
            2_500,
            "timed out opening iroh stream".to_string(),
        );

        assert!(
            !RemoteTerminalBackend::should_retry_with_extended_hello_timeout(
                &error,
                std::time::Duration::from_millis(2_500),
                std::time::Duration::from_millis(2_500),
            )
        );
    }
}
