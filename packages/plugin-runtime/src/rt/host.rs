//! Host implementation of [`PlugRtV1`]: one table per loaded library, served
//! by the host's single Tokio runtime.
//!
//! Plugin tasks become ordinary host tasks that poll the plugin future through
//! its `poll` function with the host task's real waker; nothing sleeps or
//! repolls. Work is registered with the activation scope the plugin instance
//! was bound to ([`PlugRtHost::bind_scope`]), so host teardown cancels, drains,
//! aborts and reports it like bundled work.
use super::abi::{
    BUILD_FINGERPRINT, PLUG_RT_ABI_VERSION, PlugRtGuestV1, PlugRtInstallV1, PlugRtV1, RT_CLOSED,
    RT_INVALID, RT_PANICKED, RT_PENDING, RT_READY, RT_TOO_SMALL, RT_UNSUPPORTED, RtCx, RtJob,
    RtStatus, RtStr, RtTask, size_u32,
};
use super::events::{EventPortError, EventStream, ScopeEvents};
use super::services::PortServices;
use super::waker;
use crate::background::{BackgroundTasks, Ticket};
use std::{
    collections::HashMap,
    ffi::c_void,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

/// Polls slower than this are reported: they stall a shared runtime worker.
const SLOW_POLL: Duration = Duration::from_millis(50);

fn epoch() -> tokio::time::Instant {
    static EPOCH: OnceLock<tokio::time::Instant> = OnceLock::new();
    *EPOCH.get_or_init(tokio::time::Instant::now)
}
fn deadline(nanos: u64) -> tokio::time::Instant {
    epoch() + Duration::from_nanos(nanos)
}

type Timer = Arc<Mutex<Pin<Box<tokio::time::Sleep>>>>;

/// One asynchronous service call: its outcome once ready, or the waker of
/// the plugin waiting for it.
struct CallSlot {
    scope: u64,
    outcome: Option<Vec<u8>>,
    waiter: Option<std::task::Waker>,
    abort: Option<tokio::task::AbortHandle>,
}

/// One plugin subscription: the host stream plus an item the plugin's
/// buffer could not hold yet.
struct Subscription {
    scope: u64,
    stream: Box<dyn EventStream>,
    pending: Option<Vec<u8>>,
}

/// Host state behind one library's table. Leaked on purpose: the library
/// holding pointers to it is never unloaded.
pub struct PlugRtHost {
    table: PlugRtV1,
    label: String,
    runtime: tokio::runtime::Handle,
    matched: AtomicBool,
    scopes: Mutex<HashMap<u64, BackgroundTasks>>,
    tasks: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
    timers: Mutex<HashMap<u64, Timer>>,
    events: Mutex<HashMap<u64, Arc<dyn ScopeEvents>>>,
    subscriptions: Mutex<HashMap<u64, Arc<Mutex<Subscription>>>>,
    port_services: OnceLock<Arc<dyn PortServices>>,
    calls: Mutex<HashMap<u64, Arc<Mutex<CallSlot>>>>,
    next: AtomicU64,
}
impl std::fmt::Debug for PlugRtHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlugRtHost")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PlugRtHost {
    /// Create the table for one library, served by `runtime`.
    #[must_use]
    pub fn new(runtime: tokio::runtime::Handle, label: impl Into<String>) -> &'static Self {
        let _ = epoch();
        let host: &'static mut Self = Box::leak(Box::new(Self {
            table: PlugRtV1 {
                size: size_u32::<PlugRtV1>(),
                abi_version: PLUG_RT_ABI_VERSION,
                build_fingerprint: BUILD_FINGERPRINT,
                host: std::ptr::null(),
                spawn: host_spawn,
                task_abort: host_task_abort,
                spawn_blocking: host_spawn_blocking,
                now_ns: host_now_ns,
                timer_new: host_timer_new,
                timer_reset: host_timer_reset,
                timer_poll: host_timer_poll,
                timer_drop: host_timer_drop,
                event_register: host_event_register,
                event_publish: host_event_publish,
                event_subscribe: host_event_subscribe,
                event_poll: host_event_poll,
                event_unsubscribe: host_event_unsubscribe,
                service_call: host_service_call,
                service_poll: host_service_poll,
                service_cancel: host_service_cancel,
            },
            label: label.into(),
            runtime,
            matched: AtomicBool::new(false),
            scopes: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            timers: Mutex::new(HashMap::new()),
            events: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            port_services: OnceLock::new(),
            calls: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }));
        host.table.host = std::ptr::from_ref::<Self>(host).cast();
        host
    }

    /// The table handed to the plugin; valid for the life of the process.
    #[must_use]
    pub const fn table(&'static self) -> *const PlugRtV1 {
        &raw const self.table
    }

    /// Whether the installed plugin shares this host's build fingerprint.
    #[must_use]
    pub fn fingerprint_matches(&self) -> bool {
        self.matched.load(Ordering::Acquire)
    }

    /// Install this table through the library's exported install function.
    ///
    /// # Safety
    /// `install` must be the library's `bmux_plug_rt_install_v1` export and the
    /// library must never be unloaded afterwards.
    ///
    /// # Errors
    /// The library was not built in dynamic mode, uses another plug-rt ABI, or
    /// was built with `panic = "abort"`.
    pub unsafe fn install(&'static self, install: PlugRtInstallV1) -> Result<(), String> {
        let mut guest = PlugRtGuestV1 {
            size: 0,
            abi_version: 0,
            build_fingerprint: [0; 32],
            panic_unwind: 0,
        };
        let status = unsafe { install(self.table(), &raw mut guest) };
        if guest.size < size_u32::<PlugRtGuestV1>() || guest.abi_version != PLUG_RT_ABI_VERSION {
            return Err(format!(
                "plugin '{}' speaks plug-rt ABI {} (host {PLUG_RT_ABI_VERSION})",
                self.label, guest.abi_version
            ));
        }
        if guest.panic_unwind != 1 {
            return Err(format!(
                "plugin '{}' was built with panic=abort; plug-rt requires panic=unwind",
                self.label
            ));
        }
        match status {
            RT_READY => {}
            super::abi::RT_UNSUPPORTED => {
                return Err(format!(
                    "plugin '{}' was not built in dynamic mode (--cfg bmux_plugin_dylib); rebuild it with the plugin packager",
                    self.label
                ));
            }
            other => {
                return Err(format!(
                    "plugin '{}' refused the plug-rt host table (status {other})",
                    self.label
                ));
            }
        }
        self.matched.store(
            guest.build_fingerprint == BUILD_FINGERPRINT,
            Ordering::Release,
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn assume_fingerprint_match(&self, matched: bool) {
        self.matched.store(matched, Ordering::Release);
    }

    /// Own work spawned by plugin instance `handle` with `tasks`.
    pub fn bind_scope(&self, handle: u64, tasks: BackgroundTasks) {
        lock(&self.scopes).insert(handle, tasks);
    }

    /// Resolve this library's invocation ports for asynchronous host service
    /// calls. Set once, when the library is loaded.
    pub fn set_port_services(&self, services: Arc<dyn PortServices>) {
        let _ = self.port_services.set(services);
    }

    /// Asynchronous host service calls still owned by plugin scopes.
    #[must_use]
    pub fn service_call_count(&self) -> usize {
        lock(&self.calls).len()
    }

    /// Serve the serialized event port of plugin instance `handle`.
    pub fn bind_scope_events(&self, handle: u64, events: Arc<dyn ScopeEvents>) {
        lock(&self.events).insert(handle, events);
    }

    /// Stop resolving `handle`; later spawns from it are refused as closed,
    /// and its event port and every subscription it holds are released.
    pub fn unbind_scope(&self, handle: u64) {
        lock(&self.scopes).remove(&handle);
        lock(&self.events).remove(&handle);
        let released: Vec<_> = {
            let mut subscriptions = lock(&self.subscriptions);
            let ids: Vec<u64> = subscriptions
                .iter()
                .filter(|(_, subscription)| lock(subscription).scope == handle)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| subscriptions.remove(&id))
                .collect()
        };
        // Host streams drop outside the registry lock.
        drop(released);
        let calls: Vec<_> = {
            let mut calls = lock(&self.calls);
            let ids: Vec<u64> = calls
                .iter()
                .filter(|(_, slot)| lock(slot).scope == handle)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter().filter_map(|id| calls.remove(&id)).collect()
        };
        for call in calls {
            if let Some(abort) = lock(&call).abort.take() {
                abort.abort();
            }
        }
    }

    /// Live plugin event subscriptions (diagnostics and teardown checks).
    #[must_use]
    pub fn event_subscription_count(&self) -> usize {
        lock(&self.subscriptions).len()
    }

    fn scope_events(&self, scope: u64) -> Result<Arc<dyn ScopeEvents>, RtStatus> {
        lock(&self.events)
            .get(&scope)
            .cloned()
            .ok_or(RT_UNSUPPORTED)
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    fn admit(
        &self,
        scope: u64,
        name: &str,
        blocking: bool,
    ) -> Result<Option<(BackgroundTasks, Ticket)>, RtStatus> {
        if scope == 0 {
            // Spawned outside any instance poll: owned by the runtime alone.
            return Ok(None);
        }
        let tasks = lock(&self.scopes).get(&scope).cloned().ok_or(RT_CLOSED)?;
        let ticket = if blocking {
            tasks.admit_blocking(name)
        } else {
            tasks.admit_task(name)
        }
        .map_err(|_| RT_CLOSED)?;
        Ok(Some((tasks, ticket)))
    }

    fn spawn(&'static self, scope: u64, task: TaskOwner, name: &str, out: *mut u64) -> RtStatus {
        let admitted = match self.admit(scope, name, false) {
            Ok(admitted) => admitted,
            Err(status) => return status, // `task` drops through the plugin.
        };
        let id = self.id();
        let (tasks, ticket) =
            admitted.map_or((None, None), |(tasks, ticket)| (Some(tasks), Some(ticket)));
        let ticket_id = ticket.as_ref().map(Ticket::id);
        let handle = self.runtime.spawn(HostTask {
            task,
            host: self,
            id,
            name: name.to_string(),
            _ticket: ticket,
        });
        let abort = handle.abort_handle();
        lock(&self.tasks).insert(id, abort.clone());
        if handle.is_finished() {
            lock(&self.tasks).remove(&id);
        }
        if let (Some(tasks), Some(ticket_id)) = (tasks, ticket_id) {
            tasks.bind_abort(ticket_id, Box::new(move || abort.abort()));
        }
        if !out.is_null() {
            unsafe { out.write(id) };
        }
        RT_READY
    }

    fn spawn_blocking(
        &'static self,
        scope: u64,
        job: JobOwner,
        name: &str,
        out: *mut u64,
    ) -> RtStatus {
        let ticket = match self.admit(scope, name, true) {
            Ok(admitted) => admitted.map(|(_, ticket)| ticket),
            Err(status) => return status,
        };
        let label = self.label.clone();
        let name = name.to_string();
        drop(self.runtime.spawn_blocking(move || {
            let _ticket = ticket;
            if job.run() == RT_PANICKED {
                tracing::warn!(plugin = %label, job = %name, "plugin blocking job panicked");
            }
        }));
        if !out.is_null() {
            // Blocking jobs have no abort id.
            unsafe { out.write(0) };
        }
        RT_READY
    }
}

/// Owns one plugin future; dropping it frees the future through the plugin.
struct TaskOwner(RtTask);
impl Drop for TaskOwner {
    fn drop(&mut self) {
        unsafe { (self.0.drop)(self.0.data) };
    }
}

/// Owns one plugin blocking job; dropping it frees the job through the plugin.
struct JobOwner(RtJob);
impl JobOwner {
    fn run(&self) -> RtStatus {
        unsafe { (self.0.run)(self.0.data) }
    }
}
impl Drop for JobOwner {
    fn drop(&mut self) {
        unsafe { (self.0.drop)(self.0.data) };
    }
}

struct HostTask {
    // Field order is drop order: the plugin future is freed first, then the
    // scope registration settles.
    task: TaskOwner,
    host: &'static PlugRtHost,
    id: u64,
    name: String,
    _ticket: Option<Ticket>,
}
impl Future for HostTask {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let started = std::time::Instant::now();
        let matched = this.host.fingerprint_matches();
        let task = &this.task.0;
        let status = waker::with_cx(cx.waker(), matched, |rtcx| unsafe {
            (task.poll)(task.data, rtcx)
        });
        let elapsed = started.elapsed();
        if elapsed >= SLOW_POLL {
            tracing::warn!(
                plugin = %this.host.label,
                task = %this.name,
                micros = elapsed.as_micros(),
                "slow plugin task poll"
            );
        }
        match status {
            RT_PENDING => Poll::Pending,
            RT_READY => Poll::Ready(()),
            RT_PANICKED => {
                tracing::warn!(plugin = %this.host.label, task = %this.name, "plugin task panicked");
                Poll::Ready(())
            }
            other => {
                tracing::warn!(plugin = %this.host.label, task = %this.name, status = other, "invalid plugin task poll status");
                Poll::Ready(())
            }
        }
    }
}
impl Drop for HostTask {
    fn drop(&mut self) {
        lock(&self.host.tasks).remove(&self.id);
    }
}

unsafe fn host_ref(host: *const c_void) -> &'static PlugRtHost {
    // SAFETY: `host` is the leaked `PlugRtHost` this table was built from.
    unsafe { &*host.cast::<PlugRtHost>() }
}

unsafe extern "C" fn host_spawn(
    host: *const c_void,
    scope: u64,
    task: RtTask,
    name: RtStr,
    out: *mut u64,
) -> RtStatus {
    let owner = TaskOwner(task);
    catch_unwind(AssertUnwindSafe(move || {
        let host = unsafe { host_ref(host) };
        host.spawn(scope, owner, unsafe { name.as_str() }, out)
    }))
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_task_abort(host: *const c_void, id: u64) {
    let _ = catch_unwind(|| {
        let host = unsafe { host_ref(host) };
        let abort = lock(&host.tasks).get(&id).cloned();
        if let Some(abort) = abort {
            abort.abort();
        }
    });
}

unsafe extern "C" fn host_spawn_blocking(
    host: *const c_void,
    scope: u64,
    job: RtJob,
    name: RtStr,
    out: *mut u64,
) -> RtStatus {
    let owner = JobOwner(job);
    catch_unwind(AssertUnwindSafe(move || {
        let host = unsafe { host_ref(host) };
        host.spawn_blocking(scope, owner, unsafe { name.as_str() }, out)
    }))
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_now_ns(_: *const c_void) -> u64 {
    let elapsed = tokio::time::Instant::now().saturating_duration_since(epoch());
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

unsafe extern "C" fn host_timer_new(host: *const c_void, nanos: u64, out: *mut u64) -> RtStatus {
    catch_unwind(|| {
        if out.is_null() {
            return RT_INVALID;
        }
        let host = unsafe { host_ref(host) };
        let _runtime = host.runtime.enter();
        let timer = Arc::new(Mutex::new(Box::pin(tokio::time::sleep_until(deadline(
            nanos,
        )))));
        let id = host.id();
        lock(&host.timers).insert(id, timer);
        unsafe { out.write(id) };
        RT_READY
    })
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_timer_reset(host: *const c_void, id: u64, nanos: u64) -> RtStatus {
    catch_unwind(|| {
        let host = unsafe { host_ref(host) };
        let Some(timer) = lock(&host.timers).get(&id).cloned() else {
            return RT_INVALID;
        };
        lock(&timer).as_mut().reset(deadline(nanos));
        RT_READY
    })
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_timer_poll(host: *const c_void, id: u64, cx: *const RtCx) -> RtStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if cx.is_null() {
            return RT_INVALID;
        }
        let host = unsafe { host_ref(host) };
        let Some(timer) = lock(&host.timers).get(&id).cloned() else {
            return RT_INVALID;
        };
        let mut timer = lock(&timer);
        let polled = unsafe {
            waker::with_waker(cx, |waker| {
                timer.as_mut().poll(&mut Context::from_waker(waker))
            })
        };
        if polled.is_ready() {
            RT_READY
        } else {
            RT_PENDING
        }
    }))
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_timer_drop(host: *const c_void, id: u64) {
    let _ = catch_unwind(|| {
        let host = unsafe { host_ref(host) };
        let timer = lock(&host.timers).remove(&id);
        drop(timer);
    });
}

fn event_status(result: Result<(), EventPortError>) -> RtStatus {
    match result {
        Ok(()) => RT_READY,
        Err(EventPortError::Closed) => RT_CLOSED,
        Err(EventPortError::Unsupported) => RT_UNSUPPORTED,
        Err(EventPortError::Rejected(reason)) => {
            tracing::debug!(%reason, "plugin event port operation rejected");
            RT_INVALID
        }
    }
}

unsafe extern "C" fn host_event_register(host: *const c_void, scope: u64, kind: RtStr) -> RtStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let host = unsafe { host_ref(host) };
        match host.scope_events(scope) {
            Ok(events) => event_status(events.register(unsafe { kind.as_str() })),
            Err(status) => status,
        }
    }))
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_event_publish(
    host: *const c_void,
    scope: u64,
    kind: RtStr,
    payload: *const u8,
    length: usize,
) -> RtStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if payload.is_null() && length != 0 {
            return RT_INVALID;
        }
        let host = unsafe { host_ref(host) };
        let payload = if length == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(payload, length) }
        };
        match host.scope_events(scope) {
            Ok(events) => event_status(events.publish(unsafe { kind.as_str() }, payload)),
            Err(status) => status,
        }
    }))
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_event_subscribe(
    host: *const c_void,
    scope: u64,
    kind: RtStr,
    state: u32,
    out: *mut u64,
) -> RtStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if out.is_null() {
            return RT_INVALID;
        }
        let host = unsafe { host_ref(host) };
        let events = match host.scope_events(scope) {
            Ok(events) => events,
            Err(status) => return status,
        };
        match events.subscribe(unsafe { kind.as_str() }, state == 1) {
            Ok(stream) => {
                let id = host.id();
                lock(&host.subscriptions).insert(
                    id,
                    Arc::new(Mutex::new(Subscription {
                        scope,
                        stream,
                        pending: None,
                    })),
                );
                unsafe { out.write(id) };
                RT_READY
            }
            Err(error) => event_status(Err(error)),
        }
    }))
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_event_poll(
    host: *const c_void,
    id: u64,
    cx: *const RtCx,
    buffer: *mut u8,
    capacity: usize,
    length: *mut usize,
) -> RtStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if cx.is_null() || length.is_null() || (capacity > 0 && buffer.is_null()) {
            return RT_INVALID;
        }
        let host = unsafe { host_ref(host) };
        // A released subscription (plugin drop or scope teardown) has ended.
        let Some(subscription) = lock(&host.subscriptions).get(&id).cloned() else {
            return RT_CLOSED;
        };
        let mut subscription = lock(&subscription);
        let item = match subscription.pending.take() {
            Some(item) => item,
            None => {
                let polled = unsafe {
                    waker::with_waker(cx, |waker| {
                        subscription
                            .stream
                            .poll_next(&mut Context::from_waker(waker))
                    })
                };
                match polled {
                    Poll::Pending => return RT_PENDING,
                    Poll::Ready(None) => return RT_CLOSED,
                    Poll::Ready(Some(item)) => item,
                }
            }
        };
        unsafe { length.write(item.len()) };
        if item.len() > capacity {
            subscription.pending = Some(item);
            return RT_TOO_SMALL;
        }
        if !item.is_empty() {
            unsafe { std::ptr::copy_nonoverlapping(item.as_ptr(), buffer, item.len()) };
        }
        RT_READY
    }))
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_event_unsubscribe(host: *const c_void, id: u64) {
    let _ = catch_unwind(|| {
        let host = unsafe { host_ref(host) };
        let released = lock(&host.subscriptions).remove(&id);
        drop(released);
    });
}

unsafe extern "C" fn host_service_call(
    host: *const c_void,
    scope: u64,
    port: u64,
    request: *const u8,
    length: usize,
    out: *mut u64,
) -> RtStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if out.is_null() || (request.is_null() && length != 0) {
            return RT_INVALID;
        }
        let host = unsafe { host_ref(host) };
        let Some(services) = host.port_services.get().cloned() else {
            return RT_UNSUPPORTED;
        };
        let request = if length == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(request, length) }.to_vec()
        };
        // The call is work of the plugin scope: refused once it closes, and
        // aborted with the scope's other raw tasks at teardown.
        let (tasks, ticket) = match host.admit(scope, "host-service-call", false) {
            Ok(Some((tasks, ticket))) => (Some(tasks), Some(ticket)),
            Ok(None) => (None, None),
            Err(status) => return status,
        };
        let id = host.id();
        let slot = Arc::new(Mutex::new(CallSlot {
            scope,
            outcome: None,
            waiter: None,
            abort: None,
        }));
        let outcome = services.call(port, request);
        let completed = Arc::clone(&slot);
        let ticket_id = ticket.as_ref().map(Ticket::id);
        let handle = host.runtime.spawn(async move {
            let _ticket = ticket;
            let bytes = outcome.await;
            let waiter = {
                let mut slot = lock(&completed);
                slot.outcome = Some(bytes);
                slot.waiter.take()
            };
            if let Some(waiter) = waiter {
                waiter.wake();
            }
        });
        let abort = handle.abort_handle();
        lock(&slot).abort = Some(abort.clone());
        if let (Some(tasks), Some(ticket_id)) = (tasks, ticket_id) {
            tasks.bind_abort(ticket_id, Box::new(move || abort.abort()));
        }
        lock(&host.calls).insert(id, slot);
        unsafe { out.write(id) };
        RT_READY
    }))
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_service_poll(
    host: *const c_void,
    id: u64,
    cx: *const RtCx,
    buffer: *mut u8,
    capacity: usize,
    length: *mut usize,
) -> RtStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if cx.is_null() || length.is_null() || (capacity > 0 && buffer.is_null()) {
            return RT_INVALID;
        }
        let host = unsafe { host_ref(host) };
        let Some(slot) = lock(&host.calls).get(&id).cloned() else {
            return RT_CLOSED;
        };
        let mut guard = lock(&slot);
        let Some(outcome) = guard.outcome.as_ref() else {
            // Registered under the slot lock the completing task takes, so a
            // completion cannot slip between this check and the wait.
            let waiter = unsafe { waker::with_waker(cx, Clone::clone) };
            guard.waiter = Some(waiter);
            return RT_PENDING;
        };
        unsafe { length.write(outcome.len()) };
        if outcome.len() > capacity {
            return RT_TOO_SMALL;
        }
        if !outcome.is_empty() {
            unsafe { std::ptr::copy_nonoverlapping(outcome.as_ptr(), buffer, outcome.len()) };
        }
        drop(guard);
        lock(&host.calls).remove(&id);
        RT_READY
    }))
    .unwrap_or(RT_PANICKED)
}

unsafe extern "C" fn host_service_cancel(host: *const c_void, id: u64) {
    let _ = catch_unwind(|| {
        let host = unsafe { host_ref(host) };
        let released = lock(&host.calls).remove(&id);
        if let Some(slot) = released
            && let Some(abort) = lock(&slot).abort.take()
        {
            abort.abort();
        }
    });
}
