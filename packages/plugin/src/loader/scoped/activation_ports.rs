//! Activation-owned callbacks mint a fresh invocation for every later call.
#[cfg(test)]
#[path = "activation_ports_tests.rs"]
mod tests;
use super::{ActivationScope, Arc, InvocationScope, Result, failure, scope_error};

struct CallScope {
    invocation: InvocationScope,
    owned: bool,
}
impl CallScope {
    fn new(activation: &ActivationScope, setup: Option<&InvocationScope>) -> Result<Self> {
        let (invocation, owned) = if activation.state().map_err(scope_error)?
            == bmux_plugin_runtime::LifecycleState::Activating
        {
            (
                setup
                    .cloned()
                    .ok_or_else(|| failure("activation setup port unavailable"))?,
                false,
            )
        } else {
            (activation.begin_invocation().map_err(scope_error)?, true)
        };
        Ok(Self { invocation, owned })
    }
    fn finish(mut self) -> Result<()> {
        if self.owned {
            self.invocation.close().map_err(scope_error)?;
            self.invocation.finish().map_err(scope_error)?;
            self.owned = false;
        }
        Ok(())
    }
}
impl Drop for CallScope {
    fn drop(&mut self) {
        if self.owned {
            // Future cancellation/unwind also retires the fresh caller. Failure
            // remains visible in the scope tree, never a silently reusable root.
            if let Err(error) = self
                .invocation
                .close()
                .and_then(|()| self.invocation.finish())
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
