//! `TypedDispatchClient` — consumer-side abstraction over an IPC endpoint
//! that can dispatch typed service invocations to plugins.
//!
//! bmux's core `bmux_client` knows how to send [`bmux_ipc::Request::InvokeService`]
//! over a bytestream and return the server's [`bmux_ipc::ResponsePayload::ServiceInvoked`]
//! payload. Plugins that expose typed clients (e.g. `WindowsCommandsClient`,
//! `ContextsQueryClient`) need that same capability but should not be
//! coupled to a specific transport.
//!
//! This trait factors out the single needed operation — "given a
//! capability, interface, operation, and encoded payload, give me the
//! encoded response" — so typed-client helpers can live in
//! `_plugin_api` crates and accept any `C: TypedDispatchClient`
//! (`bmux_client` in production, a test fake in unit tests, a mock for
//! examples).
//!
//! The trait is intentionally narrow. Request-id bookkeeping, timeouts,
//! protocol negotiation, and envelope encoding are implementation
//! details of the underlying transport; this trait sees only bytes in
//! and bytes out.

use std::future::Future;

use bmux_ipc::{ErrorClass, ErrorCode, InvokeServiceKind};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    CapabilityId, InterfaceId, OperationId, PluginError, ServiceInterfaceDescriptor, ServiceKind,
};

/// Why a typed-dispatch transport failed, kept so callers can tell a slow
/// peer from a dead connection without parsing the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailure {
    /// The request was sent but no reply arrived in time (`after` this long,
    /// when the transport knows). The connection may still be usable; the
    /// request's outcome is unknown.
    Timeout { after: Option<std::time::Duration> },
    /// The host could not run the request now (capacity exhausted, executor
    /// gone, response lost). The request itself was not rejected.
    Unavailable,
    /// The connection failed with this I/O error kind (EOF, reset, broken
    /// pipe, reader gone).
    ConnectionLost(std::io::ErrorKind),
    /// The peers disagree about the wire format (encoding, framing, envelope).
    Protocol,
    /// Anything else (local configuration, an unclassified failure).
    Other,
}

impl TransportFailure {
    /// The generic class of this transport failure.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::Timeout { .. } | Self::Unavailable => ErrorClass::Transient,
            Self::ConnectionLost(_) => ErrorClass::ConnectionLost,
            Self::Protocol => ErrorClass::Protocol,
            Self::Other => ErrorClass::Local,
        }
    }
}

/// Errors returned by [`TypedDispatchClient::invoke_service_raw`].
///
/// Every concrete transport (`bmux_client`, test fakes) maps its own
/// error taxonomy into this narrow enum so typed-client helpers can
/// surface a single stable error type to callers. The structured `failure`
/// and `code` fields keep the failure's kind; `details` is for humans.
#[derive(Debug, thiserror::Error)]
pub enum TypedDispatchClientError {
    /// The transport failed (connection reset, timeout, serialization).
    #[error("transport failure invoking {interface}/{operation}: {details}")]
    Transport {
        interface: String,
        operation: String,
        failure: TransportFailure,
        details: String,
    },
    /// The server returned a protocol-level error for the invocation.
    #[error("server rejected {interface}/{operation}: {details}")]
    Server {
        interface: String,
        operation: String,
        /// The code the server reported.
        code: ErrorCode,
        details: String,
    },
    /// The server returned a response of an unexpected shape (not
    /// `ServiceInvoked`).
    #[error("unexpected response invoking {interface}/{operation}: {details}")]
    UnexpectedResponse {
        interface: String,
        operation: String,
        details: String,
    },
}

impl TypedDispatchClientError {
    /// A transport failure of unknown kind ([`TransportFailure::Other`]).
    #[must_use]
    pub fn transport(
        interface: impl Into<String>,
        operation: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Self::transport_failure(interface, operation, TransportFailure::Other, details)
    }

    /// A transport failure of a known kind.
    #[must_use]
    pub fn transport_failure(
        interface: impl Into<String>,
        operation: impl Into<String>,
        failure: TransportFailure,
        details: impl Into<String>,
    ) -> Self {
        Self::Transport {
            interface: interface.into(),
            operation: operation.into(),
            failure,
            details: details.into(),
        }
    }

    /// A host-side call failure, classified by its [`PluginError`] variant
    /// (a host that gave up waiting is transient, not an unknown failure).
    #[must_use]
    pub fn from_host_error(
        interface: impl Into<String>,
        operation: impl Into<String>,
        error: &PluginError,
    ) -> Self {
        let failure = match error {
            PluginError::HostTimeout { .. } => TransportFailure::Timeout { after: None },
            PluginError::HostUnavailable { .. } => TransportFailure::Unavailable,
            _ => TransportFailure::Other,
        };
        Self::transport_failure(interface, operation, failure, error.to_string())
    }

    /// A server rejection without a known code ([`ErrorCode::Internal`]).
    #[must_use]
    pub fn server(
        interface: impl Into<String>,
        operation: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Self::server_code(interface, operation, ErrorCode::Internal, details)
    }

    /// A server rejection carrying the server's code.
    #[must_use]
    pub fn server_code(
        interface: impl Into<String>,
        operation: impl Into<String>,
        code: ErrorCode,
        details: impl Into<String>,
    ) -> Self {
        Self::Server {
            interface: interface.into(),
            operation: operation.into(),
            code,
            details: details.into(),
        }
    }

    #[must_use]
    pub fn unexpected_response(
        interface: impl Into<String>,
        operation: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Self::UnexpectedResponse {
            interface: interface.into(),
            operation: operation.into(),
            details: details.into(),
        }
    }

    /// The generic class of this failure.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Transport { failure, .. } => failure.class(),
            Self::Server { code, .. } => code.class(),
            Self::UnexpectedResponse { .. } => ErrorClass::Protocol,
        }
    }
}

impl TypedServiceClientError {
    /// The generic class of this failure. Encoding a request is a local
    /// failure; decoding a response is a protocol disagreement.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Dispatch(error) => error.class(),
            Self::Encode { .. } => ErrorClass::Local,
            Self::Decode { .. } => ErrorClass::Protocol,
        }
    }
}

/// Result alias for typed-dispatch-client operations.
pub type TypedDispatchClientResult<T> = std::result::Result<T, TypedDispatchClientError>;

/// Errors returned by generated typed service clients.
#[derive(Debug, thiserror::Error)]
pub enum TypedServiceClientError {
    /// The underlying dispatch transport failed.
    #[error(transparent)]
    Dispatch(#[from] TypedDispatchClientError),
    /// Request encoding failed.
    #[error("failed to encode {op}: {details}")]
    Encode { op: String, details: String },
    /// Response decoding failed.
    #[error("failed to decode {op}: {details}")]
    Decode { op: String, details: String },
}

/// Result alias for generated typed service clients.
pub type TypedServiceClientResult<T> = std::result::Result<T, TypedServiceClientError>;

/// Compile-time description of one typed service endpoint.
pub trait TypedServiceEndpoint {
    type Request: Serialize + Sync;
    type Response: DeserializeOwned;

    const CAPABILITY: CapabilityId;
    const KIND: InvokeServiceKind;
    const INTERFACE_ID: InterfaceId;
    const SERVICE_CONTRACT: &'static [u8] = &[];
    const OPERATION: OperationId;
    /// Whether this operation requires request-scoped ordered event delivery.
    const STREAMING: bool = false;

    /// Interface-level service descriptor for this endpoint.
    ///
    /// # Panics
    ///
    /// Panics if an endpoint declares the event kind. Generated BPDL request
    /// endpoints are query/command only, so this indicates a hand-written
    /// invalid endpoint implementation.
    #[must_use]
    fn service_descriptor() -> ServiceInterfaceDescriptor {
        let kind = match Self::KIND {
            InvokeServiceKind::Query => ServiceKind::Query,
            InvokeServiceKind::Command => ServiceKind::Command,
        };
        ServiceInterfaceDescriptor {
            capability: Self::CAPABILITY,
            kind,
            interface_id: Self::INTERFACE_ID,
            contract: Self::SERVICE_CONTRACT,
            // Endpoint descriptors address one caller-side operation; the
            // provider's reentrant set travels in its generated declaration.
            reentrant_operations: &[],
            // Access classes likewise travel in the provider's declaration.
            access: None,
            operation_access: &[],
        }
    }
}

/// Compile-time description of one request-scoped streaming typed endpoint.
pub trait StreamingTypedServiceEndpoint: TypedServiceEndpoint {
    /// Typed event frame emitted zero or more times before the final response.
    type Event: DeserializeOwned + 'static;
}

/// Invoke a generated streaming endpoint with an explicit typed sink and live
/// cancellation. Unsupported transports reject before sending the request.
///
/// # Errors
/// Returns encoding, event delivery, cancellation, transport or final decoding errors.
pub async fn invoke_typed_streaming_service<C, E>(
    client: &mut C,
    request: &E::Request,
    events: crate::service::TypedServiceEventSink<E::Event>,
    cancellation: crate::ServiceCancellation,
) -> TypedServiceClientResult<E::Response>
where
    C: TypedDispatchClient,
    E: StreamingTypedServiceEndpoint,
{
    let operation_id = E::OPERATION;
    let operation = operation_id.as_str();
    if !client.supports_service_streaming() {
        return Err(TypedDispatchClientError::unexpected_response(
            E::INTERFACE_ID.as_str(),
            operation,
            "request-scoped streaming is not supported",
        )
        .into());
    }
    let payload =
        crate::encode_service_message(request).map_err(|err| TypedServiceClientError::Encode {
            op: operation.to_string(),
            details: err.to_string(),
        })?;
    let checked = client.requires_service_contracts();
    let payload = if checked {
        crate::service_contract::encode(E::INTERFACE_ID.as_str(), E::SERVICE_CONTRACT, &payload)
            .map_err(|err| TypedServiceClientError::Encode {
                op: operation.to_string(),
                details: err.to_string(),
            })?
    } else {
        payload
    };
    let invocation_id = crate::PluginInvocationId::new();
    let expected_id = invocation_id.clone();
    let delivery_cancellation = cancellation.clone();
    let sink = crate::ServiceEventSinkHandle::new(TypedClientEventSink(
        move |event: crate::ServiceEvent| {
            if delivery_cancellation.is_cancelled() || event.invocation_id != expected_id {
                return Err(crate::PluginError::ServiceProtocol {
                    details: "cancelled or mismatched service event invocation".to_string(),
                });
            }
            let event = crate::decode_service_message::<E::Event>(&event.payload)?;
            events.emit(event)
        },
    ));
    if cancellation.is_cancelled() {
        return Err(TypedDispatchClientError::transport(
            E::INTERFACE_ID.as_str(),
            operation,
            "service stream cancelled",
        )
        .into());
    }
    let (context, completion) = crate::service::ServiceStreamContext {
        invocation_id,
        cancellation: cancellation.clone(),
        events: sink,
    }
    .scoped();
    let response = client
        .invoke_service_stream_raw(
            E::CAPABILITY.as_str(),
            E::KIND,
            E::INTERFACE_ID.as_str(),
            operation,
            payload,
            context,
        )
        .await?;
    completion.finish().map_err(|err| {
        TypedDispatchClientError::transport(E::INTERFACE_ID.as_str(), operation, err.to_string())
    })?;
    if cancellation.is_cancelled() {
        return Err(TypedDispatchClientError::transport(
            E::INTERFACE_ID.as_str(),
            operation,
            "service stream cancelled",
        )
        .into());
    }
    let response = if checked {
        crate::service_contract::decode_response(&response).map_err(|err| {
            TypedServiceClientError::Decode {
                op: operation.to_string(),
                details: err.to_string(),
            }
        })?
    } else {
        &response
    };
    crate::decode_service_message(response).map_err(|err| TypedServiceClientError::Decode {
        op: operation.to_string(),
        details: err.to_string(),
    })
}

struct TypedClientEventSink<F>(F);

impl<F> crate::ServiceEventSink for TypedClientEventSink<F>
where
    F: Fn(crate::ServiceEvent) -> crate::Result<()> + Send + Sync,
{
    fn emit_event(&self, event: crate::ServiceEvent) -> crate::Result<()> {
        (self.0)(event)
    }
}

/// Invoke a generated typed service endpoint.
///
/// # Errors
///
/// Returns transport, request encoding, or response decoding failures.
pub async fn invoke_typed_service<C, E>(
    client: &mut C,
    request: &E::Request,
) -> TypedServiceClientResult<E::Response>
where
    C: TypedDispatchClient,
    E: TypedServiceEndpoint,
{
    let operation_id = E::OPERATION;
    let operation = operation_id.as_str();
    let payload =
        crate::encode_service_message(request).map_err(|err| TypedServiceClientError::Encode {
            op: operation.to_string(),
            details: err.to_string(),
        })?;
    let checked = client.requires_service_contracts();
    let payload = if checked {
        crate::service_contract::encode(E::INTERFACE_ID.as_str(), E::SERVICE_CONTRACT, &payload)
            .map_err(|err| TypedServiceClientError::Encode {
                op: operation.to_string(),
                details: err.to_string(),
            })?
    } else {
        payload
    };
    let response = client
        .invoke_service_raw(
            E::CAPABILITY.as_str(),
            E::KIND,
            E::INTERFACE_ID.as_str(),
            operation,
            payload,
        )
        .await?;
    let response = if checked {
        crate::service_contract::decode_response(&response).map_err(|err| {
            TypedServiceClientError::Decode {
                op: operation.to_string(),
                details: err.to_string(),
            }
        })?
    } else {
        &response
    };
    crate::decode_service_message(response).map_err(|err| TypedServiceClientError::Decode {
        op: operation.to_string(),
        details: err.to_string(),
    })
}

/// A consumer-side transport that can dispatch typed service
/// invocations to plugins via the kernel IPC boundary.
///
/// Implementations own whatever state is needed for the transport
/// (connections, request ids, timeouts). Typed-client helpers in
/// `_plugin_api` crates accept `&mut C: TypedDispatchClient` and use
/// [`invoke_service_raw`](TypedDispatchClient::invoke_service_raw) to
/// round-trip encoded payloads.
///
/// The trait uses a native async method (`async fn`) whose returned
/// future is `Send`. This is sufficient for typed-client helpers that
/// use concrete generics (`async fn foo<C: TypedDispatchClient>(&mut C)`);
/// callers that need `dyn TypedDispatchClient` object safety should
/// box an adapter.
pub trait TypedDispatchClient: Send {
    /// Whether generated typed calls must use negotiated checked BPDL frames.
    fn requires_service_contracts(&self) -> bool {
        false
    }

    /// Explicit negotiation for request-scoped ordered events plus final response.
    /// Advertising support requires implementing `invoke_service_stream_raw` without
    /// dropping events, with bounded delivery and live cancellation propagation.
    fn supports_service_streaming(&self) -> bool {
        false
    }

    /// Dispatch one streaming invocation. Rejection is the default, never unary fallback.
    ///
    /// The context identity must survive retries/response growth. The transport
    /// must finish all event callbacks before returning the single terminal result,
    /// propagate sink failure, and stop new delivery after cancellation or return.
    /// Existing caller provenance and deadlines must be inherited by this call.
    fn invoke_service_stream_raw(
        &mut self,
        _capability: &str,
        _kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        _payload: Vec<u8>,
        _context: crate::service::ServiceStreamContext,
    ) -> impl Future<Output = TypedDispatchClientResult<Vec<u8>>> + Send {
        let error = TypedDispatchClientError::unexpected_response(
            interface_id,
            operation,
            "request-scoped streaming is not supported",
        );
        async move { Err(error) }
    }

    /// Dispatch an encoded typed invocation and return the encoded response.
    ///
    /// # Errors
    ///
    /// Returns [`TypedDispatchClientError`] on transport, server, or
    /// protocol-shape failures. Payload decode/encode is the caller's
    /// responsibility (typed-client helpers do it via serde).
    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = TypedDispatchClientResult<Vec<u8>>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingDispatch {
        last_capability: Option<String>,
        last_interface: Option<String>,
        last_operation: Option<String>,
        last_kind: Option<InvokeServiceKind>,
        reply: Vec<u8>,
    }

    impl TypedDispatchClient for RecordingDispatch {
        fn invoke_service_raw(
            &mut self,
            capability: &str,
            kind: InvokeServiceKind,
            interface_id: &str,
            operation: &str,
            _payload: Vec<u8>,
        ) -> impl std::future::Future<Output = TypedDispatchClientResult<Vec<u8>>> {
            std::future::poll_fn(move |_| {
                std::task::Poll::Ready({
                    self.last_capability = Some(capability.to_string());
                    self.last_kind = Some(kind);
                    self.last_interface = Some(interface_id.to_string());
                    self.last_operation = Some(operation.to_string());
                    Ok(self.reply.clone())
                })
            })
        }
    }

    #[tokio::test]
    async fn test_fake_round_trips_encoded_payload() {
        let mut client = RecordingDispatch {
            last_capability: None,
            last_interface: None,
            last_operation: None,
            last_kind: None,
            reply: b"pong".to_vec(),
        };
        let out = client
            .invoke_service_raw(
                "bmux.cap",
                InvokeServiceKind::Query,
                "iface/v1",
                "op",
                b"ping".to_vec(),
            )
            .await
            .expect("round trip");
        assert_eq!(out, b"pong");
        assert_eq!(client.last_capability.as_deref(), Some("bmux.cap"));
        assert_eq!(client.last_interface.as_deref(), Some("iface/v1"));
        assert_eq!(client.last_operation.as_deref(), Some("op"));
        assert!(matches!(client.last_kind, Some(InvokeServiceKind::Query)));
    }

    struct CheckedEndpoint;

    impl TypedServiceEndpoint for CheckedEndpoint {
        type Request = u32;
        type Response = u32;
        const CAPABILITY: CapabilityId = CapabilityId::from_static("example.read");
        const KIND: InvokeServiceKind = InvokeServiceKind::Query;
        const INTERFACE_ID: InterfaceId = InterfaceId::from_static("state/v1");
        const OPERATION: OperationId = OperationId::from_static("get");
        const SERVICE_CONTRACT: &'static [u8] = &[7; 32];
    }

    struct CheckedDispatch;

    impl TypedDispatchClient for CheckedDispatch {
        fn requires_service_contracts(&self) -> bool {
            true
        }

        fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: InvokeServiceKind,
            _interface_id: &str,
            _operation: &str,
            payload: Vec<u8>,
        ) -> impl std::future::Future<Output = TypedDispatchClientResult<Vec<u8>>> {
            std::future::poll_fn(move |_| {
                std::task::Poll::Ready({
                    let request = crate::service_contract::decode_for(
                        CheckedEndpoint::INTERFACE_ID.as_str(),
                        CheckedEndpoint::SERVICE_CONTRACT,
                        &payload,
                    )
                    .unwrap();
                    let number: u32 = crate::decode_service_message(request).unwrap();
                    Ok(crate::service_contract::encode_response(
                        &crate::encode_service_message(&(number + 1)).unwrap(),
                    ))
                })
            })
        }
    }

    #[tokio::test]
    async fn typed_checked_endpoint_binds_request_and_decodes_acceptance() {
        assert_eq!(
            invoke_typed_service::<_, CheckedEndpoint>(&mut CheckedDispatch, &41)
                .await
                .unwrap(),
            42
        );
    }

    struct StreamEndpoint;
    impl TypedServiceEndpoint for StreamEndpoint {
        type Request = u32;
        type Response = u32;
        const CAPABILITY: CapabilityId = CapabilityId::from_static("example.write");
        const KIND: InvokeServiceKind = InvokeServiceKind::Command;
        const INTERFACE_ID: InterfaceId = InterfaceId::from_static("stream/v1");
        const OPERATION: OperationId = OperationId::from_static("run");
        const SERVICE_CONTRACT: &'static [u8] = &[9; 32];
    }
    impl StreamingTypedServiceEndpoint for StreamEndpoint {
        type Event = u32;
    }

    struct StreamDispatch;
    impl TypedDispatchClient for StreamDispatch {
        fn requires_service_contracts(&self) -> bool {
            true
        }
        fn supports_service_streaming(&self) -> bool {
            true
        }
        fn invoke_service_raw(
            &mut self,
            _: &str,
            _: InvokeServiceKind,
            _: &str,
            _: &str,
            _: Vec<u8>,
        ) -> impl Future<Output = TypedDispatchClientResult<Vec<u8>>> + Send {
            std::future::poll_fn(|_| panic!("stream must not use unary dispatch"))
        }
        fn invoke_service_stream_raw(
            &mut self,
            _: &str,
            _: InvokeServiceKind,
            interface: &str,
            _: &str,
            payload: Vec<u8>,
            context: crate::service::ServiceStreamContext,
        ) -> impl Future<Output = TypedDispatchClientResult<Vec<u8>>> + Send {
            let request =
                crate::service_contract::decode_for(interface, &[9; 32], &payload).unwrap();
            let value: u32 = crate::decode_service_message(request).unwrap();
            for event in [value, value + 1] {
                // Deliberately ignore sink errors: the client guard must still
                // prevent a success after a consumer rejects an event.
                let _ = context.events.emit_event(crate::ServiceEvent {
                    invocation_id: context.invocation_id.clone(),
                    payload: crate::encode_service_message(&event).unwrap(),
                });
            }
            std::future::ready(Ok(crate::service_contract::encode_response(
                &crate::encode_service_message(&(value + 2)).unwrap(),
            )))
        }
    }

    #[tokio::test]
    async fn streaming_delivers_ordered_events_then_checked_terminal() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let sink = crate::service::TypedServiceEventSink::new(move |event| {
            captured.lock().unwrap().push(event);
            Ok(())
        });
        let result = invoke_typed_streaming_service::<_, StreamEndpoint>(
            &mut StreamDispatch,
            &40,
            sink,
            crate::ServiceCancellation::new(),
        )
        .await
        .unwrap();
        assert_eq!(*events.lock().unwrap(), vec![40, 41]);
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn streaming_rejection_and_cancellation_do_not_become_success() {
        let sink = || {
            crate::service::TypedServiceEventSink::new(|_: u32| {
                Err(crate::PluginError::ServiceProtocol {
                    details: "consumer full".to_string(),
                })
            })
        };
        assert!(
            invoke_typed_streaming_service::<_, StreamEndpoint>(
                &mut StreamDispatch,
                &1,
                sink(),
                crate::ServiceCancellation::new(),
            )
            .await
            .is_err()
        );
        let cancellation = crate::ServiceCancellation::new();
        cancellation.cancel();
        assert!(
            invoke_typed_streaming_service::<_, StreamEndpoint>(
                &mut StreamDispatch,
                &1,
                sink(),
                cancellation,
            )
            .await
            .is_err()
        );
        assert!(
            invoke_typed_streaming_service::<_, StreamEndpoint>(
                &mut CheckedDispatch,
                &1,
                sink(),
                crate::ServiceCancellation::new(),
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn error_constructors_carry_context() {
        let err = TypedDispatchClientError::transport("if", "op", "eof");
        let msg = err.to_string();
        assert!(msg.contains("if/op"), "{msg}");
        assert!(msg.contains("eof"), "{msg}");
    }

    #[test]
    fn error_classes_follow_structured_kind() {
        use std::io::ErrorKind;
        let transport =
            |failure| TypedDispatchClientError::transport_failure("if", "op", failure, "x");
        assert_eq!(
            transport(TransportFailure::Timeout { after: None }).class(),
            ErrorClass::Transient
        );
        assert_eq!(
            transport(TransportFailure::Unavailable).class(),
            ErrorClass::Transient
        );
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::ConnectionReset,
            ErrorKind::BrokenPipe,
        ] {
            assert_eq!(
                transport(TransportFailure::ConnectionLost(kind)).class(),
                ErrorClass::ConnectionLost
            );
        }
        assert_eq!(
            transport(TransportFailure::Protocol).class(),
            ErrorClass::Protocol
        );
        assert_eq!(
            TypedDispatchClientError::transport("if", "op", "x").class(),
            ErrorClass::Local
        );
        let rejected =
            TypedDispatchClientError::server_code("if", "op", ErrorCode::NotFound, "gone");
        assert_eq!(rejected.class(), ErrorClass::Rejected);
        assert!(matches!(
            rejected,
            TypedDispatchClientError::Server {
                code: ErrorCode::NotFound,
                ..
            }
        ));
        assert_eq!(
            TypedDispatchClientError::server_code("if", "op", ErrorCode::Unavailable, "busy")
                .class(),
            ErrorClass::Transient
        );
        assert!(matches!(
            TypedDispatchClientError::server("if", "op", "x"),
            TypedDispatchClientError::Server {
                code: ErrorCode::Internal,
                ..
            }
        ));
        assert_eq!(
            TypedDispatchClientError::unexpected_response("if", "op", "x").class(),
            ErrorClass::Protocol
        );
        assert_eq!(
            TypedServiceClientError::Decode {
                op: "op".into(),
                details: "x".into()
            }
            .class(),
            ErrorClass::Protocol
        );
    }

    #[test]
    fn host_errors_keep_their_wait_classification() {
        let timeout = TypedDispatchClientError::from_host_error(
            "if",
            "op",
            &PluginError::HostTimeout {
                details: "slot".into(),
            },
        );
        assert_eq!(timeout.class(), ErrorClass::Transient);
        assert!(timeout.to_string().contains("host timed out: slot"));
        let other = TypedDispatchClientError::from_host_error(
            "if",
            "op",
            &PluginError::ServiceProtocol {
                details: "bad".into(),
            },
        );
        assert_eq!(other.class(), ErrorClass::Local);
    }
}
