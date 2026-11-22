//! Exhaustive exploration of short scenarios (`--explore`): every thread schedule within a
//! bound on its departures from the default schedule.
//!
//! A scenario is a short, fully scripted run: a setup (one driver opening a pane or two), then
//! two drivers each taking a fixed step at once ([`Step`]s), with a fixed profile and no
//! faults, then the workload's usual end (every pane closed, nothing left). Every run of a
//! scenario has the same seed, so only the thread schedule varies, and every invariant the
//! workload checks is checked in each.
//!
//! # The bound
//!
//! A schedule is the choice points (decisions with more than one runnable thread) where a run
//! departs from the default choice (keep the running thread if it can go on, else the
//! lowest-numbered runnable one); its *delays* are how many there are. The exploration runs
//! every schedule with at most `--delays` delays at choice points from the drivers' start to
//! the run's end (delay bounding: Emmi, Qadeer and Rakamarić, POPL 2011). With one delay that
//! is one run per alternative at each choice point; with two, one per pair.
//!
//! It does not bound preemptions alone, as `switchy_thread::simulator::explore_runs` does:
//! there, switching because the running thread blocked is free, so every free choice is
//! branched on, and here they compound (a pane runtime's run has hundreds of choice points,
//! many with several threads woken at once): not even every schedule without a preemption
//! can be run. Nor can it prune: the simulated kernel and executor use the scheduler's public
//! switch points, so it cannot tell independent operations apart. Every schedule with `k`
//! preemptions and no other departure is within a bound of `k` delays.
//!
//! Its runs are made here, not by simvar's harness: each is a fresh `switchy_thread` run
//! (`run_with`, replaying its schedule) whose main thread drives the run's executor and clock
//! as simvar's step loop does (a step: advance the clock one step, poll one task, then let the
//! simulated threads run until idle). Each run's choice points are read through the
//! scheduler's decision hook, and checked against the scheduler's own count and the schedule
//! it followed, so a scenario that is not deterministic is reported, not explored wrongly.
//!
//! A failing schedule is shrunk (`switchy_thread::simulator::shrink`, as simvar shrinks a
//! fleet failure's) and printed with its replay command: `--explore <scenario> --workers N
//! --schedule <schedule>` replays it once (with `PANE_RUNTIME_SIM_EVENTS=1` to see what
//! happened, or `SIMULATOR_SCHEDULE` instead of `--schedule`).

use std::{
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use switchy_pty::simulator::Personality;
use switchy_thread::simulator::{
    Config, ExploredRun, Schedule, ThreadIndex, ThreadState, run_with, shrink,
};

use crate::{
    profile::{FaultRates, Kind, Profile},
    programs,
    workload::Step,
};

/// Every scenario run's seed: only the schedule varies.
const SEED: u64 = 1;

/// Every scenario run's clock epoch, so nothing is drawn for it.
const EPOCH: u64 = 1_000;

/// Steps (simulated milliseconds) a scenario run may take: beyond the workload's own budget,
/// so its livelock check reports first.
const STEP_LIMIT: u64 = 400_000;

/// Replays a shrink makes at most.
const SHRINK_ATTEMPTS: usize = 200;

/// One scenario: a setup, then two drivers' steps at once.
#[derive(Debug)]
pub struct Scenario {
    /// Its name on the command line.
    pub name: &'static str,
    /// What it races.
    pub about: &'static str,
    /// The kernel it runs on (unless `SIMULATOR_PTY_PERSONALITY` says otherwise).
    pub personality: Personality,
    /// What panes run.
    pub program: programs::Kind,
    /// Steps driver 0 takes alone first.
    pub setup: &'static [Step],
    /// Each driver's steps, taken at once.
    pub drivers: &'static [&'static [Step]],
    /// Departures from the default schedule explored by default (the nightly's bound).
    pub delays: usize,
}

impl Scenario {
    /// Its fixed profile: one driver per script, the scenario's program, no faults.
    #[must_use]
    pub fn profile(&self) -> Profile {
        let mut programs = [0; programs::Kind::ALL.len()];
        if let Some(index) = programs::Kind::ALL.iter().position(|kind| *kind == self.program) {
            programs[index] = 1;
        }
        Profile {
            drivers: self.drivers.len() as u64,
            steps: 0,
            panes: 4,
            clock: 1,
            faults: FaultRates::default(),
            programs,
            sweep: true,
            ..Profile::default()
        }
    }
}

/// The scenarios, in the plan's order.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "restart-during-close",
        about: "a pane is closed while another request restarts it (Windows: the console close)",
        personality: Personality::Windows,
        program: programs::Kind::Shell,
        setup: &[Step::new(Kind::Open)],
        drivers: &[&[Step::new(Kind::Close)], &[Step::new(Kind::Restart)]],
        delays: 2,
    },
    Scenario {
        name: "crash-during-open",
        about: "the server crashes while a pane opens, and a new one recovers",
        personality: Personality::Linux,
        program: programs::Kind::Shell,
        setup: &[Step::new(Kind::Open)],
        drivers: &[&[Step::new(Kind::Open)], &[Step::new(Kind::Crash)]],
        delays: 2,
    },
    Scenario {
        name: "resize-during-restart",
        about: "a pane is resized while its program restarts",
        personality: Personality::MacOs,
        program: programs::Kind::Shell,
        setup: &[Step::new(Kind::Open)],
        drivers: &[&[Step::new(Kind::Restart)], &[Step::new(Kind::Resize)]],
        delays: 2,
    },
    Scenario {
        name: "close-split-during-restart",
        about: "one of a session's two panes closes while the other's program restarts",
        personality: Personality::Linux,
        program: programs::Kind::Shell,
        setup: &[Step::new(Kind::Open), Step::new(Kind::Split)],
        drivers: &[
            &[Step::new(Kind::Restart).on(1)],
            &[Step::new(Kind::Close)],
        ],
        delays: 2,
    },
    Scenario {
        name: "exit-during-restart",
        about: "a pane's program exits as its restart begins: the exit notice races the restart",
        personality: Personality::Linux,
        program: programs::Kind::Shell,
        setup: &[Step::new(Kind::Open)],
        drivers: &[
            &[Step::new(Kind::Type).line("exit")],
            &[Step::new(Kind::Restart)],
        ],
        delays: 2,
    },
    Scenario {
        name: "interrupt-during-startup",
        about: "a pane is interrupted while its restarted program starts (Windows: the console \
                startup handshake)",
        personality: Personality::Windows,
        program: programs::Kind::Shell,
        setup: &[Step::new(Kind::Open)],
        drivers: &[&[Step::new(Kind::Restart)], &[Step::new(Kind::Interrupt)]],
        delays: 2,
    },
];

/// The scenario named `name`.
#[must_use]
pub fn scenario(name: &str) -> Option<&'static Scenario> {
    SCENARIOS.iter().find(|scenario| scenario.name == name)
}

/// Panic messages of the current run (the hook below records them, quietly).
static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record panics instead of printing them: an exploration makes many runs, and a failing
/// one's panic is in its report.
fn quiet_panics() {
    std::panic::set_hook(Box::new(|info| {
        if info
            .payload()
            .is::<switchy_thread::simulator::Killed>()
        {
            return;
        }
        let thread = std::thread::current().name().unwrap_or("?").to_owned();
        PANICS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(format!("thread {thread} panicked: {info}"));
    }));
}

/// One run of `scenario`, its scheduler configured by `config`.
#[must_use]
pub fn run_once(scenario: &'static Scenario, config: Config) -> ExploredRun {
    run_once_with(scenario, config, || {})
}

/// As [`run_once`], calling `prepare` first on the run's main thread.
fn run_once_with(
    scenario: &'static Scenario,
    config: Config,
    prepare: impl FnOnce(),
) -> ExploredRun {
    PANICS.lock().unwrap_or_else(PoisonError::into_inner).clear();
    let outcome = run_with(config, SEED, || {
        prepare();
        drive(scenario)
    });
    let mut problems = match outcome.result {
        Ok(Ok(())) => Vec::new(),
        Ok(Err(violations)) => violations,
        Err(_) => vec!["the run panicked".to_owned()],
    };
    problems.extend(PANICS.lock().unwrap_or_else(PoisonError::into_inner).drain(..));
    if !outcome.report.leaked.is_empty() {
        problems.push(format!(
            "5. simulated threads never finished: {}",
            outcome.report.leaked.join(", ")
        ));
    }
    ExploredRun {
        failure: (!problems.is_empty()).then(|| problems.join("\n")),
        report: outcome.report,
    }
}

/// A run's main thread: drive the scenario's workload as simvar's step loop does.
fn drive(scenario: &'static Scenario) -> Result<(), Vec<String>> {
    switchy_time::simulator::set_epoch_offset(EPOCH);
    let runtime = switchy_async::runtime::Runtime::new();
    let handle = runtime.spawn_local(crate::run(scenario.profile(), Some(scenario)));
    let mut steps = 0;
    while !handle.is_finished() {
        steps += 1;
        if steps > STEP_LIMIT {
            runtime.shutdown_now();
            return Err(vec![format!(
                "5. the scenario did not finish in {STEP_LIMIT} steps"
            )]);
        }
        let _ = switchy_time::simulator::next_step();
        runtime.tick();
        if let Some(scheduler) = switchy_thread::simulator::try_scheduler()
            && scheduler.others_alive()
        {
            scheduler.run_until_idle();
        }
    }
    let result = runtime
        .block_on(handle)
        .map_err(|error| vec![format!("5. the workload panicked: {error}")])?;
    runtime.shutdown_now();
    result
}

/// Marks the part of a run where schedules branch: from the drivers' start (after the
/// scenario's setup) to the run's end. Run-scoped; a `std` atomic, so reading it is no switch
/// point.
#[derive(Debug, Default)]
pub struct Window(AtomicBool);

/// Open the current run's exploration window: the scenario's setup is done.
pub fn open_window() {
    switchy_simulation::state::<Window>().0.store(true, Ordering::SeqCst);
}

/// A choice point of a run: the threads that could run, the default pick, and whether it is in
/// the exploration window.
#[derive(Debug, Clone)]
struct ChoicePoint {
    runnable: Vec<ThreadIndex>,
    default: ThreadIndex,
    in_window: bool,
}

/// One run of `scenario` following `schedule`, with its choice points (seen through the
/// scheduler's decision hook).
fn traced_run(scenario: &'static Scenario, schedule: &Schedule) -> (ExploredRun, Vec<ChoicePoint>) {
    let points: Arc<Mutex<Vec<ChoicePoint>>> = Arc::default();
    let recorder = {
        let points = Arc::clone(&points);
        move || {
            let window = switchy_simulation::state::<Window>();
            switchy_thread::simulator::set_decision_hook(Some(Arc::new(
                move |decision: &switchy_thread::simulator::Decision,
                      snapshot: &dyn Fn() -> switchy_thread::simulator::SchedulerSnapshot| {
                    let runnable: Vec<ThreadIndex> = snapshot()
                        .threads
                        .iter()
                        .filter(|thread| thread.state == ThreadState::Runnable)
                        .map(|thread| thread.index)
                        .collect();
                    if runnable.len() < 2 {
                        return;
                    }
                    let default = if runnable.contains(&decision.from) {
                        decision.from
                    } else {
                        runnable[0]
                    };
                    points.lock().unwrap_or_else(PoisonError::into_inner).push(ChoicePoint {
                        runnable,
                        default,
                        in_window: window.0.load(Ordering::SeqCst),
                    });
                },
            )));
        }
    };
    let run = run_once_with(scenario, Config::explore(schedule.clone()), recorder);
    let points = std::mem::take(&mut *points.lock().unwrap_or_else(PoisonError::into_inner));
    (run, points)
}

/// What exploring one scenario under one worker count found.
#[derive(Debug)]
pub struct Summary {
    /// Runs made (schedules explored).
    pub runs: usize,
    /// Choice points of the default schedule, and those in the window.
    pub choice_points: (usize, usize),
    /// Whether every schedule within the bound was explored.
    pub complete: bool,
    /// The failing schedule, shrunk, and why it failed.
    pub failure: Option<(Schedule, String)>,
}

/// Explore `scenario`: every schedule that departs from the default at no more than `delays`
/// choice points of the window (at most `max_runs` runs), on this process's async workers
/// (`SIMULATOR_ASYNC_WORKERS`).
///
/// # Errors
///
/// * If a run did not follow its schedule (the scenario is not deterministic)
pub fn explore(
    scenario: &'static Scenario,
    delays: usize,
    max_runs: Option<usize>,
) -> Result<Summary, String> {
    let progress = std::env::var_os("PANE_RUNTIME_SIM_EXPLORE_PROGRESS").is_some();
    let start = Instant::now();
    let mut summary = Summary {
        runs: 0,
        choice_points: (0, 0),
        complete: false,
        failure: None,
    };
    // Depth first: each schedule's children add one departure at a later choice point, so
    // every schedule within the bound is made exactly once.
    let mut pending = vec![Schedule::new()];
    while let Some(schedule) = pending.pop() {
        if max_runs.is_some_and(|max| summary.runs >= max) {
            return Ok(summary);
        }
        let (run, points) = traced_run(scenario, &schedule);
        summary.runs += 1;
        if progress && summary.runs.is_multiple_of(100) {
            eprintln!(
                "{} runs at {:.1}s ({} pending): {schedule}",
                summary.runs,
                start.elapsed().as_secs_f64(),
                pending.len()
            );
        }
        if summary.runs == 1 {
            summary.choice_points =
                (points.len(), points.iter().filter(|point| point.in_window).count());
        }
        if let Some(message) = run.failure {
            summary.failure = Some((run.report.schedule, message));
            break;
        }
        let traced = run.report.trace.as_ref().map_or(0, switchy_thread::simulator::Trace::choice_points);
        if run.report.schedule != schedule || traced != points.len() {
            return Err(format!(
                "schedule {schedule} was not followed (the run took {}, with {} choice points \
                 of {traced}): the scenario is not deterministic",
                run.report.schedule,
                points.len()
            ));
        }
        if schedule.choices.len() >= delays {
            continue;
        }
        let after = schedule
            .choices
            .last()
            .map_or(0, |&(point, _)| usize::try_from(point).unwrap_or(usize::MAX) + 1);
        // Pushed in reverse, so earlier choice points are explored first.
        for (index, point) in points.iter().enumerate().skip(after).rev() {
            if !point.in_window {
                continue;
            }
            for &thread in point.runnable.iter().rev() {
                if thread == point.default {
                    continue;
                }
                let mut child = schedule.clone();
                child.choices.push((index as u64, thread));
                pending.push(child);
            }
        }
    }
    summary.complete = summary.failure.is_none();
    if let Some((schedule, message)) = summary.failure.take() {
        let shrunk = shrink(&schedule, SHRINK_ATTEMPTS, |candidate| {
            let run = run_once(scenario, Config::replay(candidate.clone()));
            run.failure.map(|_| run.report.schedule)
        });
        let message = run_once(scenario, Config::replay(shrunk.clone()))
            .failure
            .unwrap_or(message);
        summary.failure = Some((shrunk, message));
    }
    Ok(summary)
}

/// Options of `--explore`.
#[derive(Debug, Default)]
struct Options {
    scenarios: Vec<&'static Scenario>,
    delays: Option<usize>,
    workers: Option<usize>,
    schedule: Option<Schedule>,
    max_runs: Option<usize>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.iter();
    let name = args
        .next()
        .ok_or("--explore needs a scenario, or all")?;
    options.scenarios = if name == "all" {
        SCENARIOS.iter().collect()
    } else {
        vec![scenario(name).ok_or_else(|| {
            let names: Vec<_> = SCENARIOS.iter().map(|scenario| scenario.name).collect();
            format!("unknown scenario {name}; expected all or one of {}", names.join(", "))
        })?]
    };
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--delays" => {
                options.delays = Some(value()?.parse().map_err(|_| "bad --delays")?);
            }
            "--workers" => {
                options.workers = Some(value()?.parse().map_err(|_| "bad --workers")?);
            }
            "--schedule" => options.schedule = Some(value()?.parse()?),
            "--max-runs" => {
                options.max_runs = Some(value()?.parse().map_err(|_| "bad --max-runs")?);
            }
            other => return Err(format!("unknown --explore option {other}")),
        }
    }
    if options.schedule.is_some() && options.scenarios.len() != 1 {
        return Err("--schedule replays one scenario".into());
    }
    Ok(options)
}

/// `--explore <scenario|all> [--delays N] [--workers N] [--schedule S] [--max-runs N]`.
///
/// Explores each scenario under 1 and 2 async workers (each in a process of its own, with
/// `SIMULATOR_ASYNC_WORKERS` set), or with `--workers`, under that many in this process; with
/// `--schedule` (or `SIMULATOR_SCHEDULE`), replays one schedule instead.
///
/// # Errors
///
/// * For bad options, or if a schedule failed
pub fn main(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut options = parse(args)?;
    if options.schedule.is_none()
        && let Ok(text) = std::env::var("SIMULATOR_SCHEDULE")
    {
        options.schedule = Some(text.parse()?);
    }
    let Some(workers) = options.workers else {
        return explore_each(args, &options);
    };
    // SAFETY: set before this process starts any thread (each worker count runs in a process
    // of its own).
    unsafe { std::env::set_var("SIMULATOR_ASYNC_WORKERS", workers.to_string()) };
    let scenario = options.scenarios[0];
    if let Some(schedule) = options.schedule {
        let run = run_once(scenario, Config::explore(schedule.clone()));
        let choice_points = run.report.trace.as_ref().map_or(0, switchy_thread::simulator::Trace::choice_points);
        return match run.failure {
            None => {
                println!(
                    "{}: schedule {schedule} passed ({} decisions, {choice_points} choice points)",
                    scenario.name, run.report.decisions
                );
                Ok(())
            }
            Some(failure) => Err(format!("{}: schedule {schedule} failed:\n{failure}", scenario.name).into()),
        };
    }
    quiet_panics();
    let mut failed = false;
    for scenario in &options.scenarios {
        let delays = options.delays.unwrap_or(scenario.delays);
        let start = Instant::now();
        let summary = explore(scenario, delays, options.max_runs)?;
        println!(
            "{}: {workers} worker(s), up to {delays} delay(s): {} schedules, {}, {} failure(s), \
             {:.1}s ({} choice points, {} in the window)",
            scenario.name,
            summary.runs,
            if summary.complete { "complete" } else { "incomplete" },
            usize::from(summary.failure.is_some()),
            start.elapsed().as_secs_f64(),
            summary.choice_points.0,
            summary.choice_points.1,
        );
        if let Some((schedule, message)) = summary.failure {
            failed = true;
            println!(
                "  failed:\n    {}\n  replay with: cargo sim pane-runtime --explore {} \
                 --workers {workers} --schedule {schedule}",
                message.replace('\n', "\n    "),
                scenario.name,
            );
        }
    }
    if failed {
        return Err("a schedule failed".into());
    }
    Ok(())
}

/// Explore each scenario under 1 and 2 workers, each in a process of its own (the worker count
/// is read from the environment once per run), as many at once as there are processors
/// (`PANE_RUNTIME_SIM_EXPLORE_JOBS` to say otherwise). Each prints when it is done.
fn explore_each(args: &[String], options: &Options) -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let jobs = std::env::var("PANE_RUNTIME_SIM_EXPLORE_JOBS")
        .ok()
        .and_then(|jobs| jobs.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from))
        .max(1);
    let queue: Mutex<Vec<(&'static str, usize)>> = Mutex::new(
        options
            .scenarios
            .iter()
            .flat_map(|scenario| [(scenario.name, 1), (scenario.name, 2)])
            .rev()
            .collect(),
    );
    let failed: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let Some((name, workers)) =
                        queue.lock().unwrap_or_else(PoisonError::into_inner).pop()
                    else {
                        return;
                    };
                    let output = std::process::Command::new(&executable)
                        .arg("--explore")
                        .arg(name)
                        .arg("--workers")
                        .arg(workers.to_string())
                        // Every option but the scenario passes through.
                        .args(&args[1..])
                        .stderr(std::process::Stdio::inherit())
                        .output();
                    let success = match output {
                        Ok(output) => {
                            print!("{}", String::from_utf8_lossy(&output.stdout));
                            output.status.success()
                        }
                        Err(error) => {
                            println!("{name}: {workers} worker(s): could not run: {error}");
                            false
                        }
                    };
                    if !success {
                        failed
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(format!("{name} ({workers} worker(s))"));
                    }
                }
            });
        }
    });
    let failed = failed.into_inner().unwrap_or_else(PoisonError::into_inner);
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("failed: {}", failed.join(", ")).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_passes_its_default_schedule() {
        for scenario in SCENARIOS {
            let summary = explore(scenario, 0, None).unwrap();
            assert_eq!(summary.runs, 1, "{}", scenario.name);
            assert!(summary.complete, "{}: {:?}", scenario.name, summary.failure);
            assert!(summary.choice_points.1 > 0, "{}: nothing to explore", scenario.name);
        }
    }

    #[test]
    fn a_run_limit_stops_the_exploration_incomplete() {
        let scenario = scenario("interrupt-during-startup").unwrap();
        let summary = explore(scenario, 1, Some(5)).unwrap();
        assert_eq!(summary.runs, 5);
        assert!(!summary.complete, "stopped at the run limit");
    }

    #[test]
    fn options_parse() {
        let args: Vec<String> = ["all", "--delays", "2", "--max-runs", "9"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let options = parse(&args).unwrap();
        assert_eq!(options.scenarios.len(), SCENARIOS.len());
        assert_eq!((options.delays, options.max_runs), (Some(2), Some(9)));
        let replay: Vec<String> = ["exit-during-restart", "--schedule", "3=1,7=2"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(parse(&replay).unwrap().schedule.unwrap().choices, vec![(3, 1), (7, 2)]);
        assert!(parse(&["nope".to_owned()]).is_err());
    }
}
