use anyhow::{Context, Result};
use bmux_client::{BmuxClient, ClientError};
use bmux_config::{BmuxConfig, StaleBuildAction};
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
    BmuxClient::connect_default(client_name)
        .await
        .map_err(map_client_connect_error)
}

pub async fn connect_if_running(
    scope: ConnectionPolicyScope,
    client_name: &'static str,
) -> Result<Option<BmuxClient>> {
    apply_stale_build_policy(scope)?;
    match BmuxClient::connect_default(client_name).await {
        Ok(client) => Ok(Some(client)),
        Err(error) if is_server_unavailable_client_error(&error) => Ok(None),
        Err(error) => Err(map_client_connect_error(error)),
    }
}

pub async fn connect_raw(client_name: &'static str) -> Result<BmuxClient> {
    BmuxClient::connect_default(client_name)
        .await
        .map_err(map_client_connect_error)
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
