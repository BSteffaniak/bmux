//! Retained shutdown acceptance: cancellation of an observer is not ownership loss.
use bmux_plugin_runtime::{
    background::BackgroundTasks,
    shutdown::{ShutdownParticipant, ShutdownPhase, ShutdownRegistry},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(2);

#[tokio::test]
async fn timed_out_drain_retains_work_and_closes_admission() {
    let tasks = BackgroundTasks::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let finished = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&finished);
    tasks
        .spawn("retained", |mut cancellation| async move {
            started_tx.send(()).unwrap();
            cancellation.cancelled().await;
            release_rx.await.unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    tokio::time::timeout(DEADLINE, started_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(tasks.shutdown(Duration::from_millis(1)).await.is_err());
    assert_eq!(finished.load(Ordering::SeqCst), 0);
    assert!(tasks.spawn("late", |_| async { Ok(()) }).is_err());
    release_tx.send(()).unwrap();
    tasks.shutdown(DEADLINE).await.unwrap();
    assert_eq!(finished.load(Ordering::SeqCst), 1);
    tasks.shutdown(DEADLINE).await.unwrap();
}

#[tokio::test]
async fn admission_racing_cancellation_is_either_rejected_or_drained() {
    for _ in 0..64 {
        let tasks = BackgroundTasks::new();
        let clone = tasks.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let other = Arc::clone(&barrier);
        let finished = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&finished);
        let admission = tokio::spawn(async move {
            other.wait().await;
            clone.spawn("racer", |mut cancellation| async move {
                cancellation.cancelled().await;
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });
        barrier.wait().await;
        tasks.cancel().unwrap();
        let admitted = admission.await.unwrap().is_ok();
        tasks.shutdown(DEADLINE).await.unwrap();
        assert_eq!(finished.load(Ordering::SeqCst), usize::from(admitted));
    }
}

#[tokio::test]
async fn concurrent_drain_observers_do_not_lose_completion_wakeup() {
    let tasks = BackgroundTasks::new();
    tasks
        .spawn("wait-for-cancel", |mut cancellation| async move {
            cancellation.cancelled().await;
            Ok(())
        })
        .unwrap();
    let (a, b) = tokio::join!(tasks.shutdown(DEADLINE), tasks.shutdown(DEADLINE));
    a.unwrap();
    b.unwrap();
}

#[tokio::test]
async fn failed_background_cleanup_is_retained_for_later_observers() {
    let tasks = BackgroundTasks::new();
    tasks
        .spawn("failure-owner", |mut cancellation| async move {
            cancellation.cancelled().await;
            Err("cannot persist fixture".into())
        })
        .unwrap();
    let first = tasks.shutdown(DEADLINE).await.unwrap_err();
    assert!(first.contains("failure-owner"));
    assert!(first.contains("cannot persist fixture"));
    assert_eq!(tasks.shutdown(DEADLINE).await.unwrap_err(), first);
}

struct Participant {
    id: &'static str,
    phase: ShutdownPhase,
    log: Arc<Mutex<Vec<&'static str>>>,
    wait: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    failure: bool,
}
impl ShutdownParticipant for Participant {
    fn id(&self) -> &str {
        self.id
    }
    fn phase(&self) -> ShutdownPhase {
        self.phase
    }
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>> {
        let wait = self.wait.lock().unwrap().take();
        let log = Arc::clone(&self.log);
        let id = self.id;
        let failure = self.failure;
        Box::pin(async move {
            if let Some(wait) = wait {
                wait.await.map_err(|error| error.to_string())?;
            }
            log.lock().unwrap().push(id);
            if failure {
                Err("retained failure".into())
            } else {
                Ok(())
            }
        })
    }
}
#[tokio::test]
async fn shutdown_observer_timeout_does_not_restart_or_discard_cleanup() {
    let registry = ShutdownRegistry::default();
    let log = Arc::new(Mutex::new(Vec::new()));
    let (release, wait) = tokio::sync::oneshot::channel();
    registry
        .register(Arc::new(Participant {
            id: "persist",
            phase: ShutdownPhase::Persist,
            log: Arc::clone(&log),
            wait: Mutex::new(Some(wait)),
            failure: true,
        }))
        .unwrap();
    registry
        .register(Arc::new(Participant {
            id: "release",
            phase: ShutdownPhase::Release,
            log: Arc::clone(&log),
            wait: Mutex::new(None),
            failure: false,
        }))
        .unwrap();
    registry
        .register(Arc::new(Participant {
            id: "clear",
            phase: ShutdownPhase::Clear,
            log: Arc::clone(&log),
            wait: Mutex::new(None),
            failure: false,
        }))
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), registry.run())
            .await
            .is_err()
    );
    assert!(log.lock().unwrap().is_empty());
    assert!(
        registry
            .register(Arc::new(Participant {
                id: "late",
                phase: ShutdownPhase::Clear,
                log: Arc::clone(&log),
                wait: Mutex::new(None),
                failure: false
            }))
            .is_err()
    );
    release.send(()).unwrap();
    let result = tokio::time::timeout(DEADLINE, registry.run())
        .await
        .unwrap()
        .unwrap_err();
    assert!(result.contains("persist"));
    assert_eq!(*log.lock().unwrap(), vec!["persist", "release", "clear"]);
    assert_eq!(registry.run().await.unwrap_err(), result);
    assert_eq!(log.lock().unwrap().len(), 3);
}
