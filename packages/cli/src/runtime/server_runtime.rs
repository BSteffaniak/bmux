use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use super::{
    ConnectionContext, SERVER_POLL_INTERVAL, SERVER_STATUS_TIMEOUT, connect_raw_with_context,
    remove_server_runtime_metadata_file,
};

pub(super) async fn server_is_running(connection_context: ConnectionContext<'_>) -> Result<bool> {
    probe_server_running(connection_context).await
}

pub(super) async fn probe_server_running(
    connection_context: ConnectionContext<'_>,
) -> Result<bool> {
    Ok(fetch_server_status(connection_context)
        .await?
        .is_some_and(|status| status.running))
}

pub(super) async fn fetch_server_status(
    connection_context: ConnectionContext<'_>,
) -> Result<Option<bmux_client::ServerStatusInfo>> {
    let connect = tokio::time::timeout(
        SERVER_STATUS_TIMEOUT,
        connect_raw_with_context("bmux-cli-status", connection_context),
    )
    .await;

    match connect {
        Ok(Ok(mut client)) => {
            match tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.server_status()).await {
                Ok(Ok(status)) => Ok(Some(status)),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "server status request failed");
                    Ok(None)
                }
                Err(error) => {
                    tracing::debug!(%error, "server status request timed out");
                    Ok(None)
                }
            }
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "server status connection failed");
            Ok(None)
        }
        Err(error) => {
            tracing::debug!(%error, "server status connection timed out");
            Ok(None)
        }
    }
}

pub(super) async fn wait_for_server_running(
    timeout: Duration,
    connection_context: ConnectionContext<'_>,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let connect = tokio::time::timeout(
            SERVER_STATUS_TIMEOUT,
            connect_raw_with_context("bmux-cli-start-wait", connection_context),
        )
        .await;
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

pub(super) async fn wait_until_server_stopped(
    timeout: Duration,
    connection_context: ConnectionContext<'_>,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let reconnect = tokio::time::timeout(
            SERVER_STATUS_TIMEOUT,
            connect_raw_with_context("bmux-cli-stop-check", connection_context),
        )
        .await;
        if reconnect.is_err() || matches!(reconnect, Ok(Err(_))) {
            return Ok(true);
        }
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }

    Ok(false)
}

pub(super) async fn wait_for_process_exit_async(pid: u32, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !is_pid_running(pid)? || process_has_exited(pid)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + SERVER_POLL_INTERVAL))
            .await;
    }
}

// An exited foreground server can remain a zombie until its launching parent
// reaps it. It has released its resources even though kill -0 still succeeds.
fn process_has_exited(pid: u32) -> Result<bool> {
    #[cfg(unix)]
    {
        let output = ProcessCommand::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .context("failed inspecting server process state")?;
        Ok(output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .trim_start()
                .starts_with('Z'))
    }
    #[cfg(windows)]
    {
        let _ = pid;
        Ok(false)
    }
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

    parse_pid_content(&content).map_or_else(
        || {
            let _ = remove_server_pid_file();
            Ok(None)
        },
        |pid| Ok(Some(pid)),
    )
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

pub fn is_pid_running(pid: u32) -> Result<bool> {
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

    if !is_pid_running(pid)? && !probe_server_running(ConnectionContext::new(Some("local"))).await?
    {
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
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::runtime::attach::runtime::describe_timeout;
    use crate::runtime::server_commands::server_event_name;
    use bmux_config::ResolvedTimeout;

    #[test]
    fn describe_timeout_formats_resolved_timeout_states() {
        assert_eq!(describe_timeout(&ResolvedTimeout::Indefinite), "indefinite");
        assert_eq!(
            describe_timeout(&ResolvedTimeout::Exact(275)),
            "exact (275ms)"
        );
        assert_eq!(
            describe_timeout(&ResolvedTimeout::Profile {
                name: "traditional".to_string(),
                ms: 450,
            }),
            "profile:traditional (450ms)"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_exit_wait_does_not_report_a_live_process_as_stopped() {
        // No IPC endpoint exists for this child. IPC failure alone must not
        // authorize reporting a completed local shutdown.
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        assert!(
            !wait_for_process_exit_async(pid, Duration::from_millis(20))
                .await
                .unwrap()
        );
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        assert!(
            wait_for_process_exit_async(pid, Duration::from_secs(1))
                .await
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_exit_wait_accepts_an_unreaped_child() {
        let mut child = ProcessCommand::new("/bin/sleep")
            .arg("0.1")
            .spawn()
            .unwrap();
        // Do not reap until after the wait, matching a foreground server whose
        // parent is synchronously waiting for the stop command to return.
        let exited = wait_for_process_exit_async(child.id(), Duration::from_secs(5)).await;
        let status = child.wait().unwrap();
        assert!(status.success());
        assert!(exited.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_exit_wait_waits_for_delayed_exit() {
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("0.2")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let (exited, status) = tokio::join!(
            wait_for_process_exit_async(pid, Duration::from_secs(5)),
            child.wait(),
        );
        assert!(status.unwrap().success());
        assert!(exited.unwrap());
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
            server_event_name(&bmux_client::ServerEvent::ServerStarted),
            "server_started"
        );
        assert_eq!(
            server_event_name(&bmux_client::ServerEvent::ServerStopping),
            "server_stopping"
        );
    }
}
