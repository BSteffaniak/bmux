#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! FFI-facing facade for bmux mobile-core.

use bmux_mobile_core::{
    ConnectionManager, ConnectionRequest, ConnectionState, ConnectionStatus, MobileCoreError,
    PairedHost, PairedHostRole, PairingRequest, PairingState, PairingStatus, TerminalChunk,
    TerminalChunkKind, TerminalDiagnostic, TerminalMouseButton, TerminalMouseEvent,
    TerminalMouseEventKind, TerminalOpenRequest, TerminalSessionState, TerminalSessionStatus,
    TerminalSize, TerminalStatusSeverity,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;

uniffi::setup_scaffolding!();

#[derive(Debug, Error, uniffi::Error)]
pub enum MobileFfiError {
    #[error("invalid pairing link: {0}")]
    InvalidPairingLink(String),
    #[error("pairing {0} not found")]
    PairingNotFound(String),
    #[error("no saved host named '{0}'")]
    HostNotFound(String),
    #[error("saved hosts: {0}")]
    HostStore(String),
    #[error("{0}")]
    ConnectionFailed(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("connection {0} is not active")]
    ConnectionNotActive(String),
    #[error("terminal session {0} not found")]
    TerminalSessionNotFound(String),
    #[error("terminal session {0} is closed")]
    TerminalSessionClosed(String),
    #[error("invalid terminal size rows={rows}, cols={cols}")]
    InvalidTerminalSize { rows: u16, cols: u16 },
    #[error("terminal backend failure: {0}")]
    TerminalBackendFailure(String),
}

impl From<MobileCoreError> for MobileFfiError {
    fn from(value: MobileCoreError) -> Self {
        match value {
            MobileCoreError::InvalidPairingLink(message) => Self::InvalidPairingLink(message),
            MobileCoreError::PairingNotFound(message) => Self::PairingNotFound(message),
            MobileCoreError::HostNotFound(message) => Self::HostNotFound(message),
            MobileCoreError::HostStore(message) => Self::HostStore(message),
            MobileCoreError::ConnectionFailed(message) => Self::ConnectionFailed(message),
            MobileCoreError::InvalidArgument(message) => Self::InvalidArgument(message),
            MobileCoreError::ConnectionNotActive(message) => Self::ConnectionNotActive(message),
            MobileCoreError::TerminalSessionNotFound(message) => {
                Self::TerminalSessionNotFound(message)
            }
            MobileCoreError::TerminalSessionClosed(message) => Self::TerminalSessionClosed(message),
            MobileCoreError::InvalidTerminalSize { rows, cols } => {
                Self::InvalidTerminalSize { rows, cols }
            }
            MobileCoreError::TerminalBackendFailure(message) => {
                Self::TerminalBackendFailure(message)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PairedHostRoleFfi {
    Full,
    ViewOnly,
}

/// A saved paired host. It carries no secret.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairedHostFfi {
    pub alias: String,
    /// The pinned host key (Iroh endpoint id).
    pub host_key: String,
    /// The role granted at pairing; the host enforces the current one.
    pub role: Option<PairedHostRoleFfi>,
    /// `direct` or `relay`, from the last connection.
    pub last_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PairingStatusFfi {
    Connecting,
    AwaitingApproval,
    Paired,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PairingStateFfi {
    pub id: String,
    pub status: PairingStatusFfi,
    /// The phrase both devices show while the host owner decides.
    pub phrase: Option<String>,
    /// The saved host, once paired.
    pub host: Option<PairedHostFfi>,
    pub error: Option<String>,
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
    Leave,
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
pub struct ConnectionStateFfi {
    pub id: String,
    pub host: String,
    pub status: ConnectionStatusFfi,
    pub session: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalOpenRequestFfi {
    /// Alias of a saved paired host.
    pub host: String,
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
    pub host: String,
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

/// Native pointer metadata v1, never bytes sent into a terminal emulator.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum TerminalPointerShapeFfi {
    Hidden,
    /// A validated CSS pointer vocabulary name, not an arbitrary escape payload.
    Shape {
        name: String,
    },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalPointerOutputFfi {
    pub version: u32,
    pub generation: String,
    pub revision: u64,
    pub shape: TerminalPointerShapeFfi,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalPresentationFfi {
    pub chunks: Vec<TerminalChunkFfi>,
    pub pointer: Option<TerminalPointerOutputFfi>,
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

const fn map_role(value: PairedHostRole) -> PairedHostRoleFfi {
    match value {
        PairedHostRole::Full => PairedHostRoleFfi::Full,
        PairedHostRole::ViewOnly => PairedHostRoleFfi::ViewOnly,
    }
}

fn map_paired_host(value: PairedHost) -> PairedHostFfi {
    PairedHostFfi {
        alias: value.alias,
        host_key: value.host_key,
        role: value.role.map(map_role),
        last_path: value.last_path,
    }
}

const fn map_pairing_status(value: PairingStatus) -> PairingStatusFfi {
    match value {
        PairingStatus::Connecting => PairingStatusFfi::Connecting,
        PairingStatus::AwaitingApproval => PairingStatusFfi::AwaitingApproval,
        PairingStatus::Paired => PairingStatusFfi::Paired,
        PairingStatus::Failed => PairingStatusFfi::Failed,
        PairingStatus::Cancelled => PairingStatusFfi::Cancelled,
    }
}

fn map_pairing_state(value: PairingState) -> PairingStateFfi {
    PairingStateFfi {
        id: value.id.to_string(),
        status: map_pairing_status(value.status),
        phrase: value.phrase,
        host: value.host.map(map_paired_host),
        error: value.error,
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
        TerminalMouseEventKindFfi::Leave => TerminalMouseEventKind::Leave,
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
        return Err(MobileCoreError::InvalidArgument(
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

fn map_connection_state(value: ConnectionState) -> ConnectionStateFfi {
    ConnectionStateFfi {
        id: value.id.to_string(),
        host: value.host,
        status: map_connection_status(value.status),
        session: value.session,
        last_error: value.last_error,
    }
}

fn map_terminal_session_state(value: TerminalSessionState) -> TerminalSessionStateFfi {
    TerminalSessionStateFfi {
        id: value.id.to_string(),
        host: value.host,
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

fn map_terminal_open_request(value: &TerminalOpenRequestFfi) -> TerminalOpenRequest {
    TerminalOpenRequest {
        host: value.host.clone(),
        session: value.session.clone(),
        rows: value.rows,
        cols: value.cols,
    }
}

const fn map_terminal_size(value: &TerminalSizeFfi) -> TerminalSize {
    TerminalSize {
        rows: value.rows,
        cols: value.cols,
    }
}

fn parse_id(value: &str, error: impl FnOnce(String) -> MobileCoreError) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| error(value.to_string()))
}

#[derive(Clone)]
pub struct MobileApi {
    manager: Arc<Mutex<ConnectionManager>>,
}

impl MobileApi {
    /// An API whose device key and saved hosts live under `storage_root`, an
    /// app-private directory.
    #[must_use]
    pub fn new(storage_root: &Path) -> Self {
        Self::with_manager(ConnectionManager::new(storage_root))
    }

    fn with_manager(manager: ConnectionManager) -> Self {
        Self {
            manager: Arc::new(Mutex::new(manager)),
        }
    }

    /// List saved paired hosts.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::HostStore`] when the saved hosts cannot be read.
    pub fn list_hosts(&self) -> Result<Vec<PairedHost>> {
        self.lock_manager()?.list_hosts()
    }

    /// Rename a saved host.
    ///
    /// # Errors
    ///
    /// Returns host lookup and alias validation errors.
    pub fn rename_host(&self, alias: &str, new_alias: &str) -> Result<PairedHost> {
        self.lock_manager()?.rename_host(alias, new_alias)
    }

    /// Forget a saved host on this device.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::HostNotFound`] for an unknown alias.
    pub fn forget_host(&self, alias: &str) -> Result<()> {
        self.lock_manager()?.forget_host(alias)
    }

    /// Start pairing through a `bmux-pair:` link.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::InvalidPairingLink`] for a malformed link.
    pub fn start_pairing(
        &self,
        link: String,
        alias: Option<String>,
        device_name: String,
    ) -> Result<PairingState> {
        self.lock_manager()?.start_pairing(PairingRequest {
            link,
            alias,
            device_name,
        })
    }

    /// The progress of a pairing.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::PairingNotFound`] for an unknown pairing.
    pub fn pairing_state(&self, pairing_id: &str) -> Result<PairingState> {
        let pairing_id = parse_id(pairing_id, MobileCoreError::PairingNotFound)?;
        self.lock_manager()?.pairing_state(pairing_id)
    }

    /// Cancel a pairing in progress.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::PairingNotFound`] for an unknown pairing.
    pub fn cancel_pairing(&self, pairing_id: &str) -> Result<PairingState> {
        let pairing_id = parse_id(pairing_id, MobileCoreError::PairingNotFound)?;
        self.lock_manager()?.cancel_pairing(pairing_id)
    }

    /// Connect to a saved host: dial it and complete the handshake.
    ///
    /// # Errors
    ///
    /// Returns host lookup and connection errors.
    pub fn connect(&self, host: &str, session: Option<String>) -> Result<ConnectionState> {
        self.lock_manager()?.connect(ConnectionRequest {
            host: host.to_string(),
            session,
        })
    }

    /// Mark a connection as disconnected.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::ConnectionNotActive`] when the id is invalid or missing.
    pub fn disconnect(&self, connection_id: &str) -> Result<ConnectionState> {
        let connection_id = parse_id(connection_id, MobileCoreError::ConnectionNotActive)?;
        self.lock_manager()?.disconnect(connection_id)
    }

    /// Open a terminal stream on a saved host and optional session.
    ///
    /// # Errors
    ///
    /// Returns host lookup, connection, and attach errors.
    pub fn open_terminal(&self, request: &TerminalOpenRequestFfi) -> Result<TerminalSessionState> {
        let request = map_terminal_open_request(request);
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

    /// Read a versioned native presentation with pointer metadata ordered after its bytes.
    ///
    /// # Errors
    /// Rejects unsupported versions before draining output; returns lookup/backend errors.
    pub fn poll_terminal_presentation(
        &self,
        terminal_id: &str,
        max_chunks: u32,
        version: u32,
    ) -> Result<bmux_mobile_core::pointer_output::TerminalPresentation> {
        bmux_mobile_core::pointer_output::validate_pointer_version(version)?;
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let max_chunks = usize::try_from(max_chunks)
            .map_err(|_| MobileCoreError::InvalidArgument("max_chunks out of range".to_string()))?;
        self.lock_manager()?
            .poll_terminal_presentation(terminal_id, max_chunks, version)
    }

    /// Read byte-only terminal chunks without pointer metadata.
    ///
    /// # Errors
    /// Returns UUID parsing, lookup, or backend errors.
    pub fn poll_terminal_output(
        &self,
        terminal_id: &str,
        max_chunks: u32,
    ) -> Result<Vec<TerminalChunk>> {
        let terminal_id = Uuid::parse_str(terminal_id)
            .map_err(|_| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        let max_chunks = usize::try_from(max_chunks)
            .map_err(|_| MobileCoreError::InvalidArgument("max_chunks out of range".to_string()))?;
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
            .map_err(|_| MobileCoreError::InvalidArgument("limit out of range".to_string()))?;
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

#[uniffi::export]
// UniFFI generated bindings pass owned values for Rust String/Record inputs.
#[allow(clippy::needless_pass_by_value)]
impl MobileApiFfi {
    /// The API for one app. `storage_dir` is an app-private directory that
    /// holds this device's key and saved hosts; every instance in one app
    /// passes the same directory.
    #[uniffi::constructor]
    #[must_use]
    pub fn new(storage_dir: String) -> Self {
        Self {
            inner: MobileApi::new(Path::new(&storage_dir)),
        }
    }

    /// List saved paired hosts.
    ///
    /// # Errors
    ///
    /// Returns mapped saved-host read errors.
    pub fn list_hosts(&self) -> std::result::Result<Vec<PairedHostFfi>, MobileFfiError> {
        self.inner
            .list_hosts()
            .map(|hosts| hosts.into_iter().map(map_paired_host).collect())
            .map_err(MobileFfiError::from)
    }

    /// Rename a saved host.
    ///
    /// # Errors
    ///
    /// Returns mapped host lookup and alias validation errors.
    pub fn rename_host(
        &self,
        alias: String,
        new_alias: String,
    ) -> std::result::Result<PairedHostFfi, MobileFfiError> {
        self.inner
            .rename_host(&alias, &new_alias)
            .map(map_paired_host)
            .map_err(MobileFfiError::from)
    }

    /// Forget a saved host on this device. The host keeps listing the device
    /// until its owner runs `bmux remote revoke`.
    ///
    /// # Errors
    ///
    /// Returns a mapped host lookup error.
    pub fn forget_host(&self, alias: String) -> std::result::Result<(), MobileFfiError> {
        self.inner.forget_host(&alias).map_err(MobileFfiError::from)
    }

    /// Start pairing through the `bmux-pair:` link `bmux remote pair` printed
    /// on the host. Poll [`Self::pairing_state`] for the phrase and outcome.
    ///
    /// # Errors
    ///
    /// Returns mapped link and alias validation errors.
    pub fn start_pairing(
        &self,
        link: String,
        alias: Option<String>,
        device_name: String,
    ) -> std::result::Result<PairingStateFfi, MobileFfiError> {
        self.inner
            .start_pairing(link, alias, device_name)
            .map(map_pairing_state)
            .map_err(MobileFfiError::from)
    }

    /// The progress of a pairing.
    ///
    /// # Errors
    ///
    /// Returns a mapped pairing lookup error.
    pub fn pairing_state(
        &self,
        pairing_id: String,
    ) -> std::result::Result<PairingStateFfi, MobileFfiError> {
        self.inner
            .pairing_state(&pairing_id)
            .map(map_pairing_state)
            .map_err(MobileFfiError::from)
    }

    /// Cancel a pairing in progress.
    ///
    /// # Errors
    ///
    /// Returns a mapped pairing lookup error.
    pub fn cancel_pairing(
        &self,
        pairing_id: String,
    ) -> std::result::Result<PairingStateFfi, MobileFfiError> {
        self.inner
            .cancel_pairing(&pairing_id)
            .map(map_pairing_state)
            .map_err(MobileFfiError::from)
    }

    /// Connect to a saved host: dial it and complete the handshake.
    ///
    /// # Errors
    ///
    /// Returns mapped host lookup and connection errors.
    pub fn connect(
        &self,
        host: String,
        session: Option<String>,
    ) -> std::result::Result<ConnectionStateFfi, MobileFfiError> {
        self.inner
            .connect(&host, session)
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

    /// Open a terminal stream on a saved host.
    ///
    /// # Errors
    ///
    /// Returns mapped host lookup, connection, and attach errors.
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

    /// Select native pointer output v1. Existing byte-only polling is unchanged.
    ///
    /// # Errors
    /// Returns explicit unsupported-version, lookup, or backend errors.
    pub fn poll_terminal_presentation(
        &self,
        terminal_id: String,
        max_chunks: u32,
        version: u32,
    ) -> std::result::Result<TerminalPresentationFfi, MobileFfiError> {
        self.inner
            .poll_terminal_presentation(&terminal_id, max_chunks, version)
            .map(|presentation| TerminalPresentationFfi {
                chunks: presentation
                    .chunks
                    .into_iter()
                    .map(map_terminal_chunk)
                    .collect(),
                pointer: presentation
                    .pointer
                    .map(|pointer| TerminalPointerOutputFfi {
                        version: pointer.version,
                        generation: pointer.generation.to_string(),
                        revision: pointer.revision,
                        shape: match pointer.shape {
                            bmux_mobile_core::pointer_output::TerminalPointerShape::Hidden => {
                                TerminalPointerShapeFfi::Hidden
                            }
                            bmux_mobile_core::pointer_output::TerminalPointerShape::Shape(
                                shape,
                            ) => TerminalPointerShapeFfi::Shape {
                                name: shape.as_str().to_string(),
                            },
                        },
                    }),
            })
            .map_err(MobileFfiError::from)
    }

    /// Read byte-only terminal chunks without pointer metadata.
    ///
    /// # Errors
    /// Returns UUID parsing, lookup, or backend errors.
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
}

pub type Result<T> = std::result::Result<T, MobileCoreError>;

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_mobile_core::ConnectionManager;

    #[test]
    fn native_presentation_rejects_unknown_version_before_terminal_lookup() {
        let root = tempfile::tempdir().expect("temporary storage root");
        let api = MobileApi::new(root.path());
        let error = api
            .poll_terminal_presentation("invalid-uuid", 8, 2)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported native pointer output version")
        );
    }

    #[test]
    fn terminal_mouse_leave_has_a_typed_mapping() {
        assert_eq!(
            map_terminal_mouse_event_kind(TerminalMouseEventKindFfi::Leave),
            TerminalMouseEventKind::Leave
        );
    }
    use bmux_mobile_core::remote_bridge::{BackendSessionHandle, TerminalBackend};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[derive(Default)]
    struct MockTerminalBackend {
        sessions: Mutex<BTreeMap<Uuid, Vec<u8>>>,
    }

    impl TerminalBackend for MockTerminalBackend {
        fn probe(&self, _host: &str) -> Result<()> {
            Ok(())
        }

        fn open(
            &self,
            _host: &str,
            _session: Option<String>,
            _remembered_session: Option<String>,
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
    fn ffi_pairing_rejects_malformed_links_and_unknown_ids() {
        let root = tempfile::tempdir().expect("temporary storage root");
        let api = MobileApiFfi::new(root.path().to_string_lossy().into_owned());
        assert!(api.list_hosts().expect("list saved hosts").is_empty());
        assert!(matches!(
            api.start_pairing("ssh://ops@prod:22".into(), None, "phone".into()),
            Err(MobileFfiError::InvalidPairingLink(_))
        ));
        assert!(matches!(
            api.pairing_state("not-a-uuid".into()),
            Err(MobileFfiError::PairingNotFound(_))
        ));
        assert!(matches!(
            api.cancel_pairing(Uuid::new_v4().to_string()),
            Err(MobileFfiError::PairingNotFound(_))
        ));
        assert!(matches!(
            api.connect("desktop".into(), None),
            Err(MobileFfiError::HostNotFound(_))
        ));
        assert!(matches!(
            api.forget_host("desktop".into()),
            Err(MobileFfiError::HostNotFound(_))
        ));
    }

    #[test]
    fn ffi_connect_reports_the_saved_host() {
        let root = tempfile::tempdir().expect("temporary storage root");
        let api = MobileApi::with_manager(ConnectionManager::with_terminal_backend(
            root.path(),
            Arc::new(MockTerminalBackend::default()),
        ));
        let state = api
            .connect("desktop", Some("main".to_string()))
            .expect("connection should work");
        assert_eq!(state.host, "desktop");
        assert_eq!(state.status, bmux_mobile_core::ConnectionStatus::Connected);
        let disconnected = api
            .disconnect(&state.id.to_string())
            .expect("disconnect should work");
        assert_eq!(
            disconnected.status,
            bmux_mobile_core::ConnectionStatus::Disconnected
        );
    }

    #[test]
    fn ffi_terminal_stream_round_trip() {
        let root = tempfile::tempdir().expect("temporary storage root");
        let api = MobileApi::with_manager(ConnectionManager::with_terminal_backend(
            root.path(),
            Arc::new(MockTerminalBackend::default()),
        ));
        let terminal = api
            .open_terminal(&TerminalOpenRequestFfi {
                host: "desktop".to_string(),
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
        assert_ne!(chunks, [] as [bmux_mobile_core::TerminalChunk; 0]);
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
