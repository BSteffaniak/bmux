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
    RT_INVALID, RT_PANICKED, RT_PENDING, RT_READY, RtCx, RtJob, RtStatus, RtStr, RtTask, size_u32,
};
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
            },
            label: label.into(),
            runtime,
            matched: AtomicBool::new(false),
            scopes: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            timers: Mutex::new(HashMap::new()),
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

    /// Stop resolving `handle`; later spawns from it are refused as closed.
    pub fn unbind_scope(&self, handle: u64) {
        lock(&self.scopes).remove(&handle);
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
