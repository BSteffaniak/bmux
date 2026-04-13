use crate::error::{MobileCoreError, Result};
use crate::ssh::{EmbeddedSshBackend, SshBackend, parse_ssh_target};
use crate::target::{
    CanonicalTarget, TargetInput, TargetRecord, TargetTransport, canonicalize_target,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use uuid::Uuid;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalChunk {
    pub sequence: u64,
    pub kind: TerminalChunkKind,
    pub bytes: Vec<u8>,
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
    chunks: VecDeque<TerminalChunk>,
    next_sequence: u64,
}

pub struct ConnectionManager {
    targets: BTreeMap<Uuid, TargetRecord>,
    connections: BTreeMap<Uuid, ConnectionState>,
    terminals: BTreeMap<Uuid, TerminalSessionRuntime>,
    ssh_backend: Option<Arc<dyn SshBackend>>,
}

impl Default for ConnectionManager {
    fn default() -> Self {
        Self {
            targets: BTreeMap::new(),
            connections: BTreeMap::new(),
            terminals: BTreeMap::new(),
            ssh_backend: Some(Arc::new(EmbeddedSshBackend::default())),
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
        }
    }

    #[must_use]
    pub fn without_ssh_backend() -> Self {
        Self {
            targets: BTreeMap::new(),
            connections: BTreeMap::new(),
            terminals: BTreeMap::new(),
            ssh_backend: None,
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
        let size = TerminalSize {
            rows: request.rows,
            cols: request.cols,
        };
        validate_terminal_size(size)?;

        let target = self
            .targets
            .get(&request.target_id)
            .ok_or_else(|| MobileCoreError::TargetNotFound(request.target_id.to_string()))?;
        let target_name = target.name.clone();
        let canonical_target = target.canonical_target.value.clone();

        let connection = self.connect(ConnectionRequest {
            target_id: request.target_id,
            session: request.session.clone(),
        })?;
        let connection = self.mark_connected(connection.id)?;

        let mut runtime = TerminalSessionRuntime {
            state: TerminalSessionState {
                id: Uuid::new_v4(),
                target_id: request.target_id,
                connection_id: connection.id,
                session: request.session,
                status: TerminalSessionStatus::Opening,
                size,
                last_sequence: 0,
            },
            chunks: VecDeque::new(),
            next_sequence: 1,
        };

        runtime.push_status_chunk(format!("connected to {target_name} ({canonical_target})"));
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
        let runtime = self
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;

        if max_chunks == 0 {
            return Ok(Vec::new());
        }

        let mut output = Vec::new();
        for _ in 0..max_chunks {
            match runtime.chunks.pop_front() {
                Some(chunk) => output.push(chunk),
                None => break,
            }
        }
        Ok(output)
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
            return Err(MobileCoreError::TerminalSessionClosed(
                terminal_id.to_string(),
            ));
        }
        if !bytes.is_empty() {
            runtime.push_chunk(TerminalChunkKind::Stdout, bytes.to_vec());
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
        validate_terminal_size(size)?;
        let runtime = self
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(terminal_id.to_string()))?;
        if runtime.state.status == TerminalSessionStatus::Closed {
            return Err(MobileCoreError::TerminalSessionClosed(
                terminal_id.to_string(),
            ));
        }
        runtime.state.size = size;
        runtime.push_status_chunk(format!("resize {}x{}", size.rows, size.cols));
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
            if runtime.state.status != TerminalSessionStatus::Closed {
                runtime.state.status = TerminalSessionStatus::Closed;
                runtime.push_status_chunk("session closed".to_string());
            }
            runtime.state.connection_id
        };
        let _ = self.disconnect(connection_id);
        self.terminal_state(terminal_id)
    }
}

impl TerminalSessionRuntime {
    fn push_chunk(&mut self, kind: TerminalChunkKind, bytes: Vec<u8>) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.state.last_sequence = sequence;
        self.chunks.push_back(TerminalChunk {
            sequence,
            kind,
            bytes,
        });
    }

    fn push_status_chunk(&mut self, message: String) {
        self.push_chunk(TerminalChunkKind::Status, message.into_bytes());
    }
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
    use crate::ssh::MockSshBackend;

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
        let mut manager = ConnectionManager::new();
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
    }

    #[test]
    fn terminal_open_rejects_zero_dimensions() {
        let mut manager = ConnectionManager::new();
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
}
