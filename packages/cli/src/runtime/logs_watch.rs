use bmux_config::ConfigPaths;
use std::path::PathBuf;

const BMUX_LOG_FILE_PREFIX: &str = "bmux.log";

pub fn active_log_file_path() -> PathBuf {
    active_log_file_path_at(&ConfigPaths::default())
}

pub(super) fn active_log_file_path_at(paths: &ConfigPaths) -> PathBuf {
    let segmented = paths
        .logs_dir()
        .join("server")
        .join("latest")
        .join("latest.log");
    if segmented.exists() {
        return segmented;
    }
    moosicbox_log_watch::active_log_file_path(&paths.logs_dir(), BMUX_LOG_FILE_PREFIX)
}
