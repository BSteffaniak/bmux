use super::*;
use crate::{
    PluginBlockingCallback, PluginCallbackAuthority, PluginCallbackAuthoritySource,
    PluginCallbackContext, PluginCallbackEntryGuard, PluginCallbackExecution,
    PluginCallbackExecutor, PluginCallbackSettlement,
};
use bmux_plugin_runtime::PluginInstanceId;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Source {
    entries: AtomicUsize,
    releases: Arc<AtomicUsize>,
    released: Arc<tokio::sync::Notify>,
    recursive: AtomicBool,
    reentrant: AtomicBool,
    cancel: tokio::sync::Notify,
    cancelled: AtomicBool,
}
struct Lease(Arc<AtomicUsize>, Arc<tokio::sync::Notify>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.notify_one();
    }
}
impl PluginCallbackAuthoritySource for Arc<Source> {
    fn check_live(&self) -> PluginResult<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(PluginCommandError::unavailable("cancelled"));
        }
        Ok(())
    }
    fn enter_adapter_entry(&self, _: PluginInstanceId) -> PluginResult<PluginCallbackEntryGuard> {
        self.check_live()?;
        if self.recursive.load(Ordering::SeqCst) {
            return Err(PluginCommandError::unavailable(
                "authenticated recursive ancestor",
            ));
        }
        self.entries.fetch_add(1, Ordering::SeqCst);
        let guard = PluginCallbackEntryGuard::from_host(Lease(
            Arc::clone(&self.releases),
            Arc::clone(&self.released),
        ));
        Ok(if self.reentrant.load(Ordering::SeqCst) {
            guard.into_reentrant()
        } else {
            guard
        })
    }
    fn cancelled(&self) -> PluginCallbackSettlement {
        let source = Self::clone(self);
        Box::pin(async move {
            let notified = source.cancel.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !source.cancelled.load(Ordering::SeqCst) {
                notified.await;
            }
            Ok(())
        })
    }
}
struct Executor;
impl PluginCallbackExecution for Executor {
    fn execute(
        &self,
        _: PluginCallbackAuthority,
        _: PluginInstanceId,
        _: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        panic!("must never acquire a second entry")
    }
    fn execute_entered(
        &self,
        authority: PluginCallbackAuthority,
        target: PluginInstanceId,
        entry: PluginCallbackEntryGuard,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        Box::pin(async move {
            entry.validate_for(&authority, target)?;
            tokio::task::spawn_blocking(move || {
                entry.validate_for(&authority, target)?;
                let result = callback();
                drop(entry);
                result
            })
            .await
            .map_err(|error| PluginCommandError::failed(error.to_string()))?
        })
    }
}
fn target() -> PluginInstanceId {
    PluginInstanceId::from_uuid(uuid::Uuid::from_u128(71))
}
fn context(source: &Arc<Source>) -> PluginCallbackContext {
    PluginCallbackContext::from_host(
        PluginCallbackAuthority::from_host(Arc::new(Arc::clone(source))),
        PluginCallbackExecutor::from_host(Arc::new(Executor)),
    )
}

#[tokio::test]
async fn selected_execution_enters_once_and_missing_authority_has_zero_effects() {
    let source = Arc::new(Source::default());
    let adapter = RustPluginAdapter::for_instance((), target());
    let author_thread = adapter
        .schedule(Some(context(&source)), true, |_| {
            Ok(std::thread::current().id())
        })
        .await
        .unwrap();
    assert_ne!(author_thread, std::thread::current().id());
    assert_eq!(source.entries.load(Ordering::SeqCst), 1);
    assert_eq!(source.releases.load(Ordering::SeqCst), 1);
    assert!(
        adapter
            .schedule(None, true, |_| -> PluginResult<()> {
                panic!("missing authority reached author")
            })
            .await
            .is_err()
    );
    source.recursive.store(true, Ordering::SeqCst);
    assert!(
        adapter
            .schedule(Some(context(&source)), true, |_| -> PluginResult<()> {
                panic!("recursive entry reached author")
            })
            .await
            .is_err()
    );
    assert_eq!(source.entries.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn queued_drop_and_wake_driven_cancel_release_exact_entry_without_author() {
    let adapter = RustPluginAdapter::for_instance((), target());
    let permit = adapter.1.acquire(true).await.unwrap();
    let source = Arc::new(Source::default());
    let mut queued = adapter.schedule(Some(context(&source)), true, |_| -> PluginResult<()> {
        panic!("queued author executed")
    });
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(queued.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert_eq!(source.entries.load(Ordering::SeqCst), 1);
    drop(queued);
    assert_eq!(source.releases.load(Ordering::SeqCst), 1);
    let queued = adapter.schedule(Some(context(&source)), true, |_| -> PluginResult<()> {
        panic!("cancelled author executed")
    });
    tokio::pin!(queued);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(queued.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    source.cancelled.store(true, Ordering::SeqCst);
    source.cancel.notify_waiters();
    assert!(queued.await.is_err());
    assert_eq!(source.releases.load(Ordering::SeqCst), 2);
    drop(permit);
}

#[tokio::test]
async fn accepted_job_retains_entry_and_permit_after_observer_loss() {
    let adapter = RustPluginAdapter::for_instance((), target());
    let source = Arc::new(Source::default());
    let (started, mut observe_started) = tokio::sync::oneshot::channel();
    let (finish, wait_finish) = std::sync::mpsc::sync_channel(1);
    let mut job = adapter.schedule(Some(context(&source)), true, move |_| {
        started.send(()).unwrap();
        wait_finish.recv().unwrap();
        Ok(())
    });
    std::future::poll_fn(|cx| {
        assert!(job.as_mut().poll(cx).is_pending());
        Pin::new(&mut observe_started).poll(cx)
    })
    .await
    .unwrap();
    drop(job);
    assert_eq!(source.releases.load(Ordering::SeqCst), 0);
    let mut waiting = Box::pin(adapter.1.acquire(true));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    finish.send(()).unwrap();
    let permit = waiting.await.unwrap();
    drop(permit);
    source.released.notified().await;
    assert_eq!(source.releases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reentrant_entry_fails_fast_on_held_gate_instead_of_waiting() {
    let adapter = RustPluginAdapter::for_instance((), target());
    let source = Arc::new(Source::default());
    source.reentrant.store(true, Ordering::SeqCst);
    // The outer entry of this same instance holds the gate exclusively (a
    // command or event body that called out and is now being re-entered).
    let outer = adapter.1.acquire(true).await.unwrap();
    let reentered = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        adapter.schedule(Some(context(&source)), false, |_| -> PluginResult<()> {
            panic!("re-entrant query must not run while the gate is held")
        }),
    )
    .await
    .expect("re-entrant admission must not wait on the instance gate");
    let error = reentered.expect_err("held gate must reject re-entry");
    assert!(
        error
            .message
            .contains("reentrant call would deadlock on exclusive instance gate"),
        "unexpected error: {}",
        error.message
    );
    assert_eq!(error.code, PluginCommandError::unavailable("").code);
    // The rejected entry was released without reaching the author and the
    // gate never recorded a waiter for it.
    assert_eq!(source.entries.load(Ordering::SeqCst), 1);
    assert_eq!(source.releases.load(Ordering::SeqCst), 1);
    drop(outer);
    // With the gate free, the same re-entrant entry is admitted and runs.
    adapter
        .schedule(Some(context(&source)), false, |_| Ok(()))
        .await
        .expect("free gate admits re-entrant query");
    assert_eq!(source.entries.load(Ordering::SeqCst), 2);
}
