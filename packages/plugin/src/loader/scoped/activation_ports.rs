//! Activation-owned callbacks mint a fresh invocation for every later call.
#[cfg(test)]
#[path = "activation_ports_tests.rs"]
mod tests;
use super::{ActivationScope, Arc, InvocationScope, Result, failure, scope_error};

/// Upper bound for the setup-to-active transition a call may wait out.
const SETTLEMENT_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

struct CallScope {
    invocation: InvocationScope,
    owned: bool,
    /// Holds the activation open (undrained) until this fresh caller has
    /// finished, including a retirement completed off-thread.
    activation_admission: Option<bmux_plugin_runtime::AdmissionGuard>,
}
impl CallScope {
    fn new(activation: &ActivationScope, setup: Option<&InvocationScope>) -> Result<Self> {
        // Background work started by `activate` may call after the setup
        // invocation closed but before the loader marks the activation active
        // (both happen in the same settlement step). Wait out that bounded
        // transition instead of rejecting through the closed setup caller.
        let deadline = std::time::Instant::now() + SETTLEMENT_WAIT;
        // Subscribed before the first state check, so the setup-to-active
        // transition wakes this wait instead of being polled for.
        let mut changes = activation.changes();
        while activation.state().map_err(scope_error)?
            == bmux_plugin_runtime::LifecycleState::Activating
        {
            let setup = setup
                .cloned()
                .ok_or_else(|| failure("activation setup port unavailable"))?;
            match setup.try_admit() {
                Ok(_) => {
                    return Ok(Self {
                        invocation: setup,
                        owned: false,
                        activation_admission: None,
                    });
                }
                Err(bmux_plugin_runtime::ScopeError::Closed)
                    if std::time::Instant::now() < deadline =>
                {
                    changes.wait_blocking_until(deadline);
                }
                Err(error) => return Err(scope_error(error)),
            }
        }
        // A closed activation rejects before dispatch: no effect started.
        let rejected = |error| match error {
            bmux_plugin_runtime::ScopeError::Closed => {
                bmux_plugin_sdk::PluginError::InvocationCancelled
            }
            error => scope_error(error),
        };
        let activation_admission = activation.try_admit().map_err(rejected)?;
        let invocation = activation.begin_invocation().map_err(rejected)?;
        Ok(Self {
            invocation,
            owned: true,
            activation_admission: Some(activation_admission),
        })
    }
    /// Retire the fresh caller once its call returned. Work the call admitted
    /// (e.g. a provider's nested callback entry) may still be settling, and
    /// that settlement can need the very thread making this call, so never
    /// block on it here: retirement completes off-thread once it drains. The
    /// activation still owns the unfinished caller, so deactivation waits for
    /// it; the call's own outcome is not discarded.
    fn finish(mut self) -> Result<()> {
        if self.owned {
            self.owned = false;
            retire_or_detach(&self.invocation, self.activation_admission.take())?;
        }
        Ok(())
    }
}
fn retire_or_detach(
    invocation: &InvocationScope,
    activation_admission: Option<bmux_plugin_runtime::AdmissionGuard>,
) -> Result<()> {
    invocation.close().map_err(scope_error)?;
    match invocation.finish() {
        Ok(()) => Ok(()),
        // Poisoned or already-failed cleanup is terminal; anything else (work
        // still outstanding) settles once the closed caller drains.
        Err(
            error @ (bmux_plugin_runtime::ScopeError::Poisoned
            | bmux_plugin_runtime::ScopeError::Cleanup(_)),
        ) => Err(scope_error(error)),
        Err(_) => {
            detach_retirement(invocation.clone(), activation_admission);
            Ok(())
        }
    }
}
/// Drain then finish a closed caller without blocking the current thread.
fn detach_retirement(
    invocation: InvocationScope,
    activation_admission: Option<bmux_plugin_runtime::AdmissionGuard>,
) {
    async fn retire(
        invocation: InvocationScope,
        activation_admission: Option<bmux_plugin_runtime::AdmissionGuard>,
    ) {
        let outcome = match invocation.drain().await {
            Ok(()) => invocation.finish(),
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            let _ = invocation.fail_cleanup(format!("activation callback retirement: {error}"));
        }
        // Only now may the activation drain past this caller.
        drop(activation_admission);
    }
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(retire(invocation, activation_admission));
        return;
    }
    let failed = invocation.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("bmux-activation-call-retire".into())
        .spawn(move || {
            struct WakeThread(std::thread::Thread);
            impl std::task::Wake for WakeThread {
                fn wake(self: Arc<Self>) {
                    self.0.unpark();
                }
                fn wake_by_ref(self: &Arc<Self>) {
                    self.0.unpark();
                }
            }
            let waker = std::task::Waker::from(Arc::new(WakeThread(std::thread::current())));
            let mut context = std::task::Context::from_waker(&waker);
            let mut retirement = std::pin::pin!(retire(invocation, activation_admission));
            while retirement.as_mut().poll(&mut context).is_pending() {
                std::thread::park();
            }
        })
    {
        let _ = failed.fail_cleanup(format!(
            "activation callback retirement unavailable: {error}"
        ));
    }
}
impl Drop for CallScope {
    fn drop(&mut self) {
        if self.owned {
            // Future cancellation/unwind also retires the fresh caller, after
            // its admitted work drains. Failure remains visible in the scope
            // tree, never a silently reusable root.
            if let Err(error) = retire_or_detach(&self.invocation, self.activation_admission.take())
            {
                let _ = self
                    .invocation
                    .fail_cleanup(format!("activation callback retirement: {error}"));
            }
        }
    }
}
struct Calls {
    activation: ActivationScope,
    setup: Option<InvocationScope>,
    dispatch: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
}
impl Calls {
    fn port(&self) -> Result<(CallScope, bmux_plugin_sdk::BoundHostServicePort)> {
        let scope = CallScope::new(&self.activation, self.setup.as_ref())?;
        let port = bmux_plugin_sdk::BoundHostServicePort::new(
            scope.invocation.clone(),
            self.dispatch.clone(),
        );
        Ok((scope, port))
    }
}
pub(super) fn services(
    activation: ActivationScope,
    dispatch: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
    setup: Option<InvocationScope>,
) -> bmux_plugin_sdk::PluginHostServices {
    let calls = Arc::new(Calls {
        activation,
        setup,
        dispatch,
    });
    let synchronous = calls.clone();
    let asynchronous = calls.clone();
    let availability = calls.clone();
    let recovery = calls.clone();
    bmux_plugin_sdk::PluginHostServices::new(move |request| {
        let (scope, port) = synchronous.port()?;
        let result = port.invoke(request);
        scope.finish()?;
        result
    })
    .with_async(move |request| {
        let calls = asynchronous.clone();
        Box::pin(async move {
            let (scope, port) = calls.port()?;
            let result = port.invoke_async(request).await;
            scope.finish()?;
            result
        })
    })
    .with_service_availability(move |key| {
        let calls = availability.clone();
        Box::pin(async move {
            let (scope, port) = calls.port()?;
            let result = port.selected_service_availability(key).await;
            scope.finish()?;
            result
        })
    })
    .with_retained_host_v2(move || {
        let (scope, port) = recovery.port()?;
        let result = port.open_retained_host_v2();
        scope.finish()?;
        result
    })
    .with_retained_host(move || {
        let (scope, port) = calls.port()?;
        let result = port.open_retained_host();
        scope.finish()?;
        result
    })
}

pub(super) fn kernel(
    activation: &ActivationScope,
    setup: Option<&InvocationScope>,
    dispatch: Arc<dyn crate::InvocationKernelDispatch>,
    request: &[u8],
) -> Result<Vec<u8>> {
    let scope = CallScope::new(activation, setup)?;
    let result = crate::InvocationKernelPort::new(scope.invocation.clone(), dispatch)
        .execute_encoded(request);
    scope.finish()?;
    result
}
