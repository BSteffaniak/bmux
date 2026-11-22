//! This process's own files, executable and strings, on Windows.
pub(crate) mod atomic;
pub(crate) mod environment;
mod environment_keys;

use std::path::Path;

/// Windows executables are identified by a digest their launcher attests.
pub(crate) const EXECUTABLE_MAPPED: bool = false;

/// Windows has no mapped executable image to pin.
pub(crate) fn open_mapped_executable() -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no mapped executable image",
    ))
}

/// Directories inherit their parent's access control on Windows.
///
/// # Errors
/// Never.
#[allow(clippy::unnecessary_wraps)] // as on Unix, where setting permissions can fail
pub(crate) const fn restrict_directory(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Files inherit their directory's access control on Windows.
pub(crate) const fn private_file(_options: &mut std::fs::OpenOptions) {}

/// As [`private_file`].
pub(crate) const fn private_file_no_follow(_options: &mut std::fs::OpenOptions) {}

/// Windows terminals end with a plain end of file.
pub(crate) const fn is_terminal_end(_error: &std::io::Error) -> bool {
    false
}
