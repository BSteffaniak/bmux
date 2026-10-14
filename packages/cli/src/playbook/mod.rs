//! Playbook system for headless scripted bmux execution.
//!
//! Supports two input formats:
//! - TOML playbook files (`.playbook.toml`)
//! - Line-oriented DSL (pipeable from stdin)
//!
//! Both formats parse into the same internal representation and are executed
//! by the same engine against an ephemeral sandbox server (or a live server).

pub use bmux_playbook::{diff, parse_dsl, parse_toml, subst, types};
pub mod display_track;
pub mod engine;
pub mod interactive;
mod managed;
pub mod sandbox;
pub mod screen;

use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::Result;

use self::engine::run_playbook;
use self::types::{Playbook, PlaybookResult};

pub(crate) fn report_managed_prelaunch_failure(error: &anyhow::Error) -> Result<()> {
    if let Some(control) = managed::ManagedRun::from_env()? {
        control.publish_failure(&format!("{error:#}"))?;
    }
    Ok(())
}

/// Execution options for playbook runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    pub interactive: bool,
}

pub use bmux_playbook::parser::{parse_file, parse_stdin};

/// Run a playbook and return the result.
/// # Errors
///
/// Returns an error if the playbook execution fails.
pub async fn run(playbook: Playbook, target_server: bool) -> Result<PlaybookResult> {
    run_with_options(playbook, target_server, RunOptions::default()).await
}

/// Run a playbook with explicit execution options.
/// # Errors
///
/// Returns an error if the playbook execution fails.
pub async fn run_with_options(
    playbook: Playbook,
    target_server: bool,
    options: RunOptions,
) -> Result<PlaybookResult> {
    run_playbook(playbook, target_server, options).await
}

/// Validate a playbook without executing it.
///
/// If `target_server` is true, the first-step `new-session` check is skipped
/// since the user may be targeting an existing session on a live server.
#[must_use]
pub fn validate(playbook: &Playbook, target_server: bool) -> Vec<String> {
    let mut errors = Vec::new();

    if playbook.steps.is_empty() {
        errors.push("playbook has no steps".to_string());
    }

    // Check that the first meaningful action is new-session (unless targeting live server)
    if !target_server && !matches!(playbook.config.driver, types::PlaybookDriver::AttachSim) {
        let first_action = playbook.steps.first().map(|s| &s.action);
        if let Some(action) = first_action
            && !matches!(action, types::Action::NewSession { .. })
        {
            errors.push(
                "first step should be 'new-session' (no session exists at start)".to_string(),
            );
        }
    }

    // Check viewport dimensions
    if playbook.config.viewport.cols < 10 || playbook.config.viewport.rows < 5 {
        errors.push("viewport too small (minimum 10x5)".to_string());
    }

    // Track expected pane count for index validation
    let mut expected_pane_count: u32 = 0;
    let mut render_marks = BTreeSet::new();

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
            types::Action::RenderMark { id } => {
                render_marks.insert(id.clone());
            }
            types::Action::AssertRender { since, assertion } => {
                validate_render_assertion_step(
                    &mut errors,
                    step.index,
                    playbook.config.render_trace,
                    &render_marks,
                    since,
                    assertion,
                );
            }

            // Validate regex patterns
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
            types::Action::AssertScreen {
                matches: Some(pattern),
                ..
            } => {
                if let Err(e) = regex::Regex::new(pattern) {
                    errors.push(format!(
                        "step {}: invalid regex pattern '{}': {}",
                        step.index, pattern, e
                    ));
                }
            }

            // Validate event names
            types::Action::WaitForEvent { event, timeout } => {
                validate_wait_for_event_step(&mut errors, step.index, event, *timeout);
            }

            // Warn if targeting a pane before any split
            types::Action::SendKeys {
                pane: Some(idx), ..
            }
            | types::Action::FocusPane { target: idx }
                if expected_pane_count < 2 && *idx > 1 =>
            {
                errors.push(format!(
                    "step {}: targets pane {} but only {} pane(s) expected at this point",
                    step.index, idx, expected_pane_count
                ));
            }

            _ => {}
        }
    }

    errors
}

fn validate_wait_for_event_step(
    errors: &mut Vec<String>,
    step_index: usize,
    event: &str,
    timeout: std::time::Duration,
) {
    if timeout.is_zero() {
        errors.push(format!(
            "step {step_index}: wait-for-event has zero timeout"
        ));
    }
    let valid_events = [
        "server_started",
        "server_stopping",
        "session_created",
        "session_removed",
        "client_attached",
        "client_detached",
        "attach_view_changed",
    ];
    if !valid_events.contains(&event) {
        errors.push(format!(
            "step {step_index}: unknown event '{event}'; valid events: {}",
            valid_events.join(", ")
        ));
    }
}

fn validate_render_assertion_step(
    errors: &mut Vec<String>,
    step_index: usize,
    render_trace: bool,
    render_marks: &BTreeSet<String>,
    since: &str,
    assertion: &types::RenderAssertion,
) {
    if !render_trace {
        errors.push(format!(
            "step {step_index}: assert-render requires @render-trace true"
        ));
    }
    if !render_marks.contains(since) {
        errors.push(format!(
            "step {step_index}: assert-render references unknown mark '{since}'"
        ));
    }
    if let (Some(min), Some(max)) = (assertion.min_frames, assertion.max_frames)
        && min > max
    {
        errors.push(format!(
            "step {step_index}: assert-render min_frames exceeds max_frames"
        ));
    }
}

/// Format a `PlaybookResult` as human-readable output.
#[must_use]
pub fn format_result(result: &PlaybookResult) -> String {
    let mut out = String::new();

    let name = result.playbook_name.as_deref().unwrap_or("<unnamed>");
    let status = if result.pass { "PASS" } else { "FAIL" };
    let _ = writeln!(
        out,
        "playbook: {name} — {status} ({} ms)",
        result.total_elapsed_ms
    );
    out.push('\n');

    for step in &result.steps {
        let icon = match (step.status, step.continue_on_error) {
            (types::StepStatus::Pass, _) => "+",
            (types::StepStatus::Fail, true) => "~", // failed but continued
            (types::StepStatus::Fail, false) => "x",
            (types::StepStatus::Skip, _) => "-",
        };
        write!(
            out,
            "  [{icon}] step {}: {} ({} ms)",
            step.index, step.action, step.elapsed_ms
        )
        .unwrap();
        if let Some(detail) = &step.detail {
            write!(out, " — {detail}").unwrap();
        }
        out.push('\n');
    }

    if !result.snapshots.is_empty() {
        let _ = writeln!(out, "\nsnapshots: {}", result.snapshots.len());
        for snap in &result.snapshots {
            let _ = writeln!(out, "  - {} ({} panes)", snap.id, snap.panes.len());
        }
    }

    if let Some(rid) = &result.recording_id {
        let _ = writeln!(out, "\nrecording: {rid}");
    }

    if let Some(err) = &result.error {
        let _ = writeln!(out, "\nerror: {err}");
    }

    out
}
