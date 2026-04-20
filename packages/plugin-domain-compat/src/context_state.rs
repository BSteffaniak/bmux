//! Client context tracking, owned by the contexts plugin.
//!
//! `ContextState` is the authoritative record of:
//!
//! - Named contexts (logical groupings keyed by UUID).
//! - The session each context is bound to.
//! - Which context each connected client has selected.
//! - A most-recently-used queue for fallback selection.
//!
//! During M4 this type lives in `bmux_plugin_domain_compat` (a neutral
//! crate reachable by both core and plugins). The state instance is
//! constructed and registered into [`bmux_plugin::PluginStateRegistry`]
//! by the contexts plugin; core server code obtains the registered
//! handle via `global_plugin_state_registry().expect_state::<ContextState>()`.

use bmux_ipc::{ContextSelector, ContextSummary};
use bmux_session_models::{ClientId, SessionId};
use std::collections::{BTreeMap, VecDeque};
use uuid::Uuid;

/// Attribute name used to stamp a bound session id onto a context.
pub const CONTEXT_SESSION_ID_ATTRIBUTE: &str = "bmux.session_id";

/// A single context: id, optional display name, arbitrary attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContext {
    pub id: Uuid,
    pub name: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

/// Authoritative tracking of runtime contexts, their session bindings,
/// and per-client selections.
#[derive(Debug, Default)]
pub struct ContextState {
    pub contexts: BTreeMap<Uuid, RuntimeContext>,
    pub session_by_context: BTreeMap<Uuid, SessionId>,
    pub selected_by_client: BTreeMap<ClientId, Uuid>,
    pub mru_contexts: VecDeque<Uuid>,
}

impl ContextState {
    /// List contexts MRU-first, then insertion order for stragglers.
    #[must_use]
    pub fn list(&self) -> Vec<ContextSummary> {
        let mut ordered_ids = self.mru_contexts.iter().copied().collect::<Vec<_>>();
        for id in self.contexts.keys().copied() {
            if !ordered_ids.contains(&id) {
                ordered_ids.push(id);
            }
        }

        ordered_ids
            .into_iter()
            .filter_map(|id| self.contexts.get(&id))
            .map(Self::to_summary)
            .collect()
    }

    /// Create a new context, automatically select it for `client_id`,
    /// and push it to the front of the MRU queue.
    pub fn create(
        &mut self,
        client_id: ClientId,
        name: Option<String>,
        attributes: BTreeMap<String, String>,
    ) -> ContextSummary {
        let context = RuntimeContext {
            id: Uuid::new_v4(),
            name,
            attributes,
        };
        let id = context.id;
        self.contexts.insert(id, context.clone());
        self.selected_by_client.insert(client_id, id);
        self.touch_mru(id);
        Self::to_summary(&context)
    }

    /// Summary of the client's currently selected context, falling back
    /// to the most-recently-used context if the client's selection is
    /// missing.
    #[must_use]
    pub fn current_for_client(&self, client_id: ClientId) -> Option<ContextSummary> {
        let selected = self
            .selected_by_client
            .get(&client_id)
            .copied()
            .filter(|id| self.contexts.contains_key(id))
            .or_else(|| {
                self.mru_contexts
                    .iter()
                    .copied()
                    .find(|id| self.contexts.contains_key(id))
            })?;
        self.contexts.get(&selected).map(Self::to_summary)
    }

    /// Session bound to the client's currently selected context.
    #[must_use]
    pub fn current_session_for_client(&self, client_id: ClientId) -> Option<SessionId> {
        let selected = self
            .selected_by_client
            .get(&client_id)
            .copied()
            .filter(|id| self.contexts.contains_key(id))
            .or_else(|| {
                self.mru_contexts
                    .iter()
                    .copied()
                    .find(|id| self.contexts.contains_key(id))
            })?;
        self.session_by_context.get(&selected).copied()
    }

    /// Context (if any) that is bound to the given session.
    #[must_use]
    pub fn context_for_session(&self, session_id: SessionId) -> Option<Uuid> {
        self.session_by_context
            .iter()
            .find_map(|(context_id, mapped_session_id)| {
                (*mapped_session_id == session_id).then_some(*context_id)
            })
    }

    /// Select a context for the given client.
    ///
    /// # Errors
    ///
    /// Returns a static error message when the selector does not
    /// resolve to exactly one context.
    pub fn select_for_client(
        &mut self,
        client_id: ClientId,
        selector: &ContextSelector,
    ) -> core::result::Result<ContextSummary, &'static str> {
        let id = self.resolve_id(selector)?;
        self.selected_by_client.insert(client_id, id);
        self.touch_mru(id);
        self.contexts
            .get(&id)
            .map(Self::to_summary)
            .ok_or("context not found")
    }

    /// Close a context selected by `selector`, preferring `client_id`
    /// as a destination for replacement selection. Returns the removed
    /// context's id + bound session (if any).
    ///
    /// # Errors
    ///
    /// Returns a static error message when the selector does not
    /// resolve or the context is already gone.
    pub fn close(
        &mut self,
        client_id: ClientId,
        selector: &ContextSelector,
        _force: bool,
    ) -> core::result::Result<(Uuid, Option<SessionId>), &'static str> {
        let id = self.resolve_id(selector)?;
        self.remove_context_by_id(id, Some(client_id))
            .ok_or("context not found")
    }

    /// Remove every context whose binding points at `session_id`.
    /// Returns the removed context ids.
    pub fn remove_contexts_for_session(&mut self, session_id: SessionId) -> Vec<Uuid> {
        let context_ids = self
            .session_by_context
            .iter()
            .filter_map(|(context_id, mapped)| (*mapped == session_id).then_some(*context_id))
            .collect::<Vec<_>>();
        let mut removed = Vec::with_capacity(context_ids.len());
        for context_id in context_ids {
            if let Some((removed_id, _)) = self.remove_context_by_id(context_id, None) {
                removed.push(removed_id);
            }
        }
        removed
    }

    /// Bind a context to a session, stamping the session id as an
    /// attribute.
    ///
    /// # Errors
    ///
    /// Returns a static error message when the context id is unknown.
    pub fn bind_session(
        &mut self,
        context_id: Uuid,
        session_id: SessionId,
    ) -> core::result::Result<(), &'static str> {
        let Some(context) = self.contexts.get_mut(&context_id) else {
            return Err("context not found");
        };
        context.attributes.insert(
            CONTEXT_SESSION_ID_ATTRIBUTE.to_string(),
            session_id.0.to_string(),
        );
        self.session_by_context.insert(context_id, session_id);
        Ok(())
    }

    /// Forget the client's selected-context so subsequent lookups fall
    /// back to the MRU queue.
    pub fn disconnect_client(&mut self, client_id: ClientId) {
        self.selected_by_client.remove(&client_id);
    }

    /// Resolve a selector to a single context id.
    ///
    /// # Errors
    ///
    /// Returns a static error message when the selector doesn't match,
    /// or is ambiguous across multiple contexts.
    pub fn resolve_id(
        &self,
        selector: &ContextSelector,
    ) -> core::result::Result<Uuid, &'static str> {
        match selector {
            ContextSelector::ById(id) => {
                if self.contexts.contains_key(id) {
                    Ok(*id)
                } else {
                    Err("context not found")
                }
            }
            ContextSelector::ByName(name) => {
                let mut matches = self
                    .contexts
                    .values()
                    .filter(|context| context.name.as_deref() == Some(name.as_str()))
                    .map(|context| context.id);
                let Some(first) = matches.next() else {
                    return Err("context not found");
                };
                if matches.next().is_some() {
                    return Err("context selector by name is ambiguous");
                }
                Ok(first)
            }
        }
    }

    /// Bump a context to the front of the MRU queue.
    pub fn touch_mru(&mut self, id: Uuid) {
        self.mru_contexts.retain(|entry| *entry != id);
        self.mru_contexts.push_front(id);
    }

    /// Remove a context by id, re-selecting replacement contexts for
    /// impacted clients. Returns `(removed_id, removed_session)` if
    /// the context existed.
    pub fn remove_context_by_id(
        &mut self,
        context_id: Uuid,
        preferred_client: Option<ClientId>,
    ) -> Option<(Uuid, Option<SessionId>)> {
        let removed = self.contexts.remove(&context_id)?;
        let removed_session = self.session_by_context.remove(&context_id);
        self.mru_contexts.retain(|entry| *entry != context_id);

        let replacement = self
            .mru_contexts
            .iter()
            .copied()
            .find(|candidate| self.contexts.contains_key(candidate));

        let impacted = self
            .selected_by_client
            .iter()
            .filter_map(|(id_key, selected)| (*selected == removed.id).then_some(*id_key))
            .collect::<Vec<_>>();
        for impacted_client in impacted {
            if let Some(next_id) = replacement {
                self.selected_by_client.insert(impacted_client, next_id);
            } else {
                self.selected_by_client.remove(&impacted_client);
            }
        }

        if let Some(client_id) = preferred_client
            && !self.selected_by_client.contains_key(&client_id)
            && let Some(next_id) = replacement
        {
            self.selected_by_client.insert(client_id, next_id);
        }

        Some((removed.id, removed_session))
    }

    /// Render a single context as a `ContextSummary`.
    #[must_use]
    pub fn to_summary(context: &RuntimeContext) -> ContextSummary {
        ContextSummary {
            id: context.id,
            name: context.name.clone(),
            attributes: context.attributes.clone(),
        }
    }
}
