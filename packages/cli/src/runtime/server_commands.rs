use anyhow::{Context, Result};
use bmux_config::ConfigPaths;
use bmux_snapshot_plugin_api::{snapshot_state, snapshot_types};
use uuid::Uuid;

use super::{
    ConnectionContext, ConnectionPolicyScope, SERVER_STATUS_TIMEOUT, ServerRuntimeMetadata,
    active_runtime_name, cleanup_stale_pid_file, connect_raw_with_context, connect_with_context,
    current_cli_build_id, fetch_server_status, map_cli_client_error, read_server_pid_file,
    read_server_runtime_metadata, wait_until_server_stopped,
};

#[derive(Debug, serde::Serialize)]
pub(super) struct ServerStatusJsonPayload {
    running: bool,
    principal_id: Option<Uuid>,
    server_control_principal_id: Option<Uuid>,
    force_local_permitted: bool,
    latest_server_event: Option<String>,
    snapshot: Option<snapshot_types::SnapshotStatusPayload>,
    server_metadata: Option<ServerRuntimeMetadata>,
    cli_build: Option<String>,
    stale_build: bool,
    stale_warning: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_server_status(
    as_json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let status = fetch_server_status(connection_context).await?;
    let snapshot_status = if matches!(status, Some(ref s) if s.running) {
        fetch_snapshot_status(connection_context).await?
    } else {
        None
    };
    let metadata = read_server_runtime_metadata()?;
    let current_build_id = current_cli_build_id().ok();
    let stale_warning = metadata.as_ref().and_then(|entry| {
        current_build_id
            .as_ref()
            .filter(|build| entry.build_id != **build)
            .map(|build| {
                format!(
                    "running server build ({}) differs from current CLI build ({}); restart with `bmux server stop`",
                    entry.build_id, build
                )
            })
    });
    let stale_build = stale_warning.is_some();

    if as_json {
        let latest_event = if matches!(status, Some(ref s) if s.running) {
            latest_server_event_name(connection_context)
                .await?
                .map(str::to_string)
        } else {
            None
        };
        let payload = ServerStatusJsonPayload {
            running: matches!(status, Some(ref s) if s.running),
            principal_id: status.as_ref().map(|entry| entry.principal_id),
            server_control_principal_id: status
                .as_ref()
                .map(|entry| entry.server_control_principal_id),
            force_local_permitted: status
                .as_ref()
                .is_some_and(|entry| entry.principal_id == entry.server_control_principal_id),
            latest_server_event: latest_event,
            snapshot: snapshot_status.clone(),
            server_metadata: metadata,
            cli_build: current_build_id,
            stale_build,
            stale_warning,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).context("failed encoding server status json")?
        );
        return Ok(u8::from(!payload.running));
    }

    match status {
        Some(status) if status.running => {
            let paths = ConfigPaths::default();
            if let Some(event_name) = latest_server_event_name(connection_context).await? {
                println!("latest server event: {event_name}");
            }
            if let Some(metadata) = metadata.as_ref() {
                println!("server pid: {}", metadata.pid);
                println!("server version: {}", metadata.version);
                println!("server build: {}", metadata.build_id);
                println!("server executable: {}", metadata.executable_path);
                println!("server started_at_ms: {}", metadata.started_at_epoch_ms);
            } else {
                println!("server metadata: missing");
            }
            if let Some(build_id) = current_build_id.as_ref() {
                println!("cli build: {build_id}");
                if let Some(warning) = stale_warning.as_ref() {
                    println!("warning: {warning}");
                }
            }
            println!("principal id: {}", status.principal_id);
            println!(
                "server control principal id: {}",
                status.server_control_principal_id
            );
            println!(
                "force-local permitted: {}",
                if status.principal_id == status.server_control_principal_id {
                    "yes"
                } else {
                    "no"
                }
            );
            println!("runtime: {}", active_runtime_name());
            #[cfg(unix)]
            println!("server socket: {}", paths.server_socket().display());
            #[cfg(windows)]
            println!("server pipe: {}", paths.server_named_pipe());
            print_snapshot_status(snapshot_status.as_ref());
            println!("bmux server is running");
            Ok(0)
        }
        _ => {
            println!("bmux server is not running");
            Ok(1)
        }
    }
}

async fn fetch_snapshot_status(
    connection_context: ConnectionContext<'_>,
) -> Result<Option<snapshot_types::SnapshotStatusPayload>> {
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-server-status-snapshot",
        connection_context,
    )
    .await?;

    let status = snapshot_state::client::status(&mut client)
        .await
        .context("snapshot status dispatch failed")?;
    Ok(status.ok())
}

fn print_snapshot_status(status: Option<&snapshot_types::SnapshotStatusPayload>) {
    let Some(status) = status else {
        println!("snapshot: unavailable");
        return;
    };

    println!(
        "snapshot: {}{}",
        if status.enabled {
            "enabled"
        } else {
            "disabled"
        },
        status
            .path
            .as_ref()
            .map_or(String::new(), |path| format!(" ({path})"))
    );
    if status.enabled {
        println!(
            "snapshot file: {}",
            if status.snapshot_exists {
                "present"
            } else {
                "missing"
            }
        );
        if let Some(last_write) = status.last_write_epoch_ms {
            println!("snapshot last write (ms): {last_write}");
        }
        if let Some(last_restore) = status.last_restore_epoch_ms {
            println!("snapshot last restore (ms): {last_restore}");
        }
        if let Some(error) = status.last_restore_error.as_ref() {
            println!("snapshot last error: {error}");
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub(super) struct ServerWhoAmIPrincipalJsonPayload {
    principal_id: Uuid,
    server_control_principal_id: Uuid,
    force_local_permitted: bool,
}

pub(super) async fn run_server_whoami_principal(
    as_json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client =
        connect_raw_with_context("bmux-cli-server-whoami-principal", connection_context).await?;
    let identity = client
        .whoami_principal()
        .await
        .map_err(map_cli_client_error)?;

    if as_json {
        let payload = ServerWhoAmIPrincipalJsonPayload {
            principal_id: identity.principal_id,
            server_control_principal_id: identity.server_control_principal_id,
            force_local_permitted: identity.force_local_permitted,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed encoding server whoami-principal json")?
        );
        return Ok(0);
    }

    println!("principal id: {}", identity.principal_id);
    println!(
        "server control principal id: {}",
        identity.server_control_principal_id
    );
    println!(
        "force-local permitted: {}",
        if identity.force_local_permitted {
            "yes"
        } else {
            "no"
        }
    );
    Ok(0)
}

pub(super) async fn latest_server_event_name(
    connection_context: ConnectionContext<'_>,
) -> Result<Option<&'static str>> {
    let connect = tokio::time::timeout(
        SERVER_STATUS_TIMEOUT,
        connect_raw_with_context("bmux-cli-status-events", connection_context),
    )
    .await;

    let Ok(Ok(mut client)) = connect else {
        return Ok(None);
    };

    let _ = tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.subscribe_events()).await;
    let Ok(Ok(events)) = tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.poll_events(1)).await
    else {
        return Ok(None);
    };
    Ok(events.last().map(server_event_name))
}

pub(super) fn server_event_name(event: &bmux_client::ServerEvent) -> &'static str {
    match event {
        bmux_client::ServerEvent::ServerStarted => "server_started",
        bmux_client::ServerEvent::ServerStopping => "server_stopping",
        bmux_client::ServerEvent::PluginBusEvent { kind, payload } => {
            plugin_bus_event_name(kind, payload)
        }
        bmux_client::ServerEvent::DeviceSealBrokerRequest { .. } => "device_seal_broker_request",
    }
}

fn plugin_bus_event_name(kind: &str, payload: &[u8]) -> &'static str {
    if kind == bmux_sessions_plugin_api::sessions_events::EVENT_KIND.as_str() {
        return serde_json::from_slice::<bmux_sessions_plugin_api::sessions_events::SessionEvent>(
            payload,
        )
        .map_or("plugin_bus_event", |event| match event {
            bmux_sessions_plugin_api::sessions_events::SessionEvent::Created { .. } => {
                "session_created"
            }
            bmux_sessions_plugin_api::sessions_events::SessionEvent::Removed { .. } => {
                "session_removed"
            }
            bmux_sessions_plugin_api::sessions_events::SessionEvent::Selected { .. }
            | bmux_sessions_plugin_api::sessions_events::SessionEvent::Renamed { .. } => {
                "plugin_bus_event"
            }
        });
    }
    if kind == bmux_clients_plugin_api::clients_events::EVENT_KIND.as_str() {
        return serde_json::from_slice::<bmux_clients_plugin_api::clients_events::ClientEvent>(
            payload,
        )
        .map_or("plugin_bus_event", |event| match event {
            bmux_clients_plugin_api::clients_events::ClientEvent::Attached { .. } => {
                "client_attached"
            }
            bmux_clients_plugin_api::clients_events::ClientEvent::Detached { .. } => {
                "client_detached"
            }
            bmux_clients_plugin_api::clients_events::ClientEvent::FollowStarted { .. } => {
                "follow_started"
            }
            bmux_clients_plugin_api::clients_events::ClientEvent::FollowStopped { .. } => {
                "follow_stopped"
            }
            bmux_clients_plugin_api::clients_events::ClientEvent::FollowTargetGone { .. } => {
                "follow_target_gone"
            }
            bmux_clients_plugin_api::clients_events::ClientEvent::FollowTargetChanged {
                ..
            } => "follow_target_changed",
            bmux_clients_plugin_api::clients_events::ClientEvent::SessionSelected { .. }
            | bmux_clients_plugin_api::clients_events::ClientEvent::FollowChanged { .. } => {
                "plugin_bus_event"
            }
        });
    }
    if kind == bmux_pane_runtime_plugin_api::pane_runtime_events::EVENT_KIND.as_str() {
        return serde_json::from_slice::<
            bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent,
        >(payload)
        .map_or("plugin_bus_event", |event| match event {
            bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent::ClientAttached {
                ..
            } => "client_attached",
            bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent::ClientDetached {
                ..
            } => "client_detached",
            bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent::Exited { .. } => {
                "pane_exited"
            }
            bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent::Restarted { .. } => {
                "pane_restarted"
            }
            bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent::OutputAvailable {
                ..
            } => "pane_output_available",
            bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent::ImageAvailable {
                ..
            } => "pane_image_available",
            bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent::AttachViewChanged {
                ..
            } => "attach_view_changed",
        });
    }
    "plugin_bus_event"
}

#[cfg(test)]
#[path = "owned_stop_tests.rs"]
mod owned_stop_tests;

const OWNED_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const STOP_TIMEOUT_MESSAGE: &str = "no observable shutdown progress for 30 seconds; no termination signal was sent. Cleanup may still be running. Inspect the selected runtime's server logs and retry `server stop`; do not kill a numeric PID from a stale PID file";
/// Default bound on observing an accepted local shutdown until process exit.
/// The server bounds its own plugin teardown and runtime release (see
/// `bmux_plugin_runtime::teardown_timeout`), so this only trips when the
/// server is stuck outside those bounded paths.
const OWNED_STOP_EXIT_BOUND: std::time::Duration = std::time::Duration::from_secs(120);
/// Environment override (whole seconds) for [`OWNED_STOP_EXIT_BOUND`].
const OWNED_STOP_EXIT_BOUND_ENV: &str = "BMUX_SERVER_STOP_TIMEOUT_SECS";

fn owned_stop_exit_bound() -> std::time::Duration {
    std::env::var(OWNED_STOP_EXIT_BOUND_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(OWNED_STOP_EXIT_BOUND, std::time::Duration::from_secs)
}

#[derive(Debug, serde::Deserialize)]
struct ShutdownReceipt {
    pid: u32,
    started_at_epoch_ms: u64,
    error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum CleanupProof {
    Complete,
    Failed(String),
    Unavailable,
}

fn cleanup_proof(
    metadata: Option<&ServerRuntimeMetadata>,
    receipt: Option<&ShutdownReceipt>,
) -> CleanupProof {
    match (metadata, receipt) {
        (Some(metadata), Some(receipt))
            if metadata.pid == receipt.pid
                && metadata.started_at_epoch_ms == receipt.started_at_epoch_ms =>
        {
            receipt
                .error
                .as_ref()
                .map_or(CleanupProof::Complete, |error| {
                    CleanupProof::Failed(error.clone())
                })
        }
        _ => CleanupProof::Unavailable,
    }
}

fn read_shutdown_receipt(path: &std::path::Path) -> Result<Option<ShutdownReceipt>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("invalid shutdown completion record {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed reading shutdown completion record {}",
                path.display()
            )
        }),
    }
}

fn report_previous_shutdown(receipt: &ShutdownReceipt) -> Result<u8> {
    // Historical evidence only: without live metadata it cannot prove the
    // identity or outcome of a newly selected server instance.
    eprintln!(
        "Server is not reachable. Last recorded shutdown: PID {}, started at {} (historical result).",
        receipt.pid, receipt.started_at_epoch_ms
    );
    if let Some(error) = &receipt.error {
        anyhow::bail!("last recorded shutdown cleanup failed: {error}");
    }
    println!("Last recorded shutdown cleanup completed; no current server identity is available.");
    Ok(0)
}

fn report_local_stop(proof: CleanupProof) -> Result<u8> {
    match proof {
        CleanupProof::Complete => println!("bmux server stopped; cleanup completed"),
        CleanupProof::Failed(error) => {
            anyhow::bail!("bmux server exited, but cleanup failed: {error}")
        }
        CleanupProof::Unavailable => println!(
            "bmux server process stopped; cleanup completion is unverified (no matching completion record)"
        ),
    }
    Ok(0)
}

fn report_remote_completion() -> Result<u8> {
    anyhow::bail!(
        "remote shutdown acknowledged and endpoint unavailable, but process exit and cleanup completion cannot be proved: the remote protocol exposes no shutdown completion record (no local fallback)"
    )
}

#[allow(clippy::too_many_lines)] // Keep identity validation and the no-signal policy together.
pub(super) async fn run_server_stop(connection_context: ConnectionContext<'_>) -> Result<u8> {
    let mut notice = tokio::time::interval(std::time::Duration::from_secs(5));
    notice.tick().await;
    let started = std::time::Instant::now();
    let stop = run_server_stop_inner(connection_context);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            result = &mut stop => return result,
            result = tokio::signal::ctrl_c() => {
                result.context("failed waiting for shutdown cancellation")?;
                eprintln!("Stopped waiting for shutdown; an accepted shutdown continues. Run `bmux server stop` to observe it again.");
                return Ok(130);
            }
            _ = notice.tick() => {
                eprintln!("Still waiting for bmux shutdown ({}s); cleanup may be quiet. Ctrl-C stops waiting, not an accepted shutdown.", started.elapsed().as_secs());
            }
        }
    }
}

#[allow(clippy::too_many_lines)] // Keep selected-instance identity checks and no-signal shutdown flow together.
async fn run_server_stop_inner(connection_context: ConnectionContext<'_>) -> Result<u8> {
    let deadline = tokio::time::Instant::now() + OWNED_STOP_TIMEOUT;
    let target = tokio::time::timeout_at(
        deadline,
        crate::connection::StopTarget::resolve(connection_context),
    )
    .await
    .context(STOP_TIMEOUT_MESSAGE)??;
    // Do not prune PID/metadata files: cleanup owns them, and an older stop
    // invocation must never remove a replacement server's identity.
    let local = target.is_local();
    let metadata = if local {
        read_server_runtime_metadata()?
    } else {
        None
    };
    let pid = if local { read_server_pid_file()? } else { None };
    if let (Some(metadata), Some(pid)) = (&metadata, pid) {
        anyhow::ensure!(
            metadata.pid == pid,
            "server PID and metadata disagree; refusing shutdown"
        );
    }
    let observed_pid = pid.or_else(|| metadata.as_ref().map(|metadata| metadata.pid));
    let process = observed_pid
        .map(super::server_runtime::ProcessObservation::open)
        .transpose()?;
    let receipt_path = local.then(|| {
        ConfigPaths::default()
            .runtime_dir
            .join("shutdown-result.json")
    });
    let connected = tokio::time::timeout_at(deadline, target.connect("bmux-cli-stop")).await;
    let acknowledged = match connected {
        Ok(Ok(mut client)) => {
            if local {
                anyhow::ensure!(
                    read_server_runtime_metadata()? == metadata && read_server_pid_file()? == pid,
                    "server identity changed while connecting; retry for the selected runtime"
                );
            }
            match tokio::time::timeout_at(deadline, client.stop_server()).await {
                Ok(Err(
                    bmux_client::ClientError::Transport(_) | bmux_client::ClientError::Timeout(_),
                )) if local && process.is_some() => {
                    eprintln!(
                        "Shutdown acknowledgment was lost; observing the selected server without repeating the request."
                    );
                }
                Ok(result) => {
                    result.map_err(map_cli_client_error)?;
                }
                Err(_) if local && process.is_some() => {
                    // The request may have been accepted. Observe the pinned
                    // instance rather than declaring failure or sending signals.
                    eprintln!(
                        "Shutdown acknowledgment is delayed; observing the selected server without repeating the request."
                    );
                }
                Err(_) => anyhow::bail!(
                    "shutdown acknowledgment unavailable; remote cleanup is unconfirmed"
                ),
            }
            true
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "stop endpoint unavailable; no signal fallback permitted");
            false
        }
        Err(_) => anyhow::bail!(STOP_TIMEOUT_MESSAGE),
    };

    if !local {
        anyhow::ensure!(
            acknowledged,
            "remote server is unreachable; shutdown and cleanup completion are unconfirmed (no local fallback)"
        );
        anyhow::ensure!(
            wait_until_server_stopped(deadline, &target).await?,
            "{STOP_TIMEOUT_MESSAGE}"
        );
        return report_remote_completion();
    }

    let Some(process) = process else {
        if acknowledged {
            anyhow::bail!(
                "shutdown acknowledged, but no local process identity is available; cleanup completion is unconfirmed (no signal sent)"
            );
        }
        if let Some(receipt) =
            read_shutdown_receipt(receipt_path.as_deref().expect("local receipt path"))?
        {
            return report_previous_shutdown(&receipt);
        }
        println!(
            "bmux server is not reachable; no local process identity or retained shutdown result is available"
        );
        return Ok(1);
    };
    wait_for_shutdown_progress(
        &process,
        metadata.as_ref(),
        &ConfigPaths::default()
            .runtime_dir
            .join("shutdown-progress.json"),
        owned_stop_exit_bound(),
    )
    .await?;
    // Bootstrap publishes the receipt before process exit. A missing receipt
    // supports old servers, but never proves their cleanup was successful.
    let receipt = read_shutdown_receipt(receipt_path.as_deref().expect("local receipt path"))?;
    report_local_stop(cleanup_proof(metadata.as_ref(), receipt.as_ref()))
}

async fn wait_for_shutdown_progress(
    process: &super::server_runtime::ProcessObservation,
    metadata: Option<&ServerRuntimeMetadata>,
    path: &std::path::Path,
    bound: std::time::Duration,
) -> Result<()> {
    use bmux_plugin_runtime::shutdown_progress::{ShutdownIdleTimer, ShutdownProgressSnapshot};
    // Explicit shutdown has no idle deadline, but it has a total bound: quiet
    // cleanup may take a while, yet a stuck server is reported, never awaited
    // forever. Keep validating provenance and monotonic observations; the CLI
    // notice timer never advances this state.
    let started = std::time::Instant::now();
    let mut observations = ShutdownIdleTimer::new(started, OWNED_STOP_TIMEOUT);
    loop {
        if process.exited()? {
            return Ok(());
        }
        if started.elapsed() >= bound {
            let step: String = observations
                .last_step()
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect();
            anyhow::bail!(
                "server did not exit within {}s of accepting shutdown (last progress: {step}); no termination signal was sent and cleanup is unconfirmed. The server log names any plugin scope holders that blocked teardown; retry `server stop` or raise {OWNED_STOP_EXIT_BOUND_ENV}",
                bound.as_secs()
            );
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                anyhow::ensure!(
                    bytes.len() <= 4096,
                    "oversized shutdown progress observation"
                );
                let snapshot: ShutdownProgressSnapshot = serde_json::from_slice(&bytes)
                    .context("invalid shutdown progress observation")?;
                if let Some(metadata) = metadata {
                    // Report only observations from the selected instance: the
                    // previous server's file stays until this one replaces it.
                    let accepted = observations
                        .observe(
                            &snapshot,
                            metadata.pid,
                            metadata.started_at_epoch_ms,
                            std::time::Instant::now(),
                        )
                        .map_err(anyhow::Error::msg)?;
                    if accepted {
                        let step: String = observations
                            .last_step()
                            .chars()
                            .map(|ch| if ch.is_control() { ' ' } else { ch })
                            .collect();
                        eprintln!("Shutdown: {step}");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot read shutdown progress observation"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
