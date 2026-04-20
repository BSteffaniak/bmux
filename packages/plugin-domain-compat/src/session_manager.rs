//! Session manager, owned by the sessions plugin.
//!
//! `SessionManager` tracks every session known to the host: its
//! identity, name, and client set. During M4 it lives in
//! `bmux_plugin_domain_compat` so both core and plugins can name it;
//! the runtime handle is registered into
//! [`bmux_plugin::PluginStateRegistry`] by the sessions plugin, and
//! core server code accesses it via `global_plugin_state_registry`.
//!
//! The heavier `SessionRuntimeManager` (pane PTY processes, snapshot
//! plumbing, event fan-out) remains in `packages/server` for this M4
//! slice — it is too entangled with server-specific runtime primitives
//! (portable-pty, tokio channels, recording runtimes) to relocate
//! without a dependency explosion. Migrating the runtime manager is a
//! deferred slice.

use anyhow::Result;
use bmux_session_models::{Session, SessionId, SessionInfo};
use std::collections::BTreeMap;

/// Authoritative session roster. Owns every live session's static
/// identity and client-set metadata.
#[derive(Debug, Default)]
pub struct SessionManager {
    sessions: BTreeMap<SessionId, Session>,
}

impl SessionManager {
    /// Construct an empty session manager.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
        }
    }

    /// Create a new session with an optional name. Returns the
    /// generated id.
    ///
    /// # Errors
    ///
    /// Today this method is infallible but keeps the `Result` return
    /// type for symmetry with [`Self::insert_session`] and for future
    /// capacity errors.
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

    /// Reference to a session by id.
    #[must_use]
    pub fn get_session(&self, session_id: &SessionId) -> Option<&Session> {
        self.sessions.get(session_id)
    }

    /// Mutable reference to a session by id.
    pub fn get_session_mut(&mut self, session_id: &SessionId) -> Option<&mut Session> {
        self.sessions.get_mut(session_id)
    }

    /// List every session as a `SessionInfo` record.
    #[must_use]
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.values().map(Into::into).collect()
    }

    /// Remove a session by id.
    ///
    /// # Errors
    ///
    /// Returns an error if no session has the given id.
    pub fn remove_session(&mut self, session_id: &SessionId) -> Result<()> {
        if self.sessions.remove(session_id).is_some() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("Session not found: {session_id}"))
        }
    }
}
