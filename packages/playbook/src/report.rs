use crate::types::{self, PlaybookResult};
use std::fmt::Write as _;
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
