//! This process's files and executable, on Windows (through `switchy_fs`'s native backend).
#[path = "atomic.rs"]
pub(crate) mod atomic;

use std::path::Path;
use switchy_fs::sync::{File, OpenOptions};

pub(crate) use super::native::{
    current_executable, process_id, retain_executable, retain_executable_snapshot,
};

/// `path`'s identity: size, attributes, and last-write and creation times.
pub(crate) fn file_identity(path: &Path) -> Option<super::FileIdentity> {
    use std::os::windows::fs::MetadataExt as _;
    // simulation-safe: native Windows backend only; simulated targets track no identity
    let metadata = std::fs::symlink_metadata(path).ok()?;
    Some(super::FileIdentity([
        metadata.file_size(),
        u64::from(metadata.file_attributes()),
        metadata.last_write_time(),
        metadata.creation_time(),
        0,
        0,
        0,
        0,
    ]))
}

/// Windows executables are identified by a digest their launcher attests.
pub(crate) const EXECUTABLE_MAPPED: bool = false;

/// Windows has no mapped executable image to pin.
pub(crate) fn open_mapped_executable() -> std::io::Result<File> {
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
pub(crate) const fn private_file(options: OpenOptions) -> OpenOptions {
    options
}

/// As [`private_file`] (`switchy_fs` cannot refuse a final symbolic link on Windows).
pub(crate) const fn private_file_no_follow(options: OpenOptions) -> OpenOptions {
    options
}
