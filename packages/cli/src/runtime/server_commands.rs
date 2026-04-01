use super::*;
use bmux_ipc::IpcEndpoint;
use bmux_ipc::transport::LocalIpcStream;
use tokio::io::AsyncWriteExt;

#[derive(Debug, serde::Serialize)]
pub(super) struct ServerStatusJsonPayload {
    running: bool,
    principal_id: Option<Uuid>,
    server_control_principal_id: Option<Uuid>,
    force_local_permitted: bool,
    latest_server_event: Option<String>,
    snapshot: Option<bmux_ipc::ServerSnapshotStatus>,
    server_metadata: Option<ServerRuntimeMetadata>,
    cli_build: Option<String>,
    stale_build: bool,
    stale_warning: Option<String>,
}

pub(super) async fn run_server_status(as_json: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let status = fetch_server_status().await?;
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
            latest_server_event_name().await?.map(str::to_string)
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
            snapshot: status.as_ref().map(|entry| entry.snapshot.clone()),
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
            if let Some(event_name) = latest_server_event_name().await? {
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
            println!(
                "snapshot: {}{}",
                if status.snapshot.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status
                    .snapshot
                    .path
                    .as_ref()
                    .map_or(String::new(), |path| format!(" ({path})"))
            );
            if status.snapshot.enabled {
                println!(
                    "snapshot file: {}",
                    if status.snapshot.snapshot_exists {
                        "present"
                    } else {
                        "missing"
                    }
                );
                if let Some(last_write) = status.snapshot.last_write_epoch_ms {
                    println!("snapshot last write (ms): {last_write}");
                }
                if let Some(last_restore) = status.snapshot.last_restore_epoch_ms {
                    println!("snapshot last restore (ms): {last_restore}");
                }
                if let Some(error) = status.snapshot.last_restore_error.as_ref() {
                    println!("snapshot last error: {error}");
                }
            }
            println!("bmux server is running");
            Ok(0)
        }
        _ => {
            println!("bmux server is not running");
            Ok(1)
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub(super) struct ServerWhoAmIPrincipalJsonPayload {
    principal_id: Uuid,
    server_control_principal_id: Uuid,
    force_local_permitted: bool,
}

pub(super) async fn run_server_whoami_principal(as_json: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_raw("bmux-cli-server-whoami-principal").await?;
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

pub(super) async fn run_server_save() -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-server-save").await?;
    let path = client.server_save().await.map_err(map_cli_client_error)?;

    match path {
        Some(path) => println!("snapshot saved: {path}"),
        None => println!("snapshot save requested"),
    }
    Ok(0)
}

pub(super) async fn run_server_restore(dry_run: bool, yes: bool) -> Result<u8> {
    if !dry_run && !yes {
        anyhow::bail!("server restore requires either --dry-run or --yes");
    }
    cleanup_stale_pid_file().await?;

    if dry_run {
        let mut client = connect(
            ConnectionPolicyScope::Normal,
            "bmux-cli-server-restore-dry-run",
        )
        .await?;
        let (ok, message) = client
            .server_restore_dry_run()
            .await
            .map_err(map_cli_client_error)?;

        if ok {
            println!("restore dry-run: OK - {message}");
            return Ok(0);
        }
        println!("restore dry-run: FAIL - {message}");
        return Ok(1);
    }

    let mut client = connect(
        ConnectionPolicyScope::Normal,
        "bmux-cli-server-restore-apply",
    )
    .await?;
    let summary = client
        .server_restore_apply()
        .await
        .map_err(map_cli_client_error)?;

    println!(
        "restore applied: sessions={}, follows={}, selected_sessions={}",
        summary.sessions, summary.follows, summary.selected_sessions
    );
    Ok(0)
}

pub(super) async fn latest_server_event_name() -> Result<Option<&'static str>> {
    let connect =
        tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-status-events")).await;

    let mut client = match connect {
        Ok(Ok(client)) => client,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    let _ = tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.subscribe_events()).await;
    let events = match tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.poll_events(1)).await {
        Ok(Ok(events)) => events,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };
    Ok(events.last().map(server_event_name))
}

pub(super) const fn server_event_name(event: &bmux_client::ServerEvent) -> &'static str {
    match event {
        bmux_client::ServerEvent::ServerStarted => "server_started",
        bmux_client::ServerEvent::ServerStopping => "server_stopping",
        bmux_client::ServerEvent::SessionCreated { .. } => "session_created",
        bmux_client::ServerEvent::SessionRemoved { .. } => "session_removed",
        bmux_client::ServerEvent::ClientAttached { .. } => "client_attached",
        bmux_client::ServerEvent::ClientDetached { .. } => "client_detached",
        bmux_client::ServerEvent::FollowStarted { .. } => "follow_started",
        bmux_client::ServerEvent::FollowStopped { .. } => "follow_stopped",
        bmux_client::ServerEvent::FollowTargetGone { .. } => "follow_target_gone",
        bmux_client::ServerEvent::FollowTargetChanged { .. } => "follow_target_changed",
        bmux_client::ServerEvent::AttachViewChanged { .. } => "attach_view_changed",
    }
}

pub(super) async fn run_server_stop() -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let graceful_stopped =
        match tokio::time::timeout(SERVER_STOP_TIMEOUT, connect_raw("bmux-cli-stop")).await {
            Ok(Ok(mut client)) => {
                client.stop_server().await.map_err(map_cli_client_error)?;
                wait_until_server_stopped(SERVER_STOP_TIMEOUT).await?
            }
            Ok(Err(_)) | Err(_) => false,
        };

    if graceful_stopped {
        println!("bmux server stopped gracefully");
        let _ = remove_server_pid_file();
        return Ok(0);
    }

    if let Some(pid) = read_server_pid_file()? {
        if try_kill_pid(pid)? {
            if wait_for_process_exit(pid, SERVER_STOP_TIMEOUT)? {
                println!("bmux server stop fallback succeeded (pid {pid})");
                let _ = remove_server_pid_file();
                return Ok(0);
            }
        } else if !is_pid_running(pid)? {
            let _ = remove_server_pid_file();
        }
    }

    println!("bmux server is not running");
    Ok(1)
}

const BRIDGE_PREFLIGHT_TOKEN: &str = "BMUX_BRIDGE_READY";

pub(super) async fn run_server_bridge(stdio: bool, preflight: bool) -> Result<u8> {
    if !stdio {
        anyhow::bail!("server bridge currently requires --stdio");
    }

    if preflight {
        println!("{BRIDGE_PREFLIGHT_TOKEN}");
        return Ok(0);
    }

    let paths = ConfigPaths::default();
    let endpoint = local_endpoint_from_paths(&paths);
    let stream = LocalIpcStream::connect(&endpoint)
        .await
        .context("failed connecting local IPC endpoint for bridge")?;
    let (mut stream_read, mut stream_write) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let to_server = tokio::spawn(async move {
        tokio::io::copy(&mut stdin, &mut stream_write).await?;
        stream_write.shutdown().await?;
        Ok::<(), std::io::Error>(())
    });
    let from_server = tokio::spawn(async move {
        tokio::io::copy(&mut stream_read, &mut stdout).await?;
        stdout.flush().await?;
        Ok::<(), std::io::Error>(())
    });

    let to_server_result: std::io::Result<()> =
        to_server.await.context("bridge stdin task failed")?;
    let from_server_result: std::io::Result<()> =
        from_server.await.context("bridge stdout task failed")?;
    to_server_result.context("bridge stdin copy failed")?;
    from_server_result.context("bridge stdout copy failed")?;
    Ok(0)
}

fn local_endpoint_from_paths(paths: &ConfigPaths) -> IpcEndpoint {
    #[cfg(unix)]
    {
        IpcEndpoint::unix_socket(paths.server_socket())
    }
    #[cfg(windows)]
    {
        IpcEndpoint::windows_named_pipe(paths.server_named_pipe())
    }
}
