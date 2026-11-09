//! Static and dynamic activation services have one contract: background work
//! handed the activation's services reaches the host after `activate` returns,
//! each call on a fresh activation invocation, and is rejected once
//! deactivation begins without blocking lifecycle cleanup or leaking pins.
use super::*;
use bmux_plugin_sdk::definition::{
    ComponentRole, PluginDefinition, PluginFuture, PluginInstance, PluginLifecycleContext,
    PluginMetadata, PluginResult,
};
use bmux_plugin_sdk::{HostServiceDispatch, HostServiceRequest, PluginHostServices};
use std::time::Duration;

type Captured = Arc<Mutex<Option<PluginHostServices>>>;

/// Hands the activation's services to "background work" (the test thread).
struct Retainer(Captured);
impl PluginInstance for Retainer {
    fn activate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        *self.0.lock().unwrap() = context.native.host_service_port;
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
pub(super) struct Recorder(Mutex<Vec<InvocationScope>>);
impl HostServiceDispatch for Recorder {
    fn invoke(&self, scope: &InvocationScope, request: HostServiceRequest<'_>) -> Result<Vec<u8>> {
        self.0.lock().unwrap().push(scope.clone());
        Ok(request.payload.to_vec())
    }
}

pub(super) fn metadata() -> PluginMetadata {
    PluginMetadata {
        plugin_id: "test.activation-parity".into(),
        version: "1".into(),
        roles: vec![ComponentRole::Backend],
        required_capabilities: vec![],
        contracts: vec![],
        manifest_toml: None,
    }
}

fn definition(captured: Captured) -> PluginDefinition {
    PluginDefinition::new(metadata(), move |_| {
        Ok(Arc::new(Retainer(captured.clone())) as Arc<dyn PluginInstance>)
    })
}

/// Dynamic adapter for the in-process descriptor. One adapter per test binary
/// mirrors `export_plugin_entrypoint!`, which owns one adapter per library.
pub(super) mod native {
    use super::{Captured, Mutex, abi};
    use std::sync::{Arc, OnceLock};

    pub(super) fn captured() -> &'static Captured {
        static CAPTURED: OnceLock<Captured> = OnceLock::new();
        CAPTURED.get_or_init(|| Arc::new(Mutex::new(None)))
    }
    fn adapter() -> &'static abi::NativeAdapter {
        static ADAPTER: OnceLock<abi::NativeAdapter> = OnceLock::new();
        ADAPTER.get_or_init(|| abi::NativeAdapter::new(super::definition(captured().clone())))
    }
    unsafe extern "C" fn declarations(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().declarations(out, cap, len) })
    }
    unsafe extern "C" fn metadata(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().metadata(out, cap, len) })
    }
    unsafe extern "C" fn create(input: *const u8, len: usize, out: *mut u64) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().create(input, len, out) })
    }
    unsafe extern "C" fn destroy(id: u64) -> i32 {
        abi::ffi_guard(|| adapter().destroy(id))
    }
    unsafe extern "C" fn start(id: u64, op: u64, input: *const u8, len: usize) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().start(id, op, input, len) })
    }
    unsafe extern "C" fn start_scoped(
        id: u64,
        op: u64,
        input: *const u8,
        len: usize,
        ports: *const abi::NativeInvocationPorts,
    ) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().start_scoped(id, op, input, len, ports) })
    }
    unsafe extern "C" fn read(id: u64, op: u64, out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().read(id, op, out, cap, len) })
    }
    unsafe extern "C" fn cancel(id: u64, op: u64) -> i32 {
        abi::ffi_guard(|| adapter().cancel(id, op))
    }
    unsafe extern "C" fn ack(id: u64, op: u64) -> i32 {
        abi::ffi_guard(|| adapter().ack(id, op))
    }
    const fn descriptor(capabilities: u64) -> abi::NativePluginDescriptorV3 {
        abi::NativePluginDescriptorV3 {
            base: abi::NativePluginDescriptor {
                size: abi::NATIVE_DESCRIPTOR_SIZE,
                version: abi::ABI_VERSION,
                capabilities,
                start_scoped,
                metadata,
                create,
                destroy,
                start,
                read,
                cancel,
                ack,
            },
            declarations,
        }
    }
    pub(in super::super) static DESCRIPTOR: abi::NativePluginDescriptorV3 =
        descriptor(abi::SCOPED_CAPABILITIES);
    /// An older guest that never negotiated the retained activation port.
    pub(super) static WITHOUT_ACTIVATION_PORT: abi::NativePluginDescriptorV3 =
        descriptor(abi::SCOPED_CAPABILITIES & !abi::CAP_RETAINED_LIFECYCLE);
    /// A guest advertising a capability bit this host does not understand.
    pub(super) static UNKNOWN_CAPABILITY: abi::NativePluginDescriptorV3 =
        descriptor(abi::SCOPED_CAPABILITIES | (1 << 40));
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

pub(super) fn lifecycle_context() -> bmux_plugin_sdk::NativeLifecycleContext {
    let service = super::super::tests::native_test_service_context(vec![]);
    bmux_plugin_sdk::NativeLifecycleContext {
        plugin_id: "test.activation-parity".into(),
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
        host_kernel_bridge: None,
    }
}

fn call(services: &PluginHostServices) -> Result<Vec<u8>> {
    let services = services.clone();
    // Background work: a thread that outlives the activate call.
    std::thread::spawn(move || {
        services.invoke(HostServiceRequest {
            capability: "test.read",
            kind: bmux_plugin_sdk::ServiceKind::Query,
            interface: "test/v1",
            operation: "read",
            payload: b"ping",
        })
    })
    .join()
    .unwrap()
}

pub(super) fn selected_execution(
    artifact: &DefinitionArtifact,
    recorder: &Arc<Recorder>,
) -> Arc<LoadedExecution> {
    let execution = artifact
        .create(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
            ComponentRole::Backend,
            &[],
        )
        .unwrap();
    execution
        .bind_callback_runtime(tokio::runtime::Handle::current())
        .unwrap();
    execution.bind_host_dispatch(recorder.clone()).unwrap();
    let selected = recorder.clone();
    execution
        .bind_lifecycle_callback_factory(Arc::new(move |binding| {
            let selected = selected.clone();
            Ok(callbacks::LifecycleCallbackDispatch {
                callback: None,
                host: Some(binding.host_dispatch(Arc::new(move |_| {
                    Ok(selected.clone() as Arc<dyn HostServiceDispatch>)
                }))),
                kernel: None,
            })
        }))
        .unwrap();
    execution
}

async fn exercise(artifact: DefinitionArtifact, captured: &Captured) {
    let recorder = Arc::new(Recorder::default());
    let execution = selected_execution(&artifact, &recorder);
    let activation = execution
        .begin_activation(lifecycle_context(), PluginConcurrencyConfig::Concurrent)
        .unwrap();
    assert!(matches!(
        settle(&execution, activation).await,
        NativeOutcome::Unit
    ));
    execution.acknowledge(activation).await.unwrap();
    let services = captured
        .lock()
        .unwrap()
        .take()
        .expect("activation received host services");

    // After activate returned: every call reaches the host on its own fresh
    // activation invocation, which is retired once the call returns.
    assert_eq!(call(&services).unwrap(), b"ping");
    assert_eq!(call(&services).unwrap(), b"ping");
    {
        let observed = recorder.0.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_ne!(observed[0].id(), observed[1].id());
        assert!(observed.iter().all(|scope| scope.try_admit().is_err()));
        drop(observed);
    }

    // Deactivation begins: the retained services fail closed before dispatch,
    // and that rejection does not turn lifecycle cleanup into a failure.
    let deactivation = execution
        .begin_deactivation(lifecycle_context())
        .await
        .unwrap();
    assert!(call(&services).is_err());
    assert!(matches!(
        settle(&execution, deactivation).await,
        NativeOutcome::Unit
    ));
    execution.acknowledge(deactivation).await.unwrap();
    assert!(call(&services).is_err());
    assert_eq!(recorder.0.lock().unwrap().len(), 2);
    drop(services);
    // Clean destruction proves no admission, child or port pin was leaked.
    execution.destroy().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_activation_services_outlive_activate_and_fail_closed_at_deactivation() {
    let captured: Captured = Arc::new(Mutex::new(None));
    exercise(
        DefinitionArtifact::from_definition(definition(captured.clone())),
        &captured,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_activation_services_outlive_activate_and_fail_closed_at_deactivation() {
    // SAFETY: the descriptor is this test binary's own in-process adapter,
    // owned by the process handle for the artifact's whole lifetime.
    let artifact =
        unsafe { DefinitionArtifact::from_process_descriptor(&raw const native::DESCRIPTOR.base) }
            .unwrap();
    exercise(artifact, native::captured()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_guest_without_activation_port_is_never_handed_call_scoped_services() {
    // SAFETY: as above; same in-process adapter, narrower advertised bits.
    let artifact = unsafe {
        DefinitionArtifact::from_process_descriptor(&raw const native::WITHOUT_ACTIVATION_PORT.base)
    }
    .unwrap();
    let recorder = Arc::new(Recorder::default());
    let execution = selected_execution(&artifact, &recorder);
    // Activation-scoped services cannot cross this ABI, so activation is
    // refused explicitly rather than silently degraded to the activate call's
    // own (closing) invocation ports.
    let error = execution
        .begin_activation(lifecycle_context(), PluginConcurrencyConfig::Concurrent)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("retained lifecycle host services not negotiated"),
        "{error}"
    );
    assert!(recorder.0.lock().unwrap().is_empty());
    // Nothing was started natively, so the instance retires without pins.
    execution.destroy().await.unwrap();
}

#[test]
fn dynamic_guest_with_unknown_capability_bits_is_rejected_at_load() {
    // SAFETY: as above; validation must reject before any operation runs.
    let error = unsafe {
        DefinitionArtifact::from_process_descriptor(&raw const native::UNKNOWN_CAPABILITY.base)
    }
    .err()
    .expect("unknown capability bits are rejected")
    .to_string();
    assert!(error.contains("unsupported native descriptor"), "{error}");
}
