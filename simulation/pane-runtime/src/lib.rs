//! Deterministic simulation testing for the pane runtime.
//!
//! Each simvar run starts a pane runtime (the real `bmux_pane_runtime_plugin` code, through its
//! `simulation` driver) whose panes run a simulated shell on switchy's simulated kernel, which
//! behaves as Linux, macOS or Windows (drawn from the seed, or `SIMULATOR_PTY_PERSONALITY`) on
//! any host, so the runtime takes that system's paths: on Windows its pseudo-console startup
//! handshake, console closing, and job-based stops. Then the run does random things to it:
//! opens panes, types lines into them, resizes, restarts and closes them, crashes the server,
//! and waits. Its tasks, threads, timers, terminals and processes all belong to the run, so the
//! seed decides every interleaving and a failing run replays exactly.
//!
//! Runs differ by more than their interleavings (see [`profile`]): each draws which actions it
//! takes and how often, how many drivers take them at once (1 to 4, overlapping as a server's
//! requests do; see [`workload`]), which kernel faults strike, its scheduling strategy and its
//! clock rate.
//!
//! Invariants checked:
//!
//! 1. An opened pane's program runs and leads a session of its own.
//! 2. Every line typed into a pane reaches its program, in order.
//! 3. A pane's terminal ends up the size last requested.
//! 4. Closing a pane drains it, cleans up, and leaves nothing running in its session.
//! 5. The workload finishes within a budget of simulated time (no livelock), with no deadlock
//!    or leaked simulated thread (checked by the scheduler and simvar).
//! 6. After the server crashes (it stops cleaning up, and its handles close) and a new one
//!    starts on the same runtime directory, no process of a session the crashed server owned
//!    runs (Unix: the new server sweeps the recorded sessions, including jobs that ignore
//!    hang-ups; Windows: closing the jobs ended them), and every process it never owned, ids
//!    reused or not, still runs.
//! 7. Restarting a pane ends everything in its old program's session and starts a new program
//!    leading a session of its own.
//! 8. The runtime acts on each program's exit at most once, and never on that of a program a
//!    restart replaced.
//! 9. Once every pane closed, nothing is left: no terminal's master side open, no process but
//!    those started outside the runtime, no session record.
//! 10. A pane whose program ended says so.
//! 11. Closing a pane leaves its siblings (its session's other panes, from splits) alone.
//! 12. A pane runtime activated on the server's runtime directory while it runs (as a command
//!     run in a client does) kills nothing the server runs.
//! 13. A pane's screen is what its input made it: its grid shows what a fresh grid shows after
//!     the same output and resizes, in the same order (nothing changes it outside its input).
//! 14. A pane takes in what its terminal gave it, in order: what its grid took in is, but for
//!     the runtime's own notices, a prefix of every byte the terminal gave its master side (its
//!     programs' output and its echoes).
//! 15. A client's replica of a pane shows what the pane's grid shows: a client observing a
//!     pane (as an unattached client does) applies every frame it is sent, and brought up to
//!     date as the pane closes, its replica's screen is the pane's.
//!
//! Panes run programs that misbehave as real ones do (see [`programs`]): they ignore
//! termination, detach daemons, leave children holding the terminal, exit at once, flood
//! output, write garbage, query the terminal, or switch screens.
//!
//! Faults excuse only what they cause: a pane whose program the outside killer killed need
//! not answer what was typed, and opening may fail while spawns, reads or kills fail.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

pub mod explore;
pub mod guide;
pub mod invariants;
pub mod metamorphic;
pub mod profile;
pub mod programs;
pub mod telemetry;
pub mod workload;

use std::{io, sync::Arc, time::Duration};

use simvar::{Sim, SimBootstrap, SimConfig};
use switchy_pty::simulator::{self, Config, Faults};

use profile::Profile;
use workload::Shared;

/// Simulated time the workload has to finish in, at a clock of one millisecond per step
/// (scaled with the clock); more means a livelock.
const BUDGET: Duration = Duration::from_secs(300);

/// The environment every run sees, whatever the host's.
const ENVIRONMENT: &[(&str, &str)] = &[
    ("HOME", "/home/sim"),
    ("PATH", "/usr/bin:/bin"),
    ("SHELL", "/bin/sh"),
    ("TERM", "xterm-256color"),
];

/// The simvar bootstrap: one client per run drives the workload.
#[derive(Debug, Default, Clone, Copy)]
pub struct PaneRuntimeBootstrap;

impl SimBootstrap for PaneRuntimeBootstrap {
    /// A guided run's profile is its input (see [`guide`]): reported as the run's
    /// `PANE_RUNTIME_SIM_PROFILE`, so its replay command carries it.
    fn props(&self) -> Vec<(String, String)> {
        if host_set("PANE_RUNTIME_SIM_PROFILE") {
            return Vec::new();
        }
        guide::profile_for(simvar::switchy::random::simulator::seed())
            .map(|profile| {
                vec![(
                    "PANE_RUNTIME_SIM_PROFILE".to_owned(),
                    profile.to_string().replace(' ', ","),
                )]
            })
            .unwrap_or_default()
    }

    fn build_sim(&self, mut config: SimConfig) -> SimConfig {
        config.duration = Duration::from_hours(24);
        config
    }

    fn on_start(&self, sim: &mut impl Sim) {
        sim.client("pane-runtime-workload", async move {
            run_workload().await.map_err(|violations| {
                Box::new(io::Error::other(violations.join("\n")))
                    as Box<dyn std::error::Error + Send>
            })
        });
    }
}

/// With `PANE_RUNTIME_SIM_TRACE`, print every scheduling decision (to find where two runs of a
/// seed part).
fn trace_decisions() {
    if !host_set("PANE_RUNTIME_SIM_TRACE") {
        return;
    }
    // With `PANE_RUNTIME_SIM_BACKTRACE_AT=N`, where the stopping thread was at decision N.
    let backtrace_at: Option<u64> = host_var("PANE_RUNTIME_SIM_BACKTRACE_AT")
        .and_then(|value| value.parse().ok());
    let threads_at: Option<u64> = host_var("PANE_RUNTIME_SIM_THREADS_AT")
        .and_then(|value| value.parse().ok());
    switchy_thread::simulator::set_decision_hook(Some(Arc::new(
        move |decision: &switchy_thread::simulator::Decision,
              snapshot: &dyn Fn() -> switchy_thread::simulator::SchedulerSnapshot| {
            if backtrace_at == Some(decision.number) {
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
            }
            let threads = snapshot().threads;
            // With `PANE_RUNTIME_SIM_THREADS_AT=N`, every thread's state at decision N.
            if threads_at == Some(decision.number) {
                for (index, thread) in threads.iter().enumerate() {
                    eprintln!("thread {index} {} {:?}", thread.name, thread.state);
                }
            }
            let name = |index: usize| threads.get(index).map_or("?", |t| t.name.as_str());
            eprintln!(
                "decision {} step {} {} {:?} -> {}",
                decision.number,
                decision.step,
                name(decision.from),
                decision.op,
                name(decision.to)
            );
        },
    )));
}

/// Whether the host's environment sets `name`: a debugging switch, or (`PANE_RUNTIME_SIM_PROFILE`,
/// `SIMULATOR_PTY_PERSONALITY`) a run's input, which its replay command carries. Read from the
/// host, not the run's sealed environment, on purpose.
pub(crate) fn host_set(name: &str) -> bool {
    std::env::var_os(name).is_some() // simulation-safe: a switch or input from the host, as above
}

/// The value of `name` in the host's environment, as [`host_set`] reads it.
pub(crate) fn host_var(name: &str) -> Option<String> {
    std::env::var(name).ok() // simulation-safe: a switch or input from the host, as above
}

/// The run's profile: `PANE_RUNTIME_SIM_PROFILE` when set, else the guide's (see [`guide`]),
/// else drawn from the seed.
///
/// # Panics
///
/// * If `PANE_RUNTIME_SIM_PROFILE` is not a profile
#[must_use]
pub fn run_profile() -> Profile {
    let profile = chosen_profile();
    if host_set(metamorphic::CALM) {
        profile.calm()
    } else {
        profile
    }
}

/// The profile before [`metamorphic::CALM`] calms it.
fn chosen_profile() -> Profile {
    if let Some(text) = host_var("PANE_RUNTIME_SIM_PROFILE") {
        return text
            .parse()
            .unwrap_or_else(|error| panic!("PANE_RUNTIME_SIM_PROFILE: {error}"));
    }
    if let Some(profile) = guide::profile_for(simvar::switchy::random::simulator::seed()) {
        return profile;
    }
    let stream = switchy_simulation::stream("pane_runtime_profile");
    Profile::draw(|bound| {
        stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .below(bound.max(1))
    })
}

/// The run's shape tags, as it starts: its kernel, schedule, clock, workers and profile.
fn run_shape(profile: &Profile) -> Vec<String> {
    let mut shape = vec![
        format!(
            "personality:{}",
            format!("{:?}", simulator::personality()).to_lowercase()
        ),
        format!(
            "schedule:{}",
            switchy_thread::simulator::scheduler().config().describe()
        ),
        format!("clock:{}", profile.clock),
        format!(
            "workers:{}",
            switchy_async::simulator::runtime::run_workers()
        ),
    ];
    shape.extend(profile.tags());
    if guide::enabled() {
        shape.push("guided".into());
    }
    shape
}

/// One run's workload. Returns the violations found, if any.
///
/// # Errors
///
/// * Every invariant violated, one line each
///
/// # Panics
///
/// * If no temporary directory can be created for the session records
pub async fn run_workload() -> Result<(), Vec<String>> {
    run(run_profile(), None).await
}

/// A run of `profile`'s workload, or of an exploration scenario's script (on its personality,
/// unless `SIMULATOR_PTY_PERSONALITY` says otherwise). Returns the violations found, if any.
///
/// # Errors
///
/// * Every invariant violated, one line each
///
/// # Panics
///
/// * If no temporary directory can be created for the session records
#[allow(clippy::too_many_lines)]
pub async fn run(profile: Profile, scenario: Option<&explore::Scenario>) -> Result<(), Vec<String>> {
    if host_set("PANE_RUNTIME_SIM_TRACE") {
        eprintln!("profile {profile}");
    }
    switchy_time::simulator::set_step_multiplier(profile.clock);
    // The faults of the runtime's own failure paths, before it starts anything. Set only when
    // on, so `SIMULATOR_THREAD_SPAWN_FAILURES` and `SIMULATOR_WALL_CLOCK_JUMPS` still apply
    // to runs whose profile has them off.
    if profile.faults.thread_spawn > 0.0 {
        switchy_thread::simulator::set_spawn_failures(profile.faults.thread_spawn);
    }
    if profile.faults.wall_clock > 0.0 {
        switchy_time::simulator::set_wall_clock_jumps(profile.faults.wall_clock);
    }
    trace_decisions();
    sim_support::watch_run();
    // The run's own environment, so nothing depends on the host it runs on.
    switchy_env::simulator::isolate(
        ENVIRONMENT
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string())),
    );
    let faults = profile.faults;
    let personality = match scenario {
        Some(scenario) if !host_set("SIMULATOR_PTY_PERSONALITY") => {
            scenario.personality
        }
        _ => Config::from_env().personality,
    };
    simulator::configure(Config {
        personality,
        pid_reuse: faults.pid_reuse,
        faults: Faults {
            interrupted: faults.interrupted,
            short: faults.short,
            spawn: faults.spawn,
            kill: faults.kill,
            signal_delay: faults.signal_delay,
            read_error: faults.read_error,
            write_stall: faults.write_stall,
            startup_query: faults.startup_query,
        },
    });
    simulator::install_default_programs();
    programs::set_mix(profile.program_mix());
    // The run's file system: the kernel's personality's, every operation a switch point, and
    // failing as often as the profile says.
    if simulator::personality().is_windows() {
        switchy_fs::simulator::set_run_personality(switchy_fs::simulator::Personality::Windows);
    }
    switchy_fs::simulator::set_operation_hook(Some(Arc::new(|_| {
        switchy_thread::simulator::scheduler().yield_point(switchy_thread::simulator::Op::Io);
    })));
    if faults.file > 0 {
        use switchy_fs::simulator::{Fault, FaultPlan, OpClass, Probability};
        let rate = Probability::one_in(faults.file);
        switchy_fs::simulator::current_filesystem().set_fault_plan(
            FaultPlan::new()
                .with(OpClass::Write, Fault::Io, rate)
                .with(OpClass::Write, Fault::NoSpace, rate)
                .with(OpClass::Write, Fault::ShortWrite, rate)
                .with(OpClass::Sync, Fault::Io, rate)
                .with(OpClass::Rename, Fault::Io, rate)
                .with(OpClass::Open, Fault::Io, rate),
        );
    }
    simulator::register_program("sh", programs::program);
    let mut shape = run_shape(&profile);
    sim_support::shape(shape.clone());
    sim_support::event("profile", profile.to_string(), bmux_simvar_tui::Tone::Neutral);

    let deadline =
        switchy_time::Instant::now() + BUDGET * u32::try_from(profile.clock).unwrap_or(1);
    // In the run's simulated file system, which outlives a crashed server like a disk.
    let records = switchy_fs::tempdir().expect("a directory for session records");
    let drivers = profile.drivers;
    let guided = guide::tracking().then(|| profile.clone());
    let shared = Shared::new(profile, records.path().to_path_buf());
    if let Some(scenario) = scenario {
        shared.setup(scenario.setup).await;
        explore::open_window();
        shared.script(scenario.drivers.iter().map(|steps| steps.to_vec()).collect());
    }
    let handles: Vec<_> = (0..drivers)
        .map(|driver| switchy_async::task::spawn(Arc::clone(&shared).drive(driver, deadline)))
        .collect();
    for handle in handles {
        if let Err(error) = handle.await {
            return Err(vec![format!("5. a driver panicked: {error}")]);
        }
    }
    let (violations, stats) = shared.finish().await;
    drop(shared);
    // A metamorphic run's summary, compared across personalities (see [`metamorphic`]).
    if host_set(metamorphic::SUMMARY) {
        println!(
            "{} {} passed={}",
            metamorphic::SUMMARY,
            stats.summary(),
            violations.is_empty()
        );
    }
    shape.extend(stats.tags());
    if let Some(profile) = &guided {
        let mut outcome = stats.outcome();
        outcome.push(format!("{:?}", simulator::personality()));
        guide::observe(profile, &outcome);
    }
    sim_support::shape(shape);
    sim_support::finish_run();
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}
