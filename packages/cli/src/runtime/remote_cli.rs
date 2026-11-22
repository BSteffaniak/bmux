#[cfg(feature = "bundled-plugin-connections")]
use crate::ssh_access::authenticate_client_connection;
use crate::ssh_access::{IrohTargetCompression, parse_iroh_target as parse_iroh_target_parts};
use anyhow::{Context, Result};
use bmux_cli_schema::{Cli, Command, ServerAutostartCommand, ServerCommand};
use bmux_config::BmuxConfig;
#[cfg(feature = "bundled-plugin-connections")]
use bmux_connections_admin::known_gateways::{
    KnownGatewayState, certificate_fingerprint_sha256, fingerprints_equal,
    load_known_gateways_store, save_known_gateways_store, tls_gateway_key,
};
use bmux_remote_ssh::{
    SshTarget, parse_host_port_with_default, run_ssh_bmux_command, run_ssh_bmux_command_silent,
    ssh_destination,
};
#[cfg(feature = "bundled-plugin-connections")]
use bmux_remote_ssh::{build_ssh_command, map_ssh_execution_error};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
#[cfg(feature = "bundled-plugin-connections")]
use std::time::Instant;

use super::expand_bmux_target_if_needed;
#[cfg(feature = "bundled-plugin-connections")]
use super::perf_telemetry;

// The remote attach transports, trust store and admin helpers below are only
// reachable through the connections plugin; target parsing and SSH proxying
// above stay in every build.
#[cfg(feature = "bundled-plugin-connections")]
use super::{
    AttachExitReason, ConnectionContext, ConnectionPolicyScope, KernelClientFactory, connect,
    connect_with_context, hosted_output::status_not_ready_lines, map_cli_client_error,
    run_session_attach_with_client,
};
#[cfg(feature = "bundled-plugin-connections")]
use bmux_client::BmuxClient;
#[cfg(feature = "bundled-plugin-connections")]
use bmux_config::{ConfigPaths, TlsTrustMode};
#[cfg(feature = "bundled-plugin-connections")]
use bmux_sessions_plugin_api::sessions_state::{self, SessionSummary};
#[cfg(feature = "bundled-plugin-connections")]
use std::io::Write;
#[cfg(feature = "bundled-plugin-connections")]
use std::process::Stdio;
#[cfg(feature = "bundled-plugin-connections")]
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(feature = "bundled-plugin-connections")]
use uuid::Uuid;

#[cfg(feature = "bundled-plugin-connections")]
/// Typed dispatch wrapper for `sessions-state:list-sessions` via
/// `BmuxClient::invoke_service_raw`. `BmuxClient` no longer carries a
/// domain-specific `list_sessions` method — callers route through the
/// typed sessions-plugin API instead.
async fn typed_list_sessions_remote(client: &mut BmuxClient) -> Result<Vec<SessionSummary>> {
    sessions_state::client::list_sessions(client)
        .await
        .context("sessions-state list-sessions dispatch failed")
}
use bmux_config::{ConnectionTargetConfig, ConnectionTransport, RemoteServerStartMode};
#[cfg(feature = "bundled-plugin-connections")]
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::presets};
use std::ffi::OsString;
#[cfg(feature = "bundled-plugin-connections")]
use tokio::io::AsyncWriteExt;
#[cfg(feature = "bundled-plugin-connections")]
use {
    bmux_ipc::transport::ErasedIpcStream,
    rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    rustls::pki_types::{CertificateDer, ServerName, UnixTime},
    rustls::{DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme},
    serde::Serialize,
    std::pin::Pin,
    std::sync::{Arc, Mutex},
    std::task::{Context as TaskContext, Poll},
    tokio::io::{AsyncRead, AsyncWrite, ReadBuf},
    tokio::net::TcpStream,
    tokio::process::{Child, ChildStdin, ChildStdout, Command as TokioProcessCommand},
    tokio_rustls::TlsConnector,
};

#[derive(Debug, Clone)]
enum ResolvedTarget {
    Local,
    Ssh(SshTarget),
    Tls(TlsTarget),
    Iroh(IrohTarget),
}

#[derive(Debug, Clone)]
#[cfg_attr(
    not(feature = "bundled-plugin-connections"),
    expect(
        dead_code,
        reason = "TLS targets are parsed and validated in every build, but only the connections transport reads their endpoint details"
    )
)]
struct TlsTarget {
    label: String,
    host: String,
    port: u16,
    server_name: String,
    ca_file: Option<PathBuf>,
    connect_timeout_ms: u64,
}

#[cfg(feature = "bundled-plugin-connections")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsPinSource {
    Config,
    Local,
}

#[cfg(feature = "bundled-plugin-connections")]
#[derive(Debug, Clone)]
struct EffectiveTlsPin {
    fingerprint_sha256: String,
    source: TlsPinSource,
}

#[cfg(feature = "bundled-plugin-connections")]
#[derive(Debug, Clone)]
struct TlsTrustDecision {
    key: String,
    mode: TlsTrustMode,
    pin: Option<EffectiveTlsPin>,
}

#[cfg(feature = "bundled-plugin-connections")]
#[derive(Debug)]
struct BmuxTlsServerVerifier {
    trust: TlsTrustDecision,
    webpki: Option<Arc<dyn ServerCertVerifier>>,
}

#[cfg_attr(
    not(feature = "bundled-plugin-connections"),
    expect(
        dead_code,
        reason = "Iroh targets are parsed and validated in every build, but only the connections transport reads their connection details"
    )
)]
#[derive(Debug, Clone)]
struct IrohTarget {
    label: String,
    endpoint_id: String,
    relay_url: Option<String>,
    require_ssh_auth: bool,
    transport_compression: IrohTargetCompression,
    connect_timeout_ms: u64,
}

#[cfg(feature = "bundled-plugin-connections")]
const fn iroh_target_uses_compression(target: &IrohTarget, config: &BmuxConfig) -> bool {
    match target.transport_compression {
        IrohTargetCompression::Zstd => true,
        IrohTargetCompression::None => false,
        IrohTargetCompression::Auto => {
            config.behavior.compression.enabled
                && matches!(
                    config.behavior.compression.remote,
                    bmux_config::CompressionMode::Auto | bmux_config::CompressionMode::Zstd
                )
        }
    }
}

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
#[allow(clippy::cast_possible_truncation)]
fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(feature = "bundled-plugin-connections")]
const fn attach_exit_reason_label(reason: AttachExitReason) -> &'static str {
    match reason {
        AttachExitReason::Detached => "detached",
        AttachExitReason::StreamClosed => "stream_closed",
        AttachExitReason::Quit => "quit",
    }
}

#[cfg(feature = "bundled-plugin-connections")]
async fn emit_iroh_connect_perf_event(
    perf_emitter: &mut perf_telemetry::PerfEventEmitter,
    client: &mut BmuxClient,
    target: &IrohTarget,
    summary: &IrohConnectPerfSummary,
    reconnect_attempt: u64,
) -> Result<()> {
    if !perf_emitter.level_at_least(perf_telemetry::PerfCaptureLevel::Basic) {
        return Ok(());
    }

    let mut payload = serde_json::json!({
        "target": target.label,
        "reconnect_attempt": reconnect_attempt,
        "connect_ms": summary.connect_ms,
        "total_ms": summary.total_ms,
    });
    if perf_emitter.level_at_least(perf_telemetry::PerfCaptureLevel::Detailed)
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
    if perf_emitter.level_at_least(perf_telemetry::PerfCaptureLevel::Trace)
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

#[cfg(feature = "bundled-plugin-connections")]
async fn refresh_perf_emitter_settings_from_server(
    perf_emitter: &mut perf_telemetry::PerfEventEmitter,
    client: &mut BmuxClient,
) {
    if let Ok(settings) =
        bmux_performance_plugin_api::performance_state::client::get_settings(client).await
    {
        let settings = settings.into();
        perf_emitter.update_settings(perf_telemetry::PerfCaptureSettings::from_runtime_settings(
            &settings,
        ));
    }
}

#[cfg(feature = "bundled-plugin-connections")]
async fn emit_iroh_attach_attempt_perf_event(
    perf_emitter: &mut perf_telemetry::PerfEventEmitter,
    client: &mut BmuxClient,
    target: &IrohTarget,
    attach_attempt: u64,
    reconnect_attempt: u64,
    attach_runtime_ms: u64,
    exit_reason: AttachExitReason,
) -> Result<()> {
    if !perf_emitter.level_at_least(perf_telemetry::PerfCaptureLevel::Basic) {
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
    if perf_emitter.level_at_least(perf_telemetry::PerfCaptureLevel::Trace)
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

#[cfg(feature = "bundled-plugin-connections")]
async fn emit_iroh_reconnect_outage_perf_event(
    perf_emitter: &mut perf_telemetry::PerfEventEmitter,
    client: &mut BmuxClient,
    target: &IrohTarget,
    reconnect_attempt: u64,
    reconnect_backoff_ms: u64,
    outage_ms: u64,
    connect_summary: &IrohConnectPerfSummary,
) -> Result<()> {
    if !perf_emitter.level_at_least(perf_telemetry::PerfCaptureLevel::Basic) {
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
    if perf_emitter.level_at_least(perf_telemetry::PerfCaptureLevel::Detailed)
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

#[cfg(feature = "bundled-plugin-connections")]
pub(super) const SSH_RECONNECT_MAX_ATTEMPTS: usize = 4;
#[cfg(feature = "bundled-plugin-connections")]
const SSH_RECONNECT_BASE_BACKOFF_MS: u64 = 300;
#[cfg(feature = "bundled-plugin-connections")]
const BRIDGE_PREFLIGHT_TOKEN: &str = "BMUX_BRIDGE_READY";
#[cfg(feature = "bundled-plugin-connections")]
const BMUX_IROH_ALPN: &[u8] = b"bmux/gateway/iroh/1";
#[cfg(feature = "bundled-plugin-connections")]
#[derive(Debug)]
struct SshBridgeStream {
    _child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
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
    // Connection management always runs locally. With its plugin absent these
    // paths arrive as external commands; they still must not run remotely.
    if matches!(
        command,
        Command::Server {
            command: ServerCommand::Autostart { .. },
        }
    ) || matches!(command, Command::External(args)
        if args.first().is_some_and(|name| matches!(name.as_str(), "connect" | "remote" | "access")))
    {
        return Ok(false);
    }
    target_is_ssh(cli.target.as_deref()).await
}

/// Whether a plugin-owned command runs on an SSH target by re-invoking bmux
/// there. These commands were built in, and built-in commands were proxied to
/// SSH targets; connect, remote and access administration always ran locally.
pub(super) fn plugin_command_proxies_to_target(plugin_id: &str, command_name: &str) -> bool {
    match plugin_id {
        "bmux.clients" => matches!(command_name, "follow" | "list-clients" | "unfollow"),
        "bmux.connections" => matches!(
            command_name,
            "auth-login"
                | "auth-logout"
                | "auth-status"
                | "host"
                | "hosts"
                | "join"
                | "kiosk-attach"
                | "kiosk-init"
                | "kiosk-issue-token"
                | "kiosk-revoke-token"
                | "kiosk-ssh-print-config"
                | "kiosk-status"
                | "setup"
                | "share"
                | "unshare"
        ),
        "bmux.performance" => matches!(command_name, "perf-off" | "perf-on" | "perf-status"),
        "bmux.playbook" => matches!(
            command_name,
            "cleanup" | "diff" | "dry-run" | "interactive" | "run" | "validate"
        ),
        "bmux.plugin_cli" => matches!(
            command_name,
            "diagnostics-watch"
                | "logs-profiles-delete"
                | "logs-profiles-list"
                | "logs-profiles-rename"
                | "logs-profiles-show"
                | "logs-tail"
                | "logs-watch"
        ),
        "bmux.sessions" => matches!(
            command_name,
            "detach"
                | "kill-all-sessions"
                | "kill-session"
                | "list-sessions"
                | "new-session"
                | "session-attach"
                | "session-new"
        ),
        "bmux.snapshot" => matches!(command_name, "restore" | "save"),
        _ => false,
    }
}

/// Whether the explicit or configured default target is an SSH target.
pub(super) async fn target_is_ssh(target: Option<&str>) -> Result<bool> {
    let config = BmuxConfig::load()?;
    let target = resolve_effective_target(&config, target).await?;
    Ok(matches!(target, ResolvedTarget::Ssh(_)))
}

pub(super) async fn run_target_proxy_from_current_argv(cli: &Cli) -> Result<u8> {
    run_target_proxy(
        cli.target.as_deref(),
        command_requires_remote_server(cli.command.as_ref()),
        command_needs_tty(cli.command.as_ref()),
    )
    .await
}

/// Re-run the current command line on the selected SSH target.
pub(super) async fn run_target_proxy(
    target: Option<&str>,
    requires_remote_server: bool,
    wants_tty: bool,
) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let target = resolve_effective_target(&config, target).await?;
    match target {
        ResolvedTarget::Ssh(target) => {
            let argv = std::env::args_os().collect::<Vec<_>>();
            let remote_args = strip_target_argument(&argv);
            if requires_remote_server {
                ensure_remote_server_ready(&target).await?;
            }
            let needs_tty = wants_tty && io::stdin().is_terminal() && io::stdout().is_terminal();
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
                | ServerCommand::Autostart {
                    command: ServerAutostartCommand::Install { .. }
                        | ServerAutostartCommand::Uninstall
                        | ServerAutostartCommand::Status { .. }
                        | ServerAutostartCommand::Print { .. },
                }
                | ServerCommand::Gateway { .. }
                | ServerCommand::Bridge { .. }
        })
    )
}

/// A follow attachment gets its session from the followed client. Do not
/// prompt (or connect a picker client) when the caller supplied a selector.
#[cfg(feature = "bundled-plugin-connections")]
async fn select_attach_session(
    session: Option<&str>,
    follow: Option<&str>,
    picker: impl std::future::Future<Output = Result<Option<String>>>,
) -> Result<Option<String>> {
    match (session, follow) {
        (Some(_), Some(_)) => {
            anyhow::bail!("--follow cannot be used with an explicit session argument")
        }
        (None, Some(_)) => Ok(None),
        (Some(session), None) => Ok(Some(session.to_owned())),
        (None, None) => picker.await,
    }
}

/// Attach an explicit endpoint using the selected runtime's terminal transport.
#[cfg(feature = "bundled-plugin-connections")]
pub(super) async fn attach_endpoint(
    state: &super::plugin_runtime::RuntimeCommandState,
    target: &str,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
) -> Result<(u8, String, Option<String>)> {
    let config = &state.config;
    let resolved = resolve_target_reference_inner(config, target)
        .map_err(|error| map_connect_target_resolution_error(target, error))?;
    match resolved {
        ResolvedTarget::Local => {
            let target_session = select_attach_session(
                session,
                follow,
                Box::pin(resolve_local_attach_session(config, Some(&state.paths))),
            )
            .await?;
            let status = Box::pin(super::attach_selected::run_attach(
                state,
                target_session.as_deref(),
                follow,
                global,
            ))
            .await?;
            Ok((status, "local".to_owned(), target_session))
        }
        ResolvedTarget::Ssh(ssh_target) => {
            let ssh_control_path = ssh_control_path_for_session();
            let mut client = connect_remote_bridge(
                &ssh_target,
                "bmux-cli-connect-remote",
                Some(&ssh_control_path),
            )
            .await?;
            let target_session = select_attach_session(
                session,
                follow,
                resolve_remote_attach_session(&mut client, &ssh_target.label),
            )
            .await?;
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
            Ok((status, ssh_target.label, target_session))
        }
        ResolvedTarget::Tls(tls_target) => {
            let mut client = connect_tls_bridge(&tls_target, "bmux-cli-connect-remote-tls").await?;
            let target_session = select_attach_session(
                session,
                follow,
                resolve_remote_attach_session(&mut client, &tls_target.label),
            )
            .await?;
            let status = run_tls_attach_with_reconnect(
                client,
                &tls_target,
                target_session.as_deref(),
                follow,
                global,
                reconnect_forever,
            )
            .await?;
            Ok((status, tls_target.label, target_session))
        }
        ResolvedTarget::Iroh(iroh_target) => {
            attach_iroh_endpoint(
                config,
                iroh_target,
                session,
                follow,
                global,
                reconnect_forever,
            )
            .await
        }
    }
}

#[cfg(feature = "bundled-plugin-connections")]
async fn attach_iroh_endpoint(
    config: &BmuxConfig,
    iroh_target: IrohTarget,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
) -> Result<(u8, String, Option<String>)> {
    let mut perf_emitter = perf_telemetry::PerfEventEmitter::new(
        perf_telemetry::PerfCaptureSettings::from_config(config),
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
    let target_session = select_attach_session(
        session,
        follow,
        resolve_remote_attach_session(&mut client, &iroh_target.label),
    )
    .await?;
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
    Ok((status, iroh_target.label, target_session))
}

#[cfg(feature = "bundled-plugin-connections")]
fn map_connect_target_resolution_error(target: &str, error: anyhow::Error) -> anyhow::Error {
    if target.starts_with("bmux://") && error.to_string().contains("share link not found:") {
        return actionable_error(&error.to_string(), "bmux setup", Some("bmux hosts"));
    }
    error
}

#[cfg(feature = "bundled-plugin-connections")]
pub(super) async fn connect_attach_target_with_kernel(
    target: &str,
    client_name: &'static str,
) -> Result<(BmuxClient, Option<KernelClientFactory>)> {
    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target)
        .await
        .map_err(|error| map_connect_target_resolution_error(target, error))?;
    match resolved {
        ResolvedTarget::Local => {
            let client = connect_with_context(
                ConnectionPolicyScope::Normal,
                client_name,
                ConnectionContext::new(Some("local")),
            )
            .await?;
            Ok((client, None))
        }
        ResolvedTarget::Ssh(ssh_target) => {
            let ssh_control_path = ssh_control_path_for_session();
            let client =
                connect_remote_bridge(&ssh_target, client_name, Some(&ssh_control_path)).await?;
            let kernel_factory = build_ssh_kernel_client_factory(&ssh_target, ssh_control_path);
            Ok((client, Some(kernel_factory)))
        }
        ResolvedTarget::Tls(tls_target) => {
            let client = connect_tls_bridge(&tls_target, client_name).await?;
            let kernel_factory = build_tls_kernel_client_factory(&tls_target);
            Ok((client, Some(kernel_factory)))
        }
        ResolvedTarget::Iroh(iroh_target) => {
            let (client, connection) = connect_iroh_bridge(&iroh_target, client_name, None).await?;
            let kernel_factory = build_iroh_kernel_client_factory(connection, &iroh_target);
            Ok((client, Some(kernel_factory)))
        }
    }
}

#[cfg(feature = "bundled-plugin-connections")]
fn format_actionable_error_lines(reason: &str, fix: &str, advanced: Option<&str>) -> Vec<String> {
    status_not_ready_lines(Some(reason), fix, advanced)
        .into_iter()
        .skip(1)
        .collect()
}

#[cfg(feature = "bundled-plugin-connections")]
fn actionable_error(reason: &str, fix: &str, advanced: Option<&str>) -> anyhow::Error {
    anyhow::anyhow!(format_actionable_error_lines(reason, fix, advanced).join("\n"))
}

// Only connection establishment is retried. Never wrap attachment, input, or a
// service mutation in this helper: a lost response does not establish non-execution.
#[cfg(feature = "bundled-plugin-connections")]
async fn retry_iroh_connection<T, Connect, ConnectFuture, Sleep, SleepFuture>(
    attempts: &mut usize,
    forever: bool,
    mut connect: Connect,
    mut sleep: Sleep,
) -> Result<Option<(T, u64)>>
where
    Connect: FnMut(usize) -> ConnectFuture,
    ConnectFuture: std::future::Future<Output = Result<T>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: std::future::Future<Output = ()>,
{
    while forever || *attempts < SSH_RECONNECT_MAX_ATTEMPTS {
        *attempts = attempts.saturating_add(1);
        let backoff_ms = reconnect_backoff_ms(*attempts);
        sleep(Duration::from_millis(backoff_ms)).await;
        match connect(*attempts).await {
            Ok(connection) => return Ok(Some((connection, backoff_ms))),
            Err(error) if iroh_connect_error_is_transient(&error) => {
                tracing::debug!(attempt = *attempts, %error, "transient iroh reconnect failure");
            }
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

#[cfg(feature = "bundled-plugin-connections")]
fn iroh_connect_error_is_transient(error: &anyhow::Error) -> bool {
    // SSH auth currently exposes anyhow rather than a typed rejection contract.
    // Treat its existing stage boundary as terminal, including local agent I/O;
    // connectivity errors inside authentication must not retry revoked trust.
    if error
        .chain()
        .any(|source| source.to_string() == "iroh SSH auth handshake failed")
    {
        return false;
    }
    // Fail closed for unknown/untyped failures, including SSH auth rejections.
    // Inspect the outermost typed boundary first so a protocol/auth error cannot
    // become retryable merely because its source contains an I/O error.
    for source in error.chain() {
        if let Some(error) = source.downcast_ref::<bmux_client::ClientError>() {
            return match error {
                bmux_client::ClientError::Timeout(_) => true,
                bmux_client::ClientError::Transport(
                    bmux_ipc::transport::IpcTransportError::Io(error),
                ) => iroh_io_error_is_transient(error),
                _ => false,
            };
        }
        if let Some(error) = source.downcast_ref::<iroh::endpoint::ConnectingError>()
            && !matches!(
                error,
                iroh::endpoint::ConnectingError::ConnectionError { .. }
            )
        {
            return false;
        }
        if let Some(error) = source.downcast_ref::<iroh::endpoint::ConnectionError>() {
            return matches!(
                error,
                iroh::endpoint::ConnectionError::TimedOut | iroh::endpoint::ConnectionError::Reset
            );
        }
        if source.is::<tokio::time::error::Elapsed>() {
            return true;
        }
        if let Some(error) = source.downcast_ref::<io::Error>() {
            return iroh_io_error_is_transient(error);
        }
    }
    false
}

#[cfg(feature = "bundled-plugin-connections")]
fn iroh_io_error_is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "Existing transport attachment passes independent runtime and telemetry state."
)]
#[cfg(feature = "bundled-plugin-connections")]
async fn run_iroh_attach_with_reconnect(
    mut client: BmuxClient,
    mut iroh_connection: iroh::endpoint::Connection,
    target: &IrohTarget,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
    reconnect_forever: bool,
    mut perf_emitter: perf_telemetry::PerfEventEmitter,
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
        let recovered = retry_iroh_connection(
            &mut reconnect_attempt,
            reconnect_forever,
            |attempt| async move {
                println!(
                    "reconnecting to iroh target '{}' (attempt {attempt})...",
                    target.label
                );
                let mut perf = IrohConnectPerfSummary::default();
                let (client, connection) = connect_iroh_bridge(
                    target,
                    "bmux-cli-connect-remote-iroh-reconnect",
                    Some(&mut perf),
                )
                .await?;
                Ok((client, connection, perf))
            },
            tokio::time::sleep,
        )
        .await?;
        let Some(((mut new_client, new_connection, connect_perf), reconnect_backoff_ms)) =
            recovered
        else {
            println!(
                "remote iroh connection closed; giving up after {SSH_RECONNECT_MAX_ATTEMPTS} reconnect attempts"
            );
            return Ok(1);
        };
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

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
mod standalone_admin {
    use super::connect_iroh_bridge;
    use super::{
        BmuxConfig, ConnectionContext, ConnectionPolicyScope, OsString, ResolvedTarget, Result,
        connect, connect_remote_bridge, connect_tls_bridge, connect_with_context,
        ensure_tls_gateway_trusted_interactively, map_cli_client_error, parse_iroh_target_parts,
        resolve_target_reference, run_remote_install_server_for_target, run_ssh_bmux_command,
        tls_target_from_config, typed_list_sessions_remote,
    };
    use bmux_connections_admin::remote::{ProbeTarget, ProbeTransport};
    use std::sync::Arc;
    fn block_on_admin<F: std::future::Future>(future: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
    }
    impl ProbeTransport for ResolvedTarget {
        fn target(&self) -> ProbeTarget<'_> {
            match self {
                Self::Local => ProbeTarget::Local,
                Self::Ssh(t) => ProbeTarget::Ssh(&t.label),
                Self::Tls(t) => ProbeTarget::Tls {
                    label: &t.label,
                    host: &t.host,
                    port: t.port,
                },
                Self::Iroh(t) => ProbeTarget::Iroh(&t.label),
            }
        }
        fn ping(&self, name: &'static str) -> Result<()> {
            block_on_admin(async {
                let mut client = match self {
                    Self::Local => connect(ConnectionPolicyScope::Normal, name).await?,
                    Self::Tls(t) => connect_tls_bridge(t, name).await?,
                    Self::Iroh(t) => connect_iroh_bridge(t, name, None).await?.0,
                    Self::Ssh(_) => anyhow::bail!("SSH probe requires a remote command"),
                };
                client.ping().await.map_err(map_cli_client_error)
            })
        }
        fn ssh(&self, argv: &[&str]) -> Result<()> {
            let Self::Ssh(t) = self else {
                anyhow::bail!("SSH target required")
            };
            run_ssh_bmux_command(
                t,
                &argv.iter().map(OsString::from).collect::<Vec<_>>(),
                false,
            )
            .map(drop)
        }
        fn install(&self) -> Result<()> {
            let Self::Ssh(t) = self else {
                anyhow::bail!("SSH target required")
            };
            block_on_admin(run_remote_install_server_for_target(t))
        }
    }
    async fn list_sessions(target: &str) -> Result<Vec<String>> {
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
        let sessions = typed_list_sessions_remote(&mut client).await?;
        Ok(sessions
            .into_iter()
            .map(|s| s.name.unwrap_or_else(|| s.id.to_string()))
            .collect())
    }
    pub(in crate::runtime) fn ports(
        target: Option<String>,
    ) -> bmux_connections_plugin::remote_admin::AdminPorts {
        bmux_connections_plugin::remote_admin::AdminPorts {
            probe: Arc::new(|target| {
                Ok(Box::new(block_on_admin(resolve_target_reference(
                    &BmuxConfig::load()?,
                    target,
                ))?))
            }),
            parse_iroh: |value| {
                let parsed = parse_iroh_target_parts(value)?;
                Ok((
                    parsed.endpoint_id,
                    parsed.relay_url,
                    parsed.require_ssh_auth,
                ))
            },
            trust: Arc::new(|name, target| {
                if let Some(t) = tls_target_from_config(name, target)? {
                    block_on_admin(ensure_tls_gateway_trusted_interactively(&t))?;
                }
                Ok(())
            }),
            sessions: Arc::new(|target| block_on_admin(list_sessions(target))),
            kiosk_attach: Arc::new(|grant| {
                block_on_admin(crate::runtime::kiosk_cli::attach_authorized(grant))
            }),
            target,
        }
    }
}
#[cfg(feature = "bundled-plugin-connections")]
pub(super) use standalone_admin::ports as admin_ports;

#[cfg(feature = "bundled-plugin-connections")]
async fn resolve_local_attach_session(
    config: &BmuxConfig,
    selected: Option<&ConfigPaths>,
) -> Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "session argument is required in non-interactive mode.\nList sessions: bmux list-sessions"
        );
    }
    let mut client = if let Some(paths) = selected {
        super::attach_selected::connect_local(config, paths, "bmux-cli-connect-local-picker")
            .await?
    } else {
        connect_with_context(
            ConnectionPolicyScope::Normal,
            "bmux-cli-connect-local-picker",
            ConnectionContext::new(Some("local")),
        )
        .await?
    };
    let sessions = typed_list_sessions_remote(&mut client).await?;
    select_session_interactively("local", &sessions)
}

#[cfg(feature = "bundled-plugin-connections")]
async fn resolve_remote_attach_session(
    client: &mut BmuxClient,
    target_label: &str,
) -> Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "session argument is required in non-interactive mode.\nList sessions: bmux --target {target_label} list-sessions"
        );
    }
    let sessions = typed_list_sessions_remote(client).await?;
    select_session_interactively(target_label, &sessions)
}

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
async fn connect_tls_bridge(target: &TlsTarget, client_name: &str) -> Result<BmuxClient> {
    ensure_tls_gateway_trusted_for_policy(target).await?;
    match try_connect_tls_bridge(target, client_name).await {
        Ok(client) => Ok(client),
        Err(error) if should_retry_after_tls_trust_prompt(&error) => {
            ensure_tls_gateway_trusted_interactively(target).await?;
            try_connect_tls_bridge(target, client_name).await
        }
        Err(error) => Err(error),
    }
}

#[cfg(feature = "bundled-plugin-connections")]
async fn try_connect_tls_bridge(target: &TlsTarget, client_name: &str) -> Result<BmuxClient> {
    let config = BmuxConfig::load().unwrap_or_default();
    let connector = build_tls_connector(target, &config)?;
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

#[cfg(feature = "bundled-plugin-connections")]
async fn connect_iroh_bridge(
    target: &IrohTarget,
    client_name: &str,
    perf_summary: Option<&mut IrohConnectPerfSummary>,
) -> Result<(BmuxClient, iroh::endpoint::Connection)> {
    // One deadline bounds bind, endpoint-online, QUIC, authentication, and IPC.
    iroh_connect_with_deadline(
        target.connect_timeout_ms,
        Box::pin(connect_iroh_bridge_inner(target, client_name, perf_summary)),
    )
    .await
    .with_context(|| format!("failed establishing iroh target '{}'", target.label))
}

#[cfg(feature = "bundled-plugin-connections")]
async fn iroh_connect_with_deadline<T>(
    timeout_ms: u64,
    connect: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(Duration::from_millis(timeout_ms.max(1)), connect)
        .await
        .context("iroh connection establishment deadline elapsed")?
}

#[allow(clippy::too_many_lines)] // staged iroh bridge setup is clearer inline for troubleshooting
#[cfg(feature = "bundled-plugin-connections")]
async fn connect_iroh_bridge_inner(
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
    // Preserve typed errors for reconnect policy; the local-IPC diagnostic mapper
    // erases EOF/refused errors and suggests starting a local server.
    .map_err(anyhow::Error::from)?;
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
#[cfg(feature = "bundled-plugin-connections")]
fn build_iroh_kernel_client_factory(
    connection: iroh::endpoint::Connection,
    target: &IrohTarget,
) -> KernelClientFactory {
    let timeout = Duration::from_millis(target.connect_timeout_ms.max(1));
    let principal_id = load_or_create_local_principal_id(&ConfigPaths::default())
        .unwrap_or_else(|_| Uuid::new_v4());
    let config = BmuxConfig::load().unwrap_or_default();
    let use_compression = iroh_target_uses_compression(target, &config);

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

#[cfg(feature = "bundled-plugin-connections")]
/// Generate a unique SSH `ControlMaster` socket path for this attach session.
fn ssh_control_path_for_session() -> String {
    format!("/tmp/bmux-ssh-{}", Uuid::new_v4().as_simple())
}

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
fn build_tls_connector(target: &TlsTarget, config: &BmuxConfig) -> Result<TlsConnector> {
    let verifier = build_tls_server_verifier(target, config)?;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("configuring TLS protocol versions")?
    .dangerous()
    .with_custom_certificate_verifier(verifier)
    .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

#[cfg(feature = "bundled-plugin-connections")]
fn build_tls_server_verifier(
    target: &TlsTarget,
    config: &BmuxConfig,
) -> Result<Arc<dyn ServerCertVerifier>> {
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

    let webpki = if roots.is_empty() {
        if target.ca_file.is_some() {
            anyhow::bail!(
                "no TLS trust roots available for target '{}'; install system certs or set ca_file",
                target.label
            );
        }
        None
    } else {
        Some(
            rustls::client::WebPkiServerVerifier::builder_with_provider(
                Arc::new(roots),
                Arc::new(rustls::crypto::ring::default_provider()),
            )
            .build()
            .context("failed building TLS server verifier")?
                as Arc<dyn ServerCertVerifier>,
        )
    };
    Ok(Arc::new(BmuxTlsServerVerifier {
        trust: tls_trust_decision(target, config)?,
        webpki,
    }))
}

#[cfg(feature = "bundled-plugin-connections")]
fn tls_trust_decision(target: &TlsTarget, config: &BmuxConfig) -> Result<TlsTrustDecision> {
    let key = tls_gateway_key(&target.host, target.port);
    let config_pin = config
        .connections
        .tls_trust
        .known_gateways
        .get(&key)
        .filter(|gateway| !gateway.fingerprint_sha256.trim().is_empty());
    let local_store = load_known_gateways_store(&ConfigPaths::default())?;
    let local_pin = local_store
        .gateways
        .get(&key)
        .filter(|gateway| !gateway.fingerprint_sha256.trim().is_empty());
    let pin = config_pin
        .map(|gateway| EffectiveTlsPin {
            fingerprint_sha256: gateway.fingerprint_sha256.clone(),
            source: TlsPinSource::Config,
        })
        .or_else(|| {
            local_pin.map(|gateway| EffectiveTlsPin {
                fingerprint_sha256: gateway.fingerprint_sha256.clone(),
                source: TlsPinSource::Local,
            })
        });
    Ok(TlsTrustDecision {
        key,
        mode: config.connections.tls_trust.mode,
        pin,
    })
}

#[cfg(feature = "bundled-plugin-connections")]
impl ServerCertVerifier for BmuxTlsServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        if let Some(webpki) = self.webpki.as_ref()
            && webpki
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .is_ok()
        {
            return Ok(ServerCertVerified::assertion());
        }

        let fingerprint = certificate_fingerprint_sha256(end_entity.as_ref());
        if let Some(pin) = self.trust.pin.as_ref() {
            if fingerprints_equal(&fingerprint, &pin.fingerprint_sha256) {
                return Ok(ServerCertVerified::assertion());
            }
            let source = match pin.source {
                TlsPinSource::Config => "declarative config",
                TlsPinSource::Local => "local known-gateways store",
            };
            return Err(RustlsError::General(format!(
                "bmux TLS gateway identity changed for {}; expected {} from {}, received {}",
                self.trust.key, pin.fingerprint_sha256, source, fingerprint
            )));
        }

        match self.trust.mode {
            TlsTrustMode::TrustNew => Ok(ServerCertVerified::assertion()),
            TlsTrustMode::Prompt => Err(RustlsError::General(format!(
                "bmux TLS gateway {} is unknown; fingerprint {}; trust prompt required",
                self.trust.key, fingerprint
            ))),
            TlsTrustMode::RequireKnown => Err(RustlsError::General(format!(
                "bmux TLS gateway {} is unknown; add a declarative pin or run 'bmux remote trust add tls://{}'",
                self.trust.key, self.trust.key
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        if let Some(webpki) = self.webpki.as_ref() {
            return webpki.verify_tls12_signature(message, cert, dss);
        }
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, &algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        if let Some(webpki) = self.webpki.as_ref() {
            return webpki.verify_tls13_signature(message, cert, dss);
        }
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        if let Some(webpki) = self.webpki.as_ref() {
            return webpki.supported_verify_schemes();
        }
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(feature = "bundled-plugin-connections")]
fn should_retry_after_tls_trust_prompt(error: &anyhow::Error) -> bool {
    error.to_string().contains("trust prompt required")
}

#[cfg(feature = "bundled-plugin-connections")]
async fn ensure_tls_gateway_trusted_for_policy(target: &TlsTarget) -> Result<()> {
    let config = BmuxConfig::load().unwrap_or_default();
    if target.ca_file.is_some() || config.connections.tls_trust.mode != TlsTrustMode::TrustNew {
        return Ok(());
    }
    let decision = tls_trust_decision(target, &config)?;
    if decision.pin.is_some() {
        return Ok(());
    }
    let fingerprint = probe_tls_gateway_fingerprint(target).await?;
    trust_tls_gateway(target, &fingerprint, None)
}

#[cfg(feature = "bundled-plugin-connections")]
async fn ensure_tls_gateway_trusted_interactively(target: &TlsTarget) -> Result<()> {
    let peer_fingerprint = probe_tls_gateway_fingerprint(target).await?;
    let config = BmuxConfig::load().unwrap_or_default();
    let key = tls_gateway_key(&target.host, target.port);
    if let Some(pin) = config.connections.tls_trust.known_gateways.get(&key)
        && !fingerprints_equal(&pin.fingerprint_sha256, &peer_fingerprint)
    {
        anyhow::bail!(
            "bmux TLS gateway identity changed for {key}; expected {} from declarative config, received {peer_fingerprint}",
            pin.fingerprint_sha256
        );
    }

    let paths = ConfigPaths::default();
    let local_store = load_known_gateways_store(&paths)?;
    if let Some(pin) = local_store.gateways.get(&key) {
        if fingerprints_equal(&pin.fingerprint_sha256, &peer_fingerprint) {
            return Ok(());
        }
        anyhow::bail!(
            "bmux TLS gateway identity changed for {key}; expected {} from local known-gateways store, received {peer_fingerprint}",
            pin.fingerprint_sha256
        );
    }

    if config.connections.tls_trust.mode == TlsTrustMode::RequireKnown {
        anyhow::bail!(
            "bmux TLS gateway {key} is unknown; add a declarative pin or run 'bmux remote trust add tls://{key}'"
        );
    }

    if config.connections.tls_trust.mode == TlsTrustMode::Prompt && !io::stdin().is_terminal() {
        anyhow::bail!(
            "bmux TLS gateway {key} is unknown; refusing to prompt without an interactive terminal"
        );
    }

    if config.connections.tls_trust.mode == TlsTrustMode::Prompt {
        println!("The bmux TLS gateway at {key} is unknown.\n");
        println!("Certificate fingerprint:");
        println!("  {peer_fingerprint}\n");
        print!("Trust this gateway and remember it? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
            anyhow::bail!("TLS gateway trust declined for {key}");
        }
    }

    trust_tls_gateway(target, &peer_fingerprint, None)?;
    Ok(())
}

#[cfg(feature = "bundled-plugin-connections")]
fn trust_tls_gateway(target: &TlsTarget, fingerprint: &str, label: Option<&str>) -> Result<()> {
    let paths = ConfigPaths::default();
    let mut store = load_known_gateways_store(&paths)?;
    let key = tls_gateway_key(&target.host, target.port);
    store.gateways.insert(
        key,
        KnownGatewayState {
            fingerprint_sha256: fingerprint.to_string(),
            trusted_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            server_name: Some(target.server_name.clone()),
            label: label
                .map(str::to_string)
                .or_else(|| Some(target.label.clone())),
        },
    );
    save_known_gateways_store(&paths, &store)
}

#[cfg(feature = "bundled-plugin-connections")]
async fn probe_tls_gateway_fingerprint(target: &TlsTarget) -> Result<String> {
    let verifier = Arc::new(ProbeTlsServerVerifier::default());
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .context("configuring TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth(),
    ));
    let address = format!("{}:{}", target.host, target.port);
    let tcp_stream = tokio::time::timeout(
        Duration::from_millis(target.connect_timeout_ms.max(1)),
        TcpStream::connect(&address),
    )
    .await
    .with_context(|| format!("timed out probing TLS target '{}'", target.label))?
    .with_context(|| format!("failed probing TLS target '{}'", target.label))?;
    let server_name = ServerName::try_from(target.server_name.clone())
        .map_err(|_| anyhow::anyhow!("invalid TLS server name '{}'", target.server_name))?;
    let _ = connector
        .connect(server_name, tcp_stream)
        .await
        .with_context(|| {
            format!(
                "failed probing TLS certificate for target '{}'",
                target.label
            )
        })?;
    verifier
        .fingerprint
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "TLS gateway '{}' did not present a certificate",
                target.label
            )
        })
}

#[cfg(feature = "bundled-plugin-connections")]
#[derive(Debug, Default)]
struct ProbeTlsServerVerifier {
    fingerprint: Mutex<Option<String>>,
}

#[cfg(feature = "bundled-plugin-connections")]
impl ServerCertVerifier for ProbeTlsServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        *self
            .fingerprint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(certificate_fingerprint_sha256(end_entity.as_ref()));
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, &algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
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

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
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

#[cfg(feature = "bundled-plugin-connections")]
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

/// Built-in commands that need a remote terminal when proxied: `attach`.
const fn command_needs_tty(command: Option<&Command>) -> bool {
    matches!(command, Some(Command::Attach { .. }))
}

#[cfg(feature = "bundled-plugin-connections")]
#[allow(clippy::cast_possible_truncation)] // Attempt count bounded to small values
pub(super) fn reconnect_backoff_ms(attempt: usize) -> u64 {
    let exponent = attempt.saturating_sub(1).min(10) as u32;
    SSH_RECONNECT_BASE_BACKOFF_MS.saturating_mul(2u64.saturating_pow(exponent))
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

fn tls_target_from_config(
    name: &str,
    target: &ConnectionTargetConfig,
) -> Result<Option<TlsTarget>> {
    if target.transport != ConnectionTransport::Tls {
        return Ok(None);
    }
    let host = target
        .host
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("TLS target '{name}' requires host"))?
        .to_string();
    Ok(Some(TlsTarget {
        label: name.to_string(),
        port: target.port.unwrap_or(443),
        server_name: target.server_name.clone().unwrap_or_else(|| host.clone()),
        ca_file: target.ca_file.clone(),
        connect_timeout_ms: target.connect_timeout_ms.max(1),
        host,
    }))
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
        ConnectionTransport::Tls => tls_target_from_config(name, target)?.map_or_else(
            || unreachable!("TLS transport should produce a TLS target"),
            |target| Ok(ResolvedTarget::Tls(target)),
        ),
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
                transport_compression: IrohTargetCompression::Auto,
                connect_timeout_ms: target.connect_timeout_ms.max(1),
            }))
        }
    }
}

fn parse_inline_ssh_target(target: &str) -> Result<ResolvedTarget> {
    bmux_remote_ssh::parse_inline_ssh_target(target).map(ResolvedTarget::Ssh)
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
        transport_compression: parsed.transport_compression,
        connect_timeout_ms: 8_000,
    }))
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use std::ffi::OsString;

    #[cfg(feature = "bundled-plugin-connections")]
    #[tokio::test]
    async fn explicit_and_follow_attach_do_not_poll_the_picker() {
        let unexpected = || async { panic!("unexpected session picker") };
        assert_eq!(
            select_attach_session(Some("named"), None, unexpected())
                .await
                .unwrap()
                .as_deref(),
            Some("named")
        );
        assert_eq!(
            select_attach_session(None, Some("client"), unexpected())
                .await
                .unwrap(),
            None
        );
        assert!(
            select_attach_session(Some("named"), Some("client"), unexpected())
                .await
                .is_err()
        );
        assert_eq!(
            select_attach_session(None, None, async { Ok(Some("picked".into())) })
                .await
                .unwrap()
                .as_deref(),
            Some("picked")
        );
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
    fn command_requires_remote_server_skips_server_autostart() {
        let command = Command::Server {
            command: ServerCommand::Autostart {
                command: ServerAutostartCommand::Status { json: false },
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
        let command = Command::External(vec!["list-sessions".into()]);
        assert!(command_requires_remote_server(Some(&command)));
    }

    #[cfg(feature = "bundled-plugin-connections")]
    #[tokio::test]
    async fn iroh_reconnect_retries_failures_without_reattaching() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let recovered = retry_iroh_connection(
            &mut attempts,
            false,
            |attempt| {
                std::future::ready(if attempt < 3 {
                    Err(io::Error::from(io::ErrorKind::ConnectionReset).into())
                } else {
                    Ok("replacement connection")
                })
            },
            |delay| {
                delays.push(delay.as_millis());
                std::future::ready(())
            },
        )
        .await
        .expect("transient errors retry");
        assert_eq!(recovered, Some(("replacement connection", 1_200)));
        assert_eq!(attempts, 3);
        assert_eq!(delays, [300, 600, 1_200]);
    }

    #[cfg(feature = "bundled-plugin-connections")]
    #[tokio::test]
    async fn iroh_reconnect_exhausts_lifetime_budget() {
        let mut attempts = SSH_RECONNECT_MAX_ATTEMPTS - 1;
        let recovered = retry_iroh_connection::<(), _, _, _, _>(
            &mut attempts,
            false,
            |_| std::future::ready(Err(io::Error::from(io::ErrorKind::UnexpectedEof).into())),
            |_| std::future::ready(()),
        )
        .await
        .expect("budget exhaustion is not terminal auth failure");
        assert_eq!(recovered, None);
        assert_eq!(attempts, SSH_RECONNECT_MAX_ATTEMPTS);
        let recovered = retry_iroh_connection::<(), _, _, _, _>(
            &mut attempts,
            false,
            |_| async { panic!("exhausted budget must not dial again") },
            |_| std::future::ready(()),
        )
        .await
        .expect("exhausted budget");
        assert_eq!(recovered, None);
    }

    #[cfg(feature = "bundled-plugin-connections")]
    #[tokio::test]
    async fn iroh_reconnect_forever_caps_backoff_and_saturates_attempts() {
        let mut attempts = usize::MAX;
        let mut calls = 0;
        let recovered = retry_iroh_connection(
            &mut attempts,
            true,
            |_| {
                calls += 1;
                std::future::ready(if calls < 3 {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused).into())
                } else {
                    Ok(())
                })
            },
            |delay| {
                assert_eq!(delay, Duration::from_millis(307_200));
                std::future::ready(())
            },
        )
        .await
        .expect("forever ignores finite attempt budget");
        assert!(recovered.is_some());
        assert_eq!(calls, 3);
        assert_eq!(attempts, usize::MAX);
    }

    #[cfg(feature = "bundled-plugin-connections")]
    #[tokio::test]
    async fn iroh_reconnect_forever_stops_on_auth_failure() {
        let mut attempts = 0;
        let error = retry_iroh_connection::<(), _, _, _, _>(
            &mut attempts,
            true,
            |_| std::future::ready(Err(anyhow::anyhow!("iroh SSH auth rejected: revoked"))),
            |_| std::future::ready(()),
        )
        .await
        .expect_err("authentication rejection must terminate even forever");
        assert!(error.to_string().contains("revoked"));
        assert_eq!(attempts, 1);
    }

    #[test]
    #[cfg(feature = "bundled-plugin-connections")]
    fn iroh_reconnect_classifies_typed_errors_not_messages() {
        for error in [
            anyhow::Error::from(iroh::endpoint::ConnectionError::TimedOut),
            anyhow::Error::from(iroh::endpoint::ConnectionError::Reset),
            anyhow::Error::from(bmux_client::ClientError::Timeout(Duration::from_secs(1))),
            anyhow::Error::from(bmux_client::ClientError::Transport(
                bmux_ipc::transport::IpcTransportError::Io(io::Error::from(
                    io::ErrorKind::UnexpectedEof,
                )),
            )),
        ] {
            assert!(iroh_connect_error_is_transient(
                &error.context("setup failed")
            ));
        }
        for error in [
            anyhow::anyhow!("timed out: unauthenticated peer"),
            anyhow::Error::from(io::Error::from(io::ErrorKind::ConnectionRefused))
                .context("iroh SSH auth handshake failed"),
            anyhow::Error::from(io::Error::from(io::ErrorKind::PermissionDenied)),
            anyhow::Error::from(iroh::endpoint::ConnectionError::VersionMismatch),
            anyhow::Error::from(iroh::endpoint::ConnectionError::LocallyClosed),
            anyhow::Error::from(bmux_client::ClientError::ServerError {
                code: bmux_ipc::ErrorCode::InvalidRequest,
                message: "revoked authority".to_string(),
            }),
        ] {
            assert!(!iroh_connect_error_is_transient(
                &error.context("setup failed")
            ));
        }
    }

    #[cfg(feature = "bundled-plugin-connections")]
    #[tokio::test]
    async fn iroh_reconnect_deadline_bounds_pending_setup() {
        // No network or live endpoint: a permanently pending online/setup phase
        // must be cancelled by the same deadline as all other connect stages.
        let error = iroh_connect_with_deadline::<()>(0, std::future::pending())
            .await
            .expect_err("pending setup must time out");
        assert!(iroh_connect_error_is_transient(&error));
        assert!(
            error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
        assert_eq!(
            iroh_connect_with_deadline(1, std::future::ready(Ok(7)))
                .await
                .expect("ready setup completes"),
            7
        );
    }

    #[cfg(feature = "bundled-plugin-connections")]
    #[test]
    fn reconnect_backoff_grows_exponentially() {
        assert_eq!(reconnect_backoff_ms(1), 300);
        assert_eq!(reconnect_backoff_ms(2), 600);
        assert_eq!(reconnect_backoff_ms(3), 1_200);
    }
}
