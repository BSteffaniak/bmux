//! Deterministic work counts: how much code a workload executes, from a coverage-instrumented
//! build's execution counts (`-C instrument-coverage`, read with `llvm-profdata show --counts`).
//!
//! Every function entry and every counted region executed is one unit of work. Units repeat
//! exactly for the same code and input on any machine and OS, so they gate like allocations, and
//! the per-function units are a deterministic profile: which functions do the work, and how often
//! they run. They count executions, not instructions: a unit in a tight loop and a unit around a
//! large block weigh the same, so instructions retired stay the measure of CPU cost and units the
//! measure of what ran. `scripts/perf-render.py work` builds, runs and reads the profiles.

use std::collections::BTreeMap;
use std::path::Path;

/// One function's execution counts in a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionCounts {
    /// Demangled, without hashes; instances of one generic function share it.
    pub name: String,
    /// Times the function was entered.
    pub entries: u64,
    /// Entries plus every counted region executed: its units of work.
    pub units: u64,
}

/// The functions of `llvm-profdata show --all-functions --counts` output, demangled, instances of
/// one name summed.
#[must_use]
pub fn parse_profdata_show(text: &str) -> Vec<FunctionCounts> {
    let mut functions: BTreeMap<String, FunctionCounts> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut entries = 0_u64;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(symbol) = trimmed.strip_suffix(':')
            && !symbol.contains(' ')
            && line.starts_with("  ")
            && !line.starts_with("    ")
        {
            current = Some(format!("{:#}", rustc_demangle::demangle(symbol)));
        } else if let Some(count) = trimmed.strip_prefix("Function count: ") {
            entries = count.parse().unwrap_or(0);
        } else if let Some(blocks) = trimmed.strip_prefix("Block counts: ")
            && let Some(name) = current.take()
        {
            let regions: u64 = blocks
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .filter_map(|count| count.trim().parse::<u64>().ok())
                .sum();
            let function = functions.entry(name.clone()).or_insert(FunctionCounts {
                name,
                entries: 0,
                units: 0,
            });
            function.entries += entries;
            function.units += entries + regions;
            entries = 0;
        }
    }
    functions.into_values().collect()
}

/// Whether `function` is the workload's work rather than the benchmark measuring it: the
/// benchmark library and the benchmark modules are left out.
#[must_use]
pub fn is_workload(function: &str) -> bool {
    !(function.starts_with("bmux_bench::")
        || function.contains("::pipeline_bench::")
        || function.contains("::ingest_bench::")
        || function.starts_with("test::"))
}

/// One function's share of a scenario's work.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkFunction {
    pub name: String,
    pub units_per_frame: f64,
    pub entries_per_frame: f64,
}

/// One scenario's work.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkScenario {
    pub name: String,
    /// Frames the profiled run processed (warm-up included: units count the whole run).
    pub frames: u64,
    /// Units of work per frame, every workload function's together.
    pub units_per_frame: f64,
    /// The functions doing the most work, most first.
    pub functions: Vec<WorkFunction>,
}

/// `total / frames` to two decimal places.
fn per_frame(total: u64, frames: u64) -> f64 {
    (total.saturating_mul(100) / frames.max(1)) as f64 / 100.0
}

/// Summarize a profile of `frames` frames of scenario `name`, keeping the `top` busiest functions.
#[must_use]
pub fn summarize(
    name: &str,
    frames: u64,
    functions: &[FunctionCounts],
    top: usize,
) -> WorkScenario {
    let mut workload: Vec<&FunctionCounts> = functions
        .iter()
        .filter(|function| is_workload(&function.name))
        .collect();
    workload.sort_by(|a, b| b.units.cmp(&a.units).then_with(|| a.name.cmp(&b.name)));
    WorkScenario {
        name: name.to_owned(),
        frames,
        units_per_frame: per_frame(workload.iter().map(|function| function.units).sum(), frames),
        functions: workload
            .iter()
            .take(top)
            .map(|function| WorkFunction {
                name: function.name.clone(),
                units_per_frame: per_frame(function.units, frames),
                entries_per_frame: per_frame(function.entries, frames),
            })
            .collect(),
    }
}

/// A benchmark's work, per scenario.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkReport {
    pub scenarios: Vec<WorkScenario>,
}

/// How much units per frame may exceed their baseline. Units are exact for the same code; this
/// absorbs work that depends on hash or allocation order.
const TOLERANCE: f64 = 0.002;

impl WorkReport {
    /// Read a report written by [`WorkReport::write`].
    ///
    /// # Errors
    /// Returns an error when the file cannot be read or is not a work report.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        serde_json::from_str(&std::fs::read_to_string(path)?).map_err(std::io::Error::other)
    }

    /// Write the report as pretty JSON.
    ///
    /// # Errors
    /// Returns an error when the file cannot be written.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let mut text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        text.push('\n');
        std::fs::write(path, text)
    }

    /// Scenarios whose units per frame grew past `baseline`'s, or that this report lacks.
    #[must_use]
    pub fn regressions(&self, baseline: &Self) -> Vec<String> {
        baseline
            .scenarios
            .iter()
            .filter_map(|expected| {
                let Some(actual) = self.scenarios.iter().find(|s| s.name == expected.name) else {
                    return Some(format!("{}: scenario missing", expected.name));
                };
                (actual.units_per_frame > expected.units_per_frame * (1.0 + TOLERANCE)).then(|| {
                    format!(
                        "{}: units of work per frame grew from {} to {}",
                        expected.name, expected.units_per_frame, actual.units_per_frame
                    )
                })
            })
            .collect()
    }

    /// Scenarios whose units per frame dropped below `baseline`'s: the baseline can be lowered.
    #[must_use]
    pub fn improvements(&self, baseline: &Self) -> Vec<String> {
        baseline
            .scenarios
            .iter()
            .filter_map(|expected| {
                let actual = self.scenarios.iter().find(|s| s.name == expected.name)?;
                (actual.units_per_frame * (1.0 + TOLERANCE) < expected.units_per_frame).then(|| {
                    format!(
                        "{}: units of work per frame {} -> {}",
                        expected.name, expected.units_per_frame, actual.units_per_frame
                    )
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkReport, parse_profdata_show, summarize};

    const SHOW: &str = "Counters:
  _RNvNtCslAL0KtBQUQ9_8bmux_tui8terminal17write_painted_run:
    Hash: 0xe1fde7245fc3925b
    Counters: 6
    Function count: 10
    Block counts: [3, 0, 7, 0, 0]
  _RNvNtCscn56y61xVqE_10bmux_bench6corpus9su_scroll:
    Hash: 0xb2ef59fed0daa88f
    Counters: 3
    Function count: 3
    Block counts: [903, 3600]
  _RNvNtCslAL0KtBQUQ9_8bmux_tui4text4line:
    Hash: 0x1
    Counters: 1
    Function count: 4
    Block counts: []
Instrumentation level: Front-end
";

    #[test]
    fn profiles_become_demangled_workload_units_per_frame() {
        let functions = parse_profdata_show(SHOW);
        assert_eq!(functions.len(), 3);
        let scenario = summarize("stream", 2, &functions, 10);
        // The benchmark library's corpus is not the workload's work.
        assert_eq!(scenario.units_per_frame, (20.0 + 4.0) / 2.0);
        assert_eq!(
            scenario.functions[0].name,
            "bmux_tui::terminal::write_painted_run"
        );
        assert_eq!(scenario.functions[0].entries_per_frame, 5.0);
        let baseline = WorkReport {
            scenarios: vec![scenario.clone()],
        };
        let mut grown = scenario;
        grown.units_per_frame *= 1.01;
        let report = WorkReport {
            scenarios: vec![grown],
        };
        assert_eq!(report.regressions(&baseline).len(), 1);
        assert!(baseline.regressions(&baseline).is_empty());
        assert_eq!(baseline.improvements(&report).len(), 1);
    }
}
