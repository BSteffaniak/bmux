//! Questions about paths, answered by the file system `switchy_fs` runs on (the host's, or the
//! simulated run's), never by `Path`'s own methods, which always ask the host.

use std::path::Path;

/// Whether `path` is a directory (following symbolic links), as `Path::is_dir` answers.
pub(crate) fn is_dir(path: &Path) -> bool {
    switchy_fs::sync::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}
