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

    pub(crate) fn recover_selection(
        &mut self,
        client: ClientId,
        expected: u64,
    ) -> Result<Selection, SelectionError> {
        let current = self.selection(client)?;
        if current.revision != expected || !current.suspended {
            return Err(SelectionError::Conflict);
        }
        let revision = expected
            .checked_add(1)
            .ok_or_else(|| SelectionError::Failed {
                reason: "selection revision exhausted".into(),
            })?;
        self.selected_contexts.insert(client, None);
        self.selected_sessions.insert(client, None);
        self.selection_revisions.insert(client, revision);
        self.selection_reservations.remove(&client);
        self.selection(client)
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
    fn recovery_clears_target_and_fences_stale_recovery() {
        let mut state = FollowState::default();
        let client = ClientId(Uuid::from_u128(1));
        state.connected_clients.insert(client);
        state.set_selected_target(client, None, Some(SessionId(Uuid::from_u128(10))));
        let reserved = state
            .begin_selection(client, state.selection(client).unwrap().revision)
            .unwrap();
        state.set_selected_target(client, None, Some(SessionId(Uuid::from_u128(20))));
        assert_eq!(
            state.recover_selection(client, reserved.revision),
            Err(SelectionError::Conflict)
        );
        let recovered = state
            .recover_selection(client, state.selection(client).unwrap().revision)
            .unwrap();
        assert!(!recovered.suspended);
        assert_eq!(state.selected_target(client), Some((None, None)));
        assert_eq!(
            state.commit_selection(client, reserved.revision, None, Some(Uuid::from_u128(10))),
            Err(SelectionError::Conflict)
        );
        assert!(state.begin_selection(client, recovered.revision).is_ok());
    }

    #[test]
    fn reconnect_does_not_revive_an_old_reservation() {
        let mut state = FollowState::default();
        let client = ClientId(Uuid::from_u128(1));
        state.connect_client(client);
        let reserved = state.begin_selection(client, 0).unwrap();
        state.disconnect_client(client);
        state.connect_client(client);
        assert!(!state.selection(client).unwrap().suspended);
        assert_eq!(
            state.commit_selection(client, reserved.revision, None, None),
            Err(SelectionError::Conflict)
        );
    }

    #[test]
    fn commit_validation_rejects_mismatched_and_missing_bindings() {
        let context_id = Uuid::from_u128(1);
        let session = Uuid::from_u128(2);
        let mut context = bmux_contexts_plugin_api::contexts_state::ContextSummary {
            id: context_id,
            name: None,
            attributes: std::collections::BTreeMap::from([(
                "bmux.session_id".to_string(),
                session.to_string(),
            )]),
        };
        assert!(crate::validate_selection_binding(context_id, Some(session), &context).is_ok());
        assert_eq!(
            crate::validate_selection_binding(context_id, Some(Uuid::from_u128(3)), &context),
            Err(SelectionError::InvalidTarget)
        );
        assert_eq!(
            crate::validate_selection_binding(Uuid::from_u128(4), Some(session), &context),
            Err(SelectionError::InvalidTarget)
        );
        context.attributes.clear();
        assert_eq!(
            crate::validate_selection_binding(context_id, Some(session), &context),
            Err(SelectionError::InvalidTarget)
        );
        assert_eq!(
            crate::validate_selection_binding(context_id, None, &context),
            Err(SelectionError::InvalidTarget)
        );
    }

    #[test]
    fn selection_policy_denial_and_failure_are_not_missing_provider_fallback() {
        struct PolicyHost(u8);
        impl bmux_plugin::ServiceCaller for PolicyHost {
            fn call_service_raw(
                &self,
                _: &str,
                _: bmux_plugin_sdk::ServiceKind,
                _: &str,
                _: &str,
                _: Vec<u8>,
            ) -> bmux_plugin_sdk::Result<Vec<u8>> {
                match self.0 {
                    0 => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation { operation: "call_service" }),
                    1 => bmux_plugin_sdk::encode_service_message(&bmux_permissions_plugin_api::session_policy_state::SessionPolicyCheckResponse { allowed: false, reason: Some("denied".into()) }),
                    _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol { details: "policy storage failed".into() }),
                }
            }
            fn execute_kernel_request(
                &self,
                _: bmux_ipc::Request,
            ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
                panic!("policy must use typed services")
            }
        }
        let client = Uuid::from_u128(1);
        let session = Uuid::from_u128(2);
        assert!(crate::authorize_selection(&PolicyHost(0), client, None, session).is_ok());
        assert!(crate::authorize_selection(&PolicyHost(1), client, None, session).is_err());
        assert!(crate::authorize_selection(&PolicyHost(2), client, None, session).is_err());
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
