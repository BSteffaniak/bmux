//! Owned asynchronous dispatch through the selected authenticated host port.
//! Worker admission and scheduling belong to the host; no synchronous fallback
//! or retry is authorized by moving a callback onto a different thread.
use super::process_runtime::ProcessCaller;
use bmux_plugin::ServiceCaller;
use bmux_plugin_sdk::{
    HostServiceOwnedRequest, ServiceKind, TypedDispatchClient, TypedDispatchClientError,
};

#[cfg(test)]
#[path = "process_dispatch_tests.rs"]
mod tests;

pub struct ProcessDispatchClient {
    caller: ProcessCaller,
}

impl ProcessDispatchClient {
    pub(super) fn new(caller: &ProcessCaller) -> Self {
        Self {
            caller: caller.clone(),
        }
    }
}

impl TypedDispatchClient for ProcessDispatchClient {
    fn requires_service_contracts(&self) -> bool {
        true
    }

    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: bmux_ipc::InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, TypedDispatchClientError>> + Send {
        let caller = self.caller.clone();
        let request = HostServiceOwnedRequest {
            capability: capability.to_owned(),
            kind: match kind {
                bmux_ipc::InvokeServiceKind::Query => ServiceKind::Query,
                bmux_ipc::InvokeServiceKind::Command => ServiceKind::Command,
            },
            interface: interface_id.to_owned(),
            operation: operation.to_owned(),
            payload,
        };
        async move {
            let interface = request.interface.clone();
            let operation = request.operation.clone();
            caller.call_service_async(request).await.map_err(|error| {
                TypedDispatchClientError::transport(&interface, &operation, error.to_string())
            })
        }
    }
}
