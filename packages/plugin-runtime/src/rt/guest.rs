//! Plugin-side client of an installed [`PlugRtV1`] table.
//!
//! Dynamic builds (`--cfg bmux_plugin_dylib`) route every plug-rt call through
//! the table the host installed into this library's copy of the crate. The
//! client is always compiled so hosts can exercise the exact boundary
//! in-process; only dynamic builds select it for [`super::spawn`] and
//! [`super::time`].
use super::abi::{
    BUILD_FINGERPRINT, PLUG_RT_ABI_VERSION, PlugRtGuestV1, PlugRtV1, RT_CLOSED, RT_INCOMPATIBLE,
    RT_INVALID, RT_PANICKED, RT_PENDING, RT_READY, RT_UNSUPPORTED, RtCx, RtJob, RtStatus, RtStr,
    RtTask, size_u32,
};
use super::scope::{self, Current};
use super::task::{JoinError, JoinHandle, Slot, panic_message};
use super::waker;
use std::{
    ffi::c_void,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

#[cfg_attr(not(bmux_plugin_dylib), allow(dead_code))]
pub(crate) const NOT_INSTALLED: &str =
    "plug-rt host table not installed: this dylib was loaded without plug-rt";

static INSTALLED: OnceLock<Client> = OnceLock::new();

/// The client installed into this image, if any.
#[cfg_attr(not(bmux_plugin_dylib), allow(dead_code))]
pub(crate) fn installed() -> Option<&'static Client> {
    INSTALLED.get()
}

#[cfg_attr(not(bmux_plugin_dylib), allow(dead_code))]
fn client() -> &'static Client {
    // The host installs the table before any instance exists; reaching this
    // without one is a host contract violation, never a recoverable state.
    installed().unwrap_or_else(|| panic!("{NOT_INSTALLED}"))
}

/// Install the host's table into this image. Called only through the exported
/// `bmux_plug_rt_install_v1` symbol of a plugin wrapper.
///
/// Writes this image's identity to `guest` first, so the host can explain a
/// refusal. Returns [`RT_UNSUPPORTED`] unless this image was built in dynamic
/// mode, and [`RT_INCOMPATIBLE`] for an ABI or panic-strategy mismatch.
///
/// # Safety
/// `table` must be null or point to a table valid for the life of the process;
/// `guest` must be null or writable.
pub unsafe fn install(table: *const PlugRtV1, guest: *mut PlugRtGuestV1) -> RtStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if !guest.is_null() && guest.is_aligned() {
            unsafe {
                guest.write(PlugRtGuestV1 {
                    size: size_u32::<PlugRtGuestV1>(),
                    abi_version: PLUG_RT_ABI_VERSION,
                    build_fingerprint: BUILD_FINGERPRINT,
                    panic_unwind: u32::from(cfg!(panic = "unwind")),
                });
            }
        }
        if !cfg!(bmux_plugin_dylib) {
            return RT_UNSUPPORTED;
        }
        if !cfg!(panic = "unwind") {
            return RT_INCOMPATIBLE;
        }
        let client = match unsafe { Client::adopt(table) } {
            Ok(client) => client,
            Err(status) => return status,
        };
        let installed = INSTALLED.get_or_init(|| client.clone());
        if std::ptr::eq(installed.table, client.table) {
            RT_READY
        } else {
            RT_INCOMPATIBLE
        }
    }))
    .unwrap_or(RT_PANICKED)
}

/// A validated view of one host table.
#[derive(Clone)]
pub struct Client {
    table: &'static PlugRtV1,
    matched: bool,
}
impl Client {
    /// Validate a host table.
    ///
    /// # Safety
    /// `table` must be null or point to a table valid for the life of the process.
    ///
    /// # Errors
    /// Null or misaligned tables, and size or version mismatches.
    pub unsafe fn adopt(table: *const PlugRtV1) -> Result<Self, RtStatus> {
        if table.is_null() || !table.is_aligned() {
            return Err(RT_INVALID);
        }
        let size = unsafe { std::ptr::addr_of!((*table).size).read() };
        let version = unsafe { std::ptr::addr_of!((*table).abi_version).read() };
        if version != PLUG_RT_ABI_VERSION || size < size_u32::<PlugRtV1>() {
            return Err(RT_INCOMPATIBLE);
        }
        let table = unsafe { &*table };
        Ok(Self {
            table,
            matched: table.build_fingerprint == BUILD_FINGERPRINT,
        })
    }

    /// Whether wakers pass between the images directly (same build fingerprint).
    #[must_use]
    pub const fn fingerprint_matches(&self) -> bool {
        self.matched
    }

    /// Force the `extern "C"` waker shim even when fingerprints match.
    #[must_use]
    pub const fn with_waker_shim(mut self) -> Self {
        self.matched = false;
        self
    }

    /// Spawn `future` on the host runtime, owned by host scope `scope`.
    pub fn spawn<F>(&'static self, scope: u64, name: &str, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let slot = Arc::new(Slot::new());
        let data = Box::into_raw(Box::new(GuestTask {
            future: Some(future),
            scope,
            slot: Arc::clone(&slot),
        }));
        let task = RtTask {
            data: data.cast(),
            poll: poll_task::<F>,
            drop: drop_task::<F>,
        };
        let mut id = 0;
        // Ownership transfers even on refusal (the host drops it first).
        let status = unsafe {
            (self.table.spawn)(self.table.host, scope, task, RtStr::new(name), &raw mut id)
        };
        if status != RT_READY {
            slot.overwrite(Err(refused(status)));
            return JoinHandle::remote(slot, None);
        }
        let table = self.table;
        JoinHandle::remote(
            slot,
            Some(Box::new(move || unsafe {
                (table.task_abort)(table.host, id)
            })),
        )
    }

    /// Run `work` on the host's blocking pool, owned by host scope `scope`.
    pub fn spawn_blocking<W, T>(&'static self, scope: u64, name: &str, work: W) -> JoinHandle<T>
    where
        W: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_blocking_in(scope, scope, name, work)
    }

    /// Run blocking work that a host-admitted operation awaits. The operation
    /// owns it, not the activation scope, so it is not refused while that
    /// scope is closing for teardown (the host's own deactivation must still
    /// run). The job still enters instance `scope`, so work it spawns joins
    /// the activation as usual.
    pub fn spawn_blocking_for_operation<W, T>(
        &'static self,
        scope: u64,
        name: &str,
        work: W,
    ) -> JoinHandle<T>
    where
        W: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_blocking_in(0, scope, name, work)
    }

    fn spawn_blocking_in<W, T>(
        &'static self,
        owner: u64,
        scope: u64,
        name: &str,
        work: W,
    ) -> JoinHandle<T>
    where
        W: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let slot = Arc::new(Slot::new());
        let data = Box::into_raw(Box::new(GuestJob {
            work: Some(work),
            scope,
            slot: Arc::clone(&slot),
        }));
        let job = RtJob {
            data: data.cast(),
            run: run_job::<W, T>,
            drop: drop_job::<W, T>,
        };
        let mut id = 0;
        let status = unsafe {
            (self.table.spawn_blocking)(self.table.host, owner, job, RtStr::new(name), &raw mut id)
        };
        if status != RT_READY {
            slot.overwrite(Err(refused(status)));
        }
        // A started blocking job cannot be aborted.
        JoinHandle::remote(slot, None)
    }

    /// The serialized event port of instance scope `scope`.
    #[must_use]
    pub fn events(&'static self, scope: u64) -> super::events::RemoteEvents {
        super::events::RemoteEvents::new(self.table, scope, self.matched)
    }

    /// The host clock.
    #[must_use]
    pub fn now(&'static self) -> time::Instant {
        time::Instant(unsafe { (self.table.now_ns)(self.table.host) })
    }

    /// A host timer firing at `deadline`.
    #[must_use]
    pub fn sleep_until(&'static self, deadline: time::Instant) -> time::Sleep {
        time::Sleep::new(self, deadline)
    }
}

fn refused(status: RtStatus) -> JoinError {
    if status == RT_CLOSED {
        JoinError::closed()
    } else {
        JoinError::unavailable(format!("host refused plug-rt work (status {status})"))
    }
}

struct GuestTask<F: Future> {
    future: Option<F>,
    scope: u64,
    slot: Arc<Slot<F::Output>>,
}

fn contained_drop<T>(value: T) {
    // A panicking destructor must not unwind across the boundary.
    let _ = catch_unwind(AssertUnwindSafe(move || drop(value)));
}

unsafe extern "C" fn poll_task<F>(data: *mut c_void, cx: *const RtCx) -> RtStatus
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // SAFETY: `data` is the boxed task created by `spawn`, never moved and
    // polled by one host task at a time.
    let task = unsafe { &mut *data.cast::<GuestTask<F>>() };
    let polled = catch_unwind(AssertUnwindSafe(|| {
        let Some(future) = task.future.as_mut() else {
            return Poll::Ready(None);
        };
        let _scope = scope::enter(Current::Remote(task.scope));
        unsafe {
            waker::with_waker(cx, |waker| {
                // SAFETY: the future lives inside the box and is never moved.
                Pin::new_unchecked(future).poll(&mut Context::from_waker(waker))
            })
        }
        .map(Some)
    }));
    match polled {
        Ok(Poll::Pending) => RT_PENDING,
        Ok(Poll::Ready(Some(value))) => {
            contained_drop(task.future.take());
            task.slot.complete(Ok(value));
            RT_READY
        }
        Ok(Poll::Ready(None)) => RT_READY,
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            contained_drop(payload);
            contained_drop(task.future.take());
            task.slot.complete(Err(JoinError::panicked(message)));
            RT_PANICKED
        }
    }
}

unsafe extern "C" fn drop_task<F>(data: *mut c_void)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the host calls drop exactly once with the box from `spawn`.
        let mut task = unsafe { Box::from_raw(data.cast::<GuestTask<F>>()) };
        let future = task.future.take();
        task.slot.complete(Err(JoinError::cancelled()));
        contained_drop(future);
    }));
}

struct GuestJob<W, T> {
    work: Option<W>,
    scope: u64,
    slot: Arc<Slot<T>>,
}

unsafe extern "C" fn run_job<W, T>(data: *mut c_void) -> RtStatus
where
    W: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // SAFETY: `data` is the boxed job created by `spawn_blocking`.
    let job = unsafe { &mut *data.cast::<GuestJob<W, T>>() };
    let Some(work) = job.work.take() else {
        return RT_READY;
    };
    let scope = job.scope;
    match catch_unwind(AssertUnwindSafe(move || {
        let _scope = scope::enter(Current::Remote(scope));
        work()
    })) {
        Ok(value) => {
            job.slot.complete(Ok(value));
            RT_READY
        }
        Err(payload) => {
            job.slot
                .complete(Err(JoinError::panicked(panic_message(payload.as_ref()))));
            contained_drop(payload);
            RT_PANICKED
        }
    }
}

unsafe extern "C" fn drop_job<W, T>(data: *mut c_void)
where
    W: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let mut job = unsafe { Box::from_raw(data.cast::<GuestJob<W, T>>()) };
        let work = job.work.take();
        job.slot.complete(Err(JoinError::cancelled()));
        contained_drop(work);
    }));
}

/// Host timers seen from the plugin side.
pub mod time {
    use super::{Client, RT_PENDING, RT_READY, waker};
    use std::{
        fmt,
        future::{Future, IntoFuture},
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };

    /// A point on the host's monotonic runtime clock.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(pub(super) u64);
    impl Instant {
        #[must_use]
        pub fn now() -> Self {
            super::client().now()
        }
        #[must_use]
        pub fn elapsed(&self) -> Duration {
            Self::now().saturating_duration_since(*self)
        }
        #[must_use]
        pub fn duration_since(&self, earlier: Self) -> Duration {
            self.saturating_duration_since(earlier)
        }
        #[must_use]
        pub fn checked_duration_since(&self, earlier: Self) -> Option<Duration> {
            self.0.checked_sub(earlier.0).map(Duration::from_nanos)
        }
        #[must_use]
        pub fn saturating_duration_since(&self, earlier: Self) -> Duration {
            Duration::from_nanos(self.0.saturating_sub(earlier.0))
        }
        #[must_use]
        pub fn checked_add(&self, duration: Duration) -> Option<Self> {
            u64::try_from(duration.as_nanos())
                .ok()
                .and_then(|nanos| self.0.checked_add(nanos))
                .map(Self)
        }
        #[must_use]
        pub fn checked_sub(&self, duration: Duration) -> Option<Self> {
            u64::try_from(duration.as_nanos())
                .ok()
                .and_then(|nanos| self.0.checked_sub(nanos))
                .map(Self)
        }
        const fn nanos(self) -> u64 {
            self.0
        }
    }
    impl std::ops::Add<Duration> for Instant {
        type Output = Self;
        fn add(self, duration: Duration) -> Self {
            self.checked_add(duration).unwrap_or(Self(u64::MAX))
        }
    }
    impl std::ops::AddAssign<Duration> for Instant {
        fn add_assign(&mut self, duration: Duration) {
            *self = *self + duration;
        }
    }
    impl std::ops::Sub<Duration> for Instant {
        type Output = Self;
        fn sub(self, duration: Duration) -> Self {
            self.checked_sub(duration).unwrap_or(Self(0))
        }
    }
    impl std::ops::Sub for Instant {
        type Output = Duration;
        fn sub(self, earlier: Self) -> Duration {
            self.saturating_duration_since(earlier)
        }
    }

    /// A host timer. Dropping it releases the host timer.
    pub struct Sleep {
        client: &'static Client,
        id: u64,
        deadline: Instant,
    }
    impl fmt::Debug for Sleep {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("Sleep")
                .field("deadline", &self.deadline)
                .finish_non_exhaustive()
        }
    }
    impl Sleep {
        pub(super) fn new(client: &'static Client, deadline: Instant) -> Self {
            let mut id = 0;
            let status = unsafe {
                (client.table.timer_new)(client.table.host, deadline.nanos(), &raw mut id)
            };
            // Timer creation only allocates host state; a refusal means the
            // host table is broken, which no plugin can recover from.
            assert!(
                status == RT_READY,
                "host refused a plug-rt timer ({status})"
            );
            Self {
                client,
                id,
                deadline,
            }
        }
        #[must_use]
        pub const fn deadline(&self) -> Instant {
            self.deadline
        }
        #[must_use]
        pub fn is_elapsed(&self) -> bool {
            self.client.now() >= self.deadline
        }
        pub fn reset(self: Pin<&mut Self>, deadline: Instant) {
            let this = self.get_mut();
            let _ = unsafe {
                (this.client.table.timer_reset)(this.client.table.host, this.id, deadline.nanos())
            };
            this.deadline = deadline;
        }
    }
    impl Future for Sleep {
        type Output = ();
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let this = self.get_mut();
            let table = this.client.table;
            let status = waker::with_cx(cx.waker(), this.client.matched, |rtcx| unsafe {
                (table.timer_poll)(table.host, this.id, rtcx)
            });
            if status == RT_PENDING {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }
    impl Drop for Sleep {
        fn drop(&mut self) {
            unsafe { (self.client.table.timer_drop)(self.client.table.host, self.id) };
        }
    }

    #[must_use]
    pub fn sleep_until(deadline: Instant) -> Sleep {
        super::client().sleep_until(deadline)
    }
    #[must_use]
    pub fn sleep(duration: Duration) -> Sleep {
        sleep_until(Instant::now() + duration)
    }

    /// Error returned by [`Timeout`] when its deadline passes first.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Elapsed(());
    impl fmt::Display for Elapsed {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("deadline has elapsed")
        }
    }
    impl std::error::Error for Elapsed {}

    /// A future bounded by a host timer.
    #[derive(Debug)]
    pub struct Timeout<F> {
        future: F,
        sleep: Sleep,
    }
    impl<F> Timeout<F> {
        pub const fn get_ref(&self) -> &F {
            &self.future
        }
        pub const fn get_mut(&mut self) -> &mut F {
            &mut self.future
        }
        pub fn into_inner(self) -> F {
            self.future
        }
    }
    impl<F: Future> Future for Timeout<F> {
        type Output = Result<F::Output, Elapsed>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            // SAFETY: `future` is structurally pinned; `sleep` is `Unpin`.
            let this = unsafe { self.get_unchecked_mut() };
            if let Poll::Ready(value) = unsafe { Pin::new_unchecked(&mut this.future) }.poll(cx) {
                return Poll::Ready(Ok(value));
            }
            Pin::new(&mut this.sleep)
                .poll(cx)
                .map(|()| Err(Elapsed(())))
        }
    }
    pub fn timeout_at<F: IntoFuture>(deadline: Instant, future: F) -> Timeout<F::IntoFuture> {
        Timeout {
            future: future.into_future(),
            sleep: sleep_until(deadline),
        }
    }
    pub fn timeout<F: IntoFuture>(duration: Duration, future: F) -> Timeout<F::IntoFuture> {
        timeout_at(Instant::now() + duration, future)
    }

    /// Ticks at a fixed period; missed ticks fire immediately (burst).
    #[derive(Debug)]
    pub struct Interval {
        sleep: Sleep,
        period: Duration,
    }
    impl Interval {
        pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
            if Pin::new(&mut self.sleep).poll(cx).is_pending() {
                return Poll::Pending;
            }
            let tick = self.sleep.deadline();
            Pin::new(&mut self.sleep).reset(tick + self.period);
            Poll::Ready(tick)
        }
        pub async fn tick(&mut self) -> Instant {
            std::future::poll_fn(|cx| self.poll_tick(cx)).await
        }
        pub fn reset(&mut self) {
            let next = self.sleep.client.now() + self.period;
            Pin::new(&mut self.sleep).reset(next);
        }
        #[must_use]
        pub const fn period(&self) -> Duration {
            self.period
        }
    }
    /// # Panics
    /// When `period` is zero.
    #[must_use]
    pub fn interval_at(start: Instant, period: Duration) -> Interval {
        assert!(period > Duration::ZERO, "`period` must be non-zero.");
        Interval {
            sleep: sleep_until(start),
            period,
        }
    }
    /// # Panics
    /// When `period` is zero.
    #[must_use]
    pub fn interval(period: Duration) -> Interval {
        interval_at(Instant::now(), period)
    }
}
