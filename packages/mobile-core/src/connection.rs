use crate::error::{MobileCoreError, Result};
use crate::ssh::{SshBackend, parse_ssh_target};
use crate::target::{
    CanonicalTarget, TargetInput, TargetRecord, TargetTransport, canonicalize_target,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

#[derive(Default)]
pub struct ConnectionManager {
    targets: BTreeMap<Uuid, TargetRecord>,
    connections: BTreeMap<Uuid, ConnectionState>,
    ssh_backend: Option<Arc<dyn SshBackend>>,
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
            ssh_backend: Some(ssh_backend),
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
        let mut manager = ConnectionManager::new();
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
}
