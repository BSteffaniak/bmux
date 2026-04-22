//! Typed-client helpers for the `bmux.snapshot` plugin.
//!
//! Free functions accepting any `C: TypedDispatchClient` that wrap
//! the `snapshot-commands::dispatch(SnapshotRequest) -> SnapshotResponse`
//! typed service call so callers don't have to repeat the
//! interface/operation strings + serde boilerplate.

use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};

use crate::{
    SNAPSHOT_COMMANDS_INTERFACE, SNAPSHOT_READ, SNAPSHOT_WRITE, SnapshotRequest, SnapshotResponse,
    SnapshotStatusPayload,
};

/// Errors returned by snapshot-plugin typed-client helpers.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotTypedClientError {
    /// Error from the underlying typed dispatch layer.
    #[error(transparent)]
    Dispatch(#[from] TypedDispatchClientError),
    /// Request encode failure.
    #[error("failed to encode snapshot request: {0}")]
    Encode(String),
    /// Response decode failure.
    #[error("failed to decode snapshot response: {0}")]
    Decode(String),
    /// Server reported an operation-level failure.
    #[error("snapshot operation '{code}' failed: {message}")]
    Operation { code: String, message: String },
    /// Received a response variant incompatible with the requested operation.
    #[error("unexpected snapshot response variant for op '{op}'")]
    UnexpectedResponse { op: &'static str },
}

type Result<T> = core::result::Result<T, SnapshotTypedClientError>;

async fn dispatch<C: TypedDispatchClient>(
    client: &mut C,
    capability: &str,
    request: SnapshotRequest,
) -> Result<SnapshotResponse> {
    let payload =
        bmux_ipc::encode(&request).map_err(|e| SnapshotTypedClientError::Encode(e.to_string()))?;
    let response_bytes = client
        .invoke_service_raw(
            capability,
            InvokeServiceKind::Command,
            SNAPSHOT_COMMANDS_INTERFACE.as_str(),
            "dispatch",
            payload,
        )
        .await?;
    bmux_ipc::decode(&response_bytes).map_err(|e| SnapshotTypedClientError::Decode(e.to_string()))
}

fn bail_if_error(response: SnapshotResponse) -> Result<SnapshotResponse> {
    if let SnapshotResponse::Error { code, message } = response {
        return Err(SnapshotTypedClientError::Operation { code, message });
    }
    Ok(response)
}

/// Force an immediate snapshot save.
///
/// Returns the written file path, or `None` when snapshot persistence is
/// disabled.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn save_now<C: TypedDispatchClient>(client: &mut C) -> Result<Option<String>> {
    let response =
        bail_if_error(dispatch(client, SNAPSHOT_WRITE.as_str(), SnapshotRequest::SaveNow).await?)?;
    match response {
        SnapshotResponse::Saved { path } => Ok(path),
        _ => Err(SnapshotTypedClientError::UnexpectedResponse { op: "save_now" }),
    }
}

/// Return the snapshot plugin's current status.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn status<C: TypedDispatchClient>(client: &mut C) -> Result<SnapshotStatusPayload> {
    let response =
        bail_if_error(dispatch(client, SNAPSHOT_READ.as_str(), SnapshotRequest::Status).await?)?;
    match response {
        SnapshotResponse::Status(payload) => Ok(payload),
        _ => Err(SnapshotTypedClientError::UnexpectedResponse { op: "status" }),
    }
}

/// Decode the on-disk snapshot without applying it.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn restore_dry_run<C: TypedDispatchClient>(client: &mut C) -> Result<(bool, String)> {
    let response = bail_if_error(
        dispatch(
            client,
            SNAPSHOT_READ.as_str(),
            SnapshotRequest::RestoreDryRun,
        )
        .await?,
    )?;
    match response {
        SnapshotResponse::DryRun { ok, message } => Ok((ok, message)),
        _ => Err(SnapshotTypedClientError::UnexpectedResponse {
            op: "restore_dry_run",
        }),
    }
}

/// Clear all participant state and apply the on-disk snapshot as a
/// full replacement. Returns `(restored_plugins, failed_plugins)`.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn restore_apply<C: TypedDispatchClient>(client: &mut C) -> Result<(u64, u64)> {
    let response = bail_if_error(
        dispatch(
            client,
            SNAPSHOT_WRITE.as_str(),
            SnapshotRequest::RestoreApply,
        )
        .await?,
    )?;
    match response {
        SnapshotResponse::Applied {
            restored_plugins,
            failed_plugins,
        } => Ok((restored_plugins, failed_plugins)),
        _ => Err(SnapshotTypedClientError::UnexpectedResponse {
            op: "restore_apply",
        }),
    }
}
