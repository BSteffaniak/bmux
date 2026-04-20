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
    #[must_use]
    pub fn new() -> Self {
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
    #[must_use]
    pub fn new() -> Self {
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

// ============================================================================
// Session Models
// ============================================================================

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Session {
    pub id: SessionId,
    pub name: Option<String>,
    pub clients: BTreeSet<ClientId>,
    pub created_at: std::time::SystemTime,
    pub last_activity: std::time::SystemTime,
}

impl Session {
    #[must_use]
    pub fn new(name: Option<String>) -> Self {
        let now = std::time::SystemTime::now();
        Self {
            id: SessionId::new(),
            name,
            clients: BTreeSet::new(),
            created_at: now,
            last_activity: now,
        }
    }

    pub fn add_client(&mut self, client_id: ClientId) {
        self.clients.insert(client_id);
        self.update_activity();
    }

    pub fn remove_client(&mut self, client_id: &ClientId) -> bool {
        let removed = self.clients.remove(client_id);
        if removed {
            self.update_activity();
        }
        removed
    }

    #[must_use]
    pub fn has_client(&self, client_id: &ClientId) -> bool {
        self.clients.contains(client_id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    fn update_activity(&mut self) {
        self.last_activity = std::time::SystemTime::now();
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: Option<String>,
    pub client_count: usize,
    pub created_at: std::time::SystemTime,
    pub last_activity: std::time::SystemTime,
}

impl From<&Session> for SessionInfo {
    fn from(session: &Session) -> Self {
        Self {
            id: session.id,
            name: session.name.clone(),
            client_count: session.clients.len(),
            created_at: session.created_at,
            last_activity: session.last_activity,
        }
    }
}
