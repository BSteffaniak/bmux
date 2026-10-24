//! Filesystem and peer-identity boundary for local Unix sockets.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path};

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe IPC socket directory: use an owned private (0700) runtime directory; stop the old runtime before migrating, and do not unlink a live socket",
    )
}

pub(super) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Walk descriptor-relative, never chmod/chown an existing directory. Ancestors
/// must not be replaceable by another user. Root-owned sticky directories (e.g.
/// /tmp) are safe ancestors, but never safe socket parents themselves.
pub(super) fn prepare_parent(path: &Path) -> io::Result<()> {
    let absolute = std::env::current_dir()?.join(path);
    let parent = absolute.parent().ok_or_else(denied)?;
    let components: Vec<_> = parent.components().collect();
    let mut directory = File::open("/")?;
    for (index, component) in components.iter().enumerate() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => CString::new(name.as_encoded_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in socket path"))?,
            _ => return Err(denied()),
        };
        let last = index == components.len() - 1;
        // SAFETY: the directory descriptor and NUL-terminated name are valid.
        let created = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
        if created < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: stat is initialized by fstatat on success; pointers are valid.
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatat succeeded.
        let stat = unsafe { stat.assume_init() };
        // Permit trusted system aliases such as macOS /var -> /private/var,
        // but never a symlink as the socket's immediate parent.
        let system_alias =
            !last && stat.st_uid == 0 && stat.st_mode & libc::S_IFMT == libc::S_IFLNK;
        let flags = libc::O_RDONLY
            | libc::O_DIRECTORY
            | libc::O_CLOEXEC
            | if system_alias { 0 } else { libc::O_NOFOLLOW };
        // SAFETY: openat returns a fresh descriptor; no untrusted symlink is followed.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is newly owned and valid.
        directory = unsafe { File::from_raw_fd(fd) };
        let metadata = directory.metadata()?;
        let uid = effective_uid();
        let mode = metadata.permissions().mode();
        if last {
            if metadata.uid() != uid || mode & 0o077 != 0 || mode & 0o700 != 0o700 {
                return Err(denied());
            }
        } else if (metadata.uid() != 0 && metadata.uid() != uid)
            || (mode & 0o022 != 0 && !(metadata.uid() == 0 && mode & 0o1000 != 0))
        {
            return Err(denied());
        }
    }
    // A socket directly under / has no private parent.
    if parent == Path::new("/") {
        return Err(denied());
    }
    Ok(())
}

pub(super) fn require_uid(peer_uid: Option<u32>, expected_uid: u32) -> io::Result<()> {
    if peer_uid == Some(expected_uid) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local IPC peer is not the effective user",
        ))
    }
}

pub(super) fn verify_peer(stream: &tokio::net::UnixStream) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // Tokio uses SO_PEERCRED on Linux and getpeereid on macOS. A failed
        // credential query propagates: there is no permissive fallback.
        require_uid(Some(stream.peer_cred()?.uid()), effective_uid())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stream;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "local IPC peer verification is unsupported on this platform",
        ))
    }
}
