mod callback_authority;
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
#[derive(Default)]
struct FixtureCounts {
    retains: AtomicUsize,
    releases: AtomicUsize,
    begins: AtomicUsize,
    acks: AtomicUsize,
}
fn fixture(handle: u64) -> &'static FixtureCounts {
    unsafe { &*(handle as *const FixtureCounts) }
}
unsafe extern "C" fn retain(handle: u64) -> AbiStatus {
    fixture(handle).retains.fetch_add(1, Ordering::SeqCst);
    OK
}
unsafe extern "C" fn release(handle: u64) -> AbiStatus {
    fixture(handle).releases.fetch_add(1, Ordering::SeqCst);
    OK
}
unsafe extern "C" fn event(_: u64, _: *const u8, _: usize) -> AbiStatus {
    BUSY
}
unsafe extern "C" fn begin(
    handle: u64,
    request: *const NativeHostRequest,
    child: *mut u64,
) -> AbiStatus {
    assert_eq!(unsafe { (*request).kind }, 0);
    fixture(handle).begins.fetch_add(1, Ordering::SeqCst);
    unsafe {
        child.write(1);
    }
    OK
}
unsafe extern "C" fn read(
    _: u64,
    _: u64,
    output: *mut u8,
    capacity: usize,
    length: *mut usize,
) -> AbiStatus {
    let response = b"response";
    unsafe {
        length.write(response.len());
    }
    if capacity < response.len() {
        return BUFFER_TOO_SMALL;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(response.as_ptr(), output, response.len());
    }
    OK
}
unsafe extern "C" fn ack(handle: u64, _: u64) -> AbiStatus {
    fixture(handle).acks.fetch_add(1, Ordering::SeqCst);
    OK
}
unsafe extern "C" fn registration(
    _: u64,
    input: *const u8,
    length: usize,
    child: *mut u64,
) -> AbiStatus {
    let bytes = unsafe { std::slice::from_raw_parts(input, length) };
    if serde_json::from_slice::<crate::definition::RegistrationMutation>(bytes).is_err() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        child.write(2);
    }
    OK
}
fn table(counts: &FixtureCounts) -> NativeInvocationPorts {
    NativeInvocationPorts {
        size: u32::try_from(std::mem::size_of::<NativeInvocationPorts>()).unwrap(),
        version: PORTS_VERSION,
        capabilities: SCOPED_CAPABILITIES_V2,
        handle: std::ptr::from_ref(counts) as u64,
        retain,
        release,
        emit_event: event,
        host_begin: begin,
        host_read: read,
        host_ack: ack,
        registration_begin: registration,
    }
}
#[test]
fn retained_activation_settlement_keeps_clones_live_until_explicit_cleanup() {
    struct Counts {
        retains: AtomicUsize,
        releases: AtomicUsize,
    }
    unsafe extern "C" fn own(handle: u64) -> AbiStatus {
        let counts = unsafe { &*(handle as *const Counts) };
        counts.retains.fetch_add(1, Ordering::SeqCst);
        OK
    }
    unsafe extern "C" fn done(handle: u64) -> AbiStatus {
        let counts = unsafe { &*(handle as *const Counts) };
        counts.releases.fetch_add(1, Ordering::SeqCst);
        OK
    }
    unsafe extern "C" fn unsupported(_: u64, _: *const u8, _: usize, _: *mut u64) -> AbiStatus {
        UNSUPPORTED
    }
    let counts = Box::new(Counts {
        retains: AtomicUsize::new(0),
        releases: AtomicUsize::new(0),
    });
    let fixture_counts = FixtureCounts::default();
    let mut base = table(&fixture_counts);
    base.version = PORTS_VERSION_V3;
    base.size = u32::try_from(std::mem::size_of::<NativeInvocationPortsV3>()).unwrap();
    base.capabilities = CAP_HOST_SERVICES | CAP_RETAINED_LIFECYCLE;
    base.handle = (&raw const *counts) as u64;
    base.retain = own;
    base.release = done;
    let table = NativeInvocationPortsV3 {
        base,
        kernel_begin: unsupported,
    };
    let port = unsafe { InvocationPorts::acquire(&raw const table.base) }.unwrap();
    let clone = port.clone();
    port.settle_activation().unwrap();
    assert_eq!(counts.retains.load(Ordering::SeqCst), 1);
    assert_eq!(counts.releases.load(Ordering::SeqCst), 0);
    let admission = clone.admit(CAP_HOST_SERVICES).unwrap();
    assert_eq!(port.close_and_drain(), Err(BUSY));
    drop(admission);
    port.close_and_drain().unwrap();
    assert_eq!(counts.releases.load(Ordering::SeqCst), 1);
    assert!(clone.admit(CAP_HOST_SERVICES).is_err());
    drop(clone);
    drop(port);
    assert_eq!(counts.releases.load(Ordering::SeqCst), 1);
}

// One test owns these counters; it exercises all lifetime phases sequentially.
#[test]
fn scoped_ports_close_drain_retain_and_never_replay_mutation_on_growth() {
    let counts = FixtureCounts::default();
    let ports = unsafe { InvocationPorts::acquire(&table(&counts)) }.unwrap();
    assert_eq!(counts.retains.load(Ordering::SeqCst), 1);
    let host = ports.host_port();
    let result = host
        .invoke(HostServiceRequest {
            capability: "example",
            kind: crate::ServiceKind::Query,
            interface: "example/v1",
            operation: "query",
            payload: b"request",
        })
        .unwrap();
    assert_eq!(result, b"response");
    assert_eq!(counts.begins.load(Ordering::SeqCst), 1);
    assert_eq!(counts.acks.load(Ordering::SeqCst), 1);
    let registration_result = host
        .registration(crate::definition::RegistrationMutation::MarkReady {
            contract: crate::RegistrationContract {
                kind: crate::RegistrationKind::Readiness,
                interface: "example.ready/v1".into(),
                version: 1,
                name: "initial".into(),
            },
        })
        .unwrap();
    assert_eq!(registration_result, b"response");
    let admitted = ports.admit(CAP_HOST_SERVICES).unwrap();
    let operation = super::super::Operation {
        state: Mutex::new(super::super::OperationState::Polling),
        cancellation: crate::ServiceCancellation::new(),
        lifecycle: None,
        ports: Some(Arc::clone(&ports)),
    };
    let (adapter, _) = super::super::tests::adapter();
    let id = super::super::tests::create(&adapter);
    let instance = adapter.instance(id).unwrap();
    let terminal = super::super::drain_terminal(
        &instance,
        &operation,
        super::super::OperationState::Ready(vec![1]),
    );
    assert!(matches!(
        &terminal,
        super::super::OperationState::Draining(_)
    ));
    assert_eq!(ports.close_and_drain(), Err(BUSY));
    assert_eq!(counts.releases.load(Ordering::SeqCst), 0);
    assert!(ports.admit(CAP_HOST_SERVICES).is_err());
    drop(admitted);
    assert_eq!(ports.close_and_drain(), Ok(()));
    assert_eq!(counts.releases.load(Ordering::SeqCst), 1);
    assert!(
        host.invoke(HostServiceRequest {
            capability: "example",
            kind: crate::ServiceKind::Query,
            interface: "example/v1",
            operation: "query",
            payload: b"late"
        })
        .is_err()
    );
    assert_eq!(counts.begins.load(Ordering::SeqCst), 1);
    let stream_ports = unsafe { InvocationPorts::acquire(&table(&counts)) }.unwrap();
    let invocation_id = crate::PluginInvocationId::new();
    let sink = NativeEventSink {
        ports: Arc::clone(&stream_ports),
        invocation_id: invocation_id.clone(),
    };
    assert!(
        sink.emit_event(crate::ServiceEvent {
            invocation_id,
            payload: vec![1]
        })
        .is_err()
    );
    // Even an author ignoring delivery rejection cannot publish a healthy final.
    assert_eq!(stream_ports.close_and_drain(), Err(FAILED));
    // Consumer BUSY is a terminal delivery failure after callback drain.
    assert!(stream_ports.gate.lock().unwrap().released);
    drop(sink);
    drop(stream_ports);
    drop(host);
    drop(ports);
    assert_eq!(counts.releases.load(Ordering::SeqCst), 2);
}
#[cfg(test)]
mod kernel_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static BEGINS: AtomicUsize = AtomicUsize::new(0);
    static ACKS: AtomicUsize = AtomicUsize::new(0);
    static RELEASES: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn retain(_: u64) -> AbiStatus {
        OK
    }
    unsafe extern "C" fn release(_: u64) -> AbiStatus {
        RELEASES.fetch_add(1, Ordering::SeqCst);
        OK
    }
    unsafe extern "C" fn ack(_: u64, _: u64) -> AbiStatus {
        ACKS.fetch_add(1, Ordering::SeqCst);
        OK
    }
    unsafe extern "C" fn kernel_begin(
        _: u64,
        request: *const u8,
        length: usize,
        child: *mut u64,
    ) -> AbiStatus {
        let bytes = unsafe { std::slice::from_raw_parts(request, length) };
        if bytes != b"kernel-request" {
            return INVALID_ARGUMENT;
        }
        BEGINS.fetch_add(1, Ordering::SeqCst);
        unsafe {
            child.write(3);
        }
        OK
    }
    fn table(counts: &FixtureCounts) -> NativeInvocationPortsV3 {
        let mut base = super::table(counts);
        base.size = u32::try_from(std::mem::size_of::<NativeInvocationPortsV3>()).unwrap();
        base.version = PORTS_VERSION_V3;
        base.capabilities = CAP_KERNEL_REQUESTS;
        base.retain = retain;
        base.release = release;
        base.host_ack = ack;
        NativeInvocationPortsV3 { base, kernel_begin }
    }
    #[test]
    fn kernel_negotiation_bounds_cancellation_and_closed_clone_are_fail_closed() {
        let counts = FixtureCounts::default();
        let mut table = table(&counts);
        // A v2 table cannot grant the v3 capability, even with excess size.
        table.base.version = PORTS_VERSION;
        assert_eq!(
            unsafe { InvocationPorts::acquire(&raw const table.base) }.err(),
            Some(UNSUPPORTED)
        );
        table.base.version = PORTS_VERSION_V3;
        table.base.size = u32::try_from(std::mem::size_of::<NativeInvocationPorts>()).unwrap();
        assert_eq!(
            unsafe { InvocationPorts::acquire(&raw const table.base) }.err(),
            Some(UNSUPPORTED)
        );
        table.base.size = u32::try_from(std::mem::size_of::<NativeInvocationPortsV3>()).unwrap();
        let ports = unsafe { InvocationPorts::acquire(&raw const table.base) }.unwrap();
        assert!(
            ports
                .host_port()
                .execute_kernel_encoded(b"kernel-request")
                .is_err()
        );
        assert_eq!(BEGINS.load(Ordering::SeqCst), 0);
        let cancellation = crate::ServiceCancellation::new();
        ports.bind_cancellation(cancellation.clone()).unwrap();
        let host = ports.host_port();
        let retained_clone = host.clone();
        assert!(
            host.execute_kernel_encoded(&vec![0; MAX_BUFFER_BYTES + 1])
                .is_err()
        );
        assert_eq!(BEGINS.load(Ordering::SeqCst), 0);
        assert_eq!(
            host.execute_kernel_encoded(b"kernel-request").unwrap(),
            b"response"
        );
        // retrieve probes/grows the output, without repeating kernel_begin.
        assert_eq!(BEGINS.load(Ordering::SeqCst), 1);
        assert_eq!(ACKS.load(Ordering::SeqCst), 1);
        cancellation.cancel();
        assert!(host.execute_kernel_encoded(b"kernel-request").is_err());
        assert_eq!(BEGINS.load(Ordering::SeqCst), 1);
        let admitted = ports.admit(CAP_KERNEL_REQUESTS).unwrap();
        assert_eq!(ports.close_and_drain(), Err(BUSY));
        assert_eq!(RELEASES.load(Ordering::SeqCst), 0);
        drop(admitted);
        assert_eq!(ports.close_and_drain(), Ok(()));
        assert_eq!(RELEASES.load(Ordering::SeqCst), 1);
        assert!(
            retained_clone
                .execute_kernel_encoded(b"kernel-request")
                .is_err()
        );
        assert_eq!(BEGINS.load(Ordering::SeqCst), 1);
        drop(host);
        drop(retained_clone);
        drop(ports);
        assert_eq!(RELEASES.load(Ordering::SeqCst), 1);
        // v3 framing alone does not confer a grant or bind cancellation.
        table.base.capabilities = 0;
        let ports = unsafe { InvocationPorts::acquire(&raw const table.base) }.unwrap();
        assert!(matches!(
            ports.host_port().execute_kernel_encoded(b"kernel-request"),
            Err(crate::PluginError::UnsupportedHostOperation { .. })
        ));
        ports.close_and_drain().unwrap();
    }
}

#[cfg(test)]
mod child_cancellation_tests {
    use super::*;
    unsafe extern "C" fn ownership(_: u64) -> AbiStatus {
        OK
    }
    unsafe extern "C" fn acknowledge(_: u64, _: u64) -> AbiStatus {
        OK
    }
    unsafe extern "C" fn cancelled(_: u64, _: *const NativeHostRequest, _: *mut u64) -> AbiStatus {
        super::super::super::CANCELLED
    }
    unsafe extern "C" fn failed(_: u64, _: *const NativeHostRequest, _: *mut u64) -> AbiStatus {
        FAILED
    }
    unsafe extern "C" fn cancelled_read(
        _: u64,
        _: u64,
        _: *mut u8,
        _: usize,
        _: *mut usize,
    ) -> AbiStatus {
        super::super::super::CANCELLED
    }
    unsafe extern "C" fn failed_ack(_: u64, _: u64) -> AbiStatus {
        FAILED
    }
    unsafe extern "C" fn unused_kernel(_: u64, _: *const u8, _: usize, _: *mut u64) -> AbiStatus {
        UNSUPPORTED
    }
    fn request() -> HostServiceRequest<'static> {
        HostServiceRequest {
            capability: "example",
            kind: crate::ServiceKind::Query,
            interface: "example/v1",
            operation: "query",
            payload: b"request",
        }
    }
    #[test]
    fn explicit_v3_child_cancel_is_an_error_without_poisoning_settled_parent() {
        for from_read in [false, true] {
            let counts = FixtureCounts::default();
            let mut base = super::table(&counts);
            base.version = PORTS_VERSION_V3;
            base.size = u32::try_from(std::mem::size_of::<NativeInvocationPortsV3>()).unwrap();
            base.retain = ownership;
            base.release = ownership;
            base.host_ack = acknowledge;
            base.host_begin = cancelled;
            if from_read {
                base.host_read = cancelled_read;
            }
            let table = NativeInvocationPortsV3 {
                base,
                kernel_begin: unused_kernel,
            };
            let ports = unsafe { InvocationPorts::acquire(&raw const table.base) }.unwrap();
            let result = if from_read {
                ports.retrieve(1)
            } else {
                ports.host_port().invoke(request())
            };
            assert!(result.is_err());
            assert_eq!(ports.close_and_drain(), Ok(()));
        }
    }
    #[test]
    fn real_failure_and_v2_cancel_remain_sticky_even_when_parent_cancelled() {
        for v2 in [false, true] {
            let counts = FixtureCounts::default();
            let mut base = super::table(&counts);
            base.retain = ownership;
            base.release = ownership;
            base.host_begin = if v2 { cancelled } else { failed };
            if !v2 {
                base.version = PORTS_VERSION_V3;
                base.size = u32::try_from(std::mem::size_of::<NativeInvocationPortsV3>()).unwrap();
            }
            let table = NativeInvocationPortsV3 {
                base,
                kernel_begin: unused_kernel,
            };
            let ports = unsafe { InvocationPorts::acquire(&raw const table.base) }.unwrap();
            let token = crate::ServiceCancellation::new();
            ports.bind_cancellation(token.clone()).unwrap();
            token.cancel();
            assert!(ports.host_port().invoke(request()).is_err());
            assert!(ports.close_and_drain().is_err());
        }
    }
    #[test]
    fn cancelled_child_with_failed_ack_is_still_cleanup_failure() {
        let counts = FixtureCounts::default();
        let mut base = super::table(&counts);
        base.retain = ownership;
        base.release = ownership;
        base.version = PORTS_VERSION_V3;
        base.size = u32::try_from(std::mem::size_of::<NativeInvocationPortsV3>()).unwrap();
        base.host_read = cancelled_read;
        base.host_ack = failed_ack;
        let table = NativeInvocationPortsV3 {
            base,
            kernel_begin: unused_kernel,
        };
        let ports = unsafe { InvocationPorts::acquire(&raw const table.base) }.unwrap();
        assert!(ports.retrieve(1).is_err());
        assert_eq!(ports.close_and_drain(), Err(FAILED));
    }
}

#[test]
fn unsupported_callback_version_rejected_before_retain() {
    let counts = FixtureCounts::default();
    let mut table = table(&counts);
    table.version = u32::MAX;
    assert_eq!(
        unsafe { InvocationPorts::acquire(&raw const table) }.err(),
        Some(UNSUPPORTED)
    );
}

mod availability_v1 {
    use super::*;
    use crate::typed_dispatch::{
        SELECTED_SERVICE_AVAILABILITY_VERSION, SelectedServiceAvailability, ServiceAvailabilityKey,
    };

    struct Lookup {
        fixture: FixtureCounts,
        presence: SelectedServiceAvailability,
        unsupported: bool,
        begins: AtomicUsize,
        reads: AtomicUsize,
        acks: AtomicUsize,
        retains: AtomicUsize,
        releases: AtomicUsize,
    }
    impl Lookup {
        fn new(presence: SelectedServiceAvailability) -> Self {
            Self {
                fixture: FixtureCounts::default(),
                presence,
                unsupported: false,
                begins: AtomicUsize::new(0),
                reads: AtomicUsize::new(0),
                acks: AtomicUsize::new(0),
                retains: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
            }
        }
    }
    unsafe extern "C" fn own(handle: u64) -> AbiStatus {
        let lookup = unsafe { &*(handle as *const Lookup) };
        lookup.retains.fetch_add(1, Ordering::SeqCst);
        OK
    }
    unsafe extern "C" fn done(handle: u64) -> AbiStatus {
        let lookup = unsafe { &*(handle as *const Lookup) };
        lookup.releases.fetch_add(1, Ordering::SeqCst);
        OK
    }
    unsafe extern "C" fn kernel(_: u64, _: *const u8, _: usize, _: *mut u64) -> AbiStatus {
        UNSUPPORTED
    }
    unsafe extern "C" fn lookup_begin(
        handle: u64,
        version: u16,
        bytes: *const u8,
        length: usize,
        child: *mut u64,
    ) -> AbiStatus {
        if child.is_null() {
            return INVALID_ARGUMENT;
        }
        unsafe {
            child.write(0);
        }
        if version != SELECTED_SERVICE_AVAILABILITY_VERSION {
            return UNSUPPORTED;
        }
        if bytes.is_null() || length > MAX_BUFFER_BYTES {
            return INVALID_ARGUMENT;
        }
        let input = unsafe { std::slice::from_raw_parts(bytes, length) };
        let Ok(key) = bmux_codec::from_typed_bytes::<ServiceAvailabilityKey>(input) else {
            return INVALID_ARGUMENT;
        };
        if key.validate().is_err() {
            return INVALID_ARGUMENT;
        }
        let lookup = unsafe { &*(handle as *const Lookup) };
        lookup.begins.fetch_add(1, Ordering::SeqCst);
        if lookup.unsupported {
            return UNSUPPORTED;
        }
        unsafe {
            child.write(1);
        }
        OK
    }
    unsafe extern "C" fn lookup_read(
        handle: u64,
        child: u64,
        output: *mut u8,
        capacity: usize,
        length: *mut usize,
    ) -> AbiStatus {
        let lookup = unsafe { &*(handle as *const Lookup) };
        if child != 1 {
            return INVALID_ARGUMENT;
        }
        lookup.reads.fetch_add(1, Ordering::SeqCst);
        let bytes = crate::encode_service_message(&lookup.presence).unwrap();
        unsafe { super::super::super::copy_output(&bytes, output, capacity, length) }
    }
    unsafe extern "C" fn lookup_ack(handle: u64, child: u64) -> AbiStatus {
        if child != 1 {
            return INVALID_ARGUMENT;
        }
        let lookup = unsafe { &*(handle as *const Lookup) };
        lookup.acks.fetch_add(1, Ordering::SeqCst);
        OK
    }
    fn key() -> ServiceAvailabilityKey {
        ServiceAvailabilityKey {
            capability: "example.read".into(),
            kind: crate::ServiceKind::Query,
            interface_id: "example-state/v1".into(),
        }
    }
    fn lookup_table(lookup: &Lookup) -> NativeInvocationPortsV3Availability {
        let mut base = table(&lookup.fixture);
        base.version = PORTS_VERSION_V3;
        base.size =
            u32::try_from(std::mem::size_of::<NativeInvocationPortsV3Availability>()).unwrap();
        base.capabilities = CAP_SELECTED_SERVICE_AVAILABILITY_V1;
        base.handle = std::ptr::from_ref(lookup) as u64;
        base.retain = own;
        base.release = done;
        base.host_read = lookup_read;
        base.host_ack = lookup_ack;
        NativeInvocationPortsV3Availability {
            base: NativeInvocationPortsV3 {
                base,
                kernel_begin: kernel,
            },
            availability_begin: lookup_begin,
        }
    }

    #[tokio::test]
    async fn negotiated_lookup_retains_reads_and_acks_without_reexecution() {
        for presence in [
            SelectedServiceAvailability::Present,
            SelectedServiceAvailability::Absent,
        ] {
            let lookup = Box::new(Lookup::new(presence));
            let table = lookup_table(&lookup);
            let ports = unsafe { InvocationPorts::acquire(&raw const table.base.base) }.unwrap();
            ports
                .bind_cancellation(crate::ServiceCancellation::new())
                .unwrap();
            let host = ports.host_port();
            assert_eq!(
                host.selected_service_availability(key()).await.unwrap(),
                presence
            );
            assert_eq!(lookup.begins.load(Ordering::SeqCst), 1);
            assert_eq!(lookup.reads.load(Ordering::SeqCst), 2);
            assert_eq!(lookup.acks.load(Ordering::SeqCst), 1);
            assert_eq!(lookup.retains.load(Ordering::SeqCst), 1);
            ports.close_and_drain().unwrap();
            assert_eq!(lookup.releases.load(Ordering::SeqCst), 1);
            assert!(host.selected_service_availability(key()).await.is_err());
            assert_eq!(lookup.begins.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn settled_activation_lookup_uses_retained_host_scope_and_drains() {
        let lookup = Box::new(Lookup::new(SelectedServiceAvailability::Present));
        let mut table = lookup_table(&lookup);
        table.base.base.capabilities |= CAP_RETAINED_LIFECYCLE;
        let ports = unsafe { InvocationPorts::acquire(&raw const table.base.base) }.unwrap();
        ports
            .bind_cancellation(crate::ServiceCancellation::new())
            .unwrap();
        ports.settle_activation().unwrap();
        assert_eq!(
            ports
                .host_port()
                .selected_service_availability(key())
                .await
                .unwrap(),
            SelectedServiceAvailability::Present
        );
        let admitted = ports.admit(CAP_SELECTED_SERVICE_AVAILABILITY_V1).unwrap();
        assert_eq!(ports.close_and_drain(), Err(BUSY));
        assert_eq!(lookup.releases.load(Ordering::SeqCst), 0);
        drop(admitted);
        ports.close_and_drain().unwrap();
        assert_eq!(lookup.releases.load(Ordering::SeqCst), 1);
        assert_eq!(lookup.begins.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn legacy_and_default_ports_report_unsupported_not_absent() {
        let lookup = Box::new(Lookup::new(SelectedServiceAvailability::Absent));
        for version in [PORTS_VERSION, PORTS_VERSION_V3] {
            let mut table = lookup_table(&lookup);
            table.base.base.version = version;
            table.base.base.capabilities = 0;
            table.base.base.size = u32::try_from(if version == PORTS_VERSION {
                std::mem::size_of::<NativeInvocationPorts>()
            } else {
                std::mem::size_of::<NativeInvocationPortsV3>()
            })
            .unwrap();
            let ports = unsafe { InvocationPorts::acquire(&raw const table.base.base) }.unwrap();
            assert!(matches!(
                ports.host_port().selected_service_availability(key()).await,
                Err(PluginError::UnsupportedHostOperation { .. })
            ));
            ports.close_and_drain().unwrap();
        }
        assert!(matches!(
            crate::definition::PluginHostServices::new(|_| Ok(vec![]))
                .selected_service_availability(key())
                .await,
            Err(PluginError::UnsupportedHostOperation { .. })
        ));
        assert_eq!(lookup.begins.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn short_tables_unknown_bits_and_legacy_grants_fail_closed_before_retain() {
        let lookup = Box::new(Lookup::new(SelectedServiceAvailability::Absent));
        let mut short = lookup_table(&lookup).base;
        short.base.size = u32::try_from(std::mem::size_of::<NativeInvocationPortsV3>()).unwrap();
        assert!(matches!(
            unsafe { InvocationPorts::acquire(&raw const short.base) },
            Err(UNSUPPORTED)
        ));
        let mut legacy = short.base;
        legacy.version = PORTS_VERSION;
        legacy.size = u32::try_from(std::mem::size_of::<NativeInvocationPorts>()).unwrap();
        assert!(matches!(
            unsafe { InvocationPorts::acquire(&raw const legacy) },
            Err(UNSUPPORTED)
        ));
        short.base.version = 4;
        assert!(matches!(
            unsafe { InvocationPorts::acquire(&raw const short.base) },
            Err(UNSUPPORTED)
        ));
        let mut table = lookup_table(&lookup);
        table.base.base.capabilities |= 1_u64 << 63;
        assert!(matches!(
            unsafe { InvocationPorts::acquire(&raw const table.base.base) },
            Err(UNSUPPORTED)
        ));
        assert_eq!(lookup.retains.load(Ordering::SeqCst), 0);
        assert_eq!(SCOPED_CAPABILITIES_V2, 15);
        assert_eq!(SCOPED_CAPABILITIES_V3_LEGACY, 127);
        assert_ne!(
            CAP_SELECTED_SERVICE_AVAILABILITY_V1 & !SCOPED_CAPABILITIES_V3_LEGACY,
            0
        );
    }

    #[tokio::test]
    async fn unsupported_version_and_host_failure_never_become_absent() {
        let mut lookup = Box::new(Lookup::new(SelectedServiceAvailability::Absent));
        lookup.unsupported = true;
        let table = lookup_table(&lookup);
        let input = crate::encode_service_message(&key()).unwrap();
        let mut child = 9;
        assert_eq!(
            unsafe {
                (table.availability_begin)(
                    table.base.base.handle,
                    2,
                    input.as_ptr(),
                    input.len(),
                    &raw mut child,
                )
            },
            UNSUPPORTED
        );
        assert_eq!(child, 0);
        assert_eq!(lookup.begins.load(Ordering::SeqCst), 0);
        let ports = unsafe { InvocationPorts::acquire(&raw const table.base.base) }.unwrap();
        ports
            .bind_cancellation(crate::ServiceCancellation::new())
            .unwrap();
        assert!(
            ports
                .host_port()
                .selected_service_availability(key())
                .await
                .is_err()
        );
        assert_eq!(lookup.acks.load(Ordering::SeqCst), 0);
    }

    unsafe extern "C" fn no_retained_owner(
        _: u64,
        version: u16,
        _: *mut super::super::super::retained::NativeRetainedHostPortV1,
    ) -> AbiStatus {
        assert_eq!(version, 1);
        UNSUPPORTED
    }

    #[test]
    fn retained_open_checks_full_tail_and_never_falls_back() {
        let lookup = Box::new(Lookup::new(SelectedServiceAvailability::Present));
        let mut table = super::super::NativeInvocationPortsV3RetainedHost {
            base: lookup_table(&lookup),
            retained_host_open: no_retained_owner,
        };
        table.base.base.base.capabilities = CAP_RETAINED_ASYNC_HOST_V1;
        table.base.base.base.size = u32::try_from(std::mem::size_of_val(&table)).unwrap();
        let ports = unsafe { InvocationPorts::acquire(&raw const table.base.base.base) }.unwrap();
        assert!(ports.open_retained_host().is_err());
        ports.close_and_drain().unwrap();
        table.base.base.base.size =
            u32::try_from(std::mem::size_of::<NativeInvocationPortsV3Availability>()).unwrap();
        assert!(matches!(
            unsafe { InvocationPorts::acquire(&raw const table.base.base.base) },
            Err(UNSUPPORTED)
        ));
        table.base.base.base.capabilities = 0;
        let ports = unsafe { InvocationPorts::acquire(&raw const table.base.base.base) }.unwrap();
        assert!(matches!(
            ports.open_retained_host(),
            Err(PluginError::UnsupportedHostOperation { .. })
        ));
        ports.close_and_drain().unwrap();
        assert_eq!(lookup.begins.load(Ordering::SeqCst), 0);
    }

    unsafe extern "C" fn fail_recovery_open(
        handle: u64,
        version: u16,
        _: *mut super::super::super::retained::NativeRetainedHostPortV2,
    ) -> AbiStatus {
        assert_eq!(version, 2);
        let lookup = unsafe { &*(handle as *const Lookup) };
        lookup.begins.fetch_add(1, Ordering::SeqCst);
        FAILED
    }
    unsafe extern "C" fn forbidden_legacy_open(
        handle: u64,
        _: u16,
        _: *mut super::super::super::retained::NativeRetainedHostPortV1,
    ) -> AbiStatus {
        // A failed selected v2 open must never invoke this fallback.
        let lookup = unsafe { &*(handle as *const Lookup) };
        lookup.begins.fetch_add(100, Ordering::SeqCst);
        FAILED
    }
    #[test]
    fn recovery_open_requires_exact_negotiation_and_does_not_downgrade() {
        let lookup = Box::new(Lookup::new(SelectedServiceAvailability::Present));
        let mut table = super::super::NativeInvocationPortsV3RetainedHostV2 {
            base: super::super::NativeInvocationPortsV3RetainedHost {
                base: lookup_table(&lookup),
                retained_host_open: forbidden_legacy_open,
            },
            retained_host_open_v2: fail_recovery_open,
        };
        table.base.base.base.base.size = u32::try_from(std::mem::size_of_val(&table)).unwrap();
        table.base.base.base.base.capabilities = CAP_RETAINED_HOST_LOOKUP_V2;
        let ports =
            unsafe { InvocationPorts::acquire(&raw const table.base.base.base.base) }.unwrap();
        assert!(ports.open_retained_host_v2().is_err());
        ports.close_and_drain().unwrap();
        assert_eq!(lookup.begins.load(Ordering::SeqCst), 1);
        table.base.base.base.base.size -= 1;
        assert!(matches!(
            unsafe { InvocationPorts::acquire(&raw const table.base.base.base.base) },
            Err(UNSUPPORTED)
        ));
        table.base.base.base.base.size += 1;
        table.base.base.base.base.capabilities |= 1_u64 << 63;
        assert!(matches!(
            unsafe { InvocationPorts::acquire(&raw const table.base.base.base.base) },
            Err(UNSUPPORTED)
        ));
        table.base.base.base.base.capabilities = CAP_RETAINED_HOST_LOOKUP_V2;
        table.base.base.base.base.version = 4;
        assert!(matches!(
            unsafe { InvocationPorts::acquire(&raw const table.base.base.base.base) },
            Err(UNSUPPORTED)
        ));
        table.base.base.base.base.version = PORTS_VERSION_V3;
        table.base.base.base.base.capabilities = CAP_RETAINED_ASYNC_HOST_V1;
        let ports =
            unsafe { InvocationPorts::acquire(&raw const table.base.base.base.base) }.unwrap();
        assert!(matches!(
            ports.open_retained_host_v2(),
            Err(PluginError::UnsupportedHostOperation { .. })
        ));
        ports.close_and_drain().unwrap();
        assert_eq!(lookup.begins.load(Ordering::SeqCst), 1);
        assert_eq!(SCOPED_CAPABILITIES_V3_RETAINED_V1, 511);
    }

    #[tokio::test]
    async fn invalid_and_cancelled_queries_do_not_reach_host() {
        let lookup = Box::new(Lookup::new(SelectedServiceAvailability::Present));
        let table = lookup_table(&lookup);
        let ports = unsafe { InvocationPorts::acquire(&raw const table.base.base) }.unwrap();
        let host = ports.host_port();
        let token = crate::ServiceCancellation::new();
        ports.bind_cancellation(token.clone()).unwrap();
        let mut invalid = key();
        invalid.interface_id.clear();
        assert!(host.selected_service_availability(invalid).await.is_err());
        token.cancel();
        assert!(matches!(
            host.selected_service_availability(key()).await,
            Err(PluginError::InvocationCancelled)
        ));
        assert_eq!(lookup.begins.load(Ordering::SeqCst), 0);
        ports.close_and_drain().unwrap();
    }
}
