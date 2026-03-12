#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)] // Internal packages don't need README metadata

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;
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

pub struct PaneId(pub Uuid);

impl PaneId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PaneId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PaneId {
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
// Errors
// ============================================================================

#[derive(Error, Debug)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Client not found: {0}")]
    ClientNotFound(ClientId),
    #[error("Session already exists: {0}")]
    AlreadyExists(SessionId),
    #[error("Session access denied for client {client}: {reason}")]
    AccessDenied { client: ClientId, reason: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Error, Debug)]
pub enum LayoutError {
    #[error("Pane not found: {0}")]
    PaneNotFound(PaneId),
    #[error("Invalid layout configuration: {0}")]
    InvalidLayout(String),
    #[error("Cannot split pane: {reason}")]
    SplitFailed { reason: String },
}

#[derive(Error, Debug)]
pub enum PaneError {
    #[error("Pane not found: {0}")]
    NotFound(PaneId),
    #[error("Pane already exists: {0}")]
    AlreadyExists(PaneId),
    #[error("Invalid dimensions: width={width}, height={height}")]
    InvalidDimensions { width: u16, height: u16 },
    #[error("Pane is busy: {0}")]
    Busy(PaneId),
}

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("Client not found: {0}")]
    NotFound(ClientId),
    #[error("Client already exists: {0}")]
    AlreadyExists(ClientId),
    #[error("Client disconnected: {0}")]
    Disconnected(ClientId),
    #[error("Authentication failed for client: {0}")]
    AuthenticationFailed(ClientId),
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

// ============================================================================
// Client Models
// ============================================================================

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]

pub struct ClientInfo {
    pub id: ClientId,
    pub session_id: Option<SessionId>,
    pub independent_view: bool,
    pub following_client: Option<ClientId>,
    pub connected_at: std::time::SystemTime,
    pub last_activity: std::time::SystemTime,
}

impl ClientInfo {
    #[must_use]
    pub fn new(independent_view: bool, following_client: Option<ClientId>) -> Self {
        let now = std::time::SystemTime::now();
        Self {
            id: ClientId::new(),
            session_id: None,
            independent_view,
            following_client,
            connected_at: now,
            last_activity: now,
        }
    }

    pub fn attach_to_session(&mut self, session_id: SessionId) {
        self.session_id = Some(session_id);
        self.update_activity();
    }

    pub fn detach_from_session(&mut self) {
        self.session_id = None;
        self.update_activity();
    }

    pub fn update_activity(&mut self) {
        self.last_activity = std::time::SystemTime::now();
    }
}
