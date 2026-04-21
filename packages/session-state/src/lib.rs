//! Neutral primitive crate for the sessions-plugin domain.
//!
//! Hosts the reader/writer trait abstractions, a handle newtype used
//! for registry lookup, and a `DefaultNoOp` fallback impl.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use anyhow::Result;
use bmux_session_models::{ClientId, Session, SessionId, SessionInfo};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Snapshot of session-manager state suitable for persistence.
///
/// Wraps a `Vec<Session>` for symmetry with `FollowStateSnapshot` and
/// `ContextStateSnapshot`. On restore, every session is inserted via
/// [`SessionManagerWriter::insert_session`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionManagerSnapshot(pub Vec<Session>);

impl SessionManagerSnapshot {
    #[must_use]
    pub fn new(sessions: Vec<Session>) -> Self {
        Self(sessions)
    }

    #[must_use]
    pub fn into_inner(self) -> Vec<Session> {
        self.0
    }
}

/// Read-only view over session-manager state.
pub trait SessionManagerReader: Send + Sync {
    /// List every session.
    fn list_sessions(&self) -> Vec<SessionInfo>;
    /// Clone-look-up a session by id.
    fn get_session(&self, session_id: SessionId) -> Option<Session>;
    /// Whether a session with the given id exists.
    fn contains(&self, session_id: SessionId) -> bool;
}

/// Mutation surface over session-manager state.
pub trait SessionManagerWriter: SessionManagerReader {
    /// Create a new session with an optional name.
    ///
    /// # Errors
    ///
    /// Returns an error if creation fails (e.g., future capacity limits).
    fn create_session(&self, name: Option<String>) -> Result<SessionId>;
    /// Insert a preconstructed session (used by restore paths).
    ///
    /// # Errors
    ///
    /// Returns an error if a session with the same id already exists.
    fn insert_session(&self, session: Session) -> Result<()>;
    /// Remove a session by id.
    ///
    /// # Errors
    ///
    /// Returns an error if no session has the given id.
    fn remove_session(&self, session_id: SessionId) -> Result<()>;
    /// Add a client to a session's client set.
    fn add_client(&self, session_id: SessionId, client_id: ClientId);
    /// Remove a client from a session's client set.
    fn remove_client(&self, session_id: SessionId, client_id: &ClientId);
    /// Capture a full snapshot for persistence.
    fn snapshot(&self) -> SessionManagerSnapshot;
    /// Replace state with a previously-captured snapshot.
    fn restore_snapshot(&self, snapshot: SessionManagerSnapshot);
}

/// Registry newtype wrapping an `Arc<dyn SessionManagerWriter>`.
#[derive(Clone)]
pub struct SessionManagerHandle(pub Arc<dyn SessionManagerWriter>);

impl SessionManagerHandle {
    #[must_use]
    pub fn new<W: SessionManagerWriter + 'static>(writer: W) -> Self {
        Self(Arc::new(writer))
    }

    #[must_use]
    pub fn from_arc(writer: Arc<dyn SessionManagerWriter>) -> Self {
        Self(writer)
    }

    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopSessionManager)
    }
}

/// No-op default impl. Registered by server at startup; plugin
/// overwrites during `activate`.
#[derive(Debug, Default)]
pub struct NoopSessionManager;

impl SessionManagerReader for NoopSessionManager {
    fn list_sessions(&self) -> Vec<SessionInfo> {
        Vec::new()
    }
    fn get_session(&self, _session_id: SessionId) -> Option<Session> {
        None
    }
    fn contains(&self, _session_id: SessionId) -> bool {
        false
    }
}

impl SessionManagerWriter for NoopSessionManager {
    fn create_session(&self, _name: Option<String>) -> Result<SessionId> {
        Err(anyhow::anyhow!("sessions plugin not active"))
    }
    fn insert_session(&self, _session: Session) -> Result<()> {
        Err(anyhow::anyhow!("sessions plugin not active"))
    }
    fn remove_session(&self, _session_id: SessionId) -> Result<()> {
        Err(anyhow::anyhow!("sessions plugin not active"))
    }
    fn add_client(&self, _session_id: SessionId, _client_id: ClientId) {}
    fn remove_client(&self, _session_id: SessionId, _client_id: &ClientId) {}
    fn snapshot(&self) -> SessionManagerSnapshot {
        SessionManagerSnapshot::default()
    }
    fn restore_snapshot(&self, _snapshot: SessionManagerSnapshot) {}
}
