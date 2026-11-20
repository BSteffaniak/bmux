//! Standalone hosted workflows. Client ports are bound to the selected invocation,
//! never to a service caller or a remote command process.
mod args;
mod output;
use anyhow::{Context, Result};
use bmux_config::{
    BmuxConfig, ConfigPaths, ConnectionTargetConfig, ConnectionTransport, HostedMode,
};
use output::{
    HostedHostState, hosted_not_ready_reason, status_not_ready_lines, status_ready_lines,
};
use qrcode::{QrCode, render::unicode};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::Arc,
};
use tokio::time::sleep;
use uuid::Uuid;
const RECENT_CACHE_MAX: usize = 10;
/// Attach an explicit endpoint using the client's terminal and reconnect transport.
pub type AttachPort = Arc<
    dyn Fn(
            String,
            Option<String>,
            Option<String>,
            bool,
            bool,
        ) -> Result<(u8, String, Option<String>)>
        + Send
        + Sync,
>;
/// Write text to the selected client clipboard.
pub type ClipboardPort = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;
/// Resources of a standalone client; never installed in a service instance.
#[derive(Clone)]
pub struct HostedClient {
    pub paths: ConfigPaths,
    pub config: BmuxConfig,
    pub runtime_name: String,
    pub admin: Option<crate::remote_admin::AdminPorts>,
    pub copy: ClipboardPort,
    pub attach: AttachPort,
}
fn reqwest_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HostRuntimeState {
    pid: u32,
    target: String,
    share_link: Option<String>,
    name: Option<String>,
    started_at_unix: i64,
}

#[derive(Debug, Clone, Default)]
struct InviteMetadata {
    resolved_target: Option<String>,
    owner: Option<String>,
    role: Option<String>,
    expires_at: Option<String>,
    one_time: Option<bool>,
}

async fn run_setup(client: &HostedClient, check: bool, mode: Option<HostedMode>) -> Result<u8> {
    let mut config = client.config.clone();
    let hosted_mode = resolve_hosted_mode(&config, mode);
    if check {
        return run_setup_check(client, hosted_mode);
    }

    println!("bmux setup");
    let mut auth_state = None;
    if hosted_mode == HostedMode::ControlPlane {
        println!("Step 1/2: auth");
        auth_state = Some(ensure_authenticated(client, &config).await?);
        println!("Step 2/2: host");
    } else {
        println!("Step 1/1: host");
    }
    let _ = spawn_host_daemon(client, "127.0.0.1:7443", None, hosted_mode)?;
    let mut host_state =
        wait_for_running_host_state(client, std::time::Duration::from_secs(5)).await?;

    if hosted_mode == HostedMode::ControlPlane
        && let Some(auth_state) = auth_state.as_ref()
    {
        let repaired_share =
            ensure_setup_share_link(client, &mut config, auth_state, &host_state).await?;
        if host_state.share_link.as_deref() != repaired_share.as_deref() {
            host_state.share_link = repaired_share;
            save_host_runtime_state(&client.paths, &host_state)?;
        }
    }

    let account = if hosted_mode == HostedMode::ControlPlane {
        auth_state
            .as_ref()
            .and_then(|state| state.account_name.clone())
            .or_else(|| config.connections.default_target.clone())
    } else {
        None
    };
    let host_name = host_state.name.as_deref().unwrap_or("host");
    let join_target = host_state
        .share_link
        .as_deref()
        .unwrap_or(host_state.target.as_str());
    for line in format_setup_summary_lines(
        account.as_deref(),
        host_name,
        host_state.share_link.as_deref(),
        join_target,
        hosted_mode == HostedMode::ControlPlane,
    ) {
        println!("{line}");
    }
    for line in status_ready_lines(None) {
        println!("{line}");
    }
    Ok(0)
}

fn run_setup_check(client: &HostedClient, mode: HostedMode) -> Result<u8> {
    let paths = client.paths.clone();
    let auth_state = load_auth_state_optional(&paths)?;
    let host_state = load_host_runtime_state(&paths)?;
    let share_ready = host_state
        .as_ref()
        .and_then(|state| state.share_link.as_deref())
        .is_some();
    let auth_ready = auth_state.is_some();
    let auth_required = mode == HostedMode::ControlPlane;
    let host_alive = host_state
        .as_ref()
        .is_some_and(|state| is_process_alive(state.pid));

    if (!auth_required || auth_ready) && host_alive && (mode == HostedMode::P2p || share_ready) {
        let account = auth_state
            .as_ref()
            .and_then(|state| state.account_name.as_deref());
        let Some(state) = host_state else {
            anyhow::bail!("host runtime status became unavailable during setup check");
        };
        let host_name = state.name.as_deref().unwrap_or("host");
        let join_target = state.share_link.as_deref().unwrap_or(state.target.as_str());
        for line in format_setup_summary_lines(
            account,
            host_name,
            state.share_link.as_deref(),
            join_target,
            auth_required,
        ) {
            println!("{line}");
        }
        for line in status_ready_lines(None) {
            println!("{line}");
        }
        return Ok(0);
    }

    let auth_check = if auth_required {
        if auth_ready {
            SetupAuthCheck::Ready
        } else {
            SetupAuthCheck::RequiredMissing
        }
    } else {
        SetupAuthCheck::NotRequired
    };
    let host_check = if host_alive {
        SetupHostCheck::Running
    } else {
        host_state
            .as_ref()
            .map_or(SetupHostCheck::Offline, |state| {
                SetupHostCheck::Stale(state.pid)
            })
    };
    let share_check = if mode == HostedMode::ControlPlane {
        if share_ready {
            SetupShareCheck::Ready
        } else {
            SetupShareCheck::RequiredMissing
        }
    } else {
        SetupShareCheck::NotRequired
    };

    for line in format_setup_check_not_ready_lines(auth_check, host_check, share_check) {
        println!("{line}");
    }
    Ok(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupAuthCheck {
    RequiredMissing,
    Ready,
    NotRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupHostCheck {
    Offline,
    Stale(u32),
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupShareCheck {
    RequiredMissing,
    Ready,
    NotRequired,
}

fn format_setup_check_not_ready_lines(
    auth_check: SetupAuthCheck,
    host_check: SetupHostCheck,
    share_check: SetupShareCheck,
) -> Vec<String> {
    let host_state = match host_check {
        SetupHostCheck::Running => HostedHostState::Running,
        SetupHostCheck::Offline => HostedHostState::Offline,
        SetupHostCheck::Stale(pid) => HostedHostState::Stale(pid),
    };
    let reason_text = hosted_not_ready_reason(
        auth_check == SetupAuthCheck::RequiredMissing,
        host_state,
        share_check == SetupShareCheck::RequiredMissing,
    );

    let mut lines = status_not_ready_lines(Some(&reason_text), "bmux setup", None);
    if auth_check == SetupAuthCheck::RequiredMissing {
        lines.push("Advanced: bmux auth login".to_string());
    } else if share_check == SetupShareCheck::RequiredMissing
        && host_check == SetupHostCheck::Running
    {
        lines.push("Advanced: bmux share <target> --name <name>".to_string());
    } else {
        match host_check {
            SetupHostCheck::Stale(_) => lines.push("Advanced: bmux host --restart".to_string()),
            SetupHostCheck::Offline => lines.push("Advanced: bmux host --daemon".to_string()),
            SetupHostCheck::Running => {}
        }
    }
    lines
}

fn format_actionable_error_lines(reason: &str, fix: &str, advanced: Option<&str>) -> Vec<String> {
    status_not_ready_lines(Some(reason), fix, advanced)
        .into_iter()
        .skip(1)
        .collect()
}

fn actionable_error(reason: &str, fix: &str, advanced: Option<&str>) -> anyhow::Error {
    anyhow::anyhow!(format_actionable_error_lines(reason, fix, advanced).join("\n"))
}

async fn wait_for_running_host_state(
    client: &HostedClient,
    timeout: std::time::Duration,
) -> Result<HostRuntimeState> {
    let paths = client.paths.clone();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(state) = load_host_runtime_state(&paths)?
            && is_process_alive(state.pid)
        {
            return Ok(state);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "host runtime did not become ready in time; run 'bmux host --status' or retry 'bmux setup'"
            );
        }
        sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn format_setup_summary_lines(
    account_name: Option<&str>,
    host_name: &str,
    share_link: Option<&str>,
    join_target: &str,
    include_auth_line: bool,
) -> Vec<String> {
    let share_url = share_link.unwrap_or("unavailable");
    let mut lines = Vec::new();
    if include_auth_line {
        let account = account_name.unwrap_or("unknown");
        lines.push(format!("Signed in as {account}"));
    }
    lines.push(format!("Host online: {host_name}"));
    lines.push(format!("Share link: {share_url}"));
    lines.push(format!(
        "Join from another machine: bmux join {join_target}"
    ));
    lines
}

const fn resolve_hosted_mode(config: &BmuxConfig, mode: Option<HostedMode>) -> HostedMode {
    match mode {
        Some(HostedMode::P2p) => HostedMode::P2p,
        Some(HostedMode::ControlPlane) => HostedMode::ControlPlane,
        None => config.connections.hosted_mode,
    }
}

async fn run_join(client: &HostedClient, link: Option<&str>, session: Option<&str>) -> Result<u8> {
    let config = client.config.clone();
    let non_interactive_default = link.is_none() && !io::stdin().is_terminal();
    let target = if let Some(link) = link {
        let normalized = normalize_join_target_input(link)?;
        if normalized != link.trim() {
            println!("Resolved invite: {normalized}");
        }
        normalized
    } else {
        choose_default_target_interactively(&config)?
    };
    if non_interactive_default {
        println!("Using default target: {target}");
    }
    let resumed_session = session.or_else(|| {
        config
            .connections
            .recent_sessions
            .get(&target)
            .and_then(|values| values.first())
            .map(String::as_str)
    });
    let invite_metadata = fetch_invite_metadata(client, &config, &target).await;
    print_join_preview(&config, &target, resumed_session, invite_metadata.as_ref());
    confirm_risky_invite(&target, invite_metadata.as_ref())?;
    println!("Connecting...");
    Box::pin(connect(
        client,
        Some(&target),
        resumed_session,
        None,
        false,
        true,
    ))
    .await
}

fn run_hosts(client: &HostedClient, verbose: bool) -> Result<u8> {
    let config = client.config.clone();
    let paths = client.paths.clone();
    let auth_ready = load_auth_state_optional(&paths)?.is_some();
    let host_state = load_host_runtime_state(&paths)?;
    let host_running = host_state
        .as_ref()
        .is_some_and(|state| is_process_alive(state.pid));
    if auth_ready && host_running {
        for line in status_ready_lines(hosts_next_action(&config, host_state.as_ref()).as_deref()) {
            println!("{line}");
        }
    } else {
        let reason = hosts_not_ready_reason(auth_ready, host_running, host_state.as_ref());
        for line in status_not_ready_lines(Some(&reason), "bmux setup", None) {
            println!("{line}");
        }
    }
    if verbose {
        println!("runtime:");
        println!("- name: {}", client.runtime_name);
        println!("- auth: {}", if auth_ready { "ready" } else { "missing" });
        println!(
            "- host: {}",
            if host_running {
                "running".to_string()
            } else if let Some(state) = host_state.as_ref() {
                format!("stale (pid {})", state.pid)
            } else {
                "offline".to_string()
            }
        );
        println!("- local ipc endpoint: {}", local_ipc_endpoint_label(&paths));
        if let Some(state) = host_state.as_ref() {
            println!("- target: {}", state.target);
            if let Some(link) = state.share_link.as_deref() {
                println!("- share link: {link}");
            }
        }
    }

    print_share_links(config.connections.share_links.iter(), verbose);
    print_configured_targets(config.connections.targets.iter(), verbose);
    print_recent_targets(config.connections.recent_targets.iter(), verbose);
    if has_no_saved_hosts(&config) {
        println!("No saved hosts yet.");
        println!("Fix: bmux setup");
        println!("Advanced: bmux host --daemon");
    }
    Ok(0)
}

fn hosts_next_action(config: &BmuxConfig, host_state: Option<&HostRuntimeState>) -> Option<String> {
    if let Some(state) = host_state {
        if let Some(link) = state.share_link.as_deref() {
            return Some(format!("bmux join {link}"));
        }
        return Some(format!("bmux join {}", state.target));
    }
    if let Some((name, _)) = config.connections.share_links.iter().next() {
        return Some(format!("bmux join bmux://{name}"));
    }
    if let Some(target) = config.connections.recent_targets.first() {
        return Some(format!("bmux join {target}"));
    }
    if let Some((name, _)) = config.connections.targets.iter().next() {
        return Some(format!("bmux connect {name}"));
    }
    None
}

fn hosts_not_ready_reason(
    auth_ready: bool,
    host_running: bool,
    host_state: Option<&HostRuntimeState>,
) -> String {
    let host_state = if host_running {
        HostedHostState::Running
    } else if let Some(state) = host_state {
        HostedHostState::Stale(state.pid)
    } else {
        HostedHostState::Offline
    };
    hosted_not_ready_reason(!auth_ready, host_state, false)
}

fn print_share_links<'a>(links: impl Iterator<Item = (&'a String, &'a String)>, verbose: bool) {
    let values = links.collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    println!("share links{}:", if verbose { " (detailed)" } else { "" });
    for (name, target) in values {
        println!("- bmux://{name} (join: bmux join bmux://{name})");
        if verbose {
            println!("  target: {target}");
            println!("  unshare: bmux unshare {name}");
        }
    }
}

fn print_configured_targets<'a>(
    targets: impl Iterator<Item = (&'a String, &'a ConnectionTargetConfig)>,
    verbose: bool,
) {
    let values = targets.collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    println!(
        "configured targets{}:",
        if verbose { " (detailed)" } else { "" }
    );
    for (name, target) in values {
        let transport = match target.transport {
            ConnectionTransport::Local => "local",
            ConnectionTransport::Ssh => "ssh",
            ConnectionTransport::Tls => "tls",
            ConnectionTransport::Iroh => "iroh",
        };
        println!("- {name} (connect: bmux connect {name})");
        if verbose {
            println!("  transport: {transport}");
            match target.transport {
                ConnectionTransport::Local => {}
                ConnectionTransport::Ssh | ConnectionTransport::Tls => {
                    if let Some(host) = target.host.as_deref() {
                        if let Some(port) = target.port {
                            println!("  endpoint: {host}:{port}");
                        } else {
                            println!("  endpoint: {host}");
                        }
                    }
                }
                ConnectionTransport::Iroh => {
                    if let Some(endpoint_id) = target.endpoint_id.as_deref() {
                        println!("  endpoint id: {endpoint_id}");
                    }
                    if let Some(relay_url) = target.relay_url.as_deref() {
                        println!("  relay: {relay_url}");
                    }
                }
            }
        }
    }
}

fn print_recent_targets<'a>(targets: impl Iterator<Item = &'a String>, verbose: bool) {
    let values = targets.collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    println!("recent:");
    for target in values {
        println!("- {target}");
        if verbose {
            println!("  join: bmux join {target}");
        }
    }
}

fn has_no_saved_hosts(config: &BmuxConfig) -> bool {
    config.connections.recent_targets.is_empty()
        && config.connections.targets.is_empty()
        && config.connections.share_links.is_empty()
}

async fn run_auth_login(client: &HostedClient, no_browser: bool) -> Result<u8> {
    let config = client.config.clone();
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
        return Err(actionable_error(
            "BMUX_AUTH_TOKEN is required in non-interactive mode",
            "bmux setup",
            Some("export BMUX_AUTH_TOKEN=<token>"),
        ));
    };

    let whoami = verify_access_token(&control_plane_url, &token).await?;
    let account_name_for_output = whoami.account_name.clone();
    let paths = client.paths.clone();
    let state = AuthState {
        access_token: token,
        account_id: whoami.account_id,
        account_name: whoami.account_name,
        expires_at_unix: None,
    };
    save_auth_state(&paths, &state)?;
    if let Some(account) = account_name_for_output.as_deref() {
        println!("Signed in as {account}");
    } else {
        println!("Signed in");
    }
    println!("auth state: {}", auth_state_path(&paths).display());
    Ok(0)
}

fn run_auth_status(client: &HostedClient) -> Result<u8> {
    let paths = client.paths.clone();
    let Some(state) = load_auth_state_optional(&paths)? else {
        println!("auth: not authenticated");
        println!("Fix: bmux setup");
        println!("Advanced: bmux auth login");
        return Ok(1);
    };
    println!("auth: authenticated");
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

fn run_auth_logout(client: &HostedClient) -> Result<u8> {
    let paths = client.paths.clone();
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

#[allow(clippy::too_many_arguments)]
async fn run_share(
    client: &HostedClient,
    target: Option<&str>,
    secondary: Option<&str>,
    name: Option<&str>,
    role: &str,
    ttl: Option<&str>,
    one_time: bool,
    copy: bool,
    qr: bool,
) -> Result<u8> {
    if target == Some("revoke") {
        let revoke_name = secondary.or(name).ok_or_else(|| {
            actionable_error(
                "missing share name",
                "bmux unshare <name>",
                Some("bmux hosts"),
            )
        })?;
        return run_unshare(client, revoke_name).await;
    }

    let mut config = client.config.clone();
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
    let control_plane_url = control_plane_url(&config);
    let auth_state = load_auth_state_optional(&client.paths)?.ok_or_else(|| {
        actionable_error("not authenticated", "bmux setup", Some("bmux auth login"))
    })?;
    let slug = suggest_share_link_name(
        name,
        &resolved_target,
        &config,
        auth_state.account_name.as_deref(),
    );
    let reserved_names = config
        .connections
        .share_links
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let create_spec = ShareCreateSpec {
        target: &resolved_target,
        role,
        ttl: ttl.map(ToString::to_string),
        one_time,
    };
    let (created_name, created) = create_share_link_with_fallback_name(
        &control_plane_url,
        &auth_state.access_token,
        &slug,
        &create_spec,
        &reserved_names,
    )
    .await?;

    config
        .connections
        .share_links
        .insert(created_name.clone(), resolved_target.clone());
    config.save_to_path(&client.paths.config_file())?;
    let link_name = created.name.clone().unwrap_or(created_name);
    let invite_url = created.url;
    for line in status_ready_lines(Some(&format!("bmux join bmux://{link_name}"))) {
        println!("{line}");
    }
    println!("Share link: bmux://{link_name}");
    if let Some(url) = invite_url.as_deref() {
        println!("Invite URL: {url}");
    }
    println!("Target: {resolved_target}");
    println!("Role: {role}");
    if let Some(value) = ttl {
        println!("TTL: {value}");
    }
    if one_time {
        println!("One-time: true");
    }
    if copy {
        let share_link = format!("bmux://{link_name}");
        match (client.copy)(&share_link) {
            Ok(()) => println!("Copied to clipboard: {share_link}"),
            Err(error) => eprintln!("warning: clipboard copy failed: {error}"),
        }
    }
    if qr {
        let qr_payload = invite_url.unwrap_or_else(|| format!("bmux://{link_name}"));
        println!("QR:");
        for line in render_text_qr(&qr_payload)? {
            println!("{line}");
        }
    }
    Ok(0)
}

async fn run_unshare(client: &HostedClient, name: &str) -> Result<u8> {
    let mut config = client.config.clone();
    let control_plane_url = control_plane_url(&config);
    let auth_state = load_auth_state_optional(&client.paths)?.ok_or_else(|| {
        actionable_error("not authenticated", "bmux setup", Some("bmux auth login"))
    })?;
    delete_share_link(&control_plane_url, &auth_state.access_token, name).await?;

    if config.connections.share_links.remove(name).is_some() {
        config.save_to_path(&client.paths.config_file())?;
        for line in status_ready_lines(Some("bmux hosts")) {
            println!("{line}");
        }
        println!("Revoked share link: bmux://{name}");
        return Ok(0);
    }
    Err(actionable_error(
        &format!("share link not found: bmux://{name}"),
        "bmux hosts",
        Some("bmux share <target> --name <name>"),
    ))
}

fn choose_default_target_interactively(config: &BmuxConfig) -> Result<String> {
    let options = build_join_target_options(config);
    if io::stdin().is_terminal() {
        println!("Choose a host or paste an invite (bmux://, https://, iroh://):");
        for (index, option) in options.iter().enumerate() {
            println!("  {}. {option}", index + 1);
        }
        print!("Selection or invite (Enter for {}): ", options[0]);
        io::stdout()
            .flush()
            .context("failed flushing join prompt")?;
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("failed reading join target")?;
        if let Some(selected) = resolve_join_prompt_selection(input.trim(), &options)? {
            return Ok(selected);
        }
        return Ok(options[0].clone());
    }
    Ok(options[0].clone())
}

fn build_join_target_options(config: &BmuxConfig) -> Vec<String> {
    let mut options = Vec::new();
    for target in &config.connections.recent_targets {
        if !options.iter().any(|value| value == target) {
            options.push(target.clone());
        }
    }
    if let Some(default_target) = config.connections.default_target.as_deref()
        && !options.iter().any(|value| value == default_target)
    {
        options.push(default_target.to_string());
    }
    for name in config.connections.share_links.keys() {
        let share = format!("bmux://{name}");
        if !options.iter().any(|value| value == &share) {
            options.push(share);
        }
    }
    for name in config.connections.targets.keys() {
        if !options.iter().any(|value| value == name) {
            options.push(name.clone());
        }
    }
    if !options.iter().any(|value| value == "local") {
        options.push("local".to_string());
    }
    options
}

fn resolve_join_prompt_selection(input: &str, options: &[String]) -> Result<Option<String>> {
    let value = input.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if let Ok(index) = value.parse::<usize>() {
        if index == 0 || index > options.len() {
            anyhow::bail!("selection out of range: {index}")
        }
        return Ok(Some(options[index - 1].clone()));
    }
    Ok(Some(normalize_join_target_input(value)?))
}

fn print_join_preview(
    config: &BmuxConfig,
    target: &str,
    session: Option<&str>,
    metadata: Option<&InviteMetadata>,
) {
    let resolved_target = metadata
        .and_then(|meta| meta.resolved_target.as_deref())
        .or_else(|| {
            target
                .strip_prefix("bmux://")
                .and_then(|name| config.connections.share_links.get(name).map(String::as_str))
        });
    if let Some(resolved) = resolved_target {
        println!("Resolved target: {resolved}");
    }
    if let Some(meta) = metadata {
        if let Some(owner) = meta.owner.as_deref() {
            println!("Owner: {owner}");
        }
        if let Some(role) = meta.role.as_deref() {
            println!("Role: {role}");
        }
        if let Some(expires_at) = meta.expires_at.as_deref() {
            println!("Expires: {expires_at}");
        }
        if meta.one_time == Some(true) {
            println!("One-time: true");
        }
    }
    if let Some(session_id) = session {
        println!("Session: {session_id}");
    }
}

fn confirm_risky_invite(target: &str, metadata: Option<&InviteMetadata>) -> Result<()> {
    if !invite_requires_confirmation(metadata) {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(actionable_error(
            &format!("invite {target} grants control access with unknown owner"),
            &format!("bmux join {target}"),
            Some("bmux hosts"),
        ));
    }
    print!("Invite grants control access but owner is unknown. Continue? [y/N]: ");
    io::stdout()
        .flush()
        .context("failed flushing invite confirmation prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed reading invite confirmation")?;
    let value = input.trim().to_ascii_lowercase();
    if value == "y" || value == "yes" {
        return Ok(());
    }
    Err(actionable_error(
        "join cancelled",
        &format!("bmux join {target}"),
        Some("bmux hosts"),
    ))
}

fn invite_requires_confirmation(metadata: Option<&InviteMetadata>) -> bool {
    let Some(meta) = metadata else {
        return false;
    };
    let is_control = meta
        .role
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("control"));
    let owner_is_unknown = meta
        .owner
        .as_deref()
        .map(str::trim)
        .is_none_or(str::is_empty);
    is_control && owner_is_unknown
}

async fn fetch_invite_metadata(
    client: &HostedClient,
    config: &BmuxConfig,
    target: &str,
) -> Option<InviteMetadata> {
    let name = target.strip_prefix("bmux://")?;
    let mut metadata = InviteMetadata {
        resolved_target: config.connections.share_links.get(name).cloned(),
        ..InviteMetadata::default()
    };

    let control_plane = control_plane_url(config);
    let http = reqwest_client();
    let mut request = http.get(format!("{control_plane}/v1/share-links/{name}"));
    if let Ok(Some(state)) = load_auth_state_optional(&client.paths) {
        request = request.bearer_auth(state.access_token);
    }
    let Ok(response) = request.send().await else {
        return Some(metadata);
    };
    if !response.status().is_success() {
        return Some(metadata);
    }
    let Ok(payload) = response.json::<serde_json::Value>().await else {
        return Some(metadata);
    };

    if metadata.resolved_target.is_none() {
        metadata.resolved_target = json_string(&payload, &["target"]);
    }
    metadata.role = json_string(&payload, &["role"]);
    metadata.owner = json_string(
        &payload,
        &[
            "owner",
            "owner_name",
            "account_name",
            "creator",
            "created_by",
        ],
    );
    metadata.expires_at = json_string(&payload, &["expires_at", "expiresAt", "expiration"]);
    metadata.one_time = json_bool(&payload, &["one_time", "oneTime", "single_use"]);
    Some(metadata)
}

fn json_string(payload: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload
            .get(*key)
            .and_then(|value| value.as_str())
            .map(ToString::to_string)
    })
}

fn json_bool(payload: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| payload.get(*key).and_then(serde_json::Value::as_bool))
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

fn host_runtime_state_path(paths: &ConfigPaths) -> PathBuf {
    paths.runtime_dir.join("host-state.json")
}

fn save_host_runtime_state(paths: &ConfigPaths, state: &HostRuntimeState) -> Result<()> {
    paths.ensure_runtime_dir().with_context(|| {
        format!(
            "failed creating runtime dir {}",
            paths.runtime_dir.display()
        )
    })?;
    let path = host_runtime_state_path(paths);
    let encoded =
        serde_json::to_string_pretty(state).context("failed serializing host runtime state")?;
    std::fs::write(&path, encoded).with_context(|| format!("failed writing {}", path.display()))
}

fn load_host_runtime_state(paths: &ConfigPaths) -> Result<Option<HostRuntimeState>> {
    let path = host_runtime_state_path(paths);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let state = serde_json::from_str::<HostRuntimeState>(&content)
                .with_context(|| format!("failed parsing host runtime state {}", path.display()))?;
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed reading {}", path.display())),
    }
}

fn clear_host_runtime_state(paths: &ConfigPaths) -> Result<()> {
    let path = host_runtime_state_path(paths);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed removing {}", path.display())),
    }
}

const fn build_create_share_request(
    name: String,
    target: String,
    role: String,
    ttl: Option<String>,
    one_time: bool,
) -> CreateShareRequest {
    CreateShareRequest {
        name,
        target,
        role,
        ttl,
        one_time,
    }
}

fn render_text_qr(payload: &str) -> Result<Vec<String>> {
    let code = QrCode::new(payload.as_bytes()).context("failed generating QR code")?;
    let rendered = code.render::<unicode::Dense1x2>().quiet_zone(true).build();
    Ok(rendered.lines().map(ToString::to_string).collect())
}

fn run_host_status(client: &HostedClient) -> Result<u8> {
    let paths = client.paths.clone();
    let Some(state) = load_host_runtime_state(&paths)? else {
        for line in status_not_ready_lines(
            Some("host runtime is not running"),
            "bmux setup",
            Some("bmux host --daemon"),
        ) {
            println!("{line}");
        }
        println!("runtime: {}", client.runtime_name);
        println!("local ipc endpoint: {}", local_ipc_endpoint_label(&paths));
        return Ok(1);
    };
    if !is_process_alive(state.pid) {
        clear_host_runtime_state(&paths)?;
        for line in status_not_ready_lines(
            Some("stale runtime state was cleared"),
            "bmux setup",
            Some("bmux host --restart"),
        ) {
            println!("{line}");
        }
        println!("runtime: {}", client.runtime_name);
        println!("local ipc endpoint: {}", local_ipc_endpoint_label(&paths));
        return Ok(1);
    }
    for line in format_host_status_lines(client, &state) {
        println!("{line}");
    }
    Ok(0)
}

fn format_host_status_lines(client: &HostedClient, state: &HostRuntimeState) -> Vec<String> {
    let mut lines = status_ready_lines(None);
    lines.push("host runtime: running".to_string());
    lines.push(format!("runtime: {}", client.runtime_name));
    lines.push(format!(
        "local ipc endpoint: {}",
        local_ipc_endpoint_label(&client.paths)
    ));
    if let Some(name) = state.name.as_deref() {
        lines.push(format!("name: {name}"));
    }
    lines.push(format!("pid: {}", state.pid));
    lines.push(format!("target: {}", state.target));
    if let Some(link) = state.share_link.as_deref() {
        lines.push(format!("share link: {link}"));
    }
    lines.push(format!("started_at_unix: {}", state.started_at_unix));
    lines
}

fn run_host_stop(client: &HostedClient) -> Result<u8> {
    let paths = client.paths.clone();
    let Some(state) = load_host_runtime_state(&paths)? else {
        for line in status_ready_lines(None) {
            println!("{line}");
        }
        println!("Reason: host runtime is already stopped");
        return Ok(0);
    };

    if !is_process_alive(state.pid) {
        clear_host_runtime_state(&paths)?;
        for line in status_ready_lines(None) {
            println!("{line}");
        }
        println!("Reason: host runtime is already stopped");
        println!("Reason: stale runtime state was cleared");
        return Ok(0);
    }

    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .args(["-TERM", &state.pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed running kill")?;
        if !status.success() {
            anyhow::bail!("failed stopping host runtime pid {}", state.pid);
        }
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &state.pid.to_string(), "/T", "/F"])
            .status()
            .context("failed running taskkill")?;
        if !status.success() {
            anyhow::bail!("failed stopping host runtime pid {}", state.pid);
        }
    }

    clear_host_runtime_state(&paths)?;
    for line in status_ready_lines(None) {
        println!("{line}");
    }
    println!("host runtime stopped (pid {})", state.pid);
    Ok(0)
}

fn spawn_host_daemon(
    client: &HostedClient,
    listen: &str,
    name: Option<&str>,
    mode: HostedMode,
) -> Result<u8> {
    let paths = client.paths.clone();
    if let Some(state) = load_host_runtime_state(&paths)?
        && is_process_alive(state.pid)
    {
        println!("host runtime already running (pid {})", state.pid);
        return Ok(0);
    }

    let current_exe = std::env::current_exe().context("failed resolving current executable")?;
    let mut command = std::process::Command::new(current_exe);
    command.arg("--runtime").arg(&client.runtime_name);
    command.args(["host", "--listen", listen]);
    command.args(["--mode", hosted_mode_to_cli_value(mode)]);
    if let Some(value) = name {
        command.args(["--name", value]);
    }
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    let child = command.spawn().context("failed starting host daemon")?;
    println!("host runtime started in background (pid {})", child.id());
    println!(
        "check status: bmux --runtime {} host --status",
        client.runtime_name
    );
    Ok(0)
}

fn local_ipc_endpoint_label(paths: &ConfigPaths) -> String {
    #[cfg(unix)]
    {
        paths.server_socket().display().to_string()
    }
    #[cfg(windows)]
    {
        paths.server_named_pipe()
    }
}

const fn hosted_mode_to_cli_value(mode: HostedMode) -> &'static str {
    match mode {
        HostedMode::P2p => "p2p",
        HostedMode::ControlPlane => "control-plane",
    }
}

fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}")])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            })
    }
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
    paths.ensure_runtime_dir().with_context(|| {
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

async fn ensure_authenticated(client: &HostedClient, config: &BmuxConfig) -> Result<AuthState> {
    let paths = client.paths.clone();
    let control_plane = control_plane_url(config);
    if let Some(mut state) = load_auth_state_optional(&paths)? {
        match verify_access_token(&control_plane, &state.access_token).await {
            Ok(whoami) => {
                if state.account_name.is_none() {
                    state.account_name = whoami.account_name;
                }
                if state.account_id.is_none() {
                    state.account_id = whoami.account_id;
                }
                return Ok(state);
            }
            Err(error) => {
                println!("Auth state is stale ({error}); re-authenticating...");
                let _ = std::fs::remove_file(auth_state_path(&paths));
            }
        }
    }
    println!("not authenticated; starting login...");
    run_auth_login(client, false).await?;
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

fn suggest_share_link_name(
    requested_name: Option<&str>,
    target: &str,
    config: &BmuxConfig,
    account_name: Option<&str>,
) -> String {
    if let Some(value) = requested_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return value.to_string();
    }
    if let Some((name, _)) = config
        .connections
        .share_links
        .iter()
        .find(|(_, existing_target)| existing_target == &target)
    {
        return name.clone();
    }
    if let Some(account) = account_name {
        let slug = sanitize_slug(account);
        if !slug.is_empty() {
            return format!("{slug}-share");
        }
    }
    let target_slug = sanitize_slug(target.trim_start_matches("bmux://"));
    if !target_slug.is_empty() {
        return format!("{target_slug}-share");
    }
    "share".to_string()
}

fn sanitize_slug(value: &str) -> String {
    value
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
        .to_string()
}

async fn ensure_host_share_link(
    client: &HostedClient,
    config: &mut BmuxConfig,
    control_plane_url: &str,
    token: &str,
    name: &str,
    target: &str,
) -> Result<String> {
    if let Some(mapped) = config.connections.share_links.get(name)
        && mapped == target
        && share_link_matches_target(control_plane_url, token, name, target).await?
    {
        return Ok(name.to_string());
    }
    let reserved_names = config
        .connections
        .share_links
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let create_spec = ShareCreateSpec {
        target,
        role: "control",
        ttl: None,
        one_time: false,
    };
    let (resolved_name, _) = create_share_link_with_fallback_name(
        control_plane_url,
        token,
        name,
        &create_spec,
        &reserved_names,
    )
    .await?;
    config
        .connections
        .share_links
        .insert(resolved_name.clone(), target.to_string());
    config.save_to_path(&client.paths.config_file())?;
    Ok(resolved_name)
}

async fn ensure_setup_share_link(
    client: &HostedClient,
    config: &mut BmuxConfig,
    auth_state: &AuthState,
    host_state: &HostRuntimeState,
) -> Result<Option<String>> {
    let control_plane_url = control_plane_url(config);
    let token = auth_state.access_token.as_str();
    let existing_name = host_state
        .share_link
        .as_deref()
        .and_then(|value| value.strip_prefix("bmux://"));
    let share_name = existing_name.map_or_else(
        || suggest_share_name(host_state.name.as_deref(), auth_state),
        ToString::to_string,
    );
    let resolved = ensure_host_share_link(
        client,
        config,
        &control_plane_url,
        token,
        &share_name,
        &host_state.target,
    )
    .await?;
    Ok(Some(format!("bmux://{resolved}")))
}

async fn share_link_matches_target(
    control_plane_url: &str,
    token: &str,
    name: &str,
    expected_target: &str,
) -> Result<bool> {
    let client = reqwest_client();
    let response = client
        .get(format!("{control_plane_url}/v1/share-links/{name}"))
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("failed contacting {control_plane_url}"))?;
    if response.status().as_u16() == 404 {
        return Ok(false);
    }
    if !response.status().is_success() {
        anyhow::bail!(
            "share lookup failed (status {})",
            response.status().as_u16()
        );
    }
    let payload = response
        .json::<serde_json::Value>()
        .await
        .context("failed parsing share lookup response")?;
    Ok(payload
        .get("target")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value == expected_target))
}

struct ShareCreateSpec<'a> {
    target: &'a str,
    role: &'a str,
    ttl: Option<String>,
    one_time: bool,
}

async fn create_share_link_with_fallback_name(
    control_plane_url: &str,
    token: &str,
    base_name: &str,
    create_spec: &ShareCreateSpec<'_>,
    reserved_names: &std::collections::BTreeSet<String>,
) -> Result<(String, ShareLinkResponse)> {
    for attempt in 0..10 {
        let candidate = if attempt == 0 {
            base_name.to_string()
        } else {
            format!("{base_name}-{}", attempt + 1)
        };
        if reserved_names.contains(&candidate) && candidate != base_name {
            continue;
        }
        let request = build_create_share_request(
            candidate.clone(),
            create_spec.target.to_string(),
            create_spec.role.to_string(),
            create_spec.ttl.clone(),
            create_spec.one_time,
        );
        match create_share_link(control_plane_url, token, &request).await {
            Ok(created) => {
                let resolved_name = created.name.clone().unwrap_or_else(|| candidate.clone());
                return Ok((resolved_name, created));
            }
            Err(error) if error.to_string().contains("status 409") => {}
            Err(error) => return Err(error),
        }
    }
    anyhow::bail!("share creation failed: could not allocate unique share name")
}

fn normalize_join_target_input(link: &str) -> Result<String> {
    let value = link.trim();
    if value.is_empty() {
        return Err(actionable_error(
            "join target cannot be empty",
            "bmux join <invite>",
            Some("bmux hosts"),
        ));
    }
    if let Some(extracted) = extract_target_from_text(value) {
        return Ok(extracted);
    }
    if value.contains("://") {
        return Ok(value.to_string());
    }
    if let Some(suggestion) = suggest_invite_typo_fix(value) {
        return Err(actionable_error(
            "invite looks malformed",
            "bmux join <invite>",
            Some(&format!("Did you mean: {suggestion}")),
        ));
    }
    if value.contains(char::is_whitespace) {
        return Err(actionable_error(
            "could not find a valid invite link in input",
            "bmux join <invite>",
            Some("bmux hosts"),
        ));
    }
    Ok(format!("bmux://{value}"))
}

fn suggest_invite_typo_fix(value: &str) -> Option<String> {
    if let Some(rest) = value.strip_prefix("bmux:/")
        && !value.starts_with("bmux://")
    {
        return Some(format!("bmux://{}", rest.trim_start_matches('/')));
    }
    if let Some(rest) = value.strip_prefix("bmux//") {
        return Some(format!("bmux://{}", rest.trim_start_matches('/')));
    }
    if value.starts_with("http:/") && !value.starts_with("http://") {
        return Some(value.replacen("http:/", "http://", 1));
    }
    if value.starts_with("https:/") && !value.starts_with("https://") {
        return Some(value.replacen("https:/", "https://", 1));
    }
    None
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
        std::process::Command::new("open")
            .arg(url)
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()
            .is_ok_and(|status| status.success())
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
    let client = reqwest_client();
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
    let client = reqwest_client();
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

async fn verify_access_token(control_plane_url: &str, token: &str) -> Result<WhoAmIResponse> {
    let client = reqwest_client();
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
    let client = reqwest_client();
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
    let client = reqwest_client();
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

async fn connect(
    client: &HostedClient,
    target: Option<&str>,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
) -> Result<u8> {
    anyhow::ensure!(
        session.is_none() || follow.is_none(),
        "--follow cannot be used with an explicit session argument"
    );
    let target = match target {
        Some(target) => target.to_string(),
        None => choose_default_target_interactively(&client.config)?,
    };
    let target = expand_attach_target(client, &target).await?;
    let (status, label, session) = (client.attach)(
        target,
        session.map(str::to_owned),
        follow.map(str::to_owned),
        global,
        reconnect_forever,
    )?;
    if status == 0
        && let Err(error) = remember_recent_selection(&client.paths, &label, session.as_deref())
    {
        tracing::warn!("failed to save recent selection: {error}");
    }
    Ok(status)
}
fn remember_recent_selection(
    paths: &ConfigPaths,
    target: &str,
    session: Option<&str>,
) -> Result<()> {
    let mut config = BmuxConfig::load_from_path(&paths.config_file())?;
    push_recent(&mut config.connections.recent_targets, target.to_string());
    if let Some(session) = session {
        let list = config
            .connections
            .recent_sessions
            .entry(target.to_string())
            .or_default();
        push_recent(list, session.to_string());
    }
    config.save_to_path(&paths.config_file())?;
    Ok(())
}

fn push_recent(list: &mut Vec<String>, value: String) {
    list.retain(|entry| entry != &value);
    list.insert(0, value);
    if list.len() > RECENT_CACHE_MAX {
        list.truncate(RECENT_CACHE_MAX);
    }
}

async fn expand_attach_target(client: &HostedClient, target: &str) -> Result<String> {
    let Some(name) = target.strip_prefix("bmux://") else {
        return Ok(target.to_string());
    };
    if let Some(mapped) = client.config.connections.share_links.get(name) {
        return Ok(mapped.clone());
    }
    let Some(auth_state) = load_auth_state_optional(&client.paths)? else {
        return Ok(target.to_string());
    };
    let control_plane = control_plane_url(&client.config);
    let client = reqwest_client();
    let response = client
        .get(format!("{control_plane}/v1/share-links/{name}"))
        .bearer_auth(auth_state.access_token)
        .send()
        .await
        .with_context(|| format!("failed contacting {control_plane}"))?;
    if !response.status().is_success() {
        return Ok(target.to_string());
    }
    let payload = response
        .json::<serde_json::Value>()
        .await
        .context("failed parsing share lookup response")?;
    Ok(payload
        .get("target")
        .and_then(|value| value.as_str())
        .map_or_else(|| target.to_string(), ToString::to_string))
}

#[cfg(test)]
mod tests {
    fn test_client(dir: &std::path::Path) -> HostedClient {
        HostedClient {
            paths: ConfigPaths::new(
                dir.join("config"),
                dir.join("runtime"),
                dir.join("data"),
                dir.join("state"),
            ),
            config: BmuxConfig::default(),
            runtime_name: "default".into(),
            admin: None,
            copy: Arc::new(|_| panic!("unexpected clipboard write")),
            attach: Arc::new(|_, _, _, _, _| panic!("unexpected attach")),
        }
    }
    #[test]
    fn host_runtime_state_round_trips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());

        let paths = client.paths;
        let expected = HostRuntimeState {
            pid: 4242,
            target: "iroh://endpoint".to_string(),
            share_link: Some("bmux://demo".to_string()),
            name: Some("demo-host".to_string()),
            started_at_unix: 1_700_000_000,
        };

        save_host_runtime_state(&paths, &expected).expect("save host state");
        let loaded = load_host_runtime_state(&paths)
            .expect("load host state")
            .expect("host state present");
        assert_eq!(loaded, expected);

        clear_host_runtime_state(&paths).expect("clear host state");
        let cleared = load_host_runtime_state(&paths).expect("load after clear");
        assert!(cleared.is_none());
    }

    #[test]
    fn host_status_returns_not_running_without_state() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());

        let code = run_host_status(&client).expect("run host status");
        assert_eq!(code, 1);
    }

    #[test]
    fn host_stop_is_noop_without_state() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());

        let code = run_host_stop(&client).expect("run host stop");
        assert_eq!(code, 0);
    }

    #[test]
    fn is_process_alive_returns_true_for_current_process() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn host_status_clears_stale_runtime_state() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        let paths = client.paths.clone();
        save_host_runtime_state(
            &paths,
            &HostRuntimeState {
                pid: 999_999,
                target: "iroh://stale".to_string(),
                share_link: Some("bmux://stale".to_string()),
                name: Some("stale".to_string()),
                started_at_unix: 1,
            },
        )
        .expect("save stale state");

        let code = run_host_status(&client).expect("run status");
        assert_eq!(code, 1);
        assert!(
            load_host_runtime_state(&paths)
                .expect("load state")
                .is_none()
        );
    }

    #[test]
    fn host_stop_clears_stale_runtime_state() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        let paths = client.paths.clone();
        save_host_runtime_state(
            &paths,
            &HostRuntimeState {
                pid: 999_999,
                target: "iroh://stale".to_string(),
                share_link: Some("bmux://stale".to_string()),
                name: Some("stale".to_string()),
                started_at_unix: 1,
            },
        )
        .expect("save stale state");

        let code = run_host_stop(&client).expect("run stop");
        assert_eq!(code, 0);
        assert!(
            load_host_runtime_state(&paths)
                .expect("load state")
                .is_none()
        );
    }

    #[test]
    fn host_status_output_matches_state_file_fields() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());

        let paths = client.paths.clone();
        let state = HostRuntimeState {
            pid: 9001,
            target: "iroh://endpoint-123".to_string(),
            share_link: Some("bmux://demo-host".to_string()),
            name: Some("demo-host".to_string()),
            started_at_unix: 1_700_000_123,
        };
        save_host_runtime_state(&paths, &state).expect("save host runtime state");

        let loaded = load_host_runtime_state(&paths)
            .expect("load host runtime state")
            .expect("state present");
        let lines = format_host_status_lines(&client, &loaded);
        assert_eq!(lines[0], "Status: ready");
        assert_eq!(lines[1], "host runtime: running");
        assert_eq!(lines[2], "runtime: default");
        assert!(lines[3].starts_with("local ipc endpoint: "));
        assert!(lines.contains(&"name: demo-host".to_string()));
        assert!(lines.contains(&"pid: 9001".to_string()));
        assert!(lines.contains(&"target: iroh://endpoint-123".to_string()));
        assert!(lines.contains(&"share link: bmux://demo-host".to_string()));
        assert!(lines.contains(&"started_at_unix: 1700000123".to_string()));
    }

    use super::*;
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
    fn normalize_join_target_input_empty_is_actionable() {
        let error = normalize_join_target_input("   ").expect_err("empty target should fail");
        assert!(error.to_string().contains("Fix: bmux join <invite>"));
        assert!(error.to_string().contains("Advanced: bmux hosts"));
    }

    #[test]
    fn normalize_join_target_input_whitespace_noise_is_actionable() {
        let error = normalize_join_target_input("invite demo code")
            .expect_err("invalid invite should fail");
        assert!(
            error
                .to_string()
                .contains("could not find a valid invite link in input")
        );
        assert!(error.to_string().contains("Fix: bmux join <invite>"));
    }

    #[test]
    fn normalize_join_target_input_malformed_scheme_offers_did_you_mean_hint() {
        let error = normalize_join_target_input("bmux:/demo").expect_err("should fail");
        assert!(error.to_string().contains("invite looks malformed"));
        assert!(error.to_string().contains("Fix: bmux join <invite>"));
        assert!(error.to_string().contains("Did you mean: bmux://demo"));
    }

    #[test]
    fn build_join_target_options_prioritizes_recent_then_links_then_named_then_local() {
        let mut config = BmuxConfig::default();
        config.connections.recent_targets = vec!["ssh-prod".to_string(), "bmux://demo".to_string()];
        config.connections.default_target = Some("default-target".to_string());
        config
            .connections
            .share_links
            .insert("demo".to_string(), "iroh://demo-endpoint".to_string());
        config
            .connections
            .share_links
            .insert("team".to_string(), "iroh://team-endpoint".to_string());
        config
            .connections
            .targets
            .insert("staging".to_string(), ConnectionTargetConfig::default());

        let options = build_join_target_options(&config);
        assert_eq!(options[0], "ssh-prod");
        assert_eq!(options[1], "bmux://demo");
        assert!(options.contains(&"default-target".to_string()));
        assert!(options.contains(&"bmux://team".to_string()));
        assert!(options.contains(&"staging".to_string()));
        assert_eq!(options.last().map(String::as_str), Some("local"));
    }

    #[test]
    fn resolve_join_prompt_selection_accepts_numeric_and_invite_text() {
        let options = vec!["bmux://demo".to_string(), "local".to_string()];
        let selected = resolve_join_prompt_selection("1", &options)
            .expect("parse selection")
            .expect("has value");
        assert_eq!(selected, "bmux://demo");

        let pasted = resolve_join_prompt_selection("Invite: bmux://team", &options)
            .expect("parse invite")
            .expect("has value");
        assert_eq!(pasted, "bmux://team");
    }

    #[test]
    fn resolve_join_prompt_selection_rejects_out_of_range_index() {
        let options = vec!["bmux://demo".to_string()];
        let error = resolve_join_prompt_selection("9", &options).expect_err("out of range");
        assert!(error.to_string().contains("selection out of range"));
    }

    #[test]
    fn invite_requires_confirmation_for_unknown_control_owner() {
        let metadata = InviteMetadata {
            role: Some("control".to_string()),
            owner: None,
            ..InviteMetadata::default()
        };
        assert!(invite_requires_confirmation(Some(&metadata)));
    }

    #[test]
    fn invite_requires_confirmation_is_false_when_owner_known() {
        let metadata = InviteMetadata {
            role: Some("control".to_string()),
            owner: Some("alice@example.com".to_string()),
            ..InviteMetadata::default()
        };
        assert!(!invite_requires_confirmation(Some(&metadata)));
    }

    #[test]
    fn invite_requires_confirmation_when_owner_is_blank() {
        let metadata = InviteMetadata {
            role: Some("control".to_string()),
            owner: Some("   ".to_string()),
            ..InviteMetadata::default()
        };
        assert!(invite_requires_confirmation(Some(&metadata)));
    }

    #[test]
    fn invite_requires_confirmation_is_false_for_non_control_roles() {
        let metadata = InviteMetadata {
            role: Some("view".to_string()),
            owner: None,
            ..InviteMetadata::default()
        };
        assert!(!invite_requires_confirmation(Some(&metadata)));
    }

    #[test]
    fn build_create_share_request_keeps_ttl_and_one_time() {
        let request = build_create_share_request(
            "demo".to_string(),
            "iroh://host".to_string(),
            "view".to_string(),
            Some("24h".to_string()),
            true,
        );
        assert_eq!(request.ttl.as_deref(), Some("24h"));
        assert!(request.one_time);
    }

    #[test]
    fn build_create_share_request_allows_unbounded_reusable_link() {
        let request = build_create_share_request(
            "demo".to_string(),
            "iroh://host".to_string(),
            "control".to_string(),
            None,
            false,
        );
        assert!(request.ttl.is_none());
        assert!(!request.one_time);
    }

    #[test]
    fn suggest_share_link_name_reuses_existing_target_mapping() {
        let mut config = BmuxConfig::default();
        config
            .connections
            .share_links
            .insert("alice-share".to_string(), "iroh://demo".to_string());
        let name = suggest_share_link_name(None, "iroh://demo", &config, Some("alice"));
        assert_eq!(name, "alice-share");
    }

    #[test]
    fn suggest_share_link_name_prefers_account_slug() {
        let config = BmuxConfig::default();
        let name = suggest_share_link_name(None, "iroh://demo", &config, Some("alice@example.com"));
        assert_eq!(name, "alice-example-com-share");
    }

    #[test]
    fn render_text_qr_produces_multiline_output() {
        let lines = render_text_qr("bmux://demo").expect("render qr");
        assert!(lines.len() > 4);
        assert!(lines.iter().any(|line| !line.trim().is_empty()));
    }

    #[test]
    fn setup_summary_lines_snapshot_is_stable() {
        let lines = format_setup_summary_lines(
            Some("alice@example.com"),
            "alice-mbp",
            Some("bmux://alice"),
            "bmux://alice",
            true,
        );
        assert_eq!(
            lines,
            vec![
                "Signed in as alice@example.com".to_string(),
                "Host online: alice-mbp".to_string(),
                "Share link: bmux://alice".to_string(),
                "Join from another machine: bmux join bmux://alice".to_string(),
            ]
        );
    }

    #[test]
    fn setup_summary_lines_falls_back_to_unknown_account() {
        let lines =
            format_setup_summary_lines(None, "demo-host", Some("bmux://demo"), "bmux://demo", true);
        assert_eq!(lines[0], "Signed in as unknown");
    }

    #[test]
    fn setup_summary_lines_reports_unavailable_share_link() {
        let lines =
            format_setup_summary_lines(Some("alice"), "demo-host", None, "iroh://endpoint", true);
        assert_eq!(lines[2], "Share link: unavailable");
        assert_eq!(
            lines[3],
            "Join from another machine: bmux join iroh://endpoint"
        );
    }

    #[test]
    fn setup_summary_lines_omit_auth_line_for_p2p_mode() {
        let lines =
            format_setup_summary_lines(Some("alice"), "demo-host", None, "iroh://endpoint", false);
        assert_eq!(lines[0], "Host online: demo-host");
    }

    #[test]
    fn resolve_hosted_mode_prefers_cli_override() {
        let config = BmuxConfig::default();
        let mode = resolve_hosted_mode(&config, Some(HostedMode::ControlPlane));
        assert_eq!(mode, HostedMode::ControlPlane);
    }

    #[test]
    fn resolve_hosted_mode_falls_back_to_config() {
        let mut config = BmuxConfig::default();
        config.connections.hosted_mode = HostedMode::ControlPlane;
        let mode = resolve_hosted_mode(&config, None);
        assert_eq!(mode, HostedMode::ControlPlane);
    }
}
