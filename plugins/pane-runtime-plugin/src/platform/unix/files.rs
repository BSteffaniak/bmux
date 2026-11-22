//! This process's files and executable, on Unix (through `switchy_fs`'s native backend).
use std::path::Path;
use switchy_fs::sync::{File, OpenOptions, Permissions};

pub(crate) use super::native::{current_executable, executable_sha256, retain_executable};

/// Whether this process's executable is identified by its mapped image (Linux), rather than by
/// a digest its launcher attests.
pub(crate) const EXECUTABLE_MAPPED: bool = cfg!(target_os = "linux");

/// This process's mapped executable image, pinned for hashing (Linux).
pub(crate) fn open_mapped_executable() -> std::io::Result<File> {
    if EXECUTABLE_MAPPED {
        File::open("/proc/self/exe")
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no mapped executable image",
        ))
    }
}

/// Make `directory` private to this user.
///
/// # Errors
/// When its permissions cannot be set.
pub(crate) fn restrict_directory(directory: &Path) -> std::io::Result<()> {
    switchy_fs::sync::set_permissions(directory, Permissions::from_mode(0o700))
}

/// Files `options` creates are private to this user.
pub(crate) const fn private_file(options: OpenOptions) -> OpenOptions {
    options.mode(0o600)
}

/// As [`private_file`], and `options` refuses to open through a symbolic link.
pub(crate) const fn private_file_no_follow(options: OpenOptions) -> OpenOptions {
    options.mode(0o600).nofollow(true)
}

/// Durable whole-file replacement.
pub(crate) mod atomic {
    use std::path::Path;
    use switchy_fs::sync::File;

    /// Exclusively create the staging file `path`, private to this user.
    pub(crate) fn create(path: &Path) -> std::io::Result<File> {
        switchy_fs::sync::create_private_file(path)
    }

    /// Replace `path` with `temporary`, and make the replacement durable in `parent`.
    pub(crate) fn publish(temporary: &Path, path: &Path, parent: &Path) -> std::io::Result<()> {
        switchy_fs::sync::rename(temporary, path)?;
        switchy_fs::sync::sync_directory(parent)
    }
}
