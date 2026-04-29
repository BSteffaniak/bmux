use anyhow::{Context, Result, anyhow, bail};
use bmux_ipc::{
    InvokeServiceKind, RecordingCaptureTarget, RecordingEventKind, RecordingProfile,
    RecordingRollingClearReport, RecordingRollingStartOptions, RecordingRollingStatus,
    RecordingStatus, RecordingSummary,
};
use bmux_plugin_sdk::TypedDispatchClient;
use bmux_recording_plugin_api::{
    RECORDING_COMMANDS_INTERFACE, RECORDING_READ, RECORDING_WRITE, RecordingRequest,
    RecordingResponse,
};
use uuid::Uuid;

async fn dispatch<C: TypedDispatchClient>(
    client: &mut C,
    capability: &str,
    request: RecordingRequest,
) -> Result<RecordingResponse> {
    let payload = bmux_ipc::encode(&request).context("encoding recording request")?;
    let response_bytes = client
        .invoke_service_raw(
            capability,
            InvokeServiceKind::Command,
            RECORDING_COMMANDS_INTERFACE.as_str(),
            "dispatch",
            payload,
        )
        .await
        .map_err(|err| anyhow!("recording dispatch failed: {err}"))?;
    bmux_ipc::decode(&response_bytes).context("decoding recording response")
}

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
        _ => bail!("unexpected recording response: expected recording started"),
    }
}

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
            recording_id.ok_or_else(|| anyhow!("no active recording to stop"))
        }
        _ => bail!("unexpected recording response: expected recording stopped"),
    }
}

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
        _ => bail!("unexpected recording response: expected custom event written"),
    }
}

pub async fn recording_status<C: TypedDispatchClient>(client: &mut C) -> Result<RecordingStatus> {
    match dispatch(client, RECORDING_READ.as_str(), RecordingRequest::Status).await? {
        RecordingResponse::Status { status } => Ok(status),
        _ => bail!("unexpected recording response: expected recording status"),
    }
}

pub async fn recording_list<C: TypedDispatchClient>(
    client: &mut C,
) -> Result<Vec<RecordingSummary>> {
    match dispatch(client, RECORDING_READ.as_str(), RecordingRequest::List).await? {
        RecordingResponse::List { recordings } => Ok(recordings),
        _ => bail!("unexpected recording response: expected recording list"),
    }
}

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
        _ => bail!("unexpected recording response: expected recording deleted"),
    }
}

pub async fn recording_delete_all<C: TypedDispatchClient>(client: &mut C) -> Result<usize> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::DeleteAll,
    )
    .await?
    {
        RecordingResponse::DeleteAll { removed_count } => Ok(removed_count),
        _ => bail!("unexpected recording response: expected recording delete-all"),
    }
}

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
        _ => bail!("unexpected recording response: expected recording cut"),
    }
}

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
        _ => bail!("unexpected recording response: expected rolling recording started"),
    }
}

pub async fn recording_rolling_stop<C: TypedDispatchClient>(client: &mut C) -> Result<Uuid> {
    match dispatch(
        client,
        RECORDING_WRITE.as_str(),
        RecordingRequest::RollingStop,
    )
    .await?
    {
        RecordingResponse::RollingStopped { recording_id } => {
            recording_id.ok_or_else(|| anyhow!("no active recording to stop"))
        }
        _ => bail!("unexpected recording response: expected rolling recording stopped"),
    }
}

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
        _ => bail!("unexpected recording response: expected rolling recording status"),
    }
}

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
        _ => bail!("unexpected recording response: expected rolling recording cleared"),
    }
}

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
        _ => bail!("unexpected recording response: expected recording capture targets"),
    }
}

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
        _ => bail!("unexpected recording response: expected recording pruned"),
    }
}
