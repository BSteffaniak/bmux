//! Neutral primitive crate for the pane-runtime plugin's attach-token
//! surface.
//!
//! Hosts the reader/writer trait abstractions, a handle newtype used
//! for registry lookup, and a `NoopAttachTokenManager` fallback. The
//! concrete `AttachTokenManager` implementation lives on the server
//! (`packages/server`) so it can be registered during `BmuxServer::new`
//! before any plugin activates. The pane-runtime plugin reaches it
//! through [`AttachTokenManagerHandle`] looked up from the shared
//! plugin state registry.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_ipc::AttachGrant;
use bmux_session_models::SessionId;
use std::sync::Arc;
use uuid::Uuid;

/// Reasons an attach-token `consume` call can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachTokenValidationError {
    /// Token not present in the manager.
    NotFound,
    /// Token was present but past its TTL; the manager evicted it.
    Expired,
    /// Token existed but was issued for a different session than the
    /// one the caller claimed.
    SessionMismatch,
}

/// Read-only query surface.
pub trait AttachTokenManagerReader: Send + Sync {
    /// Whether the given token is currently tracked (regardless of
    /// expiration).
    fn contains(&self, token: Uuid) -> bool;
}

/// Mutation surface over the attach-token manager.
pub trait AttachTokenManagerWriter: AttachTokenManagerReader {
    /// Issue a fresh attach grant for the given session.
    fn issue(&self, session_id: SessionId) -> AttachGrant;

    /// Consume (atomically validate + remove) a token previously
    /// issued for `session_id`. Returns a validation error when the
    /// token is missing, expired, or bound to a different session.
    ///
    /// # Errors
    ///
    /// See [`AttachTokenValidationError`].
    fn consume(&self, session_id: SessionId, token: Uuid)
    -> Result<(), AttachTokenValidationError>;

    /// Drop every token issued for the given session (used by
    /// session-removal flows).
    fn remove_for_session(&self, session_id: SessionId);

    /// Drop every token (used by server shutdown / full reset).
    fn clear(&self);
}

/// Registry newtype wrapping `Arc<dyn AttachTokenManagerWriter>`.
#[derive(Clone)]
pub struct AttachTokenManagerHandle(pub Arc<dyn AttachTokenManagerWriter>);

impl AttachTokenManagerHandle {
    #[must_use]
    pub fn new<W: AttachTokenManagerWriter + 'static>(writer: W) -> Self {
        Self(Arc::new(writer))
    }

    #[must_use]
    pub fn from_arc(writer: Arc<dyn AttachTokenManagerWriter>) -> Self {
        Self(writer)
    }

    /// Handle backed by the built-in no-op impl (registered by the
    /// SDK at startup; server replaces it during `BmuxServer::new`).
    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopAttachTokenManager)
    }
}

/// No-op default impl. Server overrides during construction.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAttachTokenManager;

impl AttachTokenManagerReader for NoopAttachTokenManager {
    fn contains(&self, _token: Uuid) -> bool {
        false
    }
}

impl AttachTokenManagerWriter for NoopAttachTokenManager {
    fn issue(&self, session_id: SessionId) -> AttachGrant {
        AttachGrant {
            context_id: None,
            session_id: session_id.0,
            attach_token: Uuid::nil(),
            expires_at_epoch_ms: 0,
        }
    }
    fn consume(
        &self,
        _session_id: SessionId,
        _token: Uuid,
    ) -> Result<(), AttachTokenValidationError> {
        Err(AttachTokenValidationError::NotFound)
    }
    fn remove_for_session(&self, _session_id: SessionId) {}
    fn clear(&self) {}
}
