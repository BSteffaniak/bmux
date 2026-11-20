use std::collections::BTreeSet;

use crate::types::{self, Playbook};

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
