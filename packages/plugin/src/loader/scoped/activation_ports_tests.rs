use super::*;
use crate::ServiceCaller as _;
use bmux_plugin_sdk::typed_dispatch::{SelectedServiceAvailability, ServiceAvailabilityKey};
use bmux_plugin_sdk::{HostServiceDispatch, HostServiceRequest, PluginHostServices};
use std::sync::Mutex;

struct Capture(Arc<Mutex<Option<crate::TypedServiceCaller>>>);
impl bmux_plugin_sdk::definition::PluginInstance for Capture {
    fn register_typed_services_with_context(
        &self,
        context: bmux_plugin_sdk::TypedServiceRegistrationContext<'_>,
        _: &mut bmux_plugin_sdk::TypedServiceRegistry,
    ) -> bmux_plugin_sdk::definition::PluginResult<()> {
        *self.0.lock().unwrap() = Some(crate::TypedServiceCaller::from_registration_context(
            &context,
        ));
        Ok(())
    }
}
struct Dispatch(Arc<Mutex<Vec<InvocationScope>>>);
impl HostServiceDispatch for Dispatch {
    fn invoke(&self, _: &InvocationScope, _: HostServiceRequest<'_>) -> Result<Vec<u8>> {
        panic!("availability must not invoke synchronous service")
    }
    fn invoke_async(
        &self,
        scope: &InvocationScope,
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, Result<Vec<u8>>> {
        self.0.lock().unwrap().push(scope.clone());
        Box::pin(async move { Ok(request.payload) })
    }
    fn open_retained_host(
        &self,
        scope: &InvocationScope,
    ) -> Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        self.0.lock().unwrap().push(scope.clone());
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "fixture_retained",
        })
    }
    fn selected_service_availability(
        &self,
        scope: &InvocationScope,
        key: ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, Result<SelectedServiceAvailability>>
    {
        self.0.lock().unwrap().push(scope.clone());
        Box::pin(async move {
            match key.interface_id.as_str() {
                "present/v1" => Ok(SelectedServiceAvailability::Present),
                "absent/v1" => Ok(SelectedServiceAvailability::Absent),
                _ => Err(failure("access denied")),
            }
        })
    }
}
fn key(interface: &str) -> ServiceAvailabilityKey {
    ServiceAvailabilityKey {
        capability: "example.read".into(),
        kind: bmux_plugin_sdk::ServiceKind::Query,
        interface_id: interface.into(),
    }
}

#[tokio::test]
async fn static_registration_retains_activation_capability_not_registration_invocation() {
    use bmux_plugin_sdk::definition::{ComponentRole, PluginDefinition, PluginMetadata};
    let captured = Arc::new(Mutex::new(None));
    let implementation = Arc::new(Capture(captured.clone()));
    let definition = PluginDefinition::new(
        PluginMetadata {
            plugin_id: "example.registration".into(),
            version: "1".into(),
            roles: vec![ComponentRole::Backend],
            required_capabilities: vec![],
            contracts: vec![],
            manifest_toml: None,
        },
        move |_| Ok(implementation.clone() as Arc<dyn bmux_plugin_sdk::definition::PluginInstance>),
    );
    let execution = super::super::DefinitionArtifact::from_definition(definition)
        .create(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
            ComponentRole::Backend,
            &[],
        )
        .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    execution
        .bind_host_dispatch(Arc::new(Dispatch(calls.clone())))
        .unwrap();
    let activation = execution
        .scope
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    execution.state.lock().unwrap().activation = Some(activation.clone());
    let native = super::super::super::tests::native_test_service_context(vec![]);
    let settings = std::collections::BTreeMap::new();
    // A caller-supplied temporary capability is deliberately unusable: loader
    // must replace it with its selected activation owner, not clone it.
    let forged = PluginHostServices::new(|_| Err(failure("temporary port expired")));
    execution
        .collect_typed_services(bmux_plugin_sdk::TypedServiceRegistrationContext {
            plugin_id: "example.registration",
            host_service_port: Some(&forged),
            host_kernel_bridge: None,
            required_capabilities: &[],
            provided_capabilities: &[],
            services: &[],
            available_capabilities: &[],
            enabled_plugins: &[],
            plugin_search_roots: &[],
            host: &native.host,
            connection: &native.connection,
            plugin_settings_map: &settings,
        })
        .unwrap();
    let caller = captured.lock().unwrap().take().unwrap();
    assert_eq!(
        caller
            .selected_service_availability(key("present/v1"))
            .await
            .unwrap(),
        SelectedServiceAvailability::Present
    );
    assert_eq!(
        caller
            .selected_service_availability(key("absent/v1"))
            .await
            .unwrap(),
        SelectedServiceAvailability::Absent
    );
    assert!(
        caller
            .selected_service_availability(key("denied/v1"))
            .await
            .is_err()
    );
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_ne!(calls[0].id(), calls[1].id());
        assert!(calls.iter().all(|call| call.try_admit().is_err()));
        drop(calls);
    }
    activation.close().unwrap();
    assert!(
        caller
            .selected_service_availability(key("present/v1"))
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn activation_callbacks_preserve_async_and_retained_routes_after_setup_retirement() {
    let instance =
        bmux_plugin_runtime::InstanceScope::new(bmux_plugin_runtime::PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let setup = activation
        .begin_lifecycle_invocation(bmux_plugin_runtime::LifecycleInvocationKind::Activation)
        .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let port = services(
        activation.clone(),
        Arc::new(Dispatch(calls.clone())),
        Some(setup.clone()),
    );
    setup.close().unwrap();
    setup.finish().unwrap();
    activation.mark_active().unwrap();
    let request = bmux_plugin_sdk::HostServiceOwnedRequest {
        capability: "example.read".into(),
        kind: bmux_plugin_sdk::ServiceKind::Query,
        interface: "example/v1".into(),
        operation: "read".into(),
        payload: vec![42],
    };
    assert_eq!(
        port.call_service_async(request.clone()).await.unwrap(),
        vec![42]
    );
    assert!(matches!(
        port.open_retained_host(),
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "fixture_retained"
        })
    ));
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0].id(), calls[1].id());
        assert!(
            calls
                .iter()
                .all(|call| call.id() != setup.id() && call.try_admit().is_err())
        );
        drop(calls);
    }
    activation.close().unwrap();
    assert!(port.call_service_async(request).await.is_err());
    assert!(port.open_retained_host().is_err());
    assert_eq!(calls.lock().unwrap().len(), 2);
}

/// The callee's admitted work (e.g. a provider's nested entry) may settle
/// just after the call returns. The fresh caller drains it before finishing:
/// the call succeeds and no unfinished child strands the activation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activation_call_drains_settling_callee_work_before_retiring_its_caller() {
    struct Lingering;
    impl Lingering {
        fn linger(scope: &InvocationScope) {
            let admission = scope.try_admit().unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                drop(admission);
            });
        }
    }
    impl HostServiceDispatch for Lingering {
        fn invoke(
            &self,
            scope: &InvocationScope,
            request: HostServiceRequest<'_>,
        ) -> Result<Vec<u8>> {
            Self::linger(scope);
            Ok(request.payload.to_vec())
        }
        fn invoke_async(
            &self,
            scope: &InvocationScope,
            request: bmux_plugin_sdk::HostServiceOwnedRequest,
        ) -> bmux_plugin_sdk::definition::PluginFuture<'static, Result<Vec<u8>>> {
            Self::linger(scope);
            Box::pin(async move { Ok(request.payload) })
        }
    }
    let instance =
        bmux_plugin_runtime::InstanceScope::new(bmux_plugin_runtime::PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let port = services(activation.clone(), Arc::new(Lingering), None);
    let request = bmux_plugin_sdk::HostServiceOwnedRequest {
        capability: "example.read".into(),
        kind: bmux_plugin_sdk::ServiceKind::Query,
        interface: "example/v1".into(),
        operation: "read".into(),
        payload: vec![7],
    };
    assert_eq!(
        port.call_service_async(request.clone()).await.unwrap(),
        vec![7]
    );
    let synchronous = port.clone();
    let blocking = tokio::task::spawn_blocking(move || {
        synchronous.invoke(HostServiceRequest {
            capability: "example.read",
            kind: bmux_plugin_sdk::ServiceKind::Query,
            interface: "example/v1",
            operation: "read",
            payload: &[8],
        })
    });
    assert_eq!(blocking.await.unwrap().unwrap(), vec![8]);
    // Every fresh caller finished: the activation drains and finishes.
    activation.close().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
}

#[tokio::test]
async fn unsupported_activation_lookup_never_becomes_absent() {
    struct Unsupported;
    impl HostServiceDispatch for Unsupported {
        fn invoke(&self, _: &InvocationScope, _: HostServiceRequest<'_>) -> Result<Vec<u8>> {
            panic!("no sync fallback")
        }
    }
    let instance =
        bmux_plugin_runtime::InstanceScope::new(bmux_plugin_runtime::PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let port = services(activation.clone(), Arc::new(Unsupported), None);
    assert!(matches!(
        port.selected_service_availability(key("absent/v1")).await,
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation { .. })
    ));
    activation.close().unwrap();
    assert!(
        port.selected_service_availability(key("present/v1"))
            .await
            .is_err()
    );
}

async fn equivalent_native_contexts(
    context: &bmux_plugin_sdk::NativeServiceContext,
) -> (
    bmux_plugin_sdk::NativeLifecycleContext,
    bmux_plugin_sdk::NativeCommandContext,
) {
    let value = serde_json::to_value(context).unwrap();
    let mut lifecycle: bmux_plugin_sdk::NativeLifecycleContext =
        serde_json::from_value(value.clone()).unwrap();
    let mut command_value = value;
    command_value["command"] = serde_json::json!("inspect");
    command_value["arguments"] = serde_json::json!([]);
    let mut command: bmux_plugin_sdk::NativeCommandContext =
        serde_json::from_value(command_value).unwrap();
    lifecycle.host_service_port = context.host_service_port.clone();
    command.host_service_port = context.host_service_port.clone();
    assert_eq!(
        lifecycle
            .selected_service_availability(key("present/v1"))
            .await
            .unwrap(),
        SelectedServiceAvailability::Present
    );
    assert_eq!(
        command
            .selected_service_availability(key("absent/v1"))
            .await
            .unwrap(),
        SelectedServiceAvailability::Absent
    );
    (lifecycle, command)
}

#[tokio::test]
async fn native_context_availability_preserves_selected_activation_and_errors() {
    let instance =
        bmux_plugin_runtime::InstanceScope::new(bmux_plugin_runtime::PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let setup = activation
        .begin_lifecycle_invocation(bmux_plugin_runtime::LifecycleInvocationKind::Activation)
        .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut context = super::super::super::tests::native_test_service_context(vec![]);
    assert!(matches!(
        context
            .selected_service_availability(key("present/v1"))
            .await,
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "selected_service_availability"
        })
    ));
    context.host_service_port = Some(services(
        activation.clone(),
        Arc::new(Dispatch(calls.clone())),
        Some(setup.clone()),
    ));
    setup.close().unwrap();
    setup.finish().unwrap();
    activation.mark_active().unwrap();
    assert_eq!(
        context
            .selected_service_availability(key("present/v1"))
            .await
            .unwrap(),
        SelectedServiceAvailability::Present
    );
    assert_eq!(
        context
            .selected_service_availability(key("absent/v1"))
            .await
            .unwrap(),
        SelectedServiceAvailability::Absent
    );
    assert!(
        matches!(context.selected_service_availability(key("denied/v1")).await,
        Err(bmux_plugin_sdk::PluginError::ServiceProtocol { details }) if details == "access denied")
    );
    let (lifecycle, command) = equivalent_native_contexts(&context).await;
    {
        let observed = calls.lock().unwrap();
        assert_eq!(observed.len(), 5);
        assert!(
            observed
                .iter()
                .all(|call| call.id() != setup.id() && call.try_admit().is_err())
        );
        drop(observed);
    }
    context.cancellation.cancel();
    assert!(matches!(
        context
            .selected_service_availability(key("present/v1"))
            .await,
        Err(bmux_plugin_sdk::PluginError::InvocationCancelled)
    ));
    assert_eq!(calls.lock().unwrap().len(), 5);
    context.cancellation = bmux_plugin_sdk::CancellationToken::default();
    activation.close().unwrap();
    assert!(
        context
            .selected_service_availability(key("present/v1"))
            .await
            .is_err()
    );
    assert!(
        lifecycle
            .selected_service_availability(key("present/v1"))
            .await
            .is_err()
    );
    assert!(
        command
            .selected_service_availability(key("present/v1"))
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 5);
    context.host_service_port = Some(PluginHostServices::new(|_| panic!("no service fallback")));
    assert!(matches!(
        context
            .selected_service_availability(key("present/v1"))
            .await,
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation { .. })
    ));
}

#[tokio::test]
async fn selected_static_service_context_reaches_availability_after_setup_retirement() {
    use bmux_plugin_sdk::NativeServiceContext;
    use bmux_plugin_sdk::abi::{NativeOperation, NativeOutcome};
    use bmux_plugin_sdk::definition::{
        ComponentRole, PluginCallContext, PluginDefinition, PluginFuture, PluginMetadata,
    };
    struct Inspect;
    impl bmux_plugin_sdk::definition::PluginInstance for Inspect {
        fn invoke(
            &self,
            call: PluginCallContext,
            context: NativeServiceContext,
        ) -> PluginFuture<
            '_,
            bmux_plugin_sdk::definition::PluginResult<bmux_plugin_sdk::ServiceResponse>,
        > {
            Box::pin(async move {
                assert_eq!(context.host_service_port, call.host_services);
                assert_eq!(
                    context
                        .selected_service_availability(key("present/v1"))
                        .await
                        .unwrap(),
                    SelectedServiceAvailability::Present
                );
                Ok(bmux_plugin_sdk::ServiceResponse::ok(vec![7]))
            })
        }
    }
    let definition = PluginDefinition::new(
        PluginMetadata {
            plugin_id: "example.context".into(),
            version: "1".into(),
            roles: vec![ComponentRole::Backend],
            required_capabilities: vec![],
            contracts: vec![],
            manifest_toml: None,
        },
        |_| Ok(Arc::new(Inspect) as Arc<dyn bmux_plugin_sdk::definition::PluginInstance>),
    );
    let execution = super::super::DefinitionArtifact::from_definition(definition)
        .create(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
            ComponentRole::Backend,
            &[],
        )
        .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    execution
        .bind_host_dispatch(Arc::new(Dispatch(calls.clone())))
        .unwrap();
    let activation = execution
        .scope
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let setup = activation
        .begin_lifecycle_invocation(bmux_plugin_runtime::LifecycleInvocationKind::Activation)
        .unwrap();
    setup.close().unwrap();
    setup.finish().unwrap();
    activation.mark_active().unwrap();
    execution.state.lock().unwrap().activation = Some(activation.clone());
    let id = execution
        .begin(NativeOperation::Invoke {
            invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
            context: super::super::super::tests::native_test_service_context(vec![]),
        })
        .unwrap();
    let outcome = std::future::poll_fn(|cx| execution.poll(id, cx).unwrap()).await;
    assert!(matches!(outcome, NativeOutcome::Service(response) if response.payload == vec![7]));
    execution.acknowledge(id).await.unwrap();
    {
        let observed = calls.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_ne!(observed[0].id(), setup.id());
        assert!(observed[0].try_admit().is_err());
        drop(observed);
    }
    activation.close().unwrap();
}
