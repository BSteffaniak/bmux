use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// One bounded host-owned record, not an SDK ledger implementation.
struct Host {
    effects: AtomicUsize,
    request: Mutex<Option<u64>>,
    closed: AtomicBool,
    joined: AtomicBool,
    acked: AtomicBool,
    releases: AtomicUsize,
}
impl Host {
    fn new() -> Self {
        Self {
            effects: AtomicUsize::new(0),
            request: Mutex::new(None),
            closed: AtomicBool::new(false),
            joined: AtomicBool::new(false),
            acked: AtomicBool::new(false),
            releases: AtomicUsize::new(0),
        }
    }
}
unsafe fn host<'a>(handle: u64) -> &'a Host {
    unsafe { &*(handle as *const Host) }
}
unsafe extern "C" fn retain(_: u64) -> AbiStatus {
    OK
}
unsafe extern "C" fn release(handle: u64) -> AbiStatus {
    let host = unsafe { host(handle) };
    if !host.closed.load(Ordering::SeqCst)
        || !host.joined.load(Ordering::SeqCst)
        || !host.acked.load(Ordering::SeqCst)
    {
        return BUSY;
    }
    host.releases.fetch_add(1, Ordering::SeqCst);
    OK
}
unsafe extern "C" fn begin(
    handle: u64,
    id: u64,
    request: *const NativeRetainedRequestV1,
    reservation: *const NativeRetainedReservationV1,
    output: *mut u64,
) -> AbiStatus {
    unsafe {
        output.write(0);
    }
    let request = unsafe { &*request };
    if request.version != RETAINED_HOST_VERSION
        || request.size as usize != std::mem::size_of::<NativeRetainedRequestV1>()
    {
        return UNSUPPORTED;
    }
    if let Err(status) = unsafe { &*reservation }.validate() {
        return status;
    }
    let host = unsafe { host(handle) };
    let mut selected = host.request.lock().unwrap();
    if let Some(old) = *selected {
        if old != id {
            return INVALID_ARGUMENT;
        }
    } else {
        if host.closed.load(Ordering::SeqCst) {
            return INVALID_ARGUMENT;
        }
        *selected = Some(id);
        host.effects.fetch_add(1, Ordering::SeqCst);
    }
    drop(selected);
    unsafe {
        output.write(7);
    }
    OK
}
unsafe extern "C" fn read(
    handle: u64,
    operation: u64,
    output: *mut u8,
    capacity: u32,
    length: *mut u32,
    receipt: *mut u64,
) -> AbiStatus {
    if operation != 7 {
        return INVALID_ARGUMENT;
    }
    let host = unsafe { host(handle) };
    if host.acked.load(Ordering::SeqCst) {
        return INVALID_ARGUMENT;
    }
    if !host.joined.load(Ordering::SeqCst) {
        return PENDING;
    }
    let bytes = RetainedHostOutcome::Completed(vec![4, 5]).encode().unwrap();
    unsafe {
        length.write(u32::try_from(bytes.len()).unwrap());
        receipt.write(11);
    }
    if (capacity as usize) < bytes.len() {
        return BUFFER_TOO_SMALL;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
    }
    OK
}
unsafe extern "C" fn control(handle: u64, operation: u64, control: u32) -> AbiStatus {
    if !matches!(operation, 0 | 7) {
        return INVALID_ARGUMENT;
    }
    let host = unsafe { host(handle) };
    match control {
        RETAINED_HOST_CONTROL_CANCEL => OK,
        RETAINED_HOST_CONTROL_CLOSE => {
            host.closed.store(true, Ordering::SeqCst);
            OK
        }
        RETAINED_HOST_CONTROL_JOIN => {
            if host.joined.load(Ordering::SeqCst) {
                OK
            } else {
                PENDING
            }
        }
        _ => UNSUPPORTED,
    }
}
unsafe extern "C" fn ack(handle: u64, operation: u64, receipt: u64) -> AbiStatus {
    let host = unsafe { host(handle) };
    if operation != 7 || receipt != 11 || !host.joined.load(Ordering::SeqCst) {
        return INVALID_ARGUMENT;
    }
    host.acked.store(true, Ordering::SeqCst);
    OK
}
fn table(host: &Host) -> NativeRetainedHostPortV1 {
    NativeRetainedHostPortV1 {
        size: u32::try_from(std::mem::size_of::<NativeRetainedHostPortV1>()).unwrap(),
        version: 1,
        handle: std::ptr::from_ref(host) as u64,
        retain,
        release,
        begin,
        read,
        control,
        ack,
    }
}
fn reservation() -> NativeRetainedReservationV1 {
    NativeRetainedReservationV1 {
        size: 24,
        version: 1,
        max_outcome_bytes: 64,
        operation_slots: 1,
        reserved: 0,
    }
}
fn request() -> HostServiceRequest<'static> {
    HostServiceRequest {
        capability: "example.read",
        kind: crate::ServiceKind::Query,
        interface: "example/v1",
        operation: "read",
        payload: &[],
    }
}
#[test]
fn retained_observer_loss_does_not_cancel_ack_release_or_replay() {
    let host = Box::new(Host::new());
    let port = unsafe { RetainedHostPort::from_native_owned(table(&host)) }.unwrap();
    let operation = port.begin(9, &request(), reservation()).unwrap();
    assert_eq!(
        operation.observe().unwrap(),
        RetainedHostObservation::Pending
    );
    assert!(!port.release().unwrap());
    assert!(operation.control(RETAINED_HOST_CONTROL_CANCEL).unwrap());
    assert!(!operation.control(RETAINED_HOST_CONTROL_JOIN).unwrap());
    drop(operation);
    let recovered = port.begin(9, &request(), reservation()).unwrap();
    assert_eq!(host.effects.load(Ordering::SeqCst), 1);
    assert!(port.begin(10, &request(), reservation()).is_err());
    assert!(port.control(RETAINED_HOST_CONTROL_CLOSE).unwrap());
    host.joined.store(true, Ordering::SeqCst);
    assert!(port.control(RETAINED_HOST_CONTROL_JOIN).unwrap());
    let terminal = RetainedHostObservation::Terminal {
        receipt: 11,
        outcome: RetainedHostOutcome::Completed(vec![4, 5]),
    };
    assert_eq!(recovered.observe().unwrap(), terminal);
    assert_eq!(recovered.observe().unwrap(), terminal);
    assert!(!port.release().unwrap());
    assert!(recovered.acknowledge(12).is_err());
    recovered.acknowledge(11).unwrap();
    assert!(port.release().unwrap());
    assert!(port.release().unwrap());
    assert!(recovered.observe().is_err());
    drop(recovered);
    drop(port);
    assert_eq!(host.releases.load(Ordering::SeqCst), 1);
}
#[test]
fn dropping_unsettled_port_leaves_host_record_and_cleanup_pin() {
    let host = Box::new(Host::new());
    let port = unsafe { RetainedHostPort::from_native_owned(table(&host)) }.unwrap();
    let operation = port.begin(9, &request(), reservation()).unwrap();
    drop(port);
    drop(operation);
    assert_eq!(host.releases.load(Ordering::SeqCst), 0);
    assert_eq!(host.effects.load(Ordering::SeqCst), 1);
    assert!(!host.acked.load(Ordering::SeqCst));
}
#[test]
fn representations_and_capacity_reject_before_effects() {
    let host = Box::new(Host::new());
    let mut wrong = table(&host);
    wrong.version = 2;
    assert!(unsafe { RetainedHostPort::from_native_owned(wrong) }.is_err());
    wrong = table(&host);
    wrong.size -= 1;
    assert!(unsafe { RetainedHostPort::from_native_owned(wrong) }.is_err());
    let port = unsafe { RetainedHostPort::from_native_owned(table(&host)) }.unwrap();
    let mut budget = reservation();
    budget.version = 2;
    assert!(port.begin(1, &request(), budget).is_err());
    budget = reservation();
    budget.max_outcome_bytes = 11;
    assert!(port.begin(1, &request(), budget).is_err());
    budget = reservation();
    budget.operation_slots = 0;
    assert!(port.begin(1, &request(), budget).is_err());
    assert!(port.begin(0, &request(), reservation()).is_err());
    assert!(port.control(99).is_err());
    assert_eq!(host.effects.load(Ordering::SeqCst), 0);
}
#[test]
fn closed_records_have_exact_stable_encoding_and_no_failure_text() {
    for outcome in [
        RetainedHostOutcome::Completed(vec![1]),
        RetainedHostOutcome::RejectedBeforeEffects,
        RetainedHostOutcome::EffectsUnresolved { observed_bytes: 19 },
        RetainedHostOutcome::ResponseLimitExceeded { observed_bytes: 99 },
        RetainedHostOutcome::Closed,
    ] {
        let bytes = outcome.encode().unwrap();
        assert_eq!(RetainedHostOutcome::decode(&bytes).unwrap(), outcome);
    }
    assert_eq!(
        RetainedHostOutcome::Closed.encode().unwrap(),
        vec![1, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    let mut bytes = RetainedHostOutcome::Closed.encode().unwrap();
    bytes[0] = 2;
    assert_eq!(RetainedHostOutcome::decode(&bytes), Err(UNSUPPORTED));
    bytes[0] = 1;
    bytes[4] = 99;
    assert_eq!(RetainedHostOutcome::decode(&bytes), Err(UNSUPPORTED));
    bytes[4] = 5;
    bytes.push(1);
    assert_eq!(RetainedHostOutcome::decode(&bytes), Err(INVALID_ARGUMENT));
}
#[test]
fn oversize_observation_does_not_ack_drop_or_reexecute() {
    let host = Box::new(Host::new());
    let port = unsafe { RetainedHostPort::from_native_owned(table(&host)) }.unwrap();
    let mut budget = reservation();
    budget.max_outcome_bytes = 16;
    let operation = port.begin(9, &request(), budget).unwrap();
    host.joined.store(true, Ordering::SeqCst);
    assert!(operation.observe().is_err());
    assert!(operation.observe().is_err());
    assert_eq!(host.effects.load(Ordering::SeqCst), 1);
    assert!(!host.acked.load(Ordering::SeqCst));
    assert!(!port.release().unwrap());
}
unsafe extern "C" fn open(
    handle: u64,
    version: u16,
    output: *mut NativeRetainedHostPortV1,
) -> AbiStatus {
    if version != 1 {
        return UNSUPPORTED;
    }
    if handle == 0 || output.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        output.write(table(host(handle)));
    }
    OK
}
#[test]
fn open_transfers_only_selected_owner_at_exact_version() {
    let host = Box::new(Host::new());
    let handle = std::ptr::from_ref(&*host) as u64;
    let callback: NativeRetainedHostOpenV1 = open;
    let mut output = std::mem::MaybeUninit::uninit();
    assert_eq!(
        unsafe { callback(handle, 2, output.as_mut_ptr()) },
        UNSUPPORTED
    );
    assert_eq!(
        unsafe { callback(0, 1, output.as_mut_ptr()) },
        INVALID_ARGUMENT
    );
    assert_eq!(unsafe { callback(handle, 1, output.as_mut_ptr()) }, OK);
    let port = unsafe { RetainedHostPort::from_native_owned(output.assume_init()) }.unwrap();
    let operation = port.begin(9, &request(), reservation()).unwrap();
    assert_eq!(operation.id(), 7);
    assert_eq!(host.effects.load(Ordering::SeqCst), 1);
}

unsafe fn same_slice(left: NativeRetainedSliceV1, right: NativeRetainedSliceV1) -> bool {
    left.length == right.length
        && unsafe {
            std::slice::from_raw_parts(left.pointer, left.length as usize)
                == std::slice::from_raw_parts(right.pointer, right.length as usize)
        }
}
unsafe extern "C" fn lookup_v2(
    handle: u64,
    id: u64,
    facts: *const NativeRetainedRequestV1,
    bounds: *const NativeRetainedReservationV1,
    output: *mut u64,
) -> AbiStatus {
    if facts.is_null() || bounds.is_null() || output.is_null() || id == 0 {
        return INVALID_ARGUMENT;
    }
    unsafe {
        output.write(0);
    }
    let facts = unsafe { &*facts };
    let bounds = unsafe { &*bounds };
    if bounds.validate().is_err() {
        return INVALID_ARGUMENT;
    }
    let expected = native_request(&request()).unwrap();
    let original = reservation();
    if facts.size != expected.size
        || facts.version != expected.version
        || facts.kind != expected.kind
        || !unsafe { same_slice(facts.capability, expected.capability) }
        || !unsafe { same_slice(facts.interface, expected.interface) }
        || !unsafe { same_slice(facts.operation, expected.operation) }
        || !unsafe { same_slice(facts.payload, expected.payload) }
        || bounds.size != original.size
        || bounds.version != original.version
        || bounds.max_outcome_bytes != original.max_outcome_bytes
        || bounds.operation_slots != original.operation_slots
        || bounds.reserved != original.reserved
    {
        return INVALID_ARGUMENT;
    }
    let selected = *unsafe { host(handle) }.request.lock().unwrap();
    if selected != Some(id) {
        return super::super::NOT_FOUND;
    }
    unsafe {
        output.write(7);
    }
    OK
}
fn table_v2(host: &Host) -> NativeRetainedHostPortV2 {
    let mut base = table(host);
    base.version = RETAINED_HOST_LOOKUP_VERSION;
    base.size = u32::try_from(std::mem::size_of::<NativeRetainedHostPortV2>()).unwrap();
    NativeRetainedHostPortV2 {
        base,
        lookup: lookup_v2,
    }
}
#[test]
fn recovery_lookup_never_replays_or_consumes_and_survives_close_and_ack() {
    let host = Box::new(Host::new());
    let other = Box::new(Host::new());
    let port = unsafe { RetainedHostPort::from_native_owned_v2(table_v2(&host)) }.unwrap();
    let foreign = unsafe { RetainedHostPort::from_native_owned_v2(table_v2(&other)) }.unwrap();
    assert!(port.lookup(9, &request(), reservation()).unwrap().is_none());
    assert_eq!(host.effects.load(Ordering::SeqCst), 0);
    let lost_reply = port.begin(9, &request(), reservation()).unwrap();
    drop(lost_reply);
    assert!(
        foreign
            .lookup(9, &request(), reservation())
            .unwrap()
            .is_none()
    );
    let recovered = port.lookup(9, &request(), reservation()).unwrap().unwrap();
    assert_eq!(recovered.id(), 7);
    assert_eq!(
        recovered.observe().unwrap(),
        RetainedHostObservation::Pending
    );
    let mut conflict = reservation();
    conflict.max_outcome_bytes += 1;
    assert!(port.lookup(9, &request(), conflict).is_err());
    for facts in [
        HostServiceRequest {
            kind: crate::ServiceKind::Command,
            ..request()
        },
        HostServiceRequest {
            capability: "other",
            ..request()
        },
        HostServiceRequest {
            interface: "other/v1",
            ..request()
        },
        HostServiceRequest {
            operation: "other",
            ..request()
        },
        HostServiceRequest {
            payload: b"other",
            ..request()
        },
    ] {
        assert!(port.lookup(9, &facts, reservation()).is_err());
    }
    port.control(RETAINED_HOST_CONTROL_CLOSE).unwrap();
    assert_eq!(
        port.lookup(9, &request(), reservation())
            .unwrap()
            .unwrap()
            .id(),
        7
    );
    host.joined.store(true, Ordering::SeqCst);
    let first = recovered.observe().unwrap();
    let same = port.lookup(9, &request(), reservation()).unwrap().unwrap();
    assert_eq!(same.observe().unwrap(), first);
    same.acknowledge(11).unwrap();
    assert!(same.observe().is_err());
    assert_eq!(
        port.lookup(9, &request(), reservation())
            .unwrap()
            .unwrap()
            .id(),
        7
    );
    assert_eq!(host.effects.load(Ordering::SeqCst), 1);
    assert_eq!(other.effects.load(Ordering::SeqCst), 0);
    assert!(port.release().unwrap());
}
#[test]
fn recovery_v1_and_malformed_v2_reject_without_effects() {
    let host = Box::new(Host::new());
    let legacy = unsafe { RetainedHostPort::from_native_owned(table(&host)) }.unwrap();
    assert!(matches!(
        legacy.lookup(9, &request(), reservation()),
        Err(PluginError::UnsupportedHostOperation { .. })
    ));
    let mut wrong = table_v2(&host);
    wrong.base.version = 1;
    assert!(unsafe { RetainedHostPort::from_native_owned_v2(wrong) }.is_err());
    wrong = table_v2(&host);
    wrong.base.version = 99;
    assert!(unsafe { RetainedHostPort::from_native_owned_v2(wrong) }.is_err());
    wrong = table_v2(&host);
    wrong.base.size -= 1;
    assert!(unsafe { RetainedHostPort::from_native_owned_v2(wrong) }.is_err());
    wrong = table_v2(&host);
    wrong.base.size += 8;
    assert!(unsafe { RetainedHostPort::from_native_owned_v2(wrong) }.is_err());
    assert!(unsafe { RetainedHostPort::from_native_owned(table_v2(&host).base) }.is_err());
    let port = unsafe { RetainedHostPort::from_native_owned_v2(table_v2(&host)) }.unwrap();
    assert!(port.lookup(0, &request(), reservation()).is_err());
    let mut bounds = reservation();
    bounds.reserved = 1;
    assert!(port.lookup(9, &request(), bounds).is_err());
    assert_eq!(host.effects.load(Ordering::SeqCst), 0);
}
#[test]
fn recovery_layouts_preserve_every_frozen_prefix() {
    assert_eq!(std::mem::offset_of!(NativeRetainedHostPortV2, base), 0);
    assert_eq!(
        std::mem::offset_of!(NativeRetainedHostPortV2, lookup),
        std::mem::size_of::<NativeRetainedHostPortV1>()
    );
    if usize::BITS == 64 {
        assert_eq!(std::mem::size_of::<NativeRetainedHostPortV1>(), 64);
        assert_eq!(std::mem::size_of::<NativeRetainedHostPortV2>(), 72);
        assert_eq!(
            std::mem::size_of::<super::super::NativeInvocationPortsV3RetainedHostV2>(),
            112
        );
    }
}

struct LegacyStatic;
impl RetainedHostDispatch for LegacyStatic {
    fn begin(
        &self,
        _: u64,
        _: &HostServiceRequest<'_>,
        _: NativeRetainedReservationV1,
    ) -> crate::Result<u64> {
        Err(error(UNSUPPORTED))
    }
    fn observe(&self, _: u64, _: u64) -> crate::Result<RetainedHostObservation> {
        Err(error(UNSUPPORTED))
    }
    fn control(&self, _: u64, _: u32) -> crate::Result<bool> {
        Err(error(UNSUPPORTED))
    }
    fn acknowledge(&self, _: u64, _: u64) -> crate::Result<()> {
        Err(error(UNSUPPORTED))
    }
    fn release(&self) -> crate::Result<bool> {
        Ok(false)
    }
}
#[test]
fn static_lookup_defaults_to_unsupported_not_absence() {
    let port = RetainedHostPort::new(Arc::new(LegacyStatic));
    assert!(matches!(
        port.lookup(9, &request(), reservation()),
        Err(PluginError::UnsupportedHostOperation { .. })
    ));
}

struct StaticRecovery;
impl RetainedHostDispatch for StaticRecovery {
    fn begin(
        &self,
        _: u64,
        _: &HostServiceRequest<'_>,
        _: NativeRetainedReservationV1,
    ) -> crate::Result<u64> {
        panic!("lookup must never dispatch begin")
    }
    fn lookup(
        &self,
        id: u64,
        facts: &HostServiceRequest<'_>,
        bounds: NativeRetainedReservationV1,
    ) -> crate::Result<Option<u64>> {
        let original = request();
        let expected = reservation();
        if facts.kind != original.kind
            || facts.capability != original.capability
            || facts.interface != original.interface
            || facts.operation != original.operation
            || facts.payload != original.payload
            || bounds.size != expected.size
            || bounds.version != expected.version
            || bounds.max_outcome_bytes != expected.max_outcome_bytes
            || bounds.operation_slots != expected.operation_slots
            || bounds.reserved != expected.reserved
        {
            return Err(error(INVALID_ARGUMENT));
        }
        Ok((id == 9).then_some(7))
    }
    fn observe(&self, id: u64, max_bytes: u64) -> crate::Result<RetainedHostObservation> {
        assert_eq!(id, 7);
        assert_eq!(max_bytes, reservation().max_outcome_bytes);
        Ok(RetainedHostObservation::Pending)
    }
    fn control(&self, _: u64, _: u32) -> crate::Result<bool> {
        Ok(true)
    }
    fn acknowledge(&self, _: u64, _: u64) -> crate::Result<()> {
        Err(error(INVALID_ARGUMENT))
    }
    fn release(&self) -> crate::Result<bool> {
        Ok(false)
    }
}
#[test]
fn static_recovery_uses_same_helper_and_original_bounds_without_begin() {
    let port = RetainedHostPort::new(Arc::new(StaticRecovery));
    port.control(RETAINED_HOST_CONTROL_CLOSE).unwrap();
    assert!(
        port.lookup(10, &request(), reservation())
            .unwrap()
            .is_none()
    );
    let recovered = port.lookup(9, &request(), reservation()).unwrap().unwrap();
    assert_eq!(recovered.id(), 7);
    assert_eq!(
        recovered.observe().unwrap(),
        RetainedHostObservation::Pending
    );
    let mut conflicting = reservation();
    conflicting.max_outcome_bytes += 1;
    assert!(port.lookup(9, &request(), conflicting).is_err());
}
