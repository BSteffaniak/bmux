//! Same author factory through direct Rust and the real ABI-v2 adapter.
//! These are behavioral tests, not source-text architecture assertions.
use bmux_plugin_sdk::abi::{self, NativeAdapter, NativeOperation, NativeOutcome};
use bmux_plugin_sdk::definition::{
    ActivationId, ComponentRole, LoadedInstanceId, PluginCallContext, PluginCreateContext,
    PluginDefinition, PluginFuture, PluginInstance, PluginLifecycleContext, PluginMetadata,
    PluginResult, RuntimeScopeId,
};
use bmux_plugin_sdk::{NativeLifecycleContext, PluginInvocationId, ServiceCancellation};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Counter {
    value: AtomicUsize,
}
impl PluginInstance for Counter {
    fn registrations(&self) -> PluginResult<bmux_plugin_sdk::definition::InstanceRegistrations> {
        self.value.fetch_add(1, Ordering::SeqCst);
        Ok(Default::default())
    }
    fn extension(
        &self,
        call: PluginCallContext,
        contract: String,
        _payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            match contract.as_str() {
                "mutate" => {
                    let value = self.value.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(vec![
                        u8::try_from(value).expect("bounded fixture counter");
                        8192
                    ])
                }
                "value" => Ok(self.value.load(Ordering::SeqCst).to_string().into_bytes()),
                "cancel" => {
                    std::future::poll_fn(move |_cx| {
                        if call.cancellation.is_cancelled() {
                            std::task::Poll::Ready(Ok(b"cancelled".to_vec()))
                        } else {
                            std::task::Poll::Pending
                        }
                    })
                    .await
                }
                _ => panic!("unknown fixture contract"),
            }
        })
    }
}
fn definition() -> PluginDefinition {
    PluginDefinition::new(
        PluginMetadata {
            plugin_id: "acceptance.counter".into(),
            version: "1.0.0".into(),
            roles: vec![ComponentRole::Backend, ComponentRole::Frontend],
            required_capabilities: vec![],
            contracts: vec![],
            manifest_toml: None,
        },
        |_scope| {
            Ok(Arc::new(Counter {
                value: AtomicUsize::new(0),
            }) as Arc<dyn PluginInstance>)
        },
    )
}
fn scope(host: u128, instance: u128, role: ComponentRole) -> PluginCreateContext {
    PluginCreateContext {
        cli_command: None,
        runtime_scope: RuntimeScopeId::from_uuid(uuid::Uuid::from_u128(host)),
        instance: LoadedInstanceId::from_uuid(uuid::Uuid::from_u128(instance)),
        role,
    }
}
fn lifecycle(scope: PluginCreateContext) -> PluginLifecycleContext {
    let native: NativeLifecycleContext = serde_json::from_value(serde_json::json!({
        "plugin_id": "acceptance.counter",
        "host": { "product_name": "acceptance", "product_version": "1",
            "plugin_api_version": {"major":1,"minor":0},
            "plugin_abi_version": {"major":2,"minor":0} },
        "connection": {"config_dir":"", "runtime_dir":"", "data_dir":"", "state_dir":""}
    }))
    .unwrap();
    PluginLifecycleContext {
        // Serialized ABI fixture: executable authority travels only in ports.
        callback_execution: None,
        scope,
        activation: ActivationId::from_uuid(uuid::Uuid::from_u128(1)),
        native,
        capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
    }
}
fn submit(adapter: &NativeAdapter, handle: u64, operation: u64, request: &NativeOperation) -> i32 {
    let input = serde_json::to_vec(request).unwrap();
    // SAFETY: input remains readable throughout this synchronous call.
    unsafe { adapter.start(handle, operation, input.as_ptr(), input.len()) }
}
fn create(adapter: &NativeAdapter, context: &PluginCreateContext) -> u64 {
    let input = serde_json::to_vec(context).unwrap();
    let mut handle = 0;
    // SAFETY: input/output allocations are valid and disjoint for this call.
    assert_eq!(
        unsafe { adapter.create(input.as_ptr(), input.len(), &mut handle) },
        abi::OK
    );
    assert_ne!(handle, 0);
    assert_eq!(
        submit(
            adapter,
            handle,
            1,
            &NativeOperation::Activate(lifecycle(context.clone()))
        ),
        abi::PENDING
    );
    let (status, data, _) = read(adapter, handle, 1, 1024);
    assert_eq!(status, abi::OK);
    assert!(matches!(
        serde_json::from_slice::<NativeOutcome>(&data).unwrap(),
        NativeOutcome::Unit
    ));
    assert_eq!(adapter.ack(handle, 1), abi::OK);
    handle
}
fn start(adapter: &NativeAdapter, handle: u64, operation: u64, contract: &str) -> i32 {
    submit(
        adapter,
        handle,
        operation,
        &NativeOperation::Extension {
            invocation_id: PluginInvocationId::new(),
            contract: contract.into(),
            payload: vec![],
        },
    )
}
fn read(
    adapter: &NativeAdapter,
    handle: u64,
    operation: u64,
    capacity: usize,
) -> (i32, Vec<u8>, usize) {
    let mut buffer = vec![0; capacity];
    let mut length = 0;
    // SAFETY: output is writable for capacity bytes; length is a separate allocation.
    let status = unsafe {
        adapter.read(
            handle,
            operation,
            buffer.as_mut_ptr(),
            capacity,
            &mut length,
        )
    };
    if status == abi::OK {
        assert!(length <= capacity);
        buffer.truncate(length);
    }
    (status, buffer, length)
}
fn bytes(adapter: &NativeAdapter, handle: u64, operation: u64) -> Vec<u8> {
    let (status, buffer, _) = read(adapter, handle, operation, abi::MAX_BUFFER_BYTES);
    assert_eq!(status, abi::OK);
    match serde_json::from_slice(&buffer).unwrap() {
        NativeOutcome::Bytes(value) => value,
        other => panic!("expected bytes, received {other:?}"),
    }
}
fn cleanup(adapter: &NativeAdapter, handle: u64, context: PluginCreateContext) {
    assert_eq!(
        submit(
            adapter,
            handle,
            100,
            &NativeOperation::Deactivate(lifecycle(context))
        ),
        abi::PENDING
    );
    let (status, data, _) = read(adapter, handle, 100, 1024);
    assert_eq!(status, abi::OK);
    assert!(matches!(
        serde_json::from_slice::<NativeOutcome>(&data).unwrap(),
        NativeOutcome::Unit
    ));
    assert_eq!(adapter.ack(handle, 100), abi::OK);
    assert_eq!(adapter.destroy(handle), abi::OK);
}
#[tokio::test]
async fn native_and_static_have_identical_mutation_outcomes() {
    let definition = definition();
    let host_scope =
        bmux_plugin_runtime::InstanceScope::new(bmux_plugin_runtime::PluginRuntimeScopeId::new());
    let context = PluginCreateContext {
        cli_command: None,
        runtime_scope: RuntimeScopeId::from_uuid(host_scope.runtime_id().as_uuid()),
        instance: LoadedInstanceId::from_uuid(host_scope.id().as_uuid()),
        role: ComponentRole::Backend,
    };
    let instance = definition.create(context.clone()).unwrap();
    let callback_host = bmux_plugin::LoaderCallbackHost::new(
        host_scope.clone(),
        Arc::new(instance.clone()),
        tokio::runtime::Handle::current(),
    );
    let activation = host_scope
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let setup = activation
        .begin_lifecycle_invocation(bmux_plugin_runtime::LifecycleInvocationKind::Activation)
        .unwrap();
    let mut activate = lifecycle(context.clone());
    activate.activation = ActivationId::from_uuid(activation.id().as_uuid());
    activate.callback_execution = Some(
        callback_host
            .bind_root(&activation, setup.clone(), ServiceCancellation::new())
            .unwrap()
            .context(),
    );
    instance.activate(activate).await.unwrap();
    setup.close().unwrap();
    setup.finish().unwrap();
    activation.mark_active().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    let cancellation = ServiceCancellation::new();
    let callback = callback_host
        .bind_root(&activation, invocation.clone(), cancellation.clone())
        .unwrap();
    let direct = instance
        .extension(
            PluginCallContext {
                callback_execution: Some(callback.context()),
                invocation_id: PluginInvocationId::new(),
                cancellation,
                host_services: None,
            },
            "mutate".into(),
            vec![],
        )
        .await
        .unwrap();
    let adapter = NativeAdapter::new(definition);
    let handle = create(&adapter, &context);
    assert_eq!(start(&adapter, handle, 2, "mutate"), abi::PENDING);
    assert_eq!(bytes(&adapter, handle, 2), direct);
    assert_eq!(adapter.ack(handle, 2), abi::OK);
    cleanup(&adapter, handle, context.clone());
    drop(callback);
    invocation.close().unwrap();
    invocation.drain().await.unwrap();
    invocation.finish().unwrap();
    activation.close().unwrap();
    activation.drain().await.unwrap();
    let cleanup_scope = activation
        .begin_lifecycle_invocation(bmux_plugin_runtime::LifecycleInvocationKind::Cleanup)
        .unwrap();
    let mut deactivate = lifecycle(context);
    deactivate.activation = ActivationId::from_uuid(activation.id().as_uuid());
    deactivate.callback_execution = Some(
        callback_host
            .bind_root(
                &activation,
                cleanup_scope.clone(),
                ServiceCancellation::new(),
            )
            .unwrap()
            .context(),
    );
    instance.deactivate(deactivate).await.unwrap();
    cleanup_scope.close().unwrap();
    cleanup_scope.finish().unwrap();
    activation.finish().unwrap();
    drop(callback_host);
    host_scope.close().unwrap();
    host_scope.drain().await.unwrap();
    host_scope.finish().unwrap();
}
#[test]
fn response_growth_and_retrieval_never_repeat_mutation() {
    let adapter = NativeAdapter::new(definition());
    let context = scope(1, 1, ComponentRole::Backend);
    let handle = create(&adapter, &context);
    assert_eq!(start(&adapter, handle, 2, "mutate"), abi::PENDING);
    let (status, _, required) = read(&adapter, handle, 2, 1);
    assert_eq!(status, abi::BUFFER_TOO_SMALL);
    assert!(required > 8192);
    let expected = bytes(&adapter, handle, 2);
    assert_eq!(expected, vec![1; 8192]);
    assert_eq!(bytes(&adapter, handle, 2), expected);
    assert_ne!(start(&adapter, handle, 2, "mutate"), abi::PENDING);
    assert_eq!(start(&adapter, handle, 3, "value"), abi::PENDING);
    assert_eq!(bytes(&adapter, handle, 3), b"1");
    assert_eq!(adapter.ack(handle, 2), abi::OK);
    assert_ne!(start(&adapter, handle, 2, "mutate"), abi::PENDING);
    assert_eq!(read(&adapter, handle, 2, 32).0, abi::NOT_FOUND);
    assert_ne!(adapter.destroy(handle), abi::OK);
    assert_eq!(adapter.ack(handle, 3), abi::OK);
    cleanup(&adapter, handle, context);
}
#[test]
fn live_cancellation_does_not_authorize_early_destruction() {
    let adapter = NativeAdapter::new(definition());
    let context = scope(1, 1, ComponentRole::Backend);
    let handle = create(&adapter, &context);
    assert_eq!(start(&adapter, handle, 2, "cancel"), abi::PENDING);
    assert_eq!(read(&adapter, handle, 2, 128).0, abi::PENDING);
    assert_eq!(adapter.cancel(handle, 2), abi::OK);
    assert_ne!(adapter.destroy(handle), abi::OK);
    assert_eq!(bytes(&adapter, handle, 2), b"cancelled");
    assert_eq!(adapter.ack(handle, 2), abi::OK);
    cleanup(&adapter, handle, context);
    assert_eq!(adapter.cancel(handle, 2), abi::NOT_FOUND);
}
#[test]
fn multiple_hosts_and_frontends_do_not_share_implementation_state() {
    let adapters = [
        NativeAdapter::new(definition()),
        NativeAdapter::new(definition()),
    ];
    let contexts = [
        scope(1, 1, ComponentRole::Backend),
        scope(1, 2, ComponentRole::Frontend),
        scope(2, 1, ComponentRole::Backend),
    ];
    let first = create(&adapters[0], &contexts[0]);
    let frontend = create(&adapters[0], &contexts[1]);
    let remote = create(&adapters[1], &contexts[2]);
    assert_eq!(start(&adapters[0], first, 2, "mutate"), abi::PENDING);
    assert_eq!(bytes(&adapters[0], first, 2), vec![1; 8192]);
    for (adapter, handle, context) in [
        (&adapters[0], frontend, contexts[1].clone()),
        (&adapters[1], remote, contexts[2].clone()),
    ] {
        assert_eq!(start(adapter, handle, 2, "value"), abi::PENDING);
        assert_eq!(bytes(adapter, handle, 2), b"0");
        assert_eq!(adapter.ack(handle, 2), abi::OK);
        cleanup(adapter, handle, context);
    }
    assert_eq!(adapters[0].ack(first, 2), abi::OK);
    cleanup(&adapters[0], first, contexts[0].clone());
}

#[test]
fn missing_scoped_ports_reject_before_effects_and_do_not_consume_operation_id() {
    let adapter = NativeAdapter::new(definition());
    let context = scope(1, 1, ComponentRole::Backend);
    let handle = create(&adapter, &context);
    let input = serde_json::to_vec(&NativeOperation::Extension {
        invocation_id: PluginInvocationId::new(),
        contract: "mutate".into(),
        payload: vec![],
    })
    .unwrap();
    // SAFETY: input is readable; a null ports pointer is an explicitly rejected
    // argument, not a forged callback table or executable address.
    assert_eq!(
        unsafe { adapter.start_scoped(handle, 2, input.as_ptr(), input.len(), std::ptr::null()) },
        abi::INVALID_ARGUMENT
    );
    assert_eq!(start(&adapter, handle, 2, "value"), abi::PENDING);
    assert_eq!(bytes(&adapter, handle, 2), b"0");
    assert_eq!(adapter.ack(handle, 2), abi::OK);
    cleanup(&adapter, handle, context);
}

#[test]
fn registration_outcome_is_retained_and_never_repeats_collection_on_read() {
    let adapter = NativeAdapter::new(definition());
    let context = scope(1, 1, ComponentRole::Backend);
    let handle = create(&adapter, &context);
    assert_eq!(
        submit(&adapter, handle, 2, &NativeOperation::Registrations),
        abi::PENDING
    );
    assert_eq!(read(&adapter, handle, 2, 1).0, abi::BUFFER_TOO_SMALL);
    let (status, first, _) = read(&adapter, handle, 2, 4096);
    assert_eq!(status, abi::OK);
    assert!(
        matches!(serde_json::from_slice::<NativeOutcome>(&first).unwrap(),
        NativeOutcome::Registrations(value) if value.contributions.is_empty() && value.contracts.is_empty())
    );
    assert_eq!(read(&adapter, handle, 2, 4096).1, first);
    assert_eq!(start(&adapter, handle, 3, "value"), abi::PENDING);
    assert_eq!(bytes(&adapter, handle, 3), b"1");
    assert_eq!(adapter.ack(handle, 2), abi::OK);
    assert_eq!(adapter.ack(handle, 3), abi::OK);
    cleanup(&adapter, handle, context);
}

struct FailedActivation;
impl PluginInstance for FailedActivation {
    fn activate(&self, _context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async {
            Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                "partial activation",
            ))
        })
    }
}
#[test]
fn partial_activation_failure_requires_explicit_cleanup_before_destroy() {
    let definition = PluginDefinition::new(definition().metadata, |_context| {
        Ok(Arc::new(FailedActivation) as Arc<dyn PluginInstance>)
    });
    let adapter = NativeAdapter::new(definition);
    let context = scope(1, 1, ComponentRole::Backend);
    let input = serde_json::to_vec(&context).unwrap();
    let mut handle = 0;
    // SAFETY: borrowed input and writable handle are valid for this call.
    assert_eq!(
        unsafe { adapter.create(input.as_ptr(), input.len(), &mut handle) },
        abi::OK
    );
    assert_eq!(
        submit(
            &adapter,
            handle,
            1,
            &NativeOperation::Activate(lifecycle(context.clone()))
        ),
        abi::PENDING
    );
    let (status, data, _) = read(&adapter, handle, 1, 1024);
    assert_eq!(status, abi::OK);
    assert!(
        matches!(serde_json::from_slice::<NativeOutcome>(&data).unwrap(),
        NativeOutcome::Error { message, .. } if message == "partial activation")
    );
    assert_eq!(adapter.ack(handle, 1), abi::OK);
    assert_ne!(adapter.destroy(handle), abi::OK);
    assert_ne!(start(&adapter, handle, 2, "mutate"), abi::PENDING);
    cleanup(&adapter, handle, context);
}

struct Blocking {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl PluginInstance for Blocking {
    fn extension(
        &self,
        _call: PluginCallContext,
        _contract: String,
        _payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            Ok(b"settled".to_vec())
        })
    }
}
#[test]
fn concurrent_poll_is_busy_and_cancellation_cannot_free_polling_state() {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let implementation: Arc<dyn PluginInstance> = Arc::new(Blocking {
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    });
    let definition = PluginDefinition::new(definition().metadata, move |_context| {
        Ok(Arc::clone(&implementation))
    });
    // One scoped fixture instance only; this intentionally shares its blocking gate.
    let adapter = Arc::new(NativeAdapter::new(definition));
    let context = scope(1, 1, ComponentRole::Backend);
    let handle = create(&adapter, &context);
    assert_eq!(start(&adapter, handle, 2, "blocking"), abi::PENDING);
    let worker = Arc::clone(&adapter);
    let poller = std::thread::spawn(move || bytes(&worker, handle, 2));
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    assert_eq!(read(&adapter, handle, 2, 1024).0, abi::BUSY);
    assert_eq!(adapter.cancel(handle, 2), abi::OK);
    assert_eq!(adapter.ack(handle, 2), abi::BUSY);
    assert_ne!(adapter.destroy(handle), abi::OK);
    release_tx.send(()).unwrap();
    assert_eq!(poller.join().unwrap(), b"settled");
    assert_eq!(adapter.ack(handle, 2), abi::OK);
    cleanup(&adapter, handle, context);
}
