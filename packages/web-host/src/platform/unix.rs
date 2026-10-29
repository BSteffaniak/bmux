//! Unix handle-relative assets and durable private resolver cache.
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};
use std::process::Command;
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid asset path or allowlist",
    )
}
fn open_at(parent: &File, name: &str, flags: libc::c_int) -> io::Result<File> {
    let name = CString::new(name).map_err(|_| invalid())?;
    // SAFETY: parent is a live descriptor and name is NUL terminated. O_NOFOLLOW
    // applies to every component separately; ownership of a successful fd is transferred.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returned a new uniquely owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(invalid());
    }
    let mut directory = File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_at(
                    &directory,
                    name.to_str().ok_or_else(invalid)?,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )?;
            }
            _ => return Err(invalid()),
        }
    }
    Ok(directory)
}

pub(crate) fn open_asset(parent: &File, name: &str) -> io::Result<File> {
    open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
    )
}
pub(crate) struct Lock(File);
impl Lock {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("web setup lock is not a regular file"));
        }
        // SAFETY: file owns a valid descriptor for the entire lock lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: the owned descriptor remains valid until after drop returns.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::other(
                "web cache path is not a regular directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("invalid web cache root"))?;
    private_directory(parent)?;
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(path)?.is_dir() {
                return Err(io::Error::other(
                    "web cache path is not a regular directory",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    sync_directory(path)?;
    sync_directory(parent)
}

pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}
pub(crate) fn new_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
pub(crate) fn is_directory(path: &Path) -> io::Result<bool> {
    Ok(fs::symlink_metadata(path)?.is_dir())
}
pub(crate) fn source_command(root: &Path, args: &[&str]) -> io::Result<Command> {
    let mut command = Command::new("bash");
    command.arg(root.join("scripts/dev.sh")).args(args);
    Ok(command)
}

pub(crate) fn default_cache_root() -> Option<std::path::PathBuf> {
    None
}

pub(crate) fn sync_regular(path: &Path) -> io::Result<()> {
    open_regular(path)?.sync_all()
}
pub(crate) fn publish(stage: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(stage, destination)
}
