#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! FFI-facing facade for bmux mobile-core.

use bmux_mobile_core::{
    ConnectionManager, ConnectionRequest, ConnectionState, ConnectionStatus, HostKeyPinSuggestion,
    MobileCoreError, ObservedHostKey, TargetInput, TargetRecord, TargetTransport, TerminalChunk,
    TerminalChunkKind, TerminalDiagnostic, TerminalMouseButton, TerminalMouseEvent,
    TerminalMouseEventKind, TerminalOpenRequest, TerminalSessionState, TerminalSessionStatus,
    TerminalSize, TerminalStatusSeverity, apply_pin_query_fragment_to_target,
    apply_pin_suggestion_to_target, observe_ssh_host_key, observe_ssh_host_key_fingerprint_sha256,
    observe_ssh_host_key_with_pin_suggestion,
};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;

uniffi::setup_scaffolding!();

#[derive(Debug, Error, uniffi::Error)]
pub enum MobileFfiError {
    #[error("invalid target: {0}")]
    InvalidTarget(String),
    #[error("target id not found: {0}")]
    TargetNotFound(String),
    #[error("connection {0} is not active")]
    ConnectionNotActive(String),
    #[error("ssh backend is not configured")]
    SshBackendUnavailable,
    #[error("ssh connection failed: {0}")]
    SshConnectionFailed(String),
    #[error("terminal session {0} not found")]
    TerminalSessionNotFound(String),
    #[error("terminal session {0} is closed")]
    TerminalSessionClosed(String),
    #[error("invalid terminal size rows={rows}, cols={cols}")]
    InvalidTerminalSize { rows: u16, cols: u16 },
    #[error("unsupported terminal transport target: {0}")]
    UnsupportedTerminalTransport(String),
    #[error("terminal backend failure: {0}")]
    TerminalBackendFailure(String),
}

impl From<MobileCoreError> for MobileFfiError {
    fn from(value: MobileCoreError) -> Self {
        match value {
            MobileCoreError::InvalidTarget(message) => Self::InvalidTarget(message),
            MobileCoreError::TargetNotFound(message) => Self::TargetNotFound(message),
            MobileCoreError::ConnectionNotActive(message) => Self::ConnectionNotActive(message),
            MobileCoreError::SshBackendUnavailable => Self::SshBackendUnavailable,
            MobileCoreError::SshConnectionFailed(message) => Self::SshConnectionFailed(message),
            MobileCoreError::TerminalSessionNotFound(message) => {
                Self::TerminalSessionNotFound(message)
            }
            MobileCoreError::TerminalSessionClosed(message) => Self::TerminalSessionClosed(message),
            MobileCoreError::InvalidTerminalSize { rows, cols } => {
                Self::InvalidTerminalSize { rows, cols }
            }
            MobileCoreError::UnsupportedTerminalTransport(message) => {
                Self::UnsupportedTerminalTransport(message)
            }
            MobileCoreError::TerminalBackendFailure(message) => {
                Self::TerminalBackendFailure(message)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum TargetTransportFfi {
    Local,
    Ssh,
    Tls,
    Iroh,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ConnectionStatusFfi {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
    Failed,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum TerminalSessionStatusFfi {
    Opening,
    Open,
    Closed,
    Failed,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum TerminalChunkKindFfi {
    Stdout,
    Stderr,
    Status,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum TerminalStatusSeverityFfi {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum TerminalMouseButtonFfi {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum TerminalMouseEventKindFfi {
    Down,
    Up,
    Drag,
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalMouseEventFfi {
    pub kind: TerminalMouseEventKindFfi,
    pub button: Option<TerminalMouseButtonFfi>,
    pub row: u16,
    pub col: u16,
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TargetRecordFfi {
    pub id: String,
    pub name: String,
    pub canonical_target: String,
    pub transport: TargetTransportFfi,
    pub default_session: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ConnectionStateFfi {
    pub id: String,
    pub target_id: String,
    pub status: ConnectionStatusFfi,
    pub session: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalOpenRequestFfi {
    pub target_id: String,
    pub session: Option<String>,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalSizeFfi {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalSessionStateFfi {
    pub id: String,
    pub target_id: String,
    pub connection_id: String,
    pub session: Option<String>,
    pub status: TerminalSessionStatusFfi,
    pub size: TerminalSizeFfi,
    pub last_sequence: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalChunkFfi {
    pub sequence: u64,
    pub kind: TerminalChunkKindFfi,
    pub bytes: Vec<u8>,
    pub status_severity: Option<TerminalStatusSeverityFfi>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalDiagnosticFfi {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub severity: TerminalStatusSeverityFfi,
    pub stage: String,
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservedHostKeyFfi {
    pub endpoint: String,
    pub algorithm: String,
    pub fingerprint_sha256: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct HostKeyPinSuggestionFfi {
    pub observed: ObservedHostKeyFfi,
    pub pin_query_fragment: String,
}

const fn map_transport(value: TargetTransport) -> TargetTransportFfi {
    match value {
        TargetTransport::Local => TargetTransportFfi::Local,
        TargetTransport::Ssh => TargetTransportFfi::Ssh,
        TargetTransport::Tls => TargetTransportFfi::Tls,
        TargetTransport::Iroh => TargetTransportFfi::Iroh,
    }
}

const fn map_connection_status(value: ConnectionStatus) -> ConnectionStatusFfi {
    match value {
        ConnectionStatus::Connecting => ConnectionStatusFfi::Connecting,
        ConnectionStatus::Connected => ConnectionStatusFfi::Connected,
        ConnectionStatus::Reconnecting => ConnectionStatusFfi::Reconnecting,
        ConnectionStatus::Disconnected => ConnectionStatusFfi::Disconnected,
        ConnectionStatus::Failed => ConnectionStatusFfi::Failed,
    }
}

const fn map_terminal_session_status(value: TerminalSessionStatus) -> TerminalSessionStatusFfi {
    match value {
        TerminalSessionStatus::Opening => TerminalSessionStatusFfi::Opening,
        TerminalSessionStatus::Open => TerminalSessionStatusFfi::Open,
        TerminalSessionStatus::Closed => TerminalSessionStatusFfi::Closed,
        TerminalSessionStatus::Failed => TerminalSessionStatusFfi::Failed,
    }
}

const fn map_terminal_chunk_kind(value: TerminalChunkKind) -> TerminalChunkKindFfi {
    match value {
        TerminalChunkKind::Stdout => TerminalChunkKindFfi::Stdout,
        TerminalChunkKind::Stderr => TerminalChunkKindFfi::Stderr,
        TerminalChunkKind::Status => TerminalChunkKindFfi::Status,
    }
}

const fn map_terminal_status_severity(value: TerminalStatusSeverity) -> TerminalStatusSeverityFfi {
    match value {
        TerminalStatusSeverity::Info => TerminalStatusSeverityFfi::Info,
        TerminalStatusSeverity::Warn => TerminalStatusSeverityFfi::Warn,
        TerminalStatusSeverity::Error => TerminalStatusSeverityFfi::Error,
    }
}

const fn map_terminal_mouse_button(value: TerminalMouseButtonFfi) -> TerminalMouseButton {
    match value {
        TerminalMouseButtonFfi::Left => TerminalMouseButton::Left,
        TerminalMouseButtonFfi::Middle => TerminalMouseButton::Middle,
        TerminalMouseButtonFfi::Right => TerminalMouseButton::Right,
    }
}

const fn map_terminal_mouse_event_kind(value: TerminalMouseEventKindFfi) -> TerminalMouseEventKind {
    match value {
        TerminalMouseEventKindFfi::Down => TerminalMouseEventKind::Down,
        TerminalMouseEventKindFfi::Up => TerminalMouseEventKind::Up,
        TerminalMouseEventKindFfi::Drag => TerminalMouseEventKind::Drag,
        TerminalMouseEventKindFfi::Move => TerminalMouseEventKind::Move,
        TerminalMouseEventKindFfi::ScrollUp => TerminalMouseEventKind::ScrollUp,
        TerminalMouseEventKindFfi::ScrollDown => TerminalMouseEventKind::ScrollDown,
        TerminalMouseEventKindFfi::ScrollLeft => TerminalMouseEventKind::ScrollLeft,
        TerminalMouseEventKindFfi::ScrollRight => TerminalMouseEventKind::ScrollRight,
    }
}

fn map_terminal_mouse_event(value: &TerminalMouseEventFfi) -> Result<TerminalMouseEvent> {
    let kind = map_terminal_mouse_event_kind(value.kind);
    if matches!(
        kind,
        TerminalMouseEventKind::Down | TerminalMouseEventKind::Up | TerminalMouseEventKind::Drag
    ) && value.button.is_none()
    {
        return Err(MobileCoreError::InvalidTarget(
            "mouse event kind requires button".to_string(),
        ));
    }

    Ok(TerminalMouseEvent {
        kind,
        button: value.button.map(map_terminal_mouse_button),
        row: value.row,
        col: value.col,
        shift: value.shift,
        alt: value.alt,
        control: value.control,
    })
}

fn map_target_record(value: TargetRecord) -> TargetRecordFfi {
    TargetRecordFfi {
        id: value.id.to_string(),
        name: value.name,
        canonical_target: value.canonical_target.value,
        transport: map_transport(value.transport),
        default_session: value.default_session,
    }
}

fn map_connection_state(value: ConnectionState) -> ConnectionStateFfi {
    ConnectionStateFfi {
        id: value.id.to_string(),
        target_id: value.target_id.to_string(),
        status: map_connection_status(value.status),
        session: value.session,
        last_error: value.last_error,
    }
}

fn map_terminal_session_state(value: TerminalSessionState) -> TerminalSessionStateFfi {
    TerminalSessionStateFfi {
        id: value.id.to_string(),
        target_id: value.target_id.to_string(),
        connection_id: value.connection_id.to_string(),
        session: value.session,
        status: map_terminal_session_status(value.status),
        size: TerminalSizeFfi {
            rows: value.size.rows,
            cols: value.size.cols,
        },
        last_sequence: value.last_sequence,
    }
}

fn map_terminal_chunk(value: TerminalChunk) -> TerminalChunkFfi {
    TerminalChunkFfi {
        sequence: value.sequence,
        kind: map_terminal_chunk_kind(value.kind),
        bytes: value.bytes,
        status_severity: value.status_severity.map(map_terminal_status_severity),
    }
}

fn map_terminal_diagnostic(value: TerminalDiagnostic) -> TerminalDiagnosticFfi {
    TerminalDiagnosticFfi {
        sequence: value.sequence,
        timestamp_ms: value.timestamp_ms,
        severity: map_terminal_status_severity(value.severity),
        stage: value.stage,
        code: value.code,
        message: value.message,
    }
}

fn map_terminal_open_request(value: &TerminalOpenRequestFfi) -> Result<TerminalOpenRequest> {
    let target_id = Uuid::parse_str(&value.target_id)
        .map_err(|_| MobileCoreError::InvalidTarget(value.target_id.clone()))?;
    Ok(TerminalOpenRequest {
        target_id,
        session: value.session.clone(),
        rows: value.rows,
        cols: value.cols,
    })
}

const fn map_terminal_size(value: &TerminalSizeFfi) -> TerminalSize {
    TerminalSize {
        rows: value.rows,
        cols: value.cols,
    }
}

fn map_observed_host_key(value: ObservedHostKey) -> ObservedHostKeyFfi {
    ObservedHostKeyFfi {
        endpoint: value.endpoint,
        algorithm: value.algorithm,
        fingerprint_sha256: value.fingerprint_sha256,
    }
}

fn map_pin_suggestion(value: HostKeyPinSuggestion) -> HostKeyPinSuggestionFfi {
    HostKeyPinSuggestionFfi {
        observed: map_observed_host_key(value.observed),
        pin_query_fragment: value.pin_query_fragment,
    }
}

fn map_pin_suggestion_to_core(value: &HostKeyPinSuggestionFfi) -> HostKeyPinSuggestion {
    HostKeyPinSuggestion {
        observed: ObservedHostKey {
            endpoint: value.observed.endpoint.clone(),
            algorithm: value.observed.algorithm.clone(),
            fingerprint_sha256: value.observed.fingerprint_sha256.clone(),
        },
        pin_query_fragment: value.pin_query_fragment.clone(),
    }
}

#[derive(Clone, Default)]
pub struct MobileApi {
    manager: Arc<Mutex<ConnectionManager>>,
}

impl MobileApi {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_manager(manager: ConnectionManager) -> Self {
        Self {
            manager: Arc::new(Mutex::new(manager)),
        }
    }

    /// Import a target into the shared connection manager.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::InvalidTarget`] when parsing fails.
    pub fn import_target(
        &self,
        source: &str,
        display_name: Option<String>,
    ) -> Result<TargetRecord> {
        let mut manager = self.lock_manager()?;
        manager.import_target(&TargetInput {
            source: source.to_string(),
            display_name,
        })
    }

    /// List all imported targets.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::ConnectionNotActive`] if the manager lock is poisoned.
    pub fn list_targets(&self) -> Result<Vec<TargetRecord>> {
        let manager = self.lock_manager()?;
        Ok(manager.list_targets())
    }

    /// Start a connection for a target id and optional session.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::InvalidTarget`] for invalid UUID input and
    /// [`MobileCoreError::TargetNotFound`] for unknown targets.
    pub fn connect(&self, target_id: &str, session: Option<String>) -> Result<ConnectionState> {
        let target_id = Uuid::parse_str(target_id)
            .map_err(|_| MobileCoreError::InvalidTarget(target_id.to_string()))?;
        let mut manager = self.lock_manager()?;
        manager.connect(ConnectionRequest { target_id, session })
    }

    /// Mark an active connection as connected.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::ConnectionNotActive`] when the id is invalid or missing.
    pub fn mark_connected(&self, connection_id: &str) -> Result<ConnectionState> {
        let connection_id = Uuid::parse_str(connection_id)
            .map_err(|_| MobileCoreError::ConnectionNotActive(connection_id.to_string()))?;
        let mut manager = self.lock_manager()?;
        manager.mark_connected(connection_id)
    }

    /// Mark an active connection as disconnected.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::ConnectionNotActive`] when the id is invalid or missing.
    pub fn disconnect(&self, connection_id: &str) -> Result<ConnectionState> {
        let connection_id = Uuid::parse_str(connection_id)
            .map_err(|_| MobileCoreError::ConnectionNotActive(connection_id.to_string()))?;
        let mut manager = self.lock_manager()?;
        manager.disconnect(connection_id)
    }

    /// Open a terminal stream for a target and optional session.
    ///
    /// # Errors
    ///
    /// Returns UUID parsing, target lookup, and transport setup errors.
    pub fn open_terminal(&self, request: &TerminalOpenRequestFfi) -> Result<TerminalSessionState> {
        let request = map_terminal_open_request(request)?;
        let mut manager = self.lock_manager()?;
        manager.open_terminal(request)
    }

    /// Return current state for a terminal stream.
    ///
    /// # Errors
    ///
    /// Returns UUID parsing or terminal lookup errors.
    pub fn terminal_state(&self, terminal_id: &str) -> Result<TerminalSessionState> {
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let manager = self.lock_manager()?;
        manager.terminal_state(terminal_id)
    }

    /// Read pending output chunks from a terminal stream.
    ///
    /// # Errors
    ///
    /// Returns UUID parsing or terminal lookup errors.
    pub fn poll_terminal_output(
        &self,
        terminal_id: &str,
        max_chunks: u32,
    ) -> Result<Vec<TerminalChunk>> {
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let max_chunks = usize::try_from(max_chunks)
            .map_err(|_| MobileCoreError::InvalidTarget("max_chunks out of range".to_string()))?;
        let mut manager = self.lock_manager()?;
        manager.poll_terminal_output(terminal_id, max_chunks)
    }

    /// Read terminal diagnostics since a sequence marker.
    ///
    /// # Errors
    ///
    /// Returns UUID parsing or terminal lookup errors.
    pub fn terminal_diagnostics(
        &self,
        terminal_id: &str,
        since_sequence: Option<u64>,
        limit: u32,
    ) -> Result<Vec<TerminalDiagnostic>> {
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let limit = usize::try_from(limit)
            .map_err(|_| MobileCoreError::InvalidTarget("limit out of range".to_string()))?;
        let manager = self.lock_manager()?;
        manager.terminal_diagnostics(terminal_id, since_sequence, limit)
    }

    /// Return latest terminal failure message if present.
    ///
    /// # Errors
    ///
    /// Returns UUID parsing or terminal lookup errors.
    pub fn latest_terminal_failure(&self, terminal_id: &str) -> Result<Option<String>> {
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let manager = self.lock_manager()?;
        manager.latest_terminal_failure(terminal_id)
    }

    /// Write input bytes to a terminal stream.
    ///
    /// # Errors
    ///
    /// Returns UUID parsing, terminal lookup, or closed-session errors.
    pub fn write_terminal_input(&self, terminal_id: &str, bytes: &[u8]) -> Result<()> {
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let mut manager = self.lock_manager()?;
        manager.write_terminal_input(terminal_id, bytes)
    }

    /// Send a terminal mouse event.
    ///
    /// # Errors
    ///
    /// Returns UUID parsing, terminal lookup, or closed-session errors.
    pub fn send_terminal_mouse_event(
        &self,
        terminal_id: &str,
        event: &TerminalMouseEventFfi,
    ) -> Result<()> {
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let event = map_terminal_mouse_event(event)?;
        let mut manager = self.lock_manager()?;
        manager.send_terminal_mouse_event(terminal_id, event)
    }

    /// Resize a terminal stream.
    ///
    /// # Errors
    ///
    /// Returns UUID parsing, size validation, terminal lookup, or closed-session errors.
    pub fn resize_terminal(&self, terminal_id: &str, size: &TerminalSizeFfi) -> Result<()> {
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let mut manager = self.lock_manager()?;
        manager.resize_terminal(terminal_id, map_terminal_size(size))
    }

    /// Close a terminal stream.
    ///
    /// # Errors
    ///
    /// Returns UUID parsing or terminal lookup errors.
    pub fn close_terminal(&self, terminal_id: &str) -> Result<TerminalSessionState> {
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let mut manager = self.lock_manager()?;
        manager.close_terminal(terminal_id)
    }

    /// Observe and return server SSH host-key SHA-256 fingerprint.
    ///
    /// # Errors
    ///
    /// Returns parse, DNS/network, handshake, or fingerprint availability
    /// errors.
    pub fn observe_ssh_host_key_fingerprint_sha256(&self, target: &str) -> Result<String> {
        observe_ssh_host_key_fingerprint_sha256(target)
    }

    /// Observe and return structured server SSH host-key details.
    ///
    /// # Errors
    ///
    /// Returns parse, DNS/network, handshake, or host-key availability
    /// errors.
    pub fn observe_ssh_host_key(&self, target: &str) -> Result<ObservedHostKey> {
        observe_ssh_host_key(target)
    }

    /// Observe host-key details and return a prebuilt pin query fragment.
    ///
    /// # Errors
    ///
    /// Returns parse, DNS/network, handshake, or host-key availability
    /// errors.
    pub fn observe_ssh_host_key_with_pin_suggestion(
        &self,
        target: &str,
    ) -> Result<HostKeyPinSuggestion> {
        observe_ssh_host_key_with_pin_suggestion(target)
    }

    /// Apply a host-key pin fragment to an SSH target URI.
    ///
    /// Existing host-key pin query params are removed before applying the new
    /// fragment.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::InvalidTarget`] for invalid target or pin
    /// fragment input.
    pub fn apply_pin_query_fragment_to_target(
        &self,
        target: &str,
        pin_query_fragment: &str,
    ) -> Result<String> {
        apply_pin_query_fragment_to_target(target, pin_query_fragment)
    }

    /// Apply a typed host-key pin suggestion to an SSH target URI.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::InvalidTarget`] for invalid target or
    /// suggestion data.
    pub fn apply_pin_suggestion_to_target(
        &self,
        target: &str,
        suggestion: &HostKeyPinSuggestion,
    ) -> Result<String> {
        apply_pin_suggestion_to_target(target, suggestion)
    }

    fn lock_manager(&self) -> Result<std::sync::MutexGuard<'_, ConnectionManager>> {
        self.manager.lock().map_err(|_| {
            MobileCoreError::ConnectionNotActive("mobile api manager poisoned".to_string())
        })
    }
}

#[derive(uniffi::Object)]
pub struct MobileApiFfi {
    inner: MobileApi,
}

impl Default for MobileApiFfi {
    fn default() -> Self {
        Self::new()
    }
}

#[uniffi::export]
// UniFFI generated bindings pass owned values for Rust String/Record inputs.
#[allow(clippy::needless_pass_by_value)]
impl MobileApiFfi {
    #[uniffi::constructor]
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: MobileApi::new(),
        }
    }

    /// Import and normalize a target.
    ///
    /// # Errors
    ///
    /// Returns mapped target parsing and storage errors from `mobile-core`.
    pub fn import_target(
        &self,
        source: String,
        display_name: Option<String>,
    ) -> std::result::Result<TargetRecordFfi, MobileFfiError> {
        self.inner
            .import_target(&source, display_name)
            .map(map_target_record)
            .map_err(MobileFfiError::from)
    }

    /// List imported targets.
    ///
    /// # Errors
    ///
    /// Returns mapped manager lock/state errors from `mobile-core`.
    pub fn list_targets(&self) -> std::result::Result<Vec<TargetRecordFfi>, MobileFfiError> {
        self.inner
            .list_targets()
            .map(|targets| targets.into_iter().map(map_target_record).collect())
            .map_err(MobileFfiError::from)
    }

    /// Start a connection for a target id.
    ///
    /// # Errors
    ///
    /// Returns mapped target lookup, parsing, and transport errors.
    pub fn connect(
        &self,
        target_id: String,
        session: Option<String>,
    ) -> std::result::Result<ConnectionStateFfi, MobileFfiError> {
        self.inner
            .connect(&target_id, session)
            .map(map_connection_state)
            .map_err(MobileFfiError::from)
    }

    /// Mark a connection as connected.
    ///
    /// # Errors
    ///
    /// Returns mapped connection state errors from `mobile-core`.
    pub fn mark_connected(
        &self,
        connection_id: String,
    ) -> std::result::Result<ConnectionStateFfi, MobileFfiError> {
        self.inner
            .mark_connected(&connection_id)
            .map(map_connection_state)
            .map_err(MobileFfiError::from)
    }

    /// Mark a connection as disconnected.
    ///
    /// # Errors
    ///
    /// Returns mapped connection state errors from `mobile-core`.
    pub fn disconnect(
        &self,
        connection_id: String,
    ) -> std::result::Result<ConnectionStateFfi, MobileFfiError> {
        self.inner
            .disconnect(&connection_id)
            .map(map_connection_state)
            .map_err(MobileFfiError::from)
    }

    /// Open terminal stream for a target.
    ///
    /// # Errors
    ///
    /// Returns mapped target, UUID, and transport setup errors.
    pub fn open_terminal(
        &self,
        request: TerminalOpenRequestFfi,
    ) -> std::result::Result<TerminalSessionStateFfi, MobileFfiError> {
        self.inner
            .open_terminal(&request)
            .map(map_terminal_session_state)
            .map_err(MobileFfiError::from)
    }

    /// Return terminal stream state.
    ///
    /// # Errors
    ///
    /// Returns mapped UUID parsing and terminal lookup errors.
    pub fn terminal_state(
        &self,
        terminal_id: String,
    ) -> std::result::Result<TerminalSessionStateFfi, MobileFfiError> {
        self.inner
            .terminal_state(&terminal_id)
            .map(map_terminal_session_state)
            .map_err(MobileFfiError::from)
    }

    /// Poll pending terminal output chunks.
    ///
    /// # Errors
    ///
    /// Returns mapped UUID parsing and terminal lookup errors.
    pub fn poll_terminal_output(
        &self,
        terminal_id: String,
        max_chunks: u32,
    ) -> std::result::Result<Vec<TerminalChunkFfi>, MobileFfiError> {
        self.inner
            .poll_terminal_output(&terminal_id, max_chunks)
            .map(|chunks| chunks.into_iter().map(map_terminal_chunk).collect())
            .map_err(MobileFfiError::from)
    }

    /// Read terminal diagnostics since a sequence marker.
    ///
    /// # Errors
    ///
    /// Returns mapped UUID parsing and terminal lookup errors.
    pub fn terminal_diagnostics(
        &self,
        terminal_id: String,
        since_sequence: Option<u64>,
        limit: u32,
    ) -> std::result::Result<Vec<TerminalDiagnosticFfi>, MobileFfiError> {
        self.inner
            .terminal_diagnostics(&terminal_id, since_sequence, limit)
            .map(|events| events.into_iter().map(map_terminal_diagnostic).collect())
            .map_err(MobileFfiError::from)
    }

    /// Return latest terminal failure message if present.
    ///
    /// # Errors
    ///
    /// Returns mapped UUID parsing and terminal lookup errors.
    pub fn latest_terminal_failure(
        &self,
        terminal_id: String,
    ) -> std::result::Result<Option<String>, MobileFfiError> {
        self.inner
            .latest_terminal_failure(&terminal_id)
            .map_err(MobileFfiError::from)
    }

    /// Write terminal input bytes.
    ///
    /// # Errors
    ///
    /// Returns mapped UUID parsing, terminal lookup, and closed-session errors.
    pub fn write_terminal_input(
        &self,
        terminal_id: String,
        bytes: Vec<u8>,
    ) -> std::result::Result<(), MobileFfiError> {
        self.inner
            .write_terminal_input(&terminal_id, &bytes)
            .map_err(MobileFfiError::from)
    }

    /// Send a terminal mouse event.
    ///
    /// # Errors
    ///
    /// Returns mapped UUID parsing, terminal lookup, and closed-session errors.
    pub fn send_terminal_mouse_event(
        &self,
        terminal_id: String,
        event: TerminalMouseEventFfi,
    ) -> std::result::Result<(), MobileFfiError> {
        self.inner
            .send_terminal_mouse_event(&terminal_id, &event)
            .map_err(MobileFfiError::from)
    }

    /// Resize terminal stream.
    ///
    /// # Errors
    ///
    /// Returns mapped UUID parsing, size validation, terminal lookup, and
    /// closed-session errors.
    pub fn resize_terminal(
        &self,
        terminal_id: String,
        size: TerminalSizeFfi,
    ) -> std::result::Result<(), MobileFfiError> {
        self.inner
            .resize_terminal(&terminal_id, &size)
            .map_err(MobileFfiError::from)
    }

    /// Close terminal stream.
    ///
    /// # Errors
    ///
    /// Returns mapped UUID parsing and terminal lookup errors.
    pub fn close_terminal(
        &self,
        terminal_id: String,
    ) -> std::result::Result<TerminalSessionStateFfi, MobileFfiError> {
        self.inner
            .close_terminal(&terminal_id)
            .map(map_terminal_session_state)
            .map_err(MobileFfiError::from)
    }

    /// Observe only the SSH host-key SHA-256 fingerprint.
    ///
    /// # Errors
    ///
    /// Returns mapped parse, network, handshake, and hash availability errors.
    pub fn observe_ssh_host_key_fingerprint_sha256(
        &self,
        target: String,
    ) -> std::result::Result<String, MobileFfiError> {
        self.inner
            .observe_ssh_host_key_fingerprint_sha256(&target)
            .map_err(MobileFfiError::from)
    }

    /// Observe structured SSH host-key details.
    ///
    /// # Errors
    ///
    /// Returns mapped parse, network, handshake, and key availability errors.
    pub fn observe_ssh_host_key(
        &self,
        target: String,
    ) -> std::result::Result<ObservedHostKeyFfi, MobileFfiError> {
        self.inner
            .observe_ssh_host_key(&target)
            .map(map_observed_host_key)
            .map_err(MobileFfiError::from)
    }

    /// Observe SSH host key and derive a pin suggestion.
    ///
    /// # Errors
    ///
    /// Returns mapped parse, network, handshake, and key availability errors.
    pub fn observe_ssh_host_key_with_pin_suggestion(
        &self,
        target: String,
    ) -> std::result::Result<HostKeyPinSuggestionFfi, MobileFfiError> {
        self.inner
            .observe_ssh_host_key_with_pin_suggestion(&target)
            .map(map_pin_suggestion)
            .map_err(MobileFfiError::from)
    }

    /// Apply a pin query fragment to a target.
    ///
    /// # Errors
    ///
    /// Returns mapped target or pin validation errors.
    pub fn apply_pin_query_fragment_to_target(
        &self,
        target: String,
        pin_query_fragment: String,
    ) -> std::result::Result<String, MobileFfiError> {
        self.inner
            .apply_pin_query_fragment_to_target(&target, &pin_query_fragment)
            .map_err(MobileFfiError::from)
    }

    /// Apply a typed pin suggestion to a target.
    ///
    /// # Errors
    ///
    /// Returns mapped target or pin validation errors.
    pub fn apply_pin_suggestion_to_target(
        &self,
        target: String,
        suggestion: HostKeyPinSuggestionFfi,
    ) -> std::result::Result<String, MobileFfiError> {
        let core_suggestion = map_pin_suggestion_to_core(&suggestion);
        self.inner
            .apply_pin_suggestion_to_target(&target, &core_suggestion)
            .map_err(MobileFfiError::from)
    }
}

pub type Result<T> = std::result::Result<T, MobileCoreError>;

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_mobile_core::ConnectionManager;
    use bmux_mobile_core::remote_bridge::{BackendSessionHandle, TerminalBackend};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[derive(Default)]
    struct MockTerminalBackend {
        sessions: Mutex<BTreeMap<Uuid, Vec<u8>>>,
    }

    impl TerminalBackend for MockTerminalBackend {
        fn open(
            &self,
            _target: &TargetRecord,
            _session: Option<String>,
            _rows: u16,
            _cols: u16,
        ) -> Result<BackendSessionHandle> {
            let id = Uuid::new_v4();
            self.sessions
                .lock()
                .expect("mock terminal sessions lock")
                .insert(id, Vec::new());
            Ok(BackendSessionHandle {
                id,
                session_id: Uuid::new_v4(),
                can_write: true,
            })
        }

        fn poll_output(&self, handle_id: Uuid, _max_bytes: usize) -> Result<Vec<u8>> {
            let mut sessions = self.sessions.lock().expect("mock terminal sessions lock");
            let output =
                std::mem::take(sessions.get_mut(&handle_id).ok_or_else(|| {
                    MobileCoreError::TerminalSessionNotFound(handle_id.to_string())
                })?);
            drop(sessions);
            Ok(output)
        }

        fn write_input(&self, handle_id: Uuid, bytes: &[u8]) -> Result<()> {
            let mut sessions = self.sessions.lock().expect("mock terminal sessions lock");
            sessions
                .get_mut(&handle_id)
                .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(handle_id.to_string()))?
                .extend_from_slice(bytes);
            drop(sessions);
            Ok(())
        }

        fn mouse_event(&self, handle_id: Uuid, _event: &TerminalMouseEvent) -> Result<()> {
            if self
                .sessions
                .lock()
                .expect("mock terminal sessions lock")
                .contains_key(&handle_id)
            {
                Ok(())
            } else {
                Err(MobileCoreError::TerminalSessionNotFound(
                    handle_id.to_string(),
                ))
            }
        }

        fn resize(&self, handle_id: Uuid, _rows: u16, _cols: u16) -> Result<()> {
            if self
                .sessions
                .lock()
                .expect("mock terminal sessions lock")
                .contains_key(&handle_id)
            {
                Ok(())
            } else {
                Err(MobileCoreError::TerminalSessionNotFound(
                    handle_id.to_string(),
                ))
            }
        }

        fn close(&self, handle_id: Uuid) -> Result<()> {
            if self
                .sessions
                .lock()
                .expect("mock terminal sessions lock")
                .remove(&handle_id)
                .is_some()
            {
                Ok(())
            } else {
                Err(MobileCoreError::TerminalSessionNotFound(
                    handle_id.to_string(),
                ))
            }
        }
    }

    #[test]
    fn ffi_facade_import_and_connect() {
        let api = MobileApi::new();
        let target = api
            .import_target("iroh://endpoint-abc", Some("demo".to_string()))
            .expect("target import should work");

        let state = api
            .connect(&target.id.to_string(), Some("main".to_string()))
            .expect("connection start should work");

        let connected = api
            .mark_connected(&state.id.to_string())
            .expect("connection transition should work");

        assert_eq!(connected.target_id, target.id);
    }

    #[test]
    fn ffi_invalid_ssh_target_fingerprint_request_fails() {
        let api = MobileApi::new();
        let result = api.observe_ssh_host_key_fingerprint_sha256("ssh://bad-target:abc");
        assert!(matches!(result, Err(MobileCoreError::InvalidTarget(_))));
    }

    #[test]
    fn ffi_invalid_ssh_target_host_key_request_fails() {
        let api = MobileApi::new();
        let result = api.observe_ssh_host_key("ssh://bad-target:abc");
        assert!(matches!(result, Err(MobileCoreError::InvalidTarget(_))));
    }

    #[test]
    fn ffi_invalid_ssh_target_pin_suggestion_request_fails() {
        let api = MobileApi::new();
        let result = api.observe_ssh_host_key_with_pin_suggestion("ssh://bad-target:abc");
        assert!(matches!(result, Err(MobileCoreError::InvalidTarget(_))));
    }

    #[test]
    fn ffi_apply_pin_fragment_updates_target() {
        let api = MobileApi::new();
        let result = api
            .apply_pin_query_fragment_to_target(
                "ssh://ops@prod.example.com:22?strict=false&host_key_fp=sha256:abababababababababababababababababababababababababababababababab",
                "host_key_fp=sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
            )
            .expect("pin fragment should apply");

        assert_eq!(
            result,
            "ssh://ops@prod.example.com:22?strict=false&host_key_fp=sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
        );
    }

    #[test]
    fn ffi_apply_pin_suggestion_updates_target() {
        let api = MobileApi::new();
        let suggestion = HostKeyPinSuggestion {
            observed: ObservedHostKey {
                endpoint: "prod.example.com:22".to_string(),
                algorithm: "ssh-ed25519".to_string(),
                fingerprint_sha256:
                    "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
                        .to_string(),
            },
            pin_query_fragment:
                "host_key_fp=sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
                    .to_string(),
        };

        let result = api
            .apply_pin_suggestion_to_target(
                "ssh://ops@prod.example.com:22?strict=true",
                &suggestion,
            )
            .expect("pin suggestion should apply");

        assert_eq!(
            result,
            "ssh://ops@prod.example.com:22?strict=true&host_key_fp=sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
        );
    }

    #[test]
    fn ffi_terminal_stream_round_trip() {
        let api = MobileApi::with_manager(ConnectionManager::with_terminal_backend(Arc::new(
            MockTerminalBackend::default(),
        )));
        let target = api
            .import_target("iroh://endpoint-tty", Some("tty".to_string()))
            .expect("target import should work");

        let terminal = api
            .open_terminal(&TerminalOpenRequestFfi {
                target_id: target.id.to_string(),
                session: Some("main".to_string()),
                rows: 24,
                cols: 80,
            })
            .expect("terminal open should work");

        api.write_terminal_input(&terminal.id.to_string(), b"pwd\n")
            .expect("terminal write should work");

        api.send_terminal_mouse_event(
            &terminal.id.to_string(),
            &TerminalMouseEventFfi {
                kind: TerminalMouseEventKindFfi::Down,
                button: Some(TerminalMouseButtonFfi::Left),
                row: 0,
                col: 0,
                shift: false,
                alt: false,
                control: false,
            },
        )
        .expect("terminal mouse event should work");

        let chunks = api
            .poll_terminal_output(&terminal.id.to_string(), 8)
            .expect("terminal poll should work");
        assert!(!chunks.is_empty());
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.status_severity == Some(TerminalStatusSeverity::Info))
        );

        api.resize_terminal(
            &terminal.id.to_string(),
            &TerminalSizeFfi {
                rows: 40,
                cols: 120,
            },
        )
        .expect("terminal resize should work");

        let closed = api
            .close_terminal(&terminal.id.to_string())
            .expect("terminal close should work");
        assert_eq!(closed.status, TerminalSessionStatus::Closed);
    }
}
