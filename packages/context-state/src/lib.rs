//! Neutral primitive crate for the contexts-plugin domain.
//!
//! Hosts the reader/writer trait abstractions, a handle newtype used
//! for registry lookup, and a `DefaultNoOp` fallback impl.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_ipc::{ContextSelector, ContextSummary};
use bmux_session_models::{ClientId, SessionId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use uuid::Uuid;

/// Attribute name used to stamp a bound session id onto a context.
pub const CONTEXT_SESSION_ID_ATTRIBUTE: &str = "bmux.session_id";

/// A single context: id, optional display name, arbitrary attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeContext {
    pub id: Uuid,
    pub name: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

/// Snapshot of context-state suitable for persistence round-trips.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextStateSnapshot {
    pub contexts: BTreeMap<Uuid, RuntimeContext>,
    pub session_by_context: BTreeMap<Uuid, SessionId>,
    pub selected_by_client: BTreeMap<ClientId, Uuid>,
    pub mru_contexts: VecDeque<Uuid>,
}

/// Read-only view over context-state.
pub trait ContextStateReader: Send + Sync {
    /// List all contexts, MRU-first.
    fn list(&self) -> Vec<ContextSummary>;
    /// Summary of the client's currently-selected context (or MRU
    /// fallback).
    fn current_for_client(&self, client_id: ClientId) -> Option<ContextSummary>;
    /// Session bound to the client's currently-selected context.
    fn current_session_for_client(&self, client_id: ClientId) -> Option<SessionId>;
    /// Context (if any) bound to the given session.
    fn context_for_session(&self, session_id: SessionId) -> Option<Uuid>;
    /// Resolve a selector to a single context id.
    ///
    /// # Errors
    ///
    /// Returns a static error message when the selector doesn't match.
    fn resolve_id(&self, selector: &ContextSelector) -> Result<Uuid, &'static str>;
}

/// Mutation surface over context-state.
pub trait ContextStateWriter: ContextStateReader {
    /// Create a new context and auto-select it for the caller.
    fn create(
        &self,
        client_id: ClientId,
        name: Option<String>,
        attributes: BTreeMap<String, String>,
    ) -> ContextSummary;
    /// Select a context for the given client.
    ///
    /// # Errors
    ///
    /// Returns a static error message when the selector doesn't resolve.
    fn select_for_client(
        &self,
        client_id: ClientId,
        selector: &ContextSelector,
    ) -> Result<ContextSummary, &'static str>;
    /// Close a context selected by `selector`. Returns `(removed_id,
    /// removed_session)` when the context existed.
    ///
    /// # Errors
    ///
    /// Returns a static error message when the selector doesn't resolve
    /// or the context is already gone.
    fn close(
        &self,
        client_id: ClientId,
        selector: &ContextSelector,
        force: bool,
    ) -> Result<(Uuid, Option<SessionId>), &'static str>;
    /// Remove every context whose binding points at `session_id`.
    /// Returns the removed context ids.
    fn remove_contexts_for_session(&self, session_id: SessionId) -> Vec<Uuid>;
    /// Bind a context to a session.
    ///
    /// # Errors
    ///
    /// Returns a static error message when the context id is unknown.
    fn bind_session(&self, context_id: Uuid, session_id: SessionId) -> Result<(), &'static str>;
    /// Forget the client's selected-context.
    fn disconnect_client(&self, client_id: ClientId);
    /// Capture a full snapshot for persistence.
    fn snapshot(&self) -> ContextStateSnapshot;
    /// Replace state with a previously-captured snapshot.
    fn restore_snapshot(&self, snapshot: ContextStateSnapshot);
}

/// Registry newtype wrapping an `Arc<dyn ContextStateWriter>`.
#[derive(Clone)]
pub struct ContextStateHandle(pub Arc<dyn ContextStateWriter>);

impl ContextStateHandle {
    #[must_use]
    pub fn new<W: ContextStateWriter + 'static>(writer: W) -> Self {
        Self(Arc::new(writer))
    }

    #[must_use]
    pub fn from_arc(writer: Arc<dyn ContextStateWriter>) -> Self {
        Self(writer)
    }

    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopContextState)
    }
}

/// No-op default impl. Registered by server at startup; plugin
/// overwrites during `activate`.
#[derive(Debug, Default)]
pub struct NoopContextState;

impl ContextStateReader for NoopContextState {
    fn list(&self) -> Vec<ContextSummary> {
        Vec::new()
    }
    fn current_for_client(&self, _client_id: ClientId) -> Option<ContextSummary> {
        None
    }
    fn current_session_for_client(&self, _client_id: ClientId) -> Option<SessionId> {
        None
    }
    fn context_for_session(&self, _session_id: SessionId) -> Option<Uuid> {
        None
    }
    fn resolve_id(&self, _selector: &ContextSelector) -> Result<Uuid, &'static str> {
        Err("contexts plugin not active")
    }
}

impl ContextStateWriter for NoopContextState {
    fn create(
        &self,
        _client_id: ClientId,
        name: Option<String>,
        attributes: BTreeMap<String, String>,
    ) -> ContextSummary {
        ContextSummary {
            id: Uuid::nil(),
            name,
            attributes,
        }
    }
    fn select_for_client(
        &self,
        _client_id: ClientId,
        _selector: &ContextSelector,
    ) -> Result<ContextSummary, &'static str> {
        Err("contexts plugin not active")
    }
    fn close(
        &self,
        _client_id: ClientId,
        _selector: &ContextSelector,
        _force: bool,
    ) -> Result<(Uuid, Option<SessionId>), &'static str> {
        Err("contexts plugin not active")
    }
    fn remove_contexts_for_session(&self, _session_id: SessionId) -> Vec<Uuid> {
        Vec::new()
    }
    fn bind_session(&self, _context_id: Uuid, _session_id: SessionId) -> Result<(), &'static str> {
        Err("contexts plugin not active")
    }
    fn disconnect_client(&self, _client_id: ClientId) {}
    fn snapshot(&self) -> ContextStateSnapshot {
        ContextStateSnapshot::default()
    }
    fn restore_snapshot(&self, _snapshot: ContextStateSnapshot) {}
}
