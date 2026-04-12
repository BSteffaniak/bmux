use crate::ssh_access::{
    authenticate_client_connection, authenticate_host_connection, ensure_iroh_ssh_access_ready,
    iroh_ssh_access_enabled, iroh_target_url, parse_iroh_target as parse_iroh_target_parts,
};
use anyhow::{Context, Result};
use bmux_cli_schema::{Cli, Command, ServerCommand, SessionCommand};
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_ipc::{InvokeServiceKind, RecordingRollingStartOptions, SessionSummary};
use bmux_plugin_sdk::{PluginCliCommandRequest, PluginCliCommandResponse};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};
use std::{collections::BTreeMap, collections::BTreeSet};
use uuid::Uuid;

use super::{
    AttachExitReason, ConnectionContext, ConnectionPolicyScope, KernelClientFactory,
    active_runtime_name, append_runtime_arg, connect, connect_with_context,
    expand_bmux_target_if_needed, map_cli_client_error, recording, run_server_start,
    run_session_attach, run_session_attach_with_client,
};
use bmux_cli_schema::HostedModeArg;
use bmux_config::{ConnectionTargetConfig, ConnectionTransport, HostedMode, RemoteServerStartMode};
use bmux_ipc::IpcEndpoint;
use bmux_ipc::transport::{ErasedIpcStream, LocalIpcStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::presets};
use qrcode::QrCode;
use qrcode::render::unicode;
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum ClusterGatewayMode {
    #[default]
    Auto,
    Direct,
    Pinned,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ClusterGatewayDefinition {
    targets: Vec<String>,
    hosts: Vec<ClusterGatewayHostRef>,
    gateway_mode: ClusterGatewayMode,
    gateway_candidates: Vec<String>,
    gateway_target: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ClusterGatewayHostRef {
    Target(String),
    Object {
        target: Option<String>,
        host: Option<String>,
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ClusterGatewaySettings {
    clusters: BTreeMap<String, ClusterGatewayDefinition>,
}

#[derive(Debug, Clone)]
struct ClusterGatewayRuntimeState {
    last_good: Option<GatewayLastGood>,
    cooldown_until: BTreeMap<String, Instant>,
}

#[derive(Debug, Clone)]
struct GatewayLastGood {
    target: String,
    observed_at: Instant,
}

#[derive(Debug, Clone)]
struct GatewayAttemptFailure {
    candidate: String,
    reason_code: &'static str,
    detail: String,
}

enum GatewayBatchOutcome {
    Success(u8),
    Exhausted { attempted: bool },
}

struct GatewayBatchRequest<'a> {
    config: &'a BmuxConfig,
    cluster_name: &'a str,
    candidates: &'a [String],
    plugin_id: &'a str,
    command_name: &'a str,
    arguments: &'a [String],
    respect_cooldown: bool,
    no_failover: bool,
}

#[derive(Debug, Default)]
struct GatewayCommandOverrides {
    gateway_target: Option<String>,
    gateway_mode: Option<ClusterGatewayMode>,
    no_failover: bool,
    passthrough_arguments: Vec<String>,
}

#[derive(Debug, Clone)]
struct GatewayProbeResult {
    ok: bool,
    reason_code: &'static str,
    detail: String,
    latency_ms: u128,
}

const CLUSTER_GATEWAY_LAST_GOOD_TTL: Duration = Duration::from_secs(90);
const CLUSTER_GATEWAY_FAILURE_COOLDOWN: Duration = Duration::from_secs(20);

static CLUSTER_GATEWAY_RUNTIME_STATE: OnceLock<
    Mutex<BTreeMap<String, ClusterGatewayRuntimeState>>,
> = OnceLock::new();

impl ClusterGatewayDefinition {
    fn declared_targets(&self) -> Vec<String> {
        let mut merged = Vec::new();
        for target in &self.targets {
            if !target.trim().is_empty() {
                merged.push(target.trim().to_string());
            }
        }
        for host in &self.hosts {
            if let Some(target) = cluster_gateway_target_from_host_ref(host) {
                merged.push(target);
            }
        }
        dedupe_preserve_order(merged)
    }
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
    require_ssh_auth: bool,
    connect_timeout_ms: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
struct IrohConnectPerfSummary {
    bind_ms: u64,
    online_ms: u64,
    connect_ms: u64,
    ssh_auth_ms: Option<u64>,
    open_bi_ms: u64,
    ipc_handshake_ms: u64,
    total_ms: u64,
    relay_enabled: bool,
    ssh_auth_enabled: bool,
    compression_enabled: bool,
}

#[allow(clippy::cast_possible_truncation)]
fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

const fn attach_exit_reason_label(reason: AttachExitReason) -> &'static str {
    match reason {
        AttachExitReason::Detached => "detached",
        AttachExitReason::StreamClosed => "stream_closed",
        AttachExitReason::Quit => "quit",
    }
}

async fn emit_iroh_connect_perf_event(
    perf_emitter: &mut recording::PerfEventEmitter,
    client: &mut BmuxClient,
    target: &IrohTarget,
    summary: &IrohConnectPerfSummary,
    reconnect_attempt: u64,
) -> Result<()> {
    if !perf_emitter.level_at_least(recording::PerfCaptureLevel::Basic) {
        return Ok(());
    }

    let mut payload = serde_json::json!({
        "target": target.label,
        "reconnect_attempt": reconnect_attempt,
        "connect_ms": summary.connect_ms,
        "total_ms": summary.total_ms,
    });
    if perf_emitter.level_at_least(recording::PerfCaptureLevel::Detailed)
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "bind_ms".to_string(),
            serde_json::Value::from(summary.bind_ms),
        );
        object.insert(
            "online_ms".to_string(),
            serde_json::Value::from(summary.online_ms),
        );
        object.insert(
            "open_bi_ms".to_string(),
            serde_json::Value::from(summary.open_bi_ms),
        );
        object.insert(
            "ipc_handshake_ms".to_string(),
            serde_json::Value::from(summary.ipc_handshake_ms),
        );
        if let Some(ssh_auth_ms) = summary.ssh_auth_ms {
            object.insert(
                "ssh_auth_ms".to_string(),
                serde_json::Value::from(ssh_auth_ms),
            );
        }
        object.insert(
            "relay_enabled".to_string(),
            serde_json::Value::from(summary.relay_enabled),
        );
        object.insert(
            "ssh_auth_enabled".to_string(),
            serde_json::Value::from(summary.ssh_auth_enabled),
        );
        object.insert(
            "compression_enabled".to_string(),
            serde_json::Value::from(summary.compression_enabled),
        );
    }
    if perf_emitter.level_at_least(recording::PerfCaptureLevel::Trace)
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "connect_timeout_ms".to_string(),
            serde_json::Value::from(target.connect_timeout_ms),
        );
        object.insert(
            "endpoint_id".to_string(),
            serde_json::Value::String(target.endpoint_id.clone()),
        );
    }

    perf_emitter
        .emit_with_client(client, None, None, "iroh.connect.summary", payload)
        .await
}

async fn refresh_perf_emitter_settings_from_server(
    perf_emitter: &mut recording::PerfEventEmitter,
    client: &mut BmuxClient,
) {
    if let Ok(settings) = client.performance_status().await {
        perf_emitter.update_settings(recording::PerfCaptureSettings::from_runtime_settings(
            &settings,
        ));
    }
}

async fn emit_iroh_attach_attempt_perf_event(
    perf_emitter: &mut recording::PerfEventEmitter,
    client: &mut BmuxClient,
    target: &IrohTarget,
    attach_attempt: u64,
    reconnect_attempt: u64,
    attach_runtime_ms: u64,
    exit_reason: AttachExitReason,
) -> Result<()> {
    if !perf_emitter.level_at_least(recording::PerfCaptureLevel::Basic) {
        return Ok(());
    }

    let mut payload = serde_json::json!({
        "target": target.label,
        "attach_attempt": attach_attempt,
        "reconnect_attempt": reconnect_attempt,
        "attach_runtime_ms": attach_runtime_ms,
        "exit_reason": attach_exit_reason_label(exit_reason),
        "stream_closed": matches!(exit_reason, AttachExitReason::StreamClosed),
    });
    if perf_emitter.level_at_least(recording::PerfCaptureLevel::Trace)
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "target_requires_ssh_auth".to_string(),
            serde_json::Value::from(target.require_ssh_auth),
        );
        object.insert(
            "target_uses_relay".to_string(),
            serde_json::Value::from(target.relay_url.is_some()),
        );
    }

    perf_emitter
        .emit_with_client(client, None, None, "iroh.attach.attempt", payload)
        .await
}

async fn emit_iroh_reconnect_outage_perf_event(
    perf_emitter: &mut recording::PerfEventEmitter,
    client: &mut BmuxClient,
    target: &IrohTarget,
    reconnect_attempt: u64,
    reconnect_backoff_ms: u64,
    outage_ms: u64,
    connect_summary: &IrohConnectPerfSummary,
) -> Result<()> {
    if !perf_emitter.level_at_least(recording::PerfCaptureLevel::Basic) {
        return Ok(());
    }

    let mut payload = serde_json::json!({
        "target": target.label,
        "reconnect_attempt": reconnect_attempt,
        "reconnect_backoff_ms": reconnect_backoff_ms,
        "outage_ms": outage_ms,
        "reconnect_connect_total_ms": connect_summary.total_ms,
        "reconnect_connect_ms": connect_summary.connect_ms,
    });
    if perf_emitter.level_at_least(recording::PerfCaptureLevel::Detailed)
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "reconnect_open_bi_ms".to_string(),
            serde_json::Value::from(connect_summary.open_bi_ms),
        );
        object.insert(
            "reconnect_ipc_handshake_ms".to_string(),
            serde_json::Value::from(connect_summary.ipc_handshake_ms),
        );
        if let Some(auth_ms) = connect_summary.ssh_auth_ms {
            object.insert(
                "reconnect_ssh_auth_ms".to_string(),
                serde_json::Value::from(auth_ms),
            );
        }
    }

    perf_emitter
        .emit_with_client(client, None, None, "iroh.reconnect.outage", payload)
        .await
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
    if matches!(
        command,
        Command::Connect { .. } | Command::Remote { .. } | Command::Access { .. }
    ) {
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

const fn command_requires_remote_server(command: Option<&Command>) -> bool {
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

#[allow(clippy::too_many_lines)]
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
            if status == 0
                && let Err(error) = remember_recent_selection("local", target_session.as_deref())
            {
                tracing::warn!("failed to save recent selection: {error}");
            }
            Ok(status)
        }
        ResolvedTarget::Ssh(ssh_target) => {
            let ssh_control_path = ssh_control_path_for_session();
            let mut client = connect_remote_bridge(
                &ssh_target,
                "bmux-cli-connect-remote",
                Some(&ssh_control_path),
            )
            .await?;
            let target_session = if follow.is_some() {
                None
            } else if let Some(session) = session {
                Some(session.to_string())
            } else {
                resolve_remote_attach_session(&mut client, &ssh_target.label).await?
            };
            let status = run_remote_attach_with_reconnect(
                client,
                ssh_control_path,
                &ssh_target,
                target_session.as_deref(),
                follow,
                global,
                reconnect_forever,
            )
            .await?;
            if status == 0
                && let Err(error) =
                    remember_recent_selection(&ssh_target.label, target_session.as_deref())
            {
                tracing::warn!("failed to save recent selection: {error}");
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
            if status == 0
                && let Err(error) =
                    remember_recent_selection(&tls_target.label, target_session.as_deref())
            {
                tracing::warn!("failed to save recent selection: {error}");
            }
            Ok(status)
        }
        ResolvedTarget::Iroh(iroh_target) => {
            let mut perf_emitter = recording::PerfEventEmitter::new(
                recording::PerfCaptureSettings::from_config(&config),
            );
            let mut connect_perf = IrohConnectPerfSummary::default();
            let (mut client, iroh_connection) = connect_iroh_bridge(
                &iroh_target,
                "bmux-cli-connect-remote-iroh",
                Some(&mut connect_perf),
            )
            .await?;
            refresh_perf_emitter_settings_from_server(&mut perf_emitter, &mut client).await;
            emit_iroh_connect_perf_event(
                &mut perf_emitter,
                &mut client,
                &iroh_target,
                &connect_perf,
                0,
            )
            .await?;
            let target_session = if follow.is_some() {
                None
            } else if let Some(session) = session {
                Some(session.to_string())
            } else {
                resolve_remote_attach_session(&mut client, &iroh_target.label).await?
            };
            let status = run_iroh_attach_with_reconnect(
                client,
                iroh_connection,
                &iroh_target,
                target_session.as_deref(),
                follow,
                global,
                reconnect_forever,
                perf_emitter,
            )
            .await?;
            if status == 0
                && let Err(error) =
                    remember_recent_selection(&iroh_target.label, target_session.as_deref())
            {
                tracing::warn!("failed to save recent selection: {error}");
            }
            Ok(status)
        }
    }
}

fn map_connect_target_resolution_error(target: &str, error: anyhow::Error) -> anyhow::Error {
    if target.starts_with("bmux://") && error.to_string().contains("share link not found:") {
        return actionable_error(&error.to_string(), "bmux setup", Some("bmux hosts"));
    }
    error
}

pub(super) async fn run_setup(check: bool, mode: Option<HostedModeArg>) -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    let hosted_mode = resolve_hosted_mode(&config, mode);
    if check {
        return run_setup_check(hosted_mode);
    }

    println!("bmux setup");
    let mut auth_state = None;
    if hosted_mode == HostedMode::ControlPlane {
        println!("Step 1/2: auth");
        auth_state = Some(ensure_authenticated(&config).await?);
        println!("Step 2/2: host");
    } else {
        println!("Step 1/1: host");
    }
    let _ = spawn_host_daemon("127.0.0.1:7443", None, hosted_mode)?;
    let mut host_state = wait_for_running_host_state(std::time::Duration::from_secs(5)).await?;

    if hosted_mode == HostedMode::ControlPlane
        && let Some(auth_state) = auth_state.as_ref()
    {
        let repaired_share = ensure_setup_share_link(&mut config, auth_state, &host_state).await?;
        if host_state.share_link.as_deref() != repaired_share.as_deref() {
            host_state.share_link = repaired_share;
            save_host_runtime_state(&ConfigPaths::default(), &host_state)?;
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
    println!("Setup complete.");
    Ok(0)
}

fn run_setup_check(mode: HostedMode) -> Result<u8> {
    let paths = ConfigPaths::default();
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
        println!("Status: ready");
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
    let mut reasons = Vec::new();
    if auth_check == SetupAuthCheck::RequiredMissing {
        reasons.push("not signed in".to_string());
    }
    match host_check {
        SetupHostCheck::Offline => reasons.push("host is offline".to_string()),
        SetupHostCheck::Stale(pid) => reasons.push(format!("host state is stale (pid {pid})")),
        SetupHostCheck::Running => {}
    }
    if share_check == SetupShareCheck::RequiredMissing && host_check == SetupHostCheck::Running {
        reasons.push("share link unavailable".to_string());
    }

    let reason_text = if reasons.is_empty() {
        "not ready".to_string()
    } else {
        reasons.join("; ")
    };

    let mut lines = vec![
        "Status: not ready".to_string(),
        format!("Reason: {reason_text}"),
        "Fix: bmux setup".to_string(),
    ];
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
    let mut lines = vec![format!("Reason: {reason}"), format!("Fix: {fix}")];
    if let Some(value) = advanced {
        lines.push(format!("Advanced: {value}"));
    }
    lines
}

fn actionable_error(reason: &str, fix: &str, advanced: Option<&str>) -> anyhow::Error {
    anyhow::anyhow!(format_actionable_error_lines(reason, fix, advanced).join("\n"))
}

async fn wait_for_running_host_state(timeout: std::time::Duration) -> Result<HostRuntimeState> {
    let paths = ConfigPaths::default();
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

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools
)]
pub(super) async fn run_host(
    listen: &str,
    name: Option<&str>,
    copy: bool,
    daemon: bool,
    status: bool,
    stop: bool,
    restart: bool,
    mode: Option<HostedModeArg>,
    setup_summary: bool,
) -> Result<u8> {
    if status && stop {
        anyhow::bail!("--status and --stop cannot be used together")
    }
    if status {
        return run_host_status();
    }
    if stop {
        return run_host_stop();
    }
    let mut config = BmuxConfig::load()?;
    ensure_iroh_ssh_access_ready(&config)?;
    let require_ssh_auth = iroh_ssh_access_enabled(&config);
    let ssh_allowlist = config.connections.iroh_ssh_access.allowlist.clone();
    let hosted_mode = resolve_hosted_mode(&config, mode);
    if restart {
        let _ = run_host_stop()?;
        return spawn_host_daemon(listen, name, hosted_mode);
    }
    if daemon {
        return spawn_host_daemon(listen, name, hosted_mode);
    }

    let control_plane_url = control_plane_url(&config);
    let auth_state = if hosted_mode == HostedMode::ControlPlane {
        Some(ensure_authenticated(&config).await?)
    } else {
        None
    };
    let bridge_paths = ConfigPaths::default();
    ensure_local_ipc_backend_ready(&bridge_paths, hosted_mode).await?;

    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![BMUX_IROH_ALPN.to_vec()])
        .bind()
        .await
        .context("failed binding iroh endpoint")?;
    endpoint.online().await;
    let addr = endpoint.addr();
    let endpoint_id = endpoint.id();
    let relay = addr
        .relay_urls()
        .next()
        .map(|value| normalize_relay_url_for_display(&value.to_string()));
    let target = iroh_target_url(&endpoint_id.to_string(), relay.as_deref(), require_ssh_auth);

    let resolved_share = if hosted_mode == HostedMode::ControlPlane {
        let auth_state = auth_state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("internal error: missing auth state"))?;
        let share_name = suggest_share_name(name, auth_state);
        let resolved_share = ensure_host_share_link(
            &mut config,
            &control_plane_url,
            &auth_state.access_token,
            &share_name,
            &target,
        )
        .await?;
        register_host_presence(
            &control_plane_url,
            &auth_state.access_token,
            name.map(ToString::to_string)
                .or_else(|| Some(resolved_share.clone())),
            &target,
        )
        .await?;
        Some(resolved_share)
    } else {
        None
    };

    let join_link = resolved_share
        .as_ref()
        .map_or_else(|| target.clone(), |value| format!("bmux://{value}"));

    let host_name = name
        .map(ToString::to_string)
        .or_else(|| {
            auth_state
                .as_ref()
                .and_then(|state| state.account_name.clone())
        })
        .unwrap_or_else(|| "host".to_string());
    save_host_runtime_state(
        &ConfigPaths::default(),
        &HostRuntimeState {
            pid: std::process::id(),
            target: target.clone(),
            share_link: resolved_share
                .as_ref()
                .map(|value| format!("bmux://{value}")),
            name: Some(host_name.clone()),
            started_at_unix: current_unix_timestamp(),
        },
    )?;

    if copy {
        match crate::runtime::attach::runtime::copy_text_with_clipboard_plugin(&join_link) {
            Ok(()) => println!("Copied to clipboard: {join_link}"),
            Err(error) => eprintln!(
                "warning: clipboard copy failed: {}",
                crate::runtime::attach::runtime::format_clipboard_service_error(&error)
            ),
        }
    }

    let summary_share_link = resolved_share
        .as_ref()
        .map(|value| format!("bmux://{value}"));
    if setup_summary {
        let account = auth_state
            .as_ref()
            .and_then(|state| state.account_name.as_deref());
        for line in format_setup_summary_lines(
            account,
            &host_name,
            summary_share_link.as_deref(),
            &join_link,
            hosted_mode == HostedMode::ControlPlane,
        ) {
            println!("{line}");
        }
    } else {
        println!("bmux iroh gateway online");
        println!("Host online: {host_name}");
        if require_ssh_auth {
            println!("SSH key auth: enabled");
        }
        if listen != "127.0.0.1:7443" {
            println!("note: --listen is ignored for iroh host mode ({listen})");
        }
        println!("connect URL: {target}");
        if let Some(share) = resolved_share.as_deref() {
            println!("Share link: bmux://{share}");
            println!("Join from another machine: bmux join bmux://{share}");
        } else {
            println!("Join from another machine: bmux join {target}");
        }
    }

    while let Some(incoming) = endpoint.accept().await {
        let mut accepting = match incoming.accept() {
            Ok(accepting) => accepting,
            Err(error) => {
                tracing::warn!(?error, "iroh incoming accept failed");
                continue;
            }
        };
        let bridge_paths = bridge_paths.clone();
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
                loop {
                    let Ok((send, recv)) = conn.accept_bi().await else {
                        break; // connection closed
                    };
                    let stream_paths = bridge_paths.clone();
                    tokio::spawn(async move {
                        if let Err(error) =
                            proxy_iroh_stream_to_local_ipc(send, recv, &stream_paths).await
                        {
                            tracing::debug!(?error, "iroh stream proxy failed");
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
    let _ = clear_host_runtime_state(&ConfigPaths::default());
    Ok(0)
}

/// Proxy a single iroh QUIC bi-stream to/from a local IPC connection.
///
/// Used by the iroh gateway to bridge each bi-stream (primary attach session or
/// kernel bridge side-channel) to an independent local IPC session with the bmux
/// server.
async fn proxy_iroh_stream_to_local_ipc(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    paths: &ConfigPaths,
) -> Result<()> {
    let endpoint = local_ipc_endpoint_from_paths(paths);
    let ipc_stream = LocalIpcStream::connect(&endpoint)
        .await
        .context("failed connecting local IPC endpoint for iroh stream proxy")?;
    let (mut ipc_read, mut ipc_write) = tokio::io::split(ipc_stream);

    let config = BmuxConfig::load().unwrap_or_default();
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

fn normalize_relay_url_for_display(raw: &str) -> String {
    let Some(scheme_sep) = raw.find("://") else {
        return raw.to_string();
    };
    let authority_start = scheme_sep + 3;
    let tail = &raw[authority_start..];
    let suffix_start = tail
        .find(['/', '?', '#'])
        .map_or(raw.len(), |value| authority_start + value);
    let authority = &raw[authority_start..suffix_start];
    let suffix = &raw[suffix_start..];
    let normalized_authority = normalize_url_authority_host(authority);
    format!(
        "{}{}{}",
        &raw[..authority_start],
        normalized_authority,
        suffix
    )
}

fn normalize_url_authority_host(authority: &str) -> String {
    let (prefix, host_port) = authority
        .rsplit_once('@')
        .map_or(("", authority), |(left, right)| (left, right));
    let normalized_host_port = if host_port.starts_with('[') {
        host_port.to_string()
    } else if let Some((host, port)) = host_port.rsplit_once(':') {
        if !host.is_empty()
            && !port.is_empty()
            && port.chars().all(|value| value.is_ascii_digit())
            && host.contains('.')
        {
            format!("{}:{}", host.trim_end_matches('.'), port)
        } else {
            host_port.trim_end_matches('.').to_string()
        }
    } else {
        host_port.trim_end_matches('.').to_string()
    };
    if prefix.is_empty() {
        normalized_host_port
    } else {
        format!("{prefix}@{normalized_host_port}")
    }
}

const fn resolve_hosted_mode(config: &BmuxConfig, mode: Option<HostedModeArg>) -> HostedMode {
    match mode {
        Some(HostedModeArg::P2p) => HostedMode::P2p,
        Some(HostedModeArg::ControlPlane) => HostedMode::ControlPlane,
        None => config.connections.hosted_mode,
    }
}

pub(super) async fn run_join(link: Option<&str>, session: Option<&str>) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let target = if let Some(link) = link {
        let normalized = normalize_join_target_input(link)?;
        if normalized != link.trim() {
            println!("Resolved invite: {normalized}");
        }
        normalized
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
    let invite_metadata = fetch_invite_metadata(&config, &target).await;
    print_join_preview(&config, &target, resumed_session, invite_metadata.as_ref());
    confirm_risky_invite(&target, invite_metadata.as_ref())?;
    println!("Connecting...");
    run_connect(Some(&target), resumed_session, None, false, true).await
}

pub(super) fn run_hosts(verbose: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let auth_ready = load_auth_state_optional(&paths)?.is_some();
    let host_state = load_host_runtime_state(&paths)?;
    let host_running = host_state
        .as_ref()
        .is_some_and(|state| is_process_alive(state.pid));
    if auth_ready && host_running {
        println!("Status: ready");
    } else {
        let reason = hosts_status_reason(auth_ready, host_running, host_state.as_ref());
        println!("Status: not ready ({reason})");
        println!("Fix: bmux setup");
    }
    if verbose {
        println!("runtime:");
        println!("- name: {}", active_runtime_name());
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

fn hosts_status_reason(
    auth_ready: bool,
    host_running: bool,
    host_state: Option<&HostRuntimeState>,
) -> String {
    match (auth_ready, host_running, host_state) {
        (false, false, Some(state)) => {
            format!("not signed in; host state is stale (pid {})", state.pid)
        }
        (false, false, None) => "not signed in; host is offline".to_string(),
        (false, true, _) => "not signed in".to_string(),
        (true, false, Some(state)) => format!("host state is stale (pid {})", state.pid),
        (true, false, None) => "host is offline".to_string(),
        (true, true, _) => "not ready".to_string(),
    }
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
        return Err(actionable_error(
            "BMUX_AUTH_TOKEN is required in non-interactive mode",
            "bmux setup",
            Some("export BMUX_AUTH_TOKEN=<token>"),
        ));
    };

    let whoami = verify_access_token(&control_plane_url, &token).await?;
    let account_name_for_output = whoami.account_name.clone();
    let paths = ConfigPaths::default();
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

pub(super) fn run_auth_status() -> Result<u8> {
    let paths = ConfigPaths::default();
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_share(
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
        return run_unshare(revoke_name).await;
    }

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
    let control_plane_url = control_plane_url(&config);
    let auth_state = load_auth_state_optional(&ConfigPaths::default())?.ok_or_else(|| {
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
    config.save()?;
    let link_name = created.name.clone().unwrap_or(created_name);
    let invite_url = created.url;
    println!("Share link: bmux://{link_name}");
    if let Some(url) = invite_url.as_deref() {
        println!("Invite URL: {url}");
    }
    println!("Join from another machine: bmux join bmux://{link_name}");
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
        match crate::runtime::attach::runtime::copy_text_with_clipboard_plugin(&share_link) {
            Ok(()) => println!("Copied to clipboard: {share_link}"),
            Err(error) => eprintln!(
                "warning: clipboard copy failed: {}",
                crate::runtime::attach::runtime::format_clipboard_service_error(&error)
            ),
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

pub(super) async fn run_unshare(name: &str) -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    let control_plane_url = control_plane_url(&config);
    let auth_state = load_auth_state_optional(&ConfigPaths::default())?.ok_or_else(|| {
        actionable_error("not authenticated", "bmux setup", Some("bmux auth login"))
    })?;
    delete_share_link(&control_plane_url, &auth_state.access_token, name).await?;

    if config.connections.share_links.remove(name).is_some() {
        config.save()?;
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

async fn fetch_invite_metadata(config: &BmuxConfig, target: &str) -> Option<InviteMetadata> {
    let name = target.strip_prefix("bmux://")?;
    let mut metadata = InviteMetadata {
        resolved_target: config.connections.share_links.get(name).cloned(),
        ..InviteMetadata::default()
    };

    let control_plane = control_plane_url(config);
    let client = reqwest::Client::new();
    let mut request = client.get(format!("{control_plane}/v1/share-links/{name}"));
    if let Ok(Some(state)) = load_auth_state_optional(&ConfigPaths::default()) {
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
    std::fs::create_dir_all(&paths.runtime_dir).with_context(|| {
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

fn current_unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().cast_signed())
        .unwrap_or(0)
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

fn run_host_status() -> Result<u8> {
    let paths = ConfigPaths::default();
    let Some(state) = load_host_runtime_state(&paths)? else {
        println!("host runtime: not running");
        println!("runtime: {}", active_runtime_name());
        println!("local ipc endpoint: {}", local_ipc_endpoint_label(&paths));
        println!("Fix: bmux setup");
        println!("Advanced: bmux host --daemon");
        return Ok(1);
    };
    if !is_process_alive(state.pid) {
        clear_host_runtime_state(&paths)?;
        println!("host runtime: not running");
        println!("runtime: {}", active_runtime_name());
        println!("local ipc endpoint: {}", local_ipc_endpoint_label(&paths));
        println!("Reason: stale runtime state was cleared");
        println!("Fix: bmux setup");
        println!("Advanced: bmux host --restart");
        return Ok(1);
    }
    for line in format_host_status_lines(&state) {
        println!("{line}");
    }
    Ok(0)
}

fn format_host_status_lines(state: &HostRuntimeState) -> Vec<String> {
    let mut lines = vec!["host runtime: running".to_string()];
    lines.push(format!("runtime: {}", active_runtime_name()));
    lines.push(format!(
        "local ipc endpoint: {}",
        local_ipc_endpoint_label(&ConfigPaths::default())
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

fn run_host_stop() -> Result<u8> {
    let paths = ConfigPaths::default();
    let Some(state) = load_host_runtime_state(&paths)? else {
        println!("host runtime: not running");
        println!("Fix: bmux setup");
        println!("Advanced: bmux host --daemon");
        return Ok(0);
    };

    if !is_process_alive(state.pid) {
        clear_host_runtime_state(&paths)?;
        println!("host runtime: not running");
        println!("Reason: stale runtime state was cleared");
        println!("Fix: bmux setup");
        println!("Advanced: bmux host --restart");
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
    println!("stopped host runtime (pid {})", state.pid);
    Ok(0)
}

fn spawn_host_daemon(listen: &str, name: Option<&str>, mode: HostedMode) -> Result<u8> {
    let paths = ConfigPaths::default();
    if let Some(state) = load_host_runtime_state(&paths)?
        && is_process_alive(state.pid)
    {
        println!("host runtime already running (pid {})", state.pid);
        return Ok(0);
    }

    let current_exe = std::env::current_exe().context("failed resolving current executable")?;
    let mut command = std::process::Command::new(current_exe);
    append_runtime_arg(&mut command);
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
        active_runtime_name()
    );
    Ok(0)
}

async fn ensure_local_ipc_backend_ready(paths: &ConfigPaths, mode: HostedMode) -> Result<()> {
    let endpoint = local_ipc_endpoint_from_paths(paths);
    if local_ipc_connectable(&endpoint).await {
        return Ok(());
    }

    if mode == HostedMode::P2p {
        eprintln!(
            "local IPC backend unavailable for runtime '{}'; attempting to start bmux server...",
            active_runtime_name()
        );
        let _ = run_server_start(
            true,
            false,
            None,
            RecordingRollingStartOptions::default(),
            None,
        )
        .await?;
        if wait_for_local_ipc_ready(&endpoint, Duration::from_secs(3)).await {
            return Ok(());
        }
    }

    let endpoint_label = local_ipc_endpoint_label(paths);
    Err(actionable_error(
        &format!(
            "host bridge could not reach local IPC endpoint '{}' for runtime '{}'",
            endpoint_label,
            active_runtime_name(),
        ),
        "bmux setup",
        Some(&format!(
            "bmux --runtime {} server start --daemon",
            active_runtime_name()
        )),
    ))
}

async fn local_ipc_connectable(endpoint: &IpcEndpoint) -> bool {
    LocalIpcStream::connect(endpoint).await.is_ok()
}

async fn wait_for_local_ipc_ready(endpoint: &IpcEndpoint, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if local_ipc_connectable(endpoint).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
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
    config.save()?;
    Ok(resolved_name)
}

async fn ensure_setup_share_link(
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
    let client = reqwest::Client::new();
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
    if value.contains(char::is_whitespace) {
        return Err(actionable_error(
            "could not find a valid invite link in input",
            "bmux join <invite>",
            Some("bmux hosts"),
        ));
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
        std::process::Command::new("open")
            .arg(url)
            .status()
            .is_ok_and(|status| status.success())
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

#[allow(clippy::too_many_arguments)] // keep reconnect parameters explicit at call sites
async fn run_iroh_attach_with_reconnect(
    mut client: BmuxClient,
    mut iroh_connection: iroh::endpoint::Connection,
    target: &IrohTarget,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
    mut perf_emitter: recording::PerfEventEmitter,
) -> Result<u8> {
    let mut reconnect_attempt = 0usize;
    let mut attach_attempt = 0_u64;
    loop {
        let factory = build_iroh_kernel_client_factory(iroh_connection.clone(), target);
        let attach_started_at = Instant::now();
        let outcome =
            run_session_attach_with_client(client, session, follow, global, Some(factory)).await?;
        let attach_runtime_ms = duration_millis_u64(attach_started_at.elapsed());
        if outcome.exit_reason != AttachExitReason::StreamClosed {
            return Ok(outcome.status_code);
        }
        let outage_started_at = Instant::now();
        if !reconnect_forever && reconnect_attempt >= SSH_RECONNECT_MAX_ATTEMPTS {
            println!(
                "remote iroh connection closed; giving up after {SSH_RECONNECT_MAX_ATTEMPTS} reconnect attempts"
            );
            return Ok(1);
        }
        reconnect_attempt = reconnect_attempt.saturating_add(1);
        let reconnect_backoff_ms = reconnect_backoff_ms(reconnect_attempt);
        let backoff = Duration::from_millis(reconnect_backoff_ms);
        println!(
            "remote iroh connection closed; reconnecting to '{}' (attempt {reconnect_attempt}/{}) in {}ms...",
            target.label,
            SSH_RECONNECT_MAX_ATTEMPTS,
            backoff.as_millis()
        );
        tokio::time::sleep(backoff).await;
        let mut connect_perf = IrohConnectPerfSummary::default();
        let (mut new_client, new_connection) = connect_iroh_bridge(
            target,
            "bmux-cli-connect-remote-iroh-reconnect",
            Some(&mut connect_perf),
        )
        .await?;
        refresh_perf_emitter_settings_from_server(&mut perf_emitter, &mut new_client).await;
        emit_iroh_connect_perf_event(
            &mut perf_emitter,
            &mut new_client,
            target,
            &connect_perf,
            u64::try_from(reconnect_attempt).unwrap_or(u64::MAX),
        )
        .await?;
        emit_iroh_reconnect_outage_perf_event(
            &mut perf_emitter,
            &mut new_client,
            target,
            u64::try_from(reconnect_attempt).unwrap_or(u64::MAX),
            reconnect_backoff_ms,
            duration_millis_u64(outage_started_at.elapsed()),
            &connect_perf,
        )
        .await?;
        emit_iroh_attach_attempt_perf_event(
            &mut perf_emitter,
            &mut new_client,
            target,
            attach_attempt,
            u64::try_from(reconnect_attempt).unwrap_or(u64::MAX),
            attach_runtime_ms,
            outcome.exit_reason,
        )
        .await?;
        client = new_client;
        iroh_connection = new_connection;
        attach_attempt = attach_attempt.saturating_add(1);
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
        let factory = build_tls_kernel_client_factory(target);
        let outcome =
            run_session_attach_with_client(client, session, follow, global, Some(factory)).await?;
        if outcome.exit_reason != AttachExitReason::StreamClosed {
            return Ok(outcome.status_code);
        }
        if !reconnect_forever && attempt >= SSH_RECONNECT_MAX_ATTEMPTS {
            println!(
                "remote TLS connection closed; giving up after {SSH_RECONNECT_MAX_ATTEMPTS} reconnect attempts"
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
    mut ssh_control_path: String,
    target: &SshTarget,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
) -> Result<u8> {
    let mut attempt = 0usize;
    loop {
        let factory = build_ssh_kernel_client_factory(target, ssh_control_path.clone());
        let outcome =
            run_session_attach_with_client(client, session, follow, global, Some(factory)).await?;
        if outcome.exit_reason != AttachExitReason::StreamClosed {
            return Ok(outcome.status_code);
        }
        if !reconnect_forever && attempt >= SSH_RECONNECT_MAX_ATTEMPTS {
            println!(
                "remote connection closed; giving up after {SSH_RECONNECT_MAX_ATTEMPTS} reconnect attempts"
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
        // Generate a new ControlPath for the reconnected master.
        ssh_control_path = ssh_control_path_for_session();
        client = connect_remote_bridge(
            target,
            "bmux-cli-connect-remote-reconnect",
            Some(&ssh_control_path),
        )
        .await?;
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
            let (mut client, _) =
                connect_iroh_bridge(&iroh_target, "bmux-cli-remote-test-iroh", None).await?;
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
            let (mut client, _) =
                connect_iroh_bridge(&iroh_target, "bmux-cli-remote-doctor-iroh", None).await?;
            client.ping().await.map_err(map_cli_client_error)?;
            print_doctor_step_ok("iroh", "connectivity and ping succeeded");
            println!("doctor result: OK ({})", iroh_target.label);
            Ok(0)
        }
    }
}

pub(super) async fn maybe_run_cluster_plugin_command_via_gateway(
    plugin_id: &str,
    command_name: &str,
    arguments: &[String],
) -> Result<Option<u8>> {
    if plugin_id != "bmux.cluster" {
        return Ok(None);
    }

    let overrides = parse_gateway_overrides(arguments)?;
    let config = BmuxConfig::load()?;
    let settings = cluster_gateway_settings_from_config(&config)?;
    let Some(cluster_name) = resolve_cluster_name_for_gateway(
        command_name,
        &overrides.passthrough_arguments,
        &settings,
    )?
    else {
        return Ok(None);
    };
    let base_definition = settings
        .clusters
        .get(cluster_name.as_str())
        .ok_or_else(|| anyhow::anyhow!("unknown cluster '{cluster_name}'"))?;
    let definition = apply_gateway_overrides(base_definition.clone(), &overrides)?;

    if command_name == "cluster-gateway-status" {
        print_cluster_gateway_status(&cluster_name, &definition, &overrides)?;
        return Ok(Some(0));
    }
    if command_name == "cluster-gateway-explain" {
        return run_cluster_gateway_explain(&config, &cluster_name, &definition, &overrides).await;
    }

    if definition.gateway_mode == ClusterGatewayMode::Direct {
        return Ok(None);
    }

    let candidates = ordered_gateway_candidates_for_cluster(&cluster_name, &definition)?;
    tracing::info!(
        event = "cluster_gateway_selection_start",
        cluster = %cluster_name,
        mode = ?definition.gateway_mode,
        candidates = %candidates.join(","),
        command = %command_name,
        "selecting cluster gateway"
    );

    let mut failures = Vec::new();
    let attempted = match run_gateway_candidate_batch(
        GatewayBatchRequest {
            config: &config,
            cluster_name: &cluster_name,
            candidates: &candidates,
            plugin_id,
            command_name,
            arguments: &overrides.passthrough_arguments,
            respect_cooldown: definition.gateway_mode == ClusterGatewayMode::Auto,
            no_failover: overrides.no_failover,
        },
        &mut failures,
    )
    .await?
    {
        GatewayBatchOutcome::Success(code) => return Ok(Some(code)),
        GatewayBatchOutcome::Exhausted { attempted } => attempted,
    };

    if !attempted && definition.gateway_mode == ClusterGatewayMode::Auto {
        tracing::warn!(
            event = "cluster_gateway_cooldown_override",
            cluster = %cluster_name,
            "all gateway candidates were in cooldown; retrying immediately"
        );
        if let GatewayBatchOutcome::Success(code) = run_gateway_candidate_batch(
            GatewayBatchRequest {
                config: &config,
                cluster_name: &cluster_name,
                candidates: &candidates,
                plugin_id,
                command_name,
                arguments: &overrides.passthrough_arguments,
                respect_cooldown: false,
                no_failover: overrides.no_failover,
            },
            &mut failures,
        )
        .await?
        {
            return Ok(Some(code));
        }
    }

    anyhow::bail!(
        "all gateway candidates failed for cluster '{cluster_name}': {}",
        format_gateway_failures(&failures)
    )
}

async fn run_gateway_candidate_batch(
    request: GatewayBatchRequest<'_>,
    failures: &mut Vec<GatewayAttemptFailure>,
) -> Result<GatewayBatchOutcome> {
    let mut attempted = false;
    for candidate in request.candidates {
        if request.respect_cooldown && candidate_is_in_cooldown(request.cluster_name, candidate) {
            tracing::debug!(
                event = "cluster_gateway_candidate_skipped",
                cluster = %request.cluster_name,
                candidate = %candidate,
                reason = "cooldown",
                "skipping gateway candidate in cooldown"
            );
            failures.push(GatewayAttemptFailure {
                candidate: candidate.clone(),
                reason_code: "cooldown",
                detail: "candidate in cooldown window".to_string(),
            });
            continue;
        }

        attempted = true;
        match run_plugin_command_on_target(
            request.config,
            candidate,
            request.plugin_id,
            request.command_name,
            request.arguments,
        )
        .await
        {
            Ok(code) => {
                record_gateway_success(request.cluster_name, candidate);
                tracing::info!(
                    event = "cluster_gateway_selected",
                    cluster = %request.cluster_name,
                    candidate = %candidate,
                    command = %request.command_name,
                    "cluster gateway command succeeded"
                );
                return Ok(GatewayBatchOutcome::Success(code));
            }
            Err(error) => {
                let classified = classify_gateway_error(&error);
                record_gateway_failure(request.cluster_name, candidate);
                tracing::warn!(
                    event = "cluster_gateway_candidate_failed",
                    cluster = %request.cluster_name,
                    candidate = %candidate,
                    reason_code = classified.0,
                    detail = %classified.1,
                    "cluster gateway candidate failed"
                );
                failures.push(GatewayAttemptFailure {
                    candidate: candidate.clone(),
                    reason_code: classified.0,
                    detail: classified.1,
                });
            }
        }

        if request.no_failover {
            break;
        }
    }
    Ok(GatewayBatchOutcome::Exhausted { attempted })
}

fn cluster_gateway_settings_from_config(config: &BmuxConfig) -> Result<ClusterGatewaySettings> {
    let settings = config
        .plugins
        .settings
        .get("bmux.cluster")
        .cloned()
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    settings
        .try_into()
        .map_err(|error| anyhow::anyhow!("invalid bmux.cluster settings: {error}"))
}

fn resolve_cluster_name_for_gateway(
    command_name: &str,
    arguments: &[String],
    settings: &ClusterGatewaySettings,
) -> Result<Option<String>> {
    let cluster_flag =
        value_after_flag(arguments, "--cluster").or_else(|| value_after_flag(arguments, "-c"));
    let explicit = match command_name {
        "cluster-up" => first_positional_argument(arguments),
        "cluster-events" => cluster_flag,
        "cluster-gateway-status" | "cluster-gateway-explain" => {
            cluster_flag.or_else(|| first_positional_argument(arguments))
        }
        "cluster-pane-retry" => {
            if cluster_flag.is_none() && settings.clusters.len() > 1 {
                anyhow::bail!(
                    "cluster-pane-retry requires --cluster when multiple clusters are configured"
                );
            }
            cluster_flag
        }
        "cluster-pane-new" | "cluster-pane-move" => {
            if let Some(cluster) = cluster_flag {
                Some(cluster)
            } else if let Some(host) = extract_host_argument(arguments) {
                let matches = infer_cluster_names_from_target(settings, host.as_str());
                match matches.as_slice() {
                    [single] => Some(single.clone()),
                    [] => {
                        if settings.clusters.len() > 1 {
                            anyhow::bail!(
                                "{command_name} cannot infer cluster for host '{host}'; pass --cluster"
                            );
                        }
                        None
                    }
                    _ => {
                        anyhow::bail!(
                            "{command_name} host '{host}' matches multiple clusters ({}) - pass --cluster",
                            matches.join(",")
                        );
                    }
                }
            } else {
                if settings.clusters.len() > 1 {
                    anyhow::bail!(
                        "{command_name} requires --cluster in multi-cluster setups when host inference is unavailable"
                    );
                }
                None
            }
        }
        "cluster-status" | "cluster-hosts" | "cluster-doctor" => {
            let candidate = first_positional_argument(arguments);
            candidate.filter(|value| settings.clusters.contains_key(value))
        }
        _ => None,
    };

    Ok(explicit.or_else(|| {
        if settings.clusters.len() == 1 {
            settings.clusters.keys().next().cloned()
        } else {
            None
        }
    }))
}

fn first_positional_argument(arguments: &[String]) -> Option<String> {
    let mut index = 0usize;
    while index < arguments.len() {
        let value = arguments[index].trim();
        if value.is_empty() {
            index += 1;
            continue;
        }
        if value.starts_with('-') {
            index += 1;
            if index < arguments.len() && !arguments[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        return Some(value.to_string());
    }
    None
}

fn value_after_flag(arguments: &[String], flag: &str) -> Option<String> {
    let inline_prefix = format!("{flag}=");
    arguments
        .iter()
        .find_map(|argument| {
            if argument == flag {
                return None;
            }
            argument
                .strip_prefix(inline_prefix.as_str())
                .map(|value| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
        .or_else(|| {
            arguments
                .windows(2)
                .find_map(|pair| (pair[0] == flag).then(|| pair[1].trim().to_string()))
                .filter(|value| !value.is_empty())
        })
}

fn extract_host_argument(arguments: &[String]) -> Option<String> {
    value_after_flag(arguments, "--host")
        .or_else(|| value_after_flag(arguments, "-h"))
        .or_else(|| {
            let positional = arguments
                .iter()
                .filter(|value| !value.starts_with('-'))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            positional.last().cloned()
        })
}

fn infer_cluster_names_from_target(settings: &ClusterGatewaySettings, target: &str) -> Vec<String> {
    settings
        .clusters
        .iter()
        .filter(|(_, definition)| {
            definition
                .declared_targets()
                .iter()
                .any(|value| value == target)
        })
        .map(|(cluster, _)| cluster.clone())
        .collect()
}

fn ordered_gateway_candidates_for_cluster(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
) -> Result<Vec<String>> {
    let candidates = gateway_candidates_for_cluster(cluster_name, definition)?;
    if definition.gateway_mode != ClusterGatewayMode::Auto {
        return Ok(candidates);
    }

    let mut ordered = candidates;
    if let Some(preferred) = preferred_gateway_candidate(cluster_name)
        && let Some(index) = ordered.iter().position(|candidate| candidate == &preferred)
    {
        ordered.rotate_left(index);
    }
    Ok(ordered)
}

fn cluster_gateway_state_map() -> &'static Mutex<BTreeMap<String, ClusterGatewayRuntimeState>> {
    CLUSTER_GATEWAY_RUNTIME_STATE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn preferred_gateway_candidate(cluster_name: &str) -> Option<String> {
    let state = {
        let state_map = cluster_gateway_state_map().lock().ok()?;
        state_map.get(cluster_name)?.clone()
    };
    let last_good = state.last_good?;
    if last_good.observed_at.elapsed() > CLUSTER_GATEWAY_LAST_GOOD_TTL {
        if let Ok(mut state_map) = cluster_gateway_state_map().lock()
            && let Some(cluster_state) = state_map.get_mut(cluster_name)
        {
            cluster_state.last_good = None;
        }
        None
    } else {
        Some(last_good.target)
    }
}

fn candidate_is_in_cooldown(cluster_name: &str, candidate: &str) -> bool {
    let Ok(mut state_map) = cluster_gateway_state_map().lock() else {
        return false;
    };
    let Some(cluster_state) = state_map.get_mut(cluster_name) else {
        return false;
    };
    let Some(until) = cluster_state.cooldown_until.get(candidate).copied() else {
        return false;
    };
    if Instant::now() >= until {
        cluster_state.cooldown_until.remove(candidate);
        return false;
    }
    true
}

fn record_gateway_success(cluster_name: &str, candidate: &str) {
    if let Ok(mut state_map) = cluster_gateway_state_map().lock() {
        let cluster_state = state_map
            .entry(cluster_name.to_string())
            .or_insert_with(|| ClusterGatewayRuntimeState {
                last_good: None,
                cooldown_until: BTreeMap::new(),
            });
        cluster_state.last_good = Some(GatewayLastGood {
            target: candidate.to_string(),
            observed_at: Instant::now(),
        });
        cluster_state.cooldown_until.remove(candidate);
    }
}

fn record_gateway_failure(cluster_name: &str, candidate: &str) {
    if let Ok(mut state_map) = cluster_gateway_state_map().lock() {
        let cluster_state = state_map
            .entry(cluster_name.to_string())
            .or_insert_with(|| ClusterGatewayRuntimeState {
                last_good: None,
                cooldown_until: BTreeMap::new(),
            });
        cluster_state.cooldown_until.insert(
            candidate.to_string(),
            Instant::now() + CLUSTER_GATEWAY_FAILURE_COOLDOWN,
        );
    }
}

fn classify_gateway_error(error: &anyhow::Error) -> (&'static str, String) {
    let message = error.to_string();
    let lowered = message.to_ascii_lowercase();
    let code = if lowered.contains("denied") || lowered.contains("forbidden") {
        "service_denied"
    } else if lowered.contains("auth") || lowered.contains("permission") {
        "auth_failed"
    } else if lowered.contains("timeout") || lowered.contains("timed out") {
        "timeout"
    } else if lowered.contains("not found") || lowered.contains("unreachable") {
        "unreachable"
    } else {
        "gateway_error"
    };
    (code, message)
}

fn format_gateway_failures(failures: &[GatewayAttemptFailure]) -> String {
    failures
        .iter()
        .map(|failure| {
            format!(
                "{}[{}]={}",
                failure.candidate, failure.reason_code, failure.detail
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_gateway_overrides(arguments: &[String]) -> Result<GatewayCommandOverrides> {
    let mut overrides = GatewayCommandOverrides::default();
    let mut index = 0usize;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--gateway-no-failover" {
            overrides.no_failover = true;
            index += 1;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--gateway=") {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                anyhow::bail!("--gateway requires a non-empty target value");
            }
            overrides.gateway_target = Some(trimmed.to_string());
            index += 1;
            continue;
        }
        if argument == "--gateway" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| anyhow::anyhow!("--gateway requires a target value"))?
                .trim()
                .to_string();
            if value.is_empty() || value.starts_with('-') {
                anyhow::bail!("--gateway requires a non-empty target value");
            }
            overrides.gateway_target = Some(value);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--gateway-mode=") {
            overrides.gateway_mode = Some(parse_gateway_mode_value(value)?);
            index += 1;
            continue;
        }
        if argument == "--gateway-mode" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| anyhow::anyhow!("--gateway-mode requires a value"))?;
            overrides.gateway_mode = Some(parse_gateway_mode_value(value)?);
            index += 2;
            continue;
        }
        overrides.passthrough_arguments.push(argument.clone());
        index += 1;
    }
    Ok(overrides)
}

fn parse_gateway_mode_value(value: &str) -> Result<ClusterGatewayMode> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "auto" => Ok(ClusterGatewayMode::Auto),
        "direct" => Ok(ClusterGatewayMode::Direct),
        "pinned" => Ok(ClusterGatewayMode::Pinned),
        _ => anyhow::bail!("unsupported gateway mode '{value}' (expected auto|direct|pinned)"),
    }
}

fn apply_gateway_overrides(
    mut definition: ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
) -> Result<ClusterGatewayDefinition> {
    if let Some(mode) = overrides.gateway_mode {
        definition.gateway_mode = mode;
    }
    if let Some(target) = overrides.gateway_target.as_ref() {
        definition.gateway_mode = ClusterGatewayMode::Pinned;
        definition.gateway_target = Some(target.clone());
        definition.gateway_candidates = vec![target.clone()];
    }
    if definition.gateway_mode == ClusterGatewayMode::Pinned
        && definition
            .gateway_target
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    {
        anyhow::bail!("gateway_mode='pinned' requires gateway_target or --gateway");
    }
    Ok(definition)
}

fn print_cluster_gateway_status(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
) -> Result<()> {
    let candidates = ordered_gateway_candidates_for_cluster(cluster_name, definition)?;
    let preferred = preferred_gateway_candidate(cluster_name);
    println!(
        "cluster gateway status: cluster='{cluster_name}' mode={:?} no_failover={}",
        definition.gateway_mode, overrides.no_failover
    );
    if overrides.gateway_mode.is_some() || overrides.gateway_target.is_some() {
        println!(
            "overrides: mode={:?} gateway={:?}",
            overrides.gateway_mode, overrides.gateway_target
        );
    }
    println!("candidates:");
    for candidate in candidates {
        let preferred_marker = preferred.as_ref().is_some_and(|value| value == &candidate);
        let cooldown = gateway_cooldown_remaining_ms(cluster_name, &candidate);
        let suffix = if preferred_marker {
            "preferred"
        } else {
            "normal"
        };
        match cooldown {
            Some(ms) => println!("  - {candidate} [{suffix}] cooldown_ms={ms}"),
            None => println!("  - {candidate} [{suffix}]"),
        }
    }
    Ok(())
}

async fn run_cluster_gateway_explain(
    config: &BmuxConfig,
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
) -> Result<Option<u8>> {
    let candidates = ordered_gateway_candidates_for_cluster(cluster_name, definition)?;
    let preferred = preferred_gateway_candidate(cluster_name);
    println!(
        "cluster gateway explain: cluster='{cluster_name}' mode={:?} no_failover={}",
        definition.gateway_mode, overrides.no_failover
    );

    for candidate in &candidates {
        let cooldown = gateway_cooldown_remaining_ms(cluster_name, candidate);
        let probe = probe_gateway_candidate(config, candidate, cluster_name).await;
        let preferred_marker = preferred.as_ref().is_some_and(|value| value == candidate);
        println!(
            "  - candidate={candidate} preferred={} cooldown_ms={} ok={} reason={} latency_ms={} detail={}",
            preferred_marker,
            cooldown.unwrap_or(0),
            probe.ok,
            probe.reason_code,
            probe.latency_ms,
            probe.detail
        );
    }

    let status_arguments = vec![cluster_name.to_string()];
    let mut failures = Vec::new();
    let outcome = run_gateway_candidate_batch(
        GatewayBatchRequest {
            config,
            cluster_name,
            candidates: &candidates,
            plugin_id: "bmux.cluster",
            command_name: "cluster-status",
            arguments: &status_arguments,
            respect_cooldown: definition.gateway_mode == ClusterGatewayMode::Auto,
            no_failover: overrides.no_failover,
        },
        &mut failures,
    )
    .await?;

    match outcome {
        GatewayBatchOutcome::Success(_) => {
            println!("selection result: at least one candidate is executable");
            Ok(Some(0))
        }
        GatewayBatchOutcome::Exhausted { .. } => {
            println!("selection result: no executable gateway candidate");
            println!("failures: {}", format_gateway_failures(&failures));
            Ok(Some(1))
        }
    }
}

async fn probe_gateway_candidate(
    config: &BmuxConfig,
    candidate: &str,
    cluster_name: &str,
) -> GatewayProbeResult {
    let started = Instant::now();
    let result = run_plugin_command_on_target(
        config,
        candidate,
        "bmux.cluster",
        "cluster-status",
        &[cluster_name.to_string()],
    )
    .await;
    match result {
        Ok(_) => GatewayProbeResult {
            ok: true,
            reason_code: "ok",
            detail: "gateway command bridge reachable".to_string(),
            latency_ms: started.elapsed().as_millis(),
        },
        Err(error) => {
            let classified = classify_gateway_error(&error);
            GatewayProbeResult {
                ok: false,
                reason_code: classified.0,
                detail: classified.1,
                latency_ms: started.elapsed().as_millis(),
            }
        }
    }
}

fn gateway_cooldown_remaining_ms(cluster_name: &str, candidate: &str) -> Option<u128> {
    let until = cluster_gateway_state_map()
        .lock()
        .ok()?
        .get(cluster_name)?
        .cooldown_until
        .get(candidate)
        .copied()?;
    let now = Instant::now();
    if until <= now {
        return None;
    }
    Some((until - now).as_millis())
}

#[cfg(test)]
fn clear_gateway_runtime_state_for_tests() {
    if let Ok(mut state_map) = cluster_gateway_state_map().lock() {
        state_map.clear();
    }
}

fn gateway_candidates_for_cluster(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
) -> Result<Vec<String>> {
    let mut ordered = match definition.gateway_mode {
        ClusterGatewayMode::Direct => Vec::new(),
        ClusterGatewayMode::Pinned => {
            let target = definition
                .gateway_target
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "cluster '{cluster_name}' uses gateway_mode='pinned' but gateway_target is missing"
                    )
                })?;
            vec![target.to_string()]
        }
        ClusterGatewayMode::Auto => {
            if definition.gateway_candidates.is_empty() {
                definition.declared_targets()
            } else {
                definition.gateway_candidates.clone()
            }
        }
    };

    let mut seen = BTreeSet::new();
    ordered.retain(|candidate| {
        let trimmed = candidate.trim();
        !trimmed.is_empty() && seen.insert(trimmed.to_string())
    });
    if ordered.is_empty() {
        anyhow::bail!(
            "cluster '{cluster_name}' has no gateway candidates; configure targets or gateway_candidates"
        );
    }
    Ok(ordered)
}

fn cluster_gateway_target_from_host_ref(host: &ClusterGatewayHostRef) -> Option<String> {
    match host {
        ClusterGatewayHostRef::Target(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        ClusterGatewayHostRef::Object { target, host, name } => target
            .as_deref()
            .or(host.as_deref())
            .or(name.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    }
}

fn dedupe_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::with_capacity(values.len());
    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value);
        }
    }
    deduped
}

async fn run_plugin_command_on_target(
    config: &BmuxConfig,
    target: &str,
    plugin_id: &str,
    command_name: &str,
    arguments: &[String],
) -> Result<u8> {
    let resolved = resolve_target_reference(config, target).await?;
    let request = PluginCliCommandRequest::new(
        plugin_id.to_string(),
        command_name.to_string(),
        arguments.to_vec(),
    );
    let payload = bmux_plugin_sdk::encode_service_message(&request)
        .context("failed encoding plugin command request")?;
    let response = match resolved {
        ResolvedTarget::Local => {
            let mut client =
                connect(ConnectionPolicyScope::Normal, "bmux-cluster-gateway-local").await?;
            run_plugin_bridge_service(&mut client, payload).await?
        }
        ResolvedTarget::Ssh(ssh_target) => {
            let mut client =
                connect_remote_bridge(&ssh_target, "bmux-cluster-gateway-ssh", None).await?;
            run_plugin_bridge_service(&mut client, payload).await?
        }
        ResolvedTarget::Tls(tls_target) => {
            let mut client = connect_tls_bridge(&tls_target, "bmux-cluster-gateway-tls").await?;
            run_plugin_bridge_service(&mut client, payload).await?
        }
        ResolvedTarget::Iroh(iroh_target) => {
            let (mut client, _) =
                connect_iroh_bridge(&iroh_target, "bmux-cluster-gateway-iroh", None).await?;
            run_plugin_bridge_service(&mut client, payload).await?
        }
    };

    if let Some(error) = response.error {
        anyhow::bail!(
            "gateway plugin command failed on target '{target}': {error} (exit_code={})",
            response.exit_code
        );
    }
    u8::try_from(response.exit_code.clamp(0, i32::from(u8::MAX)))
        .context("gateway plugin command returned out-of-range exit code")
}

async fn run_plugin_bridge_service(
    client: &mut BmuxClient,
    payload: Vec<u8>,
) -> Result<PluginCliCommandResponse> {
    let response_payload = client
        .invoke_service_raw(
            "bmux.commands",
            InvokeServiceKind::Command,
            "cli-command/v1",
            "run_plugin",
            payload,
        )
        .await
        .map_err(map_cli_client_error)?;
    bmux_plugin_sdk::decode_service_message(&response_payload)
        .context("failed decoding plugin bridge response")
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
        let parsed = parse_iroh_target_parts(iroh_value)?;
        target.endpoint_id = Some(parsed.endpoint_id.clone());
        target.host = Some(parsed.endpoint_id);
        target.relay_url = parsed.relay_url;
        target.iroh_ssh_auth = parsed.require_ssh_auth;
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
            ResolvedTarget::Tls(_) | ResolvedTarget::Iroh(_) => {
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
            connect_remote_bridge(&ssh_target, "bmux-cli-complete-sessions-ssh", None).await?
        }
        ResolvedTarget::Tls(tls_target) => {
            connect_tls_bridge(&tls_target, "bmux-cli-complete-sessions-tls").await?
        }
        ResolvedTarget::Iroh(iroh_target) => {
            connect_iroh_bridge(&iroh_target, "bmux-cli-complete-sessions-iroh", None)
                .await?
                .0
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
            "session argument is required in non-interactive mode.\nList sessions: bmux --target {target_label} list-sessions"
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

async fn connect_remote_bridge(
    target: &SshTarget,
    client_name: &str,
    control_path: Option<&str>,
) -> Result<BmuxClient> {
    ensure_remote_server_ready(target).await?;
    ensure_remote_bridge_stdio_clean(target).await?;
    tracing::debug!(target = %target.label, "launching remote ssh bridge stream");
    let mut command = build_ssh_bridge_command(target, control_path);
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

    // Optionally wrap the TLS stream with transport-level compression (Layer 3).
    let config = BmuxConfig::load().unwrap_or_default();
    let use_transport_compression = config.behavior.compression.enabled
        && matches!(
            config.behavior.compression.remote,
            bmux_config::CompressionMode::Auto | bmux_config::CompressionMode::Zstd
        );
    let erased = if use_transport_compression {
        ErasedIpcStream::new(Box::new(
            bmux_ipc::compressed_stream::CompressedStream::new(tls_stream, 1),
        ))
    } else {
        ErasedIpcStream::new(Box::new(tls_stream))
    };

    BmuxClient::connect_with_bridge_stream(erased, timeout, client_name.to_string(), principal_id)
        .await
        .map_err(map_cli_client_error)
}

#[allow(clippy::too_many_lines)] // staged iroh bridge setup is clearer inline for troubleshooting
async fn connect_iroh_bridge(
    target: &IrohTarget,
    client_name: &str,
    perf_summary: Option<&mut IrohConnectPerfSummary>,
) -> Result<(BmuxClient, iroh::endpoint::Connection)> {
    let total_started_at = Instant::now();
    let mut perf = IrohConnectPerfSummary::default();

    let bind_started_at = Instant::now();
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![BMUX_IROH_ALPN.to_vec()])
        .bind()
        .await
        .context("failed binding iroh client endpoint")?;
    perf.bind_ms = duration_millis_u64(bind_started_at.elapsed());

    let online_started_at = Instant::now();
    endpoint.online().await;
    perf.online_ms = duration_millis_u64(online_started_at.elapsed());

    let connect_started_at = Instant::now();
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
    perf.connect_ms = duration_millis_u64(connect_started_at.elapsed());

    if target.require_ssh_auth {
        let auth_started_at = Instant::now();
        authenticate_client_connection(&connection)
            .await
            .context("iroh SSH auth handshake failed")?;
        perf.ssh_auth_ms = Some(duration_millis_u64(auth_started_at.elapsed()));
    }

    let open_bi_started_at = Instant::now();
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("failed opening iroh bi-directional stream")?;
    perf.open_bi_ms = duration_millis_u64(open_bi_started_at.elapsed());
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge_stream);
    // Clone the connection before moving the stream halves into copy tasks.
    // The clone keeps the underlying QUIC connection alive for the kernel bridge
    // to open additional bi-streams for plugin-to-server calls.
    let retained_connection = connection.clone();
    tokio::spawn(async move {
        if let Err(error) = tokio::io::copy(&mut recv, &mut bridge_write).await {
            tracing::debug!(?error, "iroh bridge recv->client copy failed");
        }
        let _ = bridge_write.shutdown().await;
    });
    tokio::spawn(async move {
        let _endpoint_keepalive = endpoint;
        let _connection_keepalive = connection;
        if let Err(error) = tokio::io::copy(&mut bridge_read, &mut send).await {
            tracing::debug!(?error, "iroh bridge client->send copy failed");
        }
        let _ = send.finish();
    });
    let timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
    let principal_id = load_or_create_local_principal_id(&ConfigPaths::default())?;

    // Optionally wrap the iroh duplex stream with transport-level compression.
    let config = BmuxConfig::load().unwrap_or_default();
    let use_transport_compression = config.behavior.compression.enabled
        && matches!(
            config.behavior.compression.remote,
            bmux_config::CompressionMode::Auto | bmux_config::CompressionMode::Zstd
        );
    let erased = if use_transport_compression {
        ErasedIpcStream::new(Box::new(
            bmux_ipc::compressed_stream::CompressedStream::new(client_stream, 1),
        ))
    } else {
        ErasedIpcStream::new(Box::new(client_stream))
    };

    let ipc_handshake_started_at = Instant::now();
    let client = BmuxClient::connect_with_bridge_stream(
        erased,
        timeout,
        client_name.to_string(),
        principal_id,
    )
    .await
    .map_err(map_cli_client_error)?;
    perf.ipc_handshake_ms = duration_millis_u64(ipc_handshake_started_at.elapsed());
    perf.total_ms = duration_millis_u64(total_started_at.elapsed());
    perf.relay_enabled = target.relay_url.is_some();
    perf.ssh_auth_enabled = target.require_ssh_auth;
    perf.compression_enabled = use_transport_compression;

    if let Some(summary) = perf_summary {
        *summary = perf;
    }

    Ok((client, retained_connection))
}

/// Build a [`KernelClientFactory`] that opens new QUIC bi-streams on `connection`
/// for each kernel bridge invocation.  Each call produces a fresh, independently-
/// handshaked [`BmuxClient`] whose lifetime is a single IPC request/response pair.
fn build_iroh_kernel_client_factory(
    connection: iroh::endpoint::Connection,
    target: &IrohTarget,
) -> KernelClientFactory {
    let timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
    let principal_id = load_or_create_local_principal_id(&ConfigPaths::default())
        .unwrap_or_else(|_| Uuid::new_v4());
    let config = BmuxConfig::load().unwrap_or_default();
    let use_compression = config.behavior.compression.enabled
        && matches!(
            config.behavior.compression.remote,
            bmux_config::CompressionMode::Auto | bmux_config::CompressionMode::Zstd
        );

    Arc::new(move || {
        let conn = connection.clone();
        let timeout = timeout;
        let principal_id = principal_id;
        Box::pin(async move {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .context("failed opening iroh kernel bridge bi-stream")?;
            let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
            let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge_stream);
            tokio::spawn(async move {
                if let Err(error) = tokio::io::copy(&mut recv, &mut bridge_write).await {
                    tracing::debug!(?error, "iroh kernel bridge recv->client copy failed");
                }
                let _ = bridge_write.shutdown().await;
            });
            tokio::spawn(async move {
                if let Err(error) = tokio::io::copy(&mut bridge_read, &mut send).await {
                    tracing::debug!(?error, "iroh kernel bridge client->send copy failed");
                }
                let _ = send.finish();
            });
            let erased = if use_compression {
                ErasedIpcStream::new(Box::new(
                    bmux_ipc::compressed_stream::CompressedStream::new(client_stream, 1),
                ))
            } else {
                ErasedIpcStream::new(Box::new(client_stream))
            };
            BmuxClient::connect_with_bridge_stream(
                erased,
                timeout,
                "bmux-cli-iroh-kernel-bridge".to_string(),
                principal_id,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e))
        }) as Pin<Box<dyn std::future::Future<Output = Result<BmuxClient>> + Send>>
    })
}

/// Generate a unique SSH `ControlMaster` socket path for this attach session.
fn ssh_control_path_for_session() -> String {
    format!("/tmp/bmux-ssh-{}", Uuid::new_v4().as_simple())
}

/// Build a [`KernelClientFactory`] that spawns new SSH bridge processes for
/// each kernel bridge invocation, reusing the master SSH connection via
/// `ControlPath` for near-instant channel setup.
///
/// The `control_path` must point to the `ControlMaster` socket created by the
/// initial `connect_remote_bridge` call.  Preflight checks are skipped since
/// the remote server was already validated during initial connection.
fn build_ssh_kernel_client_factory(
    target: &SshTarget,
    control_path: String,
) -> KernelClientFactory {
    let target = target.clone();
    let principal_id = load_or_create_local_principal_id(&ConfigPaths::default())
        .unwrap_or_else(|_| Uuid::new_v4());

    Arc::new(move || {
        let target = target.clone();
        let control_path = control_path.clone();
        let principal_id = principal_id;
        Box::pin(async move {
            let mut command = build_ssh_bridge_command(&target, Some(&control_path));
            let mut child = command
                .spawn()
                .context("failed spawning SSH kernel bridge process")?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("failed acquiring SSH kernel bridge stdin"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("failed acquiring SSH kernel bridge stdout"))?;
            let bridge_stream = SshBridgeStream {
                _child: child,
                stdin,
                stdout,
            };
            let timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
            BmuxClient::connect_with_bridge_stream(
                ErasedIpcStream::new(Box::new(bridge_stream)),
                timeout,
                "bmux-cli-ssh-kernel-bridge".to_string(),
                principal_id,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e))
        }) as Pin<Box<dyn std::future::Future<Output = Result<BmuxClient>> + Send>>
    })
}

/// Build a [`KernelClientFactory`] that opens a new TCP+TLS connection for
/// each kernel bridge invocation.
///
/// The TLS gateway already accepts multiple independent connections, so no
/// server-side changes are needed.
fn build_tls_kernel_client_factory(target: &TlsTarget) -> KernelClientFactory {
    let target = target.clone();

    Arc::new(move || {
        let target = target.clone();
        Box::pin(async move { connect_tls_bridge(&target, "bmux-cli-tls-kernel-bridge").await })
            as Pin<Box<dyn std::future::Future<Output = Result<BmuxClient>> + Send>>
    })
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

#[allow(clippy::unused_async)] // Async signature for caller consistency in async dispatch chain
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

#[allow(clippy::unused_async)] // Async signature for caller consistency in async dispatch chain
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
                print!("{stdout}");
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

fn build_ssh_bridge_command(target: &SshTarget, control_path: Option<&str>) -> TokioProcessCommand {
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
    // When a ControlPath is provided, enable SSH connection multiplexing so
    // that kernel bridge SSH processes can piggyback on the master connection
    // instead of performing a full handshake each time.
    if let Some(cp) = control_path {
        command.arg("-o");
        command.arg("ControlMaster=auto");
        command.arg("-o");
        command.arg(format!("ControlPath={cp}"));
    }
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
        Some(
            Command::Attach { .. }
                | Command::Session {
                    command: SessionCommand::Attach { .. }
                }
        )
    )
}

#[allow(clippy::cast_possible_truncation)] // Attempt count bounded to small values
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
                require_ssh_auth: target.iroh_ssh_auth,
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
    let parsed = parse_iroh_target_parts(target)?;
    Ok(ResolvedTarget::Iroh(IrohTarget {
        label: target.to_string(),
        endpoint_id: parsed.endpoint_id,
        relay_url: parsed.relay_url,
        require_ssh_auth: parsed.require_ssh_auth,
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
    #[allow(clippy::wildcard_imports)]
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

    #[test]
    #[serial]
    fn host_runtime_state_round_trips_and_clears() {
        let runtime_dir = TempDirGuard::new("host-state-roundtrip");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());

        let paths = ConfigPaths::default();
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
    #[serial]
    fn host_status_returns_not_running_without_state() {
        let runtime_dir = TempDirGuard::new("host-status-empty");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());

        let code = run_host_status().expect("run host status");
        assert_eq!(code, 1);
    }

    #[test]
    #[serial]
    fn host_stop_is_noop_without_state() {
        let runtime_dir = TempDirGuard::new("host-stop-empty");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());

        let code = run_host_stop().expect("run host stop");
        assert_eq!(code, 0);
    }

    #[test]
    fn is_process_alive_returns_true_for_current_process() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    #[serial]
    fn host_status_clears_stale_runtime_state() {
        let runtime_dir = TempDirGuard::new("host-status-stale");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());
        let paths = ConfigPaths::default();
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

        let code = run_host_status().expect("run status");
        assert_eq!(code, 1);
        assert!(
            load_host_runtime_state(&paths)
                .expect("load state")
                .is_none()
        );
    }

    #[test]
    #[serial]
    fn host_stop_clears_stale_runtime_state() {
        let runtime_dir = TempDirGuard::new("host-stop-stale");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());
        let paths = ConfigPaths::default();
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

        let code = run_host_stop().expect("run stop");
        assert_eq!(code, 0);
        assert!(
            load_host_runtime_state(&paths)
                .expect("load state")
                .is_none()
        );
    }

    #[test]
    #[serial]
    fn host_status_output_matches_state_file_fields() {
        let runtime_dir = TempDirGuard::new("host-status-output");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());

        let paths = ConfigPaths::default();
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
        let lines = format_host_status_lines(&loaded);
        assert_eq!(lines[0], "host runtime: running");
        assert_eq!(lines[1], "runtime: default");
        assert!(lines[2].starts_with("local ipc endpoint: "));
        assert!(lines.contains(&"name: demo-host".to_string()));
        assert!(lines.contains(&"pid: 9001".to_string()));
        assert!(lines.contains(&"target: iroh://endpoint-123".to_string()));
        assert!(lines.contains(&"share link: bmux://demo-host".to_string()));
        assert!(lines.contains(&"started_at_unix: 1700000123".to_string()));
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
                pane_shell_integration: false,
                no_pane_shell_integration: false,
                rolling_recording: false,
                no_rolling_recording: false,
                rolling_window_secs: None,
                rolling_event_kind_all: false,
                rolling_event_kind: Vec::new(),
                rolling_capture_input: false,
                no_rolling_capture_input: false,
                rolling_capture_output: false,
                no_rolling_capture_output: false,
                rolling_capture_events: false,
                no_rolling_capture_events: false,
                rolling_capture_protocol_replies: false,
                no_rolling_capture_protocol_replies: false,
                rolling_capture_images: false,
                no_rolling_capture_images: false,
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
    fn normalize_relay_url_for_display_trims_trailing_dot() {
        let normalized =
            normalize_relay_url_for_display("https://use1-1.relay.n0.iroh-canary.iroh.link./");
        assert_eq!(normalized, "https://use1-1.relay.n0.iroh-canary.iroh.link/");
    }

    #[test]
    fn normalize_relay_url_for_display_keeps_non_url_strings() {
        let normalized = normalize_relay_url_for_display("not-a-url");
        assert_eq!(normalized, "not-a-url");
    }

    #[test]
    fn resolve_hosted_mode_prefers_cli_override() {
        let config = BmuxConfig::default();
        let mode = resolve_hosted_mode(&config, Some(HostedModeArg::ControlPlane));
        assert_eq!(mode, HostedMode::ControlPlane);
    }

    #[test]
    fn resolve_hosted_mode_falls_back_to_config() {
        let mut config = BmuxConfig::default();
        config.connections.hosted_mode = HostedMode::ControlPlane;
        let mode = resolve_hosted_mode(&config, None);
        assert_eq!(mode, HostedMode::ControlPlane);
    }

    #[test]
    fn gateway_candidates_auto_defaults_to_cluster_targets() {
        let definition = ClusterGatewayDefinition {
            targets: vec!["db-a".to_string(), "db-b".to_string()],
            hosts: Vec::new(),
            gateway_mode: ClusterGatewayMode::Auto,
            gateway_candidates: Vec::new(),
            gateway_target: None,
        };

        let candidates = gateway_candidates_for_cluster("prod", &definition)
            .expect("auto mode should produce candidates");
        assert_eq!(candidates, vec!["db-a".to_string(), "db-b".to_string()]);
    }

    #[test]
    fn gateway_candidates_pinned_requires_target() {
        let definition = ClusterGatewayDefinition {
            gateway_mode: ClusterGatewayMode::Pinned,
            ..ClusterGatewayDefinition::default()
        };

        let error = gateway_candidates_for_cluster("prod", &definition)
            .expect_err("pinned mode without gateway_target should fail");
        assert!(error.to_string().contains("gateway_target is missing"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_prefers_explicit_cluster_flag() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([("prod".to_string(), ClusterGatewayDefinition::default())]),
        };

        let cluster = resolve_cluster_name_for_gateway(
            "cluster-events",
            &["--cluster".to_string(), "prod".to_string()],
            &settings,
        )
        .expect("cluster flag should resolve");
        assert_eq!(cluster.as_deref(), Some("prod"));

        let cluster_inline = resolve_cluster_name_for_gateway(
            "cluster-events",
            &["--cluster=prod".to_string()],
            &settings,
        )
        .expect("inline cluster flag should resolve");
        assert_eq!(cluster_inline.as_deref(), Some("prod"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_uses_single_cluster_default() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([("prod".to_string(), ClusterGatewayDefinition::default())]),
        };

        let cluster = resolve_cluster_name_for_gateway("cluster-pane-retry", &[], &settings)
            .expect("single-cluster default should resolve");
        assert_eq!(cluster.as_deref(), Some("prod"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_infers_cluster_from_host_when_unique() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([
                (
                    "prod".to_string(),
                    ClusterGatewayDefinition {
                        targets: vec!["prod-a".to_string()],
                        ..ClusterGatewayDefinition::default()
                    },
                ),
                (
                    "staging".to_string(),
                    ClusterGatewayDefinition {
                        targets: vec!["staging-a".to_string()],
                        ..ClusterGatewayDefinition::default()
                    },
                ),
            ]),
        };

        let cluster = resolve_cluster_name_for_gateway(
            "cluster-pane-new",
            &["--host".to_string(), "prod-a".to_string()],
            &settings,
        )
        .expect("unique host should infer cluster");
        assert_eq!(cluster.as_deref(), Some("prod"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_requires_cluster_for_retry_in_multi_cluster() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([
                ("prod".to_string(), ClusterGatewayDefinition::default()),
                ("staging".to_string(), ClusterGatewayDefinition::default()),
            ]),
        };

        let error = resolve_cluster_name_for_gateway("cluster-pane-retry", &[], &settings)
            .expect_err("retry should require --cluster in multi-cluster mode");
        assert!(error.to_string().contains("requires --cluster"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_rejects_ambiguous_host_mapping() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([
                (
                    "prod".to_string(),
                    ClusterGatewayDefinition {
                        targets: vec!["db-a".to_string()],
                        ..ClusterGatewayDefinition::default()
                    },
                ),
                (
                    "staging".to_string(),
                    ClusterGatewayDefinition {
                        targets: vec!["db-a".to_string()],
                        ..ClusterGatewayDefinition::default()
                    },
                ),
            ]),
        };

        let error = resolve_cluster_name_for_gateway(
            "cluster-pane-new",
            &["--host".to_string(), "db-a".to_string()],
            &settings,
        )
        .expect_err("ambiguous host mapping should hard fail");
        assert!(error.to_string().contains("matches multiple clusters"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_status_accepts_positional_cluster() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([("prod".to_string(), ClusterGatewayDefinition::default())]),
        };

        let cluster = resolve_cluster_name_for_gateway(
            "cluster-gateway-status",
            &["prod".to_string()],
            &settings,
        )
        .expect("gateway status positional cluster should resolve");
        assert_eq!(cluster.as_deref(), Some("prod"));
    }

    #[test]
    fn parse_gateway_overrides_strips_gateway_flags() {
        let overrides = parse_gateway_overrides(&[
            "--gateway".to_string(),
            "db-b".to_string(),
            "--gateway-mode=auto".to_string(),
            "--gateway-no-failover".to_string(),
            "--cluster".to_string(),
            "prod".to_string(),
        ])
        .expect("gateway overrides should parse");

        assert_eq!(overrides.gateway_target.as_deref(), Some("db-b"));
        assert_eq!(overrides.gateway_mode, Some(ClusterGatewayMode::Auto));
        assert!(overrides.no_failover);
        assert_eq!(
            overrides.passthrough_arguments,
            vec!["--cluster".to_string(), "prod".to_string()]
        );
    }

    #[test]
    fn ordered_gateway_candidates_prioritizes_recent_success() {
        clear_gateway_runtime_state_for_tests();
        record_gateway_success("prod", "db-b");
        let definition = ClusterGatewayDefinition {
            targets: vec!["db-a".to_string(), "db-b".to_string()],
            gateway_mode: ClusterGatewayMode::Auto,
            ..ClusterGatewayDefinition::default()
        };

        let ordered = ordered_gateway_candidates_for_cluster("prod", &definition)
            .expect("ordered candidates should resolve");
        assert_eq!(ordered, vec!["db-b".to_string(), "db-a".to_string()]);
    }

    #[test]
    fn gateway_candidates_auto_accepts_hosts_object_entries() {
        let definition = ClusterGatewayDefinition {
            hosts: vec![
                ClusterGatewayHostRef::Object {
                    target: Some("db-a".to_string()),
                    host: None,
                    name: None,
                },
                ClusterGatewayHostRef::Target("db-b".to_string()),
            ],
            gateway_mode: ClusterGatewayMode::Auto,
            ..ClusterGatewayDefinition::default()
        };

        let candidates = gateway_candidates_for_cluster("prod", &definition)
            .expect("hosts entries should be valid candidates");
        assert_eq!(candidates, vec!["db-a".to_string(), "db-b".to_string()]);
    }

    #[test]
    fn setup_check_not_ready_lines_prefers_setup_fix_and_auth_advanced() {
        let lines = format_setup_check_not_ready_lines(
            SetupAuthCheck::RequiredMissing,
            SetupHostCheck::Offline,
            SetupShareCheck::RequiredMissing,
        );
        assert_eq!(lines[0], "Status: not ready");
        assert_eq!(lines[1], "Reason: not signed in; host is offline");
        assert_eq!(lines[2], "Fix: bmux setup");
        assert_eq!(lines[3], "Advanced: bmux auth login");
    }

    #[test]
    fn setup_check_not_ready_lines_uses_host_restart_for_stale_runtime() {
        let state = HostRuntimeState {
            pid: 4242,
            target: "iroh://demo".to_string(),
            share_link: Some("bmux://demo".to_string()),
            name: Some("demo-host".to_string()),
            started_at_unix: 1,
        };
        let lines = format_setup_check_not_ready_lines(
            SetupAuthCheck::Ready,
            SetupHostCheck::Stale(state.pid),
            SetupShareCheck::RequiredMissing,
        );
        assert_eq!(lines[1], "Reason: host state is stale (pid 4242)");
        assert_eq!(lines[2], "Fix: bmux setup");
        assert_eq!(lines[3], "Advanced: bmux host --restart");
    }

    #[test]
    fn setup_check_not_ready_lines_p2p_does_not_require_auth() {
        let lines = format_setup_check_not_ready_lines(
            SetupAuthCheck::NotRequired,
            SetupHostCheck::Offline,
            SetupShareCheck::NotRequired,
        );
        assert_eq!(lines[1], "Reason: host is offline");
        assert_eq!(lines[2], "Fix: bmux setup");
        assert_eq!(lines[3], "Advanced: bmux host --daemon");
    }

    #[test]
    fn setup_check_not_ready_lines_report_missing_share_when_host_running() {
        let lines = format_setup_check_not_ready_lines(
            SetupAuthCheck::Ready,
            SetupHostCheck::Running,
            SetupShareCheck::RequiredMissing,
        );
        assert_eq!(lines[1], "Reason: share link unavailable");
        assert_eq!(lines[3], "Advanced: bmux share <target> --name <name>");
    }

    #[test]
    fn connect_target_resolution_error_adds_share_link_hint() {
        let error = anyhow::anyhow!("share link not found: bmux://demo");
        let mapped = map_connect_target_resolution_error("bmux://demo", error);
        assert!(mapped.to_string().contains("Fix: bmux setup"));
        assert!(mapped.to_string().contains("Advanced: bmux hosts"));
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
        let control_plane_url = format!("http://{address}");
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
            config: None,
            record: false,
            no_capture_input: false,
            recording_id_file: None,
            record_profile: None,
            record_name: None,
            record_event_kind: Vec::new(),
            stop_server_on_exit: false,
            recordings_dir: None,
            recording_auto_export: false,
            no_recording_auto_export: false,
            recording_auto_export_dir: None,
            target: None,
            runtime: None,
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
        let control_plane_url = format!("http://{address}");
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
            config: None,
            record: false,
            no_capture_input: false,
            recording_id_file: None,
            record_profile: None,
            record_name: None,
            record_event_kind: Vec::new(),
            stop_server_on_exit: false,
            recordings_dir: None,
            recording_auto_export: false,
            no_recording_auto_export: false,
            recording_auto_export_dir: None,
            target: None,
            runtime: None,
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
