use anyhow::{Context, Result};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::{fs::File, io, path::Path};
pub(crate) fn create_private_directory(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}
pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}
pub(crate) fn create_new(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
pub(crate) fn publish(source: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(source, target)?;
    sync_directory(
        target
            .parent()
            .ok_or_else(|| io::Error::other("missing parent"))?,
    )
}
pub(crate) fn publish_noreplace(source: &Path, target: &Path) -> Result<()> {
    let target_parent = target
        .parent()
        .context("publication target has no parent")?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let target = std::ffi::CString::new(target.as_os_str().as_bytes())?;
        // Both paths point into the caller-owned export parent. The kernel
        // atomically rejects any competing destination, including empty dirs.
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("publishing artifacts without replacement");
        }
        sync_directory(target_parent).context("syncing no-replace publication parent")?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, target);
        anyhow::bail!("atomic no-replace artifact publication is unsupported on this platform")
    }
}

pub(crate) fn open_append(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

pub(crate) fn validate_runtime_socket(socket: &Path) -> Result<()> {
    std::os::unix::net::SocketAddr::from_pathname(socket).with_context(|| {
        format!(
            "Unix socket path exceeds platform bounds or is invalid: {}; select shorter TMPDIR",
            socket.display()
        )
    })?;
    Ok(())
}
