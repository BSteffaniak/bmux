//! Caller-scoped selection reservations, owned by the clients implementation.
use crate::follow_state::FollowState;
use bmux_clients_plugin_api::clients_selection_state_v1::{Selection, SelectionError};
use bmux_session_models::{ClientId, SessionId};
use uuid::Uuid;

impl FollowState {
    pub(crate) fn selection(&self, client: ClientId) -> Result<Selection, SelectionError> {
        if !self.connected_clients.contains(&client) {
            return Err(SelectionError::NoCurrentClient);
        }
        Ok(Selection {
            revision: self
                .selection_revisions
                .get(&client)
                .copied()
                .unwrap_or_default(),
            context_id: self.selected_contexts.get(&client).copied().flatten(),
            session_id: self
                .selected_sessions
                .get(&client)
                .copied()
                .flatten()
                .map(|id| id.0),
            suspended: self.selection_reservations.contains_key(&client),
        })
    }

    pub(crate) fn begin_selection(
        &mut self,
        client: ClientId,
        expected: u64,
    ) -> Result<Selection, SelectionError> {
        let current = self.selection(client)?;
        if current.revision != expected || current.suspended {
            return Err(SelectionError::Conflict);
        }
        let revision = expected
            .checked_add(1)
            .ok_or_else(|| SelectionError::Failed {
                reason: "selection revision exhausted".into(),
            })?;
        self.selection_revisions.insert(client, revision);
        self.selection_reservations.insert(client, revision);
        self.selection(client)
    }

    pub(crate) fn commit_selection(
        &mut self,
        client: ClientId,
        expected: u64,
        context: Option<Uuid>,
        session: Option<Uuid>,
    ) -> Result<Selection, SelectionError> {
        let current = self.selection(client)?;
        if current.revision != expected
            || self.selection_reservations.get(&client) != Some(&expected)
        {
            return Err(SelectionError::Conflict);
        }
        if context.is_some() && session.is_none() {
            return Err(SelectionError::InvalidTarget);
        }
        let revision = expected
            .checked_add(1)
            .ok_or_else(|| SelectionError::Failed {
                reason: "selection revision exhausted".into(),
            })?;
        self.selected_contexts.insert(client, context);
        self.selected_sessions
            .insert(client, session.map(SessionId));
        self.selection_revisions.insert(client, revision);
        self.selection_reservations.remove(&client);
        self.selection(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_suspend_input_and_reject_stale_commits() {
        let mut state = FollowState::default();
        let a = ClientId(Uuid::from_u128(1));
        let b = ClientId(Uuid::from_u128(2));
        state.connected_clients.extend([a, b]);
        state.set_selected_target(a, None, Some(SessionId(Uuid::from_u128(10))));
        state.set_selected_target(b, None, Some(SessionId(Uuid::from_u128(20))));
        let revision = state.selection(a).unwrap().revision;
        let reserved = state.begin_selection(a, revision).unwrap();
        assert_eq!(state.selected_target(a), Some((None, None)));
        assert_eq!(
            state.selected_target(b),
            Some((None, Some(SessionId(Uuid::from_u128(20)))))
        );
        assert_eq!(
            state.begin_selection(a, revision),
            Err(SelectionError::Conflict)
        );
        state.set_selected_target(a, None, Some(SessionId(Uuid::from_u128(30))));
        assert_eq!(
            state.commit_selection(a, reserved.revision, None, None),
            Err(SelectionError::Conflict)
        );
        assert!(state.selection(a).unwrap().suspended);
    }

    #[test]
    fn commit_requires_reservation_and_coherent_target() {
        let mut state = FollowState::default();
        let client = ClientId(Uuid::from_u128(1));
        state.connected_clients.insert(client);
        assert_eq!(
            state.commit_selection(client, 0, None, None),
            Err(SelectionError::Conflict)
        );
        let reserved = state.begin_selection(client, 0).unwrap();
        assert_eq!(
            state.commit_selection(client, reserved.revision, Some(Uuid::from_u128(9)), None),
            Err(SelectionError::InvalidTarget)
        );
        let committed = state
            .commit_selection(client, reserved.revision, None, None)
            .unwrap();
        assert!(!committed.suspended);
        assert!(committed.revision > reserved.revision);
    }
}
