use super::*;
use bmux_plugin_sdk::typed_dispatch::{
    SELECTED_SERVICE_AVAILABILITY_VERSION, SelectedServiceAvailability, ServiceAvailabilityKey,
};

struct AvailabilityDispatch {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    invocation: bmux_plugin_runtime::RuntimeInvocationId,
    fail: bool,
}
impl bmux_plugin_sdk::HostServiceDispatch for AvailabilityDispatch {
    fn invoke(&self, _: &InvocationScope, _: HostServiceRequest<'_>) -> Result<Vec<u8>> {
        panic!("availability must not invoke a service")
    }
    fn selected_service_availability(
        &self,
        invocation: &InvocationScope,
        key: ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, Result<SelectedServiceAvailability>>
    {
        assert_eq!(invocation.id(), self.invocation);
        let count = self.calls.clone();
        let fail = self.fail;
        Box::pin(async move {
            count.fetch_add(1, Ordering::SeqCst);
            // Force a Pending poll before the asynchronous completion.
            let mut yielded = false;
            std::future::poll_fn(move |context| {
                if yielded {
                    std::task::Poll::Ready(())
                } else {
                    yielded = true;
                    context.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            let (send, receive) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let _ = send.send(());
            });
            receive.await.map_err(|error| failure(error.to_string()))?;
            if fail {
                return Err(failure("authoritative lookup failed"));
            }
            Ok(if key.interface_id == "present/v1" {
                SelectedServiceAvailability::Present
            } else {
                SelectedServiceAvailability::Absent
            })
        })
    }
}

fn availability_key(interface_id: &str) -> Vec<u8> {
    bmux_plugin_sdk::encode_service_message(&ServiceAvailabilityKey {
        capability: "example.read".into(),
        kind: ServiceKind::Query,
        interface_id: interface_id.into(),
    })
    .unwrap()
}

fn assert_availability_outcome(
    table: &NativeInvocationPorts,
    child: u64,
    fail: bool,
    interface: &str,
) {
    let mut length = 0;
    for _ in 0..2 {
        assert_eq!(
            unsafe {
                (table.host_read)(
                    table.handle,
                    child,
                    std::ptr::null_mut(),
                    0,
                    &raw mut length,
                )
            },
            if fail {
                abi::FAILED
            } else {
                abi::BUFFER_TOO_SMALL
            }
        );
    }
    if fail {
        assert_eq!(length, 0);
        return;
    }
    let mut output = vec![0; length];
    assert_eq!(
        unsafe {
            (table.host_read)(
                table.handle,
                child,
                output.as_mut_ptr(),
                output.len(),
                &raw mut length,
            )
        },
        abi::OK
    );
    let result: SelectedServiceAvailability = bmux_codec::from_typed_bytes(&output).unwrap();
    assert_eq!(
        result,
        if interface == "present/v1" {
            SelectedServiceAvailability::Present
        } else {
            SelectedServiceAvailability::Absent
        }
    );
}

#[test]
fn availability_rejects_invalid_keys_and_unbound_or_unsupported_routes() {
    struct Unsupported;
    impl bmux_plugin_sdk::HostServiceDispatch for Unsupported {
        fn invoke(&self, _: &InvocationScope, _: HostServiceRequest<'_>) -> Result<Vec<u8>> {
            panic!("no service invocation allowed")
        }
    }
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    let port = HostPorts::new(
        invocation.clone(),
        None,
        Some(BoundHostServicePort::new(
            invocation.clone(),
            Arc::new(Unsupported),
        )),
    );
    let table = port.install_for(abi::ABI_VERSION, None).unwrap();
    let InvocationTable::V3Availability(extension) = &table else {
        panic!("availability required")
    };
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    for input in [
        b"{\"capability\":\"forged\"}".to_vec(),
        availability_key(""),
    ] {
        let mut child = 99;
        assert_ne!(
            unsafe {
                (extension.availability_begin)(
                    table.handle,
                    SELECTED_SERVICE_AVAILABILITY_VERSION,
                    input.as_ptr(),
                    input.len(),
                    &raw mut child,
                )
            },
            abi::OK
        );
        assert_eq!(child, 0);
        assert!(port.state.lock().unwrap().children.is_empty());
    }
    let input = availability_key("present/v1");
    let mut child = 0;
    assert_eq!(
        unsafe {
            (extension.availability_begin)(
                table.handle,
                SELECTED_SERVICE_AVAILABILITY_VERSION,
                input.as_ptr(),
                input.len(),
                &raw mut child,
            )
        },
        abi::OK
    );
    invocation.close().unwrap();
    // Unsupported is a retained error, never an encoded Absent; cancellation
    // after settlement must not prevent read/ack recovery.
    assert_availability_outcome(&table, child, true, "present/v1");
    assert_eq!(unsafe { (table.host_ack)(table.handle, child) }, abi::OK);
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    HostPorts::retire(&table).unwrap();

    let bare = HostPorts::new(invocation, None, None);
    let table = bare.install_for(abi::ABI_VERSION, None).unwrap();
    assert_eq!(
        table.capabilities & abi::CAP_SELECTED_SERVICE_AVAILABILITY_V1,
        0
    );
    assert_eq!(
        unsafe {
            availability_begin(
                table.handle,
                SELECTED_SERVICE_AVAILABILITY_VERSION,
                input.as_ptr(),
                input.len(),
                &raw mut child,
            )
        },
        abi::UNSUPPORTED
    );
    HostPorts::retire(&table).unwrap();
}

#[test]
fn availability_rejects_foreign_host_scope_before_table_publication() {
    struct Unsupported;
    impl bmux_plugin_sdk::HostServiceDispatch for Unsupported {
        fn invoke(&self, _: &InvocationScope, _: HostServiceRequest<'_>) -> Result<Vec<u8>> {
            panic!("foreign port must never dispatch")
        }
    }
    let first = InstanceScope::new(PluginRuntimeScopeId::new());
    let second = InstanceScope::new(PluginRuntimeScopeId::new());
    let first_activation = first
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let second_activation = second
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    first_activation.mark_active().unwrap();
    second_activation.mark_active().unwrap();
    let invocation = first_activation.begin_invocation().unwrap();
    let foreign = second_activation.begin_invocation().unwrap();
    let port = HostPorts::new(
        invocation,
        None,
        Some(BoundHostServicePort::new(foreign, Arc::new(Unsupported))),
    );
    assert!(port.install_for(abi::ABI_VERSION, None).is_err());
}

#[test]
fn activation_availability_preserves_selected_async_binding() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let invocation = activation
        .begin_lifecycle_invocation(bmux_plugin_runtime::LifecycleInvocationKind::Activation)
        .unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = BoundHostServicePort::new(
        invocation.clone(),
        Arc::new(AvailabilityDispatch {
            calls: calls.clone(),
            invocation: invocation.id(),
            fail: false,
        }),
    );
    let services = bmux_plugin_sdk::PluginHostServices::from_bound(host);
    let port = HostPorts::for_activation(invocation.clone(), activation, services);
    let table = port.install_for(abi::ABI_VERSION, None).unwrap();
    let InvocationTable::V3Availability(extension) = &table else {
        panic!("availability required")
    };
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    let input = availability_key("present/v1");
    let mut child = 0;
    assert_eq!(
        unsafe {
            (extension.availability_begin)(
                table.handle,
                SELECTED_SERVICE_AVAILABILITY_VERSION,
                input.as_ptr(),
                input.len(),
                &raw mut child,
            )
        },
        abi::OK
    );
    invocation.close().unwrap();
    assert_availability_outcome(&table, child, false, "present/v1");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(unsafe { (table.host_ack)(table.handle, child) }, abi::OK);
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    HostPorts::retire(&table).unwrap();
}

fn retained_slice(bytes: &[u8]) -> abi::retained::NativeRetainedSliceV1 {
    abi::retained::NativeRetainedSliceV1 {
        pointer: bytes.as_ptr(),
        length: u32::try_from(bytes.len()).unwrap(),
    }
}

fn assert_default_lookup_unsupported(v2: &abi::retained::NativeRetainedHostPortV2) {
    let request = abi::retained::NativeRetainedRequestV1 {
        size: u32::try_from(std::mem::size_of::<abi::retained::NativeRetainedRequestV1>()).unwrap(),
        version: 1,
        kind: 0,
        capability: retained_slice(b"example.read"),
        interface: retained_slice(b"example/v1"),
        operation: retained_slice(b"read"),
        payload: retained_slice(b""),
    };
    let reservation = abi::retained::NativeRetainedReservationV1 {
        size: u32::try_from(std::mem::size_of::<
            abi::retained::NativeRetainedReservationV1,
        >())
        .unwrap(),
        version: 1,
        max_outcome_bytes: 32,
        operation_slots: 1,
        reserved: 0,
    };
    let mut recovered = 99;
    assert_eq!(
        unsafe {
            (v2.lookup)(
                v2.base.handle,
                1,
                &raw const request,
                &raw const reservation,
                &raw mut recovered,
            )
        },
        abi::UNSUPPORTED
    );
    assert_eq!(recovered, 0);
}

#[test]
fn retained_open_requires_bound_scope_and_explicit_version() {
    struct Declined;
    impl bmux_plugin_sdk::abi::retained::RetainedHostDispatch for Declined {
        fn begin(
            &self,
            _: u64,
            _: &HostServiceRequest<'_>,
            _: abi::retained::NativeRetainedReservationV1,
        ) -> Result<u64> {
            panic!("no effects expected")
        }
        fn observe(&self, _: u64, _: u64) -> Result<abi::retained::RetainedHostObservation> {
            panic!("no observation expected")
        }
        fn control(&self, _: u64, _: u32) -> Result<bool> {
            Ok(true)
        }
        fn acknowledge(&self, _: u64, _: u64) -> Result<()> {
            Ok(())
        }
        fn release(&self) -> Result<bool> {
            Ok(true)
        }
    }
    let execution = super::super::tests::fixture();
    let invocation = execution.begin_invocation_scope().unwrap();
    let bare = HostPorts::new(invocation.clone(), None, None)
        .install_for(abi::ABI_VERSION, None)
        .unwrap();
    let mut output = std::mem::MaybeUninit::uninit();
    assert_eq!(
        unsafe { retained_host_open(bare.handle, 1, output.as_mut_ptr()) },
        abi::UNSUPPORTED
    );
    HostPorts::retire(&bare).unwrap();
    // Binding is a host-only operation; no request bytes install this capability.
    *execution.retained_dispatch.lock().unwrap() = Some(Arc::new(Declined));
    let table = HostPorts::new(invocation.clone(), None, None)
        .with_retained_execution(Some(execution.clone()))
        .install_for(abi::ABI_VERSION, None)
        .unwrap();
    let InvocationTable::V3Retained(extension) = &table else {
        panic!("retained table required")
    };
    assert_eq!(
        table.capabilities & abi::CAP_RETAINED_HOST_LOOKUP_V2,
        abi::CAP_RETAINED_HOST_LOOKUP_V2
    );
    let mut v2 = std::mem::MaybeUninit::uninit();
    assert_eq!(
        unsafe { (extension.retained_host_open_v2)(table.handle, 1, v2.as_mut_ptr()) },
        abi::UNSUPPORTED
    );
    assert_eq!(
        unsafe { (extension.retained_host_open_v2)(table.handle, 2, v2.as_mut_ptr()) },
        abi::OK
    );
    let v2 = unsafe { v2.assume_init() };
    assert_default_lookup_unsupported(&v2);
    assert_eq!(v2.base.version, 2);
    assert_eq!(v2.base.size, 72);
    let extension = &extension.base;
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    assert_eq!(
        unsafe { (extension.retained_host_open)(table.handle, 2, output.as_mut_ptr()) },
        abi::UNSUPPORTED
    );
    assert_eq!(
        unsafe { (extension.retained_host_open)(table.handle, 1, std::ptr::null_mut()) },
        abi::INVALID_ARGUMENT
    );
    assert_eq!(
        unsafe { (extension.retained_host_open)(table.handle, 1, output.as_mut_ptr()) },
        abi::OK
    );
    let retained = unsafe { output.assume_init() };
    assert_eq!(unsafe { (v2.base.release)(v2.base.handle) }, abi::OK);
    invocation.close().unwrap();
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    HostPorts::retire(&table).unwrap();
    assert!(super::super::retained::has_owner(&execution).unwrap());
    assert_eq!(unsafe { (retained.release)(retained.handle) }, abi::OK);
    assert!(!super::super::retained::has_owner(&execution).unwrap());
}

#[test]
fn availability_retains_exact_selected_results_and_errors_without_replay() {
    for fail in [false, true] {
        let instance = InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let host = BoundHostServicePort::new(
            invocation.clone(),
            Arc::new(AvailabilityDispatch {
                calls: calls.clone(),
                invocation: invocation.id(),
                fail,
            }),
        );
        let port = HostPorts::new(invocation.clone(), None, Some(host));
        let legacy = HostPorts::new(invocation.clone(), None, None)
            .install_for(abi::ABI_VERSION_V2, None)
            .unwrap();
        assert_eq!(
            legacy.capabilities & abi::CAP_SELECTED_SERVICE_AVAILABILITY_V1,
            0
        );
        HostPorts::retire(&legacy).unwrap();
        let table = port.install_for(abi::ABI_VERSION, None).unwrap();
        let InvocationTable::V3Availability(extension) = &table else {
            panic!("availability table required")
        };
        assert_eq!(table.version, abi::PORTS_VERSION_V3);
        assert_eq!(
            usize::try_from(table.size).unwrap(),
            std::mem::size_of_val(extension)
        );
        assert_ne!(
            table.capabilities & abi::CAP_SELECTED_SERVICE_AVAILABILITY_V1,
            0
        );
        assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
        for interface in ["present/v1", "missing/v1"] {
            let input = availability_key(interface);
            let mut child = 99;
            assert_eq!(
                unsafe {
                    (extension.availability_begin)(
                        table.handle,
                        2,
                        input.as_ptr(),
                        input.len(),
                        &raw mut child,
                    )
                },
                abi::UNSUPPORTED
            );
            assert_eq!(child, 0);
            assert_eq!(
                unsafe {
                    (extension.availability_begin)(
                        table.handle,
                        SELECTED_SERVICE_AVAILABILITY_VERSION,
                        input.as_ptr(),
                        input.len(),
                        &raw mut child,
                    )
                },
                abi::OK
            );
            assert_ne!(child, 0);
            assert_eq!(unsafe { (table.release)(table.handle) }, abi::FAILED);
            assert_availability_outcome(&table, child, fail, interface);
            assert_eq!(unsafe { (table.host_ack)(table.handle, child) }, abi::OK);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        invocation.close().unwrap();
        let input = availability_key("present/v1");
        let mut child = 99;
        assert_eq!(
            unsafe {
                (extension.availability_begin)(
                    table.handle,
                    SELECTED_SERVICE_AVAILABILITY_VERSION,
                    input.as_ptr(),
                    input.len(),
                    &raw mut child,
                )
            },
            abi::CANCELLED
        );
        assert_eq!(child, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
        HostPorts::retire(&table).unwrap();
    }
}
use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};

#[test]
fn retained_lifecycle_port_outlives_setup_but_not_activation_and_holds_child_admission() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(392)));
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let setup = activation
        .begin_lifecycle_invocation(bmux_plugin_runtime::LifecycleInvocationKind::Activation)
        .unwrap();
    let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = effects.clone();
    let host = bmux_plugin_sdk::PluginHostServices::new(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        Ok(vec![7])
    });
    let table = HostPorts::for_activation(setup.clone(), activation.clone(), host)
        .install_for(abi::ABI_VERSION, None)
        .unwrap();
    assert_ne!(table.capabilities & abi::CAP_RETAINED_LIFECYCLE, 0);
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    setup.close().unwrap();
    setup.finish().unwrap();
    activation.mark_active().unwrap();
    let slice = |value: &'static [u8]| abi::NativeSlice {
        pointer: value.as_ptr(),
        length: value.len(),
    };
    let request = abi::NativeHostRequest {
        size: u32::try_from(std::mem::size_of::<abi::NativeHostRequest>()).unwrap(),
        kind: 0,
        capability: slice(b"test"),
        interface: slice(b"test/v1"),
        operation: slice(b"read"),
        payload: slice(b""),
    };
    let mut child = 0;
    assert_eq!(
        unsafe { (table.host_begin)(table.handle, &raw const request, &raw mut child) },
        abi::OK
    );
    activation.close().unwrap();
    let waker = std::task::Waker::noop();
    let mut drain = Box::pin(activation.drain());
    assert!(
        std::future::Future::poll(drain.as_mut(), &mut std::task::Context::from_waker(waker))
            .is_pending()
    );
    assert_eq!(unsafe { (table.host_ack)(table.handle, child) }, abi::OK);
    assert!(matches!(
        std::future::Future::poll(drain.as_mut(), &mut std::task::Context::from_waker(waker)),
        std::task::Poll::Ready(Ok(()))
    ));
    assert_ne!(
        unsafe { (table.host_begin)(table.handle, &raw const request, &raw mut child) },
        abi::OK
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    HostPorts::retire(&table).unwrap();
}

#[test]
fn admitted_kernel_failures_are_readable_and_acknowledgeable_without_replay() {
    struct Kernel {
        mode: u8,
        effects: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl crate::InvocationKernelDispatch for Kernel {
        fn execute(&self, _: &InvocationScope, _: &[u8]) -> Result<Vec<u8>> {
            self.effects.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                0 => Err(failure("fixture execution uncertain after effect")),
                1 => panic!("fixture kernel panicked after effect"),
                // A typed error returned after kernel entry must not become a
                // pre-admission cancellation or permit replay.
                _ => Err(bmux_plugin_sdk::PluginError::InvocationCancelled),
            }
        }
    }
    for mode in 0..3 {
        let instance =
            InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(34)));
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let kernel = crate::InvocationKernelPort::new(
            invocation.clone(),
            Arc::new(Kernel {
                mode,
                effects: effects.clone(),
            }),
        );
        let table = HostPorts::new(invocation.clone(), None, None)
            .install_for(abi::ABI_VERSION, Some(kernel))
            .unwrap();
        let InvocationTable::V3(extension) = &table else {
            panic!("v3 required")
        };
        assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
        let mut child = 0;
        assert_eq!(
            unsafe {
                (extension.kernel_begin)(table.handle, b"request".as_ptr(), 7, &raw mut child)
            },
            abi::OK,
            "admitted failure must expose a recovery identity"
        );
        assert_ne!(child, 0);
        invocation.close().unwrap();
        assert_eq!(unsafe { (table.release)(table.handle) }, abi::FAILED);
        assert!(HostPorts::retire(&table).is_err());
        for _ in 0..2 {
            let mut length = 99;
            assert_eq!(
                unsafe {
                    (table.host_read)(
                        table.handle,
                        child,
                        std::ptr::null_mut(),
                        0,
                        &raw mut length,
                    )
                },
                if mode == 1 {
                    abi::PANICKED
                } else {
                    abi::FAILED
                }
            );
            assert_eq!(length, 0, "errors must not be encoded as successful bytes");
        }
        let port = lookup(table.handle).unwrap();
        assert!(port.state.lock().unwrap().admissions.contains_key(&child));
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        assert_eq!(unsafe { (table.host_ack)(table.handle, child) }, abi::OK);
        let state = port.state.lock().unwrap();
        assert!(state.children.is_empty());
        assert!(state.admissions.is_empty());
        assert_eq!(state.child_bytes, 0);
        drop(state);
        assert_eq!(
            unsafe { (table.host_ack)(table.handle, child) },
            abi::FAILED
        );
        assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
        HostPorts::retire(&table).unwrap();
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn v3_kernel_outcome_is_retained_and_v2_rejects_grant() {
    struct Kernel(std::sync::atomic::AtomicUsize);
    impl crate::InvocationKernelDispatch for Kernel {
        fn execute(&self, _: &InvocationScope, _: &[u8]) -> Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![9; 8192])
        }
    }
    let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(21)));
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    let dispatch = Arc::new(Kernel(std::sync::atomic::AtomicUsize::new(0)));
    let kernel = crate::InvocationKernelPort::new(invocation.clone(), dispatch.clone());
    let port = HostPorts::new(invocation.clone(), None, None);
    assert!(
        port.install_for(abi::ABI_VERSION_V2, Some(kernel.clone()))
            .is_err()
    );
    let table = port.install_for(abi::ABI_VERSION, Some(kernel)).unwrap();
    let InvocationTable::V3(extension) = &table else {
        panic!("v3 table expected")
    };
    assert_eq!(table.version, abi::PORTS_VERSION_V3);
    assert_ne!(table.capabilities & abi::CAP_KERNEL_REQUESTS, 0);
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    let mut child = 0;
    assert_eq!(
        unsafe { (extension.kernel_begin)(table.handle, b"request".as_ptr(), 7, &raw mut child) },
        abi::OK
    );
    invocation.close().unwrap();
    let mut rejected_child = 99;
    assert_eq!(
        unsafe {
            (extension.kernel_begin)(
                table.handle,
                b"request".as_ptr(),
                7,
                &raw mut rejected_child,
            )
        },
        abi::CANCELLED
    );
    assert_eq!(rejected_child, 0);
    let mut length = 0;
    let mut output = vec![0; 1];
    assert_eq!(
        unsafe {
            (table.host_read)(
                table.handle,
                child,
                output.as_mut_ptr(),
                output.len(),
                &raw mut length,
            )
        },
        abi::BUFFER_TOO_SMALL
    );
    output.resize(length, 0);
    assert_eq!(
        unsafe {
            (table.host_read)(
                table.handle,
                child,
                output.as_mut_ptr(),
                output.len(),
                &raw mut length,
            )
        },
        abi::OK
    );
    assert_eq!(output, vec![9; 8192]);
    assert_eq!(dispatch.0.load(Ordering::SeqCst), 1);
    assert_eq!(unsafe { (table.host_ack)(table.handle, child) }, abi::OK);
    invocation.close().unwrap();
    assert_ne!(
        unsafe { (extension.kernel_begin)(table.handle, b"request".as_ptr(), 7, &raw mut child) },
        abi::OK
    );
    assert_eq!(dispatch.0.load(Ordering::SeqCst), 1);
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    HostPorts::retire(&table).unwrap();
}

#[test]
fn v3_without_grant_has_valid_unsupported_callback() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(22)));
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    // Exercise absent grants on a live ordinary invocation, not activation setup.
    activation.mark_active().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    let table = HostPorts::new(invocation, None, None)
        .install_for(abi::ABI_VERSION, None)
        .unwrap();
    let InvocationTable::V3(extension) = &table else {
        panic!("v3 table expected")
    };
    assert_eq!(table.capabilities & abi::CAP_KERNEL_REQUESTS, 0);
    let mut child = 0;
    assert_eq!(
        unsafe { (extension.kernel_begin)(table.handle, std::ptr::null(), 0, &raw mut child) },
        abi::UNSUPPORTED
    );
    HostPorts::retire(&table).unwrap();
}

#[test]
fn cancelled_v3_host_start_is_typed_and_dispatch_is_not_replayed() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(31)));
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    let table = HostPorts::new(invocation.clone(), None, None)
        .install_for(abi::ABI_VERSION, None)
        .unwrap();
    invocation.close().unwrap();
    let slice = NativeSlice {
        pointer: b"test".as_ptr(),
        length: 4,
    };
    let request = NativeHostRequest {
        size: u32::try_from(std::mem::size_of::<NativeHostRequest>()).unwrap(),
        kind: 0,
        capability: slice,
        interface: slice,
        operation: slice,
        payload: slice,
    };
    let mut child = 99;
    assert_eq!(
        unsafe { (table.host_begin)(table.handle, &raw const request, &raw mut child) },
        abi::CANCELLED
    );
    assert_eq!(child, 0);
    assert!(
        lookup(table.handle)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .children
            .is_empty()
    );
    HostPorts::retire(&table).unwrap();
}

#[test]
fn event_cancel_before_delivery_differs_from_failure_after_delivery() {
    struct Sink(InvocationScope, Arc<std::sync::atomic::AtomicUsize>);
    impl bmux_plugin_sdk::ServiceEventSink for Sink {
        fn emit_event(&self, _: bmux_plugin_sdk::ServiceEvent) -> Result<()> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.close().unwrap();
            Err(failure("fixture delivery failed"))
        }
    }
    for version in [abi::ABI_VERSION_V2, abi::ABI_VERSION] {
        for cancelled_first in [false, true] {
            let instance =
                InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(32)));
            let activation = instance
                .begin_activation(PluginConcurrencyConfig::Concurrent)
                .unwrap();
            activation.mark_active().unwrap();
            let invocation = activation.begin_invocation().unwrap();
            let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let table = HostPorts::new(
                invocation.clone(),
                Some(ServiceEventSinkHandle::new(Sink(
                    invocation.clone(),
                    delivered.clone(),
                ))),
                None,
            )
            .install_for(version, None)
            .unwrap();
            assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
            if cancelled_first {
                invocation.close().unwrap();
            }
            let event = serde_json::to_vec(&bmux_plugin_sdk::ServiceEvent {
                invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                payload: vec![1],
            })
            .unwrap();
            let status = unsafe { (table.emit_event)(table.handle, event.as_ptr(), event.len()) };
            assert_eq!(
                status,
                if cancelled_first && version == abi::ABI_VERSION {
                    abi::CANCELLED
                } else {
                    abi::FAILED
                }
            );
            assert_eq!(
                delivered.load(Ordering::SeqCst),
                usize::from(!cancelled_first)
            );
            assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
            HostPorts::retire(&table).unwrap();
        }
    }
}

#[test]
fn typed_dispatch_cancellation_clears_child_only_for_v3() {
    struct Dispatch(bool);
    impl bmux_plugin_sdk::HostServiceDispatch for Dispatch {
        fn invoke(&self, _: &InvocationScope, _: HostServiceRequest<'_>) -> Result<Vec<u8>> {
            if self.0 {
                Err(bmux_plugin_sdk::PluginError::InvocationCancelled)
            } else {
                Err(failure("invocation cancelled"))
            }
        }
    }
    for version in [abi::ABI_VERSION_V2, abi::ABI_VERSION] {
        for typed in [false, true] {
            let instance =
                InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(33)));
            let activation = instance
                .begin_activation(PluginConcurrencyConfig::Concurrent)
                .unwrap();
            activation.mark_active().unwrap();
            let invocation = activation.begin_invocation().unwrap();
            let host = BoundHostServicePort::new(invocation.clone(), Arc::new(Dispatch(typed)));
            let table = HostPorts::new(invocation, None, Some(host))
                .install_for(version, None)
                .unwrap();
            assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
            let request = host_test_request();
            let mut child = 0;
            let status =
                unsafe { (table.host_begin)(table.handle, &raw const request, &raw mut child) };
            if typed && version == abi::ABI_VERSION {
                assert_eq!(status, abi::CANCELLED);
                assert_eq!(child, 0);
            } else {
                assert_eq!(status, abi::FAILED);
            }
            assert!(
                lookup(table.handle)
                    .unwrap()
                    .state
                    .lock()
                    .unwrap()
                    .children
                    .is_empty()
            );
            assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
            HostPorts::retire(&table).unwrap();
        }
    }
}

fn host_test_request() -> NativeHostRequest {
    let text = b"example";
    let slice = NativeSlice {
        pointer: text.as_ptr(),
        length: text.len(),
    };
    NativeHostRequest {
        size: u32::try_from(std::mem::size_of::<NativeHostRequest>()).unwrap(),
        kind: 0,
        capability: slice,
        interface: slice,
        operation: slice,
        payload: NativeSlice {
            pointer: std::ptr::null(),
            length: 0,
        },
    }
}

#[tokio::test]
async fn opaque_port_retention_and_read_growth_do_not_repeat_host_effects() {
    struct Dispatch(std::sync::atomic::AtomicUsize);
    impl bmux_plugin_sdk::HostServiceDispatch for Dispatch {
        fn invoke(
            &self,
            _scope: &InvocationScope,
            _request: HostServiceRequest<'_>,
        ) -> Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![9; 8192])
        }
    }
    let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(11)));
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    let dispatch = Arc::new(Dispatch(std::sync::atomic::AtomicUsize::new(0)));
    let port = HostPorts::new(
        invocation.clone(),
        None,
        Some(BoundHostServicePort::new(
            invocation.clone(),
            dispatch.clone(),
        )),
    );
    let table = port.install().unwrap();
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    assert!(HostPorts::retire(&table).is_err());
    let request = host_test_request();
    let mut child = 0;
    assert_eq!(
        unsafe { (table.host_begin)(table.handle, &raw const request, &raw mut child) },
        abi::OK
    );
    // The host has produced authoritative bytes. Cancellation closes new effect
    // admission but must not prevent retained reads or acknowledgement.
    invocation.close().unwrap();
    let mut rejected_child = 0;
    assert_eq!(
        unsafe { (table.host_begin)(table.handle, &raw const request, &raw mut rejected_child) },
        abi::FAILED
    );
    assert_eq!(rejected_child, 0);
    let mut length = 0;
    let mut small = [0; 1];
    assert_eq!(
        unsafe {
            (table.host_read)(
                table.handle,
                child,
                small.as_mut_ptr(),
                small.len(),
                &raw mut length,
            )
        },
        abi::BUFFER_TOO_SMALL
    );
    assert_eq!(length, 8192);
    let mut output = vec![0; length];
    assert_eq!(
        unsafe {
            (table.host_read)(
                table.handle,
                child,
                output.as_mut_ptr(),
                output.len(),
                &raw mut length,
            )
        },
        abi::OK
    );
    assert_eq!(output, vec![9; 8192]);
    assert_eq!(dispatch.0.load(Ordering::SeqCst), 1);
    assert_eq!(unsafe { (table.host_ack)(table.handle, child) }, abi::OK);
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    HostPorts::retire(&table).unwrap();
    assert_ne!(unsafe { (table.retain)(table.handle) }, abi::OK);
    invocation.close().unwrap();
    invocation.drain().await.unwrap();
    invocation.finish().unwrap();
    activation.close().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    instance.close().unwrap();
    instance.drain().await.unwrap();
    instance.finish().unwrap();
}
