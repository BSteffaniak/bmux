//! Cleanup for exclusive recording-test TMPDIRs, never ambient runtime state.
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;
use std::time::{Duration, Instant};

/// Hold every runtime's lifetime fence while removing the private fixture tree.
/// A stop acknowledgment is not completion: retain artifacts until successful
/// shutdown and lifetime-lock release, including when a test is unwinding.
pub fn cleanup_short_tmp(root: &Path, wait: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        match try_cleanup(root) {
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

fn try_cleanup(root: &Path) -> std::io::Result<()> {
    let mut fences = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !entry.file_type()?.is_dir()
            || !name
                .strip_prefix("r-")
                .is_some_and(|id| id.len() == 24 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(std::io::Error::other("unexpected fixture TMPDIR entry"));
        }
        let runtime = entry.path().join("runtime");
        let fence = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(runtime.join("server-lifetime.lock"))?;
        fence.try_lock().map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::WouldBlock, error.to_string())
        })?;
        let named = fs::symlink_metadata(runtime.join("server-lifetime.lock"))?;
        let opened = fence.metadata()?;
        if !opened.is_file() || (named.dev(), named.ino()) != (opened.dev(), opened.ino()) {
            return Err(std::io::Error::other("fixture lifetime claim replaced"));
        }
        let socket = runtime.join("server.sock");
        match fs::symlink_metadata(&socket) {
            Ok(metadata) => {
                let expected = format!("bmux-endpoint-v1 {} {}\n", metadata.dev(), metadata.ino());
                if !metadata.file_type().is_socket()
                    || fs::read_to_string(runtime.join("server-lifetime.lock"))? != expected
                {
                    return Err(std::io::Error::other("fixture endpoint ownership changed"));
                }
                match std::os::unix::net::UnixStream::connect(&socket) {
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
                    _ => return Err(std::io::Error::other("fixture endpoint live or uncertain")),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let receipt: Value = fs::read(runtime.join("shutdown-result.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::WouldBlock, "shutdown not confirmed")
            })?;
        if receipt.get("error") != Some(&Value::Null)
            || receipt["pid"].as_u64().is_none()
            || receipt["started_at_epoch_ms"].as_u64().is_none()
            || runtime.join("server.pid").try_exists()?
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "shutdown incomplete or failed",
            ));
        }
        fences.push(fence);
    }
    // The fixture owns this exclusive parent; no new launches occur during Drop.
    // Keep all fences held until the directory and executable copies are gone.
    fs::remove_dir_all(root)
}
