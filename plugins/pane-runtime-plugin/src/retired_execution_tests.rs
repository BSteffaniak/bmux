//! Retirement rejects prior wire identities, never dispatching them as v4.
#[test]
fn retired_execution_interfaces_reject_before_payload_decode() {
    use bmux_plugin_sdk::{
        ApiVersion, HostConnectionInfo, HostMetadata, HostScope, NativeServiceContext, ProviderId,
        RegisteredService, ServiceKind, ServiceRequest,
    };
    for version in [1, 2, 3] {
        let response = crate::handlers::route(NativeServiceContext {
            plugin_id: "bmux.pane_runtime".into(),
            request: ServiceRequest {
                caller_plugin_id: String::new(),
                service: RegisteredService {
                    capability: HostScope::new(format!("bmux.pane_runtime.execution.v{version}"))
                        .unwrap(),
                    kind: ServiceKind::Command,
                    interface_id: format!("pane-runtime-execution-v{version}"),
                    provider: ProviderId::Plugin("bmux.pane_runtime".into()),
                },
                operation: "new-program-session".into(),
                // Invalid on purpose: retired interfaces reject dispatch rather
                // than decoding this as a current contract or spawning work.
                payload: vec![255],
            },
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            host: HostMetadata {
                product_name: "bmux".into(),
                product_version: env!("CARGO_PKG_VERSION").into(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: vec![],
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
            caller_client_id: None,
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: None,
        });
        assert!(
            response.error.is_some(),
            "retired v{version} must be an explicit service dispatch error"
        );
        assert!(
            response.payload.is_empty(),
            "retired v{version} must not return a v4 execution response"
        );
    }
}
