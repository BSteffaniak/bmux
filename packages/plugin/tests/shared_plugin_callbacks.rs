//! Public ownership-tree behavior under callback closure and observer races.
use bmux_plugin_runtime::{
    PluginConcurrencyConfig, PluginRuntimeScopeId,
    scope::{ActivationScope, InstanceScope, LifecycleState},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn active() -> (InstanceScope, ActivationScope) {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    (instance, activation)
}
async fn finish(instance: InstanceScope, activation: ActivationScope) {
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    instance.drain().await.unwrap();
    instance.finish().unwrap();
}
#[tokio::test]
async fn nested_callback_admission_is_reentrant_and_retirement_fences_stale_handles() {
    let (instance, activation) = active();
    let invocation = activation.begin_invocation().unwrap();
    let callback = invocation.callback(Arc::new(AtomicUsize::new(0))).unwrap();
    let first = callback.try_admit().unwrap();
    let nested = callback.try_admit().unwrap();
    first.fetch_add(1, Ordering::SeqCst);
    nested.fetch_add(1, Ordering::SeqCst);
    assert_eq!(nested.load(Ordering::SeqCst), 2);
    invocation.close().unwrap();
    assert!(callback.try_admit().is_err());
    assert!(invocation.release_resource(callback.id()).is_err());
    drop(first);
    drop(nested);
    tokio::time::timeout(Duration::from_secs(2), invocation.drain())
        .await
        .unwrap()
        .unwrap();
    drop(invocation.release_resource(callback.id()).unwrap());
    invocation.finish().unwrap();
    assert!(callback.try_admit().is_err());
    finish(instance, activation).await;
}
#[tokio::test]
async fn callback_admission_racing_close_never_outlives_drain() {
    for _ in 0..64 {
        let (instance, activation) = active();
        let invocation = activation.begin_invocation().unwrap();
        let callback = Arc::new(invocation.callback(Arc::new(AtomicUsize::new(0))).unwrap());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let other = Arc::clone(&barrier);
        let caller = Arc::clone(&callback);
        let admitted = tokio::spawn(async move {
            other.wait().await;
            if let Ok(guard) = caller.try_admit() {
                tokio::task::yield_now().await;
                guard.fetch_add(1, Ordering::SeqCst);
            }
        });
        barrier.wait().await;
        invocation.close().unwrap();
        tokio::time::timeout(Duration::from_secs(2), invocation.drain())
            .await
            .unwrap()
            .unwrap();
        admitted.await.unwrap();
        assert!(callback.try_admit().is_err());
        drop(invocation.release_resource(callback.id()).unwrap());
        invocation.finish().unwrap();
        finish(instance, activation).await;
    }
}
#[tokio::test]
async fn cancelled_drain_observer_retains_admission_and_cleanup_authority() {
    let (instance, activation) = active();
    let invocation = activation.begin_invocation().unwrap();
    let guard = invocation.try_admit().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), invocation.drain())
            .await
            .is_err()
    );
    assert!(invocation.finish().is_err());
    assert!(invocation.try_admit().is_err());
    drop(guard);
    let (a, b) = tokio::join!(invocation.drain(), invocation.drain());
    a.unwrap();
    b.unwrap();
    invocation.finish().unwrap();
    finish(instance, activation).await;
}
#[tokio::test]
async fn cleanup_failure_retains_resources_until_explicit_repair() {
    let (instance, activation) = active();
    let resource = activation
        .register_resource(Arc::new(AtomicUsize::new(0)))
        .unwrap();
    activation
        .fail_cleanup("injected persist failure".into())
        .unwrap();
    assert_eq!(activation.state().unwrap(), LifecycleState::CleanupRequired);
    assert!(activation.finish().is_err());
    assert!(activation.resolve_cleanup().is_err());
    drop(activation.release_resource(resource).unwrap());
    assert!(
        activation.drain().await.is_err(),
        "failed cleanup remains explicit after draining"
    );
    activation.resolve_cleanup().unwrap();
    finish(instance, activation).await;
}
