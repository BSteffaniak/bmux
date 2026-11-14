//! Opaque native projection of actual host callback authority.
#[cfg(test)]
#[path = "callback_authority_tests.rs"]
mod tests;
use super::{HostPorts, guarded, lookup};
use crate::loader::scoped::{
    Result,
    callbacks::{CallbackBinding, CallbackEntryLease},
    failure,
};
use abi::callback_authority::{NativeCallbackAuthorityV1, NativeCallbackCancelNotifyV1};
use bmux_plugin_sdk::abi::{self, AbiStatus};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Wake, Waker},
    thread::JoinHandle,
};

static NEXT: AtomicU64 = AtomicU64::new(1);
static OWNERS: OnceLock<Mutex<BTreeMap<u64, Arc<Owner>>>> = OnceLock::new();
fn owners() -> &'static Mutex<BTreeMap<u64, Arc<Owner>>> {
    OWNERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}
struct Owner {
    binding: CallbackBinding,
    port: Arc<HostPorts>,
    state: Mutex<State>,
}
struct State {
    references: usize,
    next: u64,
    entries: BTreeMap<u64, CallbackEntryLease>,
    subscriptions: BTreeMap<u64, Subscription>,
}
struct Subscription {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}
fn owner(handle: u64) -> Result<Arc<Owner>> {
    owners()
        .lock()
        .map_err(|_| failure("callback authority registry poisoned"))?
        .get(&handle)
        .cloned()
        .ok_or_else(|| failure("unknown callback authority"))
}
fn next() -> Result<u64> {
    let mut current = NEXT.load(Ordering::Acquire);
    loop {
        let successor = current
            .checked_add(1)
            .ok_or_else(|| failure("callback identity exhausted"))?;
        match NEXT.compare_exchange(current, successor, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}
impl State {
    fn allocate(&mut self) -> Result<u64> {
        let id = self.next;
        self.next = id
            .checked_add(1)
            .ok_or_else(|| failure("callback child identity exhausted"))?;
        Ok(id)
    }
}
pub(super) unsafe extern "C" fn open(
    handle: u64,
    version: u16,
    output: *mut NativeCallbackAuthorityV1,
) -> AbiStatus {
    if version != 1 || output.is_null() {
        return abi::INVALID_ARGUMENT;
    }
    guarded(|| {
        let port = lookup(handle)?;
        let binding = port
            .callback
            .as_ref()
            .ok_or_else(|| failure("callback authority not bound"))?
            .clone();
        // Opening requires the actual live callback port, never its payload IDs.
        let _admission =
            super::host_start_admission(&port)?.map_err(|_| failure("callback port cancelled"))?;
        {
            let state = port
                .state
                .lock()
                .map_err(|_| failure("native port state poisoned"))?;
            if state.closed || !state.retained {
                return Err(failure("callback port closed"));
            }
        }
        binding.validate()?;
        let handle = next()?;
        let owner = Arc::new(Owner {
            binding,
            port,
            state: Mutex::new(State {
                references: 1,
                next: 1,
                entries: BTreeMap::new(),
                subscriptions: BTreeMap::new(),
            }),
        });
        let mut registry = owners()
            .lock()
            .map_err(|_| failure("callback authority registry poisoned"))?;
        if registry.len() >= 4096 {
            return Err(failure("callback authority capacity exhausted"));
        }
        registry.insert(handle, owner);
        drop(registry);
        unsafe {
            output.write(NativeCallbackAuthorityV1 {
                size: u32::try_from(std::mem::size_of::<NativeCallbackAuthorityV1>())
                    .map_err(|e| failure(e.to_string()))?,
                version: 1,
                handle,
                retain,
                release,
                check_live,
                enter,
                leave,
                subscribe_cancel,
                unsubscribe_cancel,
                bind_entry_ports,
            });
        }
        Ok(())
    })
}
unsafe extern "C" fn retain(handle: u64) -> AbiStatus {
    guarded(|| {
        let owner = owner(handle)?;
        let mut state = owner
            .state
            .lock()
            .map_err(|_| failure("callback authority poisoned"))?;
        if state.references == 0 {
            return Err(failure("callback authority released"));
        }
        state.references = state
            .references
            .checked_add(1)
            .ok_or_else(|| failure("callback refcount exhausted"))?;
        drop(state);
        Ok(())
    })
}
unsafe extern "C" fn release(handle: u64) -> AbiStatus {
    let mut status = abi::OK;
    let result = guarded(|| {
        let mut registry = owners()
            .lock()
            .map_err(|_| failure("callback authority registry poisoned"))?;
        let owner = registry
            .get(&handle)
            .cloned()
            .ok_or_else(|| failure("unknown callback authority"))?;
        let mut state = owner
            .state
            .lock()
            .map_err(|_| failure("callback authority poisoned"))?;
        if state.references == 1 && (!state.entries.is_empty() || !state.subscriptions.is_empty()) {
            status = abi::BUSY;
        } else {
            state.references = state
                .references
                .checked_sub(1)
                .ok_or_else(|| failure("callback authority already released"))?;
            if state.references == 0 {
                registry.remove(&handle);
            }
        }
        drop(state);
        drop(registry);
        Ok(())
    });
    if result == abi::OK { status } else { result }
}
unsafe extern "C" fn check_live(handle: u64) -> AbiStatus {
    guarded(|| owner(handle)?.binding.validate())
}
unsafe extern "C" fn enter(handle: u64, output: *mut u64) -> AbiStatus {
    if output.is_null() {
        return abi::INVALID_ARGUMENT;
    }
    unsafe {
        output.write(0);
    }
    guarded(|| {
        let owner = owner(handle)?;
        let entry = owner.binding.enter().map_err(|e| failure(e.message))?;
        let mut state = owner
            .state
            .lock()
            .map_err(|_| failure("callback authority poisoned"))?;
        if state.references == 0 {
            return Err(failure("callback authority released"));
        }
        let id = state.allocate()?;
        state.entries.insert(id, entry);
        drop(state);
        unsafe {
            output.write(id);
        }
        Ok(())
    })
}
unsafe extern "C" fn leave(handle: u64, lease: u64) -> AbiStatus {
    let mut status = abi::OK;
    let result = guarded(|| {
        let owner = owner(handle)?;
        let mut state = owner
            .state
            .lock()
            .map_err(|_| failure("callback authority poisoned"))?;
        if state
            .entries
            .get(&lease)
            .ok_or_else(|| failure("unknown callback entry lease"))?
            .has_dependents()
        {
            status = abi::BUSY;
            return Ok(());
        }
        let entry = state
            .entries
            .remove(&lease)
            .ok_or_else(|| failure("unknown callback entry lease"))?;
        drop(state);
        drop(entry);
        Ok(())
    });
    if result == abi::OK { status } else { result }
}
struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
unsafe extern "C" fn subscribe_cancel(
    handle: u64,
    notify: NativeCallbackCancelNotifyV1,
    userdata: u64,
    output: *mut u64,
) -> AbiStatus {
    if output.is_null() {
        return abi::INVALID_ARGUMENT;
    }
    unsafe {
        output.write(0);
    }
    guarded(|| {
        let owner = owner(handle)?;
        let mut state = owner
            .state
            .lock()
            .map_err(|_| failure("callback authority poisoned"))?;
        if state.references == 0 || state.subscriptions.len() >= 32 {
            return Err(failure(
                "callback cancellation subscription capacity exhausted",
            ));
        }
        let budget = owner.binding.subscription()?;
        let id = state.allocate()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let armed = Arc::new(AtomicBool::new(false));
        let start = armed.clone();
        let pinned = owner.clone();
        let thread = std::thread::Builder::new()
            .name("plugin-callback-cancel".into())
            .spawn(move || {
                let _budget = budget;
                while !start.load(Ordering::Acquire) {
                    std::thread::park();
                }
                let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
                let mut context = std::task::Context::from_waker(&waker);
                let mut cancellation = pinned.binding.context().cancelled();
                while !stopped.load(Ordering::Acquire) {
                    if cancellation.as_mut().poll(&mut context).is_ready() {
                        // Unsubscribe fences this thread, including this external call.
                        // Notification is exactly once; it never executes author work.
                        if !stopped.load(Ordering::Acquire) {
                            unsafe {
                                notify(userdata);
                            }
                        }
                        break;
                    }
                    std::thread::park();
                }
            })
            .map_err(|e| failure(e.to_string()))?;
        let worker = thread.thread().clone();
        state
            .subscriptions
            .insert(id, Subscription { stop, thread });
        unsafe {
            output.write(id);
        }
        drop(state);
        armed.store(true, Ordering::Release);
        worker.unpark();
        Ok(())
    })
}
unsafe extern "C" fn unsubscribe_cancel(handle: u64, subscription: u64) -> AbiStatus {
    let mut status = abi::OK;
    let result = guarded(|| {
        let owner = owner(handle)?;
        let mut state = owner
            .state
            .lock()
            .map_err(|_| failure("callback authority poisoned"))?;
        let sub = state
            .subscriptions
            .get(&subscription)
            .ok_or_else(|| failure("unknown callback cancellation subscription"))?;
        sub.stop.store(true, Ordering::Release);
        sub.thread.thread().unpark();
        if !sub.thread.is_finished() {
            status = abi::BUSY;
            return Ok(());
        }
        let sub = state
            .subscriptions
            .remove(&subscription)
            .ok_or_else(|| failure("subscription disappeared"))?;
        drop(state);
        sub.thread
            .join()
            .map_err(|_| failure("callback cancellation notifier panicked"))?;
        Ok(())
    });
    if result == abi::OK { status } else { result }
}
unsafe extern "C" fn bind_entry_ports(
    handle: u64,
    lease: u64,
    output: *mut abi::callback_authority::NativeInvocationPortsV3CallbackAuthorityV1,
) -> AbiStatus {
    if output.is_null() {
        return abi::INVALID_ARGUMENT;
    }
    guarded(|| {
        let owner = owner(handle)?;
        owner.binding.validate()?;
        let entry = owner
            .state
            .lock()
            .map_err(|_| failure("callback authority poisoned"))?
            .entries
            .get(&lease)
            .cloned()
            .ok_or_else(|| failure("unknown callback entry lease"))?;
        entry.validate_binding(&owner.binding)?;
        let table = owner.port.bind_entry(entry)?;
        unsafe {
            output.write(table);
        }
        Ok(())
    })
}
