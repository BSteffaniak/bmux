//! Everything the dashboard knows, folded from simvar's events.
//!
//! It holds the runs (live, recent, failed),
//! the fleet's totals and throughput, the invariants exercised, what the runs covered, and the
//! failures grouped by what failed.
//!
//! The store has no user interface; the dashboard draws it and the headless reporter
//! ([`crate::report`]) prints it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use simvar_observer::{RunId, RunKind, SchedulerView, SimEvent};

use crate::telemetry::{Invariants, Pulse, RunIndex, Shape};

/// Finished passing runs kept for browsing (failed runs are always kept).
pub const RECENT_RUNS: usize = 256;
/// Samples kept in each time series (one per second for throughput).
pub const SERIES: usize = 600;
/// Samples kept of each run's decision rate.
const RATE_SAMPLES: usize = 120;
/// Distinct trace hashes counted at most.
const TRACE_HASHES: usize = 1 << 20;

/// A bounded series of samples, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Series {
    samples: VecDeque<u64>,
    limit: usize,
}

impl Series {
    /// An empty series keeping at most `limit` samples.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(limit),
            limit,
        }
    }

    /// Add a sample, dropping the oldest beyond the limit.
    pub fn push(&mut self, sample: u64) {
        if self.samples.len() == self.limit.max(1) {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// The samples, oldest first.
    #[must_use]
    pub fn samples(&self) -> Vec<u64> {
        self.samples.iter().copied().collect()
    }

    /// The newest sample.
    #[must_use]
    pub fn last(&self) -> Option<u64> {
        self.samples.back().copied()
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Whether it passed.
    pub success: bool,
    /// The harness's report, with how to replay it.
    pub report: String,
    /// Real time it took.
    pub real_time: Duration,
    /// Simulated time that passed in it.
    pub sim_time: Duration,
    /// Simulation steps it ran.
    pub steps: u64,
}

/// One run, as far as it got.
#[derive(Debug, Clone)]
pub struct RunRecord {
    /// Which run.
    pub id: RunId,
    /// Its seed.
    pub seed: u64,
    /// Its simulated clock's epoch offset (with the seed, all a replay needs).
    pub epoch_offset: u64,
    /// Why it runs.
    pub kind: RunKind,
    /// The worker slot running it (`0` for jobs).
    pub worker: u64,
    /// Its configuration, as name and value.
    pub props: Vec<(String, String)>,
    /// Its length in steps, when fixed.
    pub steps: Option<u64>,
    /// The step it reached.
    pub step: u64,
    /// The fraction done, when its length is fixed.
    pub progress: Option<f64>,
    /// When the dashboard saw it start.
    pub started: Instant,
    /// How it ended, once it did.
    pub outcome: Option<Outcome>,
    /// Scheduling decisions so far.
    pub decisions: u64,
    /// Decisions per second, sampled.
    pub rates: Series,
    last_rate_sample: Option<(Instant, u64)>,
    /// The scheduler, when the run sends it (watched runs).
    pub scheduler: Option<SchedulerView>,
    /// Its invariants, when it sends them.
    pub invariants: Option<Invariants>,
    /// Its timeline index, when it sends one.
    pub index: Option<Arc<RunIndex>>,
    /// What it exercised.
    pub shape: Vec<String>,
}

impl RunRecord {
    fn new(id: RunId, seed: u64, now: Instant) -> Self {
        Self {
            id,
            seed,
            epoch_offset: 0,
            kind: RunKind::Fleet,
            worker: id.worker,
            props: Vec::new(),
            steps: None,
            step: 0,
            progress: None,
            started: now,
            outcome: None,
            decisions: 0,
            rates: Series::new(RATE_SAMPLES),
            last_rate_sample: None,
            scheduler: None,
            invariants: None,
            index: None,
            shape: Vec::new(),
        }
    }

    /// Whether it is still running.
    #[must_use]
    pub const fn live(&self) -> bool {
        self.outcome.is_none()
    }

    /// Whether it failed.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.outcome.as_ref().is_some_and(|o| !o.success)
    }

    /// The value of the first shape tag starting `prefix:` (for example the personality).
    #[must_use]
    pub fn tag(&self, prefix: &str) -> Option<&str> {
        self.shape.iter().find_map(|tag| {
            tag.strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix(':'))
        })
    }

    fn sample_rate(&mut self, decisions: u64, now: Instant) {
        self.decisions = self.decisions.max(decisions);
        match self.last_rate_sample {
            Some((at, before)) if now.duration_since(at) >= Duration::from_millis(250) => {
                let seconds = now.duration_since(at).as_secs_f64();
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )] // a rate for display
                let rate = (decisions.saturating_sub(before) as f64 / seconds) as u64;
                self.rates.push(rate);
                self.last_rate_sample = Some((now, decisions));
            }
            Some(_) => {}
            None => self.last_rate_sample = Some((now, decisions)),
        }
    }
}

/// Recent outcomes kept per worker.
pub const WORKER_HISTORY: usize = 64;

/// One fleet worker over the session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerStats {
    /// Runs it finished.
    pub runs: u64,
    /// Runs of those that failed.
    pub failed: u64,
    /// Its latest outcomes (true for passed), oldest first.
    pub recent: VecDeque<bool>,
    /// The run it is making.
    pub current: Option<RunId>,
}

/// Totals for the fleet (fleet runs only; replays and shrinks are not counted).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    /// Runs that passed.
    pub passed: u64,
    /// Runs that failed.
    pub failed: u64,
    /// Decisions made by finished runs.
    pub decisions: u64,
    /// Passed and failed runs by shape tag.
    pub by_tag: BTreeMap<String, (u64, u64)>,
}

/// One invariant across the fleet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InvariantTotal {
    /// Checks made by finished runs.
    pub checked: u64,
    /// Failures found.
    pub failed: u64,
}

/// Where shrinking a failure got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrinkStatus {
    /// The job's run.
    pub job: RunId,
    /// Replays tried.
    pub attempts: u64,
    /// Choices in the smallest failing schedule so far.
    pub best: u64,
    /// Choices in the original schedule.
    pub original: u64,
    /// Whether it is over.
    pub done: bool,
    /// The smallest failing schedule, once done.
    pub schedule: Option<String>,
}

/// Runs that failed the same way.
#[derive(Debug, Clone)]
pub struct FailureGroup {
    /// What failed, normalized (see [`signature`]).
    pub signature: String,
    /// The first failing run's seed.
    pub seed: u64,
    /// Its epoch offset.
    pub epoch_offset: u64,
    /// Its report.
    pub report: String,
    /// Its shape.
    pub shape: Vec<String>,
    /// The failing runs (the most recent ones).
    pub runs: Vec<RunId>,
    /// How many runs failed this way.
    pub count: u64,
    /// When first and last seen.
    pub first_seen: Instant,
    /// When last seen.
    pub last_seen: Instant,
    /// Whether a replay failed the same way, once one finished.
    pub reproduced: Option<bool>,
    /// The confirming replay, while it runs.
    pub confirming: Option<RunId>,
    /// Shrinking, once started.
    pub shrink: Option<ShrinkStatus>,
    /// Loaded from saved findings rather than seen in this session.
    pub saved: bool,
}

impl FailureGroup {
    /// The command that replays the first failing run, from its report.
    #[must_use]
    pub fn replay_command(&self) -> Option<String> {
        replay_command(&self.report)
    }
}

/// What a fold changed that deserves attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// A failure unlike any seen before (its group's index).
    NewFailure(usize),
    /// A known failure, again.
    RepeatFailure(usize),
    /// A confirming replay finished (group, whether it failed the same way).
    Confirmed(usize, bool),
    /// Shrinking finished (group).
    Shrunk(usize),
    /// Every run is done.
    Finished,
}

/// The fleet as configured when it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fleet {
    /// Runs it makes (`None`: until stopped).
    pub runs: Option<u64>,
    /// Runs at once.
    pub max_parallel: u64,
}

/// Everything the dashboard knows; see the [module documentation](self).
#[derive(Debug)]
pub struct Store {
    /// The fleet, once started.
    pub fleet: Option<Fleet>,
    /// Runs: live ones, recent ones, failed ones and jobs.
    pub runs: BTreeMap<RunId, RunRecord>,
    /// The fleet's workers, by slot.
    pub workers: BTreeMap<u64, WorkerStats>,
    pinned: BTreeSet<RunId>,
    recent: VecDeque<RunId>,
    /// Fleet totals.
    pub totals: Totals,
    /// Invariants across the fleet.
    pub invariants: BTreeMap<String, InvariantTotal>,
    /// Failures, by group, in the order first seen.
    pub failures: Vec<FailureGroup>,
    /// How many finished runs showed each shape tag.
    pub shapes: BTreeMap<String, u64>,
    trace_hashes: BTreeSet<u64>,
    /// Distinct decision traces among finished runs (capped).
    pub distinct_traces: u64,
    /// Runs finished per second.
    pub runs_per_second: Series,
    /// Decisions per second, across the fleet.
    pub decisions_per_second: Series,
    /// Simulated seconds per real second, times 100.
    pub speedup: Series,
    finished_since_tick: u64,
    decisions_since_tick: u64,
    sim_since_tick: Duration,
    live_decisions: BTreeMap<RunId, u64>,
    last_tick: Option<Instant>,
    /// When the store started.
    pub started: Instant,
    /// Whether every run is done.
    pub finished: bool,
}

impl Store {
    /// An empty store.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            fleet: None,
            runs: BTreeMap::new(),
            workers: BTreeMap::new(),
            pinned: BTreeSet::new(),
            recent: VecDeque::new(),
            totals: Totals::default(),
            invariants: BTreeMap::new(),
            failures: Vec::new(),
            shapes: BTreeMap::new(),
            trace_hashes: BTreeSet::new(),
            distinct_traces: 0,
            runs_per_second: Series::new(SERIES),
            decisions_per_second: Series::new(SERIES),
            speedup: Series::new(SERIES),
            finished_since_tick: 0,
            decisions_since_tick: 0,
            sim_since_tick: Duration::ZERO,
            live_decisions: BTreeMap::new(),
            last_tick: None,
            started: now,
            finished: false,
        }
    }

    /// Live fleet runs, by worker.
    #[must_use]
    pub fn live_fleet(&self) -> Vec<&RunRecord> {
        let mut live: Vec<&RunRecord> = self
            .runs
            .values()
            .filter(|r| r.live() && r.kind == RunKind::Fleet)
            .collect();
        live.sort_by_key(|r| (r.worker, r.id));
        live
    }

    /// Keep `run` however many runs follow it (it is open), or stop keeping it.
    pub fn pin(&mut self, run: RunId, pinned: bool) {
        if pinned {
            self.pinned.insert(run);
        } else {
            self.pinned.remove(&run);
        }
    }

    /// Runs finished so far by the fleet.
    #[must_use]
    pub const fn completed(&self) -> u64 {
        self.totals.passed + self.totals.failed
    }

    /// Fold one event. Returns what deserves attention.
    #[allow(clippy::too_many_lines)] // one arm per event
    pub fn apply(&mut self, event: SimEvent, now: Instant) -> Vec<Notice> {
        let mut notices = Vec::new();
        match event {
            SimEvent::Started { runs, max_parallel } => {
                self.fleet = Some(Fleet { runs, max_parallel });
            }
            SimEvent::RunStarted {
                id,
                seed,
                props,
                steps,
                worker,
                kind,
                epoch_offset,
            } => {
                let mut run = RunRecord::new(id, seed, now);
                run.props = props;
                run.steps = steps;
                run.worker = worker;
                run.kind = kind;
                run.epoch_offset = epoch_offset;
                if kind == RunKind::Fleet {
                    self.workers.entry(worker).or_default().current = Some(id);
                }
                self.runs.insert(id, run);
            }
            SimEvent::Progress { id, step, progress } => {
                if let Some(run) = self.runs.get_mut(&id) {
                    run.step = step;
                    run.progress = progress;
                }
            }
            SimEvent::RunFinished {
                id,
                seed,
                success,
                report,
                steps,
                real_time,
                sim_time,
            } => self.finish(
                id,
                seed,
                Outcome {
                    success,
                    report,
                    real_time,
                    sim_time,
                    steps,
                },
                now,
                &mut notices,
            ),
            SimEvent::Shrink {
                id,
                attempts,
                best_len,
                original_len,
                done,
                schedule,
            } => {
                if let Some(index) = self
                    .failures
                    .iter()
                    .position(|g| g.shrink.as_ref().is_some_and(|s| s.job == id))
                {
                    self.failures[index].shrink = Some(ShrinkStatus {
                        job: id,
                        attempts,
                        best: best_len,
                        original: original_len,
                        done,
                        schedule,
                    });
                    if done {
                        notices.push(Notice::Shrunk(index));
                    }
                }
            }
            SimEvent::Telemetry { id, payload } => {
                let Some(run) = self.runs.get_mut(&id) else {
                    return notices;
                };
                if let Some(pulse) = payload.downcast_ref::<Pulse>() {
                    run.sample_rate(pulse.decisions, now);
                    if run.kind == RunKind::Fleet {
                        self.live_decisions.insert(id, pulse.decisions);
                    }
                } else if let Some(view) = payload.downcast_ref::<SchedulerView>() {
                    run.sample_rate(view.decisions, now);
                    run.scheduler = Some(view.clone());
                } else if let Some(invariants) = payload.downcast_ref::<Invariants>() {
                    run.invariants = Some(invariants.clone());
                } else if let Some(index) = payload.downcast_ref::<RunIndex>() {
                    run.decisions = run.decisions.max(index.decisions);
                    run.index = Some(Arc::new(index.clone()));
                } else if let Some(shape) = payload.downcast_ref::<Shape>() {
                    run.shape.clone_from(&shape.0);
                }
            }
            SimEvent::Finished => {
                self.finished = true;
                notices.push(Notice::Finished);
            }
            _ => {}
        }
        notices
    }

    fn finish(
        &mut self,
        id: RunId,
        seed: u64,
        outcome: Outcome,
        now: Instant,
        notices: &mut Vec<Notice>,
    ) {
        let run = self
            .runs
            .entry(id)
            .or_insert_with(|| RunRecord::new(id, seed, now));
        run.seed = seed;
        let success = outcome.success;
        let sim_time = outcome.sim_time;
        run.outcome = Some(outcome);
        let run = run.clone();
        self.live_decisions.remove(&id);
        match run.kind {
            RunKind::Fleet => self.count_fleet_run(&run, sim_time, now, notices),
            RunKind::Replay => {
                if let Some(index) = self.failures.iter().position(|g| g.confirming == Some(id)) {
                    let group = &mut self.failures[index];
                    group.confirming = None;
                    group.reproduced = Some(!success);
                    notices.push(Notice::Confirmed(index, !success));
                }
            }
            RunKind::Shrink => {}
        }
        if run.kind == RunKind::Fleet {
            let worker = self.workers.entry(run.worker).or_default();
            worker.runs += 1;
            worker.failed += u64::from(!success);
            worker.recent.push_back(success);
            if worker.recent.len() > WORKER_HISTORY {
                worker.recent.pop_front();
            }
        }
        if success && run.kind == RunKind::Fleet {
            self.recent.push_back(id);
            while self.recent.len() > RECENT_RUNS {
                if let Some(old) = self.recent.pop_front()
                    && !self.pinned.contains(&old)
                    && !self.workers.values().any(|w| w.current == Some(old))
                {
                    self.runs.remove(&old);
                }
            }
        }
    }

    fn count_fleet_run(
        &mut self,
        run: &RunRecord,
        sim_time: Duration,
        now: Instant,
        notices: &mut Vec<Notice>,
    ) {
        let success = !run.failed();
        if success {
            self.totals.passed += 1;
        } else {
            self.totals.failed += 1;
        }
        self.totals.decisions += run.decisions;
        self.finished_since_tick += 1;
        self.sim_since_tick += sim_time;
        for tag in &run.shape {
            *self.shapes.entry(tag.clone()).or_default() += 1;
            let entry = self.totals.by_tag.entry(tag.clone()).or_default();
            if success {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
        if let Some(invariants) = &run.invariants {
            for status in &invariants.0 {
                let total = self.invariants.entry(status.name.clone()).or_default();
                total.checked += status.checked;
                total.failed += u64::try_from(status.failures.len()).unwrap_or(u64::MAX);
            }
        }
        if let Some(index) = &run.index
            && self.trace_hashes.len() < TRACE_HASHES
            && self.trace_hashes.insert(index.trace_hash)
        {
            self.distinct_traces += 1;
        }
        if !success {
            let report = run
                .outcome
                .as_ref()
                .map(|o| o.report.clone())
                .unwrap_or_default();
            let signature = signature(&report);
            if let Some(index) = self.failures.iter().position(|g| g.signature == signature) {
                let group = &mut self.failures[index];
                group.count += 1;
                group.last_seen = now;
                group.runs.push(run.id);
                if group.runs.len() > 16 {
                    group.runs.remove(0);
                }
                notices.push(Notice::RepeatFailure(index));
            } else {
                self.failures.push(FailureGroup {
                    signature,
                    seed: run.seed,
                    epoch_offset: run.epoch_offset,
                    report,
                    shape: run.shape.clone(),
                    runs: vec![run.id],
                    count: 1,
                    first_seen: now,
                    last_seen: now,
                    reproduced: None,
                    confirming: None,
                    shrink: None,
                    saved: false,
                });
                notices.push(Notice::NewFailure(self.failures.len() - 1));
            }
        }
    }

    /// Sample throughput; call about once a second.
    pub fn tick(&mut self, now: Instant) {
        let Some(last) = self.last_tick else {
            self.last_tick = Some(now);
            return;
        };
        let elapsed = now.duration_since(last);
        if elapsed < Duration::from_millis(900) {
            return;
        }
        let seconds = elapsed.as_secs_f64();
        let live: u64 = self.live_decisions.values().sum();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )] // rates for display
        {
            self.runs_per_second
                .push((self.finished_since_tick as f64 / seconds).round() as u64);
            let decisions = self.totals.decisions + live;
            let rate = decisions.saturating_sub(self.decisions_since_tick) as f64 / seconds;
            self.decisions_since_tick = decisions;
            self.decisions_per_second.push(rate as u64);
            self.speedup
                .push((self.sim_since_tick.as_secs_f64() / seconds * 100.0) as u64);
        }
        self.finished_since_tick = 0;
        self.sim_since_tick = Duration::ZERO;
        self.last_tick = Some(now);
    }

    /// Note that a replay (`job`) is confirming failure group `group`.
    pub fn confirming(&mut self, group: usize, job: RunId) {
        if let Some(group) = self.failures.get_mut(group) {
            group.confirming = Some(job);
        }
    }

    /// Note that a shrink job (`job`) started for failure group `group`.
    pub fn shrinking(&mut self, group: usize, job: RunId) {
        if let Some(group) = self.failures.get_mut(group) {
            group.shrink = Some(ShrinkStatus {
                job,
                attempts: 0,
                best: 0,
                original: 0,
                done: false,
                schedule: None,
            });
        }
    }

    /// Add a failure group loaded from saved findings (unless one with its signature exists).
    pub fn add_saved(&mut self, mut group: FailureGroup) {
        if self.failures.iter().all(|g| g.signature != group.signature) {
            group.saved = true;
            self.failures.push(group);
        }
    }
}

/// What a failure report says failed, normalized so the same failure in different runs has the
/// same signature: the first line of the run's error (numbers replaced by `#`), or of the
/// report.
#[must_use]
pub fn signature(report: &str) -> String {
    let error = report
        .split_once("Error:")
        .map_or(report, |(_, rest)| rest)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('='))
        .unwrap_or("unknown failure");
    let mut normalized = String::with_capacity(error.len());
    let mut in_number = false;
    let mut depth = 0_usize;
    for c in error.chars() {
        // Lists (`[630, 631]`) vary in length between runs: keep only that there is one.
        match c {
            '[' => {
                if depth == 0 {
                    normalized.push_str("[…]");
                }
                depth += 1;
                continue;
            }
            ']' if depth > 0 => {
                depth -= 1;
                continue;
            }
            _ if depth > 0 => continue,
            _ => {}
        }
        if c.is_ascii_digit() {
            if !in_number {
                normalized.push('#');
            }
            in_number = true;
        } else {
            in_number = false;
            normalized.push(c);
        }
    }
    // Keep the invariant's own number: `#. ...` at the start was `N. ...`.
    if let Some(rest) = normalized.strip_prefix("#.")
        && let Some(number) = error.split('.').next()
    {
        normalized = format!("{number}.{rest}");
    }
    normalized.chars().take(120).collect()
}

/// The replay command in a failure report (the one quoted after "To run again with this seed").
#[must_use]
pub fn replay_command(report: &str) -> Option<String> {
    let line = report
        .lines()
        .find(|line| line.contains("To run again with this seed"))?;
    let start = line.find('`')? + 1;
    let end = line[start..].find('`')? + start;
    Some(line[start..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::InvariantStatus;

    fn id(worker: u64, run: u64) -> RunId {
        RunId { worker, run }
    }

    fn start(store: &mut Store, id: RunId, seed: u64, kind: RunKind, now: Instant) {
        store.apply(
            SimEvent::RunStarted {
                id,
                seed,
                props: vec![],
                steps: Some(100),
                worker: id.worker,
                kind,
                epoch_offset: 7,
            },
            now,
        );
    }

    fn finish(store: &mut Store, id: RunId, seed: u64, success: bool, report: &str) -> Vec<Notice> {
        store.apply(
            SimEvent::RunFinished {
                id,
                seed,
                success,
                report: report.into(),
                steps: 100,
                real_time: Duration::from_millis(10),
                sim_time: Duration::from_secs(1),
            },
            Instant::now(),
        )
    }

    const REPORT: &str = "header\nError:\n6. the crashed server's sessions still run [630] after recovery\n\nTo run again with this seed: `SIMULATOR_SEED=1 sim`\n";

    #[test]
    fn failures_group_by_signature_and_totals_split_by_shape() {
        let now = Instant::now();
        let mut store = Store::new(now);
        store.apply(
            SimEvent::Started {
                runs: None,
                max_parallel: 2,
            },
            now,
        );
        for (n, success, pids) in [(1, true, ""), (2, false, "630"), (3, false, "711")] {
            let run = id(1, n);
            start(&mut store, run, n, RunKind::Fleet, now);
            let shape = Arc::new(Shape(vec![format!(
                "personality:{}",
                if n == 1 { "linux" } else { "windows" }
            )]));
            store.apply(
                SimEvent::Telemetry {
                    id: run,
                    payload: shape,
                },
                now,
            );
            let report = REPORT.replace("630", pids);
            let notices = finish(&mut store, run, n, success, &report);
            match n {
                1 => assert_eq!(notices, []),
                2 => assert_eq!(notices, [Notice::NewFailure(0)]),
                _ => assert_eq!(notices, [Notice::RepeatFailure(0)]),
            }
        }
        assert_eq!(store.totals.passed, 1);
        assert_eq!(store.totals.failed, 2);
        assert_eq!(store.failures.len(), 1);
        assert_eq!(store.failures[0].count, 2);
        assert_eq!(store.failures[0].seed, 2);
        assert_eq!(store.totals.by_tag["personality:windows"], (0, 2));
        assert_eq!(
            store.failures[0].replay_command().as_deref(),
            Some("SIMULATOR_SEED=1 sim")
        );
    }

    #[test]
    fn workers_keep_their_outcomes_and_open_runs_stay() {
        let now = Instant::now();
        let mut store = Store::new(now);
        start(&mut store, id(3, 1), 1, RunKind::Fleet, now);
        store.pin(id(3, 1), true);
        finish(&mut store, id(3, 1), 1, true, "");
        for n in 2..=(RECENT_RUNS as u64 + 5) {
            start(&mut store, id(3, n), n, RunKind::Fleet, now);
            finish(&mut store, id(3, n), n, n % 10 != 0, REPORT);
        }
        let worker = &store.workers[&3];
        assert_eq!(worker.runs, RECENT_RUNS as u64 + 5);
        assert_eq!(worker.recent.len(), WORKER_HISTORY);
        assert!(worker.recent.iter().any(|passed| !passed));
        assert!(store.runs.contains_key(&id(3, 1)), "pinned runs stay");
    }

    #[test]
    fn replays_confirm_failures_without_counting_as_fleet_runs() {
        let now = Instant::now();
        let mut store = Store::new(now);
        start(&mut store, id(1, 1), 9, RunKind::Fleet, now);
        finish(&mut store, id(1, 1), 9, false, REPORT);
        let job = id(0, 1);
        store.confirming(0, job);
        start(&mut store, job, 9, RunKind::Replay, now);
        let notices = finish(&mut store, job, 9, false, REPORT);
        assert_eq!(notices, [Notice::Confirmed(0, true)]);
        assert_eq!(store.failures[0].reproduced, Some(true));
        assert_eq!(store.completed(), 1);
    }

    #[test]
    fn shrink_progress_reaches_its_failure() {
        let now = Instant::now();
        let mut store = Store::new(now);
        start(&mut store, id(1, 1), 9, RunKind::Fleet, now);
        finish(&mut store, id(1, 1), 9, false, REPORT);
        store.shrinking(0, id(0, 2));
        let notices = store.apply(
            SimEvent::Shrink {
                id: id(0, 2),
                attempts: 40,
                best_len: 12,
                original_len: 900,
                done: true,
                schedule: Some("1,2".into()),
            },
            now,
        );
        assert_eq!(notices, [Notice::Shrunk(0)]);
        let shrink = store.failures[0].shrink.as_ref().unwrap();
        assert_eq!((shrink.best, shrink.original), (12, 900));
    }

    #[test]
    fn invariants_and_recent_runs_are_bounded_totals() {
        let now = Instant::now();
        let mut store = Store::new(now);
        for n in 1..=(RECENT_RUNS as u64 + 10) {
            let run = id(1, n);
            start(&mut store, run, n, RunKind::Fleet, now);
            store.apply(
                SimEvent::Telemetry {
                    id: run,
                    payload: Arc::new(Invariants(vec![InvariantStatus {
                        name: "1. opens".into(),
                        checked: 2,
                        failures: vec![],
                    }])),
                },
                now,
            );
            finish(&mut store, run, n, true, "");
        }
        assert_eq!(store.runs.len(), RECENT_RUNS);
        assert_eq!(
            store.invariants["1. opens"].checked,
            2 * (RECENT_RUNS as u64 + 10)
        );
    }

    #[test]
    fn signatures_ignore_numbers_but_keep_the_invariant() {
        assert_eq!(
            signature("Error:\n4. closing pane process 967 failed: x"),
            "4. closing pane process # failed: x"
        );
        assert_eq!(
            signature("6. still run [630, 631] after"),
            signature("6. still run [12] after")
        );
        assert_eq!(signature("boom 12"), "boom #");
    }
}
