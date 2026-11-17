//! Actual scoped-loader acceptance; no source inventories or substitute runtime.
//! Dynamic cases are explicitly ignored until X1 supplies the native artifact.
#[path = "fixtures/scoped_loader_definition.rs"]
mod fixture;
use bmux_plugin::{DefinitionArtifact, LoadedExecution, OperationSignal};
use bmux_plugin_runtime::{PluginConcurrencyConfig, PluginRuntimeScopeId};
use bmux_plugin_sdk::abi::{NativeOperation, NativeOutcome};
use bmux_plugin_sdk::definition::{ComponentRole, PluginCreateContext};
use bmux_plugin_sdk::{NativeLifecycleContext, PluginInvocationId};
use std::{
    sync::Arc,
    task::{Context, Poll, Waker},
    time::Duration,
};

fn native_context() -> NativeLifecycleContext {
    serde_json::from_value(serde_json::json!({
        "plugin_id":"acceptance.scoped-loader",
        "host":{"product_name":"acceptance","product_version":"1",
            "plugin_api_version":{"major":1,"minor":0},
            "plugin_abi_version":{"major":2,"minor":0}},
        "connection":{"config_dir":"","runtime_dir":"","data_dir":"","state_dir":""}
    }))
    .unwrap()
}
fn poll(execution: &LoadedExecution, id: u64) -> Poll<NativeOutcome> {
    execution
        .poll(id, &mut Context::from_waker(Waker::noop()))
        .unwrap()
}
async fn outcome(execution: &LoadedExecution, id: u64) -> NativeOutcome {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Poll::Ready(outcome) = poll(execution, id) {
                break outcome;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded operation settlement")
}
async fn activate(
    artifact: &DefinitionArtifact,
    runtime: PluginRuntimeScopeId,
    role: ComponentRole,
) -> Arc<LoadedExecution> {
    activate_with(artifact, runtime, role, None).await
}
async fn activate_with(
    artifact: &DefinitionArtifact,
    runtime: PluginRuntimeScopeId,
    role: ComponentRole,
    events: Option<&Arc<bmux_plugin::EventBus>>,
) -> Arc<LoadedExecution> {
    let execution = artifact
        .create(runtime, role, &["acceptance.allowed".into()])
        .unwrap();
    if let Some(events) = events {
        execution.bind_event_bus(events).unwrap();
    }
    let id = execution
        .begin_activation(native_context(), PluginConcurrencyConfig::Concurrent)
        .unwrap();
    assert!(matches!(outcome(&execution, id).await, NativeOutcome::Unit));
    execution.acknowledge(id).await.unwrap();
    execution
}
fn begin(execution: &LoadedExecution, contract: &str) -> u64 {
    execution
        .begin(NativeOperation::Extension {
            invocation_id: PluginInvocationId::new(),
            contract: contract.into(),
            payload: vec![],
        })
        .unwrap()
}
async fn extension(execution: &LoadedExecution, contract: &str) -> Vec<u8> {
    let id = begin(execution, contract);
    let result = match outcome(execution, id).await {
        NativeOutcome::Bytes(bytes) => bytes,
        other => panic!("unexpected outcome {other:?}"),
    };
    execution.acknowledge(id).await.unwrap();
    result
}
async fn retire(execution: &LoadedExecution) {
    let id = execution
        .begin_deactivation(native_context())
        .await
        .unwrap();
    assert!(matches!(outcome(execution, id).await, NativeOutcome::Unit));
    execution.acknowledge(id).await.unwrap();
    execution.destroy().await.unwrap();
    execution.destroy().await.unwrap();
}
async fn parity(artifact: DefinitionArtifact) {
    let host = PluginRuntimeScopeId::new();
    let other_host = PluginRuntimeScopeId::new();
    let backend = activate(&artifact, host, ComponentRole::Backend).await;
    let frontend = activate(&artifact, host, ComponentRole::Frontend).await;
    let other = activate(&artifact, other_host, ComponentRole::Backend).await;
    for execution in [&backend, &frontend, &other] {
        let observed: PluginCreateContext =
            serde_json::from_slice(&extension(execution, "scope").await).unwrap();
        assert_eq!(&observed, execution.create_context());
    }
    assert_ne!(
        backend.create_context().instance,
        frontend.create_context().instance
    );
    assert_ne!(
        backend.create_context().runtime_scope,
        other.create_context().runtime_scope
    );
    let mutation = begin(&backend, "mutate");
    let first = outcome(&backend, mutation).await;
    assert!(matches!(&first, NativeOutcome::Bytes(bytes) if bytes == &vec![42; 64 * 1024]));
    let second = outcome(&backend, mutation).await;
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    assert_eq!(extension(&backend, "count").await, b"1");
    assert_eq!(extension(&frontend, "count").await, b"0");
    assert_eq!(extension(&other, "count").await, b"0");
    backend.acknowledge(mutation).await.unwrap();
    assert!(
        backend
            .poll(mutation, &mut Context::from_waker(Waker::noop()))
            .is_err()
    );
    retire(&frontend).await;
    retire(&other).await;
    retire(&backend).await;
}
#[tokio::test]
async fn scoped_static_loader_creation_and_retained_outcome_parity() {
    parity(DefinitionArtifact::from_definition(
        fixture::plugin_definition(),
    ))
    .await;
}
fn dynamic_artifact() -> DefinitionArtifact {
    let path = std::env::var_os("BMUX_ACCEPTANCE_NATIVE_ARTIFACT")
        .expect("set BMUX_ACCEPTANCE_NATIVE_ARTIFACT to X1's trusted scoped_loader_native cdylib");
    let path = std::path::PathBuf::from(path);
    assert!(path.is_absolute(), "artifact selection must be absolute");
    // SAFETY: the operator explicitly selects the trusted fixture built from the
    // sibling scoped_loader_native.rs. Never use untrusted plugin downloads.
    let artifact = unsafe { DefinitionArtifact::load(&path) }.unwrap();
    assert_eq!(artifact.metadata().plugin_id, "acceptance.scoped-loader");
    artifact
}
#[tokio::test]
#[ignore = "requires explicitly built native fixture; run during integration with BMUX_ACCEPTANCE_NATIVE_ARTIFACT"]
async fn scoped_dynamic_loader_buffer_growth_executes_mutation_once() {
    parity(dynamic_artifact()).await;
}
async fn pending_retirement(artifact: DefinitionArtifact) {
    let execution = activate(
        &artifact,
        PluginRuntimeScopeId::new(),
        ComponentRole::Backend,
    )
    .await;
    let pending = begin(&execution, "pending");
    assert!(poll(&execution, pending).is_pending());
    let weak = Arc::downgrade(&execution);
    drop(artifact); // Execution must retain the actual library, not just metadata.
    drop(execution); // Losing the observer cannot destroy a pending author future.
    let execution = weak
        .upgrade()
        .expect("pending execution remains host-owned");
    assert!(execution.destroy().await.is_err());
    assert!(execution.acknowledge(pending).await.is_err());
    execution.cancel(pending).unwrap();
    assert!(
        matches!(outcome(&execution, pending).await, NativeOutcome::Bytes(bytes) if bytes == b"cancelled")
    );
    assert!(
        execution.destroy().await.is_err(),
        "final outcome still needs acknowledgement"
    );
    execution.acknowledge(pending).await.unwrap();
    retire(&execution).await;
    drop(execution);
    assert!(
        weak.upgrade().is_none(),
        "retirement releases intentional owner retention"
    );
}
#[tokio::test]
async fn scoped_static_pending_cancellation_retains_owner_until_retirement() {
    pending_retirement(DefinitionArtifact::from_definition(
        fixture::plugin_definition(),
    ))
    .await;
}
#[tokio::test]
#[ignore = "requires explicitly built native fixture; run during integration with BMUX_ACCEPTANCE_NATIVE_ARTIFACT"]
async fn scoped_dynamic_pending_call_retains_library_after_observers_drop() {
    pending_retirement(dynamic_artifact()).await;
}
async fn evidence_rejection(artifact: DefinitionArtifact) {
    let execution = activate(
        &artifact,
        PluginRuntimeScopeId::new(),
        ComponentRole::Backend,
    )
    .await;
    for (interface, hash) in [
        ("acceptance.counter/v1", [8; 32]),
        ("wrong.route/v1", [7; 32]),
    ] {
        let native = native_context();
        let context = bmux_plugin_sdk::NativeServiceContext {
            host_service_port: None,
            plugin_id: "acceptance.scoped-loader".into(),
            request: bmux_plugin_sdk::ServiceRequest {
                caller_plugin_id: "acceptance.caller".into(),
                service: bmux_plugin_sdk::RegisteredService {
                    capability: bmux_plugin_sdk::HostScope::new("acceptance.allowed").unwrap(),
                    kind: bmux_plugin_sdk::ServiceKind::Command,
                    interface_id: "acceptance.counter/v1".into(),
                    provider: bmux_plugin_sdk::ProviderId::Plugin(
                        "acceptance.scoped-loader".into(),
                    ),
                },
                operation: "mutate".into(),
                payload: bmux_plugin_sdk::service_contract::encode(interface, &hash, b"mutation")
                    .unwrap(),
            },
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            host: native.host,
            connection: native.connection,
            settings: None,
            plugin_settings_map: Default::default(),
            caller_client_id: None,
            cancellation: Default::default(),
            host_kernel_bridge: None,
        };
        let request = execution.begin(NativeOperation::Invoke {
            invocation_id: PluginInvocationId::new(),
            context,
        });
        // Today scoped artifacts reject all unnegotiated checked requests. If
        // negotiation is added, the actual provider must still reject bad evidence.
        if let Ok(id) = request {
            assert!(matches!(
                outcome(&execution, id).await,
                NativeOutcome::Error { .. }
            ));
            execution.acknowledge(id).await.unwrap();
        }
        assert_eq!(extension(&execution, "count").await, b"0");
    }
    retire(&execution).await;
}
#[tokio::test]
async fn static_loader_evidence_mismatches_do_not_execute_mutation() {
    evidence_rejection(DefinitionArtifact::from_definition(
        fixture::plugin_definition(),
    ))
    .await;
}
#[tokio::test]
#[ignore = "requires explicitly built native fixture; run during integration with BMUX_ACCEPTANCE_NATIVE_ARTIFACT"]
async fn dynamic_loader_evidence_mismatches_do_not_execute_mutation() {
    evidence_rejection(dynamic_artifact()).await;
}

#[test]
fn creation_rejects_role_and_capability_mismatches_before_factory_effects() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let created = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&created);
    let mut metadata = fixture::plugin_definition().metadata;
    metadata.roles = vec![ComponentRole::Backend];
    let artifact = DefinitionArtifact::from_definition(
        bmux_plugin_sdk::definition::PluginDefinition::new(metadata, move |scope| {
            observed.fetch_add(1, Ordering::SeqCst);
            fixture::plugin_definition().create(scope)
        }),
    );
    assert!(
        artifact
            .create(
                PluginRuntimeScopeId::new(),
                ComponentRole::Frontend,
                &["acceptance.allowed".into()]
            )
            .is_err()
    );
    assert!(
        artifact
            .create(PluginRuntimeScopeId::new(), ComponentRole::Backend, &[])
            .is_err()
    );
    assert_eq!(created.load(Ordering::SeqCst), 0);
}

/// Observe one operation through its waker only, counting polls. Progress
/// must come from real wakes (timers, task completion), never repolling.
async fn observe(execution: &LoadedExecution, id: u64) -> (NativeOutcome, usize) {
    let signal = OperationSignal::new();
    let waker = signal.waker();
    let mut polls = 0;
    loop {
        polls += 1;
        if let Poll::Ready(outcome) = execution
            .poll(id, &mut Context::from_waker(&waker))
            .unwrap()
        {
            return (outcome, polls);
        }
        signal.wait().await.unwrap();
    }
}
async fn runtime_parity(artifact: DefinitionArtifact) {
    let execution = activate(
        &artifact,
        PluginRuntimeScopeId::new(),
        ComponentRole::Backend,
    )
    .await;
    for (contract, expected) in [
        ("rt-timer", "true:true"),
        ("rt-spawn", "42:42"),
        ("rt-abort", "true:true"),
        ("rt-panic", "true"),
    ] {
        let id = begin(&execution, contract);
        // Test guard only: a lost wake fails the test instead of hanging it.
        let (outcome, polls) =
            tokio::time::timeout(Duration::from_secs(10), observe(&execution, id))
                .await
                .expect("operation is woken");
        assert!(
            matches!(&outcome, NativeOutcome::Bytes(bytes) if bytes == expected.as_bytes()),
            "{contract}: {outcome:?}"
        );
        // ~35 ms of timers at a 1-2 ms repoll would take dozens of polls.
        assert!(polls <= 12, "{contract} observed {polls} polls");
        execution.acknowledge(id).await.unwrap();
    }
    // A panicking plugin task leaves the instance serving calls.
    assert_eq!(extension(&execution, "count").await, b"0");
    assert_eq!(extension(&execution, "rt-orphan").await, b"spawned");
    assert!(
        execution
            .pending_background_work()
            .contains(&"orphan".to_string()),
        "plug-rt task joined the activation scope"
    );
    // Server shutdown order: the activation's task scope stops admitting and
    // drains first; the host's own deactivation must still run afterwards.
    execution.cancel_background_tasks().unwrap();
    execution.drain_background_tasks().await.unwrap();
    retire(&execution).await;
    // Teardown aborted the orphan and its future was dropped through the
    // plugin before its scope registration settled.
    assert!(execution.pending_background_work().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_static_runtime_spawn_timer_cancel_panic_and_wake() {
    runtime_parity(DefinitionArtifact::from_definition(
        fixture::plugin_definition(),
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly built native fixture; run during integration with BMUX_ACCEPTANCE_NATIVE_ARTIFACT"]
async fn scoped_dynamic_runtime_spawn_timer_cancel_panic_and_wake() {
    runtime_parity(dynamic_artifact()).await;
}

/// The activation's event bus has one author API in both modes: the host
/// publishes and the plugin receives; the plugin registers and publishes and
/// a host subscriber receives; teardown releases the plugin's channel and
/// every host subscription it held.
async fn event_parity(artifact: DefinitionArtifact) {
    let bus = Arc::new(bmux_plugin::EventBus::new());
    bus.register_json_channel(fixture::HOST_EVENTS.into())
        .unwrap();
    let execution = activate_with(
        &artifact,
        PluginRuntimeScopeId::new(),
        ComponentRole::Backend,
        Some(&bus),
    )
    .await;
    // Plugin -> host.
    assert_eq!(extension(&execution, "ev-register").await, b"registered");
    let mut from_plugin = bus.subscribe_json(&fixture::PLUGIN_EVENTS.into()).unwrap();
    assert_eq!(extension(&execution, "ev-publish").await, b"published");
    let event = tokio::time::timeout(Duration::from_secs(10), from_plugin.recv())
        .await
        .expect("plugin event delivered")
        .unwrap();
    assert_eq!(event.payload, serde_json::json!({"from": "plugin"}));
    // Host -> plugin, observed through the operation's waker only.
    assert_eq!(extension(&execution, "ev-subscribe").await, b"subscribed");
    let receive = begin(&execution, "ev-receive");
    assert!(
        bus.emit_json(
            &fixture::HOST_EVENTS.into(),
            &serde_json::json!({"from": "host"})
        )
        .unwrap()
    );
    let (outcome, polls) =
        tokio::time::timeout(Duration::from_secs(10), observe(&execution, receive))
            .await
            .expect("host event delivered");
    assert!(
        matches!(&outcome, NativeOutcome::Bytes(bytes) if bytes == br#"{"from":"host"}"#),
        "{outcome:?}"
    );
    assert!(polls <= 12, "observed {polls} polls");
    execution.acknowledge(receive).await.unwrap();
    // Teardown in server order releases the activation's channels.
    execution.cancel_background_tasks().unwrap();
    execution.drain_background_tasks().await.unwrap();
    retire(&execution).await;
    assert!(matches!(
        bus.subscribe_json(&fixture::PLUGIN_EVENTS.into()),
        Err(bmux_plugin::EventBusError::ChannelNotRegistered { .. })
    ));
    assert_eq!(execution.event_port_subscriptions(), 0);
    assert_eq!(execution.pending_background_work(), Vec::<String>::new());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_static_event_bus_publish_subscribe_and_release() {
    event_parity(DefinitionArtifact::from_definition(
        fixture::plugin_definition(),
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly built native fixture; run during integration with BMUX_ACCEPTANCE_NATIVE_ARTIFACT"]
async fn scoped_dynamic_event_bus_publish_subscribe_and_release() {
    event_parity(dynamic_artifact()).await;
}

/// Serves the acceptance echo interface asynchronously; `hang` never answers
/// and records when the host drops (cancels) it.
struct EchoHost {
    cancelled: Arc<std::sync::atomic::AtomicUsize>,
}
impl bmux_plugin_sdk::HostServiceDispatch for EchoHost {
    fn invoke(
        &self,
        _: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        Ok(request.payload.to_vec())
    }
    fn invoke_async(
        &self,
        _: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, bmux_plugin_sdk::Result<Vec<u8>>> {
        struct Unanswered(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Unanswered {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let cancelled = Arc::clone(&self.cancelled);
        Box::pin(async move {
            assert_eq!(request.interface, fixture::ECHO_INTERFACE);
            if request.operation == "hang" {
                let _unanswered = Unanswered(cancelled);
                std::future::pending::<()>().await;
            }
            Ok(request.payload)
        })
    }
}

/// The host's services work the same in both modes: the asynchronous route
/// (with cancellation reaching the host) and the host subscription handle.
async fn host_service_parity(artifact: DefinitionArtifact) {
    let bus = Arc::new(bmux_plugin::EventBus::new());
    bus.register_json_channel(fixture::HOST_EVENTS.into())
        .unwrap();
    let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let execution = artifact
        .create(
            PluginRuntimeScopeId::new(),
            ComponentRole::Backend,
            &["acceptance.allowed".into()],
        )
        .unwrap();
    execution.bind_event_bus(&bus).unwrap();
    execution
        .bind_host_dispatch(Arc::new(EchoHost {
            cancelled: Arc::clone(&cancelled),
        }))
        .unwrap();
    let id = execution
        .begin_activation(native_context(), PluginConcurrencyConfig::Concurrent)
        .unwrap();
    assert!(matches!(outcome(&execution, id).await, NativeOutcome::Unit));
    execution.acknowledge(id).await.unwrap();

    let call = begin(&execution, "svc-async");
    let (reply, polls) = tokio::time::timeout(Duration::from_secs(10), observe(&execution, call))
        .await
        .expect("asynchronous host call settles");
    assert!(
        matches!(&reply, NativeOutcome::Bytes(bytes) if bytes == b"echo:ping"),
        "{reply:?}"
    );
    assert!(polls <= 12, "observed {polls} polls");
    execution.acknowledge(call).await.unwrap();

    assert_eq!(extension(&execution, "svc-cancel").await, b"true");
    // The plugin dropped its call; the host's future is dropped with it.
    tokio::time::timeout(Duration::from_secs(10), async {
        while cancelled.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancellation reaches the host");

    assert_eq!(
        extension(&execution, "ev-host-subscribe").await,
        b"subscribed"
    );
    let receive = begin(&execution, "ev-host-receive");
    assert!(
        bus.emit_json(&fixture::HOST_EVENTS.into(), &serde_json::json!({"n": 7}))
            .unwrap()
    );
    let (event, _) = tokio::time::timeout(Duration::from_secs(10), observe(&execution, receive))
        .await
        .expect("host event delivered");
    assert!(
        matches!(&event, NativeOutcome::Bytes(bytes) if bytes == br#"{"n":7}"#),
        "{event:?}"
    );
    execution.acknowledge(receive).await.unwrap();

    execution.cancel_background_tasks().unwrap();
    execution.drain_background_tasks().await.unwrap();
    retire(&execution).await;
    assert_eq!(execution.event_port_subscriptions(), 0);
    assert_eq!(execution.pending_background_work(), Vec::<String>::new());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_static_host_services_async_route_and_subscriptions() {
    host_service_parity(DefinitionArtifact::from_definition(
        fixture::plugin_definition(),
    ))
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly built native fixture; run during integration with BMUX_ACCEPTANCE_NATIVE_ARTIFACT"]
async fn scoped_dynamic_host_services_async_route_and_subscriptions() {
    host_service_parity(dynamic_artifact()).await;
}
