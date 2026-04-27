//! Typed-client helpers for the `bmux.performance` plugin.
//!
//! Free functions accepting any `C: TypedDispatchClient` that wrap
//! the `performance-commands::dispatch(PerformanceRequest) ->
//! PerformanceResponse` typed service call so callers don't have to
//! repeat the interface/operation strings + serde boilerplate.

use bmux_ipc::{InvokeServiceKind, PerformanceRuntimeSettings};
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};

use crate::{
    PERFORMANCE_COMMANDS_INTERFACE, PERFORMANCE_READ, PERFORMANCE_WRITE, PerformanceRequest,
    PerformanceResponse,
};

/// Errors returned by performance-plugin typed-client helpers.
#[derive(Debug, thiserror::Error)]
pub enum PerformanceTypedClientError {
    #[error(transparent)]
    Dispatch(#[from] TypedDispatchClientError),
    #[error("failed to encode performance request: {0}")]
    Encode(String),
    #[error("failed to decode performance response: {0}")]
    Decode(String),
    #[error("unexpected performance response variant")]
    UnexpectedResponse,
}

type Result<T> = core::result::Result<T, PerformanceTypedClientError>;

async fn dispatch<C: TypedDispatchClient>(
    client: &mut C,
    capability: &str,
    request: PerformanceRequest,
) -> Result<PerformanceResponse> {
    let payload = bmux_ipc::encode(&request)
        .map_err(|e| PerformanceTypedClientError::Encode(e.to_string()))?;
    let response_bytes = client
        .invoke_service_raw(
            capability,
            InvokeServiceKind::Command,
            PERFORMANCE_COMMANDS_INTERFACE.as_str(),
            "dispatch",
            payload,
        )
        .await?;
    bmux_ipc::decode(&response_bytes)
        .map_err(|e| PerformanceTypedClientError::Decode(e.to_string()))
}

/// Retrieve runtime performance telemetry settings.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn performance_status<C: TypedDispatchClient>(
    client: &mut C,
) -> Result<PerformanceRuntimeSettings> {
    let response = dispatch(
        client,
        PERFORMANCE_READ.as_str(),
        PerformanceRequest::GetSettings,
    )
    .await?;
    match response {
        PerformanceResponse::Settings { settings } => Ok(settings),
        _ => Err(PerformanceTypedClientError::UnexpectedResponse),
    }
}

/// Update runtime performance telemetry settings.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn performance_set<C: TypedDispatchClient>(
    client: &mut C,
    settings: PerformanceRuntimeSettings,
) -> Result<PerformanceRuntimeSettings> {
    let response = dispatch(
        client,
        PERFORMANCE_WRITE.as_str(),
        PerformanceRequest::SetSettings { settings },
    )
    .await?;
    match response {
        PerformanceResponse::Settings { settings } => Ok(settings),
        _ => Err(PerformanceTypedClientError::UnexpectedResponse),
    }
}
