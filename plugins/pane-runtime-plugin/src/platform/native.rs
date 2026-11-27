//! What the native targets share: finding programs on this host, and this process's executable.
use std::path::{Path, PathBuf};

/// This process's id.
pub(crate) fn process_id() -> u32 {
    std::process::id()
}

/// This process's executable.
///
/// # Errors
/// When the operating system cannot say.
pub(crate) fn current_executable() -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

/// Retain a verified copy of the executable `source` (whose digest is `hash`) under
/// `directory` for `owner`, and return the copy's path.
///
/// # Errors
/// When the copy cannot be made or does not match `hash`.
pub(crate) fn retain_executable(
    source: &Path,
    directory: &Path,
    owner: &str,
    hash: &str,
) -> Result<PathBuf, String> {
    bmux_plugin::retain_executable(source, directory, owner, hash)
}

/// Retain a verified copy of the executable `source` as it is now under `directory` for
/// `owner`, reading it once, and return the copy's path and digest.
///
/// # Errors
/// When the copy cannot be made, or `source` changes while it is copied.
pub(crate) fn retain_executable_snapshot(
    source: &Path,
    directory: &Path,
    owner: &str,
) -> Result<(PathBuf, String), String> {
    bmux_plugin::retain_executable_snapshot(source, directory, owner)
}

pub(crate) fn path_is_usable_program(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    super::target::is_executable(&metadata)
}

pub(crate) fn program_name_is_on_path(program: &str) -> bool {
    let Some(path) = switchy_env::var_os("PATH") else {
        return false;
    };
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        if path_is_usable_program(&candidate) {
            return true;
        }
        if candidate.extension().is_none() {
            for extension in super::target::executable_extensions() {
                if path_is_usable_program(&candidate.with_extension(extension)) {
                    return true;
                }
            }
        }
    }
    false
}
