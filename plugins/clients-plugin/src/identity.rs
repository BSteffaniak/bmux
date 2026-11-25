//! Local caller identity resolution. This is not federated authentication.

use bmux_client_state::ClientPrincipalHandle;
use bmux_clients_plugin_api::clients_identity::{
    ClientsIdentityService, IdentityError, LocalPrincipal, StreamProvenance,
};
use bmux_session_models::ClientId;
use uuid::Uuid;

/// Foundational facade over the serialized host-bound kernel bridge. Missing or
/// old bridges fail closed; caller metadata is never used to choose an origin.
pub fn current_stream_provenance(
    caller: &impl bmux_plugin::ServiceCaller,
) -> Result<StreamProvenance, IdentityError> {
    match caller
        .execute_kernel_request(bmux_ipc::Request::CurrentStreamProvenance)
        .map_err(|_| IdentityError::UnsupportedContext)?
    {
        bmux_ipc::ResponsePayload::CurrentStreamProvenance { provenance } => Ok(match provenance {
            bmux_ipc::StreamProvenance::HostInternal => StreamProvenance::HostInternal,
            bmux_ipc::StreamProvenance::Local => StreamProvenance::Local,
            bmux_ipc::StreamProvenance::Admitted {
                subject_id,
                connection_id,
            } => StreamProvenance::Admitted {
                subject_id,
                connection_id,
            },
            bmux_ipc::StreamProvenance::Invalid => StreamProvenance::Invalid,
        }),
        _ => Err(IdentityError::UnsupportedContext),
    }
}

/// Bound only at invocation entry, never from a registration-time fallback connection.
pub struct CallerIdentity {
    states: std::sync::Arc<bmux_plugin::PluginStateRegistry>,
    caller: Option<Uuid>,
}

impl CallerIdentity {
    pub const fn new(
        states: std::sync::Arc<bmux_plugin::PluginStateRegistry>,
        caller: Option<Uuid>,
    ) -> Self {
        Self { states, caller }
    }
}

impl ClientsIdentityService for CallerIdentity {
    fn current_stream_provenance<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<StreamProvenance, IdentityError>> + Send + 'a>,
    > {
        // This legacy principal-only adapter has no authenticated host bridge.
        Box::pin(async { Err(IdentityError::UnsupportedContext) })
    }
    fn current_principal<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<LocalPrincipal, IdentityError>> + Send + 'a>,
    > {
        Box::pin(async move { current_principal(&self.states, self.caller) })
    }
}

pub fn current_principal(
    registry: &bmux_plugin::PluginStateRegistry,
    caller_client_id: Option<Uuid>,
) -> Result<LocalPrincipal, IdentityError> {
    let caller = caller_client_id.ok_or(IdentityError::NoCurrentClient)?;
    let handle = registry
        .get::<ClientPrincipalHandle>()
        .ok_or(IdentityError::UnsupportedContext)?;
    let lookup = handle
        .read()
        .map_err(|_| IdentityError::PrincipalUnavailable)?
        .clone();
    resolve_principal(&lookup, caller)
}

fn resolve_principal(
    lookup: &ClientPrincipalHandle,
    caller: Uuid,
) -> Result<LocalPrincipal, IdentityError> {
    let principal_id = lookup
        .0
        .get(ClientId(caller))
        .filter(|id| !id.is_nil())
        .ok_or(IdentityError::PrincipalUnavailable)?;
    Ok(LocalPrincipal { principal_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_client_state::ClientPrincipalLookup;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Principals(Mutex<BTreeMap<ClientId, Uuid>>);

    impl ClientPrincipalLookup for Principals {
        fn get(&self, client_id: ClientId) -> Option<Uuid> {
            self.0.lock().unwrap().get(&client_id).copied()
        }

        fn set(&self, client_id: ClientId, principal_id: Uuid) {
            self.0.lock().unwrap().insert(client_id, principal_id);
        }

        fn remove(&self, client_id: ClientId) {
            self.0.lock().unwrap().remove(&client_id);
        }
    }

    // The fixture is the trusted host: caller provenance is captured here, not
    // recovered from a consumer's serializable NativeServiceContext fields.
    struct IdentityHostDispatch(
        bmux_plugin_sdk::NativeServiceContext,
        std::sync::Arc<bmux_plugin::PluginStateRegistry>,
    );

    impl bmux_plugin_sdk::HostServiceDispatch for IdentityHostDispatch {
        // HostServiceDispatch fixes this return type to the SDK's PluginError.
        #[allow(clippy::result_large_err)]
        fn invoke(
            &self,
            _: &bmux_plugin_runtime::InvocationScope,
            request: bmux_plugin_sdk::HostServiceRequest<'_>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            use bmux_clients_plugin_api::clients_identity;

            assert_eq!(request.capability, "bmux.clients.read");
            assert_eq!(request.kind, bmux_plugin_sdk::ServiceKind::Query);
            assert_eq!(request.interface, "clients-identity/v1");
            let mut invocation = self.0.clone();
            invocation.request.operation = request.operation.into();
            invocation.request.payload = bmux_plugin_sdk::service_contract::decode_for(
                request.interface,
                clients_identity::SERVICE_CONTRACT,
                request.payload,
            )?
            .to_vec();
            let response = crate::ClientsPlugin::with_states(std::sync::Arc::clone(&self.1))
                .invoke_service(&invocation);
            assert!(response.error.is_none(), "{:?}", response.error);
            Ok(bmux_plugin_sdk::service_contract::encode_response(
                &response.payload,
            ))
        }
    }

    fn identity_service_context(first: Uuid) -> bmux_plugin_sdk::NativeServiceContext {
        use bmux_plugin_sdk::{
            ApiVersion, HostConnectionInfo, HostMetadata, HostScope, NativeServiceContext,
            ProviderId, RegisteredService, ServiceKind, ServiceRequest,
        };
        NativeServiceContext {
            host_service_port: None,
            plugin_id: "bmux.clients".into(),
            request: ServiceRequest {
                caller_plugin_id: "test.consumer".into(),
                service: RegisteredService {
                    capability: HostScope::new("bmux.clients.read").unwrap(),
                    kind: ServiceKind::Query,
                    interface_id: bmux_clients_plugin_api::clients_identity::INTERFACE_ID
                        .as_str()
                        .into(),
                    provider: ProviderId::Plugin("bmux.clients".into()),
                },
                operation: "current-principal".into(),
                payload: Vec::new(),
            },
            required_capabilities: Vec::new(),
            provided_capabilities: vec!["bmux.clients.read".into()],
            services: Vec::new(),
            available_capabilities: vec!["bmux.clients.read".into()],
            enabled_plugins: vec!["bmux.clients".into()],
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".into(),
                product_version: "test".into(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: Vec::new(),
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: Some(first),
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: None,
        }
    }

    #[test]
    fn generated_client_reaches_native_caller_bound_provider() {
        use bmux_clients_plugin_api::clients_identity;
        use std::sync::{Arc, RwLock};

        let first = Uuid::from_u128(101);
        let second = Uuid::from_u128(102);
        let owner = Uuid::from_u128(103);
        let other_owner = Uuid::from_u128(104);
        let lookup = ClientPrincipalHandle::new(Principals::default());
        lookup.0.set(ClientId(first), owner);
        lookup.0.set(ClientId(second), other_owner);
        let state = Arc::new(RwLock::new(lookup.clone()));
        let states = Arc::new(bmux_plugin::PluginStateRegistry::new());
        states.register(&state);
        let context = identity_service_context(first);
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::from_uuid(Uuid::from_u128(105)),
        );
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let bind = |caller| {
            let invocation = activation.begin_invocation().unwrap();
            let mut target = context.clone();
            target.caller_client_id = Some(caller);
            let port = bmux_plugin_sdk::BoundHostServicePort::new(
                invocation.clone(),
                Arc::new(IdentityHostDispatch(target, Arc::clone(&states))),
            );
            (
                invocation,
                bmux_plugin_sdk::PluginHostServices::from_bound(port),
            )
        };
        let mut unbound = bmux_plugin::ServiceCallerDispatchClient::new(&context);
        assert!(
            bmux_plugin::block_on_typed_dispatch(clients_identity::client::current_principal(
                &mut unbound
            ),)
            .is_err()
        );
        for (caller, expected) in [(first, owner), (second, other_owner)] {
            let mut invocation = context.clone();
            // Spoof the opposite connection in the consumer context: only the
            // trusted bound port may determine the provider's caller.
            invocation.caller_client_id = Some(if caller == first { second } else { first });
            let (scope, port) = bind(caller);
            invocation.host_service_port = Some(port);
            let mut client = bmux_plugin::ServiceCallerDispatchClient::new(&invocation);
            assert_eq!(
                bmux_plugin::block_on_typed_dispatch(clients_identity::client::current_principal(
                    &mut client
                ))
                .unwrap(),
                Ok(LocalPrincipal {
                    principal_id: expected
                }),
            );
            scope.close().unwrap();
            assert!(
                bmux_plugin::block_on_typed_dispatch(clients_identity::client::current_principal(
                    &mut client
                ),)
                .is_err()
            );
        }
        lookup.0.remove(ClientId(first));
        let (scope, port) = bind(first);
        let disconnected = context.clone().with_host_service_port(port);
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(&disconnected);
        assert_eq!(
            bmux_plugin::block_on_typed_dispatch(clients_identity::client::current_principal(
                &mut client
            ))
            .unwrap(),
            Err(IdentityError::PrincipalUnavailable),
        );
        scope.close().unwrap();
    }

    struct KernelCaller {
        provenance: Option<bmux_ipc::StreamProvenance>,
    }

    impl bmux_plugin::ServiceCaller for KernelCaller {
        fn call_service_raw(
            &self,
            _: &str,
            _: bmux_plugin_sdk::ServiceKind,
            _: &str,
            _: &str,
            _: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            unreachable!("the foundational facade uses only the kernel bridge")
        }

        fn execute_kernel_request(
            &self,
            request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            assert_eq!(request, bmux_ipc::Request::CurrentStreamProvenance);
            let provenance = self.provenance.clone().ok_or(
                bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "execute_kernel_request",
                },
            )?;
            Ok(bmux_ipc::ResponsePayload::CurrentStreamProvenance { provenance })
        }
    }

    #[test]
    fn provenance_facade_preserves_all_origins_and_denies_missing_bridge() {
        let subject_id = Uuid::from_u128(1);
        let connection_id = Uuid::from_u128(2);
        for (wire, expected) in [
            (bmux_ipc::StreamProvenance::Local, StreamProvenance::Local),
            (
                bmux_ipc::StreamProvenance::Admitted {
                    subject_id,
                    connection_id,
                },
                StreamProvenance::Admitted {
                    subject_id,
                    connection_id,
                },
            ),
            (
                bmux_ipc::StreamProvenance::Invalid,
                StreamProvenance::Invalid,
            ),
        ] {
            assert_eq!(
                current_stream_provenance(&KernelCaller {
                    provenance: Some(wire)
                }),
                Ok(expected)
            );
        }
        assert_eq!(
            current_stream_provenance(&KernelCaller { provenance: None }),
            Err(IdentityError::UnsupportedContext)
        );
    }

    #[test]
    fn generated_identity_contract_matches_manifest() {
        use bmux_clients_plugin_api::clients_identity;
        assert_eq!(
            clients_identity::INTERFACE_ID.as_str(),
            "clients-identity/v1"
        );
        assert!(
            crate::declaration::PLUGIN_MANIFEST.contains("interface_id = \"clients-identity/v1\"")
        );
        let value = LocalPrincipal {
            principal_id: Uuid::from_u128(7),
        };
        let encoded =
            bmux_plugin_sdk::encode_service_message(&Ok::<_, IdentityError>(value.clone()))
                .unwrap();
        let decoded: Result<LocalPrincipal, IdentityError> =
            bmux_plugin_sdk::decode_service_message(&encoded).unwrap();
        assert_eq!(decoded, Ok(value));
    }

    #[test]
    fn missing_caller_is_not_a_default_owner() {
        assert_eq!(
            current_principal(&bmux_plugin::PluginStateRegistry::new(), None),
            Err(IdentityError::NoCurrentClient)
        );
    }

    #[test]
    fn resolves_only_the_current_connection_and_rejects_disconnect() {
        let lookup = ClientPrincipalHandle::new(Principals::default());
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let owner = Uuid::from_u128(3);
        let other_owner = Uuid::from_u128(4);
        lookup.0.set(ClientId(first), owner);
        lookup.0.set(ClientId(second), other_owner);
        assert_eq!(
            resolve_principal(&lookup, first),
            Ok(LocalPrincipal {
                principal_id: owner
            })
        );
        assert_eq!(
            resolve_principal(&lookup, second),
            Ok(LocalPrincipal {
                principal_id: other_owner
            })
        );
        lookup.0.remove(ClientId(first));
        assert_eq!(
            resolve_principal(&lookup, first),
            Err(IdentityError::PrincipalUnavailable)
        );
    }

    #[test]
    fn missing_and_nil_principals_are_not_durable_owners() {
        let caller = Uuid::from_u128(1);
        assert_eq!(
            resolve_principal(&ClientPrincipalHandle::noop(), caller),
            Err(IdentityError::PrincipalUnavailable)
        );
        let lookup = ClientPrincipalHandle::new(Principals::default());
        lookup.0.set(ClientId(caller), Uuid::nil());
        assert_eq!(
            resolve_principal(&lookup, caller),
            Err(IdentityError::PrincipalUnavailable)
        );
    }
}
