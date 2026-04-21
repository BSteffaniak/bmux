//! Client follow-state tracking, owned by the clients plugin.
//!
//! `FollowState` is the authoritative record of:
//!
//! - Which clients are currently connected.
//! - Which context/session each client has "selected" (the target they
//!   are currently attached to).
//! - Follow relationships: which clients mirror another client's
//!   selection ("following a leader").
//!
//! The clients plugin owns this type. The runtime handle is
//! constructed during the plugin's `activate` callback and registered
//! into [`bmux_plugin::PluginStateRegistry`]. The type lives in
//! `bmux_clients_plugin_api` so core server code and other plugins can
//! name the type without depending on the plugin impl crate.
//!
//! The wire-level operations that mutate follow state (start follow,
//! stop follow, update selected target) ultimately route through the
//! clients plugin's typed dispatch surface.

use bmux_ipc::{ClientSummary, Event};
use bmux_session_models::{ClientId, SessionId};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// A single follow relationship: `follower -> { leader, global }`.
#[derive(Debug, Clone, Copy)]
pub struct FollowEntry {
    pub leader_client_id: ClientId,
    pub global: bool,
}

/// A follow-relationship update emitted when a leader's selection
/// changes and a global follower needs to be re-synced.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy)]
pub struct FollowTargetUpdate {
    pub follower_client_id: ClientId,
    pub leader_client_id: ClientId,
    pub context_id: Option<Uuid>,
    pub session_id: Option<SessionId>,
}

/// Authoritative tracking of connected clients, their selected
/// context/session, and follow relationships between clients.
#[derive(Debug, Default)]
pub struct FollowState {
    pub connected_clients: BTreeSet<ClientId>,
    pub selected_contexts: BTreeMap<ClientId, Option<Uuid>>,
    pub selected_sessions: BTreeMap<ClientId, Option<SessionId>>,
    pub follows: BTreeMap<ClientId, FollowEntry>,
}

impl FollowState {
    /// Mark a client as connected with no initial selection.
    pub fn connect_client(&mut self, client_id: ClientId) {
        self.connected_clients.insert(client_id);
        self.selected_contexts.entry(client_id).or_insert(None);
        self.selected_sessions.entry(client_id).or_insert(None);
    }

    /// Remove a client's tracking and return follow-target-gone events
    /// for any clients that were following it.
    pub fn disconnect_client(&mut self, client_id: ClientId) -> Vec<Event> {
        self.connected_clients.remove(&client_id);
        self.selected_contexts.remove(&client_id);
        self.selected_sessions.remove(&client_id);
        self.follows.remove(&client_id);

        #[allow(clippy::needless_collect)]
        let impacted_followers = self
            .follows
            .iter()
            .filter_map(|(follower_id, entry)| {
                (entry.leader_client_id == client_id).then_some(*follower_id)
            })
            .collect::<Vec<_>>();

        impacted_followers
            .into_iter()
            .filter_map(|follower_id| {
                self.follows
                    .remove(&follower_id)
                    .map(|entry| Event::FollowTargetGone {
                        follower_client_id: follower_id.0,
                        former_leader_client_id: entry.leader_client_id.0,
                    })
            })
            .collect()
    }

    /// Record a client's current selected context+session target.
    pub fn set_selected_target(
        &mut self,
        client_id: ClientId,
        context_id: Option<Uuid>,
        session_id: Option<SessionId>,
    ) {
        if self.connected_clients.contains(&client_id) {
            self.selected_contexts.insert(client_id, context_id);
            self.selected_sessions.insert(client_id, session_id);
        }
    }

    /// Retrieve the currently selected context+session for a client,
    /// or `None` if the client is not connected.
    #[must_use]
    pub fn selected_target(
        &self,
        client_id: ClientId,
    ) -> Option<(Option<Uuid>, Option<SessionId>)> {
        Some((
            self.selected_contexts.get(&client_id).copied()?,
            self.selected_sessions.get(&client_id).copied()?,
        ))
    }

    /// Create a follow relationship. Returns the leader's current
    /// target if `global` is true, else `(None, None)`.
    ///
    /// # Errors
    ///
    /// Returns a static error message when the follower equals the
    /// leader or when either client is not connected.
    pub fn start_follow(
        &mut self,
        follower_client_id: ClientId,
        leader_client_id: ClientId,
        global: bool,
    ) -> core::result::Result<(Option<Uuid>, Option<SessionId>), &'static str> {
        if follower_client_id == leader_client_id {
            return Err("cannot follow self");
        }
        if !self.connected_clients.contains(&leader_client_id) {
            return Err("target client not connected");
        }
        if !self.connected_clients.contains(&follower_client_id) {
            return Err("follower client not connected");
        }

        self.follows.insert(
            follower_client_id,
            FollowEntry {
                leader_client_id,
                global,
            },
        );

        if global {
            let leader_context = self
                .selected_contexts
                .get(&leader_client_id)
                .copied()
                .flatten();
            let leader_session = self
                .selected_sessions
                .get(&leader_client_id)
                .copied()
                .flatten();
            self.selected_contexts
                .insert(follower_client_id, leader_context);
            self.selected_sessions
                .insert(follower_client_id, leader_session);
            return Ok((leader_context, leader_session));
        }

        Ok((None, None))
    }

    /// Dissolve the follow relationship, returning true if one existed.
    pub fn stop_follow(&mut self, follower_client_id: ClientId) -> bool {
        self.follows.remove(&follower_client_id).is_some()
    }

    /// When a leader's target changes, sync any global followers and
    /// return a `FollowTargetUpdate` for each follower whose effective
    /// target changed.
    pub fn sync_followers_from_leader(
        &mut self,
        leader_client_id: ClientId,
        selected_context: Option<Uuid>,
        selected_session: Option<SessionId>,
    ) -> Vec<FollowTargetUpdate> {
        let followers = self
            .follows
            .iter()
            .filter_map(|(follower_id, entry)| {
                (entry.leader_client_id == leader_client_id && entry.global).then_some(*follower_id)
            })
            .collect::<Vec<_>>();

        let mut updates = Vec::new();
        for follower_id in followers {
            if self.connected_clients.contains(&follower_id) {
                let previous = self.selected_sessions.get(&follower_id).copied().flatten();
                let previous_context = self.selected_contexts.get(&follower_id).copied().flatten();
                self.selected_contexts.insert(follower_id, selected_context);
                self.selected_sessions.insert(follower_id, selected_session);
                let changed = previous != selected_session || previous_context != selected_context;
                if changed {
                    updates.push(FollowTargetUpdate {
                        follower_client_id: follower_id,
                        leader_client_id,
                        context_id: selected_context,
                        session_id: selected_session,
                    });
                }
            }
        }

        updates
    }

    /// Render the current client roster as a list of wire-level
    /// `ClientSummary` rows.
    #[must_use]
    pub fn list_clients(&self) -> Vec<ClientSummary> {
        self.connected_clients
            .iter()
            .map(|client_id| {
                let selected_session_id = self
                    .selected_sessions
                    .get(client_id)
                    .and_then(|selected| selected.map(|session_id| session_id.0));
                let selected_context_id = self.selected_contexts.get(client_id).copied().flatten();
                let (following_client_id, following_global) =
                    self.follows.get(client_id).map_or((None, false), |entry| {
                        (Some(entry.leader_client_id.0), entry.global)
                    });

                ClientSummary {
                    id: client_id.0,
                    selected_context_id,
                    selected_session_id,
                    following_client_id,
                    following_global,
                }
            })
            .collect()
    }
}
