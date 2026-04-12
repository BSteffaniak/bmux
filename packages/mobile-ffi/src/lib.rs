#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! FFI-facing facade for bmux mobile-core.

use bmux_mobile_core::{
    ConnectionManager, ConnectionRequest, ConnectionState, MobileCoreError, TargetInput,
    TargetRecord, observe_ssh_host_key_fingerprint_sha256,
};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct MobileApi {
    manager: Arc<Mutex<ConnectionManager>>,
}

impl MobileApi {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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

    /// Observe and return server SSH host-key SHA-256 fingerprint.
    ///
    /// # Errors
    ///
    /// Returns parse, DNS/network, handshake, or fingerprint availability
    /// errors.
    pub fn observe_ssh_host_key_fingerprint_sha256(&self, target: &str) -> Result<String> {
        observe_ssh_host_key_fingerprint_sha256(target)
    }

    fn lock_manager(&self) -> Result<std::sync::MutexGuard<'_, ConnectionManager>> {
        self.manager.lock().map_err(|_| {
            MobileCoreError::ConnectionNotActive("mobile api manager poisoned".to_string())
        })
    }
}

pub type Result<T> = std::result::Result<T, MobileCoreError>;

#[cfg(test)]
mod tests {
    use super::*;

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
}
