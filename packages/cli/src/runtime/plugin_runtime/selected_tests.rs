#[cfg(test)]
mod tests {
    use super::super::*;
    use bmux_plugin_sdk::HostServiceDispatch;

    /// The selected host reactor for fixtures: the test's own runtime when
    /// there is one, otherwise a shared self-driving runtime.
    fn test_runtime() -> tokio::runtime::Handle {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        tokio::runtime::Handle::try_current().unwrap_or_else(|_| {
            RUNTIME
                .get_or_init(|| {
                    tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(1)
                        .enable_all()
                        .build()
                        .expect("fixture runtime")
                })
                .handle()
                .clone()
        })
    }

    fn declaration() -> bmux_plugin::PluginDeclaration {
        bmux_plugin::PluginManifest::from_toml_str(
            "id='example.owner'\nname='Owner'\nversion='1.0.0'\n",
        )
        .unwrap()
        .to_declaration()
        .unwrap()
    }

    #[tokio::test]
    async fn lifecycle_observation_polls_author_off_worker_through_async_settlement() {
        use bmux_plugin_sdk::definition::{
            ComponentRole, PluginDefinition, PluginInstance, PluginLifecycleContext, PluginMetadata,
        };
        struct Author {
            observer: std::thread::ThreadId,
            polls: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl PluginInstance for Author {
            fn activate_with_async(
                &self,
                _: PluginLifecycleContext,
                _: bmux_plugin_sdk::HostAsyncHandle,
            ) -> bmux_plugin_sdk::PluginFuture<'_, bmux_plugin_sdk::PluginResult<()>> {
                Box::pin(async move {
                    assert_ne!(std::thread::current().id(), self.observer);
                    self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    assert_ne!(std::thread::current().id(), self.observer);
                    self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            }
        }
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = polls.clone();
        let observer = std::thread::current().id();
        let artifact = bmux_plugin::DefinitionArtifact::from_definition(PluginDefinition::new(
            PluginMetadata {
                plugin_id: "example.owner".into(),
                version: "1".into(),
                roles: vec![ComponentRole::Backend],
                required_capabilities: vec![],
                contracts: vec![],
                manifest_toml: None,
            },
            move |_| {
                Ok(Arc::new(Author {
                    observer,
                    polls: seen.clone(),
                }) as Arc<dyn PluginInstance>)
            },
        ));
        let execution = artifact
            .create(PluginRuntimeScopeId::new(), ComponentRole::Backend, &[])
            .unwrap();
        // Production selects the host reactor before activation; observation
        // fails closed without it rather than borrowing the polling thread's.
        execution
            .bind_callback_runtime(tokio::runtime::Handle::current())
            .unwrap();
        let native: NativeLifecycleContext = serde_json::from_value(serde_json::json!({
            "plugin_id": "example.owner", "host": crate::runtime::plugin_runtime::plugin_host_metadata(),
            "connection": { "config_dir": "/selected/config", "runtime_dir": "/selected/runtime",
                "data_dir": "/selected/data", "state_dir": "/selected/state" }
        })).unwrap();
        let id = execution
            .begin_activation(
                native.clone(),
                bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent,
            )
            .unwrap();
        assert!(matches!(
            ProductionPlugin::observe(&execution, id).await.unwrap(),
            NativeOutcome::Unit
        ));
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let cleanup = execution.begin_deactivation(native).await.unwrap();
        assert!(matches!(
            ProductionPlugin::observe_with_cleanup(&execution, cleanup, true)
                .await
                .unwrap(),
            NativeOutcome::Unit
        ));
        execution.destroy().await.unwrap();
    }

    #[tokio::test]
    async fn trusted_async_executor_moves_callback_off_observer_thread() {
        let result =
            crate::runtime::plugin_kernel::AuthenticatedServiceInvocation::dispatch_host_callback(
                tokio::runtime::Handle::current(),
                || {
                    // The callback is executed by the trusted host, not by a caller
                    // declaring a marker in operation metadata.
                    Ok(std::thread::current().id())
                },
            )
            .await
            .unwrap();
        assert_ne!(result, std::thread::current().id());
    }

    #[tokio::test]
    async fn asynchronous_host_dispatch_rejects_another_instance_before_scheduling() {
        let owner = bmux_plugin_runtime::InstanceScope::new(PluginRuntimeScopeId::new());
        let other = bmux_plugin_runtime::InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = other
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let error = {
            let port = KernelServices {
                declaration: declaration(),
                instance: owner.id(),
                authority: None,
                services: Vec::new(),
                builtins: Arc::new(std::sync::OnceLock::new()),
                command_route: None,
                runtime: test_runtime(),
            };
            port.invoke_owned(
                invocation.clone(),
                bmux_plugin_sdk::HostServiceOwnedRequest {
                    capability: "example.query".into(),
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface: "example.interface/v1".into(),
                    operation: "read".into(),
                    payload: Vec::new(),
                },
            )
            .await
            .unwrap_err()
        };
        assert!(error.to_string().contains("instance mismatch"));
        invocation.close().unwrap();
        invocation.finish().unwrap();
    }

    #[test]
    fn lifecycle_failure_diagnostic_preserves_original_outcome_and_cleanup_identity() {
        let outcome = NativeOutcome::Error {
            code: 17,
            message: "original activation failure".into(),
        };
        let error = observation_ack_error(41, &outcome, &failure("cleanup still required"));
        let text = error.to_string();
        assert!(text.contains("original activation failure"));
        assert!(text.contains("cleanup still required"));
        assert!(text.contains("41"));
    }

    #[test]
    fn bound_kernel_dispatch_rejects_another_instance_before_routing() {
        let owner = bmux_plugin_runtime::InstanceScope::new(PluginRuntimeScopeId::new());
        let other = bmux_plugin_runtime::InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = other
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let port = KernelServices {
            declaration: declaration(),
            instance: owner.id(),
            authority: None,
            services: Vec::new(),
            builtins: Arc::new(std::sync::OnceLock::new()),
            command_route: None,
            runtime: test_runtime(),
        };
        let error = port
            .invoke(
                &invocation,
                bmux_plugin_sdk::HostServiceRequest {
                    capability: "example.query",
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface: "example.interface/v1",
                    operation: "read",
                    payload: &[],
                },
            )
            .unwrap_err();
        drop(port);
        assert!(error.to_string().contains("instance mismatch"));
    }

    #[test]
    fn direct_service_context_provenance_uses_selected_kernel_port_without_bridge() {
        use bmux_plugin::ServiceCaller;
        use bmux_plugin_sdk::{
            NativeServiceContext, ProviderId, RegisteredService, ServiceKind, ServiceRequest,
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let port = bmux_plugin_sdk::PluginHostServices::new(|_| {
            Err(failure(
                "provenance must use kernel port, not service transport",
            ))
        })
        .with_kernel(move |bytes| {
            assert!(matches!(
                bmux_ipc::decode::<bmux_ipc::Request>(bytes).unwrap(),
                bmux_ipc::Request::CurrentStreamProvenance
            ));
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            bmux_ipc::encode(&bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                provenance: bmux_ipc::StreamProvenance::HostInternal,
            })
            .map_err(|error| failure(error.to_string()))
        });
        let paths = bmux_config::ConfigPaths::default();
        let context = NativeServiceContext {
            plugin_id: "example.direct-context".into(),
            request: ServiceRequest {
                caller_plugin_id: "not-authority".into(),
                service: RegisteredService {
                    capability: HostScope::new("example.identity").unwrap(),
                    kind: ServiceKind::Query,
                    interface_id: "example.identity/v1".into(),
                    provider: ProviderId::Plugin("example.direct-context".into()),
                },
                operation: "origin".into(),
                payload: Vec::new(),
            },
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            host: crate::runtime::plugin_runtime::plugin_host_metadata(),
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: paths.config_dir.to_string_lossy().into_owned(),
                config_dir_candidates: Vec::new(),
                runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
                data_dir: paths.data_dir.to_string_lossy().into_owned(),
                state_dir: paths.state_dir.to_string_lossy().into_owned(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            cancellation: bmux_plugin_sdk::CancellationToken::none(),
            host_kernel_bridge: None,
            host_service_port: Some(port),
        };
        let result = context
            .execute_kernel_request(bmux_ipc::Request::CurrentStreamProvenance)
            .expect("direct foundational context must preserve the selected kernel authority");
        assert!(matches!(
            result,
            bmux_ipc::ResponsePayload::CurrentStreamProvenance {
                provenance: bmux_ipc::StreamProvenance::HostInternal,
            }
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn selected_host_storage_uses_instance_binding_not_plugin_transport() {
        let root = tempfile::tempdir().unwrap();
        let cap = HostScope::new("bmux.storage").unwrap();
        let mut declaration = declaration();
        declaration.required_capabilities.insert(cap.clone());
        let services = vec![
            bmux_plugin_sdk::RegisteredService {
                capability: cap.clone(),
                kind: bmux_plugin_sdk::ServiceKind::Query,
                interface_id: "storage-query/v1".into(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            },
            bmux_plugin_sdk::RegisteredService {
                capability: cap.clone(),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "storage-command/v1".into(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            },
        ];
        let primitives = bmux_plugin::InvocationHostBuiltins::new(
            bmux_plugin::PluginId::new(declaration.id.as_str()).unwrap(),
            bmux_plugin_sdk::HostConnectionInfo {
                config_dir: root.path().join("config").to_string_lossy().into_owned(),
                config_dir_candidates: Vec::new(),
                runtime_dir: root.path().join("runtime").to_string_lossy().into_owned(),
                data_dir: root.path().join("data").to_string_lossy().into_owned(),
                state_dir: root.path().join("state").to_string_lossy().into_owned(),
            },
            services.clone(),
            vec![cap],
            BTreeMap::new(),
            None,
        )
        .unwrap();
        let builtins = Arc::new(std::sync::OnceLock::new());
        assert!(builtins.set(Arc::new(primitives)).is_ok());
        let instance = bmux_plugin_runtime::InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let port = KernelServices {
            declaration,
            instance: instance.id(),
            authority: None,
            services,
            builtins,
            command_route: None,
            runtime: test_runtime(),
        };
        let key = bmux_plugin_sdk::StorageKey::new("owned-value").unwrap();
        let set = bmux_plugin_sdk::encode_service_message(
            &bmux_plugin_sdk::StorageSetRequest::new(key.clone(), vec![4, 8]),
        )
        .unwrap();
        port.invoke(
            &invocation,
            bmux_plugin_sdk::HostServiceRequest {
                capability: "bmux.storage",
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface: "storage-command/v1",
                operation: "set",
                payload: &set,
            },
        )
        .unwrap();
        let get =
            bmux_plugin_sdk::encode_service_message(&bmux_plugin_sdk::StorageGetRequest::new(key))
                .unwrap();
        let bytes = port
            .invoke(
                &invocation,
                bmux_plugin_sdk::HostServiceRequest {
                    capability: "bmux.storage",
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface: "storage-query/v1",
                    operation: "get",
                    payload: &get,
                },
            )
            .unwrap();
        drop(port);
        let value: bmux_plugin_sdk::StorageGetResponse =
            bmux_plugin_sdk::decode_service_message(&bytes).unwrap();
        assert_eq!(value.value, Some(vec![4, 8]));
        invocation.close().unwrap();
        invocation.finish().unwrap();
    }

    #[test]
    fn nested_command_cannot_use_cached_builtin_or_missing_parent_as_authority() {
        use bmux_plugin_sdk::HostServiceDispatch;
        let owner = bmux_plugin_runtime::InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = owner
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let mut declaration = declaration();
        declaration
            .required_capabilities
            .insert(HostScope::new(bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY).unwrap());
        let (route, _lifetime, mut requests) =
            crate::runtime::plugin_kernel::CommandDispatch::channel();
        let port = KernelServices {
            declaration,
            instance: owner.id(),
            authority: None,
            services: Vec::new(),
            builtins: Arc::new(std::sync::OnceLock::new()),
            command_route: Some(route),
            runtime: test_runtime(),
        };
        let result = port.invoke(
            &invocation,
            bmux_plugin_sdk::HostServiceRequest {
                capability: bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY,
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface: bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1,
                operation: bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
                payload: &[],
            },
        );
        drop(port);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("live callback dependency")
        );
        assert!(
            requests.try_recv().is_err(),
            "missing ancestry must dispatch no request"
        );
        invocation.close().unwrap();
        invocation.finish().unwrap();
        activation.close().unwrap();
        activation.finish().unwrap();
        owner.close().unwrap();
        owner.finish().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_route_service_calls_carry_the_bound_declaration_identity() {
        let owner = bmux_plugin_runtime::InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = owner
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let mut declaration = declaration();
        declaration
            .required_capabilities
            .insert(HostScope::new("example.status.write").unwrap());
        let (route, _lifetime, mut requests) =
            crate::runtime::plugin_kernel::CommandDispatch::channel();
        let port = KernelServices {
            declaration,
            instance: owner.id(),
            authority: None,
            services: Vec::new(),
            builtins: Arc::new(std::sync::OnceLock::new()),
            command_route: Some(route),
            runtime: test_runtime(),
        };
        let scope = invocation.clone();
        let worker = tokio::task::spawn_blocking(move || {
            port.invoke(
                &scope,
                bmux_plugin_sdk::HostServiceRequest {
                    capability: "example.status.write",
                    kind: bmux_plugin_sdk::ServiceKind::Command,
                    interface: "example.status/v1",
                    operation: "set",
                    payload: b"item",
                },
            )
        });
        let negotiated = bmux_ipc::NegotiatedProtocol {
            wire_epoch: 0,
            revision: 0,
            capabilities: vec![bmux_ipc::CAPABILITY_SERVICE_CALLER_PLUGIN_V1.to_string()],
        };
        let wire = {
            let request = requests.recv().await.expect("selected request");
            let wire = request.wire_request(Some(&negotiated));
            request.respond(Ok(bmux_ipc::Response::Ok(
                bmux_ipc::ResponsePayload::ServiceInvoked {
                    payload: Vec::new(),
                },
            )));
            wire
        };
        // The identity is the declaration bound to the admitted instance.
        assert!(
            matches!(
                &wire,
                bmux_ipc::Request::InvokeServiceAsPlugin { caller_plugin_id, payload, .. }
                    if caller_plugin_id == "example.owner" && payload == b"item"
            ),
            "selected command calls must be attributed to the bound plugin: {wire:?}"
        );
        worker.await.unwrap().unwrap();
        invocation.close().unwrap();
        invocation.finish().unwrap();
        activation.close().unwrap();
        activation.finish().unwrap();
        owner.close().unwrap();
        owner.finish().unwrap();
    }

    #[test]
    fn undeclared_capability_is_rejected_before_any_kernel_effect() {
        let owner = bmux_plugin_runtime::InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = owner
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let port = KernelServices {
            declaration: declaration(),
            instance: owner.id(),
            authority: None,
            services: Vec::new(),
            builtins: Arc::new(std::sync::OnceLock::new()),
            command_route: None,
            runtime: test_runtime(),
        };
        let error = port
            .invoke(
                &invocation,
                bmux_plugin_sdk::HostServiceRequest {
                    capability: "example.query",
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface: "example.interface/v1",
                    operation: "read",
                    payload: &[],
                },
            )
            .unwrap_err();
        drop(port);
        assert!(error.to_string().contains("not declared by caller"));
    }
}
