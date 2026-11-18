//! Reproducible benchmark metrics: what each phase of a frame allocates and executes, and what
//! a frame writes, recorded per scenario, reported as JSON and compared against a baseline.
//!
//! A benchmark measures each phase of each frame with a [`Probe`], keeps the samples in a
//! [`PhaseRecorder`] per phase, and reports a [`Scenario`] per workload. Allocation counts and
//! bytes written do not depend on machine load or speed, so a [`Report`] compared against a
//! committed baseline ([`Report::regressions`]) fails on any increase. Instructions retired are
//! near-exact too once each frame keeps its fewest across repeats (interrupts and other noise only
//! add), and gate within 2% (what code generation moves them) where both reports counted them.

use std::collections::BTreeMap;
use std::path::Path;

use crate::allocations::{self, Allocations};
use crate::instructions::InstructionCounter;

/// Measures one piece of work: what it allocated on this thread and the instructions it retired.
pub struct Probe {
    counter: Option<InstructionCounter>,
}

impl Default for Probe {
    fn default() -> Self {
        Self::new()
    }
}

impl Probe {
    /// A probe counting instructions where the platform can.
    ///
    /// # Panics
    /// Panics when the benchmark binary does not install
    /// [`crate::allocations::CountingAllocator`]: its counts would all be zero.
    #[must_use]
    pub fn new() -> Self {
        assert!(
            allocations::installed(),
            "install bmux_bench::allocations::CountingAllocator as the global allocator"
        );
        Self {
            counter: InstructionCounter::new(),
        }
    }

    /// Run `work`, returning its value and what it cost.
    pub fn measure<T>(&self, work: impl FnOnce() -> T) -> (T, Cost) {
        let mark = self.mark();
        let value = work();
        (value, self.since(mark))
    }

    /// Where the work about to be measured starts.
    #[must_use]
    pub fn mark(&self) -> Mark {
        Mark {
            instructions: self.counter.as_ref().map(InstructionCounter::read),
            allocations: allocations::current(),
        }
    }

    /// What the work since `mark` cost.
    #[must_use]
    pub fn since(&self, mark: Mark) -> Cost {
        let allocations = allocations::current().since(mark.allocations);
        let instructions = self
            .counter
            .as_ref()
            .zip(mark.instructions)
            .map(|(counter, before)| counter.read().saturating_sub(before));
        Cost {
            allocations,
            instructions,
        }
    }
}

/// Where a measurement starts ([`Probe::mark`]).
#[derive(Clone, Copy, Debug)]
pub struct Mark {
    allocations: Allocations,
    instructions: Option<u64>,
}

/// What one piece of work cost.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cost {
    /// Allocated on the measuring thread.
    pub allocations: Allocations,
    /// Instructions retired, where there is a counter.
    pub instructions: Option<u64>,
}

/// Per-frame costs of one phase of a scenario.
#[derive(Debug, Default)]
pub struct PhaseRecorder {
    allocations: Vec<u64>,
    bytes: u64,
    /// Each frame's instructions, while every frame had a count.
    instructions: Option<Vec<u64>>,
}

impl PhaseRecorder {
    /// Record one frame's cost of this phase.
    pub fn record(&mut self, cost: Cost) {
        let first = self.allocations.is_empty();
        self.allocations.push(cost.allocations.count);
        self.bytes += cost.allocations.bytes;
        match (cost.instructions, &mut self.instructions) {
            (Some(count), None) if first => self.instructions = Some(vec![count]),
            (Some(count), Some(counts)) => counts.push(count),
            _ => self.instructions = None,
        }
    }

    /// Fold in a repeat of the same frames: their allocations must be identical (the workload is
    /// deterministic), and each frame keeps its fewest instructions, which strips what interrupts
    /// and other noise added (they only ever add).
    ///
    /// # Errors
    /// Returns why the repeat is not the same work: a different number of frames, or different
    /// allocations.
    pub fn fold_repeat(&mut self, repeat: &Self) -> Result<(), String> {
        if self.allocations != repeat.allocations || self.bytes != repeat.bytes {
            let frame = self
                .allocations
                .iter()
                .zip(&repeat.allocations)
                .position(|(a, b)| a != b);
            return Err(format!(
                "allocations differ between repeats ({} vs {} frames, first difference at frame \
                 {frame:?}): the workload is not deterministic",
                self.allocations.len(),
                repeat.allocations.len()
            ));
        }
        self.instructions = match (self.instructions.take(), &repeat.instructions) {
            (Some(mut counts), Some(other)) => {
                for (count, other) in counts.iter_mut().zip(other) {
                    *count = (*count).min(*other);
                }
                Some(counts)
            }
            _ => None,
        };
        Ok(())
    }

    /// The phase's metrics over the frames recorded.
    #[must_use]
    pub fn finish(&self) -> PhaseMetrics {
        let frames = self.allocations.len().max(1) as u64;
        let total: u64 = self.allocations.iter().sum();
        PhaseMetrics {
            allocations_per_frame: per_frame(total, frames),
            max_allocations: self.allocations.iter().copied().max().unwrap_or(0),
            allocated_bytes_per_frame: self.bytes / frames,
            instructions_per_frame: self
                .instructions
                .as_ref()
                .map(|counts| counts.iter().sum::<u64>() / frames),
        }
    }
}

/// How many times each scenario runs (`BMUX_BENCH_REPEATS`, 3 by default): instructions are each
/// frame's fewest across the repeats, and the repeats must allocate identically.
#[must_use]
pub fn repeats() -> usize {
    std::env::var("BMUX_BENCH_REPEATS")
        .ok()
        .and_then(|repeats| repeats.parse().ok())
        .unwrap_or(3)
        .max(1)
}

/// Whether scenario `name` runs: every scenario, unless `BMUX_BENCH_ONLY` lists the names to run
/// (comma-separated), as a profiler running one scenario per process sets it.
#[must_use]
pub fn selected(name: &str) -> bool {
    std::env::var("BMUX_BENCH_ONLY").map_or(true, |only| {
        only.split(',').any(|selected| selected.trim() == name)
    })
}

/// `total / frames`, to two decimal places (what a report keeps).
fn per_frame(total: u64, frames: u64) -> f64 {
    // Exact for any count a benchmark reaches (below 2^53 hundredths).
    let hundredths = total.saturating_mul(100) / frames.max(1);
    hundredths as f64 / 100.0
}

/// One phase's costs, per frame.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PhaseMetrics {
    /// Allocation requests per frame, on average.
    pub allocations_per_frame: f64,
    /// Most allocation requests one frame made.
    pub max_allocations: u64,
    /// Bytes allocation requests asked for, per frame.
    pub allocated_bytes_per_frame: u64,
    /// Instructions retired per frame, where there was a counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_per_frame: Option<u64>,
}

/// One workload's metrics.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Scenario {
    /// The workload, unique within a report.
    pub name: String,
    /// Frames measured (after warm-up).
    pub frames: u64,
    /// Each phase of a frame, by name.
    pub phases: BTreeMap<String, PhaseMetrics>,
    /// Bytes written to the host terminal per frame.
    pub output_bytes_per_frame: u64,
    /// Bytes still allocated after the last frame beyond those after warm-up: what the workload
    /// keeps growing (caches, history), which a steady workload should not.
    pub live_growth_bytes: i64,
}

/// A benchmark's scenarios.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    /// Its scenarios, in the order run.
    pub scenarios: Vec<Scenario>,
}

/// How much a gated number may exceed its baseline: platforms' standard libraries differ a little
/// in what they allocate, so a count may move by this fraction (or one unit) before it is a
/// regression.
const TOLERANCE: f64 = 0.01;

impl Report {
    /// Read a report written by [`Report::write`].
    ///
    /// # Errors
    /// Returns an error when the file cannot be read or is not a report.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
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

    /// Every gated number that grew past `baseline` (allocations and allocated bytes per frame
    /// of each phase, bytes written per frame), and every baseline scenario or phase this report
    /// lacks, as messages. Scenarios the baseline lacks are new and not compared.
    #[must_use]
    pub fn regressions(&self, baseline: &Self) -> Vec<String> {
        let mut regressions = Vec::new();
        for expected in &baseline.scenarios {
            let Some(actual) = self.scenarios.iter().find(|s| s.name == expected.name) else {
                regressions.push(format!("{}: scenario missing", expected.name));
                continue;
            };
            regressions.extend(compare_scenario(actual, expected));
        }
        regressions
    }

    /// Finish a benchmark run: write the report where `BMUX_BENCH_JSON` says, and compare it
    /// with the report `BMUX_BENCH_BASELINE` names, printing improvements.
    ///
    /// # Panics
    /// Panics when the report cannot be written, the baseline cannot be read, or any gated
    /// number regressed against it.
    pub fn write_and_check(&self) {
        if let Ok(path) = std::env::var("BMUX_BENCH_JSON") {
            self.write(Path::new(&path)).expect("write the report");
        }
        if let Ok(path) = std::env::var("BMUX_BENCH_BASELINE") {
            let baseline = Self::read(Path::new(&path)).expect("read the baseline");
            for improvement in self.improvements(&baseline) {
                println!("improved (the baseline can be lowered): {improvement}");
            }
            let regressions = self.regressions(&baseline);
            assert!(
                regressions.is_empty(),
                "regressions against {path}:\n{}",
                regressions.join("\n")
            );
        }
    }

    /// Scenarios whose gated numbers dropped well below `baseline`: the baseline can be lowered
    /// to lock the improvement in.
    #[must_use]
    pub fn improvements(&self, baseline: &Self) -> Vec<String> {
        let mut improvements = Vec::new();
        for expected in &baseline.scenarios {
            let Some(actual) = self.scenarios.iter().find(|s| s.name == expected.name) else {
                continue;
            };
            for (phase, before) in &expected.phases {
                let Some(after) = actual.phases.get(phase) else {
                    continue;
                };
                if grew(before.allocations_per_frame, after.allocations_per_frame) {
                    improvements.push(format!(
                        "{} {phase}: allocations per frame {} -> {}",
                        expected.name, before.allocations_per_frame, after.allocations_per_frame
                    ));
                }
                if let (Some(before), Some(after)) =
                    (before.instructions_per_frame, after.instructions_per_frame)
                    && (after as f64).mul_add(1.0 + INSTRUCTION_TOLERANCE, INSTRUCTION_SLACK)
                        < before as f64
                {
                    improvements.push(format!(
                        "{} {phase}: instructions per frame {before} -> {after}",
                        expected.name
                    ));
                }
            }
        }
        improvements
    }
}

/// Whether `actual` grew past `expected` by more than the tolerance.
fn grew(actual: f64, expected: f64) -> bool {
    actual > expected.mul_add(1.0 + TOLERANCE, 1.0)
}

/// How much instructions per frame may exceed their baseline. They repeat to within a few
/// hundredths of a percent once each frame keeps its fewest across repeats, but code generation
/// moves them more: generic code is compiled into each crate using it, and with one codegen unit
/// a change anywhere in that crate can shift inlining in unrelated code by about a percent. Units
/// of work (`crate::work`) gate the work itself exactly; this gates what it costs.
const INSTRUCTION_TOLERANCE: f64 = 0.02;
/// Instructions per frame any phase may grow by, however small: a phase that does almost nothing
/// (a frame deferred, say) varies by a few hundred instructions between runs.
const INSTRUCTION_SLACK: f64 = 1000.0;

/// Whether instruction counts gate (`BMUX_BENCH_GATE_INSTRUCTIONS=0` turns it off, for a machine
/// whose counts legitimately differ from the baseline's).
fn instructions_gate() -> bool {
    std::env::var("BMUX_BENCH_GATE_INSTRUCTIONS").map_or(true, |gate| gate != "0")
}

/// The gated numbers of `actual` that grew past `expected`'s, as messages.
fn compare_scenario(actual: &Scenario, expected: &Scenario) -> Vec<String> {
    let name = &expected.name;
    let mut regressions = Vec::new();
    let mut checks = vec![(
        "output bytes per frame".to_owned(),
        actual.output_bytes_per_frame as f64,
        expected.output_bytes_per_frame as f64,
    )];
    for (phase, before) in &expected.phases {
        let Some(after) = actual.phases.get(phase) else {
            regressions.push(format!("{name}: phase {phase} missing"));
            continue;
        };
        checks.push((
            format!("{phase} allocations per frame"),
            after.allocations_per_frame,
            before.allocations_per_frame,
        ));
        checks.push((
            format!("{phase} allocated bytes per frame"),
            after.allocated_bytes_per_frame as f64,
            before.allocated_bytes_per_frame as f64,
        ));
        // Compared only when both reports counted them (a machine without a counter has none).
        if let (Some(after), Some(before)) =
            (after.instructions_per_frame, before.instructions_per_frame)
            && instructions_gate()
            && after as f64
                > (before as f64).mul_add(1.0 + INSTRUCTION_TOLERANCE, INSTRUCTION_SLACK)
        {
            regressions.push(format!(
                "{name}: {phase} instructions per frame grew from {before} to {after}"
            ));
        }
    }
    regressions.extend(
        checks
            .into_iter()
            .filter(|(_, actual, expected)| grew(*actual, *expected))
            .map(|(what, actual, expected)| {
                format!("{name}: {what} grew from {expected} to {actual}")
            }),
    );
    regressions
}

#[cfg(test)]
mod tests {
    use super::{PhaseMetrics, Report, Scenario};

    fn report(allocations: f64, output: u64) -> Report {
        Report {
            scenarios: vec![Scenario {
                name: "stream".into(),
                frames: 10,
                phases: [(
                    "render".to_owned(),
                    PhaseMetrics {
                        allocations_per_frame: allocations,
                        max_allocations: 0,
                        allocated_bytes_per_frame: 100,
                        instructions_per_frame: None,
                    },
                )]
                .into(),
                output_bytes_per_frame: output,
                live_growth_bytes: 0,
            }],
        }
    }

    #[test]
    fn repeats_keep_each_frames_fewest_instructions_and_must_allocate_alike() {
        use super::{Cost, PhaseRecorder};
        use crate::allocations::Allocations;
        let frame = |count, instructions| Cost {
            allocations: Allocations { count, bytes: 8 },
            instructions: Some(instructions),
        };
        let mut first = PhaseRecorder::default();
        first.record(frame(2, 100));
        first.record(frame(3, 205));
        let mut repeat = PhaseRecorder::default();
        repeat.record(frame(2, 104));
        repeat.record(frame(3, 200));
        first.fold_repeat(&repeat).unwrap();
        assert_eq!(first.finish().instructions_per_frame, Some(150));
        let mut different = PhaseRecorder::default();
        different.record(frame(2, 100));
        different.record(frame(4, 200));
        assert!(first.fold_repeat(&different).is_err());
    }

    #[test]
    fn growth_past_the_tolerance_is_a_regression_and_a_drop_an_improvement() {
        let baseline = report(100.0, 1000);
        assert!(report(101.0, 1000).regressions(&baseline).is_empty());
        assert_eq!(
            report(103.0, 1000).regressions(&baseline),
            ["stream: render allocations per frame grew from 100 to 103"]
        );
        assert_eq!(report(100.0, 1100).regressions(&baseline).len(), 1);
        assert_eq!(
            Report::default().regressions(&baseline),
            ["stream: scenario missing"]
        );
        assert!(report(100.0, 1000).improvements(&baseline).is_empty());
        assert_eq!(report(40.0, 1000).improvements(&baseline).len(), 1);
    }
}
