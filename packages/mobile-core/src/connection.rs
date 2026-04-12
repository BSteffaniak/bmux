use crate::error::{MobileCoreError, Result};
use crate::target::{CanonicalTarget, TargetInput, TargetRecord, canonicalize_target};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

#[derive(Debug, Default)]
pub struct ConnectionManager {
    targets: BTreeMap<Uuid, TargetRecord>,
    connections: BTreeMap<Uuid, ConnectionState>,
}

impl ConnectionManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    /// Returns [`MobileCoreError::TargetNotFound`] when `target_id` is unknown.
    pub fn connect(&mut self, request: ConnectionRequest) -> Result<ConnectionState> {
        if !self.targets.contains_key(&request.target_id) {
            return Err(MobileCoreError::TargetNotFound(
                request.target_id.to_string(),
            ));
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
}
