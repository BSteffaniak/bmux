use super::*;
use std::{
    sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

fn scopes(policy: PluginConcurrencyConfig) -> (InstanceScope, ActivationScope, InvocationScope) {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance.begin_activation(policy).unwrap();
    activation.mark_active().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    (instance, activation, invocation)
}

async fn retire(instance: InstanceScope, activation: ActivationScope, invocation: InvocationScope) {
    invocation.drain().await.unwrap();
    invocation.finish().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    instance.drain().await.unwrap();
    instance.finish().unwrap();
}

#[derive(Default)]
struct WakeCount(AtomicUsize);
impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn last_callback_wakes_registered_drain_without_timers() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    let callback = invocation.callback(Arc::new(41_u64)).unwrap();
    let admitted = callback.try_admit().unwrap();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    let mut drain = Box::pin(invocation.drain());
    assert!(matches!(drain.as_mut().poll(&mut cx), Poll::Pending));
    assert!(matches!(callback.try_admit(), Err(ScopeError::Closed)));
    assert_eq!(*admitted, 41);
    drop(admitted);
    assert!(wakes.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(drain.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    drop(drain);
    drop(invocation.release_resource(callback.id()).unwrap());
    assert!(matches!(callback.try_admit(), Err(ScopeError::Closed)));
    retire(instance, activation, invocation).await;
}

#[tokio::test]
async fn completion_between_predicate_and_wait_is_not_lost() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    let admitted = invocation.try_admit().unwrap();
    invocation.close().unwrap();
    // Force the formerly broken interleaving: inspect nonzero, complete the
    // final callback, and only then poll the notification future.
    let mut changed = invocation.0.tree.changed.subscribe();
    assert_eq!(
        invocation.0.tree.lock().unwrap()[&invocation.0.key].admitted,
        1
    );
    drop(admitted);
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes);
    let mut cx = Context::from_waker(&waker);
    let mut notification = Box::pin(changed.changed());
    assert!(matches!(
        notification.as_mut().poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    drop(notification);
    retire(instance, activation, invocation).await;
}

#[tokio::test]
async fn close_linearizes_before_a_delayed_admitter() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    let callback = invocation.callback(Arc::new(())).unwrap();
    let ready = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let caller = {
        let ready = ready.clone();
        let resume = resume.clone();
        std::thread::spawn(move || {
            // A stale caller already possesses the capability, but has not
            // admitted. Closing must not depend on destroying this handle.
            ready.wait();
            resume.wait();
            assert!(matches!(callback.try_admit(), Err(ScopeError::Closed)));
        })
    };
    ready.wait();
    invocation.close().unwrap();
    resume.wait();
    caller.join().unwrap();
    let id = *invocation.0.tree.lock().unwrap()[&invocation.0.key]
        .resources
        .keys()
        .next()
        .unwrap();
    drop(invocation.release_resource(id).unwrap());
    retire(instance, activation, invocation).await;
}

#[tokio::test]
async fn admitted_callback_keeps_every_ancestor_outstanding() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    let guard = invocation.try_admit().unwrap();
    instance.close().unwrap();
    assert!(matches!(invocation.try_admit(), Err(ScopeError::Closed)));
    assert!(matches!(invocation.finish(), Err(ScopeError::Outstanding)));
    assert!(matches!(activation.finish(), Err(ScopeError::Outstanding)));
    assert!(matches!(instance.finish(), Err(ScopeError::Outstanding)));
    drop(guard);
    retire(instance, activation, invocation).await;
}

#[tokio::test]
async fn exclusive_invocation_does_not_deadlock_nested_callbacks() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Exclusive);
    assert!(matches!(
        activation.begin_invocation(),
        Err(ScopeError::Busy)
    ));
    let callback = invocation.callback(Arc::new(AtomicUsize::new(0))).unwrap();
    let outer = callback.try_admit().unwrap();
    let inner = callback.try_admit().unwrap();
    inner.fetch_add(1, Ordering::SeqCst);
    assert_eq!(outer.load(Ordering::SeqCst), 1);
    // Recursive host service dispatch is a child, not a competing top-level
    // invocation. It must work even with exclusive plugin concurrency.
    let child = invocation.begin_child().unwrap();
    assert_eq!(child.activation_id().unwrap(), activation.id());
    assert_ne!(child.id(), invocation.id());
    let child_callback = child.try_admit().unwrap();
    drop(child_callback);
    child.drain().await.unwrap();
    child.finish().unwrap();
    drop(inner);
    drop(outer);
    drop(invocation.release_resource(callback.id()).unwrap());
    assert!(matches!(
        callback.try_admit(),
        Err(ScopeError::UnknownResource)
    ));
    invocation.drain().await.unwrap();
    invocation.finish().unwrap();
    let second = activation.begin_invocation().unwrap();
    assert_ne!(second.id(), invocation.id());
    retire(instance, activation, second).await;
}

#[tokio::test]
async fn cancelled_drain_observer_retains_task_and_owner() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    invocation
        .spawn("producer", |mut cancel| async move {
            cancel.cancelled().await;
            cancelled_tx.send(()).unwrap();
            release_rx.await.unwrap();
            Ok(())
        })
        .unwrap();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes);
    let mut cx = Context::from_waker(&waker);
    let mut observer = Box::pin(invocation.drain());
    assert!(matches!(observer.as_mut().poll(&mut cx), Poll::Pending));
    drop(observer);
    cancelled_rx.await.unwrap();
    assert!(matches!(invocation.finish(), Err(ScopeError::Outstanding)));
    assert!(matches!(
        invocation.spawn("late", |_| async { Ok(()) }),
        Err(ScopeError::Closed)
    ));
    release_tx.send(()).unwrap();
    retire(instance, activation, invocation).await;
}

#[tokio::test]
async fn resources_and_failures_require_explicit_cleanup_acknowledgement() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    let resource = invocation
        .register_resource(Arc::new("subscription"))
        .unwrap();
    assert!(matches!(
        activation.release_resource(resource),
        Err(ScopeError::Outstanding)
    ));
    invocation
        .fail_cleanup("unsubscribe failed".into())
        .unwrap();
    assert!(matches!(
        invocation.drain().await,
        Err(ScopeError::Cleanup(_))
    ));
    assert!(matches!(
        invocation.resolve_cleanup(),
        Err(ScopeError::Outstanding)
    ));
    drop(invocation.release_resource(resource).unwrap());
    invocation.resolve_cleanup().unwrap();
    retire(instance, activation, invocation).await;
}

#[tokio::test]
async fn failed_activation_rejects_publication_and_can_be_reactivated_after_cleanup() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    assert!(matches!(
        activation.begin_invocation(),
        Err(ScopeError::Closed)
    ));
    activation.close().unwrap();
    assert!(matches!(
        activation.mark_active(),
        Err(ScopeError::InvalidTransition)
    ));
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    assert_eq!(instance.state().unwrap(), LifecycleState::Inactive);
    let second = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    assert_ne!(activation.id(), second.id());
    second.mark_active().unwrap();
    let invocation = second.begin_invocation().unwrap();
    retire(instance, second, invocation).await;
}

#[tokio::test]
async fn dropped_observers_do_not_release_unfinished_instance() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    let weak = Arc::downgrade(&instance.0.tree);
    drop(instance);
    drop(activation);
    drop(invocation);
    let retained = weak.upgrade().expect("unfinished instance remains owned");
    let instance = InstanceScope(Scope {
        tree: retained.clone(),
        key: Key::Instance,
    });
    let (activation_key, invocation_key) = {
        let nodes = retained.lock().unwrap();
        (
            *nodes
                .keys()
                .find(|key| matches!(key, Key::Activation(_)))
                .unwrap(),
            *nodes
                .keys()
                .find(|key| matches!(key, Key::Invocation(_)))
                .unwrap(),
        )
    };
    let activation = ActivationScope(Scope {
        tree: retained.clone(),
        key: activation_key,
    });
    let invocation = InvocationScope(Scope {
        tree: retained.clone(),
        key: invocation_key,
    });
    drop(retained);
    retire(instance, activation, invocation).await;
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn task_failure_is_retained_until_explicit_repair() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    invocation
        .spawn("failed", |_| async { Err("cleanup effect failed".into()) })
        .unwrap();
    for _ in 0..2 {
        assert!(matches!(
            invocation.drain().await,
            Err(ScopeError::Cleanup(_))
        ));
    }
    assert_eq!(invocation.state().unwrap(), LifecycleState::CleanupRequired);
    assert!(matches!(invocation.finish(), Err(ScopeError::Cleanup(_))));
    invocation.resolve_cleanup().unwrap();
    retire(instance, activation, invocation).await;
}

#[tokio::test]
async fn inactive_instance_can_be_destroyed_without_reactivation() {
    let (instance, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    invocation.drain().await.unwrap();
    invocation.finish().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    assert_eq!(instance.state().unwrap(), LifecycleState::Inactive);
    instance.drain().await.unwrap();
    instance.finish().unwrap();
    assert_eq!(instance.state().unwrap(), LifecycleState::Destroyed);
}

#[tokio::test]
async fn identities_are_lossless_and_hosts_close_independently() {
    let (first, activation, invocation) = scopes(PluginConcurrencyConfig::Concurrent);
    let (second, second_activation, second_invocation) =
        scopes(PluginConcurrencyConfig::Concurrent);
    assert_ne!(first.id(), second.id());
    assert_ne!(first.runtime_id(), second.runtime_id());
    let id = invocation.id();
    assert_eq!(RuntimeInvocationId::from_uuid(id.as_uuid()), id);
    first.close().unwrap();
    let guard = second_invocation.try_admit().unwrap();
    drop(guard);
    retire(first, activation, invocation).await;
    retire(second, second_activation, second_invocation).await;
}
