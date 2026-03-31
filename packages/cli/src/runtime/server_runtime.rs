use super::*;

pub(super) async fn server_is_running() -> Result<bool> {
    probe_server_running().await
}

pub(super) async fn probe_server_running() -> Result<bool> {
    Ok(fetch_server_status()
        .await?
        .is_some_and(|status| status.running))
}

pub(super) async fn fetch_server_status() -> Result<Option<bmux_client::ServerStatusInfo>> {
    let connect = tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-status")).await;

    let mut client = match connect {
        Ok(Ok(client)) => client,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    match tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.server_status()).await {
        Ok(Ok(status)) => Ok(Some(status)),
        Ok(Err(_)) | Err(_) => Ok(None),
    }
}

pub(super) async fn wait_for_server_running(timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let connect =
            tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-start-wait")).await;
        if let Ok(Ok(mut client)) = connect
            && let Ok(Ok(status)) =
                tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.server_status()).await
            && status.running
        {
            return Ok(true);
        }
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }
    Ok(false)
}

pub(super) async fn wait_until_server_stopped(timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let reconnect =
            tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-stop-check")).await;
        if reconnect.is_err() || matches!(reconnect, Ok(Err(_))) {
            return Ok(true);
        }
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }

    Ok(false)
}

pub(super) fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_pid_running(pid)? {
            return Ok(true);
        }
        std::thread::sleep(SERVER_POLL_INTERVAL);
    }
    Ok(!is_pid_running(pid)?)
}

pub(super) fn server_pid_file_path() -> PathBuf {
    bmux_config::ConfigPaths::default().server_pid_file()
}

pub(super) fn write_server_pid_file(pid: u32) -> Result<()> {
    let path = server_pid_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating runtime dir {}", parent.display()))?;
    }
    std::fs::write(&path, pid.to_string())
        .with_context(|| format!("failed writing pid file {}", path.display()))
}

pub(super) fn read_server_pid_file() -> Result<Option<u32>> {
    let path = server_pid_file_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed reading pid file {}", path.display()));
        }
    };

    if let Some(pid) = parse_pid_content(&content) {
        Ok(Some(pid))
    } else {
        let _ = remove_server_pid_file();
        Ok(None)
    }
}

pub(super) fn remove_server_pid_file() -> Result<()> {
    let path = server_pid_file_path();
    let remove_pid_result = match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed removing pid file {}", path.display()))
        }
    };
    let remove_metadata_result = remove_server_runtime_metadata_file();
    remove_pid_result.and(remove_metadata_result)
}

pub(super) fn try_kill_pid(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        let status = ProcessCommand::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .context("failed to execute kill command")?;
        Ok(status.success())
    }

    #[cfg(windows)]
    {
        let status = ProcessCommand::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .status()
            .context("failed to execute taskkill command")?;
        return Ok(status.success());
    }
}

pub(super) fn is_pid_running(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        let status = ProcessCommand::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .context("failed to execute kill -0 command")?;
        Ok(status.success())
    }

    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        let output = ProcessCommand::new("tasklist")
            .arg("/FI")
            .arg(filter)
            .output()
            .context("failed to execute tasklist command")?;
        if !output.status.success() {
            return Ok(false);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(stdout.lines().any(|line| line.contains(&pid.to_string())));
    }
}

pub(super) async fn cleanup_stale_pid_file() -> Result<()> {
    let Some(pid) = read_server_pid_file()? else {
        return Ok(());
    };

    if !is_pid_running(pid)? && !probe_server_running().await? {
        remove_server_pid_file()?;
    }

    Ok(())
}

pub(super) fn parse_pid_content(content: &str) -> Option<u32> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u32>().ok().filter(|pid| *pid > 0)
}
#[cfg(test)]
mod tests {
    use crate::input::InputProcessor;
    use crate::runtime::attach::state::AttachViewState;
    use crate::runtime::*;
    use bmux_cli_schema::{Cli, Command};
    use bmux_client::{AttachLayoutState, AttachOpenInfo, ClientError};
    use bmux_config::{BmuxConfig, ConfigPaths, ResolvedTimeout};
    use bmux_ipc::transport::IpcTransportError;
    use bmux_ipc::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
        AttachViewComponent, ErrorCode, PaneLayoutNode, PaneSummary, RecordingSummary,
        SessionSummary,
    };
    use bmux_plugin::{PluginManifest, PluginRegistry};
    use bmux_plugin_sdk::PluginCommandEffect;
    use crossterm::event::{
        Event as CrosstermEvent, KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
        KeyEventKind as CrosstermKeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    #[test]
    fn describe_timeout_formats_resolved_timeout_states() {
        assert_eq!(
            crate::runtime::describe_timeout(&ResolvedTimeout::Indefinite),
            "indefinite"
        );
        assert_eq!(
            crate::runtime::describe_timeout(&ResolvedTimeout::Exact(275)),
            "exact (275ms)"
        );
        assert_eq!(
            crate::runtime::describe_timeout(&ResolvedTimeout::Profile {
                name: "traditional".to_string(),
                ms: 450,
            }),
            "profile:traditional (450ms)"
        );
    }

    #[test]
    fn parse_pid_content_accepts_positive_pid() {
        assert_eq!(parse_pid_content("123\n"), Some(123));
    }

    #[test]
    fn parse_pid_content_rejects_invalid_values() {
        assert_eq!(parse_pid_content(""), None);
        assert_eq!(parse_pid_content("0"), None);
        assert_eq!(parse_pid_content("abc"), None);
    }

    #[test]
    fn server_event_name_maps_known_variants() {
        assert_eq!(
            crate::runtime::server_event_name(&bmux_client::ServerEvent::ServerStarted),
            "server_started"
        );
        assert_eq!(
            crate::runtime::server_event_name(&bmux_client::ServerEvent::ClientDetached {
                id: uuid::Uuid::new_v4()
            }),
            "client_detached"
        );
    }
}
