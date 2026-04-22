#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Client component for bmux terminal multiplexer.

use bmux_config::{BmuxConfig, ConfigPaths};
pub use bmux_ipc::Event as ServerEvent;
use bmux_ipc::transport::{
    ErasedIpcStream, ErasedIpcStreamReader, ErasedIpcStreamWriter, IpcStreamReader,
    IpcStreamWriter, IpcTransportError, LocalIpcStream,
};
use bmux_ipc::{
    AttachGrant, AttachPaneChunk, AttachPaneImageDelta, AttachPaneInputMode,
    AttachPaneMouseProtocol, AttachScene, CORE_PROTOCOL_CAPABILITIES, ContextSelector, Envelope,
    EnvelopeKind, ErrorCode, IncompatibilityReason, InvokeServiceKind, IpcEndpoint,
    NegotiatedProtocol, PaneLayoutNode, PaneSummary, ProtocolContract, Request, Response,
    ResponsePayload, ServerSnapshotStatus, SessionSelector, decode, default_supported_capabilities,
    encode,
};
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError, TypedDispatchClientResult};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
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
    pub zoomed: bool,
}

/// Result of a pane output batch fetch, including whether the server's PTY
/// reader has flagged additional output that was not included in this batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneOutputBatchResult {
    pub chunks: Vec<AttachPaneChunk>,
    /// True when the server indicates at least one requested pane's PTY
    /// reader has pushed new output since the batch was read.  The client
    /// should continue draining.
    pub output_still_pending: bool,
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
    pub pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pub pane_input_modes: Vec<AttachPaneInputMode>,
    pub zoomed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachPaneSnapshotState {
    pub chunks: Vec<AttachPaneChunk>,
    pub pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pub pane_input_modes: Vec<AttachPaneInputMode>,
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
    Serialization(#[from] bmux_codec::Error),
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
    #[error("protocol negotiation failed: {reason:?}")]
    ProtocolIncompatible { reason: IncompatibilityReason },
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
    stream: ClientStream,
    timeout: Duration,
    next_request_id: u64,
    principal_id: Uuid,
    negotiated_protocol: Option<NegotiatedProtocol>,
}

#[derive(Debug)]
enum ClientStream {
    Local(LocalIpcStream),
    Bridge(ErasedIpcStream),
}

impl ClientStream {
    async fn send_envelope(
        &mut self,
        envelope: &Envelope,
    ) -> std::result::Result<(), IpcTransportError> {
        match self {
            Self::Local(stream) => stream.send_envelope(envelope).await,
            Self::Bridge(stream) => stream.send_envelope(envelope).await,
        }
    }

    async fn recv_envelope(&mut self) -> std::result::Result<Envelope, IpcTransportError> {
        match self {
            Self::Local(stream) => stream.recv_envelope().await,
            Self::Bridge(stream) => stream.recv_envelope().await,
        }
    }
}

impl BmuxClient {
    #[must_use]
    pub const fn negotiated_protocol(&self) -> Option<&NegotiatedProtocol> {
        self.negotiated_protocol.as_ref()
    }

    #[must_use]
    pub fn supports_capability(&self, capability: &str) -> bool {
        self.negotiated_protocol.as_ref().is_some_and(|negotiated| {
            negotiated
                .capabilities
                .iter()
                .any(|supported| supported == capability)
        }) || CORE_PROTOCOL_CAPABILITIES.contains(&capability)
    }

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
        Self::connect_with_stream(
            ClientStream::Local(stream),
            timeout,
            client_name,
            principal_id,
        )
        .await
    }

    /// Connect over an already-established framed duplex stream.
    ///
    /// # Errors
    ///
    /// Returns an error if handshake fails.
    pub async fn connect_with_bridge_stream(
        stream: ErasedIpcStream,
        timeout: Duration,
        client_name: impl Into<String>,
        principal_id: Uuid,
    ) -> Result<Self> {
        Self::connect_with_stream(
            ClientStream::Bridge(stream),
            timeout,
            client_name,
            principal_id,
        )
        .await
    }

    async fn connect_with_stream(
        stream: ClientStream,
        timeout: Duration,
        client_name: impl Into<String>,
        principal_id: Uuid,
    ) -> Result<Self> {
        let client_name = client_name.into();
        let mut client = Self {
            stream,
            timeout,
            next_request_id: 1,
            principal_id,
            negotiated_protocol: None,
        };

        let v2_attempt = client
            .request(Request::HelloV2 {
                contract: ProtocolContract::current(default_supported_capabilities()),
                client_name: client_name.clone(),
                principal_id,
            })
            .await;

        match v2_attempt {
            Ok(ResponsePayload::HelloNegotiated { negotiated }) => {
                client.negotiated_protocol = Some(negotiated);
                Ok(client)
            }
            Ok(ResponsePayload::HelloIncompatible { reason }) => {
                Err(ClientError::ProtocolIncompatible { reason })
            }
            Ok(_) => Err(ClientError::UnexpectedResponse(
                "handshake expected hello negotiation response",
            )),
            Err(error) => Err(error),
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

    /// Configure attach policy for this connection.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn set_attach_policy(&mut self, allow_detach: bool) -> Result<()> {
        match self
            .request(Request::SetClientAttachPolicy { allow_detach })
            .await?
        {
            ResponsePayload::ClientAttachPolicySet {
                allow_detach: applied,
            } if applied == allow_detach => Ok(()),
            _ => Err(ClientError::UnexpectedResponse(
                "expected client attach policy set response",
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

    /// Send bytes directly to a specific pane by ID, bypassing focus routing.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> Result<usize> {
        match self
            .request(Request::PaneDirectInput {
                session_id,
                pane_id,
                data,
            })
            .await?
        {
            ResponsePayload::PaneDirectInputAccepted { bytes, .. } => Ok(bytes),
            _ => Err(ClientError::UnexpectedResponse(
                "expected pane direct input accepted response",
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
        self.attach_set_viewport_with_insets(session_id, cols, rows, 0, 0)
            .await
    }

    /// Update attached viewport dimensions and status insets for pane PTY sizing.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> Result<(u16, u16)> {
        match self
            .request(Request::AttachSetViewport {
                session_id,
                cols,
                rows,
                status_top_inset,
                status_bottom_inset,
                cell_pixel_width: cell_pixel_width(),
                cell_pixel_height: cell_pixel_height(),
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

    /// Fetch current layout state for an attached session.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_layout(&mut self, session_id: Uuid) -> Result<AttachLayoutState> {
        match self.request(Request::AttachLayout { session_id }).await? {
            ResponsePayload::AttachLayout {
                context_id,
                session_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
                zoomed,
            } => Ok(AttachLayoutState {
                context_id,
                session_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
                zoomed,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach layout response",
            )),
        }
    }

    /// Fetch output from multiple panes in a single batch.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> Result<PaneOutputBatchResult> {
        match self
            .request(Request::AttachPaneOutputBatch {
                session_id,
                pane_ids,
                max_bytes,
            })
            .await?
        {
            ResponsePayload::AttachPaneOutputBatch {
                chunks,
                output_still_pending,
            } => Ok(PaneOutputBatchResult {
                chunks,
                output_still_pending,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach pane output batch response",
            )),
        }
    }

    /// Fetch image deltas for multiple panes since given sequence numbers.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_pane_images(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        since_sequences: Vec<u64>,
    ) -> Result<Vec<AttachPaneImageDelta>> {
        match self
            .request(Request::AttachPaneImages {
                session_id,
                pane_ids,
                since_sequences,
            })
            .await?
        {
            ResponsePayload::AttachPaneImages { deltas } => Ok(deltas),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach pane images response",
            )),
        }
    }

    /// Fetch a full session snapshot including layout, output, and mouse state.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
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
                pane_mouse_protocols,
                pane_input_modes,
                zoomed,
            } => Ok(AttachSnapshotState {
                context_id,
                session_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
                chunks,
                pane_mouse_protocols,
                pane_input_modes,
                zoomed,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach snapshot response",
            )),
        }
    }

    /// Fetch recent output snapshots for specific panes.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> Result<AttachPaneSnapshotState> {
        match self
            .request(Request::AttachPaneSnapshot {
                session_id,
                pane_ids,
                max_bytes_per_pane,
            })
            .await?
        {
            ResponsePayload::AttachPaneSnapshot {
                chunks,
                pane_mouse_protocols,
                pane_input_modes,
            } => Ok(AttachPaneSnapshotState {
                chunks,
                pane_mouse_protocols,
                pane_input_modes,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach pane snapshot response",
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

fn map_client_error(
    interface: &str,
    operation: &str,
    err: ClientError,
) -> TypedDispatchClientError {
    match err {
        ClientError::ServerError { code, message } => {
            TypedDispatchClientError::server(interface, operation, format!("{code:?}: {message}"))
        }
        ClientError::UnexpectedResponse(details) => {
            TypedDispatchClientError::unexpected_response(interface, operation, details)
        }
        other => TypedDispatchClientError::transport(interface, operation, other.to_string()),
    }
}

impl TypedDispatchClient for BmuxClient {
    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl std::future::Future<Output = TypedDispatchClientResult<Vec<u8>>> + Send {
        let interface_owned = interface_id.to_string();
        let op_owned = operation.to_string();
        let cap_owned = capability.to_string();
        async move {
            let iface_for_err = interface_owned.clone();
            let op_for_err = op_owned.clone();
            match self
                .request(Request::InvokeService {
                    capability: cap_owned,
                    kind,
                    interface_id: interface_owned,
                    operation: op_owned,
                    payload,
                })
                .await
                .map_err(|err| map_client_error(&iface_for_err, &op_for_err, err))?
            {
                ResponsePayload::ServiceInvoked { payload } => Ok(payload),
                _ => Err(TypedDispatchClientError::unexpected_response(
                    iface_for_err,
                    op_for_err,
                    "expected service invoked",
                )),
            }
        }
    }
}

// ── Streaming client with server-push event support ──────────────────────────

/// Thread-safe map of in-flight request IDs to their response channels.
type PendingMap =
    Arc<tokio::sync::Mutex<BTreeMap<u64, tokio::sync::oneshot::Sender<Result<Response>>>>>;

fn store_stream_disconnect_reason(reason_slot: &Arc<StdMutex<Option<String>>>, reason: String) {
    if let Ok(mut slot) = reason_slot.lock()
        && slot.is_none()
    {
        *slot = Some(reason);
    }
}

fn format_stream_disconnect_reason(error: &IpcTransportError) -> String {
    match error {
        IpcTransportError::Io(io_error) if io_error.kind() == std::io::ErrorKind::UnexpectedEof => {
            format!("stream closed with unexpected EOF: {io_error}")
        }
        IpcTransportError::Io(io_error)
            if io_error.kind() == std::io::ErrorKind::ConnectionReset =>
        {
            format!("stream connection reset by peer: {io_error}")
        }
        IpcTransportError::Io(io_error) => format!("stream I/O failure: {io_error}"),
        IpcTransportError::FrameDecode(decode_error) => {
            format!("stream frame decode failure: {decode_error}")
        }
        IpcTransportError::FrameEncode(encode_error) => {
            format!("stream frame encode failure: {encode_error}")
        }
        IpcTransportError::UnsupportedEndpoint => "stream failed: unsupported endpoint".to_string(),
    }
}

/// Event-driven client that receives server-pushed events without polling.
///
/// After the initial handshake (performed as a regular [`BmuxClient`]), the
/// underlying socket is split into read/write halves. A background reader task
/// demuxes incoming frames: `Response` envelopes are routed by `request_id`,
/// `Event` envelopes are pushed to a channel consumed via [`event_receiver`].
///
/// Call [`enable_event_push`] after construction to enable server-side push
/// delivery.
#[derive(Debug)]
pub struct StreamingBmuxClient {
    writer: StreamingClientWriter,
    timeout: Duration,
    next_request_id: u64,
    principal_id: Uuid,
    negotiated_protocol: Option<NegotiatedProtocol>,
    pending: PendingMap,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    disconnect_reason: Arc<StdMutex<Option<String>>>,
    _reader_task: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
enum StreamingClientWriter {
    Local(IpcStreamWriter),
    Bridge(ErasedIpcStreamWriter),
}

impl StreamingClientWriter {
    async fn send_envelope(
        &mut self,
        envelope: &Envelope,
    ) -> std::result::Result<(), IpcTransportError> {
        match self {
            Self::Local(writer) => writer.send_envelope(envelope).await,
            Self::Bridge(writer) => writer.send_envelope(envelope).await,
        }
    }
}

#[derive(Debug)]
enum StreamingClientReader {
    Local(IpcStreamReader),
    Bridge(ErasedIpcStreamReader),
}

impl StreamingClientReader {
    async fn recv_envelope(&mut self) -> std::result::Result<Envelope, IpcTransportError> {
        match self {
            Self::Local(reader) => reader.recv_envelope().await,
            Self::Bridge(reader) => reader.recv_envelope().await,
        }
    }

    const fn enable_frame_compression(&mut self) {
        match self {
            Self::Local(reader) => reader.enable_frame_compression(),
            Self::Bridge(reader) => reader.enable_frame_compression(),
        }
    }
}

impl StreamingBmuxClient {
    /// Upgrade an existing [`BmuxClient`] (already handshaken) into a streaming
    /// client. The `BmuxClient` is consumed; its socket is split and a reader
    /// task is spawned on the current tokio runtime.
    ///
    /// Supports both local IPC sockets and bridge streams.
    ///
    /// # Errors
    ///
    /// Returns an error if request/response frame processing cannot be
    /// initialized for the provided client stream.
    pub fn from_client(client: BmuxClient) -> Result<Self> {
        let BmuxClient {
            stream,
            timeout,
            next_request_id,
            principal_id,
            negotiated_protocol,
        } = client;

        let (mut reader, mut writer) = match stream {
            ClientStream::Local(local_stream) => {
                let (reader, writer) = local_stream.into_split();
                (
                    StreamingClientReader::Local(reader),
                    StreamingClientWriter::Local(writer),
                )
            }
            ClientStream::Bridge(bridge_stream) => {
                let (reader, writer) = bridge_stream.into_split();
                (
                    StreamingClientReader::Bridge(reader),
                    StreamingClientWriter::Bridge(writer),
                )
            }
        };

        // Enable frame compression if negotiated.
        if let Some(ref negotiated) = negotiated_protocol
            && let Some(codec) = resolve_frame_codec_from_capabilities(&negotiated.capabilities)
        {
            match &mut writer {
                StreamingClientWriter::Local(writer) => {
                    writer.enable_frame_compression(codec.clone());
                }
                StreamingClientWriter::Bridge(writer) => {
                    writer.enable_frame_compression(codec.clone());
                }
            }
            reader.enable_frame_compression();
        }

        let pending: PendingMap = Arc::new(tokio::sync::Mutex::new(BTreeMap::new()));
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let disconnect_reason = Arc::new(StdMutex::new(None));

        let reader_pending = Arc::clone(&pending);
        let reader_disconnect_reason = Arc::clone(&disconnect_reason);
        let reader_task = tokio::spawn(async move {
            Self::reader_loop(reader, reader_pending, event_tx, reader_disconnect_reason).await;
        });

        Ok(Self {
            writer,
            timeout,
            next_request_id,
            principal_id,
            negotiated_protocol,
            pending,
            event_rx,
            disconnect_reason,
            _reader_task: reader_task,
        })
    }

    #[must_use]
    pub const fn negotiated_protocol(&self) -> Option<&NegotiatedProtocol> {
        self.negotiated_protocol.as_ref()
    }

    #[must_use]
    pub fn supports_capability(&self, capability: &str) -> bool {
        self.negotiated_protocol.as_ref().is_some_and(|negotiated| {
            negotiated
                .capabilities
                .iter()
                .any(|supported| supported == capability)
        }) || CORE_PROTOCOL_CAPABILITIES.contains(&capability)
    }

    /// Background reader loop that demuxes incoming envelopes.
    async fn reader_loop(
        mut reader: StreamingClientReader,
        pending: PendingMap,
        event_tx: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
        disconnect_reason: Arc<StdMutex<Option<String>>>,
    ) {
        loop {
            let envelope = match reader.recv_envelope().await {
                Ok(envelope) => envelope,
                Err(error) => {
                    let reason = format_stream_disconnect_reason(&error);
                    store_stream_disconnect_reason(&disconnect_reason, reason.clone());
                    // Connection closed or error — wake all pending requests.
                    let pending_requests = std::mem::take(&mut *pending.lock().await);
                    for (_, tx) in pending_requests {
                        let io_error_kind = match &error {
                            IpcTransportError::Io(io_error) => io_error.kind(),
                            _ => std::io::ErrorKind::BrokenPipe,
                        };
                        let io_error = std::io::Error::new(io_error_kind, reason.clone());
                        let _ =
                            tx.send(Err(ClientError::Transport(IpcTransportError::Io(io_error))));
                    }
                    return;
                }
            };

            match envelope.kind {
                EnvelopeKind::Response => {
                    let mut map = pending.lock().await;
                    if let Some(tx) = map.remove(&envelope.request_id) {
                        match decode::<Response>(&envelope.payload) {
                            Ok(response) => {
                                let _ = tx.send(Ok(response));
                            }
                            Err(e) => {
                                let _ = tx.send(Err(ClientError::Serialization(e)));
                            }
                        }
                    } else {
                        // Expected when the caller used send_one_way() — the
                        // server still sends a response but we have no pending
                        // entry.  Log at trace to avoid noise.
                        trace!(
                            request_id = envelope.request_id,
                            "streaming client received response for unknown request id"
                        );
                    }
                }
                EnvelopeKind::Event => match decode::<ServerEvent>(&envelope.payload) {
                    Ok(event) => {
                        let _ = event_tx.send(event);
                    }
                    Err(e) => {
                        warn!("streaming client failed to decode event: {e:#}");
                    }
                },
                EnvelopeKind::Request => {
                    warn!("streaming client received unexpected request envelope");
                }
            }
        }
    }

    /// Borrow the event receiver for use in `tokio::select!`.
    pub const fn event_receiver(
        &mut self,
    ) -> &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent> {
        &mut self.event_rx
    }

    #[must_use]
    pub fn disconnect_reason(&self) -> Option<String> {
        self.disconnect_reason
            .lock()
            .ok()
            .and_then(|reason| reason.clone())
    }

    /// Return this connection's principal identity.
    #[must_use]
    pub const fn principal_id(&self) -> Uuid {
        self.principal_id
    }

    /// Execute a request and return the full response.
    ///
    /// # Errors
    ///
    /// Returns an error if transport, serialization, or timeout occurs.
    pub async fn request_raw(&mut self, request: Request) -> Result<Response> {
        let request_id = self.take_request_id();
        let request_kind = request_kind_name(&request);
        let started_at = std::time::Instant::now();
        debug!(
            request_id,
            request = request_kind,
            "streaming_ipc.request.start"
        );

        let payload = encode(&request)?;
        let envelope = Envelope::new(request_id, EnvelopeKind::Request, payload);

        // Register pending response before sending to avoid races.
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(request_id, tx);
        }

        if let Err(e) = tokio::time::timeout(self.timeout, self.writer.send_envelope(&envelope))
            .await
            .map_err(|_| ClientError::Timeout(self.timeout))?
        {
            self.pending.lock().await.remove(&request_id);
            return Err(ClientError::Transport(e));
        }

        let response = tokio::time::timeout(self.timeout, rx)
            .await
            .map_err(|_| ClientError::Timeout(self.timeout))?
            .map_err(|_| {
                ClientError::Transport(IpcTransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "reader task dropped before response",
                )))
            })??;

        debug!(
            request_id,
            request = request_kind,
            response = response_kind_name(&response),
            duration_ms = started_at.elapsed().as_millis(),
            "streaming_ipc.request.done"
        );
        Ok(response)
    }

    async fn request(&mut self, request: Request) -> Result<ResponsePayload> {
        let response = self.request_raw(request).await?;
        match response {
            Response::Ok(payload) => Ok(payload),
            Response::Err(error) => Err(ClientError::ServerError {
                code: error.code,
                message: error.message,
            }),
        }
    }

    fn take_request_id(&mut self) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        request_id
    }

    /// Send a request without waiting for a response.
    ///
    /// The server may still send a response, but the client will silently
    /// discard it.  Use this for latency-sensitive operations where the
    /// response carries no essential information (e.g. `AttachInput` where
    /// the normal response is just `AttachInputAccepted`).
    ///
    /// # Errors
    ///
    /// Returns an error if the frame cannot be written to the transport.
    pub async fn send_one_way(&mut self, request: Request) -> Result<()> {
        let request_id = self.take_request_id();
        let request_kind = request_kind_name(&request);
        trace!(
            request_id,
            request = request_kind,
            "streaming_ipc.one_way.send"
        );
        let payload = encode(&request)?;
        let envelope = Envelope::new(request_id, EnvelopeKind::Request, payload);
        // Deliberately do NOT register in self.pending — the response (if
        // any) will be silently dropped by the reader task.
        self.writer
            .send_envelope(&envelope)
            .await
            .map_err(ClientError::Transport)
    }

    // ── Event push control ───────────────────────────────────────────────

    /// Enable server-push event delivery on this connection.
    ///
    /// After this call, the server will push `Event` frames asynchronously.
    /// Events are received via [`event_receiver`].
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn enable_event_push(&mut self) -> Result<()> {
        match self.request(Request::EnableEventPush).await? {
            ResponsePayload::EventPushEnabled => Ok(()),
            _ => Err(ClientError::UnexpectedResponse(
                "expected event push enabled",
            )),
        }
    }

    // ── Delegated request methods ────────────────────────────────────────

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

    /// Subscribe this client to server lifecycle events.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn subscribe_events(&mut self) -> Result<()> {
        match self.request(Request::SubscribeEvents).await? {
            ResponsePayload::EventsSubscribed => Ok(()),
            _ => Err(ClientError::UnexpectedResponse(
                "expected events subscribed",
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

    /// Fetch the server's control-plane catalog snapshot.
    ///
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

    /// Update attached viewport dimensions and status insets for pane PTY sizing.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        status_top_inset: u16,
        status_bottom_inset: u16,
    ) -> Result<(u16, u16)> {
        match self
            .request(Request::AttachSetViewport {
                session_id,
                cols,
                rows,
                status_top_inset,
                status_bottom_inset,
                cell_pixel_width: cell_pixel_width(),
                cell_pixel_height: cell_pixel_height(),
            })
            .await?
        {
            ResponsePayload::AttachViewportSet { cols, rows, .. } => Ok((cols, rows)),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach viewport set response",
            )),
        }
    }

    /// Send bytes to an attached session runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_input(&mut self, session_id: Uuid, data: Vec<u8>) -> Result<()> {
        match self
            .request(Request::AttachInput { session_id, data })
            .await?
        {
            ResponsePayload::AttachInputAccepted { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach input accepted response",
            )),
        }
    }

    /// Fetch current layout state for an attached session.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_layout(&mut self, session_id: Uuid) -> Result<AttachLayoutState> {
        match self.request(Request::AttachLayout { session_id }).await? {
            ResponsePayload::AttachLayout {
                context_id,
                session_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
                zoomed,
            } => Ok(AttachLayoutState {
                context_id,
                session_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
                zoomed,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach layout response",
            )),
        }
    }

    /// Fetch output from multiple panes in a single batch.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> Result<PaneOutputBatchResult> {
        match self
            .request(Request::AttachPaneOutputBatch {
                session_id,
                pane_ids,
                max_bytes,
            })
            .await?
        {
            ResponsePayload::AttachPaneOutputBatch {
                chunks,
                output_still_pending,
            } => Ok(PaneOutputBatchResult {
                chunks,
                output_still_pending,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach pane output batch response",
            )),
        }
    }

    /// Fetch image deltas for multiple panes since given sequence numbers.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_pane_images(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        since_sequences: Vec<u64>,
    ) -> Result<Vec<AttachPaneImageDelta>> {
        match self
            .request(Request::AttachPaneImages {
                session_id,
                pane_ids,
                since_sequences,
            })
            .await?
        {
            ResponsePayload::AttachPaneImages { deltas } => Ok(deltas),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach pane images response",
            )),
        }
    }

    /// Fetch a full session snapshot including layout, output, and mouse state.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
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
                pane_mouse_protocols,
                pane_input_modes,
                zoomed,
            } => Ok(AttachSnapshotState {
                context_id,
                session_id,
                focused_pane_id,
                panes,
                layout_root,
                scene,
                chunks,
                pane_mouse_protocols,
                pane_input_modes,
                zoomed,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach snapshot response",
            )),
        }
    }

    /// Fetch recent output snapshots for specific panes.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> Result<AttachPaneSnapshotState> {
        match self
            .request(Request::AttachPaneSnapshot {
                session_id,
                pane_ids,
                max_bytes_per_pane,
            })
            .await?
        {
            ResponsePayload::AttachPaneSnapshot {
                chunks,
                pane_mouse_protocols,
                pane_input_modes,
            } => Ok(AttachPaneSnapshotState {
                chunks,
                pane_mouse_protocols,
                pane_input_modes,
            }),
            _ => Err(ClientError::UnexpectedResponse(
                "expected attach pane snapshot response",
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
            _ => Err(ClientError::UnexpectedResponse("expected detached")),
        }
    }

    /// Configure attach policy for this connection.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn set_attach_policy(&mut self, allow_detach: bool) -> Result<()> {
        match self
            .request(Request::SetClientAttachPolicy { allow_detach })
            .await?
        {
            ResponsePayload::ClientAttachPolicySet {
                allow_detach: applied,
            } if applied == allow_detach => Ok(()),
            _ => Err(ClientError::UnexpectedResponse(
                "expected client attach policy set",
            )),
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

    /// Send bytes directly to a specific pane by ID, bypassing focus routing.
    ///
    /// # Errors
    ///
    /// Returns an error if request or response validation fails.
    pub async fn pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> Result<()> {
        match self
            .request(Request::PaneDirectInput {
                session_id,
                pane_id,
                data,
            })
            .await?
        {
            ResponsePayload::PaneDirectInputAccepted { .. } => Ok(()),
            _ => Err(ClientError::UnexpectedResponse(
                "expected pane direct input accepted",
            )),
        }
    }

    // Typed recording methods removed from StreamingBmuxClient; callers
    // migrate to `bmux_recording_plugin_api::typed_client::*` helpers.
}

impl TypedDispatchClient for StreamingBmuxClient {
    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl std::future::Future<Output = TypedDispatchClientResult<Vec<u8>>> + Send {
        let interface_owned = interface_id.to_string();
        let op_owned = operation.to_string();
        let cap_owned = capability.to_string();
        async move {
            let iface_for_err = interface_owned.clone();
            let op_for_err = op_owned.clone();
            match self
                .request_raw(Request::InvokeService {
                    capability: cap_owned,
                    kind,
                    interface_id: interface_owned,
                    operation: op_owned,
                    payload,
                })
                .await
                .map_err(|err| map_client_error(&iface_for_err, &op_for_err, err))?
            {
                Response::Ok(ResponsePayload::ServiceInvoked { payload }) => Ok(payload),
                _ => Err(TypedDispatchClientError::unexpected_response(
                    iface_for_err,
                    op_for_err,
                    "expected service invoked",
                )),
            }
        }
    }
}

const fn request_kind_name(request: &Request) -> &'static str {
    match request {
        Request::Hello { .. } => "hello",
        Request::HelloV2 { .. } => "hello_v2",
        Request::Ping => "ping",
        Request::WhoAmIPrincipal => "whoami_principal",
        Request::ServerStatus => "server_status",
        Request::ServerSave => "server_save",
        Request::ServerRestoreDryRun => "server_restore_dry_run",
        Request::ServerRestoreApply => "server_restore_apply",
        Request::ServerStop => "server_stop",
        Request::InvokeService { .. } => "invoke_service",
        Request::SplitPane { .. } => "split_pane",
        Request::LaunchPane { .. } => "launch_pane",
        Request::FocusPane { .. } => "focus_pane",
        Request::ResizePane { .. } => "resize_pane",
        Request::ClosePane { .. } => "close_pane",
        Request::RestartPane { .. } => "restart_pane",
        Request::ZoomPane { .. } => "zoom_pane",
        Request::Attach { .. } => "attach",
        Request::AttachContext { .. } => "attach_context",
        Request::AttachOpen { .. } => "attach_open",
        Request::AttachInput { .. } => "attach_input",
        Request::AttachSetViewport { .. } => "attach_set_viewport",
        Request::AttachOutput { .. } => "attach_output",
        Request::AttachLayout { .. } => "attach_layout",
        Request::AttachSnapshot { .. } => "attach_snapshot",
        Request::AttachPaneSnapshot { .. } => "attach_pane_snapshot",
        Request::AttachPaneOutputBatch { .. } => "attach_pane_output_batch",
        Request::AttachPaneImages { .. } => "attach_pane_images",
        Request::SetClientAttachPolicy { .. } => "set_client_attach_policy",
        Request::Detach => "detach",
        Request::SubscribeEvents => "subscribe_events",
        Request::PollEvents { .. } => "poll_events",
        Request::EnableEventPush => "enable_event_push",
        Request::PaneDirectInput { .. } => "pane_direct_input",
    }
}

const fn response_kind_name(response: &Response) -> &'static str {
    match response {
        Response::Ok(payload) => match payload {
            ResponsePayload::Pong => "pong",
            ResponsePayload::PrincipalIdentity { .. } => "principal_identity",
            ResponsePayload::HelloNegotiated { .. } => "hello_negotiated",
            ResponsePayload::HelloIncompatible { .. } => "hello_incompatible",
            ResponsePayload::ServerStatus { .. } => "server_status",
            ResponsePayload::ServerSnapshotSaved { .. } => "server_snapshot_saved",
            ResponsePayload::ServerSnapshotRestoreDryRun { .. } => {
                "server_snapshot_restore_dry_run"
            }
            ResponsePayload::ServerSnapshotRestored { .. } => "server_snapshot_restored",
            ResponsePayload::ServerStopping => "server_stopping",
            ResponsePayload::ServiceInvoked { .. } => "service_invoked",
            ResponsePayload::PaneSplit { .. } => "pane_split",
            ResponsePayload::PaneLaunched { .. } => "pane_launched",
            ResponsePayload::PaneFocused { .. } => "pane_focused",
            ResponsePayload::PaneResized { .. } => "pane_resized",
            ResponsePayload::PaneClosed { .. } => "pane_closed",
            ResponsePayload::PaneRestarted { .. } => "pane_restarted",
            ResponsePayload::PaneZoomed { .. } => "pane_zoomed",
            ResponsePayload::Attached { .. } => "attached",
            ResponsePayload::AttachReady { .. } => "attach_ready",
            ResponsePayload::AttachInputAccepted { .. } => "attach_input_accepted",
            ResponsePayload::AttachViewportSet { .. } => "attach_viewport_set",
            ResponsePayload::AttachOutput { .. } => "attach_output",
            ResponsePayload::AttachLayout { .. } => "attach_layout",
            ResponsePayload::AttachSnapshot { .. } => "attach_snapshot",
            ResponsePayload::AttachPaneSnapshot { .. } => "attach_pane_snapshot",
            ResponsePayload::AttachPaneOutputBatch { .. } => "attach_pane_output_batch",
            ResponsePayload::AttachPaneImages { .. } => "attach_pane_images",
            ResponsePayload::ClientAttachPolicySet { .. } => "client_attach_policy_set",
            ResponsePayload::Detached => "detached",
            ResponsePayload::PaneDirectInputAccepted { .. } => "pane_direct_input_accepted",
            ResponsePayload::EventsSubscribed => "events_subscribed",
            ResponsePayload::EventBatch { .. } => "event_batch",
            ResponsePayload::EventPushEnabled => "event_push_enabled",
        },
        Response::Err(_) => "error",
    }
}

/// Resolve a frame compression codec from negotiated capability strings.
///
/// Prefers lz4 for frames (fastest), falls back to zstd.
fn resolve_frame_codec_from_capabilities(
    capabilities: &[String],
) -> Option<Arc<dyn bmux_ipc::compression::CompressionCodec>> {
    use bmux_ipc::compression;
    if capabilities
        .iter()
        .any(|c| c == bmux_ipc::CAPABILITY_COMPRESSION_FRAME_LZ4)
    {
        compression::resolve_codec(compression::CompressionId::Lz4).map(Arc::from)
    } else if capabilities
        .iter()
        .any(|c| c == bmux_ipc::CAPABILITY_COMPRESSION_FRAME_ZSTD)
    {
        compression::resolve_codec(compression::CompressionId::Zstd).map(Arc::from)
    } else {
        None
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

/// Query the terminal's cell pixel width via `TIOCGWINSZ` ioctl.
/// Returns 0 if unavailable.
#[cfg(unix)]
fn cell_pixel_width() -> u16 {
    let (w, _) = cell_pixel_size_from_ioctl();
    w
}

/// Query the terminal's cell pixel height via `TIOCGWINSZ` ioctl.
/// Returns 0 if unavailable.
#[cfg(unix)]
fn cell_pixel_height() -> u16 {
    let (_, h) = cell_pixel_size_from_ioctl();
    h
}

#[cfg(unix)]
fn cell_pixel_size_from_ioctl() -> (u16, u16) {
    use std::os::unix::io::AsRawFd;

    #[repr(C)]
    #[allow(clippy::struct_field_names)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: u64 = 0x4008_7468;
    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: u64 = 0x5413;
    #[cfg(target_os = "android")]
    const TIOCGWINSZ: u64 = 0x5413;

    let fd = std::io::stdout().as_raw_fd();
    let mut ws = std::mem::MaybeUninit::<Winsize>::uninit();
    let ret = unsafe {
        unsafe extern "C" {
            fn ioctl(fd: i32, request: u64, ...) -> i32;
        }
        ioctl(fd, TIOCGWINSZ, ws.as_mut_ptr())
    };
    if ret != 0 {
        return (0, 0);
    }
    let ws = unsafe { ws.assume_init() };
    if ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return (0, 0);
    }
    (ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row)
}

/// Query the terminal's cell pixel width via `GetCurrentConsoleFontEx`.
/// Returns 0 if unavailable.
#[cfg(windows)]
fn cell_pixel_width() -> u16 {
    let (w, _) = cell_pixel_size_from_console();
    w
}

/// Query the terminal's cell pixel height via `GetCurrentConsoleFontEx`.
/// Returns 0 if unavailable.
#[cfg(windows)]
fn cell_pixel_height() -> u16 {
    let (_, h) = cell_pixel_size_from_console();
    h
}

#[cfg(windows)]
fn cell_pixel_size_from_console() -> (u16, u16) {
    use windows_sys::Win32::Foundation::FALSE;
    use windows_sys::Win32::System::Console::{
        CONSOLE_FONT_INFOEX, GetCurrentConsoleFontEx, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return (0, 0);
        }

        let mut font_info: CONSOLE_FONT_INFOEX = std::mem::zeroed();
        font_info.cbSize = std::mem::size_of::<CONSOLE_FONT_INFOEX>() as u32;

        if GetCurrentConsoleFontEx(handle, FALSE, &mut font_info) == 0 {
            return (0, 0);
        }

        let w = font_info.dwFontSize.X;
        let h = font_info.dwFontSize.Y;
        if w <= 0 || h <= 0 {
            return (0, 0);
        }

        (w as u16, h as u16)
    }
}

#[cfg(not(any(unix, windows)))]
fn cell_pixel_width() -> u16 {
    0
}

#[cfg(not(any(unix, windows)))]
fn cell_pixel_height() -> u16 {
    0
}

#[cfg(test)]
mod tests {
    use super::{
        BmuxClient, ClientStream, ConfigPaths, StreamingBmuxClient, load_or_create_principal_id,
    };
    use bmux_ipc::transport::ErasedIpcStream;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn temp_dir() -> TempDir {
        tempfile::Builder::new()
            .prefix("bmux-client-test-")
            .tempdir()
            .expect("temp dir should be created")
    }

    #[test]
    fn load_or_create_principal_id_creates_and_persists_value() {
        let root = temp_dir();
        let paths = ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        let first = load_or_create_principal_id(&paths).expect("principal id should be created");
        let second = load_or_create_principal_id(&paths).expect("principal id should be reused");
        assert_eq!(first, second);
    }

    #[test]
    fn load_or_create_principal_id_rejects_invalid_file_contents() {
        let root = temp_dir();
        let paths = ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        let path = paths.principal_id_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("principal parent should exist");
        }
        fs::write(&path, "not-a-uuid").expect("principal file should be written");
        let error = load_or_create_principal_id(&paths).expect_err("invalid principal should fail");
        assert!(error.to_string().contains("invalid principal id"));
    }

    #[tokio::test]
    async fn streaming_client_upgrade_accepts_bridge_stream() {
        let (bridge_stream, _peer_stream) = tokio::io::duplex(8 * 1024);
        let principal_id = Uuid::new_v4();
        let client = BmuxClient {
            stream: ClientStream::Bridge(ErasedIpcStream::new(Box::new(bridge_stream))),
            timeout: Duration::from_millis(500),
            next_request_id: 1,
            principal_id,
            negotiated_protocol: None,
        };

        let streaming =
            StreamingBmuxClient::from_client(client).expect("bridge stream upgrade should work");
        assert_eq!(streaming.principal_id(), principal_id);
    }
}
