//! Neutral primitive crate for the clients-plugin domain.
//!
//! Hosts the reader/writer trait abstractions, a handle newtype used
//! for registry lookup, and a `DefaultNoOp` fallback impl. Both core
//! (`packages/server`) and plugin implementations depend on this
//! crate; concrete `FollowState` lives in the plugin impl crate.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_ipc::{ClientSummary, Event};
use bmux_session_models::{ClientId, SessionId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;

/// A single follow relationship: `follower -> { leader, global }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowEntry {
    pub leader_client_id: ClientId,
    pub global: bool,
}

/// A follow-relationship update emitted when a leader's selection
/// changes and a global follower needs to be re-synced.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowTargetUpdate {
    pub follower_client_id: ClientId,
    pub leader_client_id: ClientId,
    pub context_id: Option<Uuid>,
    pub session_id: Option<SessionId>,
}

/// Snapshot of follow-state suitable for persistence round-trips.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FollowStateSnapshot {
    pub connected_clients: BTreeSet<ClientId>,
    pub selected_contexts: BTreeMap<ClientId, Option<Uuid>>,
    pub selected_sessions: BTreeMap<ClientId, Option<SessionId>>,
    pub follows: BTreeMap<ClientId, FollowEntrySnapshot>,
    /// Per-client attached streaming session (the session a client
    /// has an open `attach-open` stream against). Distinct from
    /// `selected_sessions`: selection is soft, attach is hard.
    #[serde(default)]
    pub attached_stream_sessions: BTreeMap<ClientId, Option<SessionId>>,
    /// Per-client "detach allowed" toggle. Defaults to `true` when
    /// absent; a client can set it to `false` to prevent accidental
    /// detach (used by integration tests and pinned surfaces).
    #[serde(default)]
    pub attach_detach_allowed: BTreeMap<ClientId, bool>,
}

/// Wire shape of [`FollowEntry`] for snapshot serialization.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct FollowEntrySnapshot {
    pub leader_client_id: ClientId,
    pub global: bool,
}

impl From<FollowEntry> for FollowEntrySnapshot {
    fn from(entry: FollowEntry) -> Self {
        Self {
            leader_client_id: entry.leader_client_id,
            global: entry.global,
        }
    }
}

impl From<FollowEntrySnapshot> for FollowEntry {
    fn from(entry: FollowEntrySnapshot) -> Self {
        Self {
            leader_client_id: entry.leader_client_id,
            global: entry.global,
        }
    }
}

/// Read-only view over follow-state. Server's hot-path lookups go
/// through this trait object; the clients plugin provides the real
/// implementation over an `Arc<RwLock<FollowState>>`.
pub trait FollowStateReader: Send + Sync {
    /// Currently-selected session for the given client, if any.
    fn selected_session(&self, client_id: ClientId) -> Option<SessionId>;
    /// Currently-selected context for the given client, if any.
    fn selected_context(&self, client_id: ClientId) -> Option<Uuid>;
    /// Follow relationship the client is currently participating in as
    /// a follower, if any.
    fn follow_target(&self, client_id: ClientId) -> Option<FollowEntry>;
    /// All connected clients rendered as wire `ClientSummary` rows.
    fn list_clients(&self) -> Vec<ClientSummary>;
    /// Joint `(selected_context, selected_session)` lookup — cheaper
    /// than two separate calls when the caller needs both.
    fn selected_target(&self, client_id: ClientId) -> Option<(Option<Uuid>, Option<SessionId>)>;
    /// Whether the given client is currently connected.
    fn is_connected(&self, client_id: ClientId) -> bool;
    /// Session the client has an active `attach-open` stream against.
    /// Distinct from `selected_session`: selection is soft
    /// (persisted, follow-driven); attach is hard (IO pipe open).
    fn attached_stream_session(&self, client_id: ClientId) -> Option<SessionId>;
    /// Whether the client currently permits server-initiated detach.
    /// Defaults to `true` when the client hasn't explicitly opted
    /// out.
    fn attach_detach_allowed(&self, client_id: ClientId) -> bool;
}

/// Mutation surface over follow-state. Server calls these on the
/// connection-protocol path (client connect/disconnect, selection
/// persistence). The clients plugin also calls them internally when
/// orchestrating `set-following`.
pub trait FollowStateWriter: FollowStateReader {
    /// Mark a client connected with empty selection.
    fn connect_client(&self, client_id: ClientId);
    /// Mark a client disconnected; returns any `FollowTargetGone`
    /// events for followers that were tracking this client.
    fn disconnect_client(&self, client_id: ClientId) -> Vec<Event>;
    /// Update the client's selected context + session target.
    fn set_selected_target(
        &self,
        client_id: ClientId,
        context_id: Option<Uuid>,
        session_id: Option<SessionId>,
    );
    /// Clear selections for every connected client (used by
    /// `clear_selected_session_for_all` in server persistence).
    fn clear_all_selections(&self);
    /// Sync global followers of `leader_client_id` to mirror the new
    /// selection. Returns the followers that actually changed target.
    fn sync_followers_from_leader(
        &self,
        leader_client_id: ClientId,
        selected_context: Option<Uuid>,
        selected_session: Option<SessionId>,
    ) -> Vec<FollowTargetUpdate>;
    /// Start a follow relationship.
    ///
    /// # Errors
    ///
    /// Returns a static error message when the relationship would be
    /// invalid (follower equals leader, either not connected).
    fn start_follow(
        &self,
        follower_client_id: ClientId,
        leader_client_id: ClientId,
        global: bool,
    ) -> Result<(Option<Uuid>, Option<SessionId>), &'static str>;
    /// Dissolve a follow relationship. Returns `true` if one existed.
    fn stop_follow(&self, follower_client_id: ClientId) -> bool;
    /// Clear all follow relationships and every client's selected
    /// context/session. Used by snapshot-restore paths that replace
    /// all state in one shot.
    fn clear_all_follow_state(&self);
    /// Clear selections for every client whose currently-selected
    /// session matches `session_id`. For each impacted client,
    /// synchronize any global followers of that client.
    ///
    /// Used by server's `persist_selected_session` path when a
    /// session is removed and every client tracking it needs to be
    /// moved off that target.
    fn clear_selections_for_session(&self, session_id: SessionId);
    /// Capture a full snapshot (for persistence).
    fn snapshot(&self) -> FollowStateSnapshot;
    /// Replace state with a previously-captured snapshot.
    fn restore_snapshot(&self, snapshot: FollowStateSnapshot);
    /// Update the client's attached streaming session (the session
    /// whose attach-open stream is currently held by this client).
    /// Passing `None` clears the field.
    fn set_attached_stream_session(&self, client_id: ClientId, session_id: Option<SessionId>);
    /// Update the "detach allowed" toggle for a client.
    fn set_attach_detach_allowed(&self, client_id: ClientId, allowed: bool);
}

/// Registry newtype wrapping an `Arc<dyn FollowStateWriter>`. Server
/// and plugin crates look this up via
/// [`bmux_plugin::PluginStateRegistry`] to access the live follow
/// state without naming the concrete plugin-owned type.
#[derive(Clone)]
pub struct FollowStateHandle(pub Arc<dyn FollowStateWriter>);

impl FollowStateHandle {
    #[must_use]
    pub fn new<W: FollowStateWriter + 'static>(writer: W) -> Self {
        Self(Arc::new(writer))
    }

    #[must_use]
    pub fn from_arc(writer: Arc<dyn FollowStateWriter>) -> Self {
        Self(writer)
    }
}

/// No-op default impl. Server registers one at startup so hot-path
/// reads don't need to check for handle presence; the clients plugin
/// overwrites the registry entry during `activate`.
#[derive(Debug, Default)]
pub struct NoopFollowState;

impl FollowStateReader for NoopFollowState {
    fn selected_session(&self, _client_id: ClientId) -> Option<SessionId> {
        None
    }
    fn selected_context(&self, _client_id: ClientId) -> Option<Uuid> {
        None
    }
    fn follow_target(&self, _client_id: ClientId) -> Option<FollowEntry> {
        None
    }
    fn list_clients(&self) -> Vec<ClientSummary> {
        Vec::new()
    }
    fn selected_target(&self, _client_id: ClientId) -> Option<(Option<Uuid>, Option<SessionId>)> {
        None
    }
    fn is_connected(&self, _client_id: ClientId) -> bool {
        false
    }
    fn attached_stream_session(&self, _client_id: ClientId) -> Option<SessionId> {
        None
    }
    fn attach_detach_allowed(&self, _client_id: ClientId) -> bool {
        true
    }
}

impl FollowStateWriter for NoopFollowState {
    fn connect_client(&self, _client_id: ClientId) {}
    fn disconnect_client(&self, _client_id: ClientId) -> Vec<Event> {
        Vec::new()
    }
    fn set_selected_target(
        &self,
        _client_id: ClientId,
        _context_id: Option<Uuid>,
        _session_id: Option<SessionId>,
    ) {
    }
    fn clear_all_selections(&self) {}
    fn sync_followers_from_leader(
        &self,
        _leader_client_id: ClientId,
        _selected_context: Option<Uuid>,
        _selected_session: Option<SessionId>,
    ) -> Vec<FollowTargetUpdate> {
        Vec::new()
    }
    fn start_follow(
        &self,
        _follower_client_id: ClientId,
        _leader_client_id: ClientId,
        _global: bool,
    ) -> Result<(Option<Uuid>, Option<SessionId>), &'static str> {
        Err("clients plugin not active")
    }
    fn stop_follow(&self, _follower_client_id: ClientId) -> bool {
        false
    }
    fn clear_all_follow_state(&self) {}
    fn clear_selections_for_session(&self, _session_id: SessionId) {}
    fn snapshot(&self) -> FollowStateSnapshot {
        FollowStateSnapshot::default()
    }
    fn restore_snapshot(&self, _snapshot: FollowStateSnapshot) {}
    fn set_attached_stream_session(&self, _client_id: ClientId, _session_id: Option<SessionId>) {}
    fn set_attach_detach_allowed(&self, _client_id: ClientId, _allowed: bool) {}
}

impl FollowStateHandle {
    /// Return a handle backed by the built-in no-op impl.
    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopFollowState)
    }
}

// ── Client principal lookup ──────────────────────────────────────────

/// Read/write trait for the server's per-client principal map.
///
/// Server owns the concrete `BTreeMap<ClientId, Uuid>` on
/// `ServerState` and registers a trait object via
/// [`ClientPrincipalHandle`]. Plugins look up the handle from the
/// plugin state registry and query principals for the clients they
/// act on behalf of (e.g. when performing policy checks that gate on
/// principal identity).
pub trait ClientPrincipalLookup: Send + Sync {
    /// Return the principal id assigned to the given client, if any.
    fn get(&self, client_id: ClientId) -> Option<Uuid>;
    /// Record a client's principal id. Called by the server on
    /// connection handshake.
    fn set(&self, client_id: ClientId, principal_id: Uuid);
    /// Forget a client's principal id on disconnect.
    fn remove(&self, client_id: ClientId);
}

/// Registry newtype wrapping `Arc<dyn ClientPrincipalLookup>`.
#[derive(Clone)]
pub struct ClientPrincipalHandle(pub Arc<dyn ClientPrincipalLookup>);

impl ClientPrincipalHandle {
    #[must_use]
    pub fn new<L: ClientPrincipalLookup + 'static>(lookup: L) -> Self {
        Self(Arc::new(lookup))
    }

    #[must_use]
    pub fn from_arc(lookup: Arc<dyn ClientPrincipalLookup>) -> Self {
        Self(lookup)
    }

    /// Construct a handle backed by the built-in no-op impl.
    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopClientPrincipalLookup)
    }
}

/// No-op default impl. Server registers its real impl at startup.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopClientPrincipalLookup;

impl ClientPrincipalLookup for NoopClientPrincipalLookup {
    fn get(&self, _client_id: ClientId) -> Option<Uuid> {
        None
    }
    fn set(&self, _client_id: ClientId, _principal_id: Uuid) {}
    fn remove(&self, _client_id: ClientId) {}
}
