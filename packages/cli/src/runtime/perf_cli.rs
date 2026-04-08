use anyhow::{Context, Result};
use bmux_cli_schema::PerfProfileArg;
use bmux_ipc::{PerformanceRecordingLevel, PerformanceRuntimeSettings};

use super::{
    ConnectionContext, ConnectionPolicyScope, cleanup_stale_pid_file, connect_with_context,
    map_cli_client_error,
};

const fn performance_level_name(level: PerformanceRecordingLevel) -> &'static str {
    match level {
        PerformanceRecordingLevel::Off => "off",
        PerformanceRecordingLevel::Basic => "basic",
        PerformanceRecordingLevel::Detailed => "detailed",
        PerformanceRecordingLevel::Trace => "trace",
    }
}

const fn profile_to_level(profile: PerfProfileArg) -> PerformanceRecordingLevel {
    match profile {
        PerfProfileArg::Basic => PerformanceRecordingLevel::Basic,
        PerfProfileArg::Detailed => PerformanceRecordingLevel::Detailed,
        PerfProfileArg::Trace => PerformanceRecordingLevel::Trace,
    }
}

fn print_performance_settings(settings: &PerformanceRuntimeSettings) {
    println!(
        "runtime performance recording level: {}",
        performance_level_name(settings.recording_level)
    );
    println!("window ms: {}", settings.window_ms);
    println!("max events/sec: {}", settings.max_events_per_sec);
    println!(
        "max payload bytes/sec: {}",
        settings.max_payload_bytes_per_sec
    );
}

pub(super) async fn run_perf_status(
    json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-perf-status",
        connection_context,
    )
    .await?;
    let settings = client
        .performance_status()
        .await
        .map_err(map_cli_client_error)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&settings)
                .context("failed encoding performance status json")?
        );
        return Ok(0);
    }

    print_performance_settings(&settings);
    Ok(0)
}

pub(super) async fn run_perf_on(
    profile: PerfProfileArg,
    json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-perf-on",
        connection_context,
    )
    .await?;
    let mut settings = client
        .performance_status()
        .await
        .map_err(map_cli_client_error)?;
    settings.recording_level = profile_to_level(profile);
    let updated = client
        .performance_set(settings)
        .await
        .map_err(map_cli_client_error)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&updated)
                .context("failed encoding performance settings json")?
        );
        return Ok(0);
    }

    println!(
        "runtime performance telemetry enabled ({})",
        performance_level_name(updated.recording_level)
    );
    print_performance_settings(&updated);
    Ok(0)
}

pub(super) async fn run_perf_off(
    json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-perf-off",
        connection_context,
    )
    .await?;
    let mut settings = client
        .performance_status()
        .await
        .map_err(map_cli_client_error)?;
    settings.recording_level = PerformanceRecordingLevel::Off;
    let updated = client
        .performance_set(settings)
        .await
        .map_err(map_cli_client_error)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&updated)
                .context("failed encoding performance settings json")?
        );
        return Ok(0);
    }

    println!("runtime performance telemetry disabled");
    print_performance_settings(&updated);
    Ok(0)
}
