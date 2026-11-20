//! Client-local commands: no server, sandbox or managed runner is required.
use anyhow::{Context, Result};
use bmux_plugin_sdk::{NativeCommandContext, PluginCommandError};
use clap::Parser;

#[derive(Parser)]
#[command(name = "bmux playbook")]
enum Offline {
    /// Validate a playbook without executing it
    Validate {
        /// Path to playbook file, or `-` for stdin
        source: String,
        /// Output results as JSON
        #[arg(long)]
        json: bool,
    },
    /// Dry-run: parse, validate, and print the execution plan without running
    DryRun {
        /// Path to playbook file, or `-` for stdin
        source: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Compare results from two playbook runs
    Diff {
        /// Path to first (baseline/left) playbook result JSON
        left: String,
        /// Path to second (new/right) playbook result JSON
        right: String,
        /// Output diff as JSON
        #[arg(long)]
        json: bool,
        /// Timing regression threshold in percent (default: 50)
        #[arg(long, default_value = "50")]
        timing_threshold: u64,
    },
}

pub(super) fn run(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let args = ["bmux playbook", context.command.as_str()]
        .into_iter()
        .chain(context.arguments.iter().map(String::as_str));
    let command = Offline::try_parse_from(args)
        .map_err(|error| PluginCommandError::new(error.exit_code(), error.to_string()))?;
    match command {
        Offline::Validate { source, json } => run_playbook_validate(&source, json),
        Offline::DryRun { source, json } => run_playbook_dry_run(&source, json),
        Offline::Diff {
            left,
            right,
            json,
            timing_threshold,
        } => run_playbook_diff(&left, &right, json, timing_threshold),
    }
    .map(i32::from)
    .map_err(|error| PluginCommandError::from(format!("{error:#}")))
}

fn run_playbook_validate(source: &str, json: bool) -> Result<u8> {
    let playbook = if source == "-" {
        bmux_playbook::parser::parse_stdin().context("failed parsing playbook from stdin")?
    } else {
        bmux_playbook::parser::parse_file(std::path::Path::new(source))
            .with_context(|| format!("failed parsing playbook from {source}"))?
    };

    let errors = bmux_playbook::validation::validate(&playbook, false);

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

fn run_playbook_dry_run(source: &str, json: bool) -> Result<u8> {
    let playbook = if source == "-" {
        bmux_playbook::parser::parse_stdin().context("failed parsing playbook from stdin")?
    } else {
        bmux_playbook::parser::parse_file(std::path::Path::new(source))
            .with_context(|| format!("failed parsing playbook from {source}"))?
    };

    let errors = bmux_playbook::validation::validate(&playbook, false);
    let valid = errors.is_empty();

    if json {
        let config = &playbook.config;
        let env_mode_str = match config.env_mode {
            Some(bmux_playbook::types::SandboxEnvMode::Clean) => "clean",
            Some(bmux_playbook::types::SandboxEnvMode::Inherit) => "inherit",
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
                "timeout_ms": u64::try_from(config.timeout.as_millis()).unwrap_or(u64::MAX),
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
                Some(bmux_playbook::types::SandboxEnvMode::Clean) => "clean",
                Some(bmux_playbook::types::SandboxEnvMode::Inherit) => "inherit",
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

#[allow(clippy::cast_precision_loss)] // Timing threshold u64→f64 acceptable for comparison
fn run_playbook_diff(
    left_path: &str,
    right_path: &str,
    json: bool,
    timing_threshold: u64,
) -> Result<u8> {
    let left_data = std::fs::read_to_string(left_path)
        .with_context(|| format!("failed reading {left_path}"))?;
    let right_data = std::fs::read_to_string(right_path)
        .with_context(|| format!("failed reading {right_path}"))?;

    let left: bmux_playbook::types::PlaybookResult = serde_json::from_str(&left_data)
        .with_context(|| format!("failed parsing {left_path} as PlaybookResult JSON"))?;
    let right: bmux_playbook::types::PlaybookResult = serde_json::from_str(&right_data)
        .with_context(|| format!("failed parsing {right_path} as PlaybookResult JSON"))?;

    let report = bmux_playbook::diff::diff_results(&left, &right, timing_threshold as f64);

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
            bmux_playbook::diff::format_diff_report(&report, left_name, right_name)
        );
    }

    // Exit code: 0 if no changes, 1 if anything changed.
    let has_changes = report.summary.outcome_changed
        || report.summary.steps_changed > 0
        || report.summary.snapshots_changed > 0
        || !report.timing_regressions.is_empty();
    Ok(u8::from(has_changes))
}

#[cfg(test)]
mod tests {
    use super::Offline;
    use clap::Parser;

    #[test]
    fn offline_commands_accept_stdin_and_unsigned_diff_thresholds() {
        assert!(
            matches!(Offline::try_parse_from(["playbook", "validate", "--json", "-"]), Ok(Offline::Validate {source, json: true}) if source == "-")
        );
        assert!(matches!(
            Offline::try_parse_from(["playbook", "diff", "left", "right"]),
            Ok(Offline::Diff {
                timing_threshold: 50,
                ..
            })
        ));
        assert!(matches!(
            Offline::try_parse_from([
                "playbook",
                "diff",
                "--timing-threshold=18446744073709551615",
                "left",
                "right"
            ]),
            Ok(Offline::Diff {
                timing_threshold: u64::MAX,
                ..
            })
        ));
        assert!(
            Offline::try_parse_from(["playbook", "diff", "--timing-threshold=-1", "left", "right"])
                .is_err()
        );
    }
}
