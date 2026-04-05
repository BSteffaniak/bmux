use anyhow::{Context, Result};
use bmux_cli_schema::{RecordingExportFormat, RecordingRenderMode};
use std::time::Duration;

use super::{discover_bundled_plugin_ids, recording, run_recording_export};

pub(super) async fn run_playbook_run(
    source: &str,
    json: bool,
    interactive: bool,
    target_server: bool,
    record: bool,
    export_gif: Option<&str>,
    viewport: Option<&str>,
    timeout: Option<u64>,
    shell: Option<&str>,
    cli_vars: &[String],
    verbose: bool,
) -> Result<u8> {
    let mut playbook = if source == "-" {
        crate::playbook::parse_stdin().context("failed parsing playbook from stdin")?
    } else {
        crate::playbook::parse_file(std::path::Path::new(source))
            .with_context(|| format!("failed parsing playbook from {source}"))?
    };

    // CLI flags override playbook config.
    if record || export_gif.is_some() {
        playbook.config.record = true;
    }
    if let Some(vp) = viewport {
        let (cols, rows) = parse_viewport_string(vp)?;
        playbook.config.viewport.cols = cols;
        playbook.config.viewport.rows = rows;
    }
    if let Some(secs) = timeout {
        playbook.config.timeout = Duration::from_secs(secs);
    }
    if let Some(sh) = shell {
        playbook.config.shell = Some(sh.to_string());
    }

    // CLI --var overrides @var directives.
    for var_str in cli_vars {
        if let Some(eq_pos) = var_str.find('=') {
            let key = var_str[..eq_pos].to_string();
            let value = var_str[eq_pos + 1..].to_string();
            playbook.config.vars.insert(key, value);
        } else {
            anyhow::bail!("invalid --var format: expected KEY=VALUE, got '{var_str}'");
        }
    }

    // Populate bundled plugin IDs so the sandbox can configure plugins.
    playbook.config.bundled_plugin_ids = discover_bundled_plugin_ids();
    playbook.config.verbose = verbose;

    let result = if interactive {
        crate::playbook::run_with_options(
            playbook,
            target_server,
            crate::playbook::RunOptions { interactive: true },
        )
        .await?
    } else {
        crate::playbook::run(playbook, target_server).await?
    };

    // Export GIF if requested and a recording was produced.
    if let Some(gif_path) = export_gif {
        if let Some(ref rec_id) = result.recording_id {
            let recording_id_str = rec_id.to_string();
            match run_recording_export(
                &recording_id_str,
                RecordingExportFormat::Gif,
                gif_path,
                None,                        // view_client: auto-detect
                1.0,                         // speed
                12,                          // fps
                None,                        // max_duration
                None,                        // max_frames
                RecordingRenderMode::Bitmap, // Use bitmap for headless (no real terminal fonts)
                None,                        // cell_size
                None,                        // cell_width
                None,                        // cell_height
                None,                        // font_family
                None,                        // font_size
                None,                        // line_height
                &[],                         // font_path
                None,                        // cursor
                None,                        // cursor_shape
                None,                        // cursor_blink
                None,                        // cursor_blink_period_ms
                None,                        // cursor_color
                None,                        // cursor_profile
                None,                        // cursor_solid_after_activity_ms
                None,                        // cursor_solid_after_input_ms
                None,                        // cursor_solid_after_output_ms
                None,                        // cursor_solid_after_cursor_ms
                None,                        // cursor_paint_mode
                None,                        // cursor_text_mode
                None,                        // cursor_bar_width_pct
                None,                        // cursor_underline_height_pct
                None,                        // export_metadata
                true,                        // show_progress
            )
            .await
            {
                Ok(_) => {
                    if !json {
                        println!("exported GIF: {gif_path}");
                    }
                }
                Err(e) => {
                    eprintln!("GIF export failed: {e:#}");
                }
            }
        } else if !json {
            eprintln!("GIF export skipped: no recording was produced");
        }
    }

    if json {
        let json_str =
            serde_json::to_string_pretty(&result).context("failed serializing playbook result")?;
        println!("{json_str}");
    } else {
        print!("{}", crate::playbook::format_result(&result));
    }

    Ok(u8::from(!result.pass))
}

pub(super) fn run_playbook_validate(source: &str, json: bool) -> Result<u8> {
    let playbook = if source == "-" {
        crate::playbook::parse_stdin().context("failed parsing playbook from stdin")?
    } else {
        crate::playbook::parse_file(std::path::Path::new(source))
            .with_context(|| format!("failed parsing playbook from {source}"))?
    };

    let errors = crate::playbook::validate(&playbook, false);

    if json {
        let report = serde_json::json!({
            "valid": errors.is_empty(),
            "errors": errors,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if errors.is_empty() {
        println!("playbook is valid");
    } else {
        println!("playbook validation errors:");
        for error in &errors {
            println!("  - {error}");
        }
    }

    Ok(u8::from(!errors.is_empty()))
}

pub(super) fn run_playbook_dry_run(source: &str, json: bool) -> Result<u8> {
    let playbook = if source == "-" {
        crate::playbook::parse_stdin().context("failed parsing playbook from stdin")?
    } else {
        crate::playbook::parse_file(std::path::Path::new(source))
            .with_context(|| format!("failed parsing playbook from {source}"))?
    };

    let errors = crate::playbook::validate(&playbook, false);
    let valid = errors.is_empty();

    if json {
        let config = &playbook.config;
        let env_mode_str = match config.env_mode {
            Some(crate::playbook::types::SandboxEnvMode::Clean) => "clean",
            Some(crate::playbook::types::SandboxEnvMode::Inherit) => "inherit",
            None => "default",
        };
        let steps: Vec<serde_json::Value> = playbook
            .steps
            .iter()
            .map(|s| {
                serde_json::json!({
                    "index": s.index,
                    "action": s.action.name(),
                    "dsl": s.to_dsl(),
                })
            })
            .collect();

        let report = serde_json::json!({
            "valid": valid,
            "config": {
                "name": config.name,
                "viewport": format!("{}x{}", config.viewport.cols, config.viewport.rows),
                "shell": config.shell,
                "timeout_ms": config.timeout.as_millis() as u64,
                "env_mode": env_mode_str,
                "record": config.record,
            },
            "steps": steps,
            "step_count": playbook.steps.len(),
            "errors": errors,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let name = playbook.config.name.as_deref().unwrap_or("<unnamed>");
        println!("playbook: {name} (dry run)");
        println!(
            "  config: viewport={}x{} shell={} timeout={}ms env_mode={}",
            playbook.config.viewport.cols,
            playbook.config.viewport.rows,
            playbook.config.shell.as_deref().unwrap_or("default"),
            playbook.config.timeout.as_millis(),
            match playbook.config.env_mode {
                Some(crate::playbook::types::SandboxEnvMode::Clean) => "clean",
                Some(crate::playbook::types::SandboxEnvMode::Inherit) => "inherit",
                None => "default",
            },
        );
        println!("  steps:");
        for step in &playbook.steps {
            println!("    {}. {}", step.index, step.to_dsl());
        }
        if valid {
            println!("  validation: ok");
        } else {
            println!("  validation: ERRORS");
            for error in &errors {
                println!("    - {error}");
            }
        }
    }

    Ok(u8::from(!valid))
}

pub(super) fn run_playbook_diff(
    left_path: &str,
    right_path: &str,
    json: bool,
    timing_threshold: u64,
) -> Result<u8> {
    let left_data = std::fs::read_to_string(left_path)
        .with_context(|| format!("failed reading {left_path}"))?;
    let right_data = std::fs::read_to_string(right_path)
        .with_context(|| format!("failed reading {right_path}"))?;

    let left: crate::playbook::types::PlaybookResult = serde_json::from_str(&left_data)
        .with_context(|| format!("failed parsing {left_path} as PlaybookResult JSON"))?;
    let right: crate::playbook::types::PlaybookResult = serde_json::from_str(&right_data)
        .with_context(|| format!("failed parsing {right_path} as PlaybookResult JSON"))?;

    let report = crate::playbook::diff::diff_results(&left, &right, timing_threshold as f64);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let left_name = std::path::Path::new(left_path)
            .file_name()
            .map_or(left_path, |n| n.to_str().unwrap_or(left_path));
        let right_name = std::path::Path::new(right_path)
            .file_name()
            .map_or(right_path, |n| n.to_str().unwrap_or(right_path));
        print!(
            "{}",
            crate::playbook::diff::format_diff_report(&report, left_name, right_name)
        );
    }

    // Exit code: 0 if no changes, 1 if anything changed.
    let has_changes = report.summary.outcome_changed
        || report.summary.steps_changed > 0
        || report.summary.snapshots_changed > 0
        || !report.timing_regressions.is_empty();
    Ok(u8::from(has_changes))
}

pub(super) fn run_playbook_cleanup(dry_run: bool, json: bool) -> Result<u8> {
    let (scanned, entries) = crate::playbook::sandbox::cleanup_orphaned_sandboxes(dry_run)?;
    let orphaned = entries.len();

    if json {
        let report = serde_json::json!({
            "scanned": scanned,
            "orphaned": orphaned,
            "dry_run": dry_run,
            "entries": entries,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if orphaned > 0 {
        for entry in &entries {
            let status = if entry.removed { "removed" } else { "found" };
            println!("  {status}: {} (age: {}s)", entry.path, entry.age_secs);
        }
        if dry_run {
            println!("{orphaned} orphaned sandbox(es) found (dry run, not removed)");
        } else {
            let removed = entries.iter().filter(|e| e.removed).count();
            println!("{removed} orphaned sandbox(es) removed");
        }
    } else {
        println!("no orphaned sandboxes found ({scanned} scanned)");
    }

    Ok(0)
}

pub(super) async fn run_playbook_interactive(
    socket: Option<&str>,
    record: bool,
    viewport: &str,
    shell: Option<&str>,
    timeout: Option<u64>,
) -> Result<u8> {
    // Parse viewport string "COLSxROWS"
    let (cols, rows) = parse_viewport_string(viewport)?;

    let timeout_duration = timeout.map(Duration::from_secs);

    crate::playbook::interactive::run_interactive(
        socket,
        record,
        cols,
        rows,
        shell,
        timeout_duration,
    )
    .await
}

pub(super) fn parse_viewport_string(viewport: &str) -> Result<(u16, u16)> {
    let parts: Vec<&str> = viewport.split('x').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid viewport format: expected COLSxROWS (e.g. 80x24), got '{viewport}'");
    }
    let cols: u16 = parts[0]
        .parse()
        .with_context(|| format!("invalid viewport cols: '{}'", parts[0]))?;
    let rows: u16 = parts[1]
        .parse()
        .with_context(|| format!("invalid viewport rows: '{}'", parts[1]))?;
    if cols < 10 || rows < 5 {
        anyhow::bail!("viewport too small (minimum 10x5): {cols}x{rows}");
    }
    Ok((cols, rows))
}

pub(super) fn run_playbook_from_recording(recording_id: &str, output: Option<&str>) -> Result<u8> {
    let recordings = recording::list_recordings_from_dir(&recording::recordings_root_dir())?;
    let resolved_id = recording::resolve_recording_id_prefix(recording_id, &recordings)?;
    let events = recording::load_recording_events(&resolved_id.to_string())?;
    let playbook_dsl = crate::playbook::from_recording::events_to_playbook(&events);

    if let Some(path) = output {
        std::fs::write(path, &playbook_dsl)
            .with_context(|| format!("failed writing playbook to {path}"))?;
        println!("wrote playbook to {path}");
    } else {
        print!("{playbook_dsl}");
    }

    Ok(0)
}
