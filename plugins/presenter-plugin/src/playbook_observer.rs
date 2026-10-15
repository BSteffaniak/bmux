//! Owned observer subprocess. The only visual transport is the retained wire.
use crate::playbook_capture::{CaptureError, CaptureSnapshot, read_capture};

#[derive(Debug)]
pub struct ObserverFailure {
    pub closed: bool,
    pub message: String,
}
impl From<String> for ObserverFailure {
    fn from(message: String) -> Self {
        Self {
            closed: false,
            message,
        }
    }
}
impl From<&str> for ObserverFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl From<CaptureError> for ObserverFailure {
    fn from(error: CaptureError) -> Self {
        Self {
            closed: matches!(error, CaptureError::Closed),
            message: error.to_string(),
        }
    }
}
impl std::fmt::Display for ObserverFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
use bmux_playbook_plugin_api::playbook_types::Attempt;
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::sync::watch;

#[cfg(unix)]
pub async fn observe(
    executable: &Path,
    owner: String,
    attempt: &Attempt,
    sender: watch::Sender<Option<Arc<CaptureSnapshot>>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ObserverFailure> {
    use std::os::unix::fs::PermissionsExt;
    let session = attempt
        .session_id
        .as_ref()
        .ok_or("attempt has no attachment target")?
        .parse::<uuid::Uuid>()
        .map_err(|error| error.to_string())?;
    let directory = std::env::temp_dir().join(format!("bmux-observer-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).map_err(|error| error.to_string())?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let socket = directory.join("display.sock");
    let listener = tokio::net::UnixListener::bind(&socket).map_err(|error| error.to_string())?;
    let mut command = tokio::process::Command::new(executable);
    command.env_clear();
    for key in ["PATH", "USER", "SHELL", "LANG", "TERM"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("HOME", &attempt.root)
        .env("BMUX_CONFIG_DIR", &attempt.config_dir)
        .env("BMUX_RUNTIME_DIR", &attempt.runtime_dir)
        .env("BMUX_DATA_DIR", &attempt.data_dir)
        .env("BMUX_STATE_DIR", &attempt.state_dir)
        .args(["attach", &session.to_string(), "--observer-socket"])
        .arg(&socket)
        .args([
            "--observer-cols",
            &attempt.viewport_cols.to_string(),
            "--observer-rows",
            &attempt.viewport_rows.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // D's start probe has already negotiated this selected executable's worker
    // version; F's header independently rejects unsupported observer versions.
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let mut stderr = child.stderr.take().ok_or("observer stderr unavailable")?;
    let mut diagnostics = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut retained = Vec::new();
        let mut buffer = [0; 1024];
        while let Ok(count) = stderr.read(&mut buffer).await {
            if count == 0 {
                break;
            }
            let remaining = 2048usize.saturating_sub(retained.len());
            retained.extend_from_slice(&buffer[..count.min(remaining)]);
        }
        String::from_utf8_lossy(&retained)
            .chars()
            .filter(|character| !character.is_control())
            .collect::<String>()
    });
    let shutdown_check = shutdown.clone();
    let result = tokio::select! {
        _ = shutdown.changed() => Ok(()),
        result = async {

        if *shutdown_check.borrow() {
            return Ok(());
        }
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .map_err(|_| "observer handshake timeout".to_owned())?
            .map_err(|error| error.to_string())?;
        read_capture(stream, owner, attempt.generation, session, sender).await.map_err(ObserverFailure::from)
        } => result,
    };
    // Closing the stream requests orderly detach. Escalation kills only this
    // explicitly owned output observer, never the run's sandbox/server process.
    let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            child
                .start_kill()
                .map_err(|error| format!("observer kill failed: {error}"))?;
            child.wait().await
        }
    }
    .map_err(|error| format!("observer reap failed: {error}"))?;
    let diagnostic = match tokio::time::timeout(Duration::from_secs(1), &mut diagnostics).await {
        Ok(Ok(text)) => text,
        _ => {
            diagnostics.abort();
            let _ = diagnostics.await;
            String::new()
        }
    };
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir(&directory);
    if *shutdown_check.borrow() {
        Ok(())
    } else {
        result.map_err(|mut error: ObserverFailure| {
            error.message = format!("{}; observer exit {status}; {diagnostic}", error.message);
            error
        })
    }
}
#[cfg(not(unix))]
pub async fn observe(
    _executable: &Path,
    _owner: String,
    _attempt: &Attempt,
    _sender: watch::Sender<Option<Arc<CaptureSnapshot>>>,
    _shutdown: watch::Receiver<bool>,
) -> Result<(), ObserverFailure> {
    Err("private retained observer transport requires Unix".into())
}
