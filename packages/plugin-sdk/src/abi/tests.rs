use super::*;
use crate::definition::{
    ComponentRole, LoadedInstanceId, PluginMetadata, PluginResult, RuntimeScopeId,
};
use std::sync::atomic::AtomicUsize;

struct Echo {
    effects: Arc<AtomicUsize>,
}
impl PluginInstance for Echo {
    fn invoke_streaming(
        &self,
        call: PluginCallContext,
        _context: crate::NativeStreamingServiceContext,
    ) -> PluginFuture<'_, PluginResult<crate::ServiceResponse>> {
        Box::pin(async move {
            self.effects.fetch_add(1, Ordering::SeqCst);
            if call.cancellation.is_cancelled() {
                return Err(crate::PluginCommandError::unavailable("cancelled"));
            }
            Ok(crate::ServiceResponse::error("example", "terminal"))
        })
    }
    fn extension(
        &self,
        call: PluginCallContext,
        contract: String,
        payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            self.effects.fetch_add(1, Ordering::SeqCst);
            assert!(contract != "panic", "author failure");
            if contract == "cancel" {
                std::future::poll_fn(|_| {
                    if call.cancellation.is_cancelled() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            }
            if contract == "cancelled_host_report" {
                std::future::poll_fn(|_| {
                    if call.cancellation.is_cancelled() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                let host = call.host_services.as_ref().unwrap();
                let error = host
                    .invoke(crate::HostServiceRequest {
                        capability: "example",
                        kind: crate::ServiceKind::Query,
                        interface: "example/v1",
                        operation: "query",
                        payload: b"request",
                    })
                    .unwrap_err();
                assert!(matches!(error, crate::PluginError::InvocationCancelled));
                return Ok(payload);
            }
            if contract == "oversized" {
                return Ok(vec![0; MAX_BUFFER_BYTES]);
            }
            Ok(payload)
        })
    }
}
fn scope() -> PluginCreateContext {
    PluginCreateContext {
        runtime_scope: RuntimeScopeId::from_uuid(uuid::Uuid::from_u128(1)),
        instance: LoadedInstanceId::from_uuid(uuid::Uuid::from_u128(2)),
        role: ComponentRole::Backend,
    }
}
pub(super) fn adapter() -> (NativeAdapter, Arc<AtomicUsize>) {
    let effects = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&effects);
    let definition = PluginDefinition::new(
        PluginMetadata {
            plugin_id: "example.abi".into(),
            version: "1".into(),
            roles: vec![ComponentRole::Backend],
            required_capabilities: vec![],
            contracts: vec![],
            manifest_toml: None,
        },
        move |_context| {
            Ok(Arc::new(Echo {
                effects: Arc::clone(&observed),
            }) as Arc<dyn PluginInstance>)
        },
    );
    (NativeAdapter::new(definition), effects)
}
pub(super) fn create(adapter: &NativeAdapter) -> u64 {
    let bytes = serde_json::to_vec(&scope()).unwrap();
    let mut handle = 0;
    assert_eq!(
        unsafe { adapter.create(bytes.as_ptr(), bytes.len(), &raw mut handle) },
        OK
    );
    handle
}
fn active(adapter: &NativeAdapter, id: u64) {
    // Host lifecycle integration is tested separately. Establish a test-only
    // admitted active state to exercise outcome ownership without native fixtures.
    adapter
        .instance(id)
        .unwrap()
        .lifecycle
        .lock()
        .unwrap()
        .phase = 2;
}
fn start(adapter: &NativeAdapter, id: u64, op: u64, contract: &str, payload: Vec<u8>) -> AbiStatus {
    let request = NativeOperation::Extension {
        invocation_id: PluginInvocationId::new(),
        contract: contract.into(),
        payload,
    };
    let bytes = serde_json::to_vec(&request).unwrap();
    unsafe { adapter.start(id, op, bytes.as_ptr(), bytes.len()) }
}
fn read(adapter: &NativeAdapter, id: u64, op: u64) -> (AbiStatus, Vec<u8>) {
    let mut buffer = vec![0; MAX_BUFFER_BYTES];
    let mut length = 0;
    let status =
        unsafe { adapter.read(id, op, buffer.as_mut_ptr(), buffer.len(), &raw mut length) };
    buffer.truncate(length);
    (status, buffer)
}
#[test]
fn cancelled_host_call_retains_author_report_after_live_cancel_and_buffer_growth() {
    unsafe extern "C" fn ownership(_: u64) -> AbiStatus {
        OK
    }
    unsafe extern "C" fn cancelled(_: u64, _: *const NativeHostRequest, _: *mut u64) -> AbiStatus {
        CANCELLED
    }
    unsafe extern "C" fn unsupported_event(_: u64, _: *const u8, _: usize) -> AbiStatus {
        UNSUPPORTED
    }
    unsafe extern "C" fn unsupported_read(
        _: u64,
        _: u64,
        _: *mut u8,
        _: usize,
        _: *mut usize,
    ) -> AbiStatus {
        UNSUPPORTED
    }
    unsafe extern "C" fn unsupported_ack(_: u64, _: u64) -> AbiStatus {
        UNSUPPORTED
    }
    unsafe extern "C" fn unsupported_begin(
        _: u64,
        _: *const u8,
        _: usize,
        _: *mut u64,
    ) -> AbiStatus {
        UNSUPPORTED
    }
    let table = NativeInvocationPortsV3 {
        base: NativeInvocationPorts {
            size: u32::try_from(std::mem::size_of::<NativeInvocationPortsV3>()).unwrap(),
            version: PORTS_VERSION_V3,
            capabilities: CAP_HOST_SERVICES | CAP_LIVE_CANCELLATION,
            handle: 1,
            retain: ownership,
            release: ownership,
            emit_event: unsupported_event,
            host_begin: cancelled,
            host_read: unsupported_read,
            host_ack: unsupported_ack,
            registration_begin: unsupported_begin,
        },
        kernel_begin: unsupported_begin,
    };
    let (adapter, effects) = adapter();
    let id = create(&adapter);
    active(&adapter, id);
    let report = br#"{"committed":true,"cancelled":true}"#.to_vec();
    let operation = NativeOperation::Extension {
        invocation_id: PluginInvocationId::new(),
        contract: "cancelled_host_report".into(),
        payload: report.clone(),
    };
    let bytes = serde_json::to_vec(&operation).unwrap();
    assert_eq!(
        unsafe { adapter.start_scoped(id, 1, bytes.as_ptr(), bytes.len(), &raw const table.base) },
        PENDING
    );
    assert_eq!(read(&adapter, id, 1).0, PENDING);
    assert_eq!(adapter.cancel(id, 1), OK);
    let mut length = 0;
    assert_eq!(
        unsafe { adapter.read(id, 1, std::ptr::null_mut(), 0, &raw mut length) },
        BUFFER_TOO_SMALL
    );
    let (status, retained) = read(&adapter, id, 1);
    assert_eq!(status, OK);
    let outcome: NativeOutcome = serde_json::from_slice(&retained).unwrap();
    assert!(matches!(outcome, NativeOutcome::Bytes(bytes) if bytes == report));
    assert_eq!(read(&adapter, id, 1).1, retained);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.ack(id, 1), OK);
    assert!(
        !adapter
            .instance(id)
            .unwrap()
            .cleanup_required
            .load(Ordering::Acquire)
    );
}

fn async_activation_table() -> NativeInvocationPortsV3 {
    unsafe extern "C" fn ownership(_: u64) -> AbiStatus {
        OK
    }
    unsafe extern "C" fn event(_: u64, _: *const u8, _: usize) -> AbiStatus {
        UNSUPPORTED
    }
    unsafe extern "C" fn host(_: u64, _: *const NativeHostRequest, _: *mut u64) -> AbiStatus {
        UNSUPPORTED
    }
    unsafe extern "C" fn read_child(
        _: u64,
        _: u64,
        _: *mut u8,
        _: usize,
        _: *mut usize,
    ) -> AbiStatus {
        UNSUPPORTED
    }
    unsafe extern "C" fn ack_child(_: u64, _: u64) -> AbiStatus {
        UNSUPPORTED
    }
    unsafe extern "C" fn begin(_: u64, _: *const u8, _: usize, _: *mut u64) -> AbiStatus {
        UNSUPPORTED
    }
    NativeInvocationPortsV3 {
        base: NativeInvocationPorts {
            size: u32::try_from(std::mem::size_of::<NativeInvocationPortsV3>()).unwrap(),
            version: PORTS_VERSION_V3,
            capabilities: CAP_ASYNC_ACTIVATION,
            handle: 1,
            retain: ownership,
            release: ownership,
            emit_event: event,
            host_begin: host,
            host_read: read_child,
            host_ack: ack_child,
            registration_begin: begin,
        },
        kernel_begin: begin,
    }
}

#[test]
fn negotiated_native_async_activation_polls_timer_on_owned_runtime_and_retires() {
    struct AsyncAuthor(Arc<AtomicUsize>);
    impl PluginInstance for AsyncAuthor {
        fn activate_with_async(
            &self,
            _: PluginLifecycleContext,
            handle: crate::HostAsyncHandle,
        ) -> PluginFuture<'_, PluginResult<()>> {
            let seen = self.0.clone();
            Box::pin(async move {
                handle
                    .spawn_background("activation-probe", move |mut cancel| async move {
                        cancel.cancelled().await;
                        Ok(())
                    })
                    .map_err(crate::PluginCommandError::failed)?;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }
    let effects = Arc::new(AtomicUsize::new(0));
    let seen = effects.clone();
    let adapter = NativeAdapter::new(PluginDefinition::new(
        PluginMetadata {
            plugin_id: "example.abi".into(),
            version: "1".into(),
            roles: vec![ComponentRole::Backend],
            required_capabilities: vec![],
            contracts: vec![],
            manifest_toml: None,
        },
        move |_| Ok(Arc::new(AsyncAuthor(seen.clone())) as Arc<dyn PluginInstance>),
    ));
    let id = create(&adapter);
    let native = serde_json::from_value(serde_json::json!({
        "plugin_id": "example.abi",
        "host": {"product_name":"test", "product_version":"1", "plugin_api_version":crate::ApiVersion::new(1,1), "plugin_abi_version":crate::ApiVersion::new(1,1)},
        "connection": {"config_dir":"/test/config", "runtime_dir":"/test/runtime", "data_dir":"/test/data", "state_dir":"/test/state"}
    })).unwrap();
    let context = PluginLifecycleContext {
        callback_execution: None,
        scope: scope(),
        activation: ActivationId::from_uuid(uuid::Uuid::new_v4()),
        native,
    };
    let table = async_activation_table();
    let bytes = serde_json::to_vec(&NativeOperation::Activate(context.clone())).unwrap();
    assert_eq!(
        unsafe { adapter.start_scoped(id, 1, bytes.as_ptr(), bytes.len(), &raw const table.base) },
        PENDING
    );
    let settle = |operation| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let (status, bytes) = read(&adapter, id, operation);
            if status == OK {
                assert!(matches!(
                    serde_json::from_slice::<NativeOutcome>(&bytes).unwrap(),
                    NativeOutcome::Unit
                ));
                break;
            }
            assert!(matches!(status, PENDING | BUSY));
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(adapter.ack(id, operation), OK);
    };
    settle(1);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let bytes = serde_json::to_vec(&NativeOperation::Deactivate(context)).unwrap();
    assert_eq!(
        unsafe { adapter.start(id, 2, bytes.as_ptr(), bytes.len()) },
        PENDING
    );
    settle(2);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let status = adapter.destroy(id);
        if status == OK {
            break;
        }
        assert_eq!(status, BUSY);
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn metadata_does_not_create_or_execute() {
    let (adapter, effects) = adapter();
    let mut length = 0;
    assert_eq!(
        unsafe { adapter.metadata(std::ptr::null_mut(), 0, &raw mut length) },
        BUFFER_TOO_SMALL
    );
    assert!(length > 0);
    assert!(adapter.instances.lock().unwrap().is_empty());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}
#[test]
fn buffer_growth_retains_one_outcome_and_ack_does_not_allow_reexecution() {
    let (adapter, effects) = adapter();
    let id = create(&adapter);
    active(&adapter, id);
    assert_eq!(start(&adapter, id, 1, "echo", vec![1, 2, 3]), PENDING);
    let mut required = 0;
    assert_eq!(
        unsafe { adapter.read(id, 1, std::ptr::null_mut(), 0, &raw mut required) },
        BUFFER_TOO_SMALL
    );
    assert!(required > 3);
    let (status, bytes) = read(&adapter, id, 1);
    assert_eq!(status, OK);
    assert!(
        matches!(serde_json::from_slice::<NativeOutcome>(&bytes).unwrap(), NativeOutcome::Bytes(value) if value == [1, 2, 3])
    );
    assert_eq!(read(&adapter, id, 1).1, bytes);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.ack(id, 1), OK);
    assert_eq!(start(&adapter, id, 1, "echo", vec![]), INVALID_ARGUMENT);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}
#[test]
fn pending_cancellation_is_live_and_does_not_destroy_work() {
    let (adapter, effects) = adapter();
    let id = create(&adapter);
    active(&adapter, id);
    assert_eq!(start(&adapter, id, 1, "cancel", vec![]), PENDING);
    assert_eq!(read(&adapter, id, 1).0, PENDING);
    assert_eq!(adapter.ack(id, 1), BUSY);
    assert_eq!(adapter.destroy(id), BUSY);
    assert_eq!(adapter.cancel(id, 1), OK);
    assert_eq!(read(&adapter, id, 1).0, OK);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.ack(id, 1), OK);
    // Active lifetime still forbids destruction even with no operations.
    assert_eq!(adapter.destroy(id), BUSY);
}
#[test]
fn unwinding_panics_retain_cleanup_authority() {
    let (adapter, _) = adapter();
    let id = create(&adapter);
    active(&adapter, id);
    assert_eq!(start(&adapter, id, 1, "panic", vec![]), PENDING);
    assert_eq!(read(&adapter, id, 1).0, PANICKED);
    assert_eq!(adapter.ack(id, 1), OK);
    assert_eq!(adapter.destroy(id), FAILED);
    assert_eq!(start(&adapter, id, 2, "echo", vec![]), FAILED);
}
#[test]
fn inactive_instances_cannot_receive_work_or_reuse_handles() {
    let (adapter, _) = adapter();
    let first = create(&adapter);
    assert_eq!(start(&adapter, first, 1, "echo", vec![]), BUSY);
    assert_eq!(adapter.destroy(first), OK);
    let second = create(&adapter);
    assert_ne!(first, second);
    assert_eq!(adapter.cancel(first, 1), NOT_FOUND);
}
#[test]
fn oversized_outcome_is_bounded_and_not_reexecuted() {
    let (adapter, effects) = adapter();
    let id = create(&adapter);
    active(&adapter, id);
    assert_eq!(start(&adapter, id, 1, "oversized", vec![]), PENDING);
    assert_eq!(read(&adapter, id, 1).0, LIMIT_EXCEEDED);
    assert_eq!(read(&adapter, id, 1).0, LIMIT_EXCEEDED);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.ack(id, 1), OK);
}
#[test]
fn invalid_output_is_rejected_before_effects() {
    let (adapter, effects) = adapter();
    let id = create(&adapter);
    active(&adapter, id);
    assert_eq!(start(&adapter, id, 1, "echo", vec![]), PENDING);
    assert_eq!(
        unsafe { adapter.read(id, 1, std::ptr::null_mut(), 1, std::ptr::null_mut()) },
        INVALID_ARGUMENT
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}
#[test]
fn native_context_disallows_unnegotiated_streaming() {
    let request = br#"{"operation":"streaming","context":{}}"#;
    let (adapter, effects) = adapter();
    let id = create(&adapter);
    assert_eq!(
        unsafe { adapter.start(id, 1, request.as_ptr(), request.len()) },
        INVALID_ARGUMENT
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}
#[test]
fn registration_retrieval_is_retained_and_acknowledged() {
    let (adapter, effects) = adapter();
    let id = create(&adapter);
    active(&adapter, id);
    let input = serde_json::to_vec(&NativeOperation::Registrations).unwrap();
    assert_eq!(
        unsafe { adapter.start(id, 1, input.as_ptr(), input.len()) },
        PENDING
    );
    let mut length = 0;
    assert_eq!(
        unsafe { adapter.read(id, 1, std::ptr::null_mut(), 0, &raw mut length) },
        BUFFER_TOO_SMALL
    );
    let (status, bytes) = read(&adapter, id, 1);
    assert_eq!(status, OK);
    assert!(
        matches!(serde_json::from_slice::<NativeOutcome>(&bytes).unwrap(), NativeOutcome::Registrations(value)
        if value.contributions.is_empty() && value.contracts.is_empty())
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.ack(id, 1), OK);
}
#[test]
fn instance_outcomes_do_not_cross_host_scopes() {
    let (adapter, effects) = adapter();
    let first = create(&adapter);
    let second = create(&adapter);
    active(&adapter, first);
    active(&adapter, second);
    assert_eq!(start(&adapter, first, 1, "echo", vec![11]), PENDING);
    assert_eq!(start(&adapter, second, 1, "echo", vec![22]), PENDING);
    let first_outcome: NativeOutcome = serde_json::from_slice(&read(&adapter, first, 1).1).unwrap();
    let second_outcome: NativeOutcome =
        serde_json::from_slice(&read(&adapter, second, 1).1).unwrap();
    assert!(matches!(first_outcome, NativeOutcome::Bytes(bytes) if bytes == [11]));
    assert!(matches!(second_outcome, NativeOutcome::Bytes(bytes) if bytes == [22]));
    assert_eq!(effects.load(Ordering::SeqCst), 2);
}
#[test]
fn capacity_is_checked_before_calling_another_factory() {
    let created = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&created);
    let definition = PluginDefinition::new(
        PluginMetadata {
            plugin_id: "example.capacity".into(),
            version: "1".into(),
            roles: vec![ComponentRole::Backend],
            required_capabilities: vec![],
            contracts: vec![],
            manifest_toml: None,
        },
        move |_context| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Echo {
                effects: Arc::new(AtomicUsize::new(0)),
            }) as Arc<dyn PluginInstance>)
        },
    );
    let adapter = NativeAdapter::new(definition);
    for _ in 0..MAX_INSTANCES {
        create(&adapter);
    }
    let input = serde_json::to_vec(&scope()).unwrap();
    let mut handle = 0;
    assert_eq!(
        unsafe { adapter.create(input.as_ptr(), input.len(), &raw mut handle) },
        LIMIT_EXCEEDED
    );
    assert_eq!(created.load(Ordering::SeqCst), MAX_INSTANCES);
}
#[test]
fn descriptor_rejects_unknown_versions_and_short_headers() {
    for header in [
        NativeDescriptorHeader {
            size: 8,
            version: 1,
        },
        NativeDescriptorHeader {
            size: 8,
            version: ABI_VERSION,
        },
    ] {
        #[repr(C, align(8))]
        struct AlignedHeader(NativeDescriptorHeader);
        let aligned = AlignedHeader(header);
        let pointer = std::ptr::from_ref(&aligned).cast::<NativePluginDescriptor>();
        assert_eq!(
            unsafe { validate_descriptor(pointer) }.err(),
            Some(UNSUPPORTED)
        );
    }
    assert_eq!(
        unsafe { validate_descriptor(std::ptr::null()) }.err(),
        Some(INVALID_ARGUMENT)
    );
}
#[test]
fn abi_guard_contains_unwinding_panics() {
    assert_eq!(ffi_guard(|| panic!("boundary")), PANICKED);
}

mod author_declaration {
    crate::define_plugin!(
        factory = |_context| {
            Ok(Arc::new(Echo {
                effects: Arc::new(AtomicUsize::new(0)),
            }) as Arc<dyn PluginInstance>)
        },
        metadata = PluginMetadata {
            plugin_id: "example.declaration".into(),
            version: "1".into(),
            roles: vec![ComponentRole::Backend],
            required_capabilities: vec![],
            contracts: vec![],
            manifest_toml: None,
        }
    );
    use super::*;
}
// Exercise descriptor generation in exactly one packaging module. Author modules
// can coexist in an rlib without exporting colliding entrypoint symbols.
crate::export_plugin_entrypoint!(author_declaration::plugin_definition);
#[test]
fn descriptor_versions_negotiate_kernel_without_reinterpreting_v2() {
    unsafe extern "C" {
        fn bmux_plugin_entry_v2() -> *const NativePluginDescriptor;
    }
    let descriptor = unsafe { bmux_plugin_entry_v2() };
    let selected = unsafe { validate_descriptor(descriptor) }.unwrap();
    assert_eq!(selected.version, ABI_VERSION);
    assert_ne!(selected.capabilities & CAP_KERNEL_REQUESTS, 0);
    assert_ne!(
        selected.capabilities & CAP_SELECTED_SERVICE_AVAILABILITY_V1,
        0
    );
    assert_ne!(selected.capabilities & !SCOPED_CAPABILITIES_V3_LEGACY, 0);
    let mut legacy = NativePluginDescriptor {
        size: u32::try_from(std::mem::size_of::<NativePluginDescriptor>()).unwrap(),
        version: ABI_VERSION_V2,
        capabilities: SCOPED_CAPABILITIES_V2,
        start_scoped: selected.start_scoped,
        metadata: selected.metadata,
        create: selected.create,
        destroy: selected.destroy,
        start: selected.start,
        read: selected.read,
        cancel: selected.cancel,
        ack: selected.ack,
    };
    assert!(unsafe { validate_descriptor(&raw const legacy) }.is_ok());
    legacy.capabilities |= CAP_KERNEL_REQUESTS;
    assert_eq!(
        unsafe { validate_descriptor(&raw const legacy) }.err(),
        Some(UNSUPPORTED)
    );
    legacy.version = ABI_VERSION;
    assert_eq!(
        unsafe { validate_descriptor(&raw const legacy) }.err(),
        Some(UNSUPPORTED)
    );
    let selected_v3 = unsafe { validate_descriptor_v3(descriptor) }.unwrap();
    let mut length = 0;
    assert_eq!(
        unsafe { (selected_v3.declarations)(std::ptr::null_mut(), 0, &raw mut length) },
        BUFFER_TOO_SMALL
    );
    let mut bytes = vec![0; length];
    assert_eq!(
        unsafe { (selected_v3.declarations)(bytes.as_mut_ptr(), bytes.len(), &raw mut length) },
        OK
    );
    let declarations: crate::PluginDeclarations = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(declarations.modeled_service_contracts, vec![]);
    legacy.capabilities |= 1 << 63;
    assert_eq!(
        unsafe { validate_descriptor(&raw const legacy) }.err(),
        Some(UNSUPPORTED)
    );
}

#[test]
fn packaging_descriptor_is_explicit_and_versioned() {
    assert_eq!(ABI_VERSION, 3);
    assert_eq!(ENTRY_SYMBOL, "bmux_plugin_entry_v2");
    assert_eq!(
        author_declaration::plugin_definition().metadata.plugin_id,
        "example.declaration"
    );
}
