//! A host-registered activation binder receives the loader's actual activation
//! scope and delivers in-process values through the lifecycle context. What it
//! mints from that scope is revoked when the activation closes; a component
//! without a binder fails explicitly; dynamic artifacts are refused.
use super::*;
use bmux_plugin_sdk::definition::{
    ActivationCapabilities, ComponentRole, PluginDefinition, PluginFuture, PluginInstance,
    PluginLifecycleContext, PluginMetadata, PluginResult,
};
use std::time::Duration;

/// A host value minted from (and owned by) one activation scope.
struct HostPort(ActivationScope);

#[derive(Default)]
struct Observed {
    /// What the author received: its context's activation id and the port.
    activation: Option<ActivationId>,
    port: Option<HostPort>,
    released: bool,
}

struct Author(Arc<Mutex<Observed>>);
impl PluginInstance for Author {
    fn activate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        let observed = self.0.clone();
        Box::pin(async move {
            let port = context.capabilities.take::<HostPort>()?;
            let mut observed = observed.lock().unwrap();
            observed.activation = Some(context.activation);
            observed.port = Some(port);
            drop(observed);
            Ok(())
        })
    }
    fn deactivate(&self, _: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        let observed = self.0.clone();
        Box::pin(async move {
            let mut observed = observed.lock().unwrap();
            observed.port = None;
            observed.released = true;
            drop(observed);
            Ok(())
        })
    }
}

fn metadata() -> PluginMetadata {
    PluginMetadata {
        plugin_id: "test.activation-binder".into(),
        version: "1".into(),
        roles: vec![ComponentRole::Frontend],
        required_capabilities: vec![],
        contracts: vec![],
        manifest_toml: None,
    }
}

fn execution(observed: &Arc<Mutex<Observed>>) -> Arc<LoadedExecution> {
    let observed = observed.clone();
    let artifact =
        DefinitionArtifact::from_definition(PluginDefinition::new(metadata(), move |_| {
            Ok(Arc::new(Author(observed.clone())) as Arc<dyn PluginInstance>)
        }));
    let execution = artifact
        .create(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
            ComponentRole::Frontend,
            &[],
        )
        .unwrap();
    execution
        .bind_callback_runtime(tokio::runtime::Handle::current())
        .unwrap();
    execution
}

async fn settle(execution: &LoadedExecution, id: u64) -> NativeOutcome {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let polled = execution
                .poll(id, &mut Context::from_waker(std::task::Waker::noop()))
                .unwrap();
            match polled {
                Poll::Ready(outcome) => break outcome,
                Poll::Pending => tokio::time::sleep(Duration::from_millis(1)).await,
            }
        }
    })
    .await
    .expect("lifecycle operation settles within the fixture budget")
}

fn lifecycle_context() -> NativeLifecycleContext {
    let service = super::super::tests::native_test_service_context(vec![]);
    NativeLifecycleContext {
        plugin_id: "test.activation-binder".into(),
        required_capabilities: vec![],
        provided_capabilities: vec![],
        services: vec![],
        available_capabilities: vec![],
        enabled_plugins: vec![],
        plugin_search_roots: vec![],
        registered_plugins: vec![],
        host: service.host,
        connection: service.connection,
        settings: None,
        plugin_settings_map: BTreeMap::new(),
        auxiliary_executables: BTreeMap::new(),
        auxiliary_executable_errors: BTreeMap::new(),
        host_service_port: None,
        capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
        host_kernel_bridge: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binder_receives_actual_scope_and_its_values_are_released_on_deactivation() {
    let observed = Arc::new(Mutex::new(Observed::default()));
    let execution = execution(&observed);
    let host_side: Arc<Mutex<Vec<ActivationScope>>> = Arc::default();
    let retained = host_side.clone();
    execution
        .bind_activation_binder(Arc::new(
            move |scope: &ActivationScope, capabilities: &ActivationCapabilities| {
                // The host keeps its half; the component gets the other.
                retained.lock().unwrap().push(scope.clone());
                capabilities
                    .insert(HostPort(scope.clone()))
                    .map_err(|error| failure(error.message))
            },
        ))
        .unwrap();
    let id = execution
        .begin_activation(lifecycle_context(), PluginConcurrencyConfig::Concurrent)
        .unwrap();
    // A binder can never be added once the activation exists.
    assert!(
        execution
            .bind_activation_binder(Arc::new(
                |_: &ActivationScope, _: &ActivationCapabilities| Ok(())
            ))
            .unwrap_err()
            .to_string()
            .contains("before activation")
    );
    assert!(matches!(settle(&execution, id).await, NativeOutcome::Unit));
    execution.acknowledge(id).await.unwrap();

    let actual = execution.activation().unwrap();
    let bound = host_side.lock().unwrap().clone();
    assert_eq!(bound.len(), 1, "binder runs exactly once per activation");
    // The binder saw the loader's real activation, not a decoded identity.
    assert_eq!(bound[0].id(), actual.id());
    {
        let observed = observed.lock().unwrap();
        assert_eq!(
            observed.activation.unwrap().as_uuid(),
            actual.id().as_uuid()
        );
        let port = observed
            .port
            .as_ref()
            .expect("component received the value");
        assert_eq!(port.0.id(), actual.id());
        drop(port.0.try_admit().expect("live activation admits"));
        drop(observed);
    }

    let cleanup = execution
        .begin_deactivation(lifecycle_context())
        .await
        .unwrap();
    // Deactivation closed the scope the binder minted from: both halves fail.
    assert!(bound[0].try_admit().is_err());
    assert!(matches!(
        settle(&execution, cleanup).await,
        NativeOutcome::Unit
    ));
    execution.acknowledge(cleanup).await.unwrap();
    {
        let observed = observed.lock().unwrap();
        assert!(observed.released && observed.port.is_none());
        drop(observed);
    }
    drop(bound);
    host_side.lock().unwrap().clear();
    // Clean destruction proves no admission or scope pin was leaked.
    execution.destroy().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_binder_fails_activation_explicitly() {
    let observed = Arc::new(Mutex::new(Observed::default()));
    let execution = execution(&observed);
    let id = execution
        .begin_activation(lifecycle_context(), PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let outcome = settle(&execution, id).await;
    assert!(
        matches!(&outcome, NativeOutcome::Error { message, .. }
            if message.contains("not supplied by host")),
        "{outcome:?}"
    );
    assert!(observed.lock().unwrap().port.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_binder_rejects_activation_before_author_entry() {
    let observed = Arc::new(Mutex::new(Observed::default()));
    let execution = execution(&observed);
    execution
        .bind_activation_binder(Arc::new(
            |_: &ActivationScope, _: &ActivationCapabilities| {
                Err(failure("host cannot supply this capability"))
            },
        ))
        .unwrap();
    let error = execution
        .begin_activation(lifecycle_context(), PluginConcurrencyConfig::Concurrent)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("host cannot supply this capability"),
        "{error}"
    );
    assert!(observed.lock().unwrap().activation.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_artifact_is_refused_an_activation_binder() {
    // SAFETY: the descriptor is this test binary's own in-process adapter,
    // owned by the process handle for the artifact's whole lifetime.
    let artifact = unsafe {
        DefinitionArtifact::from_process_descriptor(
            &raw const super::activation_parity_tests::native::DESCRIPTOR.base.base,
        )
    }
    .unwrap();
    let execution = artifact
        .create(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
            ComponentRole::Backend,
            &[],
        )
        .unwrap();
    let error = execution
        .bind_activation_binder(Arc::new(
            |_: &ActivationScope, _: &ActivationCapabilities| Ok(()),
        ))
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot cross the native ABI"), "{error}");
    execution.destroy().await.unwrap();
}
