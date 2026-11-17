use super::*;
use crate::loader::scoped::tests::fixture;
use bmux_plugin_sdk::ServiceCancellation;
use std::sync::atomic::AtomicUsize;
static NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);
unsafe extern "C" fn notify(_: u64) {
    NOTIFICATIONS.fetch_add(1, Ordering::AcqRel);
}

#[test]
fn native_entry_ports_pin_dependency_until_explicit_release() {
    let execution = fixture();
    let invocation = execution.activation().unwrap().begin_invocation().unwrap();
    let binding = execution
        .bind_root_callback(invocation.clone(), ServiceCancellation::default())
        .unwrap();
    let port = HostPorts::new(invocation, None, None).with_callback(Some(binding.clone()));
    let table = port.install_for(abi::ABI_VERSION, None).unwrap();
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    let mut authority = std::mem::MaybeUninit::uninit();
    assert_eq!(
        unsafe { open(table.handle, 1, authority.as_mut_ptr()) },
        abi::OK
    );
    let authority = unsafe { authority.assume_init() };
    let mut lease = 0;
    assert_eq!(
        unsafe { (authority.enter)(authority.handle, &raw mut lease) },
        abi::OK
    );
    let mut bound = std::mem::MaybeUninit::uninit();
    assert_eq!(
        unsafe { (authority.bind_entry_ports)(authority.handle, lease, bound.as_mut_ptr()) },
        abi::OK
    );
    let bound = unsafe { bound.assume_init() };
    let base = bound.base.base.base.base.base;
    let selected = super::lookup(base.handle).unwrap();
    let exact = owner(authority.handle)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .entries
        .get(&lease)
        .unwrap()
        .clone();
    assert!(
        selected
            .owned_entry
            .as_ref()
            .unwrap()
            .same_dependency(&exact)
    );
    drop(exact);
    drop(selected);
    let foreign_scope = execution.activation().unwrap().begin_invocation().unwrap();
    let foreign = execution
        .bind_root_callback(foreign_scope, ServiceCancellation::default())
        .unwrap();
    let foreign_entry = foreign.enter().unwrap();
    assert!(port.bind_entry(foreign_entry).is_err());
    assert_eq!(
        unsafe { (authority.leave)(authority.handle, lease) },
        abi::BUSY
    );
    assert_eq!(unsafe { (authority.release)(authority.handle) }, abi::BUSY);
    assert_eq!(unsafe { (base.release)(base.handle) }, abi::OK);
    assert_eq!(
        unsafe { (authority.leave)(authority.handle, lease) },
        abi::OK
    );
    assert!(binding.active_entry().is_err());
    assert_eq!(unsafe { (authority.release)(authority.handle) }, abi::OK);
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    HostPorts::retire(&table).unwrap();
}

// The notifier is a task on the selected callback runtime (the fixture binds
// this test runtime), woken by the cancellation itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_cancel_subscription_is_fenced_before_release() {
    let execution = fixture();
    let invocation = execution.activation().unwrap().begin_invocation().unwrap();
    let binding = execution
        .bind_root_callback(invocation.clone(), ServiceCancellation::default())
        .unwrap();
    let port = HostPorts::new(invocation.clone(), None, None).with_callback(Some(binding));
    let table = port.install_for(abi::ABI_VERSION, None).unwrap();
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    let mut authority = std::mem::MaybeUninit::uninit();
    assert_eq!(
        unsafe { open(table.handle, 1, authority.as_mut_ptr()) },
        abi::OK
    );
    let authority = unsafe { authority.assume_init() };
    let mut subscription = 0;
    let before = NOTIFICATIONS.load(Ordering::Acquire);
    assert_eq!(
        unsafe { (authority.subscribe_cancel)(authority.handle, notify, 0, &raw mut subscription) },
        abi::OK
    );
    invocation.close().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while NOTIFICATIONS.load(Ordering::Acquire) == before {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    loop {
        let status = unsafe { (authority.unsubscribe_cancel)(authority.handle, subscription) };
        if status == abi::OK {
            break;
        }
        assert_eq!(status, abi::BUSY);
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(NOTIFICATIONS.load(Ordering::Acquire), before + 1);
    assert_eq!(unsafe { (authority.release)(authority.handle) }, abi::OK);
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    HostPorts::retire(&table).unwrap();
}

static FENCED: AtomicUsize = AtomicUsize::new(0);
unsafe extern "C" fn fenced_notify(_: u64) {
    FENCED.fetch_add(1, Ordering::AcqRel);
}

/// Unsubscribing before the cancellation fences the notifier: once it
/// returned OK the external notification can never run, even though the
/// invocation is cancelled afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_cancel_unsubscribe_before_cancellation_never_notifies() {
    let execution = fixture();
    let invocation = execution.activation().unwrap().begin_invocation().unwrap();
    let binding = execution
        .bind_root_callback(invocation.clone(), ServiceCancellation::default())
        .unwrap();
    let port = HostPorts::new(invocation.clone(), None, None).with_callback(Some(binding));
    let table = port.install_for(abi::ABI_VERSION, None).unwrap();
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    let mut authority = std::mem::MaybeUninit::uninit();
    assert_eq!(
        unsafe { open(table.handle, 1, authority.as_mut_ptr()) },
        abi::OK
    );
    let authority = unsafe { authority.assume_init() };
    let mut subscription = 0;
    assert_eq!(
        unsafe {
            (authority.subscribe_cancel)(authority.handle, fenced_notify, 0, &raw mut subscription)
        },
        abi::OK
    );
    assert_eq!(
        unsafe { (authority.unsubscribe_cancel)(authority.handle, subscription) },
        abi::OK
    );
    invocation.close().unwrap();
    tokio::task::yield_now().await;
    assert_eq!(FENCED.load(Ordering::Acquire), 0);
    assert_eq!(unsafe { (authority.release)(authority.handle) }, abi::OK);
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    HostPorts::retire(&table).unwrap();
}
