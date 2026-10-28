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
        for operation in [
            "pause-publication",
            "resume-publication",
            "stop-publication",
            "source-publication",
        ] {
            let mut operator = context.clone();
            operator.request.operation = operation.into();
            operator.request.payload = if operation == "source-publication" {
                bmux_codec::to_typed_vec(&control::client::SourcePublicationRequest {
                    source_id: uuid::Uuid::new_v4().to_string(),
                })
                .unwrap()
            } else {
                bmux_codec::to_typed_vec(&()).unwrap()
            };
            assert_eq!(
                BroadcastPlugin.invoke_service(operator).error.unwrap().code,
                "local_authorization_required",
                "{operation} must require local proof before mutation"
            );
        }
        for operation in ["status", "events"] {
            let mut diagnostic = context.clone();
            diagnostic.request.service.capability =
                HostScope::new("bmux.broadcast.diagnostics.v1").unwrap();
            diagnostic.request.service.interface_id = "broadcast-diagnostics-v1".into();
            diagnostic.request.operation = operation.into();
            diagnostic.request.payload = if operation == "events" {
                bmux_codec::to_typed_vec(
                    &bmux_broadcast_plugin_api::broadcast_diagnostics_v1::client::EventsRequest {
                        last: 50,
                    },
                )
                .unwrap()
            } else {
                bmux_codec::to_typed_vec(&()).unwrap()
            };
            assert_eq!(
                BroadcastPlugin
                    .invoke_service(diagnostic)
                    .error
                    .unwrap()
                    .code,
                "local_authorization_required"
            );
        }
        let mut explicit = context.clone();
        explicit.request.operation = "start-publication".into();
        explicit.request.payload =
            bmux_codec::to_typed_vec(&control::client::StartPublicationRequest {
                endpoint: "https://example.com".into(),
                channel: "main".into(),
                credential_file: "/nonexistent/private-file-must-not-be-read".into(),
                source_id: uuid::Uuid::new_v4().to_string(),
            })
            .unwrap();
        assert_eq!(
            BroadcastPlugin.invoke_service(explicit).error.unwrap().code,
            "local_authorization_required"
        );
        let mut profile = context.clone();
        profile.request.service.interface_id = "broadcast-turnkey-v1".into();
        profile.request.operation = "start-profile".into();
        profile.request.payload = bmux_codec::to_typed_vec(&turnkey::client::StartProfileRequest {
            profile: None,
            source_id: uuid::Uuid::new_v4().to_string(),
        })
        .unwrap();
        for operation in ["start-profile", "start-profile-publication"] {
            profile.request.operation = operation.into();
            assert_eq!(
                BroadcastPlugin
                    .invoke_service(profile.clone())
                    .error
                    .unwrap()
                    .code,
                "local_authorization_required"
            );
        }
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
