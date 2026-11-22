//! What the native targets share: finding programs on this host.
use std::path::Path;

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
