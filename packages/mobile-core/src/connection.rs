use crate::error::{MobileCoreError, Result};
use crate::remote_bridge::{BackendSessionHandle, RemoteTerminalBackend, TerminalBackend};
use crate::ssh::{EmbeddedSshBackend, SshBackend, parse_ssh_target};
use crate::target::{
    CanonicalTarget, TargetInput, TargetRecord, TargetTransport, canonicalize_target,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const TERMINAL_DIAGNOSTIC_MAX_EVENTS: usize = 512;
const MAX_PRE_OUTPUT_RECOVERY_ATTEMPTS: u8 = 1;
const PRE_OUTPUT_RECOVERY_BACKOFF_MS: u64 = 250;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionRequest {
    pub target_id: Uuid,
    pub session: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionState {
    pub id: Uuid,
    pub target_id: Uuid,
    pub status: ConnectionStatus,
    pub session: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalOpenRequest {
    pub target_id: Uuid,
    pub session: Option<String>,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSessionStatus {
    Opening,
    Open,
    Closed,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalChunkKind {
    Stdout,
    Stderr,
    Status,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatusSeverity {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalChunk {
    pub sequence: u64,
    pub kind: TerminalChunkKind,
    pub bytes: Vec<u8>,
    pub status_severity: Option<TerminalStatusSeverity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalDiagnostic {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub severity: TerminalStatusSeverity,
    pub stage: String,
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalSessionState {
    pub id: Uuid,
    pub target_id: Uuid,
    pub connection_id: Uuid,
    pub session: Option<String>,
    pub status: TerminalSessionStatus,
    pub size: TerminalSize,
    pub last_sequence: u64,
}

#[derive(Debug, Clone)]
struct TerminalSessionRuntime {
    state: TerminalSessionState,
    backend: BackendSessionHandle,
    chunks: VecDeque<TerminalChunk>,
    next_sequence: u64,
    diagnostics: VecDeque<TerminalDiagnostic>,
    next_diagnostic_sequence: u64,
    last_failure: Option<String>,
    first_output_seen: bool,
    pre_output_recovery_attempts: u8,
    opened_at_ms: u64,
}

pub struct ConnectionManager {
    targets: BTreeMap<Uuid, TargetRecord>,
    connections: BTreeMap<Uuid, ConnectionState>,
    terminals: BTreeMap<Uuid, TerminalSessionRuntime>,
    ssh_backend: Option<Arc<dyn SshBackend>>,
    terminal_backend: Arc<dyn TerminalBackend>,
}

impl Default for ConnectionManager {
    fn default() -> Self {
        Self {
            targets: BTreeMap::new(),
            connections: BTreeMap::new(),
            terminals: BTreeMap::new(),
            ssh_backend: Some(Arc::new(EmbeddedSshBackend::default())),
            terminal_backend: Arc::new(RemoteTerminalBackend::new()),
        }
    }
}

impl ConnectionManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_ssh_backend(ssh_backend: Arc<dyn SshBackend>) -> Self {
        Self {
            targets: BTreeMap::new(),
            connections: BTreeMap::new(),
            terminals: BTreeMap::new(),
            ssh_backend: Some(ssh_backend),
            terminal_backend: Arc::new(RemoteTerminalBackend::new()),
        }
    }

    #[must_use]
    pub fn with_terminal_backend(terminal_backend: Arc<dyn TerminalBackend>) -> Self {
        Self {
            targets: BTreeMap::new(),
            connections: BTreeMap::new(),
            terminals: BTreeMap::new(),
            ssh_backend: Some(Arc::new(EmbeddedSshBackend::default())),
            terminal_backend,
        }
    }

    #[must_use]
    pub fn without_ssh_backend() -> Self {
        Self {
            targets: BTreeMap::new(),
            connections: BTreeMap::new(),
            terminals: BTreeMap::new(),
            ssh_backend: None,
            terminal_backend: Arc::new(RemoteTerminalBackend::new()),
        }
    }

    /// Import a target into in-memory storage.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::InvalidTarget`] when the source cannot be normalized.
    pub fn import_target(&mut self, input: &TargetInput) -> Result<TargetRecord> {
        let canonical: CanonicalTarget = canonicalize_target(input)?;
        let id = Uuid::new_v4();
        let default_name = canonical.uri.value.clone();
        let name = input
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(default_name.as_str())
            .to_string();

        let record = TargetRecord {
            id,
            name,
            canonical_target: canonical.uri,
            transport: canonical.transport,
            default_session: None,
        };
        self.targets.insert(id, record.clone());
        Ok(record)
    }

    #[must_use]
    pub fn list_targets(&self) -> Vec<TargetRecord> {
        self.targets.values().cloned().collect()
    }

    /// Start a connection attempt for a known target.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::TargetNotFound`] when `target_id` is unknown,
    /// [`MobileCoreError::SshBackendUnavailable`] for SSH targets when no embedded
    /// backend is configured, and parsing/backend errors for invalid SSH targets.
    pub fn connect(&mut self, request: ConnectionRequest) -> Result<ConnectionState> {
        let target = self
            .targets
            .get(&request.target_id)
            .ok_or_else(|| MobileCoreError::TargetNotFound(request.target_id.to_string()))?;

        if target.transport == TargetTransport::Ssh {
            let parsed = parse_ssh_target(&target.canonical_target.value)?;
            let backend = self
                .ssh_backend
                .as_ref()
                .ok_or(MobileCoreError::SshBackendUnavailable)?;
            backend.open(&parsed)?;
        }

        let state = ConnectionState {
            id: Uuid::new_v4(),
            target_id: request.target_id,
            status: ConnectionStatus::Connecting,
            session: request.session,
            last_error: None,
        };
        self.connections.insert(state.id, state.clone());
        Ok(state)
    }

    /// Transition an active connection to `connected`.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::ConnectionNotActive`] when the connection id is unknown.
    pub fn mark_connected(&mut self, connection_id: Uuid) -> Result<ConnectionState> {
        let state = self
            .connections
            .get_mut(&connection_id)
            .ok_or_else(|| MobileCoreError::ConnectionNotActive(connection_id.to_string()))?;
        state.status = ConnectionStatus::Connected;
        Ok(state.clone())
    }

    /// Transition an active connection to `failed` and store an error message.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::ConnectionNotActive`] when the connection id is unknown.
    pub fn mark_failed(&mut self, connection_id: Uuid, message: &str) -> Result<ConnectionState> {
        let state = self
            .connections
            .get_mut(&connection_id)
            .ok_or_else(|| MobileCoreError::ConnectionNotActive(connection_id.to_string()))?;
        state.status = ConnectionStatus::Failed;
        state.last_error = Some(message.to_string());
        Ok(state.clone())
    }

    /// Transition an active connection to `disconnected`.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::ConnectionNotActive`] when the connection id is unknown.
    pub fn disconnect(&mut self, connection_id: Uuid) -> Result<ConnectionState> {
        let state = self
            .connections
            .get_mut(&connection_id)
            .ok_or_else(|| MobileCoreError::ConnectionNotActive(connection_id.to_string()))?;
        state.status = ConnectionStatus::Disconnected;
        Ok(state.clone())
    }

    /// Open a terminal stream for a target and optional named session.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::TargetNotFound`] for unknown targets,
    /// [`MobileCoreError::InvalidTerminalSize`] for invalid dimensions, and
    /// connection errors from [`Self::connect`].
    pub fn open_terminal(&mut self, request: TerminalOpenRequest) -> Result<TerminalSessionState> {
        let TerminalOpenRequest {
            target_id,
            session,
            rows,
            cols,
        } = request;
        let size = TerminalSize { rows, cols };
        validate_terminal_size(size)?;

        let requested_session = session
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);

        let target = self
            .targets
            .get(&target_id)
            .ok_or_else(|| MobileCoreError::TargetNotFound(target_id.to_string()))?
            .clone();
        let target_name = target.name.clone();
        let canonical_target = target.canonical_target.value.clone();

        let connection = self.connect(ConnectionRequest {
            target_id,
            session: requested_session.clone(),
        })?;
        let connection = self.mark_connected(connection.id)?;
        let backend =
            match self
                .terminal_backend
                .open(&target, requested_session, size.rows, size.cols)
            {
                Ok(backend) => backend,
                Err(error) => {
                    let _ = self.mark_failed(connection.id, &error.to_string());
                    return Err(error);
                }
            };

        let attached_session = Some(backend.session_id.to_string());
        if let Some(connection_state) = self.connections.get_mut(&connection.id) {
            connection_state.session.clone_from(&attached_session);
        }
        if let Some(target_state) = self.targets.get_mut(&target_id) {
            target_state.default_session.clone_from(&attached_session);
        }

        let mut runtime = TerminalSessionRuntime {
            state: TerminalSessionState {
                id: Uuid::new_v4(),
                target_id,
                connection_id: connection.id,
                session: attached_session,
                status: TerminalSessionStatus::Opening,
                size,
                last_sequence: 0,
            },
            backend,
            chunks: VecDeque::new(),
            next_sequence: 1,
            diagnostics: VecDeque::new(),
            next_diagnostic_sequence: 1,
            last_failure: None,
            first_output_seen: false,
            pre_output_recovery_attempts: 0,
            opened_at_ms: now_epoch_ms(),
        };

        runtime.push_diagnostic(
            "open",
            TerminalStatusSeverity::Info,
            Some("terminal_opened"),
            format!(
                "opened terminal session {} for target {}",
                runtime.state.id, target_name
            ),
        );
        runtime.push_status_chunk(
            format!("connected to {target_name} ({canonical_target})"),
            TerminalStatusSeverity::Info,
        );
        runtime.state.status = TerminalSessionStatus::Open;

        let state = runtime.state.clone();
        self.terminals.insert(state.id, runtime);
        Ok(state)
    }

    /// Return current terminal session state for a terminal id.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::TerminalSessionNotFound`] when unknown.
    pub fn terminal_state(&self, terminal_id: Uuid) -> Result<TerminalSessionState> {
        self.terminals
            .get(&terminal_id)
            .map(|runtime| runtime.state.clone())
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))
    }

    /// Return diagnostics for a terminal stream.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::TerminalSessionNotFound`] when unknown.
    pub fn terminal_diagnostics(
        &self,
        terminal_id: Uuid,
        since_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<TerminalDiagnostic>> {
        let runtime = self
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;

        let filtered = runtime
            .diagnostics
            .iter()
            .filter(|event| since_sequence.is_none_or(|since| event.sequence > since));
        let capped_limit = if limit == 0 { 128 } else { limit.min(512) };
        Ok(filtered.take(capped_limit).cloned().collect())
    }

    /// Return latest terminal failure message, if any.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::TerminalSessionNotFound`] when unknown.
    pub fn latest_terminal_failure(&self, terminal_id: Uuid) -> Result<Option<String>> {
        let runtime = self
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        Ok(runtime.last_failure.clone())
    }

    /// Read pending output chunks for a terminal stream.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::TerminalSessionNotFound`] when unknown.
    pub fn poll_terminal_output(
        &mut self,
        terminal_id: Uuid,
        max_chunks: usize,
    ) -> Result<Vec<TerminalChunk>> {
        if max_chunks == 0 {
            return Ok(Vec::new());
        }

        let mut output = Vec::new();
        {
            let runtime = self
                .terminals
                .get_mut(&terminal_id)
                .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;

            for _ in 0..max_chunks {
                match runtime.chunks.pop_front() {
                    Some(chunk) => output.push(chunk),
                    None => break,
                }
            }
            if output.len() >= max_chunks || runtime.state.status == TerminalSessionStatus::Closed {
                return Ok(output);
            }
        }

        let max_bytes = max_chunks.saturating_mul(4096).max(1);
        loop {
            let backend_id = self
                .terminals
                .get(&terminal_id)
                .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?
                .backend
                .id;
            let data = match self.terminal_backend.poll_output(backend_id, max_bytes) {
                Ok(data) => data,
                Err(error) => {
                    if self.try_recover_pre_output_terminal_stream_drop(terminal_id, &error) {
                        continue;
                    }

                    let runtime = self.terminals.get_mut(&terminal_id).ok_or_else(|| {
                        MobileCoreError::TerminalSessionNotFound(terminal_id.to_string())
                    })?;
                    runtime.state.status = TerminalSessionStatus::Failed;
                    let detail = if runtime.first_output_seen {
                        error.to_string()
                    } else {
                        format!(
                            "stream ended before first output after {}ms: {error}",
                            runtime.open_elapsed_ms()
                        )
                    };
                    runtime.push_diagnostic(
                        "poll",
                        TerminalStatusSeverity::Error,
                        Some("backend_poll_failed"),
                        detail.clone(),
                    );
                    runtime.push_status_chunk(
                        format!("poll failed: {detail}"),
                        TerminalStatusSeverity::Error,
                    );
                    if !output.is_empty() {
                        return Ok(output);
                    }
                    return Err(error);
                }
            };

            if !data.is_empty() {
                let runtime = self.terminals.get_mut(&terminal_id).ok_or_else(|| {
                    MobileCoreError::TerminalSessionNotFound(terminal_id.to_string())
                })?;
                if !runtime.first_output_seen {
                    runtime.first_output_seen = true;
                    runtime.push_diagnostic(
                        "output",
                        TerminalStatusSeverity::Info,
                        Some("first_output"),
                        "received first output bytes",
                    );
                }
                runtime.push_chunk(TerminalChunkKind::Stdout, data, None);
                while output.len() < max_chunks {
                    match runtime.chunks.pop_front() {
                        Some(chunk) => output.push(chunk),
                        None => break,
                    }
                }
            }

            return Ok(output);
        }
    }

    /// Write input bytes into a terminal stream.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::TerminalSessionNotFound`] when unknown and
    /// [`MobileCoreError::TerminalSessionClosed`] when the stream is closed.
    pub fn write_terminal_input(&mut self, terminal_id: Uuid, bytes: &[u8]) -> Result<()> {
        let runtime = self
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        if runtime.state.status == TerminalSessionStatus::Closed {
            runtime.push_status_chunk(
                "write rejected: session is closed".to_string(),
                TerminalStatusSeverity::Error,
            );
            return Err(MobileCoreError::TerminalSessionClosed(
                terminal_id.to_string(),
            ));
        }
        if !bytes.is_empty() {
            self.terminal_backend
                .write_input(runtime.backend.id, bytes)
                .map_err(|error| {
                    runtime.state.status = TerminalSessionStatus::Failed;
                    runtime.push_diagnostic(
                        "input",
                        TerminalStatusSeverity::Error,
                        Some("backend_write_failed"),
                        error.to_string(),
                    );
                    runtime.push_status_chunk(
                        format!("write failed: {error}"),
                        TerminalStatusSeverity::Error,
                    );
                    error
                })?;
        }
        Ok(())
    }

    /// Resize a terminal stream.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::InvalidTerminalSize`] for invalid dimensions,
    /// [`MobileCoreError::TerminalSessionNotFound`] when unknown, and
    /// [`MobileCoreError::TerminalSessionClosed`] when the stream is closed.
    pub fn resize_terminal(&mut self, terminal_id: Uuid, size: TerminalSize) -> Result<()> {
        let runtime = self
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        if runtime.state.status == TerminalSessionStatus::Closed {
            runtime.push_status_chunk(
                "resize rejected: session is closed".to_string(),
                TerminalStatusSeverity::Error,
            );
            return Err(MobileCoreError::TerminalSessionClosed(
                terminal_id.to_string(),
            ));
        }
        if let Err(error) = validate_terminal_size(size) {
            runtime.push_status_chunk(
                format!("resize rejected: invalid size {}x{}", size.rows, size.cols),
                TerminalStatusSeverity::Error,
            );
            return Err(error);
        }
        self.terminal_backend
            .resize(runtime.backend.id, size.rows, size.cols)
            .map_err(|error| {
                runtime.state.status = TerminalSessionStatus::Failed;
                runtime.push_diagnostic(
                    "resize",
                    TerminalStatusSeverity::Error,
                    Some("backend_resize_failed"),
                    error.to_string(),
                );
                runtime.push_status_chunk(
                    format!("resize failed: {error}"),
                    TerminalStatusSeverity::Error,
                );
                error
            })?;
        runtime.state.size = size;
        runtime.push_status_chunk(
            format!("resize {}x{}", size.rows, size.cols),
            TerminalStatusSeverity::Info,
        );
        Ok(())
    }

    /// Close a terminal stream and mark backing connection disconnected.
    ///
    /// # Errors
    ///
    /// Returns [`MobileCoreError::TerminalSessionNotFound`] when unknown.
    pub fn close_terminal(&mut self, terminal_id: Uuid) -> Result<TerminalSessionState> {
        let connection_id = {
            let runtime = self
                .terminals
                .get_mut(&terminal_id)
                .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
            if runtime.state.status == TerminalSessionStatus::Closed {
                runtime.push_diagnostic(
                    "close",
                    TerminalStatusSeverity::Warn,
                    Some("already_closed"),
                    "session close requested after close",
                );
                runtime.push_status_chunk(
                    "session already closed".to_string(),
                    TerminalStatusSeverity::Warn,
                );
            } else {
                let _ = self
                    .terminal_backend
                    .close(runtime.backend.id)
                    .map_err(|error| {
                        runtime.push_diagnostic(
                            "close",
                            TerminalStatusSeverity::Error,
                            Some("backend_close_failed"),
                            error.to_string(),
                        );
                        runtime.push_status_chunk(
                            format!("close failed: {error}"),
                            TerminalStatusSeverity::Error,
                        );
                        error
                    });
                runtime.state.status = TerminalSessionStatus::Closed;
                runtime.push_diagnostic(
                    "close",
                    TerminalStatusSeverity::Info,
                    Some("terminal_closed"),
                    "session closed",
                );
                runtime
                    .push_status_chunk("session closed".to_string(), TerminalStatusSeverity::Info);
            }
            runtime.state.connection_id
        };
        let _ = self.disconnect(connection_id);
        self.terminal_state(terminal_id)
    }

    fn try_recover_pre_output_terminal_stream_drop(
        &mut self,
        terminal_id: Uuid,
        error: &MobileCoreError,
    ) -> bool {
        let Some(recovery) = self.pre_output_recovery_context(terminal_id, error) else {
            return false;
        };

        {
            let Some(runtime) = self.terminals.get_mut(&terminal_id) else {
                return false;
            };
            runtime.pre_output_recovery_attempts = recovery.attempt;
            runtime.push_diagnostic(
                "recover",
                TerminalStatusSeverity::Warn,
                Some("early_stream_drop_retry"),
                format!(
                    "stream closed before first output after {}ms; reconnect attempt {}/{}",
                    runtime.open_elapsed_ms(),
                    recovery.attempt,
                    MAX_PRE_OUTPUT_RECOVERY_ATTEMPTS
                ),
            );
            runtime.push_status_chunk(
                format!(
                    "stream closed before first output; retrying connection ({}/{})",
                    recovery.attempt, MAX_PRE_OUTPUT_RECOVERY_ATTEMPTS
                ),
                TerminalStatusSeverity::Warn,
            );
        }

        std::thread::sleep(std::time::Duration::from_millis(
            PRE_OUTPUT_RECOVERY_BACKOFF_MS,
        ));

        let _ = self.terminal_backend.close(recovery.old_backend_id);
        let Some(target) = self.targets.get(&recovery.target_id).cloned() else {
            let Some(runtime) = self.terminals.get_mut(&terminal_id) else {
                return false;
            };
            runtime.push_diagnostic(
                "recover",
                TerminalStatusSeverity::Error,
                Some("recovery_target_missing"),
                format!(
                    "cannot retry connection: target {} no longer exists",
                    recovery.target_id
                ),
            );
            return false;
        };

        let reopened = self.terminal_backend.open(
            &target,
            recovery.session.clone(),
            recovery.size.rows,
            recovery.size.cols,
        );

        let Some(runtime) = self.terminals.get_mut(&terminal_id) else {
            return false;
        };

        match reopened {
            Ok(backend) => {
                runtime.backend = backend;
                runtime.state.session = Some(runtime.backend.session_id.to_string());
                runtime.state.status = TerminalSessionStatus::Open;
                runtime.opened_at_ms = now_epoch_ms();
                runtime.push_diagnostic(
                    "recover",
                    TerminalStatusSeverity::Info,
                    Some("recovery_succeeded"),
                    format!(
                        "reconnected terminal stream on attempt {}/{}",
                        recovery.attempt, MAX_PRE_OUTPUT_RECOVERY_ATTEMPTS
                    ),
                );
                runtime.push_status_chunk(
                    format!(
                        "recovered terminal stream ({}/{})",
                        recovery.attempt, MAX_PRE_OUTPUT_RECOVERY_ATTEMPTS
                    ),
                    TerminalStatusSeverity::Info,
                );
                true
            }
            Err(reopen_error) => {
                runtime.push_diagnostic(
                    "recover",
                    TerminalStatusSeverity::Error,
                    Some("recovery_failed"),
                    format!(
                        "reconnect attempt {}/{} failed: {reopen_error}",
                        recovery.attempt, MAX_PRE_OUTPUT_RECOVERY_ATTEMPTS
                    ),
                );
                false
            }
        }
    }

    fn pre_output_recovery_context(
        &self,
        terminal_id: Uuid,
        error: &MobileCoreError,
    ) -> Option<PreOutputRecoveryContext> {
        let runtime = self.terminals.get(&terminal_id)?;
        if runtime.first_output_seen
            || runtime.pre_output_recovery_attempts >= MAX_PRE_OUTPUT_RECOVERY_ATTEMPTS
            || !is_early_stream_drop_error(error)
        {
            return None;
        }
        Some(PreOutputRecoveryContext {
            old_backend_id: runtime.backend.id,
            target_id: runtime.state.target_id,
            session: runtime.state.session.clone(),
            size: runtime.state.size,
            attempt: runtime.pre_output_recovery_attempts.saturating_add(1),
        })
    }
}

impl TerminalSessionRuntime {
    fn push_chunk(
        &mut self,
        kind: TerminalChunkKind,
        bytes: Vec<u8>,
        status_severity: Option<TerminalStatusSeverity>,
    ) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.state.last_sequence = sequence;
        self.chunks.push_back(TerminalChunk {
            sequence,
            kind,
            bytes,
            status_severity,
        });
    }

    fn push_status_chunk(&mut self, message: String, severity: TerminalStatusSeverity) {
        self.push_chunk(
            TerminalChunkKind::Status,
            message.into_bytes(),
            Some(severity),
        );
    }

    fn push_diagnostic(
        &mut self,
        stage: &str,
        severity: TerminalStatusSeverity,
        code: Option<&str>,
        message: impl Into<String>,
    ) {
        let message = message.into();
        let sequence = self.next_diagnostic_sequence;
        self.next_diagnostic_sequence = self.next_diagnostic_sequence.saturating_add(1);
        let event = TerminalDiagnostic {
            sequence,
            timestamp_ms: now_epoch_ms(),
            severity,
            stage: stage.to_string(),
            code: code.map(ToString::to_string),
            message: message.clone(),
        };
        self.diagnostics.push_back(event);
        while self.diagnostics.len() > TERMINAL_DIAGNOSTIC_MAX_EVENTS {
            let _ = self.diagnostics.pop_front();
        }
        if severity == TerminalStatusSeverity::Error {
            self.last_failure = Some(format!("{stage}: {message}"));
        }
    }

    fn open_elapsed_ms(&self) -> u64 {
        now_epoch_ms().saturating_sub(self.opened_at_ms)
    }
}

#[derive(Debug, Clone)]
struct PreOutputRecoveryContext {
    old_backend_id: Uuid,
    target_id: Uuid,
    session: Option<String>,
    size: TerminalSize,
    attempt: u8,
}

fn is_early_stream_drop_error(error: &MobileCoreError) -> bool {
    let MobileCoreError::TerminalBackendFailure(message) = error else {
        return false;
    };
    let lowered = message.to_ascii_lowercase();
    lowered.contains("terminal session terminated")
        || lowered.contains("stream closed")
        || lowered.contains("unexpected eof")
        || lowered.contains("broken pipe")
}

fn now_epoch_ms() -> u64 {
    let now = SystemTime::now();
    now.duration_since(UNIX_EPOCH).map_or(0, |duration| {
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
    })
}

const fn validate_terminal_size(size: TerminalSize) -> Result<()> {
    if size.rows == 0 || size.cols == 0 {
        return Err(MobileCoreError::InvalidTerminalSize {
            rows: size.rows,
            cols: size.cols,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_bridge::{BackendSessionHandle, TerminalBackend};
    use crate::ssh::MockSshBackend;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockTerminalBackend {
        sessions: Mutex<BTreeMap<Uuid, Vec<u8>>>,
    }

    struct RecoveringTerminalBackend {
        sessions: Mutex<BTreeMap<Uuid, Vec<u8>>>,
        fail_next_poll: Mutex<bool>,
    }

    impl Default for RecoveringTerminalBackend {
        fn default() -> Self {
            Self {
                sessions: Mutex::new(BTreeMap::new()),
                fail_next_poll: Mutex::new(true),
            }
        }
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

        fn resize(&self, handle_id: Uuid, _rows: u16, _cols: u16) -> Result<()> {
            let sessions = self.sessions.lock().expect("mock terminal sessions lock");
            if sessions.contains_key(&handle_id) {
                Ok(())
            } else {
                Err(MobileCoreError::TerminalSessionNotFound(
                    handle_id.to_string(),
                ))
            }
        }

        fn close(&self, handle_id: Uuid) -> Result<()> {
            let mut sessions = self.sessions.lock().expect("mock terminal sessions lock");
            if sessions.remove(&handle_id).is_some() {
                Ok(())
            } else {
                Err(MobileCoreError::TerminalSessionNotFound(
                    handle_id.to_string(),
                ))
            }
        }
    }

    impl TerminalBackend for RecoveringTerminalBackend {
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
                .expect("recovering terminal sessions lock")
                .insert(id, Vec::new());
            Ok(BackendSessionHandle {
                id,
                session_id: Uuid::new_v4(),
                can_write: true,
            })
        }

        fn poll_output(&self, handle_id: Uuid, _max_bytes: usize) -> Result<Vec<u8>> {
            let mut fail_next = self.fail_next_poll.lock().expect("recovering poll lock");
            if *fail_next {
                *fail_next = false;
                return Err(MobileCoreError::TerminalBackendFailure(
                    "terminal session terminated: stream closed with unexpected EOF".to_string(),
                ));
            }
            drop(fail_next);

            let output = {
                let mut sessions = self
                    .sessions
                    .lock()
                    .expect("recovering terminal sessions lock");
                std::mem::take(sessions.get_mut(&handle_id).ok_or_else(|| {
                    MobileCoreError::TerminalSessionNotFound(handle_id.to_string())
                })?)
            };
            Ok(output)
        }

        fn write_input(&self, handle_id: Uuid, bytes: &[u8]) -> Result<()> {
            {
                let mut sessions = self
                    .sessions
                    .lock()
                    .expect("recovering terminal sessions lock");
                sessions
                    .get_mut(&handle_id)
                    .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(handle_id.to_string()))?
                    .extend_from_slice(bytes);
            }
            Ok(())
        }

        fn resize(&self, handle_id: Uuid, _rows: u16, _cols: u16) -> Result<()> {
            let sessions = self
                .sessions
                .lock()
                .expect("recovering terminal sessions lock");
            if sessions.contains_key(&handle_id) {
                Ok(())
            } else {
                Err(MobileCoreError::TerminalSessionNotFound(
                    handle_id.to_string(),
                ))
            }
        }

        fn close(&self, handle_id: Uuid) -> Result<()> {
            let mut sessions = self
                .sessions
                .lock()
                .expect("recovering terminal sessions lock");
            if sessions.remove(&handle_id).is_some() {
                Ok(())
            } else {
                Err(MobileCoreError::TerminalSessionNotFound(
                    handle_id.to_string(),
                ))
            }
        }
    }

    #[test]
    fn import_and_connect_round_trip() {
        let mut manager = ConnectionManager::new();
        let target = manager
            .import_target(&TargetInput {
                source: "iroh://endpoint-123".to_string(),
                display_name: Some("prod-host".to_string()),
            })
            .expect("target import should work");

        let connection = manager
            .connect(ConnectionRequest {
                target_id: target.id,
                session: Some("main".to_string()),
            })
            .expect("connection should start");

        assert_eq!(connection.status, ConnectionStatus::Connecting);

        let connected = manager
            .mark_connected(connection.id)
            .expect("connection should transition to connected");
        assert_eq!(connected.status, ConnectionStatus::Connected);
    }

    #[test]
    fn connect_requires_target() {
        let mut manager = ConnectionManager::new();
        let result = manager.connect(ConnectionRequest {
            target_id: Uuid::new_v4(),
            session: None,
        });

        assert!(matches!(result, Err(MobileCoreError::TargetNotFound(_))));
    }

    #[test]
    fn connect_ssh_requires_backend() {
        let mut manager = ConnectionManager::without_ssh_backend();
        let target = manager
            .import_target(&TargetInput {
                source: "ssh://ops@prod.example.com:22".to_string(),
                display_name: None,
            })
            .expect("target import should work");

        let result = manager.connect(ConnectionRequest {
            target_id: target.id,
            session: None,
        });
        assert!(matches!(
            result,
            Err(MobileCoreError::SshBackendUnavailable)
        ));
    }

    #[test]
    fn connect_ssh_with_embedded_backend() {
        let mut manager = ConnectionManager::with_ssh_backend(Arc::new(MockSshBackend));
        let target = manager
            .import_target(&TargetInput {
                source: "ops@prod.example.com:2222".to_string(),
                display_name: Some("prod-ssh".to_string()),
            })
            .expect("target import should work");

        let connection = manager
            .connect(ConnectionRequest {
                target_id: target.id,
                session: Some("main".to_string()),
            })
            .expect("ssh connection should start");

        assert_eq!(connection.status, ConnectionStatus::Connecting);
    }

    #[test]
    fn terminal_open_write_poll_resize_close_round_trip() {
        let mut manager =
            ConnectionManager::with_terminal_backend(Arc::new(MockTerminalBackend::default()));
        let target = manager
            .import_target(&TargetInput {
                source: "iroh://endpoint-abc".to_string(),
                display_name: Some("demo".to_string()),
            })
            .expect("target import should work");

        let terminal = manager
            .open_terminal(TerminalOpenRequest {
                target_id: target.id,
                session: Some("main".to_string()),
                rows: 24,
                cols: 80,
            })
            .expect("terminal should open");
        assert_eq!(terminal.status, TerminalSessionStatus::Open);

        manager
            .write_terminal_input(terminal.id, b"ls\n")
            .expect("terminal write should work");
        manager
            .resize_terminal(
                terminal.id,
                TerminalSize {
                    rows: 40,
                    cols: 120,
                },
            )
            .expect("terminal resize should work");

        let output = manager
            .poll_terminal_output(terminal.id, 16)
            .expect("terminal poll should work");
        assert!(
            output
                .iter()
                .any(|chunk| chunk.kind == TerminalChunkKind::Status)
        );
        assert!(
            output
                .iter()
                .any(|chunk| chunk.status_severity == Some(TerminalStatusSeverity::Info))
        );
        assert!(
            output
                .iter()
                .any(|chunk| chunk.kind == TerminalChunkKind::Stdout && chunk.bytes == b"ls\n")
        );

        let closed = manager
            .close_terminal(terminal.id)
            .expect("terminal close should work");
        assert_eq!(closed.status, TerminalSessionStatus::Closed);

        let write_after_close = manager.write_terminal_input(terminal.id, b"pwd\n");
        assert!(matches!(
            write_after_close,
            Err(MobileCoreError::TerminalSessionClosed(_))
        ));

        let second_close = manager
            .close_terminal(terminal.id)
            .expect("second close should return state");
        assert_eq!(second_close.status, TerminalSessionStatus::Closed);

        let second_close_output = manager
            .poll_terminal_output(terminal.id, 16)
            .expect("post-second-close poll should work");
        assert!(second_close_output.iter().any(|chunk| {
            chunk.status_severity == Some(TerminalStatusSeverity::Warn)
                && chunk.kind == TerminalChunkKind::Status
        }));
    }

    #[test]
    fn terminal_open_rejects_zero_dimensions() {
        let mut manager =
            ConnectionManager::with_terminal_backend(Arc::new(MockTerminalBackend::default()));
        let target = manager
            .import_target(&TargetInput {
                source: "iroh://endpoint-xyz".to_string(),
                display_name: None,
            })
            .expect("target import should work");

        let result = manager.open_terminal(TerminalOpenRequest {
            target_id: target.id,
            session: None,
            rows: 0,
            cols: 80,
        });
        assert!(matches!(
            result,
            Err(MobileCoreError::InvalidTerminalSize { .. })
        ));
    }

    #[test]
    fn poll_recovers_from_early_stream_drop_before_first_output() {
        let mut manager = ConnectionManager::with_terminal_backend(Arc::new(
            RecoveringTerminalBackend::default(),
        ));
        let target = manager
            .import_target(&TargetInput {
                source: "iroh://endpoint-recover".to_string(),
                display_name: Some("recover".to_string()),
            })
            .expect("target import should work");

        let terminal = manager
            .open_terminal(TerminalOpenRequest {
                target_id: target.id,
                session: Some("main".to_string()),
                rows: 24,
                cols: 80,
            })
            .expect("terminal should open");

        let output = manager
            .poll_terminal_output(terminal.id, 64)
            .expect("poll should recover instead of failing");
        assert!(!output.is_empty());

        let diagnostics = manager
            .terminal_diagnostics(terminal.id, None, 256)
            .expect("diagnostics should load");
        assert!(
            diagnostics
                .iter()
                .any(|event| event.code.as_deref() == Some("early_stream_drop_retry"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|event| event.code.as_deref() == Some("recovery_succeeded"))
        );
    }
}
