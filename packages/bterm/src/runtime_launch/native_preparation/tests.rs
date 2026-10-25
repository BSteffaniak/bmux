use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn fixture(task: tokio::task::JoinHandle<Result<()>>) -> NativePreparation {
    let (committed, _receiver) = tokio::sync::oneshot::channel();
    let (abandoned, _abandonment) = tokio::sync::oneshot::channel();
    NativePreparation {
        task,
        committed: Some(committed),
        abandoned: Some(abandoned),
    }
}

#[tokio::test]
async fn lost_preparation_never_polls_or_replays_attachment() {
    let task = tokio::spawn(async { anyhow::bail!("lease lost") });
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    let effects = Arc::new(AtomicUsize::new(0));
    let observed = effects.clone();
    let result = fixture(task)
        .run(async move {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert!(result.is_err());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn released_preparation_allows_exactly_one_attachment_future() {
    let task = tokio::spawn(async { Ok(()) });
    let effects = Arc::new(AtomicUsize::new(0));
    let observed = effects.clone();
    fixture(task)
        .run(async move {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn drop_requests_abandonment_and_commit_is_one_shot() {
    let (committed, receiver) = tokio::sync::oneshot::channel();
    let (abandoned, abandonment) = tokio::sync::oneshot::channel();
    let mut preparation = NativePreparation {
        task: tokio::spawn(async { std::future::pending().await }),
        committed: Some(committed),
        abandoned: Some(abandoned),
    };
    preparation.commit_callback()();
    receiver.await.unwrap();
    drop(preparation);
    abandonment.await.unwrap();
}
