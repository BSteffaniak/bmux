//! Owned observer subprocess. The only visual transport is the retained wire.
use crate::playbook_capture::{CaptureSnapshot, read_capture};
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
) -> Result<(), String> {
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
        .args(["--observer-cols", "80", "--observer-rows", "24"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // D's start probe has already negotiated this selected executable's worker
    // version; F's header independently rejects unsupported observer versions.
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let shutdown_check = shutdown.clone();
    let result = async {
        if *shutdown.borrow() {
            return Ok(());
        }
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .map_err(|_| "observer handshake timeout".to_owned())?
            .map_err(|error| error.to_string())?;
        tokio::select! {
            result = read_capture(stream, owner, attempt.generation, session, sender) => result,
            _ = shutdown.changed() => Ok(()),
        }
    }
    .await;
    // Closing the stream requests orderly detach. Escalation kills only this
    // explicitly owned output observer, never the run's sandbox/server process.
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir(&directory);
    if result.is_err() && attempt.cleanup_acknowledged {
        return Ok(());
    }
    if *shutdown_check.borrow() {
        Ok(())
    } else {
        result
    }
}
#[cfg(not(unix))]
pub async fn observe(
    _executable: &Path,
    _owner: String,
    _attempt: &Attempt,
    _sender: watch::Sender<Option<Arc<CaptureSnapshot>>>,
    _shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    Err("private retained observer transport requires Unix".into())
}
