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

use bmux_ipc::InvokeServiceKind;

/// Errors returned by [`TypedDispatchClient::invoke_service_raw`].
///
/// Every concrete transport (`bmux_client`, test fakes) maps its own
/// error taxonomy into this narrow enum so typed-client helpers can
/// surface a single stable error type to callers.
#[derive(Debug, thiserror::Error)]
pub enum TypedDispatchClientError {
    /// The transport failed (connection reset, timeout, serialization).
    #[error("transport failure invoking {interface}/{operation}: {details}")]
    Transport {
        interface: String,
        operation: String,
        details: String,
    },
    /// The server returned a protocol-level error for the invocation.
    #[error("server rejected {interface}/{operation}: {details}")]
    Server {
        interface: String,
        operation: String,
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
    #[must_use]
    pub fn transport(
        interface: impl Into<String>,
        operation: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Self::Transport {
            interface: interface.into(),
            operation: operation.into(),
            details: details.into(),
        }
    }

    #[must_use]
    pub fn server(
        interface: impl Into<String>,
        operation: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Self::Server {
            interface: interface.into(),
            operation: operation.into(),
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
}

/// Result alias for typed-dispatch-client operations.
pub type TypedDispatchClientResult<T> = std::result::Result<T, TypedDispatchClientError>;

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
        async fn invoke_service_raw(
            &mut self,
            capability: &str,
            kind: InvokeServiceKind,
            interface_id: &str,
            operation: &str,
            _payload: Vec<u8>,
        ) -> TypedDispatchClientResult<Vec<u8>> {
            self.last_capability = Some(capability.to_string());
            self.last_kind = Some(kind);
            self.last_interface = Some(interface_id.to_string());
            self.last_operation = Some(operation.to_string());
            Ok(self.reply.clone())
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

    #[test]
    fn error_constructors_carry_context() {
        let err = TypedDispatchClientError::transport("if", "op", "eof");
        let msg = err.to_string();
        assert!(msg.contains("if/op"), "{msg}");
        assert!(msg.contains("eof"), "{msg}");
    }
}
