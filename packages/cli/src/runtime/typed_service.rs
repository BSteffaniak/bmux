//! Shared primitives for dispatching typed service operations over the
//! generic `Request::InvokeService` envelope.
//!
//! Each domain plugin (windows, sessions, contexts, clients) has a
//! thin wrapper in `typed_<domain>.rs` that encodes operation-specific
//! argument types and calls through to [`invoke_with`]. This module
//! contains the parts that don't vary between domains: the error type,
//! the serialize → wire → deserialize flow, and the trait-object shim
//! that lets helpers work against either [`bmux_client::BmuxClient`]
//! or [`bmux_client::StreamingBmuxClient`] without having a shared
//! trait.

use bmux_codec::{from_bytes, to_vec};
use serde::{Deserialize, Serialize};

/// Errors returned by [`invoke_with`].
#[derive(Debug)]
pub enum InvokeError {
    /// Serializing the typed arg struct to the wire format failed.
    Encode { operation: String, message: String },
    /// Deserializing the typed response from the wire format failed.
    Decode { operation: String, message: String },
    /// The client transport returned an error before the response was
    /// received.
    Client(String),
}

impl std::fmt::Display for InvokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode { operation, message } => {
                write!(f, "encoding {operation} args failed: {message}")
            }
            Self::Decode { operation, message } => {
                write!(f, "decoding {operation} response failed: {message}")
            }
            Self::Client(message) => write!(f, "client transport failed: {message}"),
        }
    }
}

impl std::error::Error for InvokeError {}

/// Encode `args`, hand the bytes to `invoke`, and decode the response
/// into `Resp`.
///
/// Used by each `typed_<domain>.rs` helper to keep the
/// serialize → transport → deserialize flow consistent and centralise
/// the error mapping.
#[allow(clippy::future_not_send)]
pub async fn invoke_with<F, Fut, Req, Resp>(
    operation: &str,
    args: &Req,
    invoke: F,
) -> Result<Resp, InvokeError>
where
    Req: Serialize + Sync,
    Resp: for<'de> Deserialize<'de>,
    F: FnOnce(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    let payload = to_vec(args).map_err(|source| InvokeError::Encode {
        operation: operation.to_string(),
        message: source.to_string(),
    })?;
    let response_bytes = invoke(payload).await.map_err(InvokeError::Client)?;
    from_bytes::<Resp>(&response_bytes).map_err(|source| InvokeError::Decode {
        operation: operation.to_string(),
        message: source.to_string(),
    })
}
