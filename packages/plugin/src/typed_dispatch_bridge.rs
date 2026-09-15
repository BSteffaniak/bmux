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

/// Version of the bounded asynchronous service route.
pub const ASYNC_SERVICE_ROUTE_V1: u16 = 1;
const MAX_ASYNC_SERVICE_BYTES: usize = 1024 * 1024;

/// One request for the owner of an asynchronous service route to dispatch.
/// Identity and authorization belong to the issuing host context, not this payload.
pub struct AsyncServiceRequest {
    pub capability: String,
    pub kind: InvokeServiceKind,
    pub interface_id: String,
    pub operation: String,
    pub payload: Vec<u8>,
    pub response: tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>,
}

/// A captured route that never reconnects or switches to another registration.
#[derive(Debug, Clone)]
pub struct AsyncServiceClient {
    sender: tokio::sync::mpsc::Sender<AsyncServiceRequest>,
}

impl AsyncServiceClient {
    /// Bind a negotiated route. The host must authorize requests before dispatch.
    ///
    /// # Errors
    /// Rejects unsupported versions and channels larger than the route budget.
    pub fn bind(
        version: u16,
        sender: tokio::sync::mpsc::Sender<AsyncServiceRequest>,
    ) -> Result<Self, String> {
        if version != ASYNC_SERVICE_ROUTE_V1 {
            return Err("unsupported async service route version".into());
        }
        if sender.max_capacity() > 16 {
            return Err("async service route capacity exceeds 16".into());
        }
        Ok(Self { sender })
    }
}

impl TypedDispatchClient for AsyncServiceClient {
    async fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, TypedDispatchClientError> {
        let error =
            |message: &str| TypedDispatchClientError::transport(interface_id, operation, message);
        if payload.len() > MAX_ASYNC_SERVICE_BYTES
            || [capability, interface_id, operation]
                .iter()
                .any(|value| value.len() > 1024)
        {
            return Err(error("async service request exceeds size limit"));
        }
        let (response, receiver) = tokio::sync::oneshot::channel();
        self.sender
            .try_send(AsyncServiceRequest {
                capability: capability.into(),
                kind,
                interface_id: interface_id.into(),
                operation: operation.into(),
                payload,
                response,
            })
            .map_err(|failure| match failure {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    error("async service route full")
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    error("async service route closed")
                }
            })?;
        let result = receiver
            .await
            .map_err(|_| error("async service response closed"))?
            .map_err(|message| error(&message))?;
        if result.len() > MAX_ASYNC_SERVICE_BYTES {
            return Err(error("async service response exceeds size limit"));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::Result as PluginResult;

    #[tokio::test]
    async fn async_route_round_trip_and_closure() {
        let (sender, mut requests) = tokio::sync::mpsc::channel(1);
        assert!(AsyncServiceClient::bind(2, sender.clone()).is_err());
        let mut client = AsyncServiceClient::bind(1, sender).expect("version one");
        let host = tokio::spawn(async move {
            let request = requests.recv().await.expect("request");
            assert_eq!(request.capability, "test.capability");
            request.response.send(Ok(request.payload)).expect("reply");
        });
        assert_eq!(
            client
                .invoke_service_raw(
                    "test.capability",
                    InvokeServiceKind::Query,
                    "test-interface",
                    "test-op",
                    vec![1]
                )
                .await
                .expect("response"),
            vec![1]
        );
        host.await.expect("host");
        assert!(
            client
                .invoke_service_raw(
                    "test.capability",
                    InvokeServiceKind::Query,
                    "test-interface",
                    "test-op",
                    vec![]
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn async_route_rejects_full_queue_and_oversized_request() {
        let (sender, _requests) = tokio::sync::mpsc::channel(1);
        let mut client = AsyncServiceClient::bind(1, sender.clone()).expect("route");
        assert!(
            client
                .invoke_service_raw(
                    "test",
                    InvokeServiceKind::Query,
                    "test",
                    "test",
                    vec![0; MAX_ASYNC_SERVICE_BYTES + 1]
                )
                .await
                .is_err()
        );
        let (response, _receiver) = tokio::sync::oneshot::channel();
        assert!(
            sender
                .try_send(AsyncServiceRequest {
                    capability: "test".into(),
                    kind: InvokeServiceKind::Query,
                    interface_id: "test".into(),
                    operation: "test".into(),
                    payload: vec![],
                    response
                })
                .is_ok()
        );
        assert!(
            client
                .invoke_service_raw("test", InvokeServiceKind::Query, "test", "test", vec![])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn async_route_cancelled_call_closes_reply_receiver() {
        let (sender, mut requests) = tokio::sync::mpsc::channel(1);
        let mut client = AsyncServiceClient::bind(1, sender).expect("route");
        let call = tokio::spawn(async move {
            client
                .invoke_service_raw("test", InvokeServiceKind::Query, "test", "test", vec![])
                .await
        });
        let request = requests.recv().await.expect("request");
        call.abort();
        assert!(call.await.expect_err("cancelled").is_cancelled());
        assert!(request.response.is_closed());
        assert!(request.response.send(Ok(vec![])).is_err());
    }

    #[tokio::test]
    async fn async_route_old_handle_cannot_use_replacement() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let mut old = AsyncServiceClient::bind(1, sender).expect("old route");
        drop(receiver);
        let (sender, mut replacement) = tokio::sync::mpsc::channel(1);
        let _new = AsyncServiceClient::bind(1, sender).expect("new route");
        assert!(
            old.invoke_service_raw("test", InvokeServiceKind::Query, "test", "test", vec![])
                .await
                .is_err()
        );
        assert!(matches!(
            replacement.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn async_route_rejects_oversized_response() {
        let (sender, mut requests) = tokio::sync::mpsc::channel(1);
        let mut client = AsyncServiceClient::bind(1, sender).expect("route");
        let host = tokio::spawn(async move {
            let request = requests.recv().await.expect("request");
            assert!(
                request
                    .response
                    .send(Ok(vec![0; MAX_ASYNC_SERVICE_BYTES + 1]))
                    .is_ok()
            );
        });
        assert!(
            client
                .invoke_service_raw("test", InvokeServiceKind::Query, "test", "test", vec![])
                .await
                .is_err()
        );
        host.await.expect("host");
    }

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
