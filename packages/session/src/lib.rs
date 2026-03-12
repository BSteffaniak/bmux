#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Session management for bmux terminal multiplexer
//!
//! This package provides session management functionality including session
//! creation, client handling, and state management.

// Re-export models for easy access
pub use bmux_session_models as models;

// Re-export commonly used types
pub use models::{
    ClientError, ClientId, ClientInfo, LayoutError, PaneError, PaneId, Session, SessionError,
    SessionId, SessionInfo,
};

use anyhow::Result;
use std::collections::BTreeMap;
use tracing::warn;

/// Session manager responsible for handling multiple sessions
#[derive(Debug, Default)]
pub struct SessionManager {
    sessions: BTreeMap<SessionId, Session>,
}

impl SessionManager {
    /// Create a new session manager
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
        }
    }

    /// Create a new session with an optional name
    ///
    /// # Errors
    ///
    /// * Session creation fails
    pub fn create_session(&mut self, name: Option<String>) -> Result<SessionId> {
        let session = Session::new(name);
        let id = session.id;
        self.sessions.insert(id, session);
        Ok(id)
    }

    /// Insert a preconstructed session (used by restore paths).
    ///
    /// # Errors
    ///
    /// Returns an error when a session with the same id already exists.
    pub fn insert_session(&mut self, session: Session) -> Result<()> {
        let id = session.id;
        if self.sessions.contains_key(&id) {
            return Err(anyhow::anyhow!("Session already exists: {id}"));
        }
        self.sessions.insert(id, session);
        Ok(())
    }

    /// Get a reference to a session by ID
    #[must_use]
    pub fn get_session(&self, session_id: &SessionId) -> Option<&Session> {
        self.sessions.get(session_id)
    }

    /// Get a mutable reference to a session by ID
    pub fn get_session_mut(&mut self, session_id: &SessionId) -> Option<&mut Session> {
        self.sessions.get_mut(session_id)
    }

    /// List all active sessions
    #[must_use]
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.values().map(Into::into).collect()
    }

    /// Remove a session by ID
    ///
    /// # Errors
    ///
    /// * Session not found
    pub fn remove_session(&mut self, session_id: &SessionId) -> Result<()> {
        if self.sessions.remove(session_id).is_some() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("Session not found: {}", session_id))
        }
    }

    /// Get the number of active sessions
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}
