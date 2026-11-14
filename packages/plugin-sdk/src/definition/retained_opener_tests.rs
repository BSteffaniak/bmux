use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn recovery_v2_missing_is_unsupported_and_does_not_open_v1() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let services = PluginHostServices::new(|_| Ok(Vec::new())).with_retained_host(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        Err(crate::PluginError::UnsupportedHostOperation { operation: "v1" })
    });
    assert!(matches!(
        services.open_retained_host_v2(),
        Err(crate::PluginError::UnsupportedHostOperation {
            operation: "open_retained_host_v2"
        })
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[test]
fn recovery_v2_selected_rejection_survives_clone_and_decorators() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let services = PluginHostServices::new(|_| Ok(Vec::new()));
    let bound = services.clone().with_retained_host_v2(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        Err(crate::PluginError::InvocationCancelled)
    });
    assert_ne!(services, bound);
    assert_eq!(bound, bound.clone());
    let decorated = bound.clone().with_kernel(|_| Ok(Vec::new()));
    assert!(matches!(
        decorated.open_retained_host_v2(),
        Err(crate::PluginError::InvocationCancelled)
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let rebound = bound
        .clone()
        .with_retained_host_v2(|| Err(crate::PluginError::InvocationCancelled));
    assert_ne!(bound, rebound);
}

#[tokio::test]
async fn from_bound_preserves_selected_v2_and_origin_admission_without_v1_fallback() {
    use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
    struct Selected(Arc<AtomicUsize>);
    impl crate::HostServiceDispatch for Selected {
        fn invoke(
            &self,
            _: &bmux_plugin_runtime::InvocationScope,
            _: crate::HostServiceRequest<'_>,
        ) -> crate::Result<Vec<u8>> {
            panic!("retained open cannot invoke service fallback")
        }
        fn open_retained_host(
            &self,
            _: &bmux_plugin_runtime::InvocationScope,
        ) -> crate::Result<crate::abi::retained::RetainedHostPort> {
            panic!("selected v2 cannot downgrade")
        }
        fn open_retained_host_v2(
            &self,
            _: &bmux_plugin_runtime::InvocationScope,
        ) -> crate::Result<crate::abi::retained::RetainedHostPort> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(crate::PluginError::InvocationCancelled)
        }
    }
    let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()));
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let services = PluginHostServices::from_bound(crate::BoundHostServicePort::new(
        invocation.clone(),
        Arc::new(Selected(Arc::clone(&attempts))),
    ));
    assert!(matches!(
        services.open_retained_host_v2(),
        Err(crate::PluginError::InvocationCancelled)
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    invocation.close().unwrap();
    assert!(matches!(
        services.open_retained_host_v2(),
        Err(crate::PluginError::ServiceProtocol { .. })
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    invocation.drain().await.unwrap();
    invocation.finish().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
}
