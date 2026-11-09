use super::*;

#[tokio::test]
async fn activation_callback_authority_is_explicit_and_retired_before_publication() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Exclusive)
        .unwrap();
    assert!(matches!(
        activation.begin_invocation(),
        Err(ScopeError::Closed)
    ));
    assert!(matches!(
        activation.begin_lifecycle_invocation(LifecycleInvocationKind::Cleanup),
        Err(ScopeError::InvalidTransition)
    ));
    let lifecycle = activation
        .begin_lifecycle_invocation(LifecycleInvocationKind::Activation)
        .unwrap();
    assert!(matches!(
        activation.begin_lifecycle_invocation(LifecycleInvocationKind::Activation),
        Err(ScopeError::Busy)
    ));
    let callback = lifecycle.callback(Arc::new(7_u64)).unwrap();
    let admitted = callback.try_admit().unwrap();
    assert_eq!(*admitted, 7);
    assert_eq!(activation.state().unwrap(), LifecycleState::Activating);
    assert!(matches!(
        activation.mark_active(),
        Err(ScopeError::Outstanding(_))
    ));
    let child = lifecycle.begin_child().unwrap();
    let nested = child.try_admit().unwrap();
    drop(nested);
    child.drain().await.unwrap();
    child.finish().unwrap();
    drop(admitted);
    lifecycle.drain().await.unwrap();
    drop(lifecycle.release_resource(callback.id()).unwrap());
    lifecycle.finish().unwrap();
    assert!(matches!(callback.try_admit(), Err(ScopeError::Closed)));
    activation.mark_active().unwrap();
    let ordinary = activation.begin_invocation().unwrap();
    ordinary.drain().await.unwrap();
    ordinary.finish().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    instance.drain().await.unwrap();
    instance.finish().unwrap();
}

#[tokio::test]
async fn cleanup_callbacks_do_not_reopen_normal_input_and_parent_close_fences_them() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let ordinary = activation.begin_invocation().unwrap();
    let ordinary_guard = ordinary.try_admit().unwrap();
    activation.close().unwrap();
    assert!(matches!(
        activation.begin_lifecycle_invocation(LifecycleInvocationKind::Cleanup),
        Err(ScopeError::Outstanding(_))
    ));
    drop(ordinary_guard);
    ordinary.drain().await.unwrap();
    ordinary.finish().unwrap();
    let cleanup = activation
        .begin_lifecycle_invocation(LifecycleInvocationKind::Cleanup)
        .unwrap();
    assert!(activation.is_cancelled().unwrap());
    assert!(!cleanup.is_cancelled().unwrap());
    assert!(matches!(
        activation.begin_invocation(),
        Err(ScopeError::Closed)
    ));
    assert!(matches!(activation.try_admit(), Err(ScopeError::Closed)));
    let callback = cleanup.try_admit().unwrap();
    let child = cleanup.begin_child().unwrap();
    let nested = child.try_admit().unwrap();
    instance.close().unwrap();
    assert!(cleanup.is_cancelled().unwrap());
    assert!(child.is_cancelled().unwrap());
    assert!(matches!(cleanup.try_admit(), Err(ScopeError::Closed)));
    assert!(matches!(child.try_admit(), Err(ScopeError::Closed)));
    assert!(matches!(cleanup.finish(), Err(ScopeError::Outstanding(_))));
    drop(nested);
    drop(callback);
    child.drain().await.unwrap();
    child.finish().unwrap();
    cleanup.drain().await.unwrap();
    cleanup.finish().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    instance.drain().await.unwrap();
    instance.finish().unwrap();
}

#[tokio::test]
async fn cleanup_required_parent_preserves_failure_and_owned_tasks() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation
        .fail_cleanup("partial activation".into())
        .unwrap();
    let cleanup = activation
        .begin_lifecycle_invocation(LifecycleInvocationKind::Cleanup)
        .unwrap();
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    cleanup
        .spawn("cleanup producer", |mut cancellation| async move {
            cancellation.cancelled().await;
            cancelled_tx.send(()).unwrap();
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        activation.resolve_cleanup(),
        Err(ScopeError::Outstanding(_))
    ));
    cleanup.close().unwrap();
    cancelled_rx.await.unwrap();
    cleanup.drain().await.unwrap();
    cleanup.finish().unwrap();
    assert!(matches!(
        activation.drain().await,
        Err(ScopeError::Cleanup(_))
    ));
    activation.resolve_cleanup().unwrap();
    activation.finish().unwrap();
    instance.drain().await.unwrap();
    instance.finish().unwrap();
}

#[tokio::test]
async fn closing_an_activation_cannot_be_bypassed_by_stale_activation_authority() {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    let lifecycle = activation
        .begin_lifecycle_invocation(LifecycleInvocationKind::Activation)
        .unwrap();
    let guard = lifecycle.try_admit().unwrap();
    activation.close().unwrap();
    assert!(matches!(lifecycle.try_admit(), Err(ScopeError::Closed)));
    assert!(matches!(lifecycle.begin_child(), Err(ScopeError::Closed)));
    assert!(matches!(
        activation.begin_lifecycle_invocation(LifecycleInvocationKind::Activation),
        Err(ScopeError::InvalidTransition)
    ));
    drop(guard);
    lifecycle.drain().await.unwrap();
    lifecycle.finish().unwrap();
    let cleanup = activation
        .begin_lifecycle_invocation(LifecycleInvocationKind::Cleanup)
        .unwrap();
    cleanup.drain().await.unwrap();
    cleanup.finish().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    instance.drain().await.unwrap();
    instance.finish().unwrap();
}
