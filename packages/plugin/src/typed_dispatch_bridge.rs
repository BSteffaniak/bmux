//! Bridge synchronous plugin service callers into typed-client helpers.
//!
//! Plugin-api crates expose typed-client helpers over
//! [`bmux_plugin_sdk::TypedDispatchClient`]. Plugin implementation code
//! often already has a synchronous [`ServiceCaller`](crate::ServiceCaller)
//! context. This module provides the tiny adapter between those two
//! generic surfaces without adding any domain-specific host API.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll};

use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{ServiceKind, TypedDispatchClient, TypedDispatchClientError};

use crate::ServiceCaller;

/// A [`TypedDispatchClient`] backed by a borrowed [`ServiceCaller`].
#[derive(Debug)]
pub struct ServiceCallerDispatchClient<'a, C: ServiceCaller + ?Sized> {
    caller: &'a C,
}

impl<'a, C: ServiceCaller + ?Sized> ServiceCallerDispatchClient<'a, C> {
    /// Create a typed-dispatch client over an existing service caller.
    #[must_use]
    pub const fn new(caller: &'a C) -> Self {
        Self { caller }
    }
}

impl<C> TypedDispatchClient for ServiceCallerDispatchClient<'_, C>
where
    C: ServiceCaller + Sync + ?Sized,
{
    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, TypedDispatchClientError>> + Send {
        let result = self
            .caller
            .call_service_raw(
                capability,
                match kind {
                    InvokeServiceKind::Query => ServiceKind::Query,
                    InvokeServiceKind::Command => ServiceKind::Command,
                },
                interface_id,
                operation,
                payload,
            )
            .map_err(|err| {
                TypedDispatchClientError::transport(interface_id, operation, err.to_string())
            });
        std::future::ready(result)
    }
}

/// Run a typed-dispatch helper future to completion on the current thread.
///
/// This is intentionally minimal: the futures produced by typed-client
/// helpers over [`ServiceCallerDispatchClient`] complete synchronously
/// because the underlying [`ServiceCaller`] API is synchronous.
///
/// # Panics
///
/// Panics if a future remains pending after being polled. That indicates
/// the caller passed a future that depends on an async runtime rather
/// than a synchronous service-caller-backed typed helper.
pub fn block_on_typed_dispatch<F: Future>(future: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("typed dispatch helper unexpectedly returned Pending"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::Result as PluginResult;

    struct FakeCaller;

    impl ServiceCaller for FakeCaller {
        fn call_service_raw(
            &self,
            _capability: &str,
            kind: ServiceKind,
            interface_id: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> PluginResult<Vec<u8>> {
            assert_eq!(kind, ServiceKind::Query);
            assert_eq!(interface_id, "test-interface");
            assert_eq!(operation, "test-op");
            Ok(payload)
        }

        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> PluginResult<bmux_ipc::ResponsePayload> {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "execute_kernel_request",
            })
        }
    }

    #[test]
    fn service_caller_dispatch_client_delegates_raw_call() {
        let caller = FakeCaller;
        let mut client = ServiceCallerDispatchClient::new(&caller);
        let response = block_on_typed_dispatch(client.invoke_service_raw(
            "test.capability",
            InvokeServiceKind::Query,
            "test-interface",
            "test-op",
            b"payload".to_vec(),
        ))
        .expect("call should succeed");

        assert_eq!(response, b"payload");
    }
}
