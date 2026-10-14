#[cfg(test)]
mod tests {
    use crate::*;
    #[test]
    fn unproven_caller_service_is_rejected_before_selected_file_access() {
        use bmux_plugin_sdk::{
            ApiVersion, HostConnectionInfo, HostMetadata, HostScope, ProviderId, RegisteredService,
            ServiceKind, ServiceRequest,
        };
        let caller = uuid::Uuid::new_v4();
        let context = NativeServiceContext {
            plugin_id: "bmux.broadcast".into(),
            request: ServiceRequest {
                caller_plugin_id: "bmux.broadcast".into(),
                service: RegisteredService {
                    capability: HostScope::new("bmux.broadcast.control.v1").unwrap(),
                    kind: ServiceKind::Command,
                    interface_id: "broadcast-control-v1".into(),
                    provider: ProviderId::Plugin("bmux.broadcast".into()),
                },
                operation: "start".into(),
                payload: bmux_codec::to_typed_vec(&control::client::StartRequest {
                    endpoint: "https://example.com".into(),
                    channel: "main".into(),
                    credential_file: "/nonexistent/private-file-must-not-be-read".into(),
                    source_id: uuid::Uuid::new_v4().to_string(),
                })
                .unwrap(),
            },
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            host: HostMetadata {
                product_name: "bmux".into(),
                product_version: "test".into(),
                plugin_api_version: ApiVersion::new(1, 1),
                plugin_abi_version: ApiVersion::new(1, 1),
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
            caller_client_id: Some(caller),
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: None,
        };
        let response = BroadcastPlugin.invoke_service(context.clone());
        assert_eq!(response.error.unwrap().code, "local_authorization_required");
        let mut profile = context.clone();
        profile.request.service.interface_id = "broadcast-turnkey-v1".into();
        profile.request.operation = "start-profile".into();
        profile.request.payload = bmux_codec::to_typed_vec(&turnkey::client::StartProfileRequest {
            profile: None,
            source_id: uuid::Uuid::new_v4().to_string(),
        })
        .unwrap();
        assert_eq!(
            BroadcastPlugin.invoke_service(profile).error.unwrap().code,
            "local_authorization_required"
        );
        let mut register = context;
        register.request.service.interface_id = "broadcast-output-v1".into();
        register.request.operation = "register".into();
        register.request.payload = bmux_codec::to_typed_vec(&output::client::RegisterRequest {
            source_id: uuid::Uuid::new_v4().to_string(),
        })
        .unwrap();
        assert_eq!(
            BroadcastPlugin.invoke_service(register).error.unwrap().code,
            "local_authorization_required"
        );
    }
}
