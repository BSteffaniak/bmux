use anyhow::Result;
use bmux_config::ConfigPaths;
use std::path::PathBuf;

const BMUX_DIAGNOSTICS_TITLE: &str = "bmux diagnostics";
const BMUX_DIAGNOSTICS_EVENTS_FILE: &str = "events.jsonl";

pub fn run_diagnostics_watch(
    lines: Option<usize>,
    since: Option<&str>,
    profile: Option<&str>,
    include: &[String],
    include_i: &[String],
    exclude: &[String],
    exclude_i: &[String],
) -> Result<u8> {
    moosicbox_log_watch::run_watch(moosicbox_log_watch::WatchRunConfig {
        title: BMUX_DIAGNOSTICS_TITLE.to_string(),
        source: Some(moosicbox_log_watch::WatchSource::File {
            path: diagnostics_events_file_path(),
        }),
        input_format: moosicbox_log_watch::WatchInputFormat::JsonLines(
            moosicbox_log_watch::JsonLinesWatchConfig {
                columns: vec![
                    moosicbox_log_watch::JsonLineColumn::new("TIME", "timestamp_ms").width(14),
                    moosicbox_log_watch::JsonLineColumn::new("LVL", "level").width(5),
                    moosicbox_log_watch::JsonLineColumn::new("COMPONENT", "component").width(28),
                    moosicbox_log_watch::JsonLineColumn::new("MESSAGE", "message"),
                ],
            },
        ),
        log_dir: diagnostics_dir_path(),
        log_file_prefix: BMUX_DIAGNOSTICS_EVENTS_FILE.to_string(),
        lines,
        since: since.map(ToString::to_string),
        profile: profile.map(ToString::to_string),
        include: include.to_vec(),
        include_i: include_i.to_vec(),
        exclude: exclude.to_vec(),
        exclude_i: exclude_i.to_vec(),
        state_file: Some(diagnostics_watch_state_file_path()),
    })?;
    Ok(0)
}

fn diagnostics_dir_path() -> PathBuf {
    ConfigPaths::default().state_dir().join("diagnostics")
}

fn diagnostics_events_file_path() -> PathBuf {
    diagnostics_dir_path().join(BMUX_DIAGNOSTICS_EVENTS_FILE)
}

fn diagnostics_watch_state_file_path() -> PathBuf {
    diagnostics_dir_path().join("watch-profiles.json")
}
