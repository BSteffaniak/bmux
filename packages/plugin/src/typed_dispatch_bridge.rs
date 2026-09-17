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
    response: tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>,
}

impl AsyncServiceRequest {
    /// Whether the caller has stopped waiting. Check before starting a side effect;
    /// cancellation does not undo work already dispatched.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.response.is_closed()
    }

    /// Complete a request without retaining an oversized response in the channel.
    ///
    /// # Errors
    /// Returns an error if the caller has stopped waiting.
    pub fn respond(self, response: Result<Vec<u8>, String>) -> Result<(), String> {
        let response = match response {
            Ok(bytes) if bytes.len() > MAX_ASYNC_SERVICE_BYTES => {
                Err("async service response exceeds size limit".into())
            }
            Err(message) if message.len() > MAX_ASYNC_SERVICE_BYTES => {
                Err("async service error exceeds size limit".into())
            }
            response => response,
        };
        self.response
            .send(response)
            .map_err(|_| "async service caller closed".into())
    }
}

/// A captured route that never reconnects or switches to another registration.
#[derive(Debug, Clone)]
pub struct AsyncServiceClient {
    wait_for_capacity: bool,
    sender: tokio::sync::mpsc::Sender<AsyncServiceRequest>,
    services: Option<std::sync::Arc<[bmux_plugin_sdk::RegisteredService]>>,
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
        Ok(Self {
            wait_for_capacity: false,
            sender,
            services: None,
        })
    }

    /// Opt into cancellable asynchronous queue admission for background work.
    /// Existing routes retain their fail-fast admission semantics.
    #[must_use]
    pub const fn with_backpressure(mut self) -> Self {
        self.wait_for_capacity = true;
        self
    }

    /// Restrict the route to the issuing context's service inventory.
    /// This is defense in depth; the host must still authorize dispatch.
    ///
    /// # Errors
    /// Rejects inventories larger than 1024 services.
    pub fn with_services(
        mut self,
        services: Vec<bmux_plugin_sdk::RegisteredService>,
    ) -> Result<Self, String> {
        if services.len() > 1024 {
            return Err("async service inventory exceeds limit".into());
        }
        self.services = Some(services.into());
        Ok(self)
    }
}

thread_local! {
    static ASYNC_COMMAND_ROUTE: std::cell::RefCell<Option<AsyncServiceClient>> = const { std::cell::RefCell::new(None) };
}

/// Restores the previous command route on the issuing thread.
/// Captured clients retain their original channel independently of this scope.
pub struct AsyncCommandRouteGuard {
    previous: Option<AsyncServiceClient>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for AsyncCommandRouteGuard {
    fn drop(&mut self) {
        ASYNC_COMMAND_ROUTE.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}

/// Install a host-authorized route while entering a bundled command.
#[must_use]
pub fn enter_async_command_route(client: AsyncServiceClient) -> AsyncCommandRouteGuard {
    let previous = ASYNC_COMMAND_ROUTE.with(|slot| slot.replace(Some(client)));
    AsyncCommandRouteGuard {
        previous,
        _thread_bound: std::marker::PhantomData,
    }
}

/// Capture a negotiated route before spawning background work.
///
/// # Errors
/// Rejects unsupported versions or hosts that did not install a route. Never
/// falls back to a new connection or a later command's route.
pub fn capture_async_command_route(version: u16) -> Result<AsyncServiceClient, String> {
    if version != ASYNC_SERVICE_ROUTE_V1 {
        return Err("unsupported async service route version".into());
    }
    ASYNC_COMMAND_ROUTE
        .with(|slot| slot.borrow().clone())
        .ok_or_else(|| "async command route unavailable".into())
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
        let service_kind = match kind {
            InvokeServiceKind::Query => ServiceKind::Query,
            InvokeServiceKind::Command => ServiceKind::Command,
        };
        if self.services.as_ref().is_some_and(|services| {
            !services.iter().any(|service| {
                service.capability.as_str() == capability
                    && service.kind == service_kind
                    && service.interface_id == interface_id
            })
        }) {
            return Err(error("service is outside the captured route inventory"));
        }
        let permit = if self.wait_for_capacity {
            Some(
                self.sender
                    .reserve()
                    .await
                    .map_err(|_| error("async service route closed"))?,
            )
        } else {
            None
        };
        let (response, receiver) = tokio::sync::oneshot::channel();
        let request = AsyncServiceRequest {
            capability: capability.into(),
            kind,
            interface_id: interface_id.into(),
            operation: operation.into(),
            payload,
            response,
        };
        if let Some(permit) = permit {
            permit.send(request);
        } else {
            self.sender
                .try_send(request)
                .map_err(|failure| match failure {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => {
                        error("async service route full")
                    }
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                        error("async service route closed")
                    }
                })?;
        }
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
            let payload = request.payload.clone();
            request.respond(Ok(payload)).expect("reply");
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
    async fn backpressured_route_waits_for_capacity_and_cancels_before_admission() {
        let (sender, mut requests) = tokio::sync::mpsc::channel(1);
        let (response, _receiver) = tokio::sync::oneshot::channel();
        sender
            .send(AsyncServiceRequest {
                capability: "test".into(),
                kind: InvokeServiceKind::Query,
                interface_id: "test".into(),
                operation: "occupied".into(),
                payload: vec![],
                response,
            })
            .await
            .unwrap();
        let mut client = AsyncServiceClient::bind(1, sender.clone())
            .unwrap()
            .with_backpressure();
        let mut cancelled_client = client.clone();
        let cancelled = tokio::spawn(async move {
            cancelled_client
                .invoke_service_raw(
                    "test",
                    InvokeServiceKind::Query,
                    "test",
                    "cancelled",
                    vec![],
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!cancelled.is_finished());
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        let pending = tokio::spawn(async move {
            client
                .invoke_service_raw("test", InvokeServiceKind::Query, "test", "pending", vec![])
                .await
        });
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());
        assert_eq!(requests.recv().await.unwrap().operation, "occupied");
        let request = requests.recv().await.unwrap();
        assert_eq!(request.operation, "pending");
        request.respond(Ok(vec![42])).unwrap();
        assert_eq!(pending.await.unwrap().unwrap(), vec![42]);
        assert!(requests.try_recv().is_err());
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
        assert!(request.is_cancelled());
        assert!(request.respond(Ok(vec![])).is_err());
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
                    .respond(Ok(vec![0; MAX_ASYNC_SERVICE_BYTES + 1]))
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

    #[tokio::test]
    async fn async_route_empty_inventory_denies_before_enqueue() {
        let (sender, mut requests) = tokio::sync::mpsc::channel(1);
        let mut client = AsyncServiceClient::bind(1, sender)
            .expect("route")
            .with_services(vec![])
            .expect("inventory");
        assert!(
            client
                .invoke_service_raw("test", InvokeServiceKind::Query, "test", "test", vec![])
                .await
                .is_err()
        );
        assert!(matches!(
            requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn captured_command_route_survives_scope_without_rebinding() {
        assert!(capture_async_command_route(1).is_err());
        let (sender, mut original) = tokio::sync::mpsc::channel(1);
        let mut captured = {
            let _scope =
                enter_async_command_route(AsyncServiceClient::bind(1, sender).expect("route"));
            assert!(capture_async_command_route(2).is_err());
            capture_async_command_route(1).expect("capture")
        };
        assert!(capture_async_command_route(1).is_err());
        let (sender, mut replacement) = tokio::sync::mpsc::channel(1);
        {
            let _scope = enter_async_command_route(
                AsyncServiceClient::bind(1, sender).expect("replacement"),
            );
            assert!(capture_async_command_route(1).is_ok());
        }
        let call = tokio::spawn(async move {
            captured
                .invoke_service_raw("test", InvokeServiceKind::Query, "test", "test", vec![])
                .await
        });
        original
            .recv()
            .await
            .expect("original route")
            .respond(Ok(vec![42]))
            .expect("reply");
        assert_eq!(call.await.expect("task").expect("result"), vec![42]);
        assert!(replacement.try_recv().is_err());
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
