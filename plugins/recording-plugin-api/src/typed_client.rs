//! Typed-client helpers for the `bmux.recording` plugin.
//!
//! Free functions accepting any `C: TypedDispatchClient` that wrap
//! the `recording-commands::dispatch(RecordingRequest) ->
//! RecordingResponse` typed service call so callers don't have to
//! repeat the interface/operation strings + serde boilerplate.

use bmux_ipc::{
    InvokeServiceKind, RecordingCaptureTarget, RecordingEventKind, RecordingProfile,
    RecordingRollingClearReport, RecordingRollingStartOptions, RecordingRollingStatus,
    RecordingStatus, RecordingSummary,
};
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};
use uuid::Uuid;

use crate::{
    RECORDING_COMMANDS_INTERFACE, RECORDING_READ, RECORDING_WRITE, RecordingRequest,
    RecordingResponse,
};

/// Errors returned by recording-plugin typed-client helpers.
#[derive(Debug, thiserror::Error)]
pub enum RecordingTypedClientError {
    #[error(transparent)]
    Dispatch(#[from] TypedDispatchClientError),
    #[error("failed to encode recording request: {0}")]
    Encode(String),
    #[error("failed to decode recording response: {0}")]
    Decode(String),
    #[error("unexpected recording response: {0}")]
    Unexpected(&'static str),
    #[error("no active recording to stop")]
    NoActiveRecording,
}

type Result<T> = core::result::Result<T, RecordingTypedClientError>;

async fn dispatch<C: TypedDispatchClient>(
    client: &mut C,
    capability: &str,
    request: RecordingRequest,
) -> Result<RecordingResponse> {
    let payload =
        bmux_ipc::encode(&request).map_err(|e| RecordingTypedClientError::Encode(e.to_string()))?;
    let response_bytes = client
        .invoke_service_raw(
            capability,
            InvokeServiceKind::Command,
            RECORDING_COMMANDS_INTERFACE.as_str(),
            "dispatch",
            payload,
        )
        .await?;
    bmux_ipc::decode(&response_bytes).map_err(|e| RecordingTypedClientError::Decode(e.to_string()))
}

/// Start a new recording session.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_start<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Option<Uuid>,
    capture_input: bool,
    name: Option<String>,
    profile: Option<RecordingProfile>,
    event_kinds: Option<Vec<RecordingEventKind>>,
) -> Result<RecordingSummary> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::Start {
            session_id,
            capture_input,
            name,
            profile,
            event_kinds,
        },
    )
    .await?
    {
        RecordingResponse::Started { recording } => Ok(recording),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording started",
        )),
    }
}

/// Stop an active recording session.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_stop<C: TypedDispatchClient>(
    client: &mut C,
    recording_id: Option<Uuid>,
) -> Result<Uuid> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::Stop { recording_id },
    )
    .await?
    {
        RecordingResponse::Stopped { recording_id } => {
            recording_id.ok_or(RecordingTypedClientError::NoActiveRecording)
        }
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording stopped",
        )),
    }
}

/// Write a custom event into the active recording.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_write_custom_event<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Option<Uuid>,
    pane_id: Option<Uuid>,
    source: String,
    name: String,
    payload: Vec<u8>,
) -> Result<()> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::WriteCustomEvent {
            session_id,
            pane_id,
            source,
            name,
            payload,
        },
    )
    .await?
    {
        RecordingResponse::CustomEventWritten { .. } => Ok(()),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording custom event written",
        )),
    }
}

/// Query recording runtime status.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_status<C: TypedDispatchClient>(client: &mut C) -> Result<RecordingStatus> {
    match dispatch(client, RECORDING_READ.as_str(), RecordingRequest::Status).await? {
        RecordingResponse::Status { status } => Ok(status),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording status",
        )),
    }
}

/// List known recordings.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_list<C: TypedDispatchClient>(
    client: &mut C,
) -> Result<Vec<RecordingSummary>> {
    match dispatch(client, RECORDING_READ.as_str(), RecordingRequest::List).await? {
        RecordingResponse::List { recordings } => Ok(recordings),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording list",
        )),
    }
}

/// Delete one recording by id.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_delete<C: TypedDispatchClient>(
    client: &mut C,
    recording_id: Uuid,
) -> Result<Uuid> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::Delete { recording_id },
    )
    .await?
    {
        RecordingResponse::Deleted { recording_id } => Ok(recording_id),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording deleted",
        )),
    }
}

/// Delete all recordings.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_delete_all<C: TypedDispatchClient>(client: &mut C) -> Result<usize> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::DeleteAll,
    )
    .await?
    {
        RecordingResponse::DeleteAll { removed_count } => Ok(removed_count),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording delete-all response",
        )),
    }
}

/// Create a bounded snapshot from the active rolling recording.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_cut<C: TypedDispatchClient>(
    client: &mut C,
    last_seconds: Option<u64>,
    name: Option<String>,
) -> Result<RecordingSummary> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::Cut { last_seconds, name },
    )
    .await?
    {
        RecordingResponse::Cut { recording } => Ok(recording),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording cut response",
        )),
    }
}

/// Start hidden rolling recording on a running server.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_rolling_start<C: TypedDispatchClient>(
    client: &mut C,
    options: RecordingRollingStartOptions,
) -> Result<RecordingSummary> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::RollingStart { options },
    )
    .await?
    {
        RecordingResponse::RollingStarted { recording } => Ok(recording),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording started response",
        )),
    }
}

/// Stop hidden rolling recording on a running server.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_rolling_stop<C: TypedDispatchClient>(client: &mut C) -> Result<Uuid> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::RollingStop,
    )
    .await?
    {
        RecordingResponse::RollingStopped { recording_id } => {
            recording_id.ok_or(RecordingTypedClientError::NoActiveRecording)
        }
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording stopped response",
        )),
    }
}

/// Fetch hidden rolling recording status and usage details.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_rolling_status<C: TypedDispatchClient>(
    client: &mut C,
) -> Result<RecordingRollingStatus> {
    match dispatch(
        client,
        RECORDING_READ.as_str(),
        RecordingRequest::RollingStatus,
    )
    .await?
    {
        RecordingResponse::RollingStatus { status } => Ok(status),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording rolling status response",
        )),
    }
}

/// Clear hidden rolling recording data and optionally restart when active.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_rolling_clear<C: TypedDispatchClient>(
    client: &mut C,
    restart_if_active: bool,
) -> Result<RecordingRollingClearReport> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::RollingClear { restart_if_active },
    )
    .await?
    {
        RecordingResponse::RollingCleared { report } => Ok(report),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording rolling cleared response",
        )),
    }
}

/// Return active recording capture targets for display-track writing.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_capture_targets<C: TypedDispatchClient>(
    client: &mut C,
) -> Result<Vec<RecordingCaptureTarget>> {
    match dispatch(
        client,
        RECORDING_READ.as_str(),
        RecordingRequest::CaptureTargets,
    )
    .await?
    {
        RecordingResponse::CaptureTargets { targets } => Ok(targets),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording capture targets response",
        )),
    }
}

/// Prune completed recordings older than the specified retention period.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn recording_prune<C: TypedDispatchClient>(
    client: &mut C,
    older_than_days: Option<u64>,
) -> Result<usize> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::Prune { older_than_days },
    )
    .await?
    {
        RecordingResponse::Pruned { pruned_count } => Ok(pruned_count),
        _ => Err(RecordingTypedClientError::Unexpected(
            "expected recording pruned response",
        )),
    }
}
