use super::*;
use anyhow::Context;
use bmux_config::{ConnectionTargetConfig, ConnectionTransport, RemoteServerStartMode};
use bmux_ipc::transport::ErasedIpcStream;
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use std::ffi::OsString;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command as TokioProcessCommand};
use tokio_rustls::TlsConnector;

#[derive(Debug, Clone)]
enum ResolvedTarget {
    Local,
    Ssh(SshTarget),
    Tls(TlsTarget),
}

#[derive(Debug, Clone)]
struct SshTarget {
    label: String,
    host: String,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<PathBuf>,
    known_hosts_file: Option<PathBuf>,
    strict_host_key_checking: bool,
    jump: Option<String>,
    remote_bmux_path: String,
    connect_timeout_ms: u64,
    server_start_mode: RemoteServerStartMode,
}

#[derive(Debug, Clone)]
struct TlsTarget {
    label: String,
    host: String,
    port: u16,
    server_name: String,
    ca_file: Option<PathBuf>,
    connect_timeout_ms: u64,
}

const SSH_RECONNECT_MAX_ATTEMPTS: usize = 4;
const SSH_RECONNECT_BASE_BACKOFF_MS: u64 = 300;
const BRIDGE_PREFLIGHT_TOKEN: &str = "BMUX_BRIDGE_READY";
const RECENT_CACHE_MAX: usize = 10;

#[derive(Debug)]
struct SshBridgeStream {
    _child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl AsyncRead for SshBridgeStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshBridgeStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut this.stdin).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.stdin).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.stdin).poll_shutdown(cx)
    }
}

pub(super) fn should_proxy_to_target(cli: &Cli) -> Result<bool> {
    let Some(command) = cli.command.as_ref() else {
        return Ok(false);
    };
    if matches!(command, Command::Connect { .. } | Command::Remote { .. }) {
        return Ok(false);
    }
    let config = BmuxConfig::load()?;
    let target = resolve_effective_target(&config, cli.target.as_deref())?;
    Ok(matches!(target, ResolvedTarget::Ssh(_)))
}

pub(super) async fn run_target_proxy_from_current_argv(cli: &Cli) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let target = resolve_effective_target(&config, cli.target.as_deref())?;
    match target {
        ResolvedTarget::Ssh(target) => {
            let argv = std::env::args_os().collect::<Vec<_>>();
            let remote_args = strip_target_argument(&argv);
            if command_requires_remote_server(cli.command.as_ref()) {
                ensure_remote_server_ready(&target).await?;
            }
            let needs_tty = command_needs_tty(cli.command.as_ref());
            run_ssh_bmux_command(&target, &remote_args, needs_tty)
        }
        ResolvedTarget::Tls(target) => {
            anyhow::bail!(
                "unexpected TLS target proxy path for '{}'; this should route through direct client transport",
                target.label
            );
        }
        ResolvedTarget::Local => Ok(1),
    }
}

fn command_requires_remote_server(command: Option<&Command>) -> bool {
    !matches!(
        command,
        Some(Command::Server {
            command: ServerCommand::Start { .. }
                | ServerCommand::Status { .. }
                | ServerCommand::Gateway { .. }
                | ServerCommand::Bridge { .. }
        })
    )
}

pub(super) async fn run_connect(
    target: &str,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
) -> Result<u8> {
    if session.is_some() && follow.is_some() {
        anyhow::bail!("--follow cannot be used with an explicit session argument");
    }

    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target)?;
    match resolved {
        ResolvedTarget::Local => {
            let target_session = if let Some(session) = session {
                Some(session.to_string())
            } else {
                resolve_local_attach_session().await?
            };
            let status = run_session_attach(target_session.as_deref(), follow, global).await?;
            if status == 0 {
                remember_recent_selection("local", target_session.as_deref())?;
            }
            Ok(status)
        }
        ResolvedTarget::Ssh(ssh_target) => {
            let mut client = connect_remote_bridge(&ssh_target, "bmux-cli-connect-remote").await?;
            let target_session = if follow.is_some() {
                None
            } else if let Some(session) = session {
                Some(session.to_string())
            } else {
                resolve_remote_attach_session(&mut client, &ssh_target.label).await?
            };
            let status = run_remote_attach_with_reconnect(
                client,
                &ssh_target,
                target_session.as_deref(),
                follow,
                global,
                reconnect_forever,
            )
            .await?;
            if status == 0 {
                remember_recent_selection(&ssh_target.label, target_session.as_deref())?;
            }
            Ok(status)
        }
        ResolvedTarget::Tls(tls_target) => {
            let mut client = connect_tls_bridge(&tls_target, "bmux-cli-connect-remote-tls").await?;
            let target_session = if follow.is_some() {
                None
            } else if let Some(session) = session {
                Some(session.to_string())
            } else {
                resolve_remote_attach_session(&mut client, &tls_target.label).await?
            };
            let status = run_tls_attach_with_reconnect(
                client,
                &tls_target,
                target_session.as_deref(),
                follow,
                global,
                reconnect_forever,
            )
            .await?;
            if status == 0 {
                remember_recent_selection(&tls_target.label, target_session.as_deref())?;
            }
            Ok(status)
        }
    }
}

async fn run_tls_attach_with_reconnect(
    mut client: BmuxClient,
    target: &TlsTarget,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
) -> Result<u8> {
    let mut attempt = 0usize;
    loop {
        let outcome = run_session_attach_with_client(client, session, follow, global, None).await?;
        if outcome.exit_reason != AttachExitReason::StreamClosed {
            return Ok(outcome.status_code);
        }
        if !reconnect_forever && attempt >= SSH_RECONNECT_MAX_ATTEMPTS {
            println!(
                "remote TLS connection closed; giving up after {} reconnect attempts",
                SSH_RECONNECT_MAX_ATTEMPTS
            );
            return Ok(1);
        }
        attempt = attempt.saturating_add(1);
        let backoff = Duration::from_millis(reconnect_backoff_ms(attempt));
        println!(
            "remote TLS connection closed; reconnecting to '{}' (attempt {attempt}/{}) in {}ms...",
            target.label,
            SSH_RECONNECT_MAX_ATTEMPTS,
            backoff.as_millis()
        );
        tokio::time::sleep(backoff).await;
        client = connect_tls_bridge(target, "bmux-cli-connect-remote-tls-reconnect").await?;
    }
}

async fn run_remote_attach_with_reconnect(
    mut client: BmuxClient,
    target: &SshTarget,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
) -> Result<u8> {
    let mut attempt = 0usize;
    loop {
        let outcome = run_session_attach_with_client(client, session, follow, global, None).await?;
        if outcome.exit_reason != AttachExitReason::StreamClosed {
            return Ok(outcome.status_code);
        }
        if !reconnect_forever && attempt >= SSH_RECONNECT_MAX_ATTEMPTS {
            println!(
                "remote connection closed; giving up after {} reconnect attempts",
                SSH_RECONNECT_MAX_ATTEMPTS
            );
            return Ok(1);
        }
        attempt = attempt.saturating_add(1);
        let backoff = Duration::from_millis(reconnect_backoff_ms(attempt));
        tracing::debug!(
            target = %target.label,
            attempt,
            backoff_ms = backoff.as_millis(),
            follow = %follow.unwrap_or_default(),
            global,
            "remote attach stream closed; scheduling reconnect"
        );
        println!(
            "remote connection closed; reconnecting to '{}' (attempt {attempt}/{}) in {}ms...",
            target.label,
            SSH_RECONNECT_MAX_ATTEMPTS,
            backoff.as_millis()
        );
        tokio::time::sleep(backoff).await;
        client = connect_remote_bridge(target, "bmux-cli-connect-remote-reconnect").await?;
    }
}

pub(super) fn run_remote_list(as_json: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let mut entries = config
        .connections
        .targets
        .iter()
        .map(|(name, value)| {
            let transport = match value.transport {
                ConnectionTransport::Local => "local",
                ConnectionTransport::Ssh => "ssh",
                ConnectionTransport::Tls => "tls",
            };
            serde_json::json!({
                "name": name,
                "transport": transport,
                "host": value.host,
                "user": value.user,
                "port": value.port,
                "default_session": value.default_session,
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["name"].as_str().unwrap_or_default())
    });

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).context("failed encoding target list")?
        );
        return Ok(0);
    }

    if entries.is_empty() {
        println!("no configured targets");
        return Ok(0);
    }

    for entry in &entries {
        let name = entry["name"].as_str().unwrap_or("-");
        let transport = entry["transport"].as_str().unwrap_or("-");
        let host = entry["host"].as_str().unwrap_or("-");
        let recent = config
            .connections
            .recent_targets
            .iter()
            .position(|value| value == name)
            .map_or("", |_| "* ");
        println!("{recent}{name}\t{transport}\t{host}");
    }
    Ok(0)
}

pub(super) async fn run_remote_test(target: &str) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target)?;
    match resolved {
        ResolvedTarget::Local => {
            let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-remote-test").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            println!("target '{target}' OK (local)");
            Ok(0)
        }
        ResolvedTarget::Ssh(ssh_target) => {
            run_ssh_bmux_command(
                &ssh_target,
                &[
                    OsString::from("server"),
                    OsString::from("status"),
                    OsString::from("--json"),
                ],
                false,
            )?;
            println!("target '{}' OK (ssh)", ssh_target.label);
            Ok(0)
        }
        ResolvedTarget::Tls(tls_target) => {
            let mut client = connect_tls_bridge(&tls_target, "bmux-cli-remote-test-tls").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            println!("target '{}' OK (tls)", tls_target.label);
            Ok(0)
        }
    }
}

pub(super) async fn run_remote_doctor(target: &str, fix: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target)?;
    match resolved {
        ResolvedTarget::Local => {
            println!("target '{target}' transport: local");
            let mut client =
                connect(ConnectionPolicyScope::Normal, "bmux-cli-remote-doctor").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            println!("local server reachable");
            Ok(0)
        }
        ResolvedTarget::Ssh(ssh_target) => {
            let version = ProcessCommand::new("ssh")
                .arg("-V")
                .output()
                .context("failed executing ssh -V")?;
            if !version.status.success() {
                anyhow::bail!("ssh binary unavailable");
            }
            let stderr = String::from_utf8_lossy(&version.stderr);
            if !stderr.trim().is_empty() {
                println!("{}", stderr.trim());
            }
            if let Err(error) =
                run_ssh_bmux_command(&ssh_target, &[OsString::from("--version")], false)
            {
                if fix {
                    println!(
                        "doctor: remote bmux missing/unhealthy; attempting install-server fix..."
                    );
                    run_remote_install_server_for_target(&ssh_target).await?;
                } else {
                    return Err(error);
                }
            }
            run_ssh_bmux_command(
                &ssh_target,
                &[
                    OsString::from("server"),
                    OsString::from("status"),
                    OsString::from("--json"),
                ],
                false,
            )?;
            println!("target '{}' doctor: OK", ssh_target.label);
            Ok(0)
        }
        ResolvedTarget::Tls(tls_target) => {
            let mut client = connect_tls_bridge(&tls_target, "bmux-cli-remote-doctor-tls").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            println!(
                "target '{}' doctor: OK (tls {}:{})",
                tls_target.label, tls_target.host, tls_target.port
            );
            Ok(0)
        }
    }
}

pub(super) async fn run_remote_init(
    name: &str,
    ssh: Option<&str>,
    tls: Option<&str>,
    user: Option<&str>,
    port: Option<u16>,
    set_default: bool,
) -> Result<u8> {
    if ssh.is_none() && tls.is_none() {
        anyhow::bail!("remote init requires one of --ssh or --tls");
    }
    if ssh.is_some() && tls.is_some() {
        anyhow::bail!("remote init accepts either --ssh or --tls (not both)");
    }

    let mut config = BmuxConfig::load()?;
    let mut target = ConnectionTargetConfig::default();
    if let Some(ssh_value) = ssh {
        let (parsed_user, host, parsed_port) = parse_ssh_target_parts(ssh_value)?;
        target.transport = ConnectionTransport::Ssh;
        target.host = Some(host);
        target.user = user
            .map(ToString::to_string)
            .or(parsed_user)
            .or(target.user);
        target.port = port.or(parsed_port).or(Some(22));
    }
    if let Some(tls_value) = tls {
        let (host, parsed_port) = parse_host_port_with_default(tls_value, 443)?;
        target.transport = ConnectionTransport::Tls;
        target.host = Some(host.clone());
        target.server_name = Some(host);
        target.port = Some(port.unwrap_or(parsed_port));
    }

    config.connections.targets.insert(name.to_string(), target);
    if set_default {
        config.connections.default_target = Some(name.to_string());
    }
    config.save()?;

    println!("saved remote target '{name}'");
    let test_status = run_remote_test(name).await?;
    if test_status == 0 {
        println!("remote init validation succeeded for '{name}'");
    }
    Ok(0)
}

pub(super) async fn run_remote_install_server(target: &str) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target)?;
    match resolved {
        ResolvedTarget::Ssh(ssh_target) => {
            run_remote_install_server_for_target(&ssh_target).await?;
            println!("remote install-server completed for '{}'", ssh_target.label);
            Ok(0)
        }
        ResolvedTarget::Tls(_) => {
            anyhow::bail!(
                "install-server is only supported for SSH targets; install and run bmux gateway on the remote host"
            );
        }
        ResolvedTarget::Local => {
            println!("local target does not require remote install");
            Ok(0)
        }
    }
}

pub(super) async fn run_remote_upgrade(target: Option<&str>) -> Result<u8> {
    let config = BmuxConfig::load()?;
    if let Some(target) = target {
        let resolved = resolve_target_reference(&config, target)?;
        match resolved {
            ResolvedTarget::Ssh(ssh_target) => {
                run_remote_upgrade_for_target(&ssh_target)?;
                println!("remote upgrade completed for '{}'", ssh_target.label);
                return Ok(0);
            }
            ResolvedTarget::Tls(_) => {
                anyhow::bail!("remote upgrade currently supports SSH targets only");
            }
            ResolvedTarget::Local => {
                println!("local target does not require remote upgrade");
                return Ok(0);
            }
        }
    }

    let mut upgraded = 0usize;
    for (name, target_config) in &config.connections.targets {
        if target_config.transport != ConnectionTransport::Ssh {
            continue;
        }
        let ResolvedTarget::Ssh(ssh_target) = resolve_named_target(name, target_config)? else {
            continue;
        };
        run_remote_upgrade_for_target(&ssh_target)?;
        upgraded = upgraded.saturating_add(1);
    }
    println!("remote upgrade completed for {upgraded} SSH target(s)");
    Ok(0)
}

async fn resolve_local_attach_session() -> Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "session argument is required in non-interactive mode.\nList sessions: bmux list-sessions"
        );
    }
    let mut client = connect(
        ConnectionPolicyScope::Normal,
        "bmux-cli-connect-local-picker",
    )
    .await?;
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;
    select_session_interactively("local", &sessions)
}

async fn resolve_remote_attach_session(
    client: &mut BmuxClient,
    target_label: &str,
) -> Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "session argument is required in non-interactive mode.\nList sessions: bmux --target {} list-sessions",
            target_label
        );
    }
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;
    select_session_interactively(target_label, &sessions)
}

fn select_session_interactively(
    label: &str,
    sessions: &[SessionSummary],
) -> Result<Option<String>> {
    let ordered = sessions_ordered_by_recent(label, sessions)?;
    if ordered.is_empty() {
        anyhow::bail!(
            "No sessions found on target '{label}'.\nCreate one: bmux --target {label} new-session <name>"
        );
    }
    if ordered.len() == 1 {
        let selected = &ordered[0];
        let value = selected
            .name
            .clone()
            .unwrap_or_else(|| selected.id.to_string());
        println!("auto-selected session: {value}");
        return Ok(Some(value));
    }

    println!("Available sessions on '{label}':");
    for (index, session) in ordered.iter().enumerate() {
        let name = session
            .name
            .clone()
            .unwrap_or_else(|| session.id.to_string());
        println!("{}: {}", index + 1, name);
    }
    print!("Select session [1-{}] (Enter for 1): ", ordered.len());
    io::stdout().flush().context("failed flushing prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed reading session selection")?;
    let trimmed = input.trim();
    let selection = if trimmed.is_empty() {
        1
    } else {
        trimmed
            .parse::<usize>()
            .context("invalid session selection")?
    };
    if selection == 0 || selection > ordered.len() {
        anyhow::bail!("invalid session selection: {selection}");
    }
    let session = &ordered[selection - 1];
    Ok(Some(
        session
            .name
            .clone()
            .unwrap_or_else(|| session.id.to_string()),
    ))
}

fn sessions_ordered_by_recent(
    label: &str,
    sessions: &[SessionSummary],
) -> Result<Vec<SessionSummary>> {
    let config = BmuxConfig::load()?;
    let recents = config
        .connections
        .recent_sessions
        .get(label)
        .cloned()
        .unwrap_or_default();
    if recents.is_empty() {
        return Ok(sessions.to_vec());
    }
    let mut ordered = sessions.to_vec();
    ordered.sort_by_key(|session| {
        let name = session
            .name
            .clone()
            .unwrap_or_else(|| session.id.to_string());
        recents
            .iter()
            .position(|value| value == &name)
            .unwrap_or(usize::MAX)
    });
    Ok(ordered)
}

fn remember_recent_selection(target: &str, session: Option<&str>) -> Result<()> {
    let mut config = BmuxConfig::load()?;
    push_recent(&mut config.connections.recent_targets, target.to_string());
    if let Some(session) = session {
        let list = config
            .connections
            .recent_sessions
            .entry(target.to_string())
            .or_default();
        push_recent(list, session.to_string());
    }
    config.save()?;
    Ok(())
}

fn push_recent(list: &mut Vec<String>, value: String) {
    list.retain(|entry| entry != &value);
    list.insert(0, value);
    if list.len() > RECENT_CACHE_MAX {
        list.truncate(RECENT_CACHE_MAX);
    }
}

async fn connect_remote_bridge(target: &SshTarget, client_name: &str) -> Result<BmuxClient> {
    ensure_remote_server_ready(target).await?;
    ensure_remote_bridge_stdio_clean(target).await?;
    tracing::debug!(target = %target.label, "launching remote ssh bridge stream");
    let mut command = build_ssh_bridge_command(target);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed launching SSH bridge for {}", target.label))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed acquiring SSH bridge stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed acquiring SSH bridge stdout"))?;
    let bridge_stream = SshBridgeStream {
        _child: child,
        stdin,
        stdout,
    };
    let timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
    let principal_id = load_or_create_local_principal_id(&ConfigPaths::default())?;
    BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(bridge_stream)),
        timeout,
        client_name.to_string(),
        principal_id,
    )
    .await
    .map_err(|error| {
        let mapped = map_cli_client_error(error).to_string();
        if mapped.contains("transport error") {
            anyhow::anyhow!(
                "failed establishing remote bridge with '{}': {mapped}\nif your remote shell prints startup output in non-interactive mode, disable it for ssh command sessions",
                target.label
            )
        } else {
            anyhow::anyhow!(mapped)
        }
    })
}

async fn connect_tls_bridge(target: &TlsTarget, client_name: &str) -> Result<BmuxClient> {
    let connector = build_tls_connector(target)?;
    let address = format!("{}:{}", target.host, target.port);
    let connect_future = TcpStream::connect(&address);
    let tcp_stream = tokio::time::timeout(
        Duration::from_millis(target.connect_timeout_ms.max(1)),
        connect_future,
    )
    .await
    .with_context(|| format!("timed out connecting TLS target '{}'", target.label))?
    .with_context(|| format!("failed connecting TLS target '{}'", target.label))?;
    let server_name = ServerName::try_from(target.server_name.clone())
        .map_err(|_| anyhow::anyhow!("invalid TLS server name '{}'", target.server_name))?;
    let tls_stream = connector
        .connect(server_name, tcp_stream)
        .await
        .with_context(|| format!("TLS handshake failed for target '{}'", target.label))?;
    let timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
    let principal_id = load_or_create_local_principal_id(&ConfigPaths::default())?;
    BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(tls_stream)),
        timeout,
        client_name.to_string(),
        principal_id,
    )
    .await
    .map_err(map_cli_client_error)
}

fn build_tls_connector(target: &TlsTarget) -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    if let Some(error) = native.errors.first() {
        tracing::debug!(?error, "failed loading one or more native TLS certificates");
    }

    if let Some(ca_file) = target.ca_file.as_ref() {
        let pem = std::fs::read(ca_file)
            .with_context(|| format!("failed reading CA bundle {}", ca_file.display()))?;
        let mut reader = std::io::Cursor::new(pem);
        let certs = rustls_pemfile::certs(&mut reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("failed parsing CA bundle {}", ca_file.display()))?;
        for cert in certs {
            roots.add(cert).with_context(|| {
                format!("failed adding CA certificate from {}", ca_file.display())
            })?;
        }
    }

    if roots.is_empty() {
        anyhow::bail!(
            "no TLS trust roots available for target '{}'; install system certs or set ca_file",
            target.label
        );
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

async fn ensure_remote_server_ready(target: &SshTarget) -> Result<()> {
    let status = run_ssh_bmux_command_silent(
        target,
        &[OsString::from("server"), OsString::from("status")],
        false,
    )?;
    if status == 0 {
        return Ok(());
    }

    match target.server_start_mode {
        RemoteServerStartMode::RequireRunning => {
            anyhow::bail!(
                "remote bmux server is not running on '{}' and server_start_mode=require_running.\nstart it with: ssh {} {} server start --daemon",
                target.label,
                ssh_destination(target),
                target.remote_bmux_path
            );
        }
        RemoteServerStartMode::Auto => {
            tracing::debug!(target = %target.label, "remote server missing; attempting auto start");
            println!(
                "remote bmux server is not running on '{}'; starting it automatically...",
                target.label
            );
            let start_status = run_ssh_bmux_command(
                target,
                &[
                    OsString::from("server"),
                    OsString::from("start"),
                    OsString::from("--daemon"),
                ],
                false,
            )?;
            if start_status != 0 {
                anyhow::bail!(
                    "failed to auto-start remote bmux server on '{}'",
                    target.label
                );
            }
            let verify_status = run_ssh_bmux_command_silent(
                target,
                &[OsString::from("server"), OsString::from("status")],
                false,
            )?;
            if verify_status != 0 {
                anyhow::bail!(
                    "remote bmux server on '{}' did not become ready after auto-start",
                    target.label
                );
            }
            Ok(())
        }
    }
}

async fn ensure_remote_bridge_stdio_clean(target: &SshTarget) -> Result<()> {
    let output = run_ssh_bmux_command_capture(
        target,
        &[
            OsString::from("server"),
            OsString::from("bridge"),
            OsString::from("--stdio"),
            OsString::from("--preflight"),
        ],
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed == BRIDGE_PREFLIGHT_TOKEN {
        return Ok(());
    }
    anyhow::bail!(
        "remote bridge preflight failed for '{}': expected '{}' token, got '{}'.\nthis usually means your remote shell writes output for non-interactive SSH commands (MOTD/profile). disable that output for command sessions.",
        target.label,
        BRIDGE_PREFLIGHT_TOKEN,
        trimmed
    );
}

fn load_or_create_local_principal_id(paths: &ConfigPaths) -> Result<Uuid> {
    let path = paths.principal_id_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating principal id dir {}", parent.display()))?;
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let raw = content.trim();
            Uuid::parse_str(raw)
                .with_context(|| format!("invalid principal id in {}: {raw}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let principal_id = Uuid::new_v4();
            std::fs::write(&path, principal_id.to_string())
                .with_context(|| format!("failed writing principal id file {}", path.display()))?;
            Ok(principal_id)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed reading principal id file {}", path.display())),
    }
}

fn run_ssh_bmux_command(target: &SshTarget, args: &[OsString], force_tty: bool) -> Result<u8> {
    run_ssh_bmux_command_inner(target, args, force_tty, true)
}

fn run_ssh_bmux_command_capture(
    target: &SshTarget,
    args: &[OsString],
) -> Result<std::process::Output> {
    let output = build_ssh_command(target, args, false)
        .output()
        .with_context(|| format!("failed executing ssh target {}", target.label))?;
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(map_ssh_execution_error(target, stderr.trim()))
}

async fn run_remote_install_server_for_target(target: &SshTarget) -> Result<()> {
    let mut command = build_ssh_command(
        target,
        &[
            OsString::from("sh"),
            OsString::from("-lc"),
            OsString::from(
                "command -v bmux >/dev/null 2>&1 || cargo install --locked bmux_cli --bin bmux",
            ),
        ],
        false,
    );
    let status = command
        .status()
        .with_context(|| format!("failed running install command on '{}'", target.label))?;
    if !status.success() {
        anyhow::bail!(
            "remote install command failed on '{}'; ensure cargo is installed and reachable on the remote host",
            target.label
        );
    }
    ensure_remote_server_ready(target).await
}

fn run_remote_upgrade_for_target(target: &SshTarget) -> Result<()> {
    let mut command = build_ssh_command(
        target,
        &[
            OsString::from("sh"),
            OsString::from("-lc"),
            OsString::from("cargo install --locked --force bmux_cli --bin bmux"),
        ],
        false,
    );
    let status = command
        .status()
        .with_context(|| format!("failed running upgrade command on '{}'", target.label))?;
    if !status.success() {
        anyhow::bail!(
            "remote upgrade command failed on '{}'; verify cargo/network access on remote host",
            target.label
        );
    }
    Ok(())
}

fn run_ssh_bmux_command_silent(
    target: &SshTarget,
    args: &[OsString],
    force_tty: bool,
) -> Result<u8> {
    run_ssh_bmux_command_inner(target, args, force_tty, false)
}

fn run_ssh_bmux_command_inner(
    target: &SshTarget,
    args: &[OsString],
    force_tty: bool,
    print_stdout: bool,
) -> Result<u8> {
    if !force_tty {
        let output = build_ssh_command(target, args, false)
            .output()
            .with_context(|| format!("failed executing ssh target {}", target.label))?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if print_stdout && !stdout.trim().is_empty() {
                print!("{}", stdout);
            }
            return Ok(0);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(map_ssh_execution_error(target, stderr.trim()));
    }
    let mut command = build_ssh_command(target, args, force_tty);
    let status = command
        .status()
        .with_context(|| format!("failed executing ssh target {}", target.label))?;
    Ok(exit_code_from_status(status))
}

fn build_ssh_command(target: &SshTarget, args: &[OsString], force_tty: bool) -> ProcessCommand {
    let mut command = ProcessCommand::new("ssh");
    if force_tty {
        command.arg("-t");
    }
    if let Some(port) = target.port {
        command.arg("-p");
        command.arg(port.to_string());
    }
    if let Some(path) = target.identity_file.as_ref() {
        command.arg("-i");
        command.arg(path);
    }
    if let Some(jump) = target.jump.as_ref() {
        command.arg("-J");
        command.arg(jump);
    }
    command.arg("-o");
    command.arg(format!(
        "StrictHostKeyChecking={}",
        if target.strict_host_key_checking {
            "yes"
        } else {
            "no"
        }
    ));
    if let Some(known_hosts) = target.known_hosts_file.as_ref() {
        command.arg("-o");
        command.arg(format!("UserKnownHostsFile={}", known_hosts.display()));
    }
    command.arg("-o");
    let timeout_secs = (target.connect_timeout_ms.saturating_add(999)) / 1000;
    command.arg(format!("ConnectTimeout={timeout_secs}"));
    command.arg("-o");
    command.arg("ServerAliveInterval=15");
    command.arg("-o");
    command.arg("ServerAliveCountMax=3");
    command.arg("-o");
    command.arg("BatchMode=yes");
    let destination = target.user.as_ref().map_or_else(
        || target.host.clone(),
        |user| format!("{user}@{}", target.host),
    );
    command.arg(destination);
    command.arg(&target.remote_bmux_path);
    command.args(args);
    command
}

fn build_ssh_bridge_command(target: &SshTarget) -> TokioProcessCommand {
    let mut command = TokioProcessCommand::new("ssh");
    command.arg("-T");
    if let Some(port) = target.port {
        command.arg("-p");
        command.arg(port.to_string());
    }
    if let Some(path) = target.identity_file.as_ref() {
        command.arg("-i");
        command.arg(path);
    }
    if let Some(jump) = target.jump.as_ref() {
        command.arg("-J");
        command.arg(jump);
    }
    command.arg("-o");
    command.arg(format!(
        "StrictHostKeyChecking={}",
        if target.strict_host_key_checking {
            "yes"
        } else {
            "no"
        }
    ));
    if let Some(known_hosts) = target.known_hosts_file.as_ref() {
        command.arg("-o");
        command.arg(format!("UserKnownHostsFile={}", known_hosts.display()));
    }
    command.arg("-o");
    let timeout_secs = (target.connect_timeout_ms.saturating_add(999)) / 1000;
    command.arg(format!("ConnectTimeout={timeout_secs}"));
    command.arg("-o");
    command.arg("ServerAliveInterval=15");
    command.arg("-o");
    command.arg("ServerAliveCountMax=3");
    command.arg("-o");
    command.arg("BatchMode=yes");
    let destination = target.user.as_ref().map_or_else(
        || target.host.clone(),
        |user| format!("{user}@{}", target.host),
    );
    command.arg(destination);
    command.arg(&target.remote_bmux_path);
    command.arg("server");
    command.arg("bridge");
    command.arg("--stdio");
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::inherit());
    command
}

fn map_ssh_execution_error(target: &SshTarget, stderr: &str) -> anyhow::Error {
    if stderr.contains("Host key verification failed") {
        return anyhow::anyhow!(
            "ssh host key verification failed for '{}'. verify known_hosts or set known_hosts_file",
            target.label
        );
    }
    if stderr.contains("Permission denied") {
        return anyhow::anyhow!(
            "ssh authentication failed for '{}'. check user/identity_file and remote access",
            target.label
        );
    }
    if stderr.contains("Could not resolve hostname") {
        return anyhow::anyhow!(
            "ssh target '{}' hostname could not be resolved",
            target.label
        );
    }
    if stderr.contains("Connection timed out") || stderr.contains("Operation timed out") {
        return anyhow::anyhow!("ssh connection to '{}' timed out", target.label);
    }
    if stderr.is_empty() {
        return anyhow::anyhow!("ssh command failed for '{}'", target.label);
    }
    anyhow::anyhow!("ssh command failed for '{}': {stderr}", target.label)
}

fn ssh_destination(target: &SshTarget) -> String {
    target.user.as_ref().map_or_else(
        || target.host.clone(),
        |user| format!("{user}@{}", target.host),
    )
}

fn strip_target_argument(argv: &[OsString]) -> Vec<OsString> {
    if argv.len() <= 1 {
        return Vec::new();
    }
    let mut filtered = Vec::new();
    let mut index = 1;
    while index < argv.len() {
        let value = argv[index].to_string_lossy();
        if value == "--target" {
            index = index.saturating_add(2);
            continue;
        }
        if value.starts_with("--target=") {
            index = index.saturating_add(1);
            continue;
        }
        filtered.push(argv[index].clone());
        index = index.saturating_add(1);
    }
    filtered
}

fn command_needs_tty(command: Option<&Command>) -> bool {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return false;
    }
    matches!(
        command,
        Some(Command::Attach { .. })
            | Some(Command::Session {
                command: SessionCommand::Attach { .. }
            })
    )
}

fn reconnect_backoff_ms(attempt: usize) -> u64 {
    let exponent = attempt.saturating_sub(1).min(10) as u32;
    SSH_RECONNECT_BASE_BACKOFF_MS.saturating_mul(2u64.saturating_pow(exponent))
}

fn exit_code_from_status(status: std::process::ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

fn resolve_effective_target(
    config: &BmuxConfig,
    cli_target: Option<&str>,
) -> Result<ResolvedTarget> {
    if let Some(value) = cli_target {
        return resolve_target_reference(config, value);
    }
    if let Ok(value) = std::env::var("BMUX_TARGET")
        && !value.trim().is_empty()
    {
        return resolve_target_reference(config, value.trim());
    }
    if let Some(default) = config.connections.default_target.as_deref()
        && !default.trim().is_empty()
    {
        return resolve_target_reference(config, default.trim());
    }
    Ok(ResolvedTarget::Local)
}

fn resolve_target_reference(config: &BmuxConfig, target: &str) -> Result<ResolvedTarget> {
    if target.trim().is_empty() || target == "local" {
        return Ok(ResolvedTarget::Local);
    }
    if let Some(named) = config.connections.targets.get(target) {
        return resolve_named_target(target, named);
    }
    if target.trim().starts_with("tls://") {
        return parse_inline_tls_target(target);
    }
    parse_inline_ssh_target(target)
}

fn resolve_named_target(name: &str, target: &ConnectionTargetConfig) -> Result<ResolvedTarget> {
    match target.transport {
        ConnectionTransport::Local => Ok(ResolvedTarget::Local),
        ConnectionTransport::Ssh => {
            let host = target
                .host
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(name)
                .to_string();
            Ok(ResolvedTarget::Ssh(SshTarget {
                label: name.to_string(),
                host,
                user: target.user.clone(),
                port: target.port,
                identity_file: target.identity_file.clone(),
                known_hosts_file: target.known_hosts_file.clone(),
                strict_host_key_checking: target.strict_host_key_checking,
                jump: target.jump.clone(),
                remote_bmux_path: target.remote_bmux_path.clone(),
                connect_timeout_ms: target.connect_timeout_ms.max(1),
                server_start_mode: target.server_start_mode,
            }))
        }
        ConnectionTransport::Tls => {
            let host = target
                .host
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("TLS target '{name}' requires host"))?
                .to_string();
            let port = target.port.unwrap_or(443);
            let server_name = target.server_name.clone().unwrap_or_else(|| host.clone());
            Ok(ResolvedTarget::Tls(TlsTarget {
                label: name.to_string(),
                host,
                port,
                server_name,
                ca_file: target.ca_file.clone(),
                connect_timeout_ms: target.connect_timeout_ms.max(1),
            }))
        }
    }
}

fn parse_inline_ssh_target(target: &str) -> Result<ResolvedTarget> {
    let mut raw = target.trim();
    if let Some(without_scheme) = raw.strip_prefix("ssh://") {
        raw = without_scheme;
    }
    let (user, host_port) = if let Some((user, rest)) = raw.split_once('@') {
        (Some(user.to_string()), rest)
    } else {
        (None, raw)
    };
    let (host, port) = if let Some((host, port_raw)) = host_port.rsplit_once(':') {
        if port_raw.is_empty() {
            (host_port.to_string(), None)
        } else {
            let parsed = port_raw
                .parse::<u16>()
                .with_context(|| format!("invalid SSH port in target '{target}'"))?;
            (host.to_string(), Some(parsed))
        }
    } else {
        (host_port.to_string(), None)
    };
    if host.trim().is_empty() {
        anyhow::bail!("target must include a host");
    }
    Ok(ResolvedTarget::Ssh(SshTarget {
        label: target.to_string(),
        host,
        user,
        port,
        identity_file: None,
        known_hosts_file: None,
        strict_host_key_checking: true,
        jump: None,
        remote_bmux_path: "bmux".to_string(),
        connect_timeout_ms: 8_000,
        server_start_mode: RemoteServerStartMode::Auto,
    }))
}

fn parse_inline_tls_target(target: &str) -> Result<ResolvedTarget> {
    let raw = target
        .trim()
        .strip_prefix("tls://")
        .ok_or_else(|| anyhow::anyhow!("TLS target must start with tls://"))?;
    let (host, port) = if let Some((host, port_raw)) = raw.rsplit_once(':') {
        if port_raw.is_empty() {
            (raw.to_string(), 443)
        } else {
            let parsed = port_raw
                .parse::<u16>()
                .with_context(|| format!("invalid TLS port in target '{target}'"))?;
            (host.to_string(), parsed)
        }
    } else {
        (raw.to_string(), 443)
    };
    if host.trim().is_empty() {
        anyhow::bail!("TLS target must include a host");
    }
    Ok(ResolvedTarget::Tls(TlsTarget {
        label: target.to_string(),
        host: host.clone(),
        port,
        server_name: host,
        ca_file: None,
        connect_timeout_ms: 8_000,
    }))
}

fn parse_ssh_target_parts(target: &str) -> Result<(Option<String>, String, Option<u16>)> {
    let mut raw = target.trim();
    if let Some(without_scheme) = raw.strip_prefix("ssh://") {
        raw = without_scheme;
    }
    let (user, host_port) = if let Some((user, rest)) = raw.split_once('@') {
        (Some(user.to_string()), rest)
    } else {
        (None, raw)
    };
    let (host, port) = if let Some((host, port_raw)) = host_port.rsplit_once(':') {
        if port_raw.is_empty() {
            (host_port.to_string(), None)
        } else {
            let parsed = port_raw
                .parse::<u16>()
                .with_context(|| format!("invalid SSH port in target '{target}'"))?;
            (host.to_string(), Some(parsed))
        }
    } else {
        (host_port.to_string(), None)
    };
    if host.trim().is_empty() {
        anyhow::bail!("target must include a host");
    }
    Ok((user, host, port))
}

fn parse_host_port_with_default(value: &str, default_port: u16) -> Result<(String, u16)> {
    let raw = value.trim();
    if let Some((host, port_raw)) = raw.rsplit_once(':') {
        if port_raw.is_empty() {
            return Ok((raw.to_string(), default_port));
        }
        let port = port_raw
            .parse::<u16>()
            .with_context(|| format!("invalid port in '{value}'"))?;
        if host.trim().is_empty() {
            anyhow::bail!("host is required");
        }
        return Ok((host.to_string(), port));
    }
    if raw.is_empty() {
        anyhow::bail!("host is required");
    }
    Ok((raw.to_string(), default_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_target() -> SshTarget {
        SshTarget {
            label: "prod".to_string(),
            host: "example.com".to_string(),
            user: Some("bmux".to_string()),
            port: Some(2222),
            identity_file: None,
            known_hosts_file: None,
            strict_host_key_checking: true,
            jump: None,
            remote_bmux_path: "bmux".to_string(),
            connect_timeout_ms: 8_000,
            server_start_mode: RemoteServerStartMode::Auto,
        }
    }

    #[test]
    fn strip_target_argument_removes_long_forms() {
        let argv = vec![
            OsString::from("bmux"),
            OsString::from("--target"),
            OsString::from("prod"),
            OsString::from("list-sessions"),
            OsString::from("--target=staging"),
        ];
        let filtered = strip_target_argument(&argv);
        assert_eq!(filtered, vec![OsString::from("list-sessions")]);
    }

    #[test]
    fn parse_inline_ssh_target_accepts_user_host_port() {
        let resolved = parse_inline_ssh_target("alice@example.com:2200").expect("parse target");
        let ResolvedTarget::Ssh(ssh) = resolved else {
            panic!("expected ssh target");
        };
        assert_eq!(ssh.user.as_deref(), Some("alice"));
        assert_eq!(ssh.host, "example.com");
        assert_eq!(ssh.port, Some(2200));
    }

    #[test]
    fn parse_inline_ssh_target_accepts_ssh_scheme() {
        let resolved = parse_inline_ssh_target("ssh://bob@example.com").expect("parse target");
        let ResolvedTarget::Ssh(ssh) = resolved else {
            panic!("expected ssh target");
        };
        assert_eq!(ssh.user.as_deref(), Some("bob"));
        assert_eq!(ssh.host, "example.com");
        assert_eq!(ssh.port, None);
    }

    #[test]
    fn parse_inline_tls_target_accepts_host_and_default_port() {
        let resolved = parse_inline_tls_target("tls://example.com").expect("parse tls target");
        let ResolvedTarget::Tls(tls) = resolved else {
            panic!("expected tls target");
        };
        assert_eq!(tls.host, "example.com");
        assert_eq!(tls.port, 443);
    }

    #[test]
    fn map_ssh_execution_error_highlights_auth_failures() {
        let error = map_ssh_execution_error(&sample_target(), "Permission denied (publickey)");
        assert!(error.to_string().contains("authentication failed"));
    }

    #[test]
    fn command_requires_remote_server_skips_server_start() {
        let command = Command::Server {
            command: ServerCommand::Start {
                daemon: false,
                foreground_internal: false,
            },
        };
        assert!(!command_requires_remote_server(Some(&command)));
    }

    #[test]
    fn command_requires_remote_server_skips_server_gateway() {
        let command = Command::Server {
            command: ServerCommand::Gateway {
                listen: "0.0.0.0:7443".to_string(),
                quick: false,
                cert_file: Some("cert.pem".to_string()),
                key_file: Some("key.pem".to_string()),
            },
        };
        assert!(!command_requires_remote_server(Some(&command)));
    }

    #[test]
    fn command_requires_remote_server_keeps_list_sessions() {
        let command = Command::ListSessions { json: false };
        assert!(command_requires_remote_server(Some(&command)));
    }

    #[test]
    fn reconnect_backoff_grows_exponentially() {
        assert_eq!(reconnect_backoff_ms(1), 300);
        assert_eq!(reconnect_backoff_ms(2), 600);
        assert_eq!(reconnect_backoff_ms(3), 1_200);
    }
}
