use anyhow::{Context, Result};
use bmux_config::ConfigPaths;
use tracing::Level;

use super::{EFFECTIVE_LOG_LEVEL, active_log_file_path};

pub(super) fn run_logs_path(as_json: bool) -> Result<u8> {
    print_logs_path(as_json, &active_log_file_path())
}

pub(super) fn run_logs_path_selected(as_json: bool, paths: &ConfigPaths) -> Result<u8> {
    print_logs_path(as_json, &super::logs_watch::active_log_file_path_at(paths))
}

fn print_logs_path(as_json: bool, path: &std::path::Path) -> Result<u8> {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "path": path }))
                .context("failed to encode log path json")?
        );
        return Ok(0);
    }
    println!("{}", path.display());
    Ok(0)
}

pub(super) fn run_logs_level(as_json: bool) -> Result<u8> {
    let level = EFFECTIVE_LOG_LEVEL.get().copied().unwrap_or(Level::INFO);
    let value = match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    };
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "level": value }))
                .context("failed to encode log level json")?
        );
        return Ok(0);
    }
    println!("{value}");
    Ok(0)
}
