//! The store as plain text, for CI and terminals without the dashboard.
//!
//! It prints a periodic summary line, and each failure group as it appears. It reads the same [`Store`] the dashboard
//! draws, so neither knows something the other does not.

use std::fmt::Write as _;
use std::time::Instant;

use crate::store::{FailureGroup, Store};

/// One line summing up the fleet: runs, failures, throughput, coverage.
#[must_use]
pub fn summary(store: &Store, now: Instant) -> String {
    let elapsed = now.duration_since(store.started).as_secs();
    let mut line = format!(
        "[{:02}:{:02}:{:02}] {} runs ({} passed, {} failed",
        elapsed / 3600,
        elapsed / 60 % 60,
        elapsed % 60,
        store.completed(),
        store.totals.passed,
        store.totals.failed,
    );
    if !store.failures.is_empty() {
        let _ = write!(line, " in {} groups", store.failures.len());
    }
    line.push(')');
    if let Some(rate) = store.runs_per_second.last() {
        let _ = write!(line, ", {rate} runs/s");
    }
    if let Some(rate) = store.decisions_per_second.last() {
        let _ = write!(line, ", {} decisions/s", human(rate));
    }
    let _ = write!(line, ", {} distinct traces", store.distinct_traces);
    line
}

/// A failure group, as lines: what failed, how often, where it stands, how to replay it.
#[must_use]
pub fn failure(group: &FailureGroup) -> Vec<String> {
    let mut lines = vec![format!("FAILED x{}: {}", group.count, group.signature)];
    if !group.shape.is_empty() {
        lines.push(format!("  shape: {}", group.shape.join(" ")));
    }
    match group.reproduced {
        Some(true) => lines.push("  reproduced by a replay".into()),
        Some(false) => lines.push("  did NOT reproduce in a replay (nondeterminism?)".into()),
        None => {}
    }
    if let Some(shrink) = group.shrink.as_ref().filter(|s| s.done) {
        lines.push(format!(
            "  shrunk from {} to {} scheduling choices",
            shrink.original, shrink.best
        ));
    }
    if let Some(command) = group.replay_command() {
        lines.push(format!("  replay: {command}"));
    } else {
        lines.push(format!(
            "  replay: SIMULATOR_SEED={} SIMULATOR_EPOCH_OFFSET={} SIMULATOR_RUNS=1",
            group.seed, group.epoch_offset
        ));
    }
    lines
}

/// A count for people: `41.2M`, `3.4k`, `812`.
#[must_use]
pub fn human(value: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let value_f = value as f64;
    if value >= 1_000_000_000 {
        format!("{:.1}G", value_f / 1e9)
    } else if value >= 1_000_000 {
        format!("{:.1}M", value_f / 1e6)
    } else if value >= 10_000 {
        format!("{:.1}k", value_f / 1e3)
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_at_a_glance() {
        assert_eq!(human(812), "812");
        assert_eq!(human(34_000), "34.0k");
        assert_eq!(human(41_200_000), "41.2M");
    }

    #[test]
    fn a_summary_names_runs_failures_and_traces() {
        let store = Store::new(Instant::now());
        let line = summary(&store, Instant::now());
        assert!(line.contains("0 runs (0 passed, 0 failed)"), "{line}");
        assert!(line.contains("0 distinct traces"), "{line}");
    }
}
