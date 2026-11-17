//! Component-local projection with bounded instance-owned cleanup records.
mod cleanup;
#[cfg(test)]
mod tests;
use super::{
    AbiStatus, BUSY, INVALID_ARGUMENT, NativeCallbackAuthorityV1,
    NativeInvocationPortsV3CallbackAuthorityV1, OK, PANICKED, UNSUPPORTED, ports::InvocationPorts,
};
use crate::{
    PluginBlockingCallback, PluginCallbackAuthority, PluginCallbackAuthoritySource,
    PluginCallbackContext, PluginCallbackEntryGuard, PluginCallbackExecution,
    PluginCallbackExecutor, PluginCallbackSettlement, PluginCommandError, PluginHostServices,
    PluginResult, ServiceCancellation,
};
use bmux_plugin_runtime::PluginInstanceId;
use cleanup::{EntryRecord, SubscriptionRecord};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

fn failure(status: AbiStatus) -> PluginCommandError {
    PluginCommandError::unavailable(format!("native callback authority status {status}"))
}
struct Authority {
    table: NativeCallbackAuthorityV1,
    cancellation: ServiceCancellation,
    entry: Mutex<Option<EntryRecord>>,
    subscriptions: Mutex<Vec<SubscriptionRecord>>,
    uncertain: AtomicBool,
}
impl Authority {
    fn live(&self) -> PluginResult<()> {
        if self.uncertain.load(Ordering::Acquire) {
            return Err(failure(PANICKED));
        }
        if self
            .entry
            .lock()
            .map_err(|_| failure(PANICKED))?
            .as_ref()
            .is_some_and(|entry| entry.unknown || entry.retiring)
        {
            return Err(failure(BUSY));
        }
        if self
            .subscriptions
            .lock()
            .map_err(|_| failure(PANICKED))?
            .iter()
            .any(|record| record.unknown)
        {
            return Err(failure(PANICKED));
        }
        let status = unsafe { (self.table.check_live)(self.table.handle) };
        if status == OK {
            Ok(())
        } else {
            Err(failure(status))
        }
    }
    fn ports(&self) -> crate::Result<Arc<InvocationPorts>> {
        self.entry
            .lock()
            .map_err(|_| crate::PluginError::ServiceProtocol {
                details: "native entry poisoned".into(),
            })?
            .as_ref()
            .and_then(|entry| entry.ports.clone())
            .ok_or(crate::PluginError::UnsupportedHostOperation {
                operation: "callback_entry_ports",
            })
    }
    fn bind(self: &Arc<Self>, lease: u64) -> PluginResult<Arc<InvocationPorts>> {
        let mut output =
            std::mem::MaybeUninit::<NativeInvocationPortsV3CallbackAuthorityV1>::zeroed();
        let status =
            unsafe { (self.table.bind_entry_ports)(self.table.handle, lease, output.as_mut_ptr()) };
        if status != OK {
            return Err(failure(status));
        }
        self.escrow_output(&output)?;
        let header = unsafe {
            output
                .as_ptr()
                .cast::<super::NativeDescriptorHeader>()
                .read()
        };
        if header.version != super::PORTS_VERSION_V3
            || (header.size as usize)
                < std::mem::size_of::<NativeInvocationPortsV3CallbackAuthorityV1>()
        {
            self.mark_unknown()?;
            return Err(failure(INVALID_ARGUMENT));
        }
        let output = unsafe { output.assume_init() };
        let base = output.base.base.base.base.base;
        // Record the transferred reference BEFORE validation/acquisition. A
        // rejected or BUSY release remains inspectable in this entry record.
        {
            let mut entry = self.entry.lock().map_err(|_| failure(PANICKED))?;
            entry
                .as_mut()
                .ok_or_else(|| failure(INVALID_ARGUMENT))?
                .transferred = Some(base);
        }
        // The host transferred this table's only reference; adopting it moves
        // that reference into the ports (released when they close) instead of
        // retaining a second one. Until adoption succeeds the entry record
        // keeps the transferred reference for explicit release.
        let ports =
            unsafe { InvocationPorts::adopt((&raw const output).cast()) }.map_err(failure)?;
        {
            let mut guard = self.entry.lock().map_err(|_| failure(PANICKED))?;
            let entry = guard.as_mut().ok_or_else(|| failure(INVALID_ARGUMENT))?;
            entry.ports = Some(Arc::clone(&ports));
            entry.transferred = None;
            drop(guard);
        }
        ports
            .bind_cancellation(self.cancellation.clone())
            .map_err(failure)?;
        Ok(ports)
    }
    fn escrow_output(
        &self,
        output: &std::mem::MaybeUninit<NativeInvocationPortsV3CallbackAuthorityV1>,
    ) -> PluginResult<()> {
        // Retain the output allocation without inspecting padding or creating
        // invalid function-pointer values from an unsupported tail.
        let raw = Arc::new(*output);
        let header = unsafe {
            output
                .as_ptr()
                .cast::<super::NativeDescriptorHeader>()
                .read()
        };
        let prefix = if header.version == super::PORTS_VERSION_V3
            && (header.size as usize) >= std::mem::size_of::<super::NativeInvocationPorts>()
        {
            let prefix = unsafe {
                output
                    .as_ptr()
                    .cast::<super::NativeInvocationPorts>()
                    .read()
            };
            (prefix.handle != 0 && prefix.capabilities & !super::SCOPED_CAPABILITIES == 0)
                .then_some(prefix)
        } else {
            None
        };
        let mut guard = self.entry.lock().map_err(|_| failure(PANICKED))?;
        let entry = guard.as_mut().ok_or_else(|| failure(INVALID_ARGUMENT))?;
        entry.raw_output = Some(raw);
        entry.transferred = prefix;
        drop(guard);
        Ok(())
    }
    fn mark_unknown(&self) -> PluginResult<()> {
        self.uncertain.store(true, Ordering::Release);
        let mut entry = self.entry.lock().map_err(|_| failure(PANICKED))?;
        entry
            .as_mut()
            .ok_or_else(|| failure(INVALID_ARGUMENT))?
            .unknown = true;
        drop(entry);
        Ok(())
    }
}
struct Entry {
    owner: Arc<Authority>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        if let Ok(mut entry) = self.owner.entry.lock() {
            if let Some(entry) = entry.as_mut() {
                entry.retiring = true;
            }
        } else {
            self.owner.uncertain.store(true, Ordering::Release);
        }
        // The retired lease may unblock a pending terminal drain.
        super::host_wake::notify_progress();
    }
}
impl PluginCallbackAuthoritySource for Arc<Authority> {
    fn check_live(&self) -> PluginResult<()> {
        self.live()
    }
    fn enter_adapter_entry(&self, _: PluginInstanceId) -> PluginResult<PluginCallbackEntryGuard> {
        self.live()?;
        // Reserve the only entry slot before invoking host code; never hold a
        // component mutex over an ABI callback. Reentry cannot consume it.
        {
            let mut slot = self.entry.lock().map_err(|_| failure(PANICKED))?;
            if slot.is_some() {
                return Err(failure(BUSY));
            }
            *slot = Some(EntryRecord::pending());
        }
        let mut lease = 0;
        let status = unsafe { (self.table.enter)(self.table.handle, &raw mut lease) };
        {
            let mut slot = self.entry.lock().map_err(|_| failure(PANICKED))?;
            let entry = slot.as_mut().ok_or_else(|| failure(INVALID_ARGUMENT))?;
            entry.lease = lease;
            entry.retiring = status != OK || lease == 0;
            entry.unknown = status != OK && lease != 0 || status == OK && lease == 0;
            if status != OK && lease == 0 {
                *slot = None;
            }
        }
        if status != OK {
            return Err(failure(status));
        }
        if lease == 0 {
            return Err(failure(INVALID_ARGUMENT));
        }
        if let Err(error) = self.bind(lease) {
            if let Ok(mut slot) = self.entry.lock()
                && let Some(entry) = slot.as_mut()
            {
                entry.retiring = true;
            }
            return Err(error);
        }
        Ok(PluginCallbackEntryGuard::from_host(Entry {
            owner: Self::clone(self),
        }))
    }
    fn cancelled(&self) -> PluginCallbackSettlement {
        let owner = Self::clone(self);
        Box::pin(async move { Subscription::new(owner)?.await })
    }
}
struct WakeState {
    cancelled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}
unsafe extern "C" fn notify(userdata: u64) {
    // The instance cleanup record owns this allocation until fenced unsubscribe.
    let _ = std::panic::catch_unwind(|| {
        let state = unsafe { &*(userdata as *const WakeState) };
        state.cancelled.store(true, Ordering::Release);
        let wake = state.waker.lock().ok().and_then(|mut slot| slot.take());
        if let Some(wake) = wake {
            wake.wake();
        }
    });
}
struct Subscription {
    owner: Arc<Authority>,
    state: Arc<WakeState>,
    id: u64,
}
impl Subscription {
    fn new(owner: Arc<Authority>) -> PluginResult<Self> {
        owner.live()?;
        let state = Arc::new(WakeState {
            cancelled: AtomicBool::new(false),
            waker: Mutex::new(None),
        });
        {
            let mut records = owner.subscriptions.lock().map_err(|_| failure(PANICKED))?;
            if records.len() >= 32 {
                return Err(failure(BUSY));
            }
            records.push(SubscriptionRecord {
                id: 0,
                state: Arc::clone(&state),
                retiring: false,
                unknown: false,
            });
        }
        let userdata = Arc::as_ptr(&state) as u64;
        let mut id = 0;
        let status = unsafe {
            (owner.table.subscribe_cancel)(owner.table.handle, notify, userdata, &raw mut id)
        };
        {
            let mut records = owner.subscriptions.lock().map_err(|_| failure(PANICKED))?;
            let position = records
                .iter()
                .position(|record| Arc::ptr_eq(&record.state, &state))
                .ok_or_else(|| failure(INVALID_ARGUMENT))?;
            if status != OK && id == 0 {
                records.remove(position);
            } else {
                let record = &mut records[position];
                record.id = id;
                record.retiring = status != OK || id == 0;
                record.unknown = status != OK || id == 0;
            }
            drop(records);
        }
        if status != OK {
            return Err(failure(status));
        }
        if id == 0 {
            return Err(failure(INVALID_ARGUMENT));
        }
        Ok(Self { owner, state, id })
    }
}
impl Future for Subscription {
    type Output = PluginResult<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.cancelled.load(Ordering::Acquire) {
            return Poll::Ready(Ok(()));
        }
        let Ok(mut slot) = self.state.waker.lock() else {
            return Poll::Ready(Err(failure(PANICKED)));
        };
        *slot = Some(cx.waker().clone());
        drop(slot);
        if self.state.cancelled.load(Ordering::Acquire) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        if let Ok(mut records) = self.owner.subscriptions.lock() {
            if let Some(record) = records.iter_mut().find(|record| record.id == self.id) {
                record.retiring = true;
            }
        } else {
            self.owner.uncertain.store(true, Ordering::Release);
        }
        super::host_wake::notify_progress();
    }
}

/// One bounded executor and cleanup owner per actual component instance.
#[derive(Default)]
pub(super) struct Executor {
    draining: AtomicBool,
    jobs: Arc<AtomicUsize>,
    owners: Mutex<Vec<Arc<Authority>>>,
    owner_slots: AtomicUsize,
    // Invalid successful open cannot be decoded/released speculatively. Keep
    // its exact origin and status bounded and visible to lifecycle inspection.
    open_failures: Mutex<Vec<(Arc<InvocationPorts>, AbiStatus)>>,
}
struct JobSlot(Arc<AtomicUsize>);
impl Drop for JobSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
// A bounded CAS loop is compatible with both product-pinned compiler versions.
fn reserve(counter: &AtomicUsize) -> PluginResult<()> {
    for _ in 0..64 {
        let current = counter.load(Ordering::Acquire);
        if current >= 32 {
            return Err(failure(BUSY));
        }
        if counter
            .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(failure(BUSY))
}
impl PluginCallbackExecution for Executor {
    fn execute(
        &self,
        _: PluginCallbackAuthority,
        _: PluginInstanceId,
        _: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        Box::pin(async { Err(failure(UNSUPPORTED)) })
    }
    fn execute_entered(
        &self,
        authority: PluginCallbackAuthority,
        instance: PluginInstanceId,
        entry: PluginCallbackEntryGuard,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        let jobs = Arc::clone(&self.jobs);
        Box::pin(async move {
            entry.validate_for(&authority, instance)?;
            reserve(&jobs)?;
            let slot = JobSlot(jobs);
            // Blocking author code runs on the host's blocking pool through
            // plug-rt, never on a thread created per callback.
            let job = crate::rt::spawn_blocking_named("native-plugin-callback", move || {
                let _slot = slot;
                let result = entry.validate_for(&authority, instance).and_then(|()| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback))
                        .unwrap_or_else(|_| Err(failure(PANICKED)))
                });
                drop(entry);
                result
            });
            job.await
                .map_err(|error| PluginCommandError::failed(error.to_string()))?
        })
    }
}
impl Executor {
    pub(super) fn project(
        self: &Arc<Self>,
        ports: &Arc<InvocationPorts>,
        cancellation: ServiceCancellation,
    ) -> PluginResult<(PluginCallbackContext, PluginHostServices)> {
        reserve(&self.owner_slots)?;
        let table = match ports.open_callback_authority() {
            Ok(table) => table,
            Err(status) => {
                if status == INVALID_ARGUMENT {
                    self.open_failures
                        .lock()
                        .map_err(|_| failure(PANICKED))?
                        .push((Arc::clone(ports), status));
                } else {
                    self.owner_slots.fetch_sub(1, Ordering::AcqRel);
                }
                return Err(failure(status));
            }
        };
        let owner = Arc::new(Authority {
            table,
            cancellation,
            entry: Mutex::new(None),
            subscriptions: Mutex::new(Vec::new()),
            uncertain: AtomicBool::new(false),
        });
        self.owners
            .lock()
            .map_err(|_| failure(PANICKED))?
            .push(Arc::clone(&owner));
        let authority = PluginCallbackAuthority::from_host(Arc::new(Arc::clone(&owner)));
        Ok((
            PluginCallbackContext::from_host(
                authority,
                PluginCallbackExecutor::from_host(self.clone()),
            ),
            services(&owner),
        ))
    }
}
fn services(owner: &Arc<Authority>) -> PluginHostServices {
    let invoke = Arc::clone(owner);
    let kernel = Arc::clone(owner);
    let registration = Arc::clone(owner);
    let retained = Arc::clone(owner);
    let retained_v1 = Arc::clone(owner);
    let availability = Arc::clone(owner);
    PluginHostServices::new(move |request| invoke.ports()?.host_port().invoke(request))
        .with_kernel(move |request| kernel.ports()?.host_port().execute_kernel_encoded(request))
        .with_registrations(move |mutation| {
            registration.ports()?.host_port().registration(mutation)
        })
        .with_retained_host(move || retained_v1.ports()?.open_retained_host())
        .with_retained_host_v2(move || retained.ports()?.open_retained_host_v2())
        .with_service_availability(move |key| {
            let owner = Arc::clone(&availability);
            Box::pin(async move {
                owner
                    .ports()?
                    .host_port()
                    .selected_service_availability(key)
                    .await
            })
        })
}
