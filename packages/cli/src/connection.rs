use anyhow::{Context, Result};
use bmux_client::{BmuxClient, ClientError};
use bmux_config::{BmuxConfig, ConnectionTargetConfig, ConnectionTransport, StaleBuildAction};
use bmux_ipc::transport::ErasedIpcStream;
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ServerRuntimeMetadata {
    pub(crate) pid: u32,
    pub(crate) version: String,
    pub(crate) build_id: String,
    pub(crate) executable_path: String,
    pub(crate) started_at_epoch_ms: u64,
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

pub async fn connect(
    scope: ConnectionPolicyScope,
    client_name: &'static str,
) -> Result<BmuxClient> {
    apply_stale_build_policy(scope)?;
    connect_for_active_target(client_name).await
}

pub async fn connect_if_running(
    scope: ConnectionPolicyScope,
    client_name: &'static str,
) -> Result<Option<BmuxClient>> {
    apply_stale_build_policy(scope)?;
    match connect_for_active_target(client_name).await {
        Ok(client) => Ok(Some(client)),
        Err(error)
            if error
                .to_string()
                .contains("server is not running (IPC endpoint unavailable)")
                || error.to_string().contains("TLS target unreachable") =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub async fn connect_raw(client_name: &'static str) -> Result<BmuxClient> {
    connect_for_active_target(client_name).await
}

#[derive(Debug, Clone)]
enum ActiveTarget {
    Local,
    Tls(TlsTarget),
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

async fn connect_for_active_target(client_name: &'static str) -> Result<BmuxClient> {
    match resolve_active_target()? {
        ActiveTarget::Local => BmuxClient::connect_default(client_name)
            .await
            .map_err(map_client_connect_error),
        ActiveTarget::Tls(target) => connect_tls_target(&target, client_name).await,
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

fn resolve_active_target() -> Result<ActiveTarget> {
    let config = BmuxConfig::load().context("failed loading bmux config")?;
    let selected = std::env::var("BMUX_TARGET")
        .ok()
        .or_else(|| config.connections.default_target.clone());
    let Some(target) = selected else {
        return Ok(ActiveTarget::Local);
    };
    resolve_target_reference(&config, target.trim())
}

fn resolve_target_reference(config: &BmuxConfig, target: &str) -> Result<ActiveTarget> {
    if target.is_empty() || target == "local" {
        return Ok(ActiveTarget::Local);
    }
    if let Some(named) = config.connections.targets.get(target) {
        return resolve_named_target(target, named);
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
                "SSH targets require CLI target proxying; run command with --target {}",
                name
            )
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

    if let ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(io_error)) = &error {
        if io_error.kind() == std::io::ErrorKind::UnexpectedEof {
            return anyhow::anyhow!(
                "bmux error: server connection lost unexpectedly.\nThe server may have crashed or been stopped. Check `bmux server status` and server logs."
            );
        }
    }

    anyhow::Error::from(error)
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
        evaluate_stale_build_policy, is_server_unavailable_client_error, map_client_connect_error,
    };
    use bmux_client::ClientError;
    use bmux_config::StaleBuildAction;
    use bmux_ipc::frame::FrameDecodeError;
    use bmux_ipc::transport::IpcTransportError;

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
}
