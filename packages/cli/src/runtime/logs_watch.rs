use anyhow::{Context, Result};
use bmux_config::ConfigPaths;
use moosicbox_log_watch::{
    LogFilterCaseMode as InternalLogFilterCaseMode, LogFilterKind as InternalLogFilterKind,
};
use std::path::PathBuf;

#[cfg(test)]
pub(crate) use moosicbox_log_watch::{
    LogFilterCaseMode, LogFilterKind, LogFilterRule, compile_filter_regex, line_visible_in_watch,
    normalize_profile_name as normalize_logs_watch_profile,
    watch_filter_rule_to_state as logs_watch_filter_rule_to_state,
    watch_filter_state_to_rule as logs_watch_filter_state_to_rule,
};

const BMUX_LOG_FILE_PREFIX: &str = "bmux.log";
const BMUX_WATCH_TITLE: &str = "bmux logs watch";

pub(crate) fn run_logs_watch(
    lines: Option<usize>,
    since: Option<&str>,
    profile: Option<&str>,
    include: &[String],
    include_i: &[String],
    exclude: &[String],
    exclude_i: &[String],
) -> Result<u8> {
    moosicbox_log_watch::run_watch(moosicbox_log_watch::WatchRunConfig {
        title: BMUX_WATCH_TITLE.to_string(),
        log_dir: ConfigPaths::default().logs_dir(),
        log_file_prefix: BMUX_LOG_FILE_PREFIX.to_string(),
        lines,
        since: since.map(ToString::to_string),
        profile: profile.map(ToString::to_string),
        include: include.to_vec(),
        include_i: include_i.to_vec(),
        exclude: exclude.to_vec(),
        exclude_i: exclude_i.to_vec(),
        state_file: Some(logs_watch_state_file_path()),
    })?;
    Ok(0)
}

pub(crate) fn run_logs_profiles_list(as_json: bool) -> Result<u8> {
    let summaries = moosicbox_log_watch::profiles_list(&logs_watch_state_file_path())?;

    if as_json {
        let payload = summaries
            .iter()
            .map(|summary| {
                serde_json::json!({
                    "name": summary.name,
                    "active": summary.active,
                    "filter_count": summary.filter_count,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed encoding logs profiles list json")?
        );
        return Ok(0);
    }

    for summary in summaries {
        let marker = if summary.active { "*" } else { " " };
        println!(
            "{marker} {} ({} filters)",
            summary.name, summary.filter_count
        );
    }
    Ok(0)
}

pub(crate) fn run_logs_profiles_show(profile: Option<&str>, as_json: bool) -> Result<u8> {
    let details = moosicbox_log_watch::profile_show(&logs_watch_state_file_path(), profile)?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "name": details.name,
                "active": details.active,
                "quick_filter": details.quick_filter,
                "since": details.since,
                "lines": details.lines,
                "selected_filter_index": details.selected_filter_index,
                "filters": details.filters,
            }))
            .context("failed encoding logs profile json")?
        );
        return Ok(0);
    }

    println!("profile: {}", details.name);
    println!("active: {}", if details.active { "yes" } else { "no" });
    println!(
        "lines: {}",
        details
            .lines
            .map_or_else(|| "(default)".to_string(), |value| value.to_string())
    );
    println!("since: {}", details.since.as_deref().unwrap_or("(none)"));
    println!(
        "quick filter: {}",
        details.quick_filter.as_deref().unwrap_or("(none)")
    );
    println!("filters:");
    if details.filters.is_empty() {
        println!("  (none)");
    } else {
        for filter in &details.filters {
            let kind = match filter.kind {
                InternalLogFilterKind::Include => "include",
                InternalLogFilterKind::Exclude => "exclude",
            };
            let case = match filter.case_mode {
                InternalLogFilterCaseMode::Sensitive => "case-sensitive",
                InternalLogFilterCaseMode::Insensitive => "case-insensitive",
            };
            let enabled = if filter.enabled {
                "enabled"
            } else {
                "disabled"
            };
            println!("  - {kind} /{}/ ({case}, {enabled})", filter.pattern);
        }
    }
    Ok(0)
}

pub(crate) fn run_logs_profiles_delete(profile: &str) -> Result<u8> {
    moosicbox_log_watch::profile_delete(&logs_watch_state_file_path(), profile)?;
    println!("deleted profile '{profile}'");
    Ok(0)
}

pub(crate) fn run_logs_profiles_rename(from: &str, to: &str) -> Result<u8> {
    moosicbox_log_watch::profile_rename(&logs_watch_state_file_path(), from, to)?;
    println!("renamed profile '{from}' -> '{to}'");
    Ok(0)
}

pub(crate) fn active_log_file_path() -> PathBuf {
    moosicbox_log_watch::active_log_file_path(
        &ConfigPaths::default().logs_dir(),
        BMUX_LOG_FILE_PREFIX,
    )
}

fn logs_watch_state_file_path() -> PathBuf {
    ConfigPaths::default()
        .state_dir()
        .join("runtime")
        .join("logs-watch-state.json")
}
