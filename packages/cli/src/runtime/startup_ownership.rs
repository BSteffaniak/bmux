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

pub(super) fn inherit_companion_startup_fence(
    paths: &bmux_config::ConfigPaths,
    foreground_internal: bool,
) -> anyhow::Result<Option<File>> {
    use anyhow::{Context as _, ensure};
    let protocol = std::env::var_os("BMUX_COMPANION_STARTUP_LOCK_PROTOCOL");
    let selected = std::env::var_os("BMUX_COMPANION_STARTUP_LOCK");
    if protocol.is_none() && selected.is_none() {
        return Ok(None);
    }
    ensure!(
        foreground_internal,
        "companion startup fence requires foreground server"
    );
    ensure!(
        protocol.as_deref() == Some(std::ffi::OsStr::new("v1")),
        "unsupported companion startup fence protocol"
    );
    let path = std::path::PathBuf::from(selected.context("missing companion startup fence path")?);
    ensure!(
        path.is_absolute() && path == paths.runtime_dir.join("companion-startup.lock"),
        "companion startup fence selects different runtime"
    );
    #[cfg(unix)]
    {
        use std::os::fd::AsFd as _;
        use std::os::unix::fs::MetadataExt as _;
        // Descriptor cloning retains the inherited open-file description and
        // sets CLOEXEC, so future execution children cannot prolong the fence.
        let descriptor = std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .context("cannot retain companion startup fence")?;
        let file = File::from(descriptor);
        let named = std::fs::symlink_metadata(&path)?;
        let opened = file.metadata()?;
        ensure!(
            named.is_file()
                && opened.is_file()
                && named.dev() == opened.dev()
                && named.ino() == opened.ino(),
            "companion startup fence descriptor does not match selected file"
        );
        // The original stdin also shares the fence; CLOEXEC on the duplicate
        // alone would still let execution children retain the original forever.
        // SAFETY: fcntl only changes descriptor flags on this process's stdin.
        let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFD) };
        ensure!(
            flags >= 0
                && unsafe {
                    libc::fcntl(libc::STDIN_FILENO, libc::F_SETFD, flags | libc::FD_CLOEXEC)
                } >= 0,
            "cannot protect companion startup fence from child inheritance: {}",
            std::io::Error::last_os_error()
        );
        Ok(Some(file))
    }
    #[cfg(not(unix))]
    anyhow::bail!("inherited companion startup fences are unsupported on this platform")
}

/// Verify inherited digest claims before any plugin may advertise them. Keep
/// the opened image alive for the server lifetime. No environment mutation is
/// safe here: bootstrap already runs inside a multithreaded Tokio runtime.
pub(super) fn attest_managed_executable(foreground_internal: bool) -> Result<Option<File>> {
    if std::env::var_os("BMUX_MANAGED_TARGET_IDENTITY").is_none() {
        ensure!(
            std::env::var_os("BMUX_MANAGED_EXECUTABLE_SHA256").is_none(),
            "executable attestation requires explicit managed launch"
        );
        return Ok(None);
    }
    ensure!(
        foreground_internal,
        "managed executable attestation requires foreground launch"
    );
    #[cfg(target_os = "macos")]
    {
        macos_attestation::verify().map(Some)
    }
    // Linux readiness hashes the kernel's /proc/self/exe handle directly.
    #[cfg(target_os = "linux")]
    {
        Ok(None)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("managed executing-image attestation is unsupported on this platform")
}

#[cfg(target_os = "macos")]
mod macos_attestation {
    use anyhow::{Context as _, Result, ensure};
    use serde::{Deserialize, Serialize};
    use sha2::{Digest as _, Sha256};
    use std::collections::BTreeSet;
    use std::fs::{File, OpenOptions};
    use std::io::Read as _;
    use std::os::darwin::fs::MetadataExt as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::path::{Component, Path, PathBuf};

    // Darwin proc_info.h PROC_PIDREGIONPATHINFO layout. libc exposes the
    // vnode portion, but not this enclosing VM region structure.
    #[repr(C)]
    struct RegionInfo {
        protection: [u32; 4],
        offset: u64,
        accounting: [u32; 14],
        address: u64,
        size: u64,
    }
    #[repr(C)]
    struct RegionPath {
        region: RegionInfo,
        vnode: libc::vnode_info_path,
    }
    unsafe extern "C" {
        fn _dyld_get_image_header(index: u32) -> *const libc::c_void;
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Manifest {
        schema_version: u32,
        compatibility_id: String,
        target: String,
        artifacts: Vec<Artifact>,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Artifact {
        role: String,
        path: PathBuf,
        sha256: String,
    }

    fn hex_digest(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut result = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(&mut result, "{byte:02x}").expect("writing into String cannot fail");
        }
        result
    }

    fn hash(mut file: File) -> Result<String> {
        let mut digest = Sha256::new();
        let mut buffer = vec![0_u8; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        Ok(hex_digest(&digest.finalize()))
    }

    fn open(path: &Path) -> Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .context("open selected managed artifact without following symlinks")
    }

    fn mapped_identity() -> Result<(u64, u64)> {
        // SAFETY: image zero is the process's main Mach-O image. Its address is
        // used only as a kernel query key; no image memory is dereferenced.
        let header = unsafe { _dyld_get_image_header(0) };
        ensure!(!header.is_null(), "main Mach-O image is unavailable");
        let mut output = std::mem::MaybeUninit::<RegionPath>::zeroed();
        let size = i32::try_from(std::mem::size_of::<RegionPath>())?;
        // SAFETY: output is writable storage of exactly the advertised size;
        // PROC_PIDREGIONPATHINFO=8 queries this process's mapped backing vnode.
        let count = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                8,
                header as u64,
                output.as_mut_ptr().cast(),
                size,
            )
        };
        ensure!(
            count == size,
            "kernel cannot attest the executing Mach-O backing vnode: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the complete structure was initialized by the successful query.
        let output = unsafe { output.assume_init() };
        let address = header as u64;
        ensure!(
            output.region.address <= address
                && address - output.region.address < output.region.size,
            "kernel image evidence does not contain the main Mach-O header"
        );
        let stat = output.vnode.vip_vi.vi_stat;
        ensure!(
            stat.vst_ino != 0,
            "executing image has no file-backed vnode evidence"
        );
        Ok((u64::from(stat.vst_dev), stat.vst_ino))
    }

    fn verify_image_evidence(selected: (u64, u64), mapped: (u64, u64), flags: u32) -> Result<()> {
        ensure!(
            selected == mapped,
            "selected pathname is not the executing Mach-O image; preserve the original immutable bundle"
        );
        ensure!(
            flags & (libc::UF_IMMUTABLE | libc::SF_IMMUTABLE) != 0,
            "managed macOS companion image must be immutable (UF_IMMUTABLE or SF_IMMUTABLE) before exec"
        );
        Ok(())
    }

    pub(super) fn verify() -> Result<File> {
        let claimed = std::env::var("BMUX_MANAGED_EXECUTABLE_SHA256")
            .context("managed macOS launch requires verified executable SHA-256 evidence")?;
        let identity: serde_json::Value =
            serde_json::from_str(&std::env::var("BMUX_MANAGED_TARGET_IDENTITY")?)?;
        ensure!(
            identity["companion_sha256"].as_str() == Some(claimed.as_str()),
            "managed executable claim differs from selected target"
        );
        let executable = std::env::current_exe().context("locate candidate managed image")?;
        let image = open(&executable)?;
        let metadata = image.metadata()?;
        ensure!(metadata.is_file(), "managed image is not a regular file");
        verify_image_evidence(
            (metadata.dev(), metadata.ino()),
            mapped_identity()?,
            metadata.st_flags(),
        )?;
        ensure!(
            hash(image.try_clone()?)? == claimed,
            "executing immutable companion digest differs from selected target"
        );
        let root = executable
            .parent()
            .context("managed image has no bundle root")?
            .canonicalize()?;
        let manifest: Manifest = serde_json::from_reader(open(&root.join("bterm-bundle.json"))?)?;
        ensure!(
            manifest.schema_version == 1
                && manifest.compatibility_id == "bterm-bmux-v1"
                && manifest.target == format!("{}-apple-darwin", std::env::consts::ARCH),
            "unsupported managed bundle contract"
        );
        let artifact_set = hex_digest(&Sha256::digest(serde_json::to_vec(&manifest)?));
        ensure!(
            identity["artifact_set_sha256"].as_str() == Some(artifact_set.as_str()),
            "managed bundle manifest differs from selected target"
        );
        let mut paths = BTreeSet::new();
        let mut companions = 0;
        let mut frontends = 0;
        for artifact in &manifest.artifacts {
            ensure!(
                !artifact.path.as_os_str().is_empty()
                    && artifact
                        .path
                        .components()
                        .all(|part| matches!(part, Component::Normal(_)))
                    && paths.insert(&artifact.path),
                "invalid or duplicate managed bundle artifact path"
            );
            let path = root.join(&artifact.path).canonicalize()?;
            ensure!(
                path.starts_with(&root),
                "managed bundle artifact escapes its root"
            );
            let file = open(&path)?;
            ensure!(
                hash(file.try_clone()?)? == artifact.sha256,
                "managed bundle artifact digest mismatch"
            );
            if artifact.role == "companion" {
                companions += 1;
                let selected = file.metadata()?;
                ensure!(
                    (selected.dev(), selected.ino()) == (metadata.dev(), metadata.ino())
                        && artifact.sha256 == claimed,
                    "manifest companion is not the executing immutable image"
                );
            }
            if artifact.role == "frontend" {
                frontends += 1;
            }
        }
        ensure!(
            companions == 1 && frontends == 1,
            "managed bundle requires exactly one companion and frontend"
        );
        let final_metadata = image.metadata()?;
        verify_image_evidence(
            (final_metadata.dev(), final_metadata.ino()),
            mapped_identity()?,
            final_metadata.st_flags(),
        )?;
        Ok(image)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sha256_encoding_is_lowercase_full_width() {
            assert_eq!(hex_digest(&[0, 15, 16, 255]), "000f10ff");
            assert_eq!(
                hex_digest(&Sha256::digest(b"abc")),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            );
        }

        #[test]
        fn pathname_replacement_never_attests_old_running_image() {
            assert!(verify_image_evidence((1, 20), (1, 10), libc::UF_IMMUTABLE).is_err());
            assert!(verify_image_evidence((2, 10), (1, 10), libc::UF_IMMUTABLE).is_err());
        }

        #[test]
        fn mutable_same_inode_is_not_executing_byte_proof() {
            assert!(verify_image_evidence((1, 10), (1, 10), 0).is_err());
            verify_image_evidence((1, 10), (1, 10), libc::UF_IMMUTABLE).unwrap();
            verify_image_evidence((1, 10), (1, 10), libc::SF_IMMUTABLE).unwrap();
        }

        #[test]
        fn darwin_region_query_layout_matches_sdk_contract() {
            assert_eq!(std::mem::size_of::<RegionInfo>(), 96);
            assert_eq!(std::mem::offset_of!(RegionPath, vnode), 96);
            assert!(mapped_identity().is_ok());
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
    #[cfg(unix)]
    pub(super) async fn prepare_endpoint(&self) -> Result<()> {
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
        Ok(())
    }

    /// Pipes are kernel-owned: never reclaim a filesystem name for them.
    /// Offload the synchronous pipe-open probe rather than blocking the executor.
    #[cfg(windows)]
    pub(super) async fn prepare_endpoint(&self) -> Result<()> {
        self.validate_windows_claim()?;
        let pipe_name = self.pipe_name.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            match tokio::net::windows::named_pipe::ClientOptions::new().open(&pipe_name) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => {
                    Err(error).context("named pipe is busy or uncertain; refusing replacement")
                }
                Ok(_) => anyhow::bail!("selected named pipe is live; refusing replacement"),
            }
        })
        .await
        .context("join endpoint ownership probe")?
    }

    /// The OS file lock acquired above remains the startup authority; pipe
    /// lifetime needs no on-disk bind receipt because the kernel removes the
    /// endpoint with its final handle. Reject corrupt claims and inherited use.
    #[cfg(windows)]
    fn validate_windows_claim(&self) -> Result<()> {
        ensure!(
            self.owner_process == std::process::id(),
            "inherited runtime lifetime claim"
        );
        let file = self
            .claim_file
            .lock()
            .map_err(|_| anyhow::anyhow!("lifetime claim poisoned"))?;
        let metadata = file.metadata().context("inspect runtime lifetime claim")?;
        drop(file);
        ensure!(
            metadata.is_file() && metadata.len() == 0,
            "unsupported Windows lifetime claim contents"
        );
        Ok(())
    }

    /// Called immediately after each poll of the server's bind/accept future.
    /// No receipt is created for failed binds. Under the held claim, a pending
    /// first poll has performed the synchronous bind before yielding to restore.
    #[cfg(unix)]
    pub(super) fn record_bound_endpoint(&self) -> Result<()> {
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

    #[cfg(windows)]
    pub(super) fn record_bound_endpoint(&self) -> Result<()> {
        self.validate_windows_claim()
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

#[cfg(all(test, windows))]
mod windows_claim_tests {
    use super::*;

    #[test]
    fn bind_poll_rejects_corrupt_claim_without_releasing_exclusion() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        let claim = StartupOwnership::acquire(&paths).unwrap();
        claim.record_bound_endpoint().unwrap();
        claim.claim_file.lock().unwrap().set_len(1).unwrap();
        assert!(claim.record_bound_endpoint().is_err());
        assert!(StartupOwnership::acquire(&paths).is_err());
        claim.claim_file.lock().unwrap().set_len(0).unwrap();
        claim.record_bound_endpoint().unwrap();
        drop(claim);
        assert!(StartupOwnership::acquire(&paths).is_ok());
    }
}

#[cfg(test)]
mod tests;
