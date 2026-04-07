use crate::ssh_access::{
    authenticate_client_connection, parse_iroh_target as parse_iroh_target_parts,
};
use anyhow::{Context, Result};
use bmux_client::{BmuxClient, ClientError};
use bmux_config::{
    BmuxConfig, ConfigPaths, ConnectionTargetConfig, ConnectionTransport, StaleBuildAction,
};
use bmux_ipc::IncompatibilityReason;
use bmux_ipc::transport::ErasedIpcStream;
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::presets};
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const DEFAULT_CONTROL_PLANE_URL: &str = "https://api.bmux.run";

#[derive(Debug, Clone, serde::Deserialize)]
struct AuthState {
    access_token: String,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ServerRuntimeMetadata {
    pub pid: u32,
    pub version: String,
    pub build_id: String,
    pub executable_path: String,
    pub started_at_epoch_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPolicyScope {
    Normal,
    RecoveryInspection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerBuildPolicyEffect {
    Warn(String),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectionContext<'a> {
    pub target_override: Option<&'a str>,
}

impl<'a> ConnectionContext<'a> {
    pub const fn new(target_override: Option<&'a str>) -> Self {
        Self { target_override }
    }
}

pub async fn connect(
    scope: ConnectionPolicyScope,
    client_name: &'static str,
) -> Result<BmuxClient> {
    connect_with_context(scope, client_name, ConnectionContext::default()).await
}

pub async fn connect_with_context(
    scope: ConnectionPolicyScope,
    client_name: &'static str,
    context: ConnectionContext<'_>,
) -> Result<BmuxClient> {
    apply_stale_build_policy(scope)?;
    connect_for_active_target(client_name, context).await
}

pub async fn connect_if_running_with_context(
    scope: ConnectionPolicyScope,
    client_name: &'static str,
    context: ConnectionContext<'_>,
) -> Result<Option<BmuxClient>> {
    apply_stale_build_policy(scope)?;
    match connect_for_active_target(client_name, context).await {
        Ok(client) => Ok(Some(client)),
        Err(error)
            if error
                .to_string()
                .contains("server is not running (IPC endpoint unavailable)")
                || error.to_string().contains("TLS target unreachable")
                || error.to_string().contains("iroh target unreachable") =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub async fn connect_raw(client_name: &'static str) -> Result<BmuxClient> {
    connect_raw_with_context(client_name, ConnectionContext::default()).await
}

pub async fn connect_raw_with_context(
    client_name: &'static str,
    context: ConnectionContext<'_>,
) -> Result<BmuxClient> {
    connect_for_active_target(client_name, context).await
}

#[derive(Debug, Clone)]
enum ActiveTarget {
    Local,
    Tls(TlsTarget),
    Iroh(IrohTarget),
}

#[derive(Debug, Clone)]
struct TlsTarget {
    label: String,
    host: String,
    port: u16,
    server_name: String,
    ca_file: Option<std::path::PathBuf>,
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

const BMUX_IROH_ALPN: &[u8] = b"bmux/gateway/iroh/1";

async fn connect_for_active_target(
    client_name: &'static str,
    context: ConnectionContext<'_>,
) -> Result<BmuxClient> {
    match resolve_active_target(context).await? {
        ActiveTarget::Local => BmuxClient::connect_default(client_name)
            .await
            .map_err(map_client_connect_error),
        ActiveTarget::Tls(target) => connect_tls_target(&target, client_name).await,
        ActiveTarget::Iroh(target) => connect_iroh_target(&target, client_name).await,
    }
}

async fn connect_tls_target(target: &TlsTarget, client_name: &'static str) -> Result<BmuxClient> {
    let connector = build_tls_connector(target)?;
    let address = format!("{}:{}", target.host, target.port);
    let connect_future = TcpStream::connect(&address);
    let tcp_stream = tokio::time::timeout(
        std::time::Duration::from_millis(target.connect_timeout_ms.max(1)),
        connect_future,
    )
    .await
    .with_context(|| format!("TLS target '{}' connect timed out", target.label))?
    .with_context(|| format!("TLS target unreachable: {}", target.label))?;
    let server_name = ServerName::try_from(target.server_name.clone())
        .map_err(|_| anyhow::anyhow!("invalid TLS server_name '{}'", target.server_name))?;
    let tls_stream = connector
        .connect(server_name, tcp_stream)
        .await
        .with_context(|| format!("TLS handshake failed for target '{}'", target.label))?;
    let principal_id = load_or_create_local_principal_id()?;
    BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(tls_stream)),
        std::time::Duration::from_millis(target.connect_timeout_ms.max(1)),
        client_name.to_string(),
        principal_id,
    )
    .await
    .map_err(map_client_connect_error)
}

async fn connect_iroh_target(target: &IrohTarget, client_name: &'static str) -> Result<BmuxClient> {
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
        std::time::Duration::from_millis(target.connect_timeout_ms.max(1)),
        endpoint.connect(remote_addr, BMUX_IROH_ALPN),
    )
    .await
    .with_context(|| format!("iroh target '{}' connect timed out", target.label))?
    .with_context(|| format!("iroh target unreachable: {}", target.label))?;

    if target.require_ssh_auth {
        authenticate_client_connection(&connection)
            .await
            .context("iroh SSH auth handshake failed")?;
    }

    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("failed opening iroh stream")?;
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge_stream);
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut recv, &mut bridge_write).await;
        let _ = bridge_write.shutdown().await;
    });
    tokio::spawn(async move {
        let _endpoint_keepalive = endpoint;
        let _connection_keepalive = connection;
        let _ = tokio::io::copy(&mut bridge_read, &mut send).await;
        let _ = send.finish();
    });
    let principal_id = load_or_create_local_principal_id()?;
    BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(client_stream)),
        std::time::Duration::from_millis(target.connect_timeout_ms.max(1)),
        client_name.to_string(),
        principal_id,
    )
    .await
    .map_err(map_client_connect_error)
}

fn build_tls_connector(target: &TlsTarget) -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
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

async fn resolve_active_target(context: ConnectionContext<'_>) -> Result<ActiveTarget> {
    let config = BmuxConfig::load().context("failed loading bmux config")?;
    let selected = context
        .target_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| std::env::var("BMUX_TARGET").ok())
        .or_else(|| config.connections.default_target.clone());
    let Some(target) = selected else {
        return Ok(ActiveTarget::Local);
    };
    let expanded = expand_bmux_target_if_needed(&config, target.trim()).await?;
    resolve_target_reference(&config, &expanded)
}

pub async fn expand_bmux_target_if_needed(config: &BmuxConfig, target: &str) -> Result<String> {
    let Some(name) = target.strip_prefix("bmux://") else {
        return Ok(target.to_string());
    };
    if let Some(mapped) = config.connections.share_links.get(name) {
        return Ok(mapped.clone());
    }
    let Some(auth_state) = load_auth_state_optional(&ConfigPaths::default())? else {
        return Ok(target.to_string());
    };
    let control_plane = control_plane_url(config);
    let client = reqwest::Client::new();
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

fn control_plane_url(config: &BmuxConfig) -> String {
    std::env::var("BMUX_CONTROL_PLANE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.connections.control_plane_url.clone())
        .unwrap_or_else(|| DEFAULT_CONTROL_PLANE_URL.to_string())
}

fn load_auth_state_optional(paths: &ConfigPaths) -> Result<Option<AuthState>> {
    let path = paths.runtime_dir.join("auth-state.json");
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

fn resolve_target_reference(config: &BmuxConfig, target: &str) -> Result<ActiveTarget> {
    if target.is_empty() || target == "local" {
        return Ok(ActiveTarget::Local);
    }
    if let Some(name) = target.strip_prefix("bmux://") {
        let mapped = config.connections.share_links.get(name).ok_or_else(|| {
            anyhow::anyhow!("share link not found: bmux://{name}; run 'bmux share' or 'bmux hosts'")
        })?;
        return resolve_target_reference(config, mapped);
    }
    if let Some(named) = config.connections.targets.get(target) {
        return resolve_named_target(target, named);
    }
    if target.starts_with("https://") {
        return parse_https_target(target);
    }
    if target.starts_with("iroh://") {
        return parse_iroh_target(target);
    }
    if target.starts_with("tls://") {
        return parse_inline_tls_target(target);
    }
    Ok(ActiveTarget::Local)
}

fn resolve_named_target(name: &str, target: &ConnectionTargetConfig) -> Result<ActiveTarget> {
    match target.transport {
        ConnectionTransport::Local => Ok(ActiveTarget::Local),
        ConnectionTransport::Tls => {
            let host = target
                .host
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("TLS target '{name}' requires host"))?
                .to_string();
            let port = target.port.unwrap_or(443);
            let server_name = target.server_name.clone().unwrap_or_else(|| host.clone());
            Ok(ActiveTarget::Tls(TlsTarget {
                label: name.to_string(),
                host,
                port,
                server_name,
                ca_file: target.ca_file.clone(),
                connect_timeout_ms: target.connect_timeout_ms.max(1),
            }))
        }
        ConnectionTransport::Ssh => {
            anyhow::bail!(
                "SSH targets require CLI target proxying; run command with --target {name}"
            )
        }
        ConnectionTransport::Iroh => {
            let endpoint_id = target
                .endpoint_id
                .as_deref()
                .or(target.host.as_deref())
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("iroh target '{name}' requires endpoint_id"))?
                .to_string();
            Ok(ActiveTarget::Iroh(IrohTarget {
                label: name.to_string(),
                endpoint_id,
                relay_url: target.relay_url.clone(),
                require_ssh_auth: target.iroh_ssh_auth,
                connect_timeout_ms: target.connect_timeout_ms.max(1),
            }))
        }
    }
}

fn parse_inline_tls_target(target: &str) -> Result<ActiveTarget> {
    let raw = target
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
    Ok(ActiveTarget::Tls(TlsTarget {
        label: target.to_string(),
        host: host.clone(),
        port,
        server_name: host,
        ca_file: None,
        connect_timeout_ms: 8_000,
    }))
}

fn parse_https_target(target: &str) -> Result<ActiveTarget> {
    let raw = target
        .strip_prefix("https://")
        .ok_or_else(|| anyhow::anyhow!("hosted target must start with https://"))?;
    let host = raw.split('/').next().unwrap_or_default();
    let (host, port) = if let Some((host, port_raw)) = host.rsplit_once(':') {
        if port_raw.is_empty() {
            (host.to_string(), 443)
        } else {
            let parsed = port_raw
                .parse::<u16>()
                .with_context(|| format!("invalid TLS port in target '{target}'"))?;
            (host.to_string(), parsed)
        }
    } else {
        (host.to_string(), 443)
    };
    if host.trim().is_empty() {
        anyhow::bail!("hosted target must include a host");
    }
    Ok(ActiveTarget::Tls(TlsTarget {
        label: target.to_string(),
        host: host.clone(),
        port,
        server_name: host,
        ca_file: None,
        connect_timeout_ms: 8_000,
    }))
}

fn parse_iroh_target(target: &str) -> Result<ActiveTarget> {
    let parsed = parse_iroh_target_parts(target)?;
    Ok(ActiveTarget::Iroh(IrohTarget {
        label: target.to_string(),
        endpoint_id: parsed.endpoint_id,
        relay_url: parsed.relay_url,
        require_ssh_auth: parsed.require_ssh_auth,
        connect_timeout_ms: 8_000,
    }))
}

fn load_or_create_local_principal_id() -> Result<uuid::Uuid> {
    let path = bmux_config::ConfigPaths::default().principal_id_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating principal id dir {}", parent.display()))?;
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let raw = content.trim();
            uuid::Uuid::parse_str(raw)
                .with_context(|| format!("invalid principal id in {}: {raw}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let principal_id = uuid::Uuid::new_v4();
            std::fs::write(&path, principal_id.to_string())
                .with_context(|| format!("failed writing principal id file {}", path.display()))?;
            Ok(principal_id)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed reading principal id file {}", path.display())),
    }
}

pub fn is_server_unavailable_client_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(io_error))
            if matches!(
                io_error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
    )
}

pub fn map_client_connect_error(error: ClientError) -> anyhow::Error {
    if let ClientError::ProtocolIncompatible { reason } = &error {
        return anyhow::anyhow!(format_protocol_incompatibility(reason));
    }

    if let ClientError::ServerError { code, .. } = &error
        && *code == bmux_ipc::ErrorCode::VersionMismatch
    {
        return anyhow::anyhow!(
            "bmux error: client/server protocol mismatch. Restart with `bmux server stop` and retry."
        );
    }

    if let ClientError::Transport(bmux_ipc::transport::IpcTransportError::FrameDecode(
        bmux_ipc::frame::FrameDecodeError::UnsupportedVersion { .. },
    )) = &error
    {
        return anyhow::anyhow!(
            "bmux error: client/server protocol mismatch. Restart with `bmux server stop` and retry."
        );
    }

    if is_server_unavailable_client_error(&error) {
        return anyhow::anyhow!(
            "bmux server is not running (IPC endpoint unavailable).\nRun `bmux server start --daemon`.\nTroubleshooting: if the server is running in another shell, ensure both processes use the same runtime directory (`XDG_RUNTIME_DIR`/`TMPDIR`). On Unix, a stale socket file can also cause connection refused; remove stale runtime files or run `bmux server stop` and retry."
        );
    }

    if let ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(io_error)) = &error
        && io_error.kind() == std::io::ErrorKind::UnexpectedEof
    {
        let paths = ConfigPaths::default();
        let runtime = std::env::var("BMUX_RUNTIME_NAME")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "default".to_string());
        return anyhow::anyhow!(
            "bmux error: server connection lost unexpectedly.\nThe server may have crashed or been stopped, or a hosted bridge could not reach its local IPC endpoint.\nLocal CLI runtime: {runtime}\nLocal CLI IPC endpoint: {}\nCheck `bmux --runtime {runtime} server status` on this machine and host logs on the remote machine.",
            local_ipc_endpoint_label_for_error(&paths)
        );
    }

    anyhow::Error::from(error)
}

fn local_ipc_endpoint_label_for_error(paths: &ConfigPaths) -> String {
    #[cfg(unix)]
    {
        paths.server_socket().display().to_string()
    }
    #[cfg(windows)]
    {
        paths.server_named_pipe()
    }
}

fn format_protocol_incompatibility(reason: &IncompatibilityReason) -> String {
    match reason {
        IncompatibilityReason::WireEpochMismatch { client, server } => {
            format!(
                "bmux error: incompatible IPC wire epoch (client={client}, server={server}). Restart or update the server so both sides share a wire epoch."
            )
        }
        IncompatibilityReason::NoCommonRevision {
            client_min,
            client_max,
            server_min,
            server_max,
        } => format!(
            "bmux error: no overlapping protocol revision (client={client_min}-{client_max}, server={server_min}-{server_max}). Update either side to overlapping revisions."
        ),
        IncompatibilityReason::MissingCoreCapabilities { missing } => format!(
            "bmux error: missing shared core protocol capabilities: {}. Update server/client so core contracts align.",
            missing.join(", ")
        ),
    }
}

pub fn apply_stale_build_policy(scope: ConnectionPolicyScope) -> Result<()> {
    let config = BmuxConfig::load().context("failed loading bmux config")?;
    match evaluate_stale_build_policy(
        scope,
        config.behavior.stale_build_action,
        read_server_runtime_metadata().ok().flatten(),
        current_cli_build_id().ok(),
    )? {
        Some(ServerBuildPolicyEffect::Warn(message)) => eprintln!("{message}"),
        None => {}
    }
    Ok(())
}

pub fn evaluate_stale_build_policy(
    scope: ConnectionPolicyScope,
    action: StaleBuildAction,
    metadata: Option<ServerRuntimeMetadata>,
    current_build_id: Option<String>,
) -> Result<Option<ServerBuildPolicyEffect>> {
    if scope == ConnectionPolicyScope::RecoveryInspection {
        return Ok(None);
    }
    let (Some(metadata), Some(current_build_id)) = (metadata, current_build_id) else {
        return Ok(None);
    };
    if metadata.build_id == current_build_id {
        return Ok(None);
    }

    let message = format_stale_build_message(&metadata, &current_build_id, action);
    match action {
        StaleBuildAction::Error => anyhow::bail!(message),
        StaleBuildAction::Warn => Ok(Some(ServerBuildPolicyEffect::Warn(message))),
    }
}

fn format_stale_build_message(
    metadata: &ServerRuntimeMetadata,
    current_build_id: &str,
    action: StaleBuildAction,
) -> String {
    format!(
        "bmux {}: running server build differs from current CLI build.\nserver build: {} ({})\ncli build: {}\nRestart with `bmux server stop` and retry.",
        match action {
            StaleBuildAction::Error => "error",
            StaleBuildAction::Warn => "warning",
        },
        metadata.build_id,
        metadata.executable_path,
        current_build_id,
    )
}

fn server_runtime_metadata_file_path() -> std::path::PathBuf {
    let paths = bmux_config::ConfigPaths::default();
    paths.runtime_dir.join("server-meta.json")
}

pub fn current_cli_build_id() -> Result<String> {
    let executable = std::env::current_exe().context("failed resolving current executable")?;
    let metadata = std::fs::metadata(&executable).with_context(|| {
        format!(
            "failed reading executable metadata {}",
            executable.display()
        )
    })?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0_u128, |duration| duration.as_millis());
    Ok(format!("{}-{modified}", metadata.len()))
}

#[allow(clippy::cast_possible_truncation)] // Epoch millis won't exceed u64 for billions of years
fn current_server_runtime_metadata(pid: u32) -> Result<ServerRuntimeMetadata> {
    let executable = std::env::current_exe().context("failed resolving current executable")?;
    Ok(ServerRuntimeMetadata {
        pid,
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: current_cli_build_id()?,
        executable_path: executable.display().to_string(),
        started_at_epoch_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64),
    })
}

pub fn write_server_runtime_metadata(pid: u32) -> Result<()> {
    let path = server_runtime_metadata_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating runtime dir {}", parent.display()))?;
    }
    let metadata = current_server_runtime_metadata(pid)?;
    let payload =
        serde_json::to_vec_pretty(&metadata).context("failed encoding server metadata")?;
    std::fs::write(&path, payload)
        .with_context(|| format!("failed writing server metadata file {}", path.display()))
}

pub fn read_server_runtime_metadata() -> Result<Option<ServerRuntimeMetadata>> {
    let path = server_runtime_metadata_file_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed reading server metadata file {}", path.display())
            });
        }
    };
    let metadata = serde_json::from_slice::<ServerRuntimeMetadata>(&bytes).with_context(|| {
        format!(
            "failed parsing server metadata file {}; remove stale file and retry",
            path.display()
        )
    })?;
    Ok(Some(metadata))
}

pub fn remove_server_runtime_metadata_file() -> Result<()> {
    let path = server_runtime_metadata_file_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed removing server metadata file {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConnectionPolicyScope, ServerBuildPolicyEffect, ServerRuntimeMetadata,
        evaluate_stale_build_policy, expand_bmux_target_if_needed,
        is_server_unavailable_client_error, map_client_connect_error,
    };
    use bmux_client::ClientError;
    use bmux_config::{BmuxConfig, StaleBuildAction};
    use bmux_ipc::IncompatibilityReason;
    use bmux_ipc::frame::FrameDecodeError;
    use bmux_ipc::transport::IpcTransportError;
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
                "bmux-connection-tests-{label}-{}",
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
    fn stale_build_policy_blocks_normal_commands_by_default() {
        let error = evaluate_stale_build_policy(
            ConnectionPolicyScope::Normal,
            StaleBuildAction::Error,
            Some(ServerRuntimeMetadata {
                pid: 1,
                version: "0.0.1-alpha.0".to_string(),
                build_id: "server-build".to_string(),
                executable_path: "/tmp/bmux-server".to_string(),
                started_at_epoch_ms: 0,
            }),
            Some("cli-build".to_string()),
        )
        .expect_err("expected stale build policy to block");

        let message = error.to_string();
        assert!(message.contains("running server build differs from current CLI build"));
        assert!(message.contains("bmux server stop"));
    }

    #[test]
    fn stale_build_policy_warns_when_configured() {
        let effect = evaluate_stale_build_policy(
            ConnectionPolicyScope::Normal,
            StaleBuildAction::Warn,
            Some(ServerRuntimeMetadata {
                pid: 1,
                version: "0.0.1-alpha.0".to_string(),
                build_id: "server-build".to_string(),
                executable_path: "/tmp/bmux-server".to_string(),
                started_at_epoch_ms: 0,
            }),
            Some("cli-build".to_string()),
        )
        .expect("warn mode should continue");

        assert!(matches!(
            effect,
            Some(ServerBuildPolicyEffect::Warn(message))
                if message.contains("bmux warning") && message.contains("bmux server stop")
        ));
    }

    #[test]
    fn stale_build_policy_skips_recovery_inspection_commands() {
        let effect = evaluate_stale_build_policy(
            ConnectionPolicyScope::RecoveryInspection,
            StaleBuildAction::Error,
            Some(ServerRuntimeMetadata {
                pid: 1,
                version: "0.0.1-alpha.0".to_string(),
                build_id: "server-build".to_string(),
                executable_path: "/tmp/bmux-server".to_string(),
                started_at_epoch_ms: 0,
            }),
            Some("cli-build".to_string()),
        )
        .expect("recovery commands should bypass stale build policy");

        assert!(effect.is_none());
    }

    #[test]
    fn map_client_connect_error_rewrites_protocol_mismatch() {
        let error = map_client_connect_error(ClientError::Transport(
            IpcTransportError::FrameDecode(FrameDecodeError::UnsupportedVersion {
                actual: 1,
                expected: 3,
            }),
        ));

        let message = error.to_string();
        assert!(message.contains("client/server protocol mismatch"));
        assert!(message.contains("bmux server stop"));
    }

    #[test]
    fn map_client_connect_error_formats_transport_not_found() {
        let error = map_client_connect_error(ClientError::Transport(IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )));
        let message = error.to_string();

        assert!(message.contains("bmux server is not running"));
        assert!(message.contains("bmux server start --daemon"));
        assert!(message.contains("XDG_RUNTIME_DIR"));
        assert!(message.contains("TMPDIR"));
    }

    #[test]
    fn map_client_connect_error_formats_transport_connection_refused() {
        let error = map_client_connect_error(ClientError::Transport(IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        )));
        let message = error.to_string();

        assert!(message.contains("bmux server is not running"));
        assert!(message.contains("bmux server start --daemon"));
        assert!(message.contains("stale socket"));
    }

    #[test]
    fn map_client_connect_error_formats_protocol_incompatible_reason() {
        let error = map_client_connect_error(ClientError::ProtocolIncompatible {
            reason: IncompatibilityReason::NoCommonRevision {
                client_min: 3,
                client_max: 5,
                server_min: 1,
                server_max: 2,
            },
        });

        let message = error.to_string();
        assert!(message.contains("no overlapping protocol revision"));
        assert!(message.contains("client=3-5"));
        assert!(message.contains("server=1-2"));
    }

    #[test]
    fn server_unavailable_helper_matches_not_found_and_connection_refused() {
        let not_found = ClientError::Transport(IpcTransportError::Io(std::io::Error::from(
            std::io::ErrorKind::NotFound,
        )));
        assert!(is_server_unavailable_client_error(&not_found));

        let refused = ClientError::Transport(IpcTransportError::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused,
        )));
        assert!(is_server_unavailable_client_error(&refused));

        let denied = ClientError::Transport(IpcTransportError::Io(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));
        assert!(!is_server_unavailable_client_error(&denied));
    }

    #[test]
    fn runtime_command_handlers_do_not_bypass_connection_module() {
        let runtime_source = include_str!("runtime/mod.rs");
        assert!(!runtime_source.contains("BmuxClient::connect_default"));
    }

    #[tokio::test]
    async fn expand_bmux_target_prefers_local_share_link_map() {
        let mut config = BmuxConfig::default();
        config
            .connections
            .share_links
            .insert("demo".to_string(), "iroh://local-endpoint".to_string());

        let resolved = expand_bmux_target_if_needed(&config, "bmux://demo")
            .await
            .expect("expand target");

        assert_eq!(resolved, "iroh://local-endpoint");
    }

    #[tokio::test]
    #[serial]
    async fn expand_bmux_target_without_auth_preserves_bmux_link() {
        let runtime_dir = TempDirGuard::new("no-auth");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());
        let _control_plane_guard = EnvVarGuard::set("BMUX_CONTROL_PLANE_URL", "http://127.0.0.1:9");

        let config = BmuxConfig::default();
        let resolved = expand_bmux_target_if_needed(&config, "bmux://demo")
            .await
            .expect("expand target");

        assert_eq!(resolved, "bmux://demo");
    }

    #[tokio::test]
    #[serial]
    async fn expand_bmux_target_uses_control_plane_lookup() {
        let runtime_dir = TempDirGuard::new("control-plane");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", runtime_dir.path());

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

            let body = r#"{"target":"iroh://remote-endpoint"}"#;
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

        let config = BmuxConfig::default();
        let resolved = expand_bmux_target_if_needed(&config, "bmux://demo")
            .await
            .expect("expand target");
        assert_eq!(resolved, "iroh://remote-endpoint");

        let request = request_rx.await.expect("capture request");
        assert!(request.contains("GET /v1/share-links/demo HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer token-123")
        );
    }
}
