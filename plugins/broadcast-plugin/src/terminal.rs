//! Private retained terminal adapter. Never invokes PATH or creates an input surface.
#[cfg(all(test, unix))]
#[path = "terminal_tests.rs"]
mod tests;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::watch,
};
use uuid::Uuid;

/// Explicit selected runtime, verified by artifact digest and version before launch.
/// This adapter preserves all selected ConfigPaths; it never discovers a default runtime.
pub struct RuntimeSelection {
    pub executable: PathBuf,
    pub sha256: String,
    pub version: String,
    pub paths: bmux_config::ConfigPaths,
}
impl RuntimeSelection {
    pub(crate) async fn verify(&self) -> Result<(), &'static str> {
        if !self.executable.is_absolute() || self.sha256.len() != 64 {
            return Err("runtime_identity");
        }
        let mut file = std::fs::File::open(&self.executable).map_err(|_| "runtime_unavailable")?;
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count =
                std::io::Read::read(&mut file, &mut buffer).map_err(|_| "runtime_unavailable")?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if digest != self.sha256 {
            return Err("runtime_identity");
        }
        let output = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::process::Command::new(&self.executable)
                .arg("--version")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "runtime_timeout")?
        .map_err(|_| "runtime_unavailable")?;
        if !output.status.success()
            || output.stdout.len() > 256
            || String::from_utf8_lossy(&output.stdout).trim() != self.version
        {
            return Err("runtime_version");
        }
        Ok(())
    }
}

#[cfg(unix)]
fn observer_directory() -> Result<tempfile::TempDir, &'static str> {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::Builder::new()
        .prefix("bmux-bo-")
        .tempdir_in("/tmp")
        .map_err(|_| "observer_directory")?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "observer_directory")?;
    Ok(directory)
}

/// Latest complete self-contained retained output; EOF invalidates the channel.
/// Parent explicitly selects the target and admits exactly one worker's BMAO1 epoch.
#[cfg(unix)]
pub async fn observe(
    selection: RuntimeSelection,
    target: Uuid,
    cols: u16,
    rows: u16,
    sender: watch::Sender<Option<Arc<[u8]>>>,
    mut stop: watch::Receiver<bool>,
) -> Result<(), &'static str> {
    selection.verify().await?;
    if *stop.borrow() {
        sender.send_replace(None);
        return Ok(());
    }
    if cols == 0 || rows == 0 || usize::from(cols) * usize::from(rows) > 262_144 {
        return Err("viewport_bound");
    }
    // sockaddr_un is only 104 bytes on macOS. Runtime roots (especially
    // Library/Application Support paths) plus a UUID can exceed that limit.
    // This is only a private output rendezvous; the observer still connects to
    // the explicitly selected daemon through the unchanged ConfigPaths.
    let directory = observer_directory()?;
    let socket = directory.path().join("out.sock");
    let result = async {
        let listener = tokio::net::UnixListener::bind(&socket).map_err(|_| "observer_socket")?;
        let mut command = tokio::process::Command::new(&selection.executable);
        command.env_clear();
        for name in ["HOME", "USER", "SHELL", "LANG", "TERM"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command
            .env("BMUX_CONFIG_DIR", &selection.paths.config_dir)
            .env("BMUX_RUNTIME_DIR", &selection.paths.runtime_dir)
            .env("BMUX_DATA_DIR", &selection.paths.data_dir)
            .env("BMUX_STATE_DIR", &selection.paths.state_dir)
            .args(["attach", &target.to_string(), "--observer-socket"])
            .arg(&socket)
            .args([
                "--observer-cols",
                &cols.to_string(),
                "--observer-rows",
                &rows.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "observer_launch")?;
        let result = async {
            let (stream, _) = tokio::select! {
                accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()) => {
                    accepted.map_err(|_| "observer_timeout")?.map_err(|_| "observer_connect")?
                }
                status = child.wait() => {
                    return Err(if status.is_ok() { "observer_exited" } else { "observer_wait" });
                }
                _ = stop.changed() => return Ok(()),
            };
            tokio::select! {
                result = read_output(stream, target, &sender) => result,
                _ = stop.changed() => Ok(()),
            }
        }
        .await;
        let _ = child.kill().await;
        let _ = child.wait().await;
        result
    }
    .await;
    sender.send_replace(None);
    let _ = std::fs::remove_file(&socket);
    let _ = directory.close();
    result
}

#[cfg(unix)]
async fn read_output(
    mut stream: tokio::net::UnixStream,
    target: Uuid,
    sender: &watch::Sender<Option<Arc<[u8]>>>,
) -> Result<(), &'static str> {
    let fps = std::env::var("BMUX_BROADCAST_MAX_FPS")
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|fps| (1..=60).contains(fps))
        .unwrap_or(60);
    tokio::time::timeout(
        Duration::from_secs(2),
        stream.write_all(&[b'B', b'M', b'A', b'Q', 2, 2, fps, 0]),
    )
    .await
    .map_err(|_| "observer_timeout")?
    .map_err(|_| "observer_closed")?;
    let mut header = [0_u8; 38];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut header))
        .await
        .map_err(|_| "observer_timeout")?
        .map_err(|_| "observer_closed")?;
    if &header[..5] != b"BMAO\x02"
        || &header[21..37] != target.as_bytes()
        || header[37] == 0
        || header[37] > fps
    {
        return Err("observer_protocol");
    }
    let epoch = Uuid::from_slice(&header[5..21]).map_err(|_| "observer_protocol")?;
    let generation = format!("{epoch}:{target}");
    let mut revision = 0;
    let mut baseline = None;
    loop {
        let length = stream.read_u32().await.map_err(|_| "observer_closed")? as usize;
        if length == 0 || length > crate::relay::MAX_FRAME_BYTES {
            return Err("frame_bound");
        }
        let mut bytes = vec![0_u8; length];
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut bytes))
            .await
            .map_err(|_| "observer_timeout")?
            .map_err(|_| "observer_closed")?;
        let frame = bmux_web_renderer::decode_binary(&bytes, baseline.as_ref())
            .map_err(|_| "invalid_frame")?;
        if frame.generation() != generation || frame.revision() <= revision {
            return Err("observer_protocol");
        }
        revision = frame.revision();
        // Reconstruct before the latest-value channel: no coalesced delta chain
        // can lose an intermediate base. The controller retains one full repair.
        let full = bmux_web_renderer::encode_binary(&frame).map_err(|_| "invalid_frame")?;
        baseline = Some(frame);
        sender.send_replace(Some(full.into()));
    }
}
