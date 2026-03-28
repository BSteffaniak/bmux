//! Playbook system for headless scripted bmux execution.
//!
//! Supports two input formats:
//! - TOML playbook files (`.playbook.toml`)
//! - Line-oriented DSL (pipeable from stdin)
//!
//! Both formats parse into the same internal representation and are executed
//! by the same engine against an ephemeral sandbox server (or a live server).

pub mod display_track;
pub mod engine;
pub mod interactive;
pub mod parse_dsl;
pub mod parse_toml;
pub mod sandbox;
pub mod screen;
pub mod types;

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

use self::engine::run_playbook;
use self::types::{Playbook, PlaybookResult};

/// Parse a playbook from a file path. Detects format by extension.
pub fn parse_file(path: &Path) -> Result<Playbook> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading {}", path.display()))?;

    if path.extension().map_or(false, |ext| ext == "toml") {
        parse_toml::parse_toml(&content)
    } else {
        // Try TOML first, fall back to DSL
        parse_toml::parse_toml(&content).or_else(|_| parse_dsl::parse_dsl(&content))
    }
}

/// Parse a playbook from stdin.
pub fn parse_stdin() -> Result<Playbook> {
    let mut content = String::new();
    std::io::stdin()
        .read_to_string(&mut content)
        .context("failed reading stdin")?;

    // Try TOML first, fall back to DSL
    parse_toml::parse_toml(&content).or_else(|_| parse_dsl::parse_dsl(&content))
}

/// Run a playbook and return the result.
pub async fn run(playbook: Playbook, target_server: bool) -> Result<PlaybookResult> {
    run_playbook(playbook, target_server).await
}

/// Validate a playbook without executing it.
///
/// If `target_server` is true, the first-step `new-session` check is skipped
/// since the user may be targeting an existing session on a live server.
pub fn validate(playbook: &Playbook, target_server: bool) -> Vec<String> {
    let mut errors = Vec::new();

    if playbook.steps.is_empty() {
        errors.push("playbook has no steps".to_string());
    }

    // Check that the first meaningful action is new-session (unless targeting live server)
    if !target_server {
        let first_action = playbook.steps.first().map(|s| &s.action);
        if let Some(action) = first_action {
            if !matches!(action, types::Action::NewSession { .. }) {
                errors.push(
                    "first step should be 'new-session' (no session exists at start)".to_string(),
                );
            }
        }
    }

    // Check viewport dimensions
    if playbook.config.viewport.cols < 10 || playbook.config.viewport.rows < 5 {
        errors.push("viewport too small (minimum 10x5)".to_string());
    }

    // Track expected pane count for index validation
    let mut expected_pane_count: u32 = 0;

    for step in &playbook.steps {
        match &step.action {
            types::Action::NewSession { .. } => {
                expected_pane_count = 1;
            }
            types::Action::SplitPane { .. } => {
                expected_pane_count += 1;
            }
            types::Action::ClosePane { .. } => {
                expected_pane_count = expected_pane_count.saturating_sub(1);
            }

            // Validate regex patterns at parse time
            types::Action::WaitFor {
                pattern, timeout, ..
            } => {
                if timeout.is_zero() {
                    errors.push(format!("step {}: wait-for has zero timeout", step.index));
                }
                if let Err(e) = regex::Regex::new(pattern) {
                    errors.push(format!(
                        "step {}: invalid regex pattern '{}': {}",
                        step.index, pattern, e
                    ));
                }
            }
            types::Action::AssertScreen { matches, .. } => {
                if let Some(pattern) = matches {
                    if let Err(e) = regex::Regex::new(pattern) {
                        errors.push(format!(
                            "step {}: invalid regex pattern '{}': {}",
                            step.index, pattern, e
                        ));
                    }
                }
            }

            // Warn if targeting a pane before any split
            types::Action::SendKeys {
                pane: Some(idx), ..
            }
            | types::Action::FocusPane { target: idx } => {
                if expected_pane_count < 2 && *idx > 1 {
                    errors.push(format!(
                        "step {}: targets pane {} but only {} pane(s) expected at this point",
                        step.index, idx, expected_pane_count
                    ));
                }
            }

            _ => {}
        }
    }

    errors
}

/// Format a `PlaybookResult` as human-readable output.
pub fn format_result(result: &PlaybookResult) -> String {
    let mut out = String::new();

    let name = result.playbook_name.as_deref().unwrap_or("<unnamed>");
    let status = if result.pass { "PASS" } else { "FAIL" };
    out.push_str(&format!(
        "playbook: {name} — {status} ({} ms)\n",
        result.total_elapsed_ms
    ));
    out.push('\n');

    for step in &result.steps {
        let icon = match step.status {
            types::StepStatus::Pass => "+",
            types::StepStatus::Fail => "x",
            types::StepStatus::Skip => "-",
        };
        out.push_str(&format!(
            "  [{icon}] step {}: {} ({} ms)",
            step.index, step.action, step.elapsed_ms
        ));
        if let Some(detail) = &step.detail {
            out.push_str(&format!(" — {detail}"));
        }
        out.push('\n');
    }

    if !result.snapshots.is_empty() {
        out.push_str(&format!("\nsnapshots: {}\n", result.snapshots.len()));
        for snap in &result.snapshots {
            out.push_str(&format!("  - {} ({} panes)\n", snap.id, snap.panes.len()));
        }
    }

    if let Some(rid) = &result.recording_id {
        out.push_str(&format!("\nrecording: {rid}\n"));
    }

    if let Some(err) = &result.error {
        out.push_str(&format!("\nerror: {err}\n"));
    }

    out
}
