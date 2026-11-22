//! This process's own files, executable and strings, on Unix.
pub(crate) mod atomic;
pub(crate) mod environment;

use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

/// Whether this process's executable is identified by its mapped image (Linux), rather than by
/// a digest its launcher attests.
pub(crate) const EXECUTABLE_MAPPED: bool = cfg!(target_os = "linux");

/// This process's mapped executable image, pinned for hashing (Linux).
pub(crate) fn open_mapped_executable() -> std::io::Result<std::fs::File> {
    if EXECUTABLE_MAPPED {
        std::fs::File::open("/proc/self/exe")
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
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
}

/// Files `options` creates are private to this user.
pub(crate) fn private_file(options: &mut std::fs::OpenOptions) {
    options.mode(0o600);
}

/// As [`private_file`], and `options` refuses to open through a symbolic link.
pub(crate) fn private_file_no_follow(options: &mut std::fs::OpenOptions) {
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
}

/// Whether a terminal read failed only because the terminal ended (`EIO` once the last slave
/// closes).
pub(crate) fn is_terminal_end(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::EIO)
}
