use crate::ssh_access::{
    authenticate_host_connection, ensure_iroh_ssh_access_ready, iroh_ssh_access_enabled,
    iroh_target_compression_from_config, iroh_target_url,
};
use anyhow::{Context, Result};
use bmux_cli_schema::GatewayHostMode;
use bmux_config::{ConfigPaths, ServerGatewayConfig};
use bmux_ipc::IpcEndpoint;
use bmux_ipc::transport::LocalIpcStream;
use bmux_snapshot_plugin_api::{snapshot_commands, snapshot_state, snapshot_types};
use iroh::{Endpoint, endpoint::presets};
use std::process::{Command as ProcessCommand, Stdio};
use uuid::Uuid;

use super::{
    ConnectionContext, ConnectionPolicyScope, SERVER_STATUS_TIMEOUT, SERVER_STOP_TIMEOUT,
    ServerRuntimeMetadata, active_runtime_name, cleanup_stale_pid_file, connect_raw_with_context,
    connect_with_context, current_cli_build_id, fetch_server_status, is_pid_running,
    map_cli_client_error, read_server_pid_file, read_server_runtime_metadata,
    remove_server_pid_file, try_kill_pid, wait_for_process_exit, wait_until_server_stopped,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pemfile::{certs, pkcs8_private_keys};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

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

pub(super) async fn run_server_save(connection_context: ConnectionContext<'_>) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-server-save",
        connection_context,
    )
    .await?;
    let result = snapshot_commands::client::save_now(&mut client)
        .await
        .context("snapshot save dispatch failed")?
        .map_err(snapshot_plugin_error)?;

    match result.path {
        Some(path) => println!("snapshot saved: {path}"),
        None => println!("snapshot save requested"),
    }
    Ok(0)
}

pub(super) async fn run_server_restore(
    dry_run: bool,
    yes: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    if !dry_run && !yes {
        anyhow::bail!("server restore requires either --dry-run or --yes");
    }
    cleanup_stale_pid_file().await?;

    if dry_run {
        let mut client = connect_with_context(
            ConnectionPolicyScope::Normal,
            "bmux-cli-server-restore-dry-run",
            connection_context,
        )
        .await?;
        let result = snapshot_state::client::restore_dry_run(&mut client)
            .await
            .context("snapshot restore dry-run dispatch failed")?
            .map_err(snapshot_plugin_error)?;

        if result.ok {
            println!("restore dry-run: OK - {}", result.message);
            return Ok(0);
        }
        println!("restore dry-run: FAIL - {}", result.message);
        return Ok(1);
    }

    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-server-restore-apply",
        connection_context,
    )
    .await?;
    let summary = snapshot_commands::client::restore_apply(&mut client)
        .await
        .context("snapshot restore apply dispatch failed")?
        .map_err(snapshot_plugin_error)?;

    println!(
        "restore applied: restored_plugins={}, failed_plugins={}",
        summary.restored_plugins, summary.failed_plugins
    );
    Ok(0)
}

fn snapshot_plugin_error(error: snapshot_types::SnapshotError) -> anyhow::Error {
    match error {
        snapshot_types::SnapshotError::NotRegistered { message }
        | snapshot_types::SnapshotError::LockPoisoned { message }
        | snapshot_types::SnapshotError::NoRuntime { message } => anyhow::anyhow!(message),
        snapshot_types::SnapshotError::Failed { code, message } => {
            anyhow::anyhow!("snapshot {code}: {message}")
        }
    }
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

pub(super) async fn run_server_stop(connection_context: ConnectionContext<'_>) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let graceful_stopped = match tokio::time::timeout(
        SERVER_STOP_TIMEOUT,
        connect_raw_with_context("bmux-cli-stop", connection_context),
    )
    .await
    {
        Ok(Ok(mut client)) => {
            client.stop_server().await.map_err(map_cli_client_error)?;
            wait_until_server_stopped(SERVER_STOP_TIMEOUT, connection_context).await?
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

#[allow(clippy::similar_names)] // stdin/stdout are standard names
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
    let (mut ipc_reader, mut ipc_writer) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let to_server = tokio::spawn(async move {
        tokio::io::copy(&mut stdin, &mut ipc_writer).await?;
        ipc_writer.shutdown().await?;
        Ok::<(), std::io::Error>(())
    });
    let from_server = tokio::spawn(async move {
        tokio::io::copy(&mut ipc_reader, &mut stdout).await?;
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

pub(super) struct PreparedGateway {
    listen: String,
    listener: TcpListener,
    acceptor: TlsAcceptor,
}

impl PreparedGateway {
    pub(super) async fn run(self) -> Result<()> {
        println!("bmux TLS gateway listening on {}", self.listen);
        loop {
            let (tcp_stream, peer_addr) = self
                .listener
                .accept()
                .await
                .context("failed accepting TLS gateway connection")?;
            let acceptor = self.acceptor.clone();
            tokio::spawn(async move {
                if let Err(error) = handle_gateway_connection(acceptor, tcp_stream).await {
                    eprintln!("gateway connection from {peer_addr} failed: {error:#}");
                }
            });
        }
    }
}

pub(super) async fn prepare_configured_gateway(
    config: &ServerGatewayConfig,
) -> Result<Option<PreparedGateway>> {
    if !config.enabled {
        return Ok(None);
    }
    prepare_gateway(
        &config.listen,
        config.quick,
        config.cert_file.as_deref(),
        config.key_file.as_deref(),
    )
    .await
    .map(Some)
}

async fn prepare_gateway(
    listen: &str,
    quick: bool,
    cert_file: Option<&str>,
    key_file: Option<&str>,
) -> Result<PreparedGateway> {
    let (cert_file, key_file) = resolve_gateway_tls_files(quick, cert_file, key_file)?;
    let cert_chain = load_cert_chain(&cert_file)?;
    let private_key = load_private_key(&key_file)?;
    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .context("failed building TLS server config")?;
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed binding TLS gateway on {listen}"))?;
    Ok(PreparedGateway {
        listen: listen.to_string(),
        listener,
        acceptor: TlsAcceptor::from(Arc::new(tls_config)),
    })
}

pub(super) async fn run_server_gateway(
    listen: &str,
    host: bool,
    host_mode: GatewayHostMode,
    host_relay: &str,
    quick: bool,
    cert_file: Option<&str>,
    key_file: Option<&str>,
) -> Result<u8> {
    if host && host_mode == GatewayHostMode::Iroh {
        return run_server_gateway_iroh().await;
    }

    let gateway = prepare_gateway(listen, quick, cert_file, key_file).await?;

    if host {
        let tunnel_target = format!("80:127.0.0.1:{}", parse_listen_port(listen)?);
        println!("starting hosted reverse tunnel via '{host_relay}' (target: {tunnel_target})");
        spawn_reverse_tunnel(host_relay, &tunnel_target)?;
        println!(
            "when tunnel is ready, your public URL will be shown by ssh output. use that URL with 'bmux connect <url>'"
        );
    }
    gateway.run().await?;
    Ok(0)
}

#[allow(clippy::too_many_lines)]
async fn run_server_gateway_iroh() -> Result<u8> {
    const BMUX_IROH_ALPN: &[u8] = b"bmux/gateway/iroh/1";
    const IROH_DIRECT_BIND_ENV: &str = "BMUX_IROH_DIRECT_BIND";
    let config = bmux_config::BmuxConfig::load().context("failed loading bmux config")?;
    ensure_iroh_ssh_access_ready(&config)?;
    let require_ssh_auth = iroh_ssh_access_enabled(&config);
    let ssh_allowlist = config.connections.iroh_ssh_access.allowlist.clone();

    let direct_bind = std::env::var(IROH_DIRECT_BIND_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .parse::<std::net::SocketAddr>()
                .with_context(|| format!("invalid {IROH_DIRECT_BIND_ENV} socket address"))
        })
        .transpose()?;
    let mut builder = if direct_bind.is_some() {
        Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .relay_mode(iroh::RelayMode::Disabled)
    } else {
        Endpoint::builder(presets::N0)
    };
    builder = builder.alpns(vec![BMUX_IROH_ALPN.to_vec()]);
    if let Some(bind_addr) = direct_bind {
        builder = builder
            .bind_addr(bind_addr)
            .context("failed configuring direct iroh bind address")?;
    }
    let endpoint = builder
        .bind()
        .await
        .context("failed binding iroh endpoint")?;
    if direct_bind.is_none() {
        endpoint.online().await;
    }
    let addr = endpoint.addr();
    let endpoint_id = endpoint.id();
    let relay = addr
        .relay_urls()
        .next()
        .map(std::string::ToString::to_string);
    let direct_addr = direct_bind.and_then(|configured| {
        endpoint
            .bound_sockets()
            .into_iter()
            .find(|bound| bound.is_ipv4() == configured.is_ipv4())
    });
    let transport_compression = iroh_target_compression_from_config(&config);
    let mut url = iroh_target_url(
        &endpoint_id.to_string(),
        relay.as_deref(),
        require_ssh_auth,
        transport_compression,
    );
    if let Some(direct_addr) = direct_addr {
        let separator = if url.contains('?') { '&' } else { '?' };
        url.push(separator);
        url.push_str("addr=");
        url.push_str(&direct_addr.to_string());
    }
    println!("bmux iroh gateway online");
    println!("connect URL: {url}");
    if require_ssh_auth {
        println!("ssh key auth: enabled");
    }

    while let Some(incoming) = endpoint.accept().await {
        let mut accepting = match incoming.accept() {
            Ok(accepting) => accepting,
            Err(error) => {
                tracing::warn!(?error, "iroh incoming accept failed");
                continue;
            }
        };
        let ssh_allowlist = ssh_allowlist.clone();
        tokio::spawn(async move {
            let result: Result<()> = async {
                let alpn = accepting.alpn().await.context("failed reading ALPN")?;
                if alpn.as_slice() != BMUX_IROH_ALPN {
                    anyhow::bail!("unexpected iroh ALPN");
                }
                let conn = accepting
                    .await
                    .context("failed accepting iroh connection")?;

                if require_ssh_auth {
                    authenticate_host_connection(&conn, &ssh_allowlist)
                        .await
                        .context("iroh SSH auth failed")?;
                }

                // Accept multiple bi-streams per connection.  The first stream is
                // the primary attach session; additional streams are opened by the
                // client-side kernel bridge for plugin-to-server IPC calls.
                let paths = ConfigPaths::default();
                loop {
                    let Ok((send, recv)) = conn.accept_bi().await else {
                        break; // connection closed
                    };
                    let stream_paths = paths.clone();
                    tokio::spawn(async move {
                        if let Err(error) =
                            proxy_gateway_iroh_stream(send, recv, &stream_paths).await
                        {
                            tracing::debug!(?error, "iroh gateway stream proxy failed");
                        }
                    });
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(?error, "iroh connection handling failed");
            }
        });
    }
    Ok(0)
}

/// Proxy a single iroh QUIC bi-stream to/from a local IPC connection.
async fn proxy_gateway_iroh_stream(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    paths: &ConfigPaths,
) -> Result<()> {
    let endpoint = local_endpoint_from_paths(paths);
    let ipc_stream = LocalIpcStream::connect(&endpoint)
        .await
        .context("failed connecting local IPC endpoint for iroh gateway stream")?;
    let (mut ipc_read, mut ipc_write) = tokio::io::split(ipc_stream);

    let config = bmux_config::BmuxConfig::load().unwrap_or_default();
    let use_compression = config.behavior.compression.enabled
        && matches!(
            config.behavior.compression.remote,
            bmux_config::CompressionMode::Auto | bmux_config::CompressionMode::Zstd
        );

    if use_compression {
        let compressed =
            bmux_ipc::compressed_stream::CompressedStream::new(tokio::io::join(recv, send), 1);
        let (mut iroh_read, mut iroh_write) = tokio::io::split(compressed);

        let inbound = tokio::spawn(async move {
            tokio::io::copy(&mut iroh_read, &mut ipc_write).await?;
            ipc_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        });
        let outbound = tokio::spawn(async move {
            tokio::io::copy(&mut ipc_read, &mut iroh_write).await?;
            iroh_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        });

        inbound
            .await
            .context("iroh inbound task failed")?
            .context("iroh inbound copy failed")?;
        outbound
            .await
            .context("iroh outbound task failed")?
            .context("iroh outbound copy failed")?;
    } else {
        let inbound = tokio::spawn(async move {
            tokio::io::copy(&mut recv, &mut ipc_write).await?;
            ipc_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        });
        let outbound = tokio::spawn(async move {
            tokio::io::copy(&mut ipc_read, &mut send).await?;
            send.finish()?;
            Ok::<(), anyhow::Error>(())
        });

        inbound
            .await
            .context("iroh inbound task failed")?
            .context("iroh inbound copy failed")?;
        outbound
            .await
            .context("iroh outbound task failed")?
            .context("iroh outbound copy failed")?;
    }
    Ok(())
}

fn parse_listen_port(listen: &str) -> Result<u16> {
    let (_, port) = listen
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("listen address must include host:port"))?;
    port.parse::<u16>()
        .with_context(|| format!("invalid listen port in {listen}"))
}

fn spawn_reverse_tunnel(host_relay: &str, tunnel_target: &str) -> Result<()> {
    let mut command = ProcessCommand::new("ssh");
    command
        .arg("-N")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg("-R")
        .arg(tunnel_target)
        .arg(host_relay)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::null());
    command
        .spawn()
        .with_context(|| format!("failed launching reverse tunnel via {host_relay}"))?;
    Ok(())
}

fn resolve_gateway_tls_files(
    quick: bool,
    cert_file: Option<&str>,
    key_file: Option<&str>,
) -> Result<(String, String)> {
    if quick {
        if cert_file.is_some() || key_file.is_some() {
            anyhow::bail!("--quick cannot be combined with --cert-file/--key-file");
        }
        return generate_quick_gateway_cert_pair();
    }

    let cert_file = cert_file
        .ok_or_else(|| anyhow::anyhow!("--cert-file is required unless --quick is enabled"))?;
    let key_file = key_file
        .ok_or_else(|| anyhow::anyhow!("--key-file is required unless --quick is enabled"))?;
    Ok((cert_file.to_string(), key_file.to_string()))
}

fn generate_quick_gateway_cert_pair() -> Result<(String, String)> {
    let paths = ConfigPaths::default();
    std::fs::create_dir_all(paths.data_dir.join("tls")).with_context(|| {
        format!(
            "failed creating TLS data dir {}",
            paths.data_dir.join("tls").display()
        )
    })?;
    let cert_path = paths.data_dir.join("tls").join("gateway-quick-cert.pem");
    let key_path = paths.data_dir.join("tls").join("gateway-quick-key.pem");

    if cert_path.exists() && key_path.exists() {
        return Ok((
            cert_path.display().to_string(),
            key_path.display().to_string(),
        ));
    }

    let mut san_names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Ok(hostname) = std::env::var("HOSTNAME")
        && !hostname.trim().is_empty()
    {
        san_names.push(hostname.clone());
        san_names.push(format!("{}.local", hostname.trim_end_matches(".local")));
    }
    let cert = rcgen::generate_simple_self_signed(san_names)
        .context("failed generating quick self-signed gateway certificate")?;
    let cert_pem = cert.cert.pem();
    let key_pem = cert.signing_key.serialize_pem();
    std::fs::write(&cert_path, cert_pem)
        .with_context(|| format!("failed writing {}", cert_path.display()))?;
    std::fs::write(&key_path, key_pem)
        .with_context(|| format!("failed writing {}", key_path.display()))?;
    println!(
        "generated quick TLS gateway cert/key at '{}' and '{}'",
        cert_path.display(),
        key_path.display()
    );
    Ok((
        cert_path.display().to_string(),
        key_path.display().to_string(),
    ))
}

async fn handle_gateway_connection(
    acceptor: TlsAcceptor,
    tcp_stream: tokio::net::TcpStream,
) -> Result<()> {
    let tls_stream = acceptor
        .accept(tcp_stream)
        .await
        .context("TLS accept failed")?;
    let endpoint = local_endpoint_from_paths(&ConfigPaths::default());
    let ipc_stream = LocalIpcStream::connect(&endpoint)
        .await
        .context("failed connecting local IPC endpoint for TLS gateway")?;

    // Optionally wrap the TLS side with transport-level compression.
    // The local IPC side is never compressed (Unix socket, negligible latency).
    let config = bmux_config::BmuxConfig::load().unwrap_or_default();
    let use_transport_compression = config.behavior.compression.enabled
        && matches!(
            config.behavior.compression.remote,
            bmux_config::CompressionMode::Auto | bmux_config::CompressionMode::Zstd
        );

    let (mut ipc_read, mut ipc_write) = tokio::io::split(ipc_stream);

    if use_transport_compression {
        let compressed = bmux_ipc::compressed_stream::CompressedStream::new(tls_stream, 1);
        let (mut tls_read, mut tls_write) = tokio::io::split(compressed);

        let inbound = tokio::spawn(async move {
            tokio::io::copy(&mut tls_read, &mut ipc_write).await?;
            ipc_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        });
        let outbound = tokio::spawn(async move {
            tokio::io::copy(&mut ipc_read, &mut tls_write).await?;
            tls_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        });

        let inbound_result: std::io::Result<()> =
            inbound.await.context("TLS inbound task failed")?;
        let outbound_result: std::io::Result<()> =
            outbound.await.context("TLS outbound task failed")?;
        inbound_result.context("TLS inbound copy failed")?;
        outbound_result.context("TLS outbound copy failed")?;
    } else {
        let (mut tls_read, mut tls_write) = tokio::io::split(tls_stream);

        let inbound = tokio::spawn(async move {
            tokio::io::copy(&mut tls_read, &mut ipc_write).await?;
            ipc_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        });
        let outbound = tokio::spawn(async move {
            tokio::io::copy(&mut ipc_read, &mut tls_write).await?;
            tls_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        });

        let inbound_result: std::io::Result<()> =
            inbound.await.context("TLS inbound task failed")?;
        let outbound_result: std::io::Result<()> =
            outbound.await.context("TLS outbound task failed")?;
        inbound_result.context("TLS inbound copy failed")?;
        outbound_result.context("TLS outbound copy failed")?;
    }
    Ok(())
}

fn load_cert_chain(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let pem =
        std::fs::read(path).with_context(|| format!("failed reading certificate file {path}"))?;
    let mut reader = std::io::Cursor::new(pem);
    let chain = certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed parsing PEM certificates from {path}"))?;
    if chain.is_empty() {
        anyhow::bail!("certificate file {path} did not contain any certificates");
    }
    Ok(chain)
}

fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let pem =
        std::fs::read(path).with_context(|| format!("failed reading private key file {path}"))?;
    let mut reader = std::io::Cursor::new(pem);
    let keys = pkcs8_private_keys(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed parsing PEM private key from {path}"))?;
    let Some(key) = keys.into_iter().next() else {
        anyhow::bail!("private key file {path} did not contain a PKCS8 private key");
    };
    Ok(PrivateKeyDer::from(key))
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

#[cfg(test)]
mod tests {
    use super::{prepare_configured_gateway, prepare_gateway};
    use bmux_config::ServerGatewayConfig;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestCertificate {
        directory: PathBuf,
        cert_file: String,
        key_file: String,
    }

    impl TestCertificate {
        fn create() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should follow epoch")
                .as_nanos();
            let directory = std::env::temp_dir()
                .join(format!("bmux-gateway-test-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&directory).expect("create test certificate directory");
            let cert_file = directory.join("cert.pem");
            let key_file = directory.join("key.pem");
            let certificate = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
                .expect("generate test certificate");
            std::fs::write(&cert_file, certificate.cert.pem()).expect("write test certificate");
            std::fs::write(&key_file, certificate.signing_key.serialize_pem())
                .expect("write test private key");
            Self {
                directory,
                cert_file: cert_file.to_string_lossy().into_owned(),
                key_file: key_file.to_string_lossy().into_owned(),
            }
        }
    }

    impl Drop for TestCertificate {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[tokio::test]
    async fn disabled_config_does_not_prepare_gateway() {
        let gateway = prepare_configured_gateway(&ServerGatewayConfig::default())
            .await
            .expect("disabled gateway should not fail");
        assert!(gateway.is_none());
    }

    #[tokio::test]
    async fn enabled_config_prepares_gateway_from_typed_options() {
        let certificate = TestCertificate::create();
        let config = ServerGatewayConfig {
            enabled: true,
            listen: "127.0.0.1:0".to_string(),
            quick: false,
            cert_file: Some(certificate.cert_file.clone()),
            key_file: Some(certificate.key_file.clone()),
        };
        let gateway = prepare_configured_gateway(&config)
            .await
            .expect("enabled gateway should prepare")
            .expect("enabled gateway should return runtime");
        assert!(gateway.listener.local_addr().is_ok());
    }

    #[tokio::test]
    async fn configured_gateway_binds_and_releases_listener_when_cancelled() {
        let certificate = TestCertificate::create();
        let gateway = prepare_gateway(
            "127.0.0.1:0",
            false,
            Some(&certificate.cert_file),
            Some(&certificate.key_file),
        )
        .await
        .expect("prepare gateway");
        let address = gateway
            .listener
            .local_addr()
            .expect("gateway local address");
        let task = tokio::spawn(gateway.run());
        tokio::task::yield_now().await;
        assert!(tokio::net::TcpListener::bind(address).await.is_err());
        task.abort();
        let _ = task.await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match tokio::net::TcpListener::bind(address).await {
                Ok(listener) => {
                    drop(listener);
                    break;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::AddrInUse
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(error) => panic!("aborting gateway should release listener: {error}"),
            }
        }
    }

    #[tokio::test]
    async fn configured_gateway_rejects_occupied_listener() {
        let certificate = TestCertificate::create();
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied test listener");
        let address = occupied.local_addr().expect("occupied local address");
        let result = prepare_gateway(
            &address.to_string(),
            false,
            Some(&certificate.cert_file),
            Some(&certificate.key_file),
        )
        .await;
        assert!(result.is_err());
    }
}
