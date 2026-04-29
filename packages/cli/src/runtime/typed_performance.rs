use anyhow::{Context, Result, anyhow, bail};
use bmux_ipc::{InvokeServiceKind, PerformanceRuntimeSettings};
use bmux_performance_plugin_api::{
    PERFORMANCE_COMMANDS_INTERFACE, PERFORMANCE_READ, PERFORMANCE_WRITE, PerformanceRequest,
    PerformanceResponse,
};
use bmux_plugin_sdk::TypedDispatchClient;

async fn dispatch<C: TypedDispatchClient>(
    client: &mut C,
    capability: &str,
    request: PerformanceRequest,
) -> Result<PerformanceResponse> {
    let payload = bmux_ipc::encode(&request).context("encoding performance request")?;
    let response_bytes = client
        .invoke_service_raw(
            capability,
            InvokeServiceKind::Command,
            PERFORMANCE_COMMANDS_INTERFACE.as_str(),
            "dispatch",
            payload,
        )
        .await
        .map_err(|err| anyhow!("performance dispatch failed: {err}"))?;
    bmux_ipc::decode(&response_bytes).context("decoding performance response")
}

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
        _ => bail!("unexpected performance status response"),
    }
}

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
        _ => bail!("unexpected performance set response"),
    }
}
