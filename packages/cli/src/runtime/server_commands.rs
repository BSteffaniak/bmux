use super::*;
use bmux_cli_schema::GatewayHostMode;
use bmux_ipc::IpcEndpoint;
use bmux_ipc::transport::LocalIpcStream;
use iroh::{Endpoint, endpoint::presets};
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

    let (cert_file, key_file) = resolve_gateway_tls_files(quick, cert_file, key_file)?;
    let cert_chain = load_cert_chain(&cert_file)?;
    let private_key = load_private_key(&key_file)?;
    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .context("failed building TLS server config")?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed binding TLS gateway on {listen}"))?;

    println!("bmux TLS gateway listening on {listen}");
    if host {
        let tunnel_target = format!("80:127.0.0.1:{}", parse_listen_port(listen)?);
        println!(
            "starting hosted reverse tunnel via '{}' (target: {})",
            host_relay, tunnel_target
        );
        spawn_reverse_tunnel(host_relay, &tunnel_target)?;
        println!(
            "when tunnel is ready, your public URL will be shown by ssh output. use that URL with 'bmux connect <url>'"
        );
    }
    loop {
        let (tcp_stream, peer_addr) = listener
            .accept()
            .await
            .context("failed accepting TLS gateway connection")?;
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_gateway_connection(acceptor, tcp_stream).await {
                tracing::warn!(peer = %peer_addr, ?error, "tls gateway connection failed");
            }
        });
    }
}

async fn run_server_gateway_iroh() -> Result<u8> {
    const BMUX_IROH_ALPN: &[u8] = b"bmux/gateway/iroh/1";
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![BMUX_IROH_ALPN.to_vec()])
        .bind()
        .await
        .context("failed binding iroh endpoint")?;
    endpoint.online().await;
    let addr = endpoint.addr();
    let endpoint_id = endpoint.id();
    let relay = addr.relay_urls().next().map(|value| value.to_string());
    let url = relay.as_ref().map_or_else(
        || format!("iroh://{endpoint_id}"),
        |relay| format!("iroh://{endpoint_id}?relay={relay}"),
    );
    println!("bmux iroh gateway online");
    println!("connect URL: {url}");

    while let Some(incoming) = endpoint.accept().await {
        let mut accepting = match incoming.accept() {
            Ok(accepting) => accepting,
            Err(error) => {
                tracing::warn!(?error, "iroh incoming accept failed");
                continue;
            }
        };
        tokio::spawn(async move {
            let result: Result<()> = async {
                let alpn = accepting.alpn().await.context("failed reading ALPN")?;
                if alpn.as_slice() != BMUX_IROH_ALPN {
                    anyhow::bail!("unexpected iroh ALPN");
                }
                let conn = accepting
                    .await
                    .context("failed accepting iroh connection")?;
                let (mut send, mut recv) = conn
                    .accept_bi()
                    .await
                    .context("failed accepting iroh stream")?;
                let endpoint = local_endpoint_from_paths(&ConfigPaths::default());
                let ipc_stream = LocalIpcStream::connect(&endpoint)
                    .await
                    .context("failed connecting local IPC endpoint for iroh gateway")?;
                let (mut ipc_read, mut ipc_write) = tokio::io::split(ipc_stream);

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

                let inbound_result: std::io::Result<()> =
                    inbound.await.context("iroh inbound task failed")?;
                let outbound_result: anyhow::Result<()> =
                    outbound.await.context("iroh outbound task failed")?;
                inbound_result.context("iroh inbound copy failed")?;
                outbound_result.context("iroh outbound copy failed")?;
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
    std::fs::create_dir_all(&paths.runtime_dir).with_context(|| {
        format!(
            "failed creating runtime dir {}",
            paths.runtime_dir.display()
        )
    })?;
    let cert_path = paths.runtime_dir.join("gateway-quick-cert.pem");
    let key_path = paths.runtime_dir.join("gateway-quick-key.pem");

    if cert_path.exists() && key_path.exists() {
        return Ok((
            cert_path.display().to_string(),
            key_path.display().to_string(),
        ));
    }

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .context("failed generating quick self-signed gateway certificate")?;
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();
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
    let (mut tls_read, mut tls_write) = tokio::io::split(tls_stream);
    let (mut ipc_read, mut ipc_write) = tokio::io::split(ipc_stream);

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

    let inbound_result: std::io::Result<()> = inbound.await.context("TLS inbound task failed")?;
    let outbound_result: std::io::Result<()> =
        outbound.await.context("TLS outbound task failed")?;
    inbound_result.context("TLS inbound copy failed")?;
    outbound_result.context("TLS outbound copy failed")?;
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
