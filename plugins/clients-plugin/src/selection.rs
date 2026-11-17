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

    mod selection_policy_tests {
        use super::*;
        use bmux_plugin::StreamOrigin;
        use bmux_plugin_sdk::typed_dispatch::{
            SelectedServiceAvailability, ServiceAvailabilityKey,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct PolicyHost {
            mode: u8,
            origin: Option<StreamOrigin>,
            calls: AtomicUsize,
        }
        impl bmux_plugin::ServiceCaller for PolicyHost {
            fn selected_service_availability(
                &self,
                key: ServiceAvailabilityKey,
            ) -> bmux_plugin_sdk::definition::PluginFuture<
                '_,
                bmux_plugin_sdk::Result<SelectedServiceAvailability>,
            > {
                use bmux_plugin_sdk::TypedServiceEndpoint;

                let descriptor = bmux_permissions_plugin_api::session_policy_state::client::CheckEndpoint::service_descriptor();
                assert_eq!(key.capability, descriptor.capability.to_string());
                assert_eq!(key.kind, descriptor.kind);
                assert_eq!(key.interface_id, descriptor.interface_id.to_string());
                Box::pin(async move {
                    match self.mode {
                        0 => Ok(SelectedServiceAvailability::Absent),
                        6 => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                            operation: "selected_service_availability",
                        }),
                        7 => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "selected registry unavailable".into(),
                        }),
                        _ => Ok(SelectedServiceAvailability::Present),
                    }
                })
            }

            fn current_stream_origin(&self) -> bmux_plugin_sdk::Result<StreamOrigin> {
                self.origin
                    .clone()
                    .ok_or_else(|| bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "trusted origin unavailable".into(),
                    })
            }

            fn call_service_raw(
                &self,
                capability: &str,
                kind: bmux_plugin_sdk::ServiceKind,
                interface: &str,
                operation: &str,
                payload: Vec<u8>,
            ) -> bmux_plugin_sdk::Result<Vec<u8>> {
                use bmux_plugin_sdk::PluginContract;

                self.calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(capability, "bmux.sessions.policy");
                assert_eq!(kind, bmux_plugin_sdk::ServiceKind::Query);
                assert_eq!(interface, "session-policy-state");
                assert_eq!(operation, "check");
                bmux_plugin_sdk::service_contract::decode_for(
                    interface,
                    bmux_permissions_plugin_api::Contract::service_contract_for(interface).unwrap(),
                    &payload,
                )?;
                match self.mode {
                    1 | 3 => {
                        let response = bmux_plugin_sdk::encode_service_message(
                            &bmux_permissions_plugin_api::session_policy_state::SessionPolicyCheckResponse {
                                allowed: self.mode == 3,
                                reason: Some("policy decision".into()),
                            },
                        )?;
                        Ok(bmux_plugin_sdk::service_contract::encode_response(
                            &response,
                        ))
                    }
                    2 => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "policy storage failed".into(),
                    }),
                    4 => Ok(b"corrupt checked policy response".to_vec()),
                    5 => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                        operation: "call_service",
                    }),
                    _ => panic!("policy must not be invoked without proven presence"),
                }
            }
            fn execute_kernel_request(
                &self,
                _: bmux_ipc::Request,
            ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
                panic!("policy must use typed services")
            }
        }
        #[test]
        fn optional_policy_requires_proven_absence_and_trusted_local_origin() {
            let client = Uuid::from_u128(1);
            let session = Uuid::from_u128(2);
            let check = |mode, origin, allowed, calls| {
                let host = PolicyHost {
                    mode,
                    origin,
                    calls: AtomicUsize::new(0),
                };
                assert_eq!(
                    crate::authorize_selection(&host, client, None, session).is_ok(),
                    allowed,
                );
                assert_eq!(host.calls.load(Ordering::SeqCst), calls);
            };
            // Proven absence allows only independently trusted local callers.
            check(0, Some(StreamOrigin::Local), true, 0);
            for origin in [
                None,
                Some(StreamOrigin::Invalid),
                Some(StreamOrigin::HostInternal),
                Some(StreamOrigin::Admitted {
                    subject_id: Uuid::from_u128(3),
                    lease_id: Uuid::from_u128(4),
                }),
            ] {
                check(0, origin, false, 0);
            }
            // Advertised denial, storage failure, corrupt response and unsupported
            // dispatch never become the optional-provider fallback.
            for mode in [1, 2, 4, 5] {
                check(mode, Some(StreamOrigin::Local), false, 1);
            }
            check(3, Some(StreamOrigin::Local), true, 1);
            // Unsupported negotiation and failed lookup are not absence.
            for mode in [6, 7] {
                check(mode, Some(StreamOrigin::Local), false, 0);
            }
        }
    }

    #[test]
    fn reset_paths_fence_inflight_selection() {
        use bmux_client_state::FollowStateWriter;
        let client = ClientId(Uuid::from_u128(1));
        for clear_follow in [false, true] {
            let mut state = FollowState::default();
            state.connect_client(client);
            state.set_selected_target(client, None, Some(SessionId(Uuid::from_u128(10))));
            let reserved = state
                .begin_selection(client, state.selection(client).unwrap().revision)
                .unwrap();
            let inner = std::sync::Arc::new(std::sync::RwLock::new(state));
            let adapter = crate::FollowStateAdapter {
                inner: inner.clone(),
                events: crate::ClientEvents::default(),
            };
            if clear_follow {
                adapter.clear_all_follow_state();
            } else {
                adapter.clear_all_selections();
            }
            assert_eq!(
                inner.write().unwrap().commit_selection(
                    client,
                    reserved.revision,
                    None,
                    Some(Uuid::from_u128(10))
                ),
                Err(SelectionError::Conflict)
            );
            assert_eq!(
                inner.read().unwrap().selected_target(client),
                Some((None, None))
            );
        }
    }

    #[test]
    fn revision_exhaustion_keeps_input_suspended() {
        let mut state = FollowState::default();
        let client = ClientId(Uuid::from_u128(1));
        state.connect_client(client);
        state.selection_revisions.insert(client, u64::MAX);
        state.set_selected_target(client, None, Some(SessionId(Uuid::from_u128(10))));
        assert_eq!(state.selected_target(client), Some((None, None)));
        assert!(state.recover_selection(client, u64::MAX).is_err());
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
