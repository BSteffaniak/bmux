#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod target;
mod transport;

use bmux_client::{
    BmuxClient, ConnectionPoolAcquireError, ConnectionPoolLimits, EndpointConnectionLease,
    EndpointConnectionPool,
};
use bmux_connections_plugin_api::{
    connection_types::{ConnectionError, InvocationOptions, ServiceInvokeKind},
    connections_commands::client::InvokeServiceRequest,
};
use bmux_plugin_sdk::{CancellationToken, prelude::*};
use std::sync::OnceLock;

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
                Ok::<Result<Vec<u8>, ConnectionError>, ServiceResponse>(invoke_service(
                    &config,
                    &paths,
                    &ctx.cancellation,
                    request,
                ))
            },
        })
    }
}

fn invoke_service(
    config: &bmux_config::BmuxConfig,
    paths: &bmux_config::ConfigPaths,
    cancellation: &CancellationToken,
    request: InvokeServiceRequest,
) -> Result<Vec<u8>, ConnectionError> {
    let invocation = request.invocation;
    let target_name = invocation.target.clone();
    let options = normalized_invocation_options(invocation.options.as_ref());
    if cancellation.is_cancelled() {
        return Err(ConnectionError::Cancelled {
            target: target_name,
        });
    }
    let timeout_ms = cancellation
        .deadline_ms
        .map_or(options.timeout_ms, |deadline| {
            deadline.min(options.timeout_ms)
        });
    if timeout_ms == 0 {
        return Err(ConnectionError::TimedOut {
            target: target_name,
            phase: "request".to_string(),
            timeout_ms,
        });
    }

    block_on_connection(async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let remaining =
            remaining_before_deadline(deadline).ok_or_else(|| ConnectionError::TimedOut {
                target: invocation.target.clone(),
                phase: "resolve".to_string(),
                timeout_ms,
            })?;
        let expanded = tokio::time::timeout(
            remaining,
            target::expand_reference(config, paths, &invocation.target),
        )
        .await
        .map_err(|_| ConnectionError::TimedOut {
            target: invocation.target.clone(),
            phase: "resolve".to_string(),
            timeout_ms,
        })??;
        let resolved = target::resolve_internal(config, &expanded)?;
        let kind = match invocation.kind {
            ServiceInvokeKind::InvokeQuery => bmux_ipc::InvokeServiceKind::Query,
            ServiceInvokeKind::InvokeCommand => bmux_ipc::InvokeServiceKind::Command,
        };
        let mut client = acquire_endpoint(
            config,
            paths,
            &resolved,
            AcquisitionPolicy {
                target: &invocation.target,
                cancellation,
                options: &options,
                deadline,
                timeout_ms,
            },
        )
        .await?;
        let remaining =
            remaining_before_deadline(deadline).ok_or_else(|| ConnectionError::TimedOut {
                target: invocation.target.clone(),
                phase: "invoke".to_string(),
                timeout_ms,
            })?;
        // Service calls are never replayed: a timeout or disconnect after
        // dispatch has an ambiguous completion state, especially for
        // non-idempotent commands. Ambiguous or failed clients are discarded
        // instead of returning to the idle pool.
        match tokio::time::timeout(
            remaining,
            client.invoke_service_raw(
                &invocation.capability,
                kind,
                &invocation.interface_id,
                &invocation.operation,
                invocation.payload,
            ),
        )
        .await
        {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(error)) => {
                client.mark_unhealthy();
                Err(ConnectionError::ServiceFailed {
                    target: invocation.target,
                    reason: error.to_string(),
                })
            }
            Err(_) => {
                client.mark_unhealthy();
                Err(ConnectionError::TimedOut {
                    target: invocation.target,
                    phase: "invoke".to_string(),
                    timeout_ms,
                })
            }
        }
    })
}

struct AcquisitionPolicy<'a> {
    target: &'a str,
    cancellation: &'a CancellationToken,
    options: &'a InvocationOptions,
    deadline: tokio::time::Instant,
    timeout_ms: u64,
}

fn connection_pool() -> &'static EndpointConnectionPool<BmuxClient> {
    static POOL: OnceLock<EndpointConnectionPool<BmuxClient>> = OnceLock::new();
    POOL.get_or_init(|| {
        EndpointConnectionPool::new(ConnectionPoolLimits::default())
            .expect("default connection pool limits are valid")
    })
}

async fn acquire_endpoint(
    config: &bmux_config::BmuxConfig,
    paths: &bmux_config::ConfigPaths,
    resolved: &target::ResolvedTarget,
    policy: AcquisitionPolicy<'_>,
) -> Result<EndpointConnectionLease<BmuxClient>, ConnectionError> {
    let AcquisitionPolicy {
        target,
        cancellation,
        options,
        deadline,
        timeout_ms,
    } = policy;
    let endpoint_id = resolved.connection_identity(config, paths);
    let mut last_connection_error = None;
    for attempt in 1..=options.max_attempts {
        if cancellation.is_cancelled() {
            return Err(ConnectionError::Cancelled {
                target: target.to_string(),
            });
        }
        let resolved = resolved.clone();
        match connection_pool()
            .acquire(endpoint_id.clone(), deadline, || async move {
                transport::connect(config, paths, &resolved).await
            })
            .await
        {
            Ok(client) => return Ok(client),
            Err(ConnectionPoolAcquireError::Connect(
                error @ ConnectionError::ConnectionFailed { .. },
            )) => {
                if options.max_attempts == 1 {
                    return Err(error);
                }
                last_connection_error = Some(error);
            }
            Err(ConnectionPoolAcquireError::Connect(error)) => return Err(error),
            Err(ConnectionPoolAcquireError::AdmissionTimedOut) => {
                let error = ConnectionError::TimedOut {
                    target: target.to_string(),
                    phase: "pool-admission".to_string(),
                    timeout_ms,
                };
                if options.max_attempts == 1 {
                    return Err(error);
                }
                last_connection_error = Some(error);
            }
        }
        if attempt < options.max_attempts {
            wait_for_retry(
                cancellation,
                deadline,
                options.retry_backoff_ms,
                target,
                timeout_ms,
            )
            .await?;
        }
    }
    Err(ConnectionError::RetryExhausted {
        target: target.to_string(),
        attempts: options.max_attempts,
        reason: last_connection_error.map_or_else(
            || "endpoint acquisition failed".to_string(),
            |error| format!("{error:?}"),
        ),
    })
}

const DEFAULT_INVOCATION_TIMEOUT_MS: u64 = 30_000;
const MAX_INVOCATION_ATTEMPTS: u32 = 10;
const MAX_RETRY_BACKOFF_MS: u64 = 30_000;

fn normalized_invocation_options(options: Option<&InvocationOptions>) -> InvocationOptions {
    let options = options.cloned().unwrap_or(InvocationOptions {
        timeout_ms: DEFAULT_INVOCATION_TIMEOUT_MS,
        max_attempts: 1,
        retry_backoff_ms: 0,
    });
    InvocationOptions {
        timeout_ms: options.timeout_ms,
        max_attempts: options.max_attempts.clamp(1, MAX_INVOCATION_ATTEMPTS),
        retry_backoff_ms: options.retry_backoff_ms.min(MAX_RETRY_BACKOFF_MS),
    }
}

fn remaining_before_deadline(deadline: tokio::time::Instant) -> Option<std::time::Duration> {
    deadline.checked_duration_since(tokio::time::Instant::now())
}

async fn wait_for_retry(
    cancellation: &CancellationToken,
    deadline: tokio::time::Instant,
    backoff_ms: u64,
    target: &str,
    timeout_ms: u64,
) -> Result<(), ConnectionError> {
    if cancellation.is_cancelled() {
        return Err(ConnectionError::Cancelled {
            target: target.to_string(),
        });
    }
    let remaining =
        remaining_before_deadline(deadline).ok_or_else(|| ConnectionError::TimedOut {
            target: target.to_string(),
            phase: "retry-backoff".to_string(),
            timeout_ms,
        })?;
    if backoff_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms).min(remaining)).await;
    }
    if cancellation.is_cancelled() {
        return Err(ConnectionError::Cancelled {
            target: target.to_string(),
        });
    }
    if remaining_before_deadline(deadline).is_none() {
        return Err(ConnectionError::TimedOut {
            target: target.to_string(),
            phase: "retry-backoff".to_string(),
            timeout_ms,
        });
    }
    Ok(())
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
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "local".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: b"routed payload".to_vec(),
                options: None,
            },
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
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "local".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: b"typed payload".to_vec(),
                options: None,
            },
        };
        let response = invoke_service(
            &BmuxConfig::default(),
            &paths,
            &CancellationToken::new(),
            request,
        )
        .expect("local invocation");
        assert_eq!(response, b"typed payload");

        let identity = target::resolve_internal(&BmuxConfig::default(), "local")
            .expect("local target")
            .connection_identity(&BmuxConfig::default(), &paths);
        assert_eq!(connection_pool().endpoint_counts(&identity), (0, 1, 1));

        let second = InvokeServiceRequest {
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "local".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: b"reused payload".to_vec(),
                options: None,
            },
        };
        let response = invoke_service(
            &BmuxConfig::default(),
            &paths,
            &CancellationToken::new(),
            second,
        )
        .expect("reused local invocation");
        assert_eq!(response, b"reused payload");
        assert_eq!(connection_pool().endpoint_counts(&identity), (0, 1, 1));

        server.request_shutdown();
        task.await.expect("server task join").expect("server run");
    }

    #[test]
    fn invocation_options_are_bounded_and_default_to_one_attempt() {
        assert_eq!(
            normalized_invocation_options(None),
            InvocationOptions {
                timeout_ms: DEFAULT_INVOCATION_TIMEOUT_MS,
                max_attempts: 1,
                retry_backoff_ms: 0,
            }
        );
        assert_eq!(
            normalized_invocation_options(Some(&InvocationOptions {
                timeout_ms: 17,
                max_attempts: u32::MAX,
                retry_backoff_ms: u64::MAX,
            })),
            InvocationOptions {
                timeout_ms: 17,
                max_attempts: MAX_INVOCATION_ATTEMPTS,
                retry_backoff_ms: MAX_RETRY_BACKOFF_MS,
            }
        );
    }

    #[test]
    fn cancelled_invocation_fails_before_endpoint_resolution() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        let request = InvokeServiceRequest {
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "missing".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: Vec::new(),
                options: None,
            },
        };
        assert_eq!(
            invoke_service(
                &BmuxConfig::default(),
                &paths,
                &CancellationToken::cancelled(),
                request,
            ),
            Err(ConnectionError::Cancelled {
                target: "missing".to_string(),
            })
        );
    }

    #[test]
    fn zero_deadline_fails_before_endpoint_resolution() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        let request = InvokeServiceRequest {
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "missing".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: Vec::new(),
                options: Some(InvocationOptions {
                    timeout_ms: 0,
                    max_attempts: 1,
                    retry_backoff_ms: 0,
                }),
            },
        };
        assert_eq!(
            invoke_service(
                &BmuxConfig::default(),
                &paths,
                &CancellationToken::new(),
                request,
            ),
            Err(ConnectionError::TimedOut {
                target: "missing".to_string(),
                phase: "request".to_string(),
                timeout_ms: 0,
            })
        );
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
