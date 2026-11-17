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

// ---- Serialized activation events ------------------------------------------

type FakeChannels =
    std::sync::Mutex<std::collections::HashMap<String, tokio::sync::broadcast::Sender<Vec<u8>>>>;

/// A host event port over one broadcast channel per kind.
#[derive(Default)]
struct FakeEvents {
    channels: FakeChannels,
}
impl super::events::ScopeEvents for FakeEvents {
    fn register(&self, kind: &str) -> Result<(), super::events::EventPortError> {
        self.channels
            .lock()
            .unwrap()
            .insert(kind.into(), tokio::sync::broadcast::channel(8).0);
        Ok(())
    }
    fn publish(&self, kind: &str, payload: &[u8]) -> Result<(), super::events::EventPortError> {
        let channels = self.channels.lock().unwrap();
        let channel = channels
            .get(kind)
            .ok_or_else(|| super::events::EventPortError::Rejected("unknown".into()))?;
        let _ = channel.send(payload.to_vec());
        Ok(())
    }
    fn subscribe(
        &self,
        kind: &str,
        _state: bool,
    ) -> Result<Box<dyn super::events::EventStream>, super::events::EventPortError> {
        let channels = self.channels.lock().unwrap();
        let channel = channels
            .get(kind)
            .ok_or_else(|| super::events::EventPortError::Rejected("unknown".into()))?;
        Ok(super::events::broadcast_stream(channel.subscribe(), Some))
    }
}

#[tokio::test]
async fn dynamic_events_publish_subscribe_with_wakes_and_release_with_the_scope() {
    let (host, client) = dynamic(true);
    let events = client.events(21);
    // No port bound yet: an explicit unsupported error, never a fallback.
    assert_eq!(
        events.register("k").unwrap_err(),
        super::events::EventPortError::Unsupported
    );
    host.bind_scope_events(21, Arc::new(FakeEvents::default()));
    events.register("k").unwrap();
    assert!(matches!(
        events.publish("missing", b"{}"),
        Err(super::events::EventPortError::Rejected(_))
    ));
    let mut subscription = events.subscribe("k", false).unwrap();
    assert_eq!(host.event_subscription_count(), 1);
    // The plugin waits with its waker; a publish wakes it, nothing repolls.
    let (polls, next) = counted(async move {
        let first = subscription.next().await;
        let second = subscription.next().await;
        (first, second, subscription)
    });
    let waiter = tokio::spawn(next);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(polls.load(Ordering::SeqCst) <= 1);
    events.publish("k", b"{\"n\":1}").unwrap();
    // Larger than the plugin's initial buffer: retained and copied after growth.
    let large = vec![b'x'; 10_000];
    events.publish("k", &large).unwrap();
    let (first, second, mut subscription) = waiter.await.unwrap();
    assert_eq!(first.as_deref(), Some(&b"{\"n\":1}"[..]));
    assert_eq!(second.as_deref(), Some(&large[..]));
    // Teardown releases the scope's subscriptions; the stream then ends.
    host.unbind_scope(21);
    assert_eq!(host.event_subscription_count(), 0);
    assert_eq!(subscription.next().await, None);
    assert_eq!(
        events.register("k").unwrap_err(),
        super::events::EventPortError::Unsupported
    );
}

// ---- Asynchronous host service calls ---------------------------------------

/// Echoes the request after the host releases it, recording host futures
/// that were dropped before completing.
struct EchoPorts {
    release: Arc<tokio::sync::Notify>,
    cancelled: Arc<AtomicUsize>,
}
impl super::services::PortServices for EchoPorts {
    fn call(&self, port: u64, request: Vec<u8>) -> super::services::ServiceOutcome {
        let release = Arc::clone(&self.release);
        let cancelled = Arc::clone(&self.cancelled);
        Box::pin(async move {
            struct Unfinished(Arc<AtomicUsize>, bool);
            impl Drop for Unfinished {
                fn drop(&mut self) {
                    if !self.1 {
                        self.0.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            let mut guard = Unfinished(cancelled, false);
            release.notified().await;
            guard.1 = true;
            let mut reply = port.to_le_bytes().to_vec();
            reply.extend_from_slice(&request);
            reply
        })
    }
}

#[tokio::test]
async fn dynamic_service_calls_wake_on_completion_cancel_and_close_with_the_scope() {
    let (host, client) = dynamic(false);
    let tasks = BackgroundTasks::new();
    host.bind_scope(31, tasks.clone());
    // No resolver installed: an explicit refusal, never a fallback.
    assert!(client.service_call(31, 1, b"x").await.is_err());
    let release = Arc::new(tokio::sync::Notify::new());
    let cancelled = Arc::new(AtomicUsize::new(0));
    host.set_port_services(Arc::new(EchoPorts {
        release: Arc::clone(&release),
        cancelled: Arc::clone(&cancelled),
    }));
    // Completion wakes the plugin; the outcome may exceed its first buffer.
    let large = vec![b'y'; 10_000];
    let (polls, call) = counted(client.service_call(31, 7, &large));
    let waiter = tokio::spawn(call);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        polls.load(Ordering::SeqCst) <= 1,
        "pending call is not repolled"
    );
    release.notify_one();
    let reply = waiter.await.unwrap().unwrap();
    assert_eq!(&reply[..8], &7_u64.to_le_bytes());
    assert_eq!(&reply[8..], &large[..]);
    assert_eq!(host.service_call_count(), 0);
    // Dropping the plugin future cancels the call: the host future drops.
    let call = client.service_call(31, 8, b"never");
    tokio::task::yield_now().await;
    drop(call);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(host.service_call_count(), 0);
    // A call in flight at teardown is aborted with the scope.
    let pending = client.service_call(31, 9, b"teardown");
    // Let the host task start the call before teardown aborts it.
    tokio::task::yield_now().await;
    tasks.shutdown(Duration::from_secs(1)).await.unwrap();
    assert_eq!(cancelled.load(Ordering::SeqCst), 2);
    host.unbind_scope(31);
    assert!(pending.await.is_err());
    // Closed scope: refused before any host work.
    assert!(client.service_call(31, 10, b"late").await.is_err());
}
