#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

// ============================================================================
// IDs
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// A random identifier. Simulated code names sessions from its seed instead
    /// ([`Session::with_id`]).
    #[must_use]
    pub fn new() -> Self {
        // simulation-safe: never called by simulated code (which names sessions from its seed)
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ClientId(pub Uuid);

impl ClientId {
    /// A random identifier.
    #[must_use]
    pub fn new() -> Self {
        // simulation-safe: never called by simulated code (no clients connect in a simulation)
        Self(Uuid::new_v4())
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Session selector accepted by session-domain commands and protocol helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum SessionSelector {
    ById(Uuid),
    ByName(String),
}

// ============================================================================
// Session Models
// ============================================================================

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Session {
    pub id: SessionId,
    pub name: Option<String>,
    pub clients: BTreeSet<ClientId>,
}

impl Session {
    #[must_use]
    pub fn new(name: Option<String>) -> Self {
        Self::with_id(SessionId::new(), name)
    }

    /// A session with the identifier `id` (for callers that choose their own, such as a
    /// simulation whose identifiers come from its seed).
    #[must_use]
    pub const fn with_id(id: SessionId, name: Option<String>) -> Self {
        Self {
            id,
            name,
            clients: BTreeSet::new(),
        }
    }

    pub fn add_client(&mut self, client_id: ClientId) {
        self.clients.insert(client_id);
    }

    pub fn remove_client(&mut self, client_id: &ClientId) -> bool {
        self.clients.remove(client_id)
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: Option<String>,
    pub client_count: usize,
}

impl From<&Session> for SessionInfo {
    fn from(session: &Session) -> Self {
        Self {
            id: session.id,
            name: session.name.clone(),
            client_count: session.clients.len(),
        }
    }
}
