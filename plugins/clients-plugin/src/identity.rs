//! Local caller identity resolution. This is not federated authentication.

use bmux_client_state::ClientPrincipalHandle;
use bmux_clients_plugin_api::clients_identity::{
    ClientsIdentityService, IdentityError, LocalPrincipal,
};
use bmux_session_models::ClientId;
use uuid::Uuid;

/// Bound only at invocation entry, never from a registration-time fallback connection.
pub struct CallerIdentity {
    caller: Option<Uuid>,
}

impl CallerIdentity {
    pub const fn new(caller: Option<Uuid>) -> Self {
        Self { caller }
    }
}

impl ClientsIdentityService for CallerIdentity {
    fn current_principal<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<LocalPrincipal, IdentityError>> + Send + 'a>,
    > {
        Box::pin(async move { current_principal(self.caller) })
    }
}

pub fn current_principal(caller_client_id: Option<Uuid>) -> Result<LocalPrincipal, IdentityError> {
    let caller = caller_client_id.ok_or(IdentityError::NoCurrentClient)?;
    let registry = bmux_plugin::global_plugin_state_registry();
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

    // TestServiceRouter fixes the error type to the SDK's existing PluginError.
    #[allow(clippy::result_large_err)]
    #[test]
    fn generated_client_reaches_native_caller_bound_provider() {
        use bmux_clients_plugin_api::clients_identity;
        use bmux_plugin_sdk::{
            ApiVersion, HostConnectionInfo, HostMetadata, HostScope, NativeServiceContext,
            ProviderId, RegisteredService, RustPlugin, ServiceKind, ServiceRequest,
        };
        use std::sync::{Arc, RwLock};

        let first = Uuid::from_u128(101);
        let second = Uuid::from_u128(102);
        let owner = Uuid::from_u128(103);
        let other_owner = Uuid::from_u128(104);
        let lookup = ClientPrincipalHandle::new(Principals::default());
        lookup.0.set(ClientId(first), owner);
        lookup.0.set(ClientId(second), other_owner);
        let state = Arc::new(RwLock::new(lookup.clone()));
        bmux_plugin::global_plugin_state_registry().register(&state);
        let context = NativeServiceContext {
            plugin_id: "bmux.clients".into(),
            request: ServiceRequest {
                caller_plugin_id: "test.consumer".into(),
                service: RegisteredService {
                    capability: HostScope::new("bmux.clients.read").unwrap(),
                    kind: ServiceKind::Query,
                    interface_id: clients_identity::INTERFACE_ID.as_str().into(),
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
        };
        let target = context.clone();
        let _router = bmux_plugin::test_support::install_test_service_router(Arc::new(
            move |_, caller, capability, kind, interface, operation, payload| {
                assert_eq!(capability, "bmux.clients.read");
                assert_eq!(kind, ServiceKind::Query);
                assert_eq!(interface, "clients-identity/v1");
                let mut invocation = target.clone();
                invocation.caller_client_id = caller;
                invocation.request.operation = operation.into();
                invocation.request.payload = payload;
                let response = crate::ClientsPlugin.invoke_service(invocation);
                assert!(response.error.is_none(), "{:?}", response.error);
                Ok(response.payload)
            },
        ));
        for (caller, expected) in [(first, owner), (second, other_owner)] {
            let mut invocation = context.clone();
            invocation.caller_client_id = Some(caller);
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
        }
        lookup.0.remove(ClientId(first));
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(&context);
        assert_eq!(
            bmux_plugin::block_on_typed_dispatch(clients_identity::client::current_principal(
                &mut client
            ))
            .unwrap(),
            Err(IdentityError::PrincipalUnavailable),
        );
    }

    #[test]
    fn generated_identity_contract_matches_manifest() {
        use bmux_clients_plugin_api::clients_identity;
        assert_eq!(
            clients_identity::INTERFACE_ID.as_str(),
            "clients-identity/v1"
        );
        assert!(include_str!("../plugin.toml").contains("interface_id = \"clients-identity/v1\""));
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
        assert_eq!(current_principal(None), Err(IdentityError::NoCurrentClient));
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
