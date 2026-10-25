//! Process-owned startup exclusion for the CLI entry point.
//!
//! The foreground process owns the claim before inspecting endpoints or loading
//! plugins. The launcher owns no claim. The file is never unlinked and survives
//! crashes; the OS releases its lock. Unix socket reclamation requires both a
//! matching persisted bind receipt and a definitive refused connection while
//! holding the claim. Unknown, replaced, live and non-socket names are preserved.
//! This is a cooperating CLI lifecycle protocol, not protection from an actor
//! with write access deliberately bypassing that protocol.

use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Mutex;

#[cfg(unix)]
use anyhow::bail;
use anyhow::{Context, Result, ensure};
use bmux_config::ConfigPaths;

const CLAIM_NAME: &str = "server-lifetime.lock";

pub(super) struct StartupOwnership {
    claim_file: Mutex<File>,
    owner_process: u32,
    #[cfg(unix)]
    socket: PathBuf,
    #[cfg(unix)]
    recorded: Mutex<bool>,
    #[cfg(windows)]
    pipe_name: String,
}

impl Drop for StartupOwnership {
    fn drop(&mut self) {
        if std::process::id() != self.owner_process {
            return;
        }
        // flock belongs to the open file description, which a concurrent fork
        // can briefly inherit even with CLOEXEC. Release explicitly when this
        // owner ends, rather than waiting for every inherited descriptor to close.
        // No claim is ever handed to a child as ownership authority.
        let file = self
            .claim_file
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = file.unlock() {
            tracing::warn!(%error, "failed releasing runtime lifetime claim");
        }
    }
}

impl StartupOwnership {
    /// Nonblocking exclusion: a starting owner is not a stopped runtime.
    /// Acquired only in the real server process, never its daemon launcher.
    pub(super) fn acquire(paths: &ConfigPaths) -> Result<Self> {
        paths
            .ensure_runtime_dir()
            .context("prepare runtime claim directory")?;
        let path = paths.runtime_dir.join(CLAIM_NAME);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            ensure!(
                metadata.file_type().is_file(),
                "runtime lifetime claim is not a regular file"
            );
        }
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
        };
        #[cfg(not(unix))]
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path);
        let file = file.context("open runtime lifetime claim")?;
        file.try_lock()
            .context("runtime already owns lifetime claim (running or starting)")?;
        ensure!(
            file.metadata()?.is_file(),
            "runtime lifetime claim is not a regular file"
        );
        #[cfg(windows)]
        ensure!(
            file.metadata()?.len() == 0,
            "unsupported Windows lifetime claim contents"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let named = std::fs::symlink_metadata(&path)?;
            let opened = file.metadata()?;
            ensure!(
                named.is_file() && named.dev() == opened.dev() && named.ino() == opened.ino(),
                "runtime lifetime claim was replaced"
            );
        }
        Ok(Self {
            claim_file: Mutex::new(file),
            owner_process: std::process::id(),
            #[cfg(unix)]
            socket: paths.server_socket(),
            #[cfg(unix)]
            recorded: Mutex::new(false),
            #[cfg(windows)]
            pipe_name: paths.server_named_pipe(),
        })
    }

    /// Reclaim only a refused socket previously recorded by a cooperating owner.
    /// Never infer ownership from refusal, PID liveness, executable name or mtime.
    pub(super) async fn prepare_endpoint(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
            let mut receipt = String::new();
            {
                let mut file = self
                    .claim_file
                    .lock()
                    .map_err(|_| anyhow::anyhow!("lifetime claim poisoned"))?;
                ensure!(
                    file.metadata()?.len() <= 128,
                    "oversized endpoint ownership receipt"
                );
                file.seek(SeekFrom::Start(0))?;
                file.read_to_string(&mut receipt)?;
            }
            validate_receipt(&receipt)?;
            let metadata = match std::fs::symlink_metadata(&self.socket) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error).context("inspect claimed endpoint"),
            };
            ensure!(
                metadata.file_type().is_socket(),
                "selected endpoint is not a socket; refusing reclamation"
            );
            let expected = format!("bmux-endpoint-v1 {} {}\n", metadata.dev(), metadata.ino());
            ensure!(
                receipt == expected,
                "selected endpoint has no matching ownership receipt; refusing reclamation"
            );
            match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                tokio::net::UnixStream::connect(&self.socket),
            )
            .await
            {
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
                Ok(Err(error)) => {
                    return Err(error).context("endpoint state uncertain; refusing reclamation");
                }
                Ok(Ok(_)) => bail!("selected endpoint is live; refusing replacement"),
                Err(_) => bail!("selected endpoint probe timed out; refusing replacement"),
            }
            // Cooperating binders/removers hold this same lifetime claim. Recheck
            // identity to preserve a replacement installed before this operation.
            let current = std::fs::symlink_metadata(&self.socket)?;
            ensure!(
                current.file_type().is_socket()
                    && current.dev() == metadata.dev()
                    && current.ino() == metadata.ino(),
                "selected endpoint was replaced; refusing reclamation"
            );
            std::fs::remove_file(&self.socket).context("reclaim stopped owned endpoint")?;
        }
        #[cfg(windows)]
        ensure!(
            self.claim_file
                .lock()
                .map_err(|_| anyhow::anyhow!("lifetime claim poisoned"))?
                .metadata()?
                .len()
                == 0,
            "unsupported Windows lifetime claim contents"
        );
        #[cfg(windows)]
        match tokio::net::windows::named_pipe::ClientOptions::new().open(&self.pipe_name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("named pipe is busy or uncertain; refusing replacement");
            }
            Ok(_) => anyhow::bail!("selected named pipe is live; refusing replacement"),
        }
        // Windows names die with the last pipe handle; first-instance bind is
        // the final authority. Never delete files to recover a pipe name.
        Ok(())
    }

    /// Called immediately after each poll of the server's bind/accept future.
    /// No receipt is created for failed binds. Under the held claim, a pending
    /// first poll has performed the synchronous bind before yielding to restore.
    pub(super) fn record_bound_endpoint(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
            let mut recorded = self
                .recorded
                .lock()
                .map_err(|_| anyhow::anyhow!("bind receipt poisoned"))?;
            if *recorded {
                return Ok(());
            }
            let metadata = std::fs::symlink_metadata(&self.socket)
                .context("capture bound endpoint receipt")?;
            ensure!(
                metadata.file_type().is_socket(),
                "bound endpoint replaced by non-socket"
            );
            let mut file = self
                .claim_file
                .lock()
                .map_err(|_| anyhow::anyhow!("lifetime claim poisoned"))?;
            file.seek(SeekFrom::Start(0))?;
            file.set_len(0)?;
            writeln!(
                file,
                "bmux-endpoint-v1 {} {}",
                metadata.dev(),
                metadata.ino()
            )?;
            file.sync_data()?;
            drop(file);
            *recorded = true;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn validate_receipt(receipt: &str) -> Result<()> {
    if receipt.is_empty() {
        return Ok(());
    }
    let mut parts = receipt.split_whitespace();
    ensure!(
        parts.next() == Some("bmux-endpoint-v1"),
        "unsupported endpoint ownership receipt"
    );
    let device = parts
        .next()
        .context("corrupt endpoint ownership receipt")?
        .parse::<u64>()?;
    let inode = parts
        .next()
        .context("corrupt endpoint ownership receipt")?
        .parse::<u64>()?;
    ensure!(
        parts.next().is_none() && receipt == format!("bmux-endpoint-v1 {device} {inode}\n"),
        "corrupt endpoint ownership receipt"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
