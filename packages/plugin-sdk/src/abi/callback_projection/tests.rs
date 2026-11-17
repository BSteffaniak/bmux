use super::super::NativeInvocationPortsV3CallbackAuthorityV1;
use super::*;
#[derive(Default)]
struct Host {
    notifications: AtomicUsize,
    unsubscribes: AtomicUsize,
    releases: AtomicUsize,
    busy: AtomicBool,
}
fn host(handle: u64) -> &'static Host {
    unsafe { &*(handle as *const Host) }
}
unsafe extern "C" fn retain(_: u64) -> AbiStatus {
    OK
}
unsafe extern "C" fn release(handle: u64) -> AbiStatus {
    host(handle).releases.fetch_add(1, Ordering::SeqCst);
    OK
}
unsafe extern "C" fn live(_: u64) -> AbiStatus {
    OK
}
unsafe extern "C" fn enter(_: u64, _: *mut u64) -> AbiStatus {
    UNSUPPORTED
}
unsafe extern "C" fn leave(_: u64, _: u64) -> AbiStatus {
    OK
}
unsafe extern "C" fn subscribe(
    handle: u64,
    notify: super::super::NativeCallbackCancelNotifyV1,
    userdata: u64,
    output: *mut u64,
) -> AbiStatus {
    unsafe {
        output.write(1);
    }
    host(handle).notifications.fetch_add(1, Ordering::SeqCst);
    unsafe {
        notify(userdata);
    }
    OK
}
unsafe extern "C" fn unsubscribe(handle: u64, _: u64) -> AbiStatus {
    host(handle).unsubscribes.fetch_add(1, Ordering::SeqCst);
    if host(handle).busy.load(Ordering::SeqCst) {
        BUSY
    } else {
        OK
    }
}
unsafe extern "C" fn bind(
    _: u64,
    _: u64,
    _: *mut NativeInvocationPortsV3CallbackAuthorityV1,
) -> AbiStatus {
    UNSUPPORTED
}
fn owner(host: &Host, executor: &Executor) -> Arc<Authority> {
    let owner = Arc::new(Authority {
        table: NativeCallbackAuthorityV1 {
            size: u32::try_from(std::mem::size_of::<NativeCallbackAuthorityV1>()).unwrap(),
            version: 1,
            handle: std::ptr::from_ref(host) as u64,
            retain,
            release,
            check_live: live,
            enter,
            leave,
            subscribe_cancel: subscribe,
            unsubscribe_cancel: unsubscribe,
            bind_entry_ports: bind,
        },
        cancellation: ServiceCancellation::new(),
        entry: Mutex::new(None),
        operation: std::sync::Weak::new(),
        subscriptions: Mutex::new(Vec::new()),
        uncertain: AtomicBool::new(false),
    });
    executor.owners.lock().unwrap().push(Arc::clone(&owner));
    executor.owner_slots.store(1, Ordering::Release);
    owner
}
#[tokio::test]
async fn cancel_before_subscribe_return_is_not_lost_and_unsubscribe_fences() {
    let host = Host::default();
    let executor = Executor::default();
    let owner = owner(&host, &executor);
    Subscription::new(Arc::clone(&owner))
        .unwrap()
        .await
        .unwrap();
    assert_eq!(host.notifications.load(Ordering::SeqCst), 1);
    executor.drain().unwrap();
    assert_eq!(host.unsubscribes.load(Ordering::SeqCst), 1);
    drop(owner);
    executor.retired().unwrap();
    assert_eq!(host.releases.load(Ordering::SeqCst), 1);
}
#[test]
fn busy_unsubscribe_is_inspectable_retryable_and_reclaims_after_fence() {
    let host = Host::default();
    host.busy.store(true, Ordering::SeqCst);
    let executor = Executor::default();
    let owner = owner(&host, &executor);
    let subscription = Subscription::new(Arc::clone(&owner)).unwrap();
    let userdata = Arc::as_ptr(&subscription.state) as u64;
    drop(subscription);
    assert!(matches!(executor.drain(), Err(BUSY)));
    assert_eq!(owner.subscriptions.lock().unwrap().len(), 1);
    assert_eq!(host.releases.load(Ordering::SeqCst), 0);
    unsafe {
        notify(userdata);
    }
    host.busy.store(false, Ordering::SeqCst);
    executor.drain().unwrap();
    assert!(owner.subscriptions.lock().unwrap().is_empty());
    drop(owner);
    executor.retired().unwrap();
    assert_eq!(host.unsubscribes.load(Ordering::SeqCst), 2);
    assert_eq!(host.releases.load(Ordering::SeqCst), 1);
}
unsafe extern "C" fn short_bind(
    _: u64,
    _: u64,
    output: *mut NativeInvocationPortsV3CallbackAuthorityV1,
) -> AbiStatus {
    unsafe {
        output.cast::<super::super::NativeDescriptorHeader>().write(
            super::super::NativeDescriptorHeader {
                size: 8,
                version: 99,
            },
        );
    }
    OK
}

#[test]
fn invalid_entry_tail_stays_in_bounded_raw_escrow_and_blocks_retirement() {
    let host = Host::default();
    let executor = Executor::default();
    let mut owner = owner(&host, &executor);
    // The table is fixture-owned before any authority wrapper is published.
    executor.owners.lock().unwrap().clear();
    Arc::get_mut(&mut owner).unwrap().table.bind_entry_ports = short_bind;
    executor.owners.lock().unwrap().push(Arc::clone(&owner));
    let mut pending = EntryRecord::pending();
    pending.lease = 7;
    *owner.entry.lock().unwrap() = Some(pending);
    assert!(owner.bind(7).is_err());
    {
        let mut records = owner.entry.lock().unwrap();
        let entry = records.as_mut().unwrap();
        assert_eq!(
            std::mem::size_of_val(entry.raw_output.as_ref().unwrap().as_ref()),
            std::mem::size_of::<NativeInvocationPortsV3CallbackAuthorityV1>()
        );
        assert!(entry.transferred.is_none());
        assert!(entry.unknown);
        entry.retiring = true;
        drop(records);
    }
    assert!(owner.live().is_err());
    assert!(matches!(executor.retired(), Err(PANICKED)));
    assert_eq!(host.releases.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn raw_guard_cannot_reach_component_executor_author() {
    let host = Host::default();
    let executor = Executor::default();
    let owner = owner(&host, &executor);
    let authority = PluginCallbackAuthority::from_host(Arc::new(Arc::clone(&owner)));
    let target = PluginInstanceId::from_uuid(uuid::Uuid::from_u128(4));
    assert!(
        executor
            .execute_entered(
                authority,
                target,
                PluginCallbackEntryGuard::from_host(()),
                Box::new(|| panic!("raw guard reached author"))
            )
            .await
            .is_err()
    );
    assert_eq!(executor.jobs.load(Ordering::SeqCst), 0);
    drop(owner);
    executor.retired().unwrap();
}
