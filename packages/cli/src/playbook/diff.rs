//! Playbook result diffing.
//!
//! Compares two `PlaybookResult` JSON outputs to produce a structured diff
//! report covering step status changes, screen text differences (via Myers
//! diff algorithm), timing comparison, and failure capture comparison.
//!
//! See `docs/playbooks.md` for the full reference.

use serde::Serialize;
use similar::TextDiff;

use super::types::{PaneCapture, PlaybookResult, StepStatus};

// ---------------------------------------------------------------------------
// Diff report types
// ---------------------------------------------------------------------------

/// Complete diff report comparing two playbook results.
#[derive(Debug, Serialize)]
pub struct DiffReport {
    pub summary: DiffSummary,
    pub step_diffs: Vec<StepDiff>,
    pub snapshot_diffs: Vec<SnapshotDiff>,
    pub failure_capture_diffs: Vec<FailureCaptureDiff>,
    pub timing_regressions: Vec<TimingRegression>,
}

/// High-level summary of the diff.
#[derive(Debug, Serialize)]
pub struct DiffSummary {
    pub left_pass: bool,
    pub right_pass: bool,
    pub outcome_changed: bool,
    pub left_step_count: usize,
    pub right_step_count: usize,
    pub left_total_ms: u64,
    pub right_total_ms: u64,
    pub timing_delta_ms: i64,
    pub timing_delta_pct: f64,
    pub steps_changed: usize,
    pub steps_added: usize,
    pub steps_removed: usize,
    pub snapshots_changed: usize,
}

/// Per-step diff entry.
#[derive(Debug, Serialize)]
pub struct StepDiff {
    pub index: usize,
    pub action: String,
    pub status_changed: bool,
    pub left_status: StepStatus,
    pub right_status: StepStatus,
    pub left_ms: u64,
    pub right_ms: u64,
    pub timing_delta_ms: i64,
    pub timing_delta_pct: f64,
    pub timing_regression: bool,
    pub detail_changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_expected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_actual: Option<String>,
}

/// Diff of a named snapshot across two runs.
#[derive(Debug, Serialize)]
pub struct SnapshotDiff {
    pub id: String,
    pub present_left: bool,
    pub present_right: bool,
    pub pane_diffs: Vec<PaneDiff>,
}

/// Diff of failure captures between two runs of the same step.
#[derive(Debug, Serialize)]
pub struct FailureCaptureDiff {
    pub step_index: usize,
    pub action: String,
    pub pane_diffs: Vec<PaneDiff>,
}

/// Diff of a single pane's screen text.
#[derive(Debug, Serialize)]
pub struct PaneDiff {
    pub pane_index: u32,
    pub text_changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_diff: Option<String>,
}

/// A step where timing regressed beyond the threshold.
#[derive(Debug, Serialize)]
pub struct TimingRegression {
    pub index: usize,
    pub action: String,
    pub left_ms: u64,
    pub right_ms: u64,
    pub delta_pct: f64,
}

// ---------------------------------------------------------------------------
// Core diff logic
// ---------------------------------------------------------------------------

/// Compare two playbook results and produce a structured diff report.
pub fn diff_results(
    left: &PlaybookResult,
    right: &PlaybookResult,
    timing_threshold_pct: f64,
) -> DiffReport {
    let mut step_diffs = Vec::new();
    let mut steps_changed: usize = 0;
    let mut timing_regressions = Vec::new();

    let max_steps = left.steps.len().max(right.steps.len());
    let steps_added = right.steps.len().saturating_sub(left.steps.len());
    let steps_removed = left.steps.len().saturating_sub(right.steps.len());

    for i in 0..max_steps {
        let left_step = left.steps.get(i);
        let right_step = right.steps.get(i);

        match (left_step, right_step) {
            (Some(ls), Some(rs)) => {
                let status_changed = ls.status != rs.status;
                let detail_changed = ls.detail != rs.detail;
                if status_changed {
                    steps_changed += 1;
                }

                let left_ms = ls.elapsed_ms;
                let right_ms = rs.elapsed_ms;
                let delta_ms = right_ms as i64 - left_ms as i64;
                let delta_pct = if left_ms > 0 {
                    (delta_ms as f64 / left_ms as f64) * 100.0
                } else if right_ms > 0 {
                    100.0
                } else {
                    0.0
                };
                let timing_regression = delta_pct > timing_threshold_pct && left_ms > 50; // ignore tiny steps

                if timing_regression {
                    timing_regressions.push(TimingRegression {
                        index: i,
                        action: rs.action.clone(),
                        left_ms,
                        right_ms,
                        delta_pct,
                    });
                }

                let mut diff = StepDiff {
                    index: i,
                    action: rs.action.clone(),
                    status_changed,
                    left_status: ls.status,
                    right_status: rs.status,
                    left_ms,
                    right_ms,
                    timing_delta_ms: delta_ms,
                    timing_delta_pct: delta_pct,
                    timing_regression,
                    detail_changed,
                    left_detail: None,
                    right_detail: None,
                    right_expected: None,
                    right_actual: None,
                };

                // Include details when something changed.
                if status_changed || detail_changed {
                    diff.left_detail = ls.detail.clone();
                    diff.right_detail = rs.detail.clone();
                    diff.right_expected = rs.expected.clone();
                    diff.right_actual = rs.actual.clone();
                }

                step_diffs.push(diff);
            }
            (Some(ls), None) => {
                // Step only in left (removed).
                step_diffs.push(StepDiff {
                    index: i,
                    action: ls.action.clone(),
                    status_changed: true,
                    left_status: ls.status,
                    right_status: StepStatus::Skip, // placeholder
                    left_ms: ls.elapsed_ms,
                    right_ms: 0,
                    timing_delta_ms: -(ls.elapsed_ms as i64),
                    timing_delta_pct: -100.0,
                    timing_regression: false,
                    detail_changed: true,
                    left_detail: ls.detail.clone(),
                    right_detail: Some("(step removed)".to_string()),
                    right_expected: None,
                    right_actual: None,
                });
            }
            (None, Some(rs)) => {
                // Step only in right (added).
                step_diffs.push(StepDiff {
                    index: i,
                    action: rs.action.clone(),
                    status_changed: true,
                    left_status: StepStatus::Skip, // placeholder
                    right_status: rs.status,
                    left_ms: 0,
                    right_ms: rs.elapsed_ms,
                    timing_delta_ms: rs.elapsed_ms as i64,
                    timing_delta_pct: 100.0,
                    timing_regression: false,
                    detail_changed: true,
                    left_detail: Some("(step added)".to_string()),
                    right_detail: rs.detail.clone(),
                    right_expected: rs.expected.clone(),
                    right_actual: rs.actual.clone(),
                });
            }
            (None, None) => unreachable!(),
        }
    }

    // Diff snapshots by matching on ID.
    let snapshot_diffs = diff_snapshots(&left.snapshots, &right.snapshots);
    let snapshots_changed = snapshot_diffs
        .iter()
        .filter(|sd| {
            sd.pane_diffs.iter().any(|pd| pd.text_changed) || sd.present_left != sd.present_right
        })
        .count();

    // Diff failure captures from failed steps.
    let failure_capture_diffs = diff_failure_captures(&left.steps, &right.steps);

    let left_total = left.total_elapsed_ms;
    let right_total = right.total_elapsed_ms;
    let total_delta = right_total as i64 - left_total as i64;
    let total_delta_pct = if left_total > 0 {
        (total_delta as f64 / left_total as f64) * 100.0
    } else {
        0.0
    };

    DiffReport {
        summary: DiffSummary {
            left_pass: left.pass,
            right_pass: right.pass,
            outcome_changed: left.pass != right.pass,
            left_step_count: left.steps.len(),
            right_step_count: right.steps.len(),
            left_total_ms: left_total,
            right_total_ms: right_total,
            timing_delta_ms: total_delta,
            timing_delta_pct: total_delta_pct,
            steps_changed,
            steps_added,
            steps_removed,
            snapshots_changed,
        },
        step_diffs,
        snapshot_diffs,
        failure_capture_diffs,
        timing_regressions,
    }
}

// ---------------------------------------------------------------------------
// Snapshot diffing
// ---------------------------------------------------------------------------

fn diff_snapshots(
    left: &[super::types::SnapshotCapture],
    right: &[super::types::SnapshotCapture],
) -> Vec<SnapshotDiff> {
    let mut diffs = Vec::new();

    // Collect all snapshot IDs from both sides.
    let mut all_ids: Vec<String> = left.iter().map(|s| s.id.clone()).collect();
    for rs in right {
        if !all_ids.contains(&rs.id) {
            all_ids.push(rs.id.clone());
        }
    }

    for id in &all_ids {
        let ls = left.iter().find(|s| &s.id == id);
        let rs = right.iter().find(|s| &s.id == id);

        let pane_diffs = match (ls, rs) {
            (Some(l), Some(r)) => diff_pane_captures(&l.panes, &r.panes),
            (Some(l), None) => l
                .panes
                .iter()
                .map(|p| PaneDiff {
                    pane_index: p.index,
                    text_changed: true,
                    left_text: Some(p.screen_text.clone()),
                    right_text: None,
                    unified_diff: None,
                })
                .collect(),
            (None, Some(r)) => r
                .panes
                .iter()
                .map(|p| PaneDiff {
                    pane_index: p.index,
                    text_changed: true,
                    left_text: None,
                    right_text: Some(p.screen_text.clone()),
                    unified_diff: None,
                })
                .collect(),
            (None, None) => continue,
        };

        diffs.push(SnapshotDiff {
            id: id.clone(),
            present_left: ls.is_some(),
            present_right: rs.is_some(),
            pane_diffs,
        });
    }

    diffs
}

fn diff_failure_captures(
    left_steps: &[super::types::StepResult],
    right_steps: &[super::types::StepResult],
) -> Vec<FailureCaptureDiff> {
    let mut diffs = Vec::new();

    let max_steps = left_steps.len().max(right_steps.len());
    for i in 0..max_steps {
        let ls = left_steps.get(i);
        let rs = right_steps.get(i);

        let left_caps = ls.and_then(|s| s.failure_captures.as_ref());
        let right_caps = rs.and_then(|s| s.failure_captures.as_ref());

        // Only diff if at least one side has failure captures.
        if left_caps.is_none() && right_caps.is_none() {
            continue;
        }

        let pane_diffs = match (left_caps, right_caps) {
            (Some(lc), Some(rc)) => diff_pane_captures(lc, rc),
            (Some(lc), None) => lc
                .iter()
                .map(|p| PaneDiff {
                    pane_index: p.index,
                    text_changed: true,
                    left_text: Some(p.screen_text.clone()),
                    right_text: None,
                    unified_diff: None,
                })
                .collect(),
            (None, Some(rc)) => rc
                .iter()
                .map(|p| PaneDiff {
                    pane_index: p.index,
                    text_changed: true,
                    left_text: None,
                    right_text: Some(p.screen_text.clone()),
                    unified_diff: None,
                })
                .collect(),
            (None, None) => continue,
        };

        let action = rs.or(ls).map(|s| s.action.clone()).unwrap_or_default();

        diffs.push(FailureCaptureDiff {
            step_index: i,
            action,
            pane_diffs,
        });
    }

    diffs
}

fn diff_pane_captures(left: &[PaneCapture], right: &[PaneCapture]) -> Vec<PaneDiff> {
    let mut diffs = Vec::new();

    // Collect all pane indices.
    let mut all_indices: Vec<u32> = left.iter().map(|p| p.index).collect();
    for rp in right {
        if !all_indices.contains(&rp.index) {
            all_indices.push(rp.index);
        }
    }
    all_indices.sort_unstable();

    for idx in all_indices {
        let lp = left.iter().find(|p| p.index == idx);
        let rp = right.iter().find(|p| p.index == idx);

        match (lp, rp) {
            (Some(l), Some(r)) => {
                let changed = l.screen_text != r.screen_text;
                let unified = if changed {
                    Some(unified_text_diff(&l.screen_text, &r.screen_text))
                } else {
                    None
                };
                diffs.push(PaneDiff {
                    pane_index: idx,
                    text_changed: changed,
                    left_text: if changed {
                        Some(l.screen_text.clone())
                    } else {
                        None
                    },
                    right_text: if changed {
                        Some(r.screen_text.clone())
                    } else {
                        None
                    },
                    unified_diff: unified,
                });
            }
            (Some(l), None) => {
                diffs.push(PaneDiff {
                    pane_index: idx,
                    text_changed: true,
                    left_text: Some(l.screen_text.clone()),
                    right_text: None,
                    unified_diff: None,
                });
            }
            (None, Some(r)) => {
                diffs.push(PaneDiff {
                    pane_index: idx,
                    text_changed: true,
                    left_text: None,
                    right_text: Some(r.screen_text.clone()),
                    unified_diff: None,
                });
            }
            (None, None) => {}
        }
    }

    diffs
}

// ---------------------------------------------------------------------------
// Text diff using similar crate
// ---------------------------------------------------------------------------

fn unified_text_diff(left: &str, right: &str) -> String {
    let diff = TextDiff::from_lines(left, right);
    let mut output = String::new();

    output.push_str("--- left\n");
    output.push_str("+++ right\n");

    for hunk in diff.unified_diff().context_radius(2).iter_hunks() {
        output.push_str(&format!("{hunk}"));
    }

    output
}

// ---------------------------------------------------------------------------
// Human-readable formatting
// ---------------------------------------------------------------------------

/// Format a diff report as human-readable text.
pub fn format_diff_report(report: &DiffReport, left_name: &str, right_name: &str) -> String {
    let mut out = String::new();
    let s = &report.summary;

    out.push_str(&format!("playbook diff: {left_name} → {right_name}\n\n"));

    // Outcome
    let left_status = if s.left_pass { "PASS" } else { "FAIL" };
    let right_status = if s.right_pass { "PASS" } else { "FAIL" };
    let changed = if s.outcome_changed { " (CHANGED)" } else { "" };
    out.push_str(&format!(
        "  outcome: {left_status} → {right_status}{changed}\n"
    ));
    out.push_str(&format!(
        "  total time: {}ms → {}ms ({:+}ms, {:+.1}%)\n",
        s.left_total_ms, s.right_total_ms, s.timing_delta_ms, s.timing_delta_pct
    ));
    out.push('\n');

    // Steps
    out.push_str("  steps:\n");
    for sd in &report.step_diffs {
        let icon = if sd.status_changed {
            "!"
        } else if sd.timing_regression {
            "~"
        } else {
            "="
        };
        let ls = format_status(sd.left_status);
        let rs = format_status(sd.right_status);
        let timing_warn = if sd.timing_regression {
            format!("  !! {:+.0}%", sd.timing_delta_pct)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "    [{icon}] {:>2}. {:<24} {ls} → {rs}  {}ms → {}ms{timing_warn}\n",
            sd.index, sd.action, sd.left_ms, sd.right_ms
        ));
        if sd.status_changed {
            if let Some(ref expected) = sd.right_expected {
                out.push_str(&format!("         expected: '{expected}'\n"));
            }
            if let Some(ref actual) = sd.right_actual {
                let truncated = if actual.len() > 120 {
                    format!("{}...", &actual[..120])
                } else {
                    actual.clone()
                };
                out.push_str(&format!("         actual:   '{truncated}'\n"));
            }
        }
    }

    // Snapshot diffs
    let changed_snapshots: Vec<_> = report
        .snapshot_diffs
        .iter()
        .filter(|sd| sd.pane_diffs.iter().any(|pd| pd.text_changed))
        .collect();
    if !changed_snapshots.is_empty() {
        out.push_str("\n  snapshots:\n");
        for sd in changed_snapshots {
            for pd in &sd.pane_diffs {
                if pd.text_changed {
                    out.push_str(&format!("    {} pane={}: CHANGED\n", sd.id, pd.pane_index));
                    if let Some(ref udiff) = pd.unified_diff {
                        for line in udiff.lines() {
                            out.push_str(&format!("      {line}\n"));
                        }
                    }
                }
            }
        }
    }

    // Failure capture diffs
    if !report.failure_capture_diffs.is_empty() {
        out.push_str("\n  failure captures:\n");
        for fcd in &report.failure_capture_diffs {
            for pd in &fcd.pane_diffs {
                if pd.text_changed {
                    out.push_str(&format!(
                        "    step {} ({}) pane={}: CHANGED\n",
                        fcd.step_index, fcd.action, pd.pane_index
                    ));
                    if let Some(ref udiff) = pd.unified_diff {
                        for line in udiff.lines() {
                            out.push_str(&format!("      {line}\n"));
                        }
                    }
                }
            }
        }
    }

    // Timing regressions
    if !report.timing_regressions.is_empty() {
        out.push_str(&format!(
            "\n  timing regressions (>{}%):\n",
            report.summary.timing_delta_pct.abs() as u64
        ));
        for tr in &report.timing_regressions {
            out.push_str(&format!(
                "    {}. {}: {}ms → {}ms ({:+.0}%)\n",
                tr.index, tr.action, tr.left_ms, tr.right_ms, tr.delta_pct
            ));
        }
    }

    out
}

fn format_status(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Pass => "pass",
        StepStatus::Fail => "FAIL",
        StepStatus::Skip => "skip",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playbook::types::{PlaybookResult, StepResult, StepStatus};

    fn make_result(pass: bool, steps: Vec<StepResult>) -> PlaybookResult {
        PlaybookResult {
            playbook_name: Some("test".to_string()),
            pass,
            steps,
            snapshots: vec![],
            recording_id: None,
            recording_path: None,
            total_elapsed_ms: 1000,
            error: None,
            sandbox_root: None,
        }
    }

    fn make_step(index: usize, action: &str, status: StepStatus, ms: u64) -> StepResult {
        StepResult {
            index,
            action: action.to_string(),
            status,
            elapsed_ms: ms,
            detail: None,
            expected: None,
            actual: None,
            failure_captures: None,
        }
    }

    #[test]
    fn diff_identical_results() {
        let result = make_result(
            true,
            vec![
                make_step(0, "new-session", StepStatus::Pass, 100),
                make_step(1, "send-keys", StepStatus::Pass, 5),
            ],
        );
        let report = diff_results(&result, &result, 50.0);
        assert!(!report.summary.outcome_changed);
        assert_eq!(report.summary.steps_changed, 0);
        assert_eq!(report.summary.steps_added, 0);
        assert_eq!(report.summary.steps_removed, 0);
        assert!(report.timing_regressions.is_empty());
    }

    #[test]
    fn diff_status_change() {
        let left = make_result(
            true,
            vec![
                make_step(0, "new-session", StepStatus::Pass, 100),
                make_step(1, "assert-screen", StepStatus::Pass, 10),
            ],
        );
        let right = make_result(
            false,
            vec![
                make_step(0, "new-session", StepStatus::Pass, 100),
                make_step(1, "assert-screen", StepStatus::Fail, 10),
            ],
        );
        let report = diff_results(&left, &right, 50.0);
        assert!(report.summary.outcome_changed);
        assert_eq!(report.summary.steps_changed, 1);
        let changed_step = report
            .step_diffs
            .iter()
            .find(|sd| sd.status_changed)
            .unwrap();
        assert_eq!(changed_step.index, 1);
        assert_eq!(changed_step.left_status, StepStatus::Pass);
        assert_eq!(changed_step.right_status, StepStatus::Fail);
    }

    #[test]
    fn diff_timing_regression() {
        let left = make_result(true, vec![make_step(0, "wait-for", StepStatus::Pass, 100)]);
        let right = make_result(true, vec![make_step(0, "wait-for", StepStatus::Pass, 2500)]);
        let report = diff_results(&left, &right, 50.0);
        assert_eq!(report.timing_regressions.len(), 1);
        assert_eq!(report.timing_regressions[0].left_ms, 100);
        assert_eq!(report.timing_regressions[0].right_ms, 2500);
    }

    #[test]
    fn unified_text_diff_basic() {
        let left = "line1\nline2\nline3\n";
        let right = "line1\nmodified\nline3\n";
        let diff = unified_text_diff(left, right);
        assert!(diff.contains("--- left"));
        assert!(diff.contains("+++ right"));
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+modified"));
    }
}
