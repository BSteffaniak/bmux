//! The same behaviors through the bundled facade and through the exact
//! dynamic boundary (`PlugRtV1` host table + plugin-side client) in-process.
use super::guest::Client;
use super::host::PlugRtHost;
use crate::background::BackgroundTasks;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

/// Sets its flag when dropped, proving which side released a future.
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Counts polls so tests can prove progress came from wakes, not repolling.
struct Counted<F> {
    polls: Arc<AtomicUsize>,
    future: Pin<Box<F>>,
}
impl<F: Future> Future for Counted<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        self.future.as_mut().poll(cx)
    }
}
fn counted<F: Future>(future: F) -> (Arc<AtomicUsize>, Counted<F>) {
    let polls = Arc::new(AtomicUsize::new(0));
    (
        polls.clone(),
        Counted {
            polls,
            future: Box::pin(future),
        },
    )
}

fn dynamic(shim: bool) -> (&'static PlugRtHost, &'static Client) {
    let host = PlugRtHost::new(tokio::runtime::Handle::current(), "test.plug-rt");
    host.assume_fingerprint_match(!shim);
    let client = unsafe { Client::adopt(host.table()) }.expect("valid host table");
    let client = if shim {
        client.with_waker_shim()
    } else {
        client
    };
    (host, Box::leak(Box::new(client)))
}

// ---- Bundled ----------------------------------------------------------------

#[tokio::test]
async fn bundled_spawn_timer_and_wake_without_repolling() {
    let tasks = BackgroundTasks::new();
    let (sender, receiver) = tokio::sync::oneshot::channel::<u32>();
    let (polls, future) = counted(async move { receiver.await.unwrap() + 1 });
    let handle = {
        let _scope = tasks.enter();
        super::spawn_named("waiter", future)
    };
    super::time::sleep(Duration::from_millis(20)).await;
    let idle = polls.load(Ordering::SeqCst);
    assert!(idle <= 1, "an idle task is not repolled: {idle} polls");
    sender.send(41).unwrap();
    assert_eq!(handle.await.unwrap(), 42);
    assert!(polls.load(Ordering::SeqCst) <= 2);
    tasks.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn bundled_teardown_aborts_raw_tasks_and_closes_admission() {
    let tasks = BackgroundTasks::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(dropped.clone());
    let handle = {
        let _scope = tasks.enter();
        super::spawn_named("forever", async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        })
    };
    tasks.shutdown(Duration::from_secs(1)).await.unwrap();
    assert!(dropped.load(Ordering::SeqCst), "aborted future was dropped");
    assert!(handle.await.unwrap_err().is_cancelled());
    let late = {
        let _scope = tasks.enter();
        super::spawn(async {})
    };
    assert!(late.await.unwrap_err().is_closed());
}

#[tokio::test]
async fn bundled_panics_are_contained_and_blocking_jobs_are_owned() {
    let tasks = BackgroundTasks::new();
    let _scope = tasks.enter();
    let panicked = super::spawn(async { panic!("plugin bug") });
    assert!(panicked.await.unwrap_err().is_panic());
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let job = super::spawn_blocking_named("io", move || gate.recv().is_ok());
    assert_eq!(tasks.blocking_names(), vec!["io".to_string()]);
    release.send(()).unwrap();
    assert!(job.await.unwrap());
    let deadline = super::time::Instant::now() + Duration::from_secs(1);
    tasks.shutdown_until(deadline, None).await.unwrap();
    assert!(tasks.blocking_names().is_empty());
}

// ---- Dynamic boundary -------------------------------------------------------

async fn dynamic_spawn_timer_and_wake(shim: bool) {
    let (host, client) = dynamic(shim);
    let tasks = BackgroundTasks::new();
    host.bind_scope(7, tasks.clone());
    let (sender, receiver) = tokio::sync::oneshot::channel::<u32>();
    let (polls, future) = counted(async move {
        let started = client.now();
        client
            .sleep_until(started + Duration::from_millis(10))
            .await;
        assert!(client.now() >= started + Duration::from_millis(10));
        receiver.await.unwrap() + 1
    });
    let handle = client.spawn(7, "waiter", future);
    tokio::time::sleep(Duration::from_millis(40)).await;
    let idle = polls.load(Ordering::SeqCst);
    assert!(
        idle <= 2,
        "timer and idle wait are wake driven: {idle} polls"
    );
    sender.send(41).unwrap();
    assert_eq!(handle.await.unwrap(), 42);
    assert!(polls.load(Ordering::SeqCst) <= 3);
    tasks.shutdown(Duration::from_secs(1)).await.unwrap();
    host.unbind_scope(7);
}

#[tokio::test]
async fn dynamic_spawn_timer_and_wake_pass_through_wakers() {
    dynamic_spawn_timer_and_wake(false).await;
}

#[tokio::test]
async fn dynamic_spawn_timer_and_wake_through_waker_shim() {
    dynamic_spawn_timer_and_wake(true).await;
}

#[tokio::test]
async fn dynamic_teardown_drops_futures_through_the_plugin() {
    let (host, client) = dynamic(false);
    let tasks = BackgroundTasks::new();
    host.bind_scope(9, tasks.clone());
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(dropped.clone());
    let handle = client.spawn(9, "forever", async move {
        let _flag = flag;
        std::future::pending::<()>().await;
    });
    tokio::task::yield_now().await;
    assert_eq!(tasks.pending_names(), vec!["forever".to_string()]);
    tasks.shutdown(Duration::from_secs(1)).await.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert!(handle.await.unwrap_err().is_cancelled());
    // Stopped admitting: the plugin side observes a typed closed error.
    assert!(
        client
            .spawn(9, "late", async {})
            .await
            .unwrap_err()
            .is_closed()
    );
    host.unbind_scope(9);
    assert!(
        client
            .spawn(9, "gone", async {})
            .await
            .unwrap_err()
            .is_closed()
    );
}

#[tokio::test]
async fn dynamic_abort_panic_and_blocking() {
    let (host, client) = dynamic(true);
    let tasks = BackgroundTasks::new();
    host.bind_scope(3, tasks.clone());
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(dropped.clone());
    let aborted = client.spawn(3, "aborted", async move {
        let _flag = flag;
        std::future::pending::<()>().await;
    });
    aborted.abort();
    assert!(aborted.await.unwrap_err().is_cancelled());
    assert!(dropped.load(Ordering::SeqCst));
    let panicked = client.spawn(3, "panics", async { panic!("plugin bug") });
    assert!(panicked.await.unwrap_err().is_panic());
    let job = client.spawn_blocking(3, "io", || 7);
    assert_eq!(job.await.unwrap(), 7);
    let job = client.spawn_blocking(3, "io-panic", || -> u8 { panic!("blocking bug") });
    assert!(job.await.unwrap_err().is_panic());
    // The host is unaffected and still serves the scope.
    assert_eq!(client.spawn(3, "after", async { 5 }).await.unwrap(), 5);
    tasks.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn closing_scope_refuses_plugin_work_but_runs_operation_owned_jobs() {
    let (host, client) = dynamic(false);
    let tasks = BackgroundTasks::new();
    host.bind_scope(13, tasks.clone());
    tasks.cancel().unwrap();
    // Plugin-initiated work is refused once teardown began...
    assert!(
        client
            .spawn_blocking(13, "late", || 1)
            .await
            .unwrap_err()
            .is_closed()
    );
    assert!(
        client
            .spawn(13, "late", async {})
            .await
            .unwrap_err()
            .is_closed()
    );
    // ...but blocking work a host operation awaits (a synchronous author
    // callback of the host's own deactivation) still runs, inside the scope.
    let scope = client
        .spawn_blocking_for_operation(13, "deactivate-callback", super::scope::current_remote)
        .await
        .unwrap();
    assert_eq!(scope, 13);
    // Bundled: same contract.
    let bundled = {
        let _scope = tasks.enter();
        super::spawn_blocking_for_operation("deactivate-callback", || 2)
    };
    assert_eq!(bundled.await.unwrap(), 2);
    host.unbind_scope(13);
}

#[tokio::test]
async fn dynamic_nested_spawns_inherit_the_scope() {
    let (host, client) = dynamic(false);
    let tasks = BackgroundTasks::new();
    host.bind_scope(11, tasks.clone());
    let inner = client.spawn(11, "outer", async move {
        // Inside a plugin task the entered scope is the instance handle.
        let scope = super::scope::current_remote();
        client.spawn(scope, "inner", async move { scope }).await
    });
    assert_eq!(inner.await.unwrap().unwrap(), 11);
    tasks.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[test]
fn install_refuses_bundled_builds_and_reports_identity() {
    let mut guest = super::abi::PlugRtGuestV1 {
        size: 0,
        abi_version: 0,
        build_fingerprint: [0; 32],
        panic_unwind: 0,
    };
    let status = unsafe { super::guest::install(std::ptr::null(), &raw mut guest) };
    assert_eq!(status, super::abi::RT_UNSUPPORTED);
    assert_eq!(guest.abi_version, super::abi::PLUG_RT_ABI_VERSION);
    assert_eq!(guest.build_fingerprint, super::abi::BUILD_FINGERPRINT);
    assert_eq!(guest.panic_unwind, 1);
}
