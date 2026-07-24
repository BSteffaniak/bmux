#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod target;
mod transport;

use bmux_connections_plugin_api::{
    connection_types::{ConnectionError, ServiceInvokeKind},
    connections_commands::client::InvokeServiceRequest,
};
use bmux_plugin_sdk::prelude::*;

#[derive(Default)]
pub struct ConnectionsPlugin;

impl RustPlugin for ConnectionsPlugin {
    type Contract = bmux_connections_plugin_api::Contract;

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "connections-state", "resolve" => |request: bmux_connections_plugin_api::connections_state::client::ResolveRequest, ctx| {
                let config = target::load_config(&ctx.connection)
                    .map_err(|error| ServiceResponse::error("config_failed", error.to_string()))?;
                let paths = target::context_paths(&ctx.connection);
                let target = block_on_connection(async { target::expand_reference(&config, &paths, &request.target).await })
                    .map_err(|error| ServiceResponse::error("resolution_failed", format!("{error:?}")))?;
                Ok::<_, ServiceResponse>(target::resolve(&config, &target))
            },
            "connections-state", "list-targets" => |(): (), ctx| {
                let config = target::load_config(&ctx.connection)
                    .map_err(|error| ServiceResponse::error("config_failed", error.to_string()))?;
                Ok::<_, ServiceResponse>(target::list(&config))
            },
            "connections-commands", "invoke-service" => |request: InvokeServiceRequest, ctx| {
                let config = target::load_config(&ctx.connection)
                    .map_err(|error| ServiceResponse::error("config_failed", error.to_string()))?;
                let paths = target::context_paths(&ctx.connection);
                Ok::<Result<Vec<u8>, ConnectionError>, ServiceResponse>(invoke_service(&config, &paths, request))
            },
        })
    }
}

fn invoke_service(
    config: &bmux_config::BmuxConfig,
    paths: &bmux_config::ConfigPaths,
    request: InvokeServiceRequest,
) -> Result<Vec<u8>, ConnectionError> {
    block_on_connection(async move {
        let expanded = target::expand_reference(config, paths, &request.target).await?;
        let resolved = target::resolve_internal(config, &expanded)?;
        let kind = match request.kind {
            ServiceInvokeKind::InvokeQuery => bmux_ipc::InvokeServiceKind::Query,
            ServiceInvokeKind::InvokeCommand => bmux_ipc::InvokeServiceKind::Command,
        };
        let mut client = transport::connect(config, paths, &resolved).await?;
        client
            .invoke_service_raw(
                &request.capability,
                kind,
                &request.interface_id,
                &request.operation,
                request.payload,
            )
            .await
            .map_err(|error| ConnectionError::ServiceFailed {
                target: request.target,
                reason: error.to_string(),
            })
    })
}

fn block_on_connection<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T> + Send,
    T: Send,
{
    let handle = tokio::runtime::Handle::try_current()
        .expect("connections service requires the host tokio runtime");
    tokio::task::block_in_place(|| handle.block_on(future))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_config::{BmuxConfig, ConfigPaths};
    use bmux_connections_plugin_api::connection_types::ServiceInvokeKind;
    use bmux_plugin_sdk::{
        CancellationToken, HostConnectionInfo, HostMetadata, HostScope, NativeServiceContext,
        ProviderId, RegisteredService, ServiceKind, ServiceRequest,
    };
    use bmux_server::BmuxServer;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn test_paths(root: &std::path::Path) -> ConfigPaths {
        ConfigPaths::new(
            root.join("config"),
            root.join("runtime"),
            root.join("data"),
            root.join("state"),
        )
    }

    fn service_context(
        paths: &ConfigPaths,
        operation: &str,
        payload: Vec<u8>,
    ) -> NativeServiceContext {
        NativeServiceContext {
            plugin_id: "bmux.connections".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new(if operation == "invoke-service" {
                        "bmux.connections.invoke"
                    } else {
                        "bmux.connections.read"
                    })
                    .expect("capability"),
                    kind: if operation == "invoke-service" {
                        ServiceKind::Command
                    } else {
                        ServiceKind::Query
                    },
                    interface_id: if operation == "invoke-service" {
                        "connections-commands".to_string()
                    } else {
                        "connections-state".to_string()
                    },
                    provider: ProviderId::Plugin("bmux.connections".to_string()),
                },
                operation: operation.to_string(),
                payload,
            },
            required_capabilities: Vec::new(),
            provided_capabilities: vec![
                "bmux.connections.read".to_string(),
                "bmux.connections.invoke".to_string(),
            ],
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: vec!["bmux.connections".to_string()],
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux-test".to_string(),
                product_version: "0.0.0".to_string(),
                plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: HostConnectionInfo {
                config_dir: paths.config_dir.to_string_lossy().into_owned(),
                config_dir_candidates: Vec::new(),
                runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
                data_dir: paths.data_dir.to_string_lossy().into_owned(),
                state_dir: paths.state_dir.to_string_lossy().into_owned(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            cancellation: CancellationToken::new(),
            host_kernel_bridge: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plugin_service_routes_resolution_and_invocation() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");
        std::fs::write(paths.config_file(), "").expect("config file");
        let server = std::sync::Arc::new(BmuxServer::from_config_paths(&paths));
        server
            .register_service_handler(
                "example.echo",
                bmux_ipc::InvokeServiceKind::Query,
                "echo/v1",
                "echo",
                |_route, _context, payload| async move { Ok(payload) },
            )
            .expect("register service");
        let running = std::sync::Arc::clone(&server);
        let task = tokio::spawn(async move { running.run().await });
        for _ in 0..100 {
            if paths.server_socket().exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let resolve = bmux_connections_plugin_api::connections_state::client::ResolveRequest {
            target: "local".to_string(),
        };
        let resolve_payload =
            bmux_plugin_sdk::encode_service_message(&resolve).expect("encode resolve");
        let response =
            ConnectionsPlugin.invoke_service(service_context(&paths, "resolve", resolve_payload));
        assert!(response.error.is_none());
        let resolved: Result<
            bmux_connections_plugin_api::connection_types::ResolvedEndpoint,
            bmux_connections_plugin_api::connection_types::ConnectionError,
        > = bmux_plugin_sdk::decode_service_message(&response.payload).expect("decode resolve");
        assert_eq!(resolved.expect("resolved").address, "local");

        let request = InvokeServiceRequest {
            target: "local".to_string(),
            capability: "example.echo".to_string(),
            kind: ServiceInvokeKind::InvokeQuery,
            interface_id: "echo/v1".to_string(),
            operation: "echo".to_string(),
            payload: b"routed payload".to_vec(),
        };
        let payload = bmux_plugin_sdk::encode_service_message(&request).expect("encode invoke");
        let response =
            ConnectionsPlugin.invoke_service(service_context(&paths, "invoke-service", payload));
        assert!(response.error.is_none());
        let invoked: Result<Vec<u8>, ConnectionError> =
            bmux_plugin_sdk::decode_service_message(&response.payload).expect("decode invoke");
        assert_eq!(invoked.expect("invoked"), b"routed payload");

        server.request_shutdown();
        task.await.expect("server task join").expect("server run");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_endpoint_invokes_typed_service_end_to_end() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");
        std::fs::write(paths.config_file(), "").expect("config file");
        let server = std::sync::Arc::new(BmuxServer::from_config_paths(&paths));
        server
            .register_service_handler(
                "example.echo",
                bmux_ipc::InvokeServiceKind::Query,
                "echo/v1",
                "echo",
                |_route, _context, payload| async move { Ok(payload) },
            )
            .expect("register service");
        let running = std::sync::Arc::clone(&server);
        let task = tokio::spawn(async move { running.run().await });
        for _ in 0..100 {
            if paths.server_socket().exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let request = InvokeServiceRequest {
            target: "local".to_string(),
            capability: "example.echo".to_string(),
            kind: ServiceInvokeKind::InvokeQuery,
            interface_id: "echo/v1".to_string(),
            operation: "echo".to_string(),
            payload: b"typed payload".to_vec(),
        };
        let response =
            invoke_service(&BmuxConfig::default(), &paths, request).expect("local invocation");
        assert_eq!(response, b"typed payload");

        server.request_shutdown();
        task.await.expect("server task join").expect("server run");
    }

    #[test]
    fn load_config_uses_host_candidate_chain() {
        let root = tempfile::tempdir().expect("tempdir");
        let canonical = root.path().join("canonical");
        let fallback = root.path().join("fallback");
        std::fs::create_dir_all(&fallback).expect("fallback dir");
        std::fs::write(
            fallback.join("bmux.toml"),
            "[connections]\ndefault_target = 'remote'\n",
        )
        .expect("fallback config");
        let connection = HostConnectionInfo {
            config_dir: canonical.to_string_lossy().into_owned(),
            config_dir_candidates: vec![
                canonical.to_string_lossy().into_owned(),
                fallback.to_string_lossy().into_owned(),
            ],
            runtime_dir: root.path().join("runtime").to_string_lossy().into_owned(),
            data_dir: root.path().join("data").to_string_lossy().into_owned(),
            state_dir: root.path().join("state").to_string_lossy().into_owned(),
        };
        let config = target::load_config(&connection).expect("fallback config");
        assert_eq!(config.connections.default_target.as_deref(), Some("remote"));
    }

    #[test]
    fn context_paths_preserve_all_plugin_host_roots() {
        let connection = bmux_plugin_sdk::HostConnectionInfo {
            config_dir: "/config".to_string(),
            config_dir_candidates: vec![PathBuf::from("/config")]
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            runtime_dir: "/runtime".to_string(),
            data_dir: "/data".to_string(),
            state_dir: "/state".to_string(),
        };
        let paths = target::context_paths(&connection);
        assert_eq!(paths.config_dir, PathBuf::from("/config"));
        assert_eq!(paths.runtime_dir, PathBuf::from("/runtime"));
        assert_eq!(paths.data_dir, PathBuf::from("/data"));
        assert_eq!(paths.state_dir, PathBuf::from("/state"));
    }
}

bmux_plugin_sdk::export_plugin!(ConnectionsPlugin, include_str!("../plugin.toml"));
