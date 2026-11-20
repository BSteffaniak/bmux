use crate::diagnostics_cli::WatchOptions;
use anyhow::Result;
use std::path::Path;

const BMUX_DIAGNOSTICS_TITLE: &str = "bmux diagnostics";
const BMUX_DIAGNOSTICS_EVENTS_FILE: &str = "events.jsonl";

pub fn run_diagnostics_watch(state_dir: &Path, options: WatchOptions) -> Result<u8> {
    let dir = state_dir.join("diagnostics");
    moosicbox_log_watch::run_watch(moosicbox_log_watch::WatchRunConfig {
        title: BMUX_DIAGNOSTICS_TITLE.to_string(),
        source: Some(moosicbox_log_watch::WatchSource::File {
            path: dir.join(BMUX_DIAGNOSTICS_EVENTS_FILE),
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
        log_dir: dir.clone(),
        log_file_prefix: BMUX_DIAGNOSTICS_EVENTS_FILE.to_string(),
        lines: options.lines,
        since: options.since,
        profile: options.profile,
        include: options.include,
        include_i: options.include_i,
        exclude: options.exclude,
        exclude_i: options.exclude_i,
        state_file: Some(dir.join("watch-profiles.json")),
    })?;
    Ok(0)
}
