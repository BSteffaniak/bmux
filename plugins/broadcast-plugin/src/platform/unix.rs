use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

pub(crate) fn private_dir(path: &Path, create: bool) -> Result<(), &'static str> {
    use std::os::unix::fs::MetadataExt;
    if create {
        let parent = path.parent().ok_or("private_directory_required")?;
        let metadata =
            std::fs::symlink_metadata(parent).map_err(|_| "private_directory_required")?;
        // SAFETY: geteuid only observes this process's effective identity.
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err("private_directory_required");
        }
        let mut builder = std::fs::DirBuilder::new();
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
        match builder.create(path) {
            Ok(()) => {
                std::fs::File::open(parent)
                    .and_then(|file| file.sync_all())
                    .map_err(|_| "private_directory_required")?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("private_directory_required"),
        }
    }
    let m = std::fs::symlink_metadata(path).map_err(|_| "private_directory_required")?;
    // SAFETY: geteuid only reads the effective process identity.
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
    {
        return Err("private_directory_required");
    }
    Ok(())
}

pub(crate) fn open_private(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = file.metadata()?;
    // SAFETY: geteuid only observes the current effective identity.
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "owner-private file required",
        ));
    }
    Ok(file)
}
pub(crate) fn create_new(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}
pub(crate) fn lock(path: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = file.metadata()?;
    // SAFETY: geteuid observes identity; flock operates on this owned descriptor.
    if !m.is_file()
        || m.mode() & 0o077 != 0
        || m.uid() != unsafe { libc::geteuid() }
        || unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private lock unavailable",
        ));
    }
    Ok(file)
}
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
pub(crate) fn publish_new(source: &Path, target: &Path) -> io::Result<()> {
    // The link atomically publishes without overwriting a concurrent winner.
    std::fs::hard_link(source, target)?;
    sync_directory(
        target
            .parent()
            .ok_or_else(|| io::Error::other("missing asset parent"))?,
    )
}

pub(crate) fn publish(source: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(source, target)?;
    sync_directory(
        target
            .parent()
            .ok_or_else(|| io::Error::other("missing parent"))?,
    )
}

pub(crate) struct ObserverListener {
    listener: tokio::net::UnixListener,
    _directory: tempfile::TempDir,
    endpoint: PathBuf,
}
impl ObserverListener {
    pub(crate) fn bind() -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::Builder::new()
            .prefix("bmux-bo-")
            .tempdir_in("/tmp")?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let endpoint = directory.path().join("out.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint)?;
        Ok(Self {
            listener,
            _directory: directory,
            endpoint,
        })
    }
    pub(crate) fn endpoint(&self) -> &Path {
        &self.endpoint
    }
    pub(crate) async fn accept(&mut self, _child: u32) -> io::Result<tokio::net::UnixStream> {
        self.listener.accept().await.map(|(stream, _)| stream)
    }
}
impl Drop for ObserverListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.endpoint);
        // TempDir removes only this newly owned rendezvous.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn unsafe_file_and_directory_permissions_and_symlinks_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("key");
        drop(create_new(&file).unwrap());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_private(&file).is_err());
        let alias = root.path().join("alias");
        symlink(&file, &alias).unwrap();
        assert!(open_private(&alias).is_err());
        assert!(private_dir(&alias, false).is_err());
    }

    #[tokio::test]
    async fn rendezvous_is_short_private_and_owned_cleanup_removes_it() {
        let listener = ObserverListener::bind().unwrap();
        let endpoint = listener.endpoint().to_path_buf();
        let parent = endpoint.parent().unwrap().to_path_buf();
        assert!(endpoint.as_os_str().as_encoded_bytes().len() < 104);
        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(listener);
        assert!(!parent.exists());
    }
}
