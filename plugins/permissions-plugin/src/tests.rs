#[derive(Clone)]
struct InvocationTestHost(bmux_plugin::test_support::TestServiceRouter);

struct InvocationTestDispatch {
    router: bmux_plugin::test_support::TestServiceRouter,
    caller: Option<Uuid>,
    plugin: &'static str,
}

impl bmux_plugin_sdk::HostServiceDispatch for InvocationTestDispatch {
    // The host dispatch contract requires the SDK's rich error type.
    #[allow(clippy::result_large_err)]
    fn invoke(
        &self,
        _: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        (self.router)(
            self.plugin,
            self.caller,
            request.capability,
            request.kind,
            request.interface,
            request.operation,
            request.payload.to_vec(),
        )
    }
}

impl InvocationTestHost {
    fn port(
        &self,
        plugin: &'static str,
        caller: Option<Uuid>,
    ) -> bmux_plugin_sdk::PluginHostServices {
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::from_uuid(Uuid::new_v4()),
        );
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        bmux_plugin_sdk::PluginHostServices::from_bound(bmux_plugin_sdk::BoundHostServicePort::new(
            invocation,
            std::sync::Arc::new(InvocationTestDispatch {
                router: self.0.clone(),
                caller,
                plugin,
            }),
        ))
    }
}

fn checked_test_router(
    router: bmux_plugin::test_support::TestServiceRouter,
) -> bmux_plugin::test_support::TestServiceRouter {
    std::sync::Arc::new(
        // The test router ABI returns PluginError by value.
        #[allow(clippy::result_large_err)]
        move |caller, client, capability, kind, interface, operation, payload| {
            if !bmux_plugin_sdk::service_contract::is_checked(&payload) {
                return router(
                    caller, client, capability, kind, interface, operation, payload,
                );
            }
            let contract = <bmux_pane_runtime_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface)
            .or_else(|| <bmux_clients_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface))
                .ok_or(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    // This host does not provide sessions queries. Name
                    // resolution must observe an unavailable authority,
                    // not panic or dispatch an unchecked request.
                    operation: "test_router_wrong_target_query",
                })?;
            let request =
                bmux_plugin_sdk::service_contract::decode_for(interface, contract, &payload)?
                    .to_vec();
            let response = router(
                caller, client, capability, kind, interface, operation, request,
            )?;
            Ok(bmux_plugin_sdk::service_contract::encode_response(
                &response,
            ))
        },
    )
}

use super::*;
use bmux_plugin::ServiceCaller;
use bmux_plugin::test_support::TestServiceRouter;
use bmux_plugin_sdk::{
    ApiVersion, HostConnectionInfo, HostMetadata, HostScope, NativeServiceContext, ProviderId,
    RegisteredService, ServiceKind, ServiceRequest,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct MockHost {
    sessions: Vec<SessionSummary>,
    contexts: Vec<ContextSummary>,
    selected_session_id: Option<Uuid>,
    storage: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MockHost {
    fn with_session(id: Uuid, name: &str) -> Self {
        Self {
            sessions: vec![SessionSummary {
                id,
                name: Some(name.to_string()),
                client_count: 1,
            }],
            contexts: vec![ContextSummary {
                id: Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb),
                name: Some("default".to_string()),
                attributes: BTreeMap::new(),
            }],
            selected_session_id: Some(id),
            storage: Mutex::new(BTreeMap::new()),
        }
    }

    fn with_sessions(sessions: Vec<SessionSummary>) -> Self {
        Self {
            contexts: vec![ContextSummary {
                id: Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb),
                name: Some("default".to_string()),
                attributes: BTreeMap::new(),
            }],
            selected_session_id: sessions.first().map(|session| session.id),
            sessions,
            storage: Mutex::new(BTreeMap::new()),
        }
    }

    fn with_sessions_and_selected(
        sessions: Vec<SessionSummary>,
        selected_session_id: Option<Uuid>,
    ) -> Self {
        Self {
            sessions,
            contexts: vec![ContextSummary {
                id: Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb),
                name: Some("default".to_string()),
                attributes: BTreeMap::new(),
            }],
            selected_session_id,
            storage: Mutex::new(BTreeMap::new()),
        }
    }
}

impl ServiceCaller for MockHost {
    fn call_service_raw(
        &self,
        _capability: &str,
        _kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        let checked = bmux_plugin_sdk::service_contract::is_checked(&payload);
        let payload = if checked {
            let contract = <bmux_sessions_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface_id).or_else(|| <bmux_contexts_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface_id)).or_else(|| <bmux_clients_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface_id)).expect("generated mock contract");
            bmux_plugin_sdk::service_contract::decode_for(interface_id, contract, &payload)?
                .to_vec()
        } else {
            payload
        };
        // This mock preserves the ServiceCaller error ABI by value.
        #[allow(clippy::result_large_err)]
        let mock_response = || -> bmux_plugin_sdk::Result<Vec<u8>> {
            match (interface_id, operation) {
                // Typed host surfaces used by permissions-plugin's
                // domain helpers. Keep test fixtures aligned with the
                // production plugin-api contracts so legacy host
                // interfaces do not linger in tests.
                ("storage-query/v1", "get") => {
                    let request: StorageGetRequest = decode_service_message(&payload)?;
                    let value = self
                        .storage
                        .lock()
                        .expect("storage lock should succeed")
                        .get(request.key.as_str())
                        .cloned();
                    encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value })
                }
                ("storage-command/v1", "set") => {
                    let request: StorageSetRequest = decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .expect("storage lock should succeed")
                        .insert(request.key.to_string(), request.value);
                    encode_service_message(&())
                }
                ("contexts-state", "list-contexts") => encode_service_message(&self.contexts),
                ("contexts-state", "current-context") => {
                    let current = self.contexts.first().cloned();
                    encode_service_message(&current)
                }
                ("sessions-state", "list-sessions") => {
                    #[derive(serde::Serialize)]
                    struct Entry {
                        id: Uuid,
                        name: Option<String>,
                        client_count: u32,
                    }
                    let entries: Vec<Entry> = self
                        .sessions
                        .iter()
                        .map(|s| Entry {
                            id: s.id,
                            name: s.name.clone(),
                            client_count: u32::try_from(s.client_count).unwrap_or(0),
                        })
                        .collect();
                    encode_service_message(&entries)
                }
                ("clients-state", "current-client") => {
                    let summary = bmux_clients_plugin_api::clients_state::ClientSummary {
                        id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
                        selected_session_id: self.selected_session_id,
                        selected_context_id: None,
                        following_client_id: None,
                        following_global: false,
                    };
                    let result: std::result::Result<
                        bmux_clients_plugin_api::clients_state::ClientSummary,
                        bmux_clients_plugin_api::clients_state::ClientQueryError,
                    > = Ok(summary);
                    encode_service_message(&result)
                }
                _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "mock_service",
                }),
            }
        };
        let response = mock_response()?;
        Ok(if checked {
            bmux_plugin_sdk::service_contract::encode_response(&response)
        } else {
            response
        })
    }

    fn execute_kernel_request(
        &self,
        _request: bmux_ipc::Request,
    ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "mock_execute_kernel_request",
        })
    }
}

fn service_test_session_id() -> Uuid {
    Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa)
}

// Test router signature is fixed by bmux_plugin::test_support and returns
// bmux_plugin_sdk::Result, whose PluginError is intentionally rich.
#[allow(clippy::result_large_err)]
fn install_service_test_router() -> InvocationTestHost {
    install_authorization_router(Arc::new(Mutex::new(bmux_plugin::StreamOrigin::Local)))
}

#[allow(clippy::result_large_err)] // Test router uses the SDK error contract.
fn install_authorization_router(
    provenance: Arc<Mutex<bmux_plugin::StreamOrigin>>,
) -> InvocationTestHost {
    let storage = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
    let router_storage = Arc::clone(&storage);
    let router: TestServiceRouter = Arc::new(
        move |_caller_plugin,
              _caller_client,
              _capability,
              _kind,
              interface_id,
              operation,
              payload| {
            match (interface_id, operation) {
                (
                    bmux_plugin_sdk::CORE_INVOCATION_INTERFACE_V1,
                    bmux_plugin_sdk::CORE_INVOCATION_STREAM_ORIGIN_OPERATION_V1,
                ) => encode_service_message(&provenance.lock().unwrap().clone()),
                ("storage-query/v1", "get") => {
                    let request: StorageGetRequest = decode_service_message(&payload)?;
                    let value = router_storage
                        .lock()
                        .expect("storage lock should succeed")
                        .get(request.key.as_str())
                        .cloned();
                    encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value })
                }
                ("storage-command/v1", "set") => {
                    let request: StorageSetRequest = decode_service_message(&payload)?;
                    router_storage
                        .lock()
                        .expect("storage lock should succeed")
                        .insert(request.key.to_string(), request.value);
                    encode_service_message(&())
                }
                ("pane-runtime-commands", "set-client-write-permission") => {
                    encode_service_message(&Ok::<_, pane_runtime_commands::PaneCommandError>(0_u8))
                }
                _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "test_router",
                }),
            }
        },
    );
    InvocationTestHost(checked_test_router(router))
}

fn service_test_caller_id(interface_id: &str, operation: &str, payload: &[u8]) -> Uuid {
    if interface_id == "session-policy-state" && operation == "check" {
        decode_service_message::<SessionPolicyCheckRequest>(payload)
            .map_or_else(|_| Uuid::new_v4(), |req| req.client_id)
    } else {
        Uuid::new_v4()
    }
}

fn service_test_capability(capability: &str) -> HostScope {
    HostScope::new(capability).expect("capability should parse")
}

fn service_test_context(
    host: &InvocationTestHost,
    interface_id: &str,
    operation: &str,
    payload: Vec<u8>,
    capability: &str,
    kind: ServiceKind,
    data_dir: &Path,
) -> NativeServiceContext {
    let host_services = vec![
        RegisteredService {
            capability: service_test_capability("bmux.connections.pairing"),
            kind: ServiceKind::Query,
            interface_id: "pairing-state".into(),
            provider: ProviderId::Plugin("bmux.connections".into()),
        },
        RegisteredService {
            capability: HostScope::new("bmux.sessions.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "sessions-state".to_string(),
            provider: ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.clients.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "clients-state".to_string(),
            provider: ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.contexts.read").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "contexts-state".to_string(),
            provider: ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.storage").expect("capability should parse"),
            kind: ServiceKind::Query,
            interface_id: "storage-query/v1".to_string(),
            provider: ProviderId::Host,
        },
        RegisteredService {
            capability: HostScope::new("bmux.storage").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "storage-command/v1".to_string(),
            provider: ProviderId::Host,
        },
        RegisteredService {
            capability: service_test_capability("bmux.pane_runtime.write"),
            kind: ServiceKind::Command,
            interface_id: "pane-runtime-commands".to_string(),
            provider: ProviderId::Plugin("bmux.pane-runtime".to_string()),
        },
    ];

    let caller_client_id = Some(service_test_caller_id(interface_id, operation, &payload));
    NativeServiceContext {
        host_service_port: Some(host.port("bmux.permissions", caller_client_id)),
        plugin_id: "bmux.permissions".to_string(),
        request: ServiceRequest {
            caller_plugin_id: "test.caller".to_string(),
            service: RegisteredService {
                capability: HostScope::new(capability).expect("capability should parse"),
                kind,
                interface_id: interface_id.to_string(),
                provider: ProviderId::Plugin("bmux.permissions".to_string()),
            },
            operation: operation.to_string(),
            payload,
        },
        required_capabilities: vec![
            "bmux.commands".to_string(),
            "bmux.sessions.read".to_string(),
            "bmux.clients.read".to_string(),
            "bmux.pane_runtime.write".to_string(),
            "bmux.storage".to_string(),
        ],
        provided_capabilities: vec![
            "bmux.permissions.read".to_string(),
            "bmux.permissions.write".to_string(),
            "bmux.sessions.policy".to_string(),
        ],
        services: host_services,
        available_capabilities: vec![
            "bmux.sessions.read".to_string(),
            "bmux.clients.read".to_string(),
            "bmux.pane_runtime.write".to_string(),
            "bmux.storage".to_string(),
        ],
        enabled_plugins: vec!["bmux.permissions".to_string()],
        plugin_search_roots: vec!["/plugins".to_string()],
        host: HostMetadata {
            product_name: "bmux".to_string(),
            product_version: "0.1.0".to_string(),
            plugin_api_version: ApiVersion::new(1, 0),
            plugin_abi_version: ApiVersion::new(1, 0),
        },
        connection: HostConnectionInfo {
            config_dir: "/config".to_string(),
            config_dir_candidates: vec!["/config".to_string()],
            runtime_dir: "/runtime".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
            state_dir: "/state".to_string(),
        },
        settings: None,
        plugin_settings_map: std::collections::BTreeMap::new(),
        caller_client_id,
        cancellation: bmux_plugin_sdk::CancellationToken::default(),
        host_kernel_bridge: None,
    }
}

#[test]
fn service_fixture_storage_uses_only_its_explicit_host_port() {
    let first_host = install_service_test_router();
    let second_host = install_service_test_router();
    let first = service_test_context(
        &first_host,
        "permissions-commands",
        "grant",
        Vec::new(),
        "bmux.permissions.write",
        ServiceKind::Command,
        Path::new("test-host-one"),
    );
    let second = service_test_context(
        &second_host,
        "permissions-commands",
        "grant",
        Vec::new(),
        "bmux.permissions.write",
        ServiceKind::Command,
        Path::new("test-host-two"),
    );
    let request = StorageGetRequest::new(bmux_plugin_sdk::storage_key!("permissions-v1"));
    first
        .storage_set(&StorageSetRequest::new(
            bmux_plugin_sdk::storage_key!("permissions-v1"),
            b"first-host".to_vec(),
        ))
        .unwrap();
    assert_eq!(
        first.storage_get(&request).unwrap().value,
        Some(b"first-host".to_vec())
    );
    assert_eq!(second.storage_get(&request).unwrap().value, None);
    // Removing the port must not discover the other live fixture or fall
    // back to any process-global router, even for a local caller.
    let mut unbound = first;
    unbound.host_service_port = None;
    let error = unbound.storage_get(&request).unwrap_err();
    assert!(error.to_string().contains("host service port not selected"));
    assert_eq!(second.storage_get(&request).unwrap().value, None);
}

#[test]
fn provenance_fixture_queries_are_isolated_without_kernel_authority() {
    let local =
        install_authorization_router(Arc::new(Mutex::new(bmux_plugin::StreamOrigin::Local)));
    let invalid =
        install_authorization_router(Arc::new(Mutex::new(bmux_plugin::StreamOrigin::Invalid)));
    for (host, expected) in [
        (&local, bmux_plugin::StreamOrigin::Local),
        (&invalid, bmux_plugin::StreamOrigin::Invalid),
    ] {
        let context = service_test_context(
            host,
            "permissions-state",
            "list-permissions",
            Vec::new(),
            "bmux.permissions.read",
            ServiceKind::Query,
            Path::new("provenance-fixture"),
        );
        assert_eq!(caller_provenance(&context).unwrap(), expected);
        assert!(
            context
                .host_service_port
                .as_ref()
                .unwrap()
                .execute_kernel_encoded(b"no kernel grant")
                .is_err()
        );
    }
}

fn service_test_data_dir() -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bmux-permissions-service-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("test data dir should be creatable");
    dir
}

#[test]
fn resolve_current_session_uses_selected_session_name() {
    let alpha_id = Uuid::new_v4();
    let beta_id = Uuid::new_v4();
    let host = MockHost::with_sessions_and_selected(
        vec![
            SessionSummary {
                id: alpha_id,
                name: Some("alpha".to_string()),
                client_count: 1,
            },
            SessionSummary {
                id: beta_id,
                name: Some("beta".to_string()),
                client_count: 1,
            },
        ],
        Some(beta_id),
    );
    let session = resolve_current_session(&host).expect("active session should resolve");
    assert_eq!(session, "beta");
}

#[test]
fn resolve_current_session_falls_back_when_selected_session_missing() {
    let alpha_id = Uuid::new_v4();
    let host = MockHost::with_sessions_and_selected(
        vec![SessionSummary {
            id: alpha_id,
            name: Some("alpha".to_string()),
            client_count: 1,
        }],
        Some(Uuid::new_v4()),
    );
    let session =
        resolve_current_session(&host).expect("fallback to first listed session should work");
    assert_eq!(session, "alpha");
}

#[test]
fn resolve_current_session_requires_available_sessions() {
    let host = MockHost::with_sessions(Vec::new());
    let error = resolve_current_session(&host)
        .expect_err("missing sessions should produce an actionable error");
    assert!(error.contains("no active session"));
}

#[test]
fn observer_role_denies_mutation_and_admin() {
    let session_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");

    grant_entry(
        &host,
        GrantRequest {
            session: "alpha".to_string(),
            client_id: client_id.to_string(),
            role: "observer".to_string(),
        },
    )
    .expect("grant should succeed");

    let mutation = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "mutation".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");
    assert!(!mutation.allowed);

    let admin = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "admin".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");
    assert!(!admin.allowed);
}

#[test]
fn pane_mutation_actions_are_explicit_and_role_gated() {
    for action in [
        "pane.split",
        "pane.launch",
        "pane.focus",
        "pane.resize",
        "pane.close",
        "pane.restart",
        "pane.zoom",
        "pane.floating.create",
        "pane.floating.focus",
        "pane.floating.move",
        "pane.floating.resize",
        "pane.floating.raise",
        "pane.floating.lower",
        "pane.floating.set_z",
        "pane.floating.set_layer",
    ] {
        for role in ["owner", "writer", "observer"] {
            assert_eq!(
                evaluate_role_action(role, action).allowed,
                role != "observer",
                "{role}: {action}"
            );
        }
    }
    for action in [
        "pane.arbitrary",
        "pane.floating.arbitrary",
        "pane.zoom.extra",
    ] {
        for role in ["owner", "writer", "observer"] {
            assert!(
                !evaluate_role_action(role, action).allowed,
                "{role}: {action}"
            );
        }
    }
}

#[test]
fn writer_role_allows_mutation_but_denies_admin() {
    let session_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");

    grant_entry(
        &host,
        GrantRequest {
            session: "alpha".to_string(),
            client_id: client_id.to_string(),
            role: "writer".to_string(),
        },
    )
    .expect("grant should succeed");

    let mutation = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "mutation".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");
    assert!(mutation.allowed);

    let admin = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "admin".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");
    assert!(!admin.allowed);
}

#[test]
fn owner_role_allows_admin() {
    let session_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");

    grant_entry(
        &host,
        GrantRequest {
            session: "alpha".to_string(),
            client_id: client_id.to_string(),
            role: "owner".to_string(),
        },
    )
    .expect("grant should succeed");

    let admin = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "admin".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");
    assert!(admin.allowed);
}

#[test]
fn observer_role_denies_granular_mutation_action() {
    let session_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");

    grant_entry(
        &host,
        GrantRequest {
            session: "alpha".to_string(),
            client_id: client_id.to_string(),
            role: "observer".to_string(),
        },
    )
    .expect("grant should succeed");

    let decision = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "context.close".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");
    assert!(!decision.allowed);
}

#[test]
fn writer_role_allows_granular_mutation_action() {
    let session_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");

    grant_entry(
        &host,
        GrantRequest {
            session: "alpha".to_string(),
            client_id: client_id.to_string(),
            role: "writer".to_string(),
        },
    )
    .expect("grant should succeed");

    let decision = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "context.select".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");
    assert!(decision.allowed);
}

#[test]
fn policy_rejects_legacy_alias_action_names() {
    let session_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");

    grant_entry(
        &host,
        GrantRequest {
            session: "alpha".to_string(),
            client_id: client_id.to_string(),
            role: "writer".to_string(),
        },
    )
    .expect("grant should succeed");

    let decision = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "pane_split".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");

    assert!(!decision.allowed);
    assert!(
        decision
            .reason
            .is_some_and(|reason| reason.contains("invalid session policy action"))
    );
}

#[test]
fn missing_entry_defaults_to_allow() {
    let session_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");

    let decision = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "mutation".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");
    assert!(decision.allowed);
}

fn remote_policy_context(
    router: &InvocationTestHost,
    data_dir: &Path,
    action: &str,
) -> NativeServiceContext {
    service_test_context(
        router,
        "session-policy-state",
        "check",
        encode_service_message(&SessionPolicyCheckRequest {
            session_id: service_test_session_id(),
            client_id: Uuid::new_v4(),
            principal_id: Uuid::new_v4(),
            action: action.into(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        })
        .unwrap(),
        "bmux.sessions.policy",
        ServiceKind::Query,
        data_dir,
    )
}

fn remote_policy_allowed(response: &ServiceResponse) -> bool {
    decode_service_message::<SessionPolicyCheckResponse>(&response.payload)
        .unwrap()
        .allowed
}

// One scenario walks the role lifecycle; splitting it would repeat the setup.
#[allow(clippy::too_many_lines)]
#[test]
fn remote_roles_are_owner_local_persisted_and_drive_remote_policy() {
    use bmux_permissions_plugin_api::remote_role_types::RemoteRole;
    use bmux_plugin::StreamOrigin as StreamProvenance;
    use bmux_plugin_sdk::RouteAccess;
    let origin = Arc::new(Mutex::new(StreamProvenance::Local));
    let router = install_authorization_router(Arc::clone(&origin));
    let plugin = PermissionsPlugin::default();
    let authorizer = plugin.remote.authorizer();
    let data_dir = service_test_data_dir();
    let subject = Uuid::new_v4();
    let admitted = || StreamProvenance::Admitted {
        subject_id: subject,
        connection_id: Uuid::new_v4(),
    };
    let set = |role: Option<RemoteRole>| {
        let (operation, payload) = role.map_or_else(
            || {
                (
                    "clear-remote-role",
                    encode_service_message(
                        &bmux_permissions_plugin_api::remote_role_commands::client::ClearRemoteRoleRequest {
                            principal_id: subject,
                        },
                    )
                    .unwrap(),
                )
            },
            |role| {
                (
                    "set-remote-role",
                    encode_service_message(
                        &bmux_permissions_plugin_api::remote_role_commands::client::SetRemoteRoleRequest {
                            principal_id: subject,
                            role,
                        },
                    )
                    .unwrap(),
                )
            },
        );
        service_test_context(
            &router,
            "remote-role-commands",
            operation,
            payload,
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        )
    };
    let policy = |action| remote_policy_context(&router, &data_dir, action);

    // Unknown remote principals get nothing.
    *origin.lock().unwrap() = admitted();
    assert!(
        plugin
            .invoke_service(&policy("pane.direct_input"))
            .error
            .is_some()
    );
    assert!(!authorizer.allows(subject, RouteAccess::Observe));
    // Only the local owner sets roles.
    assert_eq!(
        plugin
            .invoke_service(&set(Some(RemoteRole::Full)))
            .error
            .unwrap()
            .code,
        "admin_denied"
    );
    *origin.lock().unwrap() = StreamProvenance::Local;
    assert!(
        plugin
            .invoke_service(&set(Some(RemoteRole::Full)))
            .error
            .is_none()
    );
    assert!(authorizer.allows(subject, RouteAccess::Input));
    assert!(!authorizer.allows(subject, RouteAccess::OwnerLocal));
    *origin.lock().unwrap() = admitted();
    assert!(remote_policy_allowed(
        &plugin.invoke_service(&policy("pane.direct_input"))
    ));
    assert!(!remote_policy_allowed(
        &plugin.invoke_service(&policy("admin"))
    ));

    // View-only observes but cannot change anything.
    *origin.lock().unwrap() = StreamProvenance::Local;
    assert!(
        plugin
            .invoke_service(&set(Some(RemoteRole::ViewOnly)))
            .error
            .is_none()
    );
    assert!(authorizer.allows(subject, RouteAccess::Observe));
    assert!(!authorizer.allows(subject, RouteAccess::Input));
    *origin.lock().unwrap() = admitted();
    assert!(!remote_policy_allowed(
        &plugin.invoke_service(&policy("pane.direct_input"))
    ));

    // Roles persist through plugin storage: a fresh instance reads them back.
    let stored = remote_roles::RemoteRoles::load_for_test(&service_test_context(
        &router,
        "remote-role-state",
        "remote-role",
        Vec::new(),
        "bmux.permissions.read",
        ServiceKind::Query,
        &data_dir,
    ))
    .unwrap();
    assert_eq!(stored.get(&subject), Some(&RemoteRole::ViewOnly));

    *origin.lock().unwrap() = StreamProvenance::Local;
    assert!(plugin.invoke_service(&set(None)).error.is_none());
    assert!(!authorizer.allows(subject, RouteAccess::Observe));
    *origin.lock().unwrap() = StreamProvenance::Invalid;
    assert!(
        plugin
            .invoke_service(&policy("pane.direct_input"))
            .error
            .is_some()
    );
}

#[test]
fn absent_host_provenance_is_never_local_owner() {
    let host = MockHost::with_session(Uuid::new_v4(), "alpha");
    assert!(require_local_owner(&host).is_err());
}

#[test]
fn revoke_preserves_explicit_denial_in_policy_state() {
    let session_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");

    grant_entry(
        &host,
        GrantRequest {
            session: "alpha".to_string(),
            client_id: client_id.to_string(),
            role: "observer".to_string(),
        },
    )
    .expect("grant should succeed");
    revoke_entry(
        &host,
        &RevokeRequest {
            session: "alpha".to_string(),
            client_id: client_id.to_string(),
        },
    )
    .expect("revoke should succeed");

    let decision = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            client_id,
            principal_id: Uuid::new_v4(),
            action: "admin".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        },
    )
    .expect("policy evaluation should succeed");
    assert!(!decision.allowed);
}

/// Test-only selected callback authority: always live, never cancelled.
struct LiveTestAuthority;
impl bmux_plugin_sdk::PluginCallbackAuthoritySource for LiveTestAuthority {
    fn check_live(&self) -> bmux_plugin_sdk::definition::PluginResult<()> {
        Ok(())
    }
    fn enter_adapter_entry(
        &self,
        _: bmux_plugin_runtime::PluginInstanceId,
    ) -> bmux_plugin_sdk::definition::PluginResult<bmux_plugin_sdk::PluginCallbackEntryGuard> {
        Ok(bmux_plugin_sdk::PluginCallbackEntryGuard::from_host(()))
    }
    fn cancelled(&self) -> bmux_plugin_sdk::PluginCallbackSettlement {
        Box::pin(std::future::pending())
    }
}

/// Test-only selected executor. It validates the entered dependency and
/// runs the author callback on a dedicated worker thread, joining before
/// settling so the single-poll typed-dispatch bridge observes completion.
struct WorkerThreadTestExecutor;
impl bmux_plugin_sdk::PluginCallbackExecution for WorkerThreadTestExecutor {
    fn execute(
        &self,
        _: bmux_plugin_sdk::PluginCallbackAuthority,
        _: bmux_plugin_runtime::PluginInstanceId,
        _: bmux_plugin_sdk::PluginBlockingCallback,
    ) -> bmux_plugin_sdk::PluginCallbackSettlement {
        Box::pin(async {
            Err(PluginCommandError::unavailable(
                "test executor requires an entered dependency",
            ))
        })
    }
    fn execute_entered(
        &self,
        authority: bmux_plugin_sdk::PluginCallbackAuthority,
        instance: bmux_plugin_runtime::PluginInstanceId,
        entry: bmux_plugin_sdk::PluginCallbackEntryGuard,
        callback: bmux_plugin_sdk::PluginBlockingCallback,
    ) -> bmux_plugin_sdk::PluginCallbackSettlement {
        Box::pin(async move {
            entry.validate_for(&authority, instance)?;
            std::thread::spawn(move || {
                entry.validate_for(&authority, instance)?;
                let result = callback();
                drop(entry);
                result
            })
            .join()
            .map_err(|_| PluginCommandError::failed("test callback worker panicked"))?
        })
    }
}

fn selected_test_execution() -> bmux_plugin_sdk::PluginCallbackContext {
    bmux_plugin_sdk::PluginCallbackContext::from_host(
        bmux_plugin_sdk::PluginCallbackAuthority::from_host(Arc::new(LiveTestAuthority)),
        bmux_plugin_sdk::PluginCallbackExecutor::from_host(Arc::new(WorkerThreadTestExecutor)),
    )
}

#[test]
fn checked_instance_mutation_is_validated_and_applied_once() {
    use bmux_plugin_sdk::definition::{PluginCallContext, PluginInstance};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let host = install_service_test_router();
    let writes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&writes);
    let router = InvocationTestHost(Arc::new(
        // TestServiceRouter's fixed error type is PluginError.
        #[allow(clippy::result_large_err)]
        move |plugin, caller, capability, kind, interface, operation, payload| {
            if interface == "storage-command/v1" && operation == "set" {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            (host.0)(
                plugin, caller, capability, kind, interface, operation, payload,
            )
        },
    ));
    // Exercise the selected definition instance (the same instance the
    // `plugin!` factory produces), not the raw synchronous body.
    let plugin = bmux_plugin_sdk::author::DeclaredInstance::new(
        PermissionsPlugin::default(),
        crate::declaration::PLUGIN_MANIFEST,
        crate::declaration::SERVICE_CONTRACTS,
    )
    .unwrap()
    .for_instance(bmux_plugin_sdk::definition::LoadedInstanceId::from_uuid(
        Uuid::new_v4(),
    ));
    let body = encode_service_message(&GrantRequest {
        session: service_test_session_id().to_string(),
        client_id: Uuid::new_v4().to_string(),
        role: "observer".into(),
    })
    .unwrap();
    let contract = <bmux_permissions_plugin_api::Contract as bmux_plugin_sdk::PluginContract>
        ::service_contract_for("permissions-commands").unwrap();
    let context = |digest: &[u8]| {
        service_test_context(
            &router,
            "permissions-commands",
            "grant",
            bmux_plugin_sdk::service_contract::encode("permissions-commands", digest, &body)
                .unwrap(),
            "bmux.permissions.write",
            ServiceKind::Command,
            Path::new("checked-instance"),
        )
    };
    let invoke = |context: NativeServiceContext| {
        let call = PluginCallContext {
            callback_execution: Some(selected_test_execution()),
            invocation_id: context.cancellation.invocation_id.clone(),
            cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
            host_services: context.host_service_port.clone(),
        };
        bmux_plugin::block_on_typed_dispatch(plugin.invoke(call, context))
    };
    let mut wrong = contract.to_vec();
    wrong[0] ^= 1;
    assert!(invoke(context(&wrong)).is_err());
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    let response = invoke(context(contract)).unwrap();
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    let payload = bmux_plugin_sdk::service_contract::decode_response(&response.payload).unwrap();
    let ack: CommandAck = decode_service_message(payload).unwrap();
    assert!(ack.ok);
}

#[test]
fn invoke_service_grant_list_and_revoke_roundtrip() {
    let router = install_service_test_router();
    let plugin = PermissionsPlugin::default();
    let data_dir = service_test_data_dir();
    let client_id = Uuid::new_v4().to_string();
    let session_id = service_test_session_id().to_string();

    let grant_context = service_test_context(
        &router,
        "permissions-commands",
        "grant",
        encode_service_message(&GrantRequest {
            session: session_id.clone(),
            client_id: client_id.clone(),
            role: "observer".to_string(),
        })
        .expect("grant request should encode"),
        "bmux.permissions.write",
        ServiceKind::Command,
        &data_dir,
    );
    let grant = plugin.invoke_service(&grant_context);
    assert!(
        grant.error.is_none(),
        "unexpected grant error: {:?}",
        grant.error
    );

    let list_context = service_test_context(
        &router,
        "permissions-state",
        "list-permissions",
        encode_service_message(&ListPermissionsRequest {
            session: session_id.clone(),
        })
        .expect("list request should encode"),
        "bmux.permissions.read",
        ServiceKind::Query,
        &data_dir,
    );
    let listed = plugin.invoke_service(&list_context);
    assert!(
        listed.error.is_none(),
        "unexpected list error: {:?}",
        listed.error
    );
    let listed_payload: Vec<PermissionEntry> =
        decode_service_message(&listed.payload).expect("list response should decode");
    assert_eq!(listed_payload.len(), 1);
    assert_eq!(listed_payload[0].client_id, client_id);
    assert_eq!(listed_payload[0].role, "observer");

    let revoke_context = service_test_context(
        &router,
        "permissions-commands",
        "revoke",
        encode_service_message(&RevokeRequest {
            session: session_id.clone(),
            client_id: listed_payload[0].client_id.clone(),
        })
        .expect("revoke request should encode"),
        "bmux.permissions.write",
        ServiceKind::Command,
        &data_dir,
    );
    let revoke = plugin.invoke_service(&revoke_context);
    assert!(
        revoke.error.is_none(),
        "unexpected revoke error: {:?}",
        revoke.error
    );

    let relist_context = service_test_context(
        &router,
        "permissions-state",
        "list-permissions",
        encode_service_message(&ListPermissionsRequest {
            session: session_id,
        })
        .expect("list request should encode"),
        "bmux.permissions.read",
        ServiceKind::Query,
        &data_dir,
    );
    let relisted = plugin.invoke_service(&relist_context);
    assert!(
        relisted.error.is_none(),
        "unexpected relist error: {:?}",
        relisted.error
    );
    let relisted_payload: Vec<PermissionEntry> =
        decode_service_message(&relisted.payload).expect("relist response should decode");
    assert_eq!(relisted_payload.len(), 1);
    assert_eq!(relisted_payload[0].role, "revoked");
}

#[test]
fn invoke_service_policy_check_denies_observer_mutation() {
    let router = install_service_test_router();
    let plugin = PermissionsPlugin::default();
    let data_dir = service_test_data_dir();
    let client_id = Uuid::new_v4();
    let session_id = service_test_session_id().to_string();

    let grant_context = service_test_context(
        &router,
        "permissions-commands",
        "grant",
        encode_service_message(&GrantRequest {
            session: session_id,
            client_id: client_id.to_string(),
            role: "observer".to_string(),
        })
        .expect("grant request should encode"),
        "bmux.permissions.write",
        ServiceKind::Command,
        &data_dir,
    );
    let grant = plugin.invoke_service(&grant_context);
    assert!(
        grant.error.is_none(),
        "unexpected grant error: {:?}",
        grant.error
    );

    let policy_context = service_test_context(
        &router,
        "session-policy-state",
        "check",
        encode_service_message(&SessionPolicyCheckRequest {
            session_id: service_test_session_id(),
            client_id,
            principal_id: Uuid::new_v4(),
            action: "mutation".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        })
        .expect("policy request should encode"),
        "bmux.sessions.policy",
        ServiceKind::Query,
        &data_dir,
    );
    let policy = plugin.invoke_service(&policy_context);
    assert!(
        policy.error.is_none(),
        "unexpected policy error: {:?}",
        policy.error
    );
    let decision: SessionPolicyCheckResponse =
        decode_service_message(&policy.payload).expect("policy response should decode");
    assert!(!decision.allowed);
    assert!(decision.reason.is_some());
}

#[test]
fn invoke_service_grant_rejects_invalid_role() {
    let router = install_service_test_router();
    let plugin = PermissionsPlugin::default();
    let data_dir = service_test_data_dir();
    let context = service_test_context(
        &router,
        "permissions-commands",
        "grant",
        encode_service_message(&GrantRequest {
            session: "alpha".to_string(),
            client_id: Uuid::new_v4().to_string(),
            role: "invalid".to_string(),
        })
        .expect("grant request should encode"),
        "bmux.permissions.write",
        ServiceKind::Command,
        &data_dir,
    );

    let response = plugin.invoke_service(&context);
    let error = response.error.expect("expected grant failure");
    assert_eq!(error.code, "grant_failed");
    assert!(error.message.contains("invalid role"));
}

#[test]
fn invoke_service_rejects_invalid_grant_payload() {
    let router = install_service_test_router();
    let plugin = PermissionsPlugin::default();
    let data_dir = service_test_data_dir();
    let context = service_test_context(
        &router,
        "permissions-commands",
        "grant",
        vec![1, 2, 3],
        "bmux.permissions.write",
        ServiceKind::Command,
        &data_dir,
    );

    let response = plugin.invoke_service(&context);
    let error = response.error.expect("expected invalid request error");
    assert_eq!(error.code, "invalid_request");
}

#[test]
fn invoke_service_list_reports_missing_session() {
    let router = install_service_test_router();
    let plugin = PermissionsPlugin::default();
    let data_dir = service_test_data_dir();
    let context = service_test_context(
        &router,
        "permissions-state",
        "list-permissions",
        encode_service_message(&ListPermissionsRequest {
            session: "missing-session".to_string(),
        })
        .expect("list request should encode"),
        "bmux.permissions.read",
        ServiceKind::Query,
        &data_dir,
    );

    let response = plugin.invoke_service(&context);
    let error = response.error.expect("expected list failure");
    assert_eq!(error.code, "list_failed");
    assert!(error.message.contains("not found"));
}

#[test]
fn invoke_service_policy_defaults_to_allow_without_entry() {
    let router = install_service_test_router();
    let plugin = PermissionsPlugin::default();
    let data_dir = service_test_data_dir();
    let context = service_test_context(
        &router,
        "session-policy-state",
        "check",
        encode_service_message(&SessionPolicyCheckRequest {
            session_id: service_test_session_id(),
            client_id: Uuid::new_v4(),
            principal_id: Uuid::new_v4(),
            action: "mutation".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        })
        .expect("policy request should encode"),
        "bmux.sessions.policy",
        ServiceKind::Query,
        &data_dir,
    );

    let response = plugin.invoke_service(&context);
    assert!(
        response.error.is_none(),
        "unexpected policy error: {:?}",
        response.error
    );
    let decision: SessionPolicyCheckResponse =
        decode_service_message(&response.payload).expect("policy response should decode");
    assert!(decision.allowed);
    assert!(decision.reason.is_none());
}

#[test]
fn invoke_service_rejects_invalid_policy_payload() {
    let router = install_service_test_router();
    let plugin = PermissionsPlugin::default();
    let data_dir = service_test_data_dir();
    let context = service_test_context(
        &router,
        "session-policy-state",
        "check",
        vec![1, 2, 3],
        "bmux.sessions.policy",
        ServiceKind::Query,
        &data_dir,
    );

    let response = plugin.invoke_service(&context);
    let error = response.error.expect("expected invalid request");
    assert_eq!(error.code, "invalid_request");
}

#[test]
fn invoke_service_rejects_unsupported_operation() {
    let router = install_service_test_router();
    let plugin = PermissionsPlugin::default();
    let data_dir = service_test_data_dir();
    let context = service_test_context(
        &router,
        "permissions-commands",
        "unknown",
        Vec::new(),
        "bmux.permissions.write",
        ServiceKind::Command,
        &data_dir,
    );

    let response = plugin.invoke_service(&context);
    let error = response
        .error
        .expect("expected unsupported operation error");
    assert_eq!(error.code, "unsupported_service_operation");
}

#[test]
fn session_creation_uses_mutation_role_policy() {
    assert_eq!(
        classify_action("session.create"),
        PolicyActionKind::Mutation
    );
    for (role, allowed) in [("owner", true), ("writer", true), ("observer", false)] {
        let decision = evaluate_role_action(role, "session.create");
        assert_eq!(decision.allowed, allowed, "creation decision for {role}");
        assert_eq!(decision.reason.is_some(), !allowed);
        assert_eq!(
            decision.allowed,
            evaluate_role_action(role, "context.create").allowed,
            "session and context creation must share mutation semantics"
        );
        assert!(!evaluate_role_action(role, "session.create.arbitrary").allowed);
        assert!(!evaluate_role_action(role, "unknown-action").allowed);
    }
    assert!(!evaluate_role_action("unknown-role", "session.create").allowed);
}

#[test]
fn invoke_service_policy_denies_unknown_action() {
    let router = install_service_test_router();
    let plugin = PermissionsPlugin::default();
    let data_dir = service_test_data_dir();
    let client_id = Uuid::new_v4();
    let session_id = service_test_session_id().to_string();

    let grant_context = service_test_context(
        &router,
        "permissions-commands",
        "grant",
        encode_service_message(&GrantRequest {
            session: session_id,
            client_id: client_id.to_string(),
            role: "observer".to_string(),
        })
        .expect("grant request should encode"),
        "bmux.permissions.write",
        ServiceKind::Command,
        &data_dir,
    );
    let grant = plugin.invoke_service(&grant_context);
    assert!(
        grant.error.is_none(),
        "unexpected grant error: {:?}",
        grant.error
    );

    let context = service_test_context(
        &router,
        "session-policy-state",
        "check",
        encode_service_message(&SessionPolicyCheckRequest {
            session_id: service_test_session_id(),
            client_id,
            principal_id: Uuid::new_v4(),
            action: "unknown-action".to_string(),
            context_id: None,
            plugin_id: None,
            capability: None,
            execution_class: None,
        })
        .expect("policy request should encode"),
        "bmux.sessions.policy",
        ServiceKind::Query,
        &data_dir,
    );

    let response = plugin.invoke_service(&context);
    assert!(
        response.error.is_none(),
        "unexpected policy error: {:?}",
        response.error
    );
    let decision: SessionPolicyCheckResponse =
        decode_service_message(&response.payload).expect("policy response should decode");
    assert!(!decision.allowed);
    assert!(
        decision
            .reason
            .is_some_and(|reason| reason.contains("invalid session policy action"))
    );
}

#[test]
fn hot_path_execution_denies_interpreter_without_override() {
    let session_id = Uuid::new_v4();
    let context_id = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb);
    let host = MockHost::with_session(session_id, "alpha");
    let decision = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            context_id: Some(context_id),
            client_id: Uuid::new_v4(),
            principal_id: Uuid::new_v4(),
            action: "hot_path_execution".to_string(),
            plugin_id: Some("example.interpreter".to_string()),
            capability: Some("bmux.terminal.input_intercept".to_string()),
            execution_class: Some("interpreter".to_string()),
        },
    )
    .expect("policy evaluation should succeed");
    assert!(!decision.allowed);
}

#[test]
fn hot_path_execution_allows_with_scoped_override() {
    let session_id = Uuid::new_v4();
    let context_id = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb);
    let host = MockHost::with_session(session_id, "alpha");
    grant_hot_path_override(
        &host,
        GrantHotPathOverrideRequest {
            plugin_id: "example.interpreter".to_string(),
            capability: "bmux.terminal.input_intercept".to_string(),
            execution_class: "interpreter".to_string(),
            scope: "session_context".to_string(),
            session: Some("alpha".to_string()),
            context: Some("default".to_string()),
        },
    )
    .expect("override grant should succeed");

    let decision = evaluate_policy(
        &host,
        &SessionPolicyCheckRequest {
            session_id,
            context_id: Some(context_id),
            client_id: Uuid::new_v4(),
            principal_id: Uuid::new_v4(),
            action: "hot_path_execution".to_string(),
            plugin_id: Some("example.interpreter".to_string()),
            capability: Some("bmux.terminal.input_intercept".to_string()),
            execution_class: Some("interpreter".to_string()),
        },
    )
    .expect("policy evaluation should succeed");
    assert!(decision.allowed);
}

#[test]
fn inspect_hot_path_decision_reports_precedence_scope() {
    let session_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");
    grant_hot_path_override(
        &host,
        GrantHotPathOverrideRequest {
            plugin_id: "example.interpreter".to_string(),
            capability: "bmux.terminal.input_intercept".to_string(),
            execution_class: "interpreter".to_string(),
            scope: "global".to_string(),
            session: None,
            context: None,
        },
    )
    .expect("global grant should succeed");
    grant_hot_path_override(
        &host,
        GrantHotPathOverrideRequest {
            plugin_id: "example.interpreter".to_string(),
            capability: "bmux.terminal.input_intercept".to_string(),
            execution_class: "interpreter".to_string(),
            scope: "session".to_string(),
            session: Some("alpha".to_string()),
            context: None,
        },
    )
    .expect("session grant should succeed");

    let decision = inspect_hot_path_decision(
        &host,
        &CheckHotPathDecisionRequest {
            plugin_id: "example.interpreter".to_string(),
            capability: "bmux.terminal.input_intercept".to_string(),
            execution_class: "interpreter".to_string(),
            session: Some("alpha".to_string()),
            context: None,
        },
    )
    .expect("decision should resolve");
    assert!(decision.allowed);
    assert_eq!(decision.matched_scope.as_deref(), Some("session"));
}

#[test]
fn inspect_hot_path_decision_allows_native_fast_without_override() {
    let session_id = Uuid::new_v4();
    let host = MockHost::with_session(session_id, "alpha");
    let decision = inspect_hot_path_decision(
        &host,
        &CheckHotPathDecisionRequest {
            plugin_id: "example.fast".to_string(),
            capability: "bmux.terminal.output_intercept".to_string(),
            execution_class: "native_fast".to_string(),
            session: Some("alpha".to_string()),
            context: None,
        },
    )
    .expect("decision should resolve");
    assert!(decision.allowed);
    assert!(decision.matched_scope.is_none());
}

#[test]
fn compact_hot_path_decision_format_includes_reason_for_denies() {
    let line = format_hot_path_decision_compact(&HotPathDecisionResponse {
        allowed: false,
        reason: Some("denied by policy".to_string()),
        matched_scope: None,
        session_id: Some(Uuid::from_u128(1)),
        context_id: None,
    });
    assert!(line.contains("deny"));
    assert!(line.contains("reason=denied by policy"));
}

#[test]
fn remote_roles_may_select_their_own_view_but_view_only_never_writes() {
    for role in ["observer", "writer"] {
        for action in ["session.select", "context.select"] {
            assert!(evaluate_remote_role_action(role, action).allowed, "{role} {action}");
        }
    }
    for action in ["pane.direct_input", "pane.resize", "session.create", "session.kill"] {
        assert!(!evaluate_remote_role_action("observer", action).allowed, "{action}");
    }
    assert!(evaluate_remote_role_action("writer", "pane.resize").allowed);
    // Local observers keep their session policy.
    assert!(!evaluate_role_action("observer", "session.select").allowed);
}
