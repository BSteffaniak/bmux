use super::*;
use anyhow::Context;
use bmux_config::{ConnectionTargetConfig, ConnectionTransport, RemoteServerStartMode};
use bmux_ipc::IpcEndpoint;
use bmux_ipc::transport::{ErasedIpcStream, LocalIpcStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::presets};
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command as TokioProcessCommand};
use tokio::time::sleep;
use tokio_rustls::TlsConnector;

#[derive(Debug, Clone)]
enum ResolvedTarget {
    Local,
    Ssh(SshTarget),
    Tls(TlsTarget),
    Iroh(IrohTarget),
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

#[derive(Debug, Clone)]
struct IrohTarget {
    label: String,
    endpoint_id: String,
    relay_url: Option<String>,
    connect_timeout_ms: u64,
}

const SSH_RECONNECT_MAX_ATTEMPTS: usize = 4;
const SSH_RECONNECT_BASE_BACKOFF_MS: u64 = 300;
const BRIDGE_PREFLIGHT_TOKEN: &str = "BMUX_BRIDGE_READY";
const RECENT_CACHE_MAX: usize = 10;
const BMUX_IROH_ALPN: &[u8] = b"bmux/gateway/iroh/1";
const DEFAULT_CONTROL_PLANE_URL: &str = "https://api.bmux.run";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthState {
    access_token: String,
    account_id: Option<String>,
    account_name: Option<String>,
    expires_at_unix: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct WhoAmIResponse {
    account_id: Option<String>,
    account_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct DeviceStartResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval_seconds: Option<u64>,
    expires_in: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct DevicePollRequest {
    device_code: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DevicePollResponse {
    status: Option<String>,
    access_token: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CreateShareRequest {
    name: String,
    target: String,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
    #[serde(default)]
    one_time: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ShareLinkResponse {
    name: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RegisterHostRequest {
    name: Option<String>,
    target: String,
}

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

pub(super) async fn should_proxy_to_target(cli: &Cli) -> Result<bool> {
    let Some(command) = cli.command.as_ref() else {
        return Ok(false);
    };
    if matches!(command, Command::Connect { .. } | Command::Remote { .. }) {
        return Ok(false);
    }
    let config = BmuxConfig::load()?;
    let target = resolve_effective_target(&config, cli.target.as_deref()).await?;
    Ok(matches!(target, ResolvedTarget::Ssh(_)))
}

pub(super) async fn run_target_proxy_from_current_argv(cli: &Cli) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let target = resolve_effective_target(&config, cli.target.as_deref()).await?;
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
        ResolvedTarget::Iroh(target) => {
            anyhow::bail!(
                "unexpected iroh target proxy path for '{}'; this should route through direct client transport",
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
    target: Option<&str>,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
) -> Result<u8> {
    if session.is_some() && follow.is_some() {
        anyhow::bail!("--follow cannot be used with an explicit session argument");
    }

    let config = BmuxConfig::load()?;
    let selected_target = if let Some(target) = target {
        target.to_string()
    } else {
        choose_default_target_interactively(&config)?
    };
    let resolved = resolve_target_reference(&config, &selected_target)
        .await
        .map_err(|error| map_connect_target_resolution_error(&selected_target, error))?;
    match resolved {
        ResolvedTarget::Local => {
            let target_session = if let Some(session) = session {
                Some(session.to_string())
            } else {
                resolve_local_attach_session().await?
            };
            let status = run_session_attach(
                target_session.as_deref(),
                follow,
                global,
                ConnectionContext::new(Some("local")),
            )
            .await?;
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
        ResolvedTarget::Iroh(iroh_target) => {
            let mut client =
                connect_iroh_bridge(&iroh_target, "bmux-cli-connect-remote-iroh").await?;
            let target_session = if follow.is_some() {
                None
            } else if let Some(session) = session {
                Some(session.to_string())
            } else {
                resolve_remote_attach_session(&mut client, &iroh_target.label).await?
            };
            let status = run_iroh_attach_with_reconnect(
                client,
                &iroh_target,
                target_session.as_deref(),
                follow,
                global,
                reconnect_forever,
            )
            .await?;
            if status == 0 {
                remember_recent_selection(&iroh_target.label, target_session.as_deref())?;
            }
            Ok(status)
        }
    }
}

fn map_connect_target_resolution_error(target: &str, error: anyhow::Error) -> anyhow::Error {
    if target.starts_with("bmux://") && error.to_string().contains("share link not found:") {
        return anyhow::anyhow!(
            "{}\nHint: run 'bmux hosts' to list known links, or ask the owner to share it again with 'bmux share'.",
            error
        );
    }
    error
}

pub(super) async fn run_setup() -> Result<u8> {
    println!("bmux setup");
    println!("Step 1/2: auth");
    let _ = ensure_authenticated(&BmuxConfig::load()?).await?;
    println!("Step 2/2: host");
    run_host("127.0.0.1:7443", None, false).await
}

pub(super) async fn run_host(listen: &str, name: Option<&str>, copy: bool) -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    let control_plane_url = control_plane_url(&config);
    let auth_state = ensure_authenticated(&config).await?;

    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![BMUX_IROH_ALPN.to_vec()])
        .bind()
        .await
        .context("failed binding iroh endpoint")?;
    endpoint.online().await;
    let addr = endpoint.addr();
    let endpoint_id = endpoint.id();
    let relay = addr.relay_urls().next().map(|value| value.to_string());
    let target = relay.as_ref().map_or_else(
        || format!("iroh://{endpoint_id}"),
        |relay_url| format!("iroh://{endpoint_id}?relay={relay_url}"),
    );

    let share_name = suggest_share_name(name, &auth_state);
    let share_result = ensure_host_share_link(
        &mut config,
        &control_plane_url,
        &auth_state.access_token,
        &share_name,
        &target,
    )
    .await;
    let resolved_share = match share_result {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!("warning: failed to create share link: {error}");
            None
        }
    };
    if let Err(error) = register_host_presence(
        &control_plane_url,
        &auth_state.access_token,
        name.map(ToString::to_string)
            .or_else(|| resolved_share.clone()),
        &target,
    )
    .await
    {
        eprintln!("warning: host registration failed: {error}");
    }

    let join_link = resolved_share
        .as_ref()
        .map(|value| format!("bmux://{value}"))
        .unwrap_or_else(|| target.clone());
    if copy {
        match crate::runtime::attach::runtime::copy_text_with_clipboard_plugin(&join_link) {
            Ok(()) => println!("copied to clipboard: {join_link}"),
            Err(error) => eprintln!(
                "warning: clipboard copy failed: {}",
                crate::runtime::attach::runtime::format_clipboard_service_error(&error)
            ),
        }
    }

    println!("bmux iroh gateway online");
    if listen != "127.0.0.1:7443" {
        println!("note: --listen is ignored for iroh host mode ({listen})");
    }
    println!("connect URL: {target}");
    if let Some(share) = resolved_share.as_deref() {
        println!("share link: bmux://{share}");
        println!("join: bmux join bmux://{share}");
    } else {
        println!("join: bmux join {target}");
    }

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
                let endpoint = local_ipc_endpoint_from_paths(&ConfigPaths::default());
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

pub(super) async fn run_join(link: Option<&str>, session: Option<&str>) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let target = if let Some(link) = link {
        normalize_join_target_input(link)?
    } else {
        choose_default_target_interactively(&config)?
    };
    let resumed_session = session.or_else(|| {
        config
            .connections
            .recent_sessions
            .get(&target)
            .and_then(|values| values.first())
            .map(String::as_str)
    });
    run_connect(Some(&target), resumed_session, None, false, true).await
}

pub(super) fn run_hosts() -> Result<u8> {
    let config = BmuxConfig::load()?;
    if !config.connections.recent_targets.is_empty() {
        println!("recent:");
        for target in &config.connections.recent_targets {
            println!("- {target}");
        }
    }
    if !config.connections.targets.is_empty() {
        println!("configured targets:");
        for (name, target) in &config.connections.targets {
            let transport = match target.transport {
                ConnectionTransport::Local => "local",
                ConnectionTransport::Ssh => "ssh",
                ConnectionTransport::Tls => "tls",
                ConnectionTransport::Iroh => "iroh",
            };
            println!("- {name} ({transport})");
        }
    }
    if !config.connections.share_links.is_empty() {
        println!("share links:");
        for (name, target) in &config.connections.share_links {
            println!("- bmux://{name} -> {target}");
            println!("  join: bmux join bmux://{name}");
        }
    }
    if config.connections.recent_targets.is_empty()
        && config.connections.targets.is_empty()
        && config.connections.share_links.is_empty()
    {
        println!("no hosts configured");
    }
    Ok(0)
}

pub(super) async fn run_auth_login(no_browser: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let control_plane_url = control_plane_url(&config);
    let token = if let Ok(value) = std::env::var("BMUX_AUTH_TOKEN") {
        value
    } else if io::stdin().is_terminal() {
        let started = start_device_login(&control_plane_url).await?;
        println!("Complete sign-in to continue.");
        println!("URL: {}", started.verification_uri);
        println!("Code: {}", started.user_code);
        if !no_browser {
            if open_browser(&started.verification_uri) {
                println!("Opened browser for sign-in.");
            } else {
                println!("Could not open browser automatically; open the URL manually.");
            }
        }
        println!("Waiting for confirmation...");
        wait_for_device_token(&control_plane_url, &started).await?
    } else {
        anyhow::bail!(
            "BMUX_AUTH_TOKEN is required in non-interactive mode; interactive login supports device flow"
        );
    };

    let whoami = verify_access_token(&control_plane_url, &token).await?;
    let paths = ConfigPaths::default();
    let state = AuthState {
        access_token: token,
        account_id: whoami.account_id,
        account_name: whoami.account_name,
        expires_at_unix: None,
    };
    save_auth_state(&paths, &state)?;
    println!(
        "auth login complete ({})",
        auth_state_path(&paths).display()
    );
    Ok(0)
}

pub(super) fn run_auth_status() -> Result<u8> {
    let paths = ConfigPaths::default();
    let Some(state) = load_auth_state_optional(&paths)? else {
        println!("not authenticated");
        return Ok(1);
    };
    println!("authenticated");
    if let Some(account_name) = state.account_name.as_deref() {
        println!("account: {account_name}");
    }
    if let Some(account_id) = state.account_id.as_deref() {
        println!("account id: {account_id}");
    }
    if let Some(expires_at) = state.expires_at_unix {
        println!("expires_at_unix: {expires_at}");
    }
    println!("state file: {}", auth_state_path(&paths).display());
    Ok(0)
}

pub(super) fn run_auth_logout() -> Result<u8> {
    let paths = ConfigPaths::default();
    let path = auth_state_path(&paths);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!("auth state removed ({})", path.display());
            Ok(0)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("already logged out");
            Ok(0)
        }
        Err(error) => Err(error).with_context(|| format!("failed removing {}", path.display())),
    }
}

pub(super) async fn run_share(
    target: Option<&str>,
    name: Option<&str>,
    role: &str,
    ttl: Option<&str>,
    one_time: bool,
    copy: bool,
) -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    let resolved_target = if let Some(target) = target {
        target.to_string()
    } else if let Some(default_target) = config.connections.default_target.clone() {
        default_target
    } else {
        config
            .connections
            .recent_targets
            .first()
            .cloned()
            .unwrap_or_else(|| "local".to_string())
    };
    let slug = name
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("share-{}", Uuid::new_v4().simple()));

    let control_plane_url = control_plane_url(&config);
    let auth_state = load_auth_state_optional(&ConfigPaths::default())?
        .ok_or_else(|| anyhow::anyhow!("not authenticated. run 'bmux auth login' first"))?;
    let created = create_share_link(
        &control_plane_url,
        &auth_state.access_token,
        &CreateShareRequest {
            name: slug.clone(),
            target: resolved_target.clone(),
            role: role.to_string(),
            ttl: ttl.map(ToString::to_string),
            one_time,
        },
    )
    .await?;

    config
        .connections
        .share_links
        .insert(slug.clone(), resolved_target.clone());
    config.save()?;
    let link_name = created.name.clone().unwrap_or(slug);
    println!("created share link: bmux://{link_name}");
    if let Some(url) = created.url {
        println!("url: {url}");
    }
    println!("target: {resolved_target}");
    println!("role: {role}");
    if let Some(value) = ttl {
        println!("ttl: {value}");
    }
    if one_time {
        println!("one-time: true");
    }
    if copy {
        let share_link = format!("bmux://{link_name}");
        match crate::runtime::attach::runtime::copy_text_with_clipboard_plugin(&share_link) {
            Ok(()) => println!("copied to clipboard: {share_link}"),
            Err(error) => eprintln!(
                "warning: clipboard copy failed: {}",
                crate::runtime::attach::runtime::format_clipboard_service_error(&error)
            ),
        }
    }
    Ok(0)
}

pub(super) async fn run_unshare(name: &str) -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    let control_plane_url = control_plane_url(&config);
    let auth_state = load_auth_state_optional(&ConfigPaths::default())?
        .ok_or_else(|| anyhow::anyhow!("not authenticated. run 'bmux auth login' first"))?;
    delete_share_link(&control_plane_url, &auth_state.access_token, name).await?;

    if config.connections.share_links.remove(name).is_some() {
        config.save()?;
        println!("removed share link bmux://{name}");
        return Ok(0);
    }
    anyhow::bail!("share link not found: bmux://{name}");
}

fn choose_default_target_interactively(config: &BmuxConfig) -> Result<String> {
    if let Some(target) = config.connections.recent_targets.first() {
        return Ok(target.clone());
    }
    if let Some(default_target) = config.connections.default_target.as_deref() {
        return Ok(default_target.to_string());
    }
    if let Some(first_named) = config.connections.targets.keys().next() {
        return Ok(first_named.clone());
    }
    if io::stdin().is_terminal() {
        println!("No recent targets found.");
        print!("Paste an invite link (or press Enter to use local): ");
        io::stdout()
            .flush()
            .context("failed flushing join prompt")?;
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("failed reading join target")?;
        let value = input.trim();
        if !value.is_empty() {
            return normalize_join_target_input(value);
        }
    }
    Ok("local".to_string())
}

fn control_plane_url(config: &BmuxConfig) -> String {
    std::env::var("BMUX_CONTROL_PLANE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.connections.control_plane_url.clone())
        .unwrap_or_else(|| DEFAULT_CONTROL_PLANE_URL.to_string())
}

fn auth_state_path(paths: &ConfigPaths) -> PathBuf {
    paths.runtime_dir.join("auth-state.json")
}

fn load_auth_state_optional(paths: &ConfigPaths) -> Result<Option<AuthState>> {
    let path = auth_state_path(paths);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let state = serde_json::from_str::<AuthState>(&content)
                .with_context(|| format!("failed parsing auth state {}", path.display()))?;
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed reading {}", path.display())),
    }
}

fn save_auth_state(paths: &ConfigPaths, state: &AuthState) -> Result<()> {
    std::fs::create_dir_all(&paths.runtime_dir).with_context(|| {
        format!(
            "failed creating runtime dir {}",
            paths.runtime_dir.display()
        )
    })?;
    let path = auth_state_path(paths);
    let encoded = serde_json::to_string_pretty(state).context("failed serializing auth state")?;
    std::fs::write(&path, encoded).with_context(|| format!("failed writing {}", path.display()))?;
    Ok(())
}

async fn ensure_authenticated(config: &BmuxConfig) -> Result<AuthState> {
    let paths = ConfigPaths::default();
    if let Some(state) = load_auth_state_optional(&paths)? {
        return Ok(state);
    }
    println!("not authenticated; starting login...");
    run_auth_login(false).await?;
    load_auth_state_optional(&paths)?
        .ok_or_else(|| anyhow::anyhow!("auth login succeeded but no auth state was stored"))
        .map(|mut state| {
            if state.account_name.is_none() {
                state.account_name = config
                    .connections
                    .default_target
                    .as_deref()
                    .map(ToString::to_string);
            }
            state
        })
}

fn suggest_share_name(name: Option<&str>, auth_state: &AuthState) -> String {
    if let Some(value) = name
        && !value.trim().is_empty()
    {
        return value.trim().to_string();
    }
    if let Some(account_name) = auth_state.account_name.as_deref() {
        let slug = account_name
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        if !slug.is_empty() {
            return slug;
        }
    }
    format!("host-{}", Uuid::new_v4().simple())
}

async fn ensure_host_share_link(
    config: &mut BmuxConfig,
    control_plane_url: &str,
    token: &str,
    name: &str,
    target: &str,
) -> Result<String> {
    if let Some(mapped) = config.connections.share_links.get(name)
        && mapped == target
    {
        return Ok(name.to_string());
    }
    let created = create_share_link(
        control_plane_url,
        token,
        &CreateShareRequest {
            name: name.to_string(),
            target: target.to_string(),
            role: "control".to_string(),
            ttl: None,
            one_time: false,
        },
    )
    .await?;
    let resolved_name = created.name.unwrap_or_else(|| name.to_string());
    config
        .connections
        .share_links
        .insert(resolved_name.clone(), target.to_string());
    config.save()?;
    Ok(resolved_name)
}

async fn register_host_presence(
    control_plane_url: &str,
    token: &str,
    name: Option<String>,
    target: &str,
) -> Result<()> {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{control_plane_url}/v1/hosts"))
        .bearer_auth(token)
        .json(&RegisterHostRequest {
            name,
            target: target.to_string(),
        })
        .send()
        .await
        .with_context(|| format!("failed contacting {control_plane_url}"))?;
    if response.status().is_success() || response.status().as_u16() == 404 {
        return Ok(());
    }
    anyhow::bail!(
        "host registration failed (status {})",
        response.status().as_u16()
    )
}

fn normalize_join_target_input(link: &str) -> Result<String> {
    let value = link.trim();
    if value.is_empty() {
        anyhow::bail!("join target cannot be empty");
    }
    if let Some(extracted) = extract_target_from_text(value) {
        return Ok(extracted);
    }
    if value.contains("://") {
        return Ok(value.to_string());
    }
    if value.contains(char::is_whitespace) {
        anyhow::bail!("could not find a valid invite link in input");
    }
    Ok(format!("bmux://{value}"))
}

fn extract_target_from_text(value: &str) -> Option<String> {
    value
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| "()[]{}<>,.;\"'".contains(ch)))
        .find_map(|token| {
            if token.starts_with("bmux://")
                || token.starts_with("iroh://")
                || token.starts_with("https://")
                || token.starts_with("ssh://")
                || token.starts_with("tls://")
            {
                Some(token.to_string())
            } else {
                None
            }
        })
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        return std::process::Command::new("open")
            .arg(url)
            .status()
            .is_ok_and(|status| status.success());
    }
    #[cfg(target_os = "windows")]
    {
        return std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()
            .is_ok_and(|status| status.success());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .status()
            .is_ok_and(|status| status.success())
    }
}

async fn start_device_login(control_plane_url: &str) -> Result<DeviceStartResponse> {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{control_plane_url}/v1/auth/device/start"))
        .send()
        .await
        .with_context(|| format!("failed contacting {control_plane_url}"))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "device login start failed (status {})",
            response.status().as_u16()
        );
    }
    response
        .json::<DeviceStartResponse>()
        .await
        .context("failed parsing device login response")
}

async fn wait_for_device_token(
    control_plane_url: &str,
    started: &DeviceStartResponse,
) -> Result<String> {
    let mut interval = started.interval_seconds.unwrap_or(2).max(1);
    let expires_after = started.expires_in.unwrap_or(600);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(expires_after);
    loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("device login expired; run 'bmux auth login' again")
        }
        let result = poll_device_login(control_plane_url, &started.device_code).await?;
        let status = result.status.as_deref().unwrap_or("approved");
        match status {
            "approved" | "complete" => {
                if let Some(token) = result.access_token {
                    return Ok(token);
                }
                anyhow::bail!("device login response missing access token")
            }
            "pending" | "authorization_pending" => {}
            "slow_down" => {
                interval += 1;
            }
            "denied" | "access_denied" => {
                anyhow::bail!("device login denied")
            }
            "expired" | "expired_token" => {
                anyhow::bail!("device login expired; run 'bmux auth login' again")
            }
            other => {
                if let Some(error) = result.error.as_deref() {
                    anyhow::bail!("device login failed: {error}")
                }
                anyhow::bail!("device login failed with status {other}")
            }
        }
        sleep(std::time::Duration::from_secs(interval)).await;
    }
}

async fn poll_device_login(
    control_plane_url: &str,
    device_code: &str,
) -> Result<DevicePollResponse> {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{control_plane_url}/v1/auth/device/poll"))
        .json(&DevicePollRequest {
            device_code: device_code.to_string(),
        })
        .send()
        .await
        .with_context(|| format!("failed contacting {control_plane_url}"))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "device login poll failed (status {})",
            response.status().as_u16()
        );
    }
    response
        .json::<DevicePollResponse>()
        .await
        .context("failed parsing device poll response")
}

fn local_ipc_endpoint_from_paths(paths: &ConfigPaths) -> IpcEndpoint {
    #[cfg(unix)]
    {
        IpcEndpoint::unix_socket(paths.server_socket())
    }
    #[cfg(windows)]
    {
        IpcEndpoint::windows_named_pipe(paths.server_named_pipe())
    }
}

async fn verify_access_token(control_plane_url: &str, token: &str) -> Result<WhoAmIResponse> {
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{control_plane_url}/v1/auth/whoami"))
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("failed contacting {control_plane_url}"))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "authentication failed (status {})",
            response.status().as_u16()
        );
    }
    response
        .json::<WhoAmIResponse>()
        .await
        .context("failed parsing auth response")
}

async fn create_share_link(
    control_plane_url: &str,
    token: &str,
    request: &CreateShareRequest,
) -> Result<ShareLinkResponse> {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{control_plane_url}/v1/share-links"))
        .bearer_auth(token)
        .json(request)
        .send()
        .await
        .with_context(|| format!("failed contacting {control_plane_url}"))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "share creation failed (status {})",
            response.status().as_u16()
        );
    }
    response
        .json::<ShareLinkResponse>()
        .await
        .context("failed parsing share response")
}

async fn delete_share_link(control_plane_url: &str, token: &str, name: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let response = client
        .delete(format!("{control_plane_url}/v1/share-links/{name}"))
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("failed contacting {control_plane_url}"))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "share removal failed (status {})",
            response.status().as_u16()
        );
    }
    Ok(())
}

async fn run_iroh_attach_with_reconnect(
    mut client: BmuxClient,
    target: &IrohTarget,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
) -> Result<u8> {
    let mut attempt = 0usize;
    loop {
        let outcome = run_session_attach_with_client(client, session, follow, global).await?;
        if outcome.exit_reason != AttachExitReason::StreamClosed {
            return Ok(outcome.status_code);
        }
        if !reconnect_forever && attempt >= SSH_RECONNECT_MAX_ATTEMPTS {
            println!(
                "remote iroh connection closed; giving up after {} reconnect attempts",
                SSH_RECONNECT_MAX_ATTEMPTS
            );
            return Ok(1);
        }
        attempt = attempt.saturating_add(1);
        let backoff = Duration::from_millis(reconnect_backoff_ms(attempt));
        println!(
            "remote iroh connection closed; reconnecting to '{}' (attempt {attempt}/{}) in {}ms...",
            target.label,
            SSH_RECONNECT_MAX_ATTEMPTS,
            backoff.as_millis()
        );
        tokio::time::sleep(backoff).await;
        client = connect_iroh_bridge(target, "bmux-cli-connect-remote-iroh-reconnect").await?;
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
        let outcome = run_session_attach_with_client(client, session, follow, global).await?;
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
        let outcome = run_session_attach_with_client(client, session, follow, global).await?;
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
                ConnectionTransport::Iroh => "iroh",
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
    let resolved = resolve_target_reference(&config, target).await?;
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
        ResolvedTarget::Iroh(iroh_target) => {
            let mut client = connect_iroh_bridge(&iroh_target, "bmux-cli-remote-test-iroh").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            println!("target '{}' OK (iroh)", iroh_target.label);
            Ok(0)
        }
    }
}

pub(super) async fn run_remote_doctor(target: &str, fix: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target).await?;
    println!("remote doctor: target='{target}' fix={fix}");
    match resolved {
        ResolvedTarget::Local => {
            print_doctor_step_ok("transport", "local");
            let mut client =
                connect(ConnectionPolicyScope::Normal, "bmux-cli-remote-doctor").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            print_doctor_step_ok("server", "local server reachable");
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
                print_doctor_step_ok("ssh", stderr.trim());
            }
            if let Err(error) =
                run_ssh_bmux_command(&ssh_target, &[OsString::from("--version")], false)
            {
                if fix {
                    print_doctor_step_warn(
                        "bmux",
                        "remote bmux missing/unhealthy; attempting install-server fix",
                    );
                    run_remote_install_server_for_target(&ssh_target).await?;
                    print_doctor_step_ok("bmux", "install-server fix succeeded");
                } else {
                    return Err(error);
                }
            } else {
                print_doctor_step_ok("bmux", "remote bmux binary is available");
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
            print_doctor_step_ok("server", "remote server status check succeeded");
            println!("doctor result: OK ({})", ssh_target.label);
            Ok(0)
        }
        ResolvedTarget::Tls(tls_target) => {
            let mut client = connect_tls_bridge(&tls_target, "bmux-cli-remote-doctor-tls").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            print_doctor_step_ok(
                "tls",
                &format!(
                    "handshake and ping succeeded ({}:{})",
                    tls_target.host, tls_target.port
                ),
            );
            println!("doctor result: OK ({})", tls_target.label);
            Ok(0)
        }
        ResolvedTarget::Iroh(iroh_target) => {
            let mut client =
                connect_iroh_bridge(&iroh_target, "bmux-cli-remote-doctor-iroh").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            print_doctor_step_ok("iroh", "connectivity and ping succeeded");
            println!("doctor result: OK ({})", iroh_target.label);
            Ok(0)
        }
    }
}

pub(super) async fn run_remote_init(
    name: &str,
    ssh: Option<&str>,
    tls: Option<&str>,
    iroh: Option<&str>,
    user: Option<&str>,
    port: Option<u16>,
    set_default: bool,
) -> Result<u8> {
    let selected =
        usize::from(ssh.is_some()) + usize::from(tls.is_some()) + usize::from(iroh.is_some());
    if selected == 0 {
        anyhow::bail!("remote init requires one of --ssh, --tls, or --iroh");
    }
    if selected > 1 {
        anyhow::bail!("remote init accepts only one transport selector (--ssh, --tls, or --iroh)");
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
    if let Some(iroh_value) = iroh {
        target.transport = ConnectionTransport::Iroh;
        let (endpoint_id, relay_url) =
            if let Some((endpoint, relay)) = iroh_value.split_once("?relay=") {
                (endpoint.to_string(), Some(relay.to_string()))
            } else {
                (iroh_value.to_string(), None)
            };
        target.endpoint_id = Some(endpoint_id.clone());
        target.host = Some(endpoint_id);
        target.relay_url = relay_url;
        target.port = None;
        target.user = None;
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
    let resolved = resolve_target_reference(&config, target).await?;
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
        ResolvedTarget::Iroh(_) => {
            anyhow::bail!(
                "install-server is not supported for iroh targets; run install on the host machine"
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
        let resolved = resolve_target_reference(&config, target).await?;
        match resolved {
            ResolvedTarget::Ssh(ssh_target) => {
                run_remote_upgrade_for_target(&ssh_target)?;
                println!("remote upgrade completed for '{}'", ssh_target.label);
                return Ok(0);
            }
            ResolvedTarget::Tls(_) => {
                anyhow::bail!("remote upgrade currently supports SSH targets only");
            }
            ResolvedTarget::Iroh(_) => {
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

pub(super) fn run_remote_complete_targets() -> Result<u8> {
    let config = BmuxConfig::load()?;
    let mut names = config
        .connections
        .targets
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    names.sort_by_key(|name| {
        config
            .connections
            .recent_targets
            .iter()
            .position(|value| value == name)
            .unwrap_or(usize::MAX)
    });
    for name in names {
        println!("{name}");
    }
    Ok(0)
}

pub(super) async fn run_remote_complete_sessions(target: &str) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target).await?;
    let mut client = match resolved {
        ResolvedTarget::Local => {
            connect_with_context(
                ConnectionPolicyScope::Normal,
                "bmux-cli-complete-sessions-local",
                ConnectionContext::new(Some("local")),
            )
            .await?
        }
        ResolvedTarget::Ssh(ssh_target) => {
            connect_remote_bridge(&ssh_target, "bmux-cli-complete-sessions-ssh").await?
        }
        ResolvedTarget::Tls(tls_target) => {
            connect_tls_bridge(&tls_target, "bmux-cli-complete-sessions-tls").await?
        }
        ResolvedTarget::Iroh(iroh_target) => {
            connect_iroh_bridge(&iroh_target, "bmux-cli-complete-sessions-iroh").await?
        }
    };
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;
    let ordered = sessions_ordered_by_recent(target, &sessions)?;
    for session in ordered {
        let value = session.name.unwrap_or_else(|| session.id.to_string());
        println!("{value}");
    }
    Ok(0)
}

fn print_doctor_step_ok(step: &str, message: &str) {
    println!("[OK] {step}: {message}");
}

fn print_doctor_step_warn(step: &str, message: &str) {
    println!("[WARN] {step}: {message}");
}

async fn resolve_local_attach_session() -> Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "session argument is required in non-interactive mode.\nList sessions: bmux list-sessions"
        );
    }
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-connect-local-picker",
        ConnectionContext::new(Some("local")),
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

async fn connect_iroh_bridge(target: &IrohTarget, client_name: &str) -> Result<BmuxClient> {
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![BMUX_IROH_ALPN.to_vec()])
        .bind()
        .await
        .context("failed binding iroh client endpoint")?;
    endpoint.online().await;
    let endpoint_id: EndpointId = target
        .endpoint_id
        .parse()
        .with_context(|| format!("invalid iroh endpoint id '{}'", target.endpoint_id))?;
    let remote_addr = if let Some(relay_url) = target.relay_url.as_deref() {
        let relay = relay_url
            .parse()
            .with_context(|| format!("invalid iroh relay url '{relay_url}'"))?;
        EndpointAddr::new(endpoint_id).with_relay_url(relay)
    } else {
        EndpointAddr::new(endpoint_id)
    };
    let connection = tokio::time::timeout(
        Duration::from_millis(target.connect_timeout_ms.max(1)),
        endpoint.connect(remote_addr, BMUX_IROH_ALPN),
    )
    .await
    .with_context(|| format!("timed out connecting iroh target '{}'", target.label))?
    .with_context(|| format!("failed connecting iroh target '{}'", target.label))?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("failed opening iroh bi-directional stream")?;
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge_stream);
    tokio::spawn(async move {
        if let Err(error) = tokio::io::copy(&mut recv, &mut bridge_write).await {
            tracing::debug!(?error, "iroh bridge recv->client copy failed");
        }
        let _ = bridge_write.shutdown().await;
    });
    tokio::spawn(async move {
        if let Err(error) = tokio::io::copy(&mut bridge_read, &mut send).await {
            tracing::debug!(?error, "iroh bridge client->send copy failed");
        }
        let _ = send.finish();
    });
    let timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
    let principal_id = load_or_create_local_principal_id(&ConfigPaths::default())?;
    BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(client_stream)),
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

async fn resolve_effective_target(
    config: &BmuxConfig,
    cli_target: Option<&str>,
) -> Result<ResolvedTarget> {
    if let Some(value) = cli_target {
        return resolve_target_reference(config, value).await;
    }
    if let Ok(value) = std::env::var("BMUX_TARGET")
        && !value.trim().is_empty()
    {
        return resolve_target_reference(config, value.trim()).await;
    }
    if let Some(default) = config.connections.default_target.as_deref()
        && !default.trim().is_empty()
    {
        return resolve_target_reference(config, default.trim()).await;
    }
    Ok(ResolvedTarget::Local)
}

async fn resolve_target_reference(config: &BmuxConfig, target: &str) -> Result<ResolvedTarget> {
    let target = expand_bmux_target_if_needed(config, target).await?;
    resolve_target_reference_inner(config, &target)
}

fn resolve_target_reference_inner(config: &BmuxConfig, target: &str) -> Result<ResolvedTarget> {
    if target.trim().is_empty() || target == "local" {
        return Ok(ResolvedTarget::Local);
    }
    if let Some(name) = target.trim().strip_prefix("bmux://") {
        let mapped = config.connections.share_links.get(name).ok_or_else(|| {
            anyhow::anyhow!("share link not found: bmux://{name}; run 'bmux share' or 'bmux hosts'")
        })?;
        return resolve_target_reference_inner(config, mapped);
    }
    if let Some(named) = config.connections.targets.get(target) {
        return resolve_named_target(target, named);
    }
    if target.trim().starts_with("https://") {
        return parse_https_target(target);
    }
    if target.trim().starts_with("iroh://") {
        return parse_iroh_target(target);
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
        ConnectionTransport::Iroh => {
            let endpoint_id = target
                .endpoint_id
                .as_deref()
                .or(target.host.as_deref())
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("iroh target '{name}' requires endpoint_id"))?
                .to_string();
            Ok(ResolvedTarget::Iroh(IrohTarget {
                label: name.to_string(),
                endpoint_id,
                relay_url: target.relay_url.clone(),
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

fn parse_https_target(target: &str) -> Result<ResolvedTarget> {
    let raw = target
        .trim()
        .strip_prefix("https://")
        .ok_or_else(|| anyhow::anyhow!("hosted target must start with https://"))?;
    let host = raw.split('/').next().unwrap_or_default();
    if host.trim().is_empty() {
        anyhow::bail!("hosted target must include a host");
    }
    let (host, port) = parse_host_port_with_default(host, 443)?;
    Ok(ResolvedTarget::Tls(TlsTarget {
        label: target.to_string(),
        host: host.clone(),
        port,
        server_name: host,
        ca_file: None,
        connect_timeout_ms: 8_000,
    }))
}

fn parse_iroh_target(target: &str) -> Result<ResolvedTarget> {
    let raw = target
        .trim()
        .strip_prefix("iroh://")
        .ok_or_else(|| anyhow::anyhow!("iroh target must start with iroh://"))?;
    let (endpoint_id, relay_url) = if let Some((endpoint, relay)) = raw.split_once("?relay=") {
        (endpoint.to_string(), Some(relay.to_string()))
    } else {
        (raw.to_string(), None)
    };
    if endpoint_id.trim().is_empty() {
        anyhow::bail!("iroh target must include an endpoint id");
    }
    Ok(ResolvedTarget::Iroh(IrohTarget {
        label: target.to_string(),
        endpoint_id,
        relay_url,
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
    use serial_test::serial;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_ref() {
                unsafe { std::env::set_var(self.key, previous) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "bmux-remote-cli-tests-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

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
                host: false,
                host_mode: bmux_cli_schema::GatewayHostMode::Iroh,
                host_relay: "nokey@localhost.run".to_string(),
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

    #[test]
    fn normalize_join_target_input_promotes_plain_name_to_bmux_link() {
        let normalized = normalize_join_target_input("team-dev").expect("normalize link");
        assert_eq!(normalized, "bmux://team-dev");
    }

    #[test]
    fn normalize_join_target_input_extracts_embedded_link_from_text() {
        let normalized = normalize_join_target_input("Invite code: (bmux://demo-host), join now")
            .expect("normalize link");
        assert_eq!(normalized, "bmux://demo-host");
    }

    #[test]
    fn connect_target_resolution_error_adds_share_link_hint() {
        let error = anyhow::anyhow!("share link not found: bmux://demo");
        let mapped = map_connect_target_resolution_error("bmux://demo", error);
        assert!(mapped.to_string().contains("run 'bmux hosts'"));
    }

    #[tokio::test]
    #[serial]
    async fn should_proxy_to_target_resolves_bmux_target_via_control_plane() {
        let runtime_dir = TempDirGuard::new("proxy-control-plane-runtime");
        let config_dir = TempDirGuard::new("proxy-control-plane-config");
        let data_dir = TempDirGuard::new("proxy-control-plane-data");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());
        let _config_guard = EnvVarGuard::set("BMUX_CONFIG_DIR", config_dir.path());
        let _data_guard = EnvVarGuard::set("BMUX_DATA_DIR", data_dir.path());
        let _target_guard = EnvVarGuard::set("BMUX_TARGET", "bmux://demo");

        let auth_state_path = runtime_dir.path().join("auth-state.json");
        std::fs::write(&auth_state_path, r#"{"access_token":"token-123"}"#)
            .expect("write auth state");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock control plane");
        let address = listener.local_addr().expect("listener addr");
        let control_plane_url = format!("http://{}", address);
        let _control_plane_guard = EnvVarGuard::set("BMUX_CONTROL_PLANE_URL", &control_plane_url);

        let (request_tx, request_rx) = oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept connection");
            let mut buffer = [0_u8; 4096];
            let bytes_read = socket.read(&mut buffer).await.expect("read request");
            let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
            let _ = request_tx.send(request);

            let body = r#"{"target":"ssh://alice@example.com"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let cli = Cli {
            record: false,
            no_capture_input: false,
            recording_id_file: None,
            record_profile: None,
            record_event_kind: Vec::new(),
            stop_server_on_exit: false,
            target: None,
            core_builtins_only: false,
            command: Some(Command::ListSessions { json: false }),
            verbose: false,
            log_level: None,
        };

        assert!(
            should_proxy_to_target(&cli)
                .await
                .expect("resolve proxy target")
        );

        let request = request_rx.await.expect("capture request");
        assert!(request.contains("GET /v1/share-links/demo HTTP/1.1"));
    }

    #[tokio::test]
    #[serial]
    async fn should_proxy_to_target_does_not_proxy_when_control_plane_denies_lookup() {
        let runtime_dir = TempDirGuard::new("proxy-control-plane-denied-runtime");
        let config_dir = TempDirGuard::new("proxy-control-plane-denied-config");
        let data_dir = TempDirGuard::new("proxy-control-plane-denied-data");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());
        let _config_guard = EnvVarGuard::set("BMUX_CONFIG_DIR", config_dir.path());
        let _data_guard = EnvVarGuard::set("BMUX_DATA_DIR", data_dir.path());
        let _target_guard = EnvVarGuard::set("BMUX_TARGET", "bmux://demo");

        let auth_state_path = runtime_dir.path().join("auth-state.json");
        std::fs::write(&auth_state_path, r#"{"access_token":"token-123"}"#)
            .expect("write auth state");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock control plane");
        let address = listener.local_addr().expect("listener addr");
        let control_plane_url = format!("http://{}", address);
        let _control_plane_guard = EnvVarGuard::set("BMUX_CONTROL_PLANE_URL", &control_plane_url);

        let (request_tx, request_rx) = oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept connection");
            let mut buffer = [0_u8; 4096];
            let bytes_read = socket.read(&mut buffer).await.expect("read request");
            let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
            let _ = request_tx.send(request);

            let response =
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let cli = Cli {
            record: false,
            no_capture_input: false,
            recording_id_file: None,
            record_profile: None,
            record_event_kind: Vec::new(),
            stop_server_on_exit: false,
            target: None,
            core_builtins_only: false,
            command: Some(Command::ListSessions { json: false }),
            verbose: false,
            log_level: None,
        };

        let error = should_proxy_to_target(&cli)
            .await
            .expect_err("lookup denial should not proxy to ssh");
        assert!(
            error
                .to_string()
                .contains("share link not found: bmux://demo")
        );

        let request = request_rx.await.expect("capture request");
        assert!(request.contains("GET /v1/share-links/demo HTTP/1.1"));
    }
}
