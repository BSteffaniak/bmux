//! Deterministic simulation testing for the pane runtime.
//!
//! Each simvar run starts a pane runtime (the real `bmux_pane_runtime_plugin` code, through its
//! `simulation` driver) whose panes run a simulated shell on switchy's simulated kernel, which
//! behaves as Linux, macOS or Windows (drawn from the seed, or `SIMULATOR_PTY_PERSONALITY`) on
//! any host, so the runtime takes that system's paths: on Windows its pseudo-console startup
//! handshake, console closing, and job-based stops. Then the run does random things to it: opens panes, types lines into them, resizes and closes them, and
//! waits. Its tasks, threads, timers, terminals and processes all belong to the run, so the seed
//! decides every interleaving and a failing run replays exactly.
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

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

pub mod telemetry;

use std::{collections::BTreeMap, io, time::Duration};

use bmux_pane_runtime_plugin::simulation::{Pane, Panes};
use simvar::{Sim, SimBootstrap, SimConfig};
use switchy_pty::simulator::{self, Config, Faults, Process, ProcessState};
use telemetry::{Board, PaneView};

/// Simulated time the workload has to finish in; more means a livelock.
const BUDGET: Duration = Duration::from_secs(300);
/// How long a typed line or a resize has to reach the program, in simulated time.
const SETTLE: Duration = Duration::from_secs(5);

/// The environment every run sees, whatever the host's.
const ENVIRONMENT: &[(&str, &str)] = &[
    ("HOME", "/home/sim"),
    ("PATH", "/usr/bin:/bin"),
    ("SHELL", "/bin/sh"),
    ("TERM", "xterm-256color"),
];

/// The shell panes run.
const SHELL: &str = "/bin/sh";

/// The simvar bootstrap: one client per run drives the workload.
#[derive(Debug, Default, Clone, Copy)]
pub struct PaneRuntimeBootstrap;

impl SimBootstrap for PaneRuntimeBootstrap {
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

/// The simulated shell the panes run: it answers each line it reads with `got:<line>`, starts
/// a job that ignores hang-ups in its session at `bg`, and ends at `exit` or end of input.
fn shell(process: &mut Process) -> i32 {
    let mut line = Vec::new();
    let mut buffer = [0_u8; 256];
    loop {
        match process.read(&mut buffer) {
            Ok(0) | Err(_) => return 0,
            Ok(read) => {
                for &byte in &buffer[..read] {
                    if byte != b'\n' {
                        line.push(byte);
                        continue;
                    }
                    let text = String::from_utf8_lossy(&line).into_owned();
                    line.clear();
                    if text == "exit" {
                        return 0;
                    }
                    if text == "bg" && process.spawn(&["ignore-hup"], false).is_err() {
                        return 1;
                    }
                    if process.write(format!("got:{text}\n").as_bytes()).is_err() {
                        return 1;
                    }
                }
            }
        }
    }
}

fn draw(bound: u64) -> u64 {
    switchy_simulation::stream("pane_runtime_workload")
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .below(bound.max(1))
}

fn random_size() -> (u16, u16) {
    let rows = u16::try_from(5 + draw(60)).unwrap_or(24);
    let cols = u16::try_from(20 + draw(200)).unwrap_or(80);
    (rows, cols)
}

/// With `PANE_RUNTIME_SIM_TRACE`, print every scheduling decision (to find where two runs of a
/// seed part).
fn trace_decisions() {
    if std::env::var_os("PANE_RUNTIME_SIM_TRACE").is_none() {
        return;
    }
    // With `PANE_RUNTIME_SIM_BACKTRACE_AT=N`, where the stopping thread was at decision N.
    let backtrace_at: Option<u64> = std::env::var("PANE_RUNTIME_SIM_BACKTRACE_AT")
        .ok()
        .and_then(|value| value.parse().ok());
    switchy_thread::simulator::set_decision_hook(Some(std::sync::Arc::new(
        move |decision: &switchy_thread::simulator::Decision,
              snapshot: &dyn Fn() -> switchy_thread::simulator::SchedulerSnapshot| {
            if backtrace_at == Some(decision.number) {
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
            }
            let threads = snapshot().threads;
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

/// Record that an invariant failed with `message` (which starts with the invariant's number):
/// as a violation of the run, in the dashboard's invariants, and to a dashboard breaking on
/// failures (which may pause here).
fn violated(violations: &mut Vec<String>, message: String) {
    Board::failed(&message);
    sim_support::failure(&message);
    violations.push(message);
}

/// What a run did, recorded in its trace (and printed with `PANE_RUNTIME_SIM_STATS`).
#[derive(Debug, Default)]
struct Stats {
    opened: u64,
    typed: u64,
    resized: u64,
    closed: u64,
    crashed: u64,
    restarted: u64,
}

impl Stats {
    fn record(&self) {
        for value in [
            self.opened,
            self.typed,
            self.resized,
            self.closed,
            self.crashed,
            self.restarted,
        ] {
            simvar::record(value);
        }
        if std::env::var_os("PANE_RUNTIME_SIM_STATS").is_some() {
            eprintln!("pane-runtime stats: {self:?}");
        }
    }
}

/// What the workload knows about one open pane.
struct Opened {
    pid: u32,
    typed: Vec<String>,
}

/// The kernel's view of `pid`: its session and terminal, while it runs.
fn running(pid: u32) -> Option<(u32, Option<String>)> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|process| process.pid == pid && process.state == ProcessState::Running)
        .map(|process| (process.session, process.terminal))
}

/// Wait (in simulated time) until `ready` holds, at most [`SETTLE`]; whether it did.
async fn settle(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = switchy_time::Instant::now() + SETTLE;
    while !ready() {
        if switchy_time::Instant::now() >= deadline {
            return false;
        }
        switchy_async::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

/// Whether `pid`'s terminal shows each of `lines` answered, in order.
fn answered(pid: u32, lines: &[String]) -> bool {
    let Some(terminal) = simulator::process_io(pid).and_then(|io| io.terminal) else {
        return lines.is_empty();
    };
    let written =
        String::from_utf8_lossy(&simulator::terminal_written(&terminal).unwrap_or_default())
            .into_owned();
    let mut rest = written.as_str();
    lines.iter().all(|line| {
        let answer = format!("got:{line}\r\n");
        rest.find(&answer).is_some_and(|at| {
            rest = &rest[at + answer.len()..];
            true
        })
    })
}

/// One run's state: the pane runtime, its open panes, and what went wrong.
struct Workload {
    panes: Panes,
    open: BTreeMap<Pane, Opened>,
    violations: Vec<String>,
    stats: Stats,
    /// The runtime directory's session records, which outlive a crashed server.
    records: tempfile::TempDir,
    /// Processes no pane runtime owns, with their terminals, kept until the run ends.
    witnesses: Vec<Witness>,
}

/// A process started outside the pane runtime, after a crash.
struct Witness {
    serial: u64,
    _child: switchy_pty::PtyChild,
    _pair: switchy_pty::PtyPair,
}

/// The processes running in the sessions `serials` name.
fn running_in(serials: &[u64]) -> Vec<u32> {
    simulator::snapshot()
        .processes
        .into_iter()
        .filter(|p| p.state == ProcessState::Running && serials.contains(&p.session_serial))
        .map(|p| p.pid)
        .collect()
}

impl Workload {
    /// Open a pane, checking invariant 1.
    async fn open_pane(&mut self) {
        let (rows, cols) = random_size();
        match self.panes.open(rows, cols).await {
            Ok(pane) => {
                let Some(pid) = self.panes.pid(pane) else {
                    violated(
                        &mut self.violations,
                        "1. an opened pane has no process".into(),
                    );
                    return;
                };
                // 1. Its program runs and leads its own session.
                Board::checked(1);
                match running(pid) {
                    Some((session, _)) if session == pid => {}
                    other => violated(
                        &mut self.violations,
                        format!("1. pane process {pid} does not lead its own session: {other:?}"),
                    ),
                }
                self.stats.opened += 1;
                Board::pane(PaneView {
                    pane: format!("{pane:?}"),
                    pid,
                    size: (rows, cols),
                    typed: 0,
                    open: true,
                });
                self.open.insert(
                    pane,
                    Opened {
                        pid,
                        typed: Vec::new(),
                    },
                );
            }
            Err(error) => violated(
                &mut self.violations,
                format!("1. opening a pane failed: {error:#}"),
            ),
        }
    }

    /// Type a random line into a pane (sometimes `bg`, starting a job that ignores hang-ups).
    fn type_line(&mut self) {
        let index = usize::try_from(draw(3)).unwrap_or(0) % self.open.len().max(1);
        if let Some((&pane, opened)) = self.open.iter_mut().nth(index) {
            let line: String = if draw(5) == 0 {
                "bg".to_owned()
            } else {
                (0..=draw(20))
                    .map(|_| char::from(b'a' + u8::try_from(draw(26)).unwrap_or(0)))
                    .collect()
            };
            if self.panes.input(pane, format!("{line}\r").as_bytes()) {
                self.stats.typed += 1;
                opened.typed.push(line);
                Board::typed(&format!("{pane:?}"));
            }
        }
    }

    /// Resize a pane, checking invariant 3.
    async fn resize(&mut self) {
        let Some((&pane, opened)) = self.open.iter().next() else {
            return;
        };
        let pid = opened.pid;
        let (rows, cols) = random_size();
        if !self.panes.resize(pane, rows, cols) {
            return;
        }
        self.stats.resized += 1;
        Board::resized(&format!("{pane:?}"), (rows, cols));
        // 3. The terminal ends up the requested size.
        Board::checked(3);
        let sized = settle(|| {
            let terminal = running(pid).and_then(|(_, terminal)| terminal);
            simulator::snapshot().terminals.iter().any(|t| {
                terminal.as_deref() == Some(t.name.as_str())
                    && (t.size.rows, t.size.cols) == (rows, cols)
            })
        })
        .await;
        if !sized && running(pid).is_some() {
            violated(
                &mut self.violations,
                format!("3. pane process {pid}'s terminal never became {rows}x{cols}"),
            );
        }
    }

    /// Close the first open pane, if any.
    async fn close_one(&mut self) {
        if let Some((pane, opened)) = self.open.pop_first() {
            self.stats.closed += 1;
            close(&mut self.panes, pane, &opened, &mut self.violations).await;
            Board::closed(&format!("{pane:?}"));
        }
    }

    /// Restart a pane, checking invariant 7.
    async fn restart(&mut self) {
        let index = usize::try_from(draw(3)).unwrap_or(0) % self.open.len().max(1);
        let Some((&pane, opened)) = self.open.iter_mut().nth(index) else {
            return;
        };
        let old = opened.pid;
        let Some(old_session) = simulator::snapshot()
            .processes
            .into_iter()
            .find(|p| p.pid == old && p.state == ProcessState::Running)
            .map(|p| p.session_serial)
        else {
            return;
        };
        if let Err(error) = self.panes.restart(pane).await {
            violated(
                &mut self.violations,
                format!("7. restarting pane process {old} failed: {error:#}"),
            );
            return;
        }
        self.stats.restarted += 1;
        // 7. The old session ends; the new program leads its own.
        Board::checked(7);
        let Some(pid) = self.panes.pid(pane) else {
            return;
        };
        opened.pid = pid;
        opened.typed.clear();
        match running(pid) {
            Some((session, _)) if session == pid => {}
            other => violated(
                &mut self.violations,
                format!("7. restarted pane process {pid} does not lead its own session: {other:?}"),
            ),
        }
        if !settle(|| running_in(&[old_session]).is_empty()).await {
            violated(
                &mut self.violations,
                format!(
                    "7. pane process {old}'s session still runs {:?} after a restart",
                    running_in(&[old_session])
                ),
            );
        }
    }

    /// Crash the server, start processes it never owned, then start a new server on the same
    /// runtime directory, checking invariant 6.
    async fn crash(&mut self) {
        let snapshot = simulator::snapshot();
        let owned: Vec<u64> = self
            .open
            .values()
            .filter_map(|opened| {
                snapshot
                    .processes
                    .iter()
                    .find(|p| p.pid == opened.pid && p.state == ProcessState::Running)
                    .map(|p| p.session_serial)
            })
            .collect();
        self.panes.crash().await;
        for (pane, _) in std::mem::take(&mut self.open) {
            Board::closed(&format!("{pane:?}"));
        }
        self.stats.crashed += 1;
        // Processes started while the crashed sessions' may linger (their ids may be reused).
        for _ in 0..=draw(2) {
            let Ok(pair) = switchy_pty::PtyPair::open(switchy_pty::PtySize::new(80, 24)) else {
                continue;
            };
            let Ok(child) = pair
                .slave
                .spawn(switchy_pty::SpawnCommand::new("sleep").arg("3600000"))
            else {
                continue;
            };
            if let Some(serial) = simulator::serial_of(child.id()) {
                self.witnesses.push(Witness {
                    serial,
                    _child: child,
                    _pair: pair,
                });
            }
        }
        self.panes = Panes::recovering(SHELL, self.records.path());
        // 6. Nothing the crashed server owned runs; everything else does.
        Board::checked(6);
        let survivors = running_in(&owned);
        if !survivors.is_empty() {
            violated(
                &mut self.violations,
                format!("6. the crashed server's sessions still run {survivors:?} after recovery"),
            );
        }
        let serials: Vec<u64> = simulator::snapshot()
            .processes
            .iter()
            .filter(|p| p.state == ProcessState::Running)
            .map(|p| p.serial)
            .collect();
        for witness in &self.witnesses {
            if !serials.contains(&witness.serial) {
                violated(
                    &mut self.violations,
                    format!(
                        "6. recovery ended process serial {} it never owned",
                        witness.serial
                    ),
                );
            }
        }
    }

    /// Close every pane, end every process, and check invariant 5's threads.
    async fn finish(mut self) -> Result<(), Vec<String>> {
        while !self.open.is_empty() {
            self.close_one().await;
        }
        self.stats.record();
        let _ = self.panes.take_exits();
        simulator::kill_all();
        self.witnesses.clear();
        // 5. Let the panes' threads see their processes end before the run does.
        Board::checked(5);
        let drained = settle(|| {
            switchy_thread::simulator::try_scheduler()
                .is_none_or(|scheduler| !scheduler.others_alive())
        })
        .await;
        if !drained {
            let left: Vec<String> = switchy_thread::simulator::scheduler()
                .snapshot()
                .threads
                .into_iter()
                .skip(1)
                .filter(|thread| thread.state != switchy_thread::simulator::ThreadState::Finished)
                .map(|thread| format!("{} ({:?})", thread.name, thread.state))
                .collect();
            violated(
                &mut self.violations,
                format!("5. threads still running after every pane closed: {left:?}"),
            );
        }
        if self.violations.is_empty() {
            Ok(())
        } else {
            Err(self.violations)
        }
    }
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
    switchy_time::simulator::set_step_multiplier(1);
    trace_decisions();
    sim_support::watch_run();
    // The run's own environment, so nothing depends on the host it runs on.
    switchy_env::simulator::isolate(
        ENVIRONMENT
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string())),
    );
    simulator::configure(Config {
        personality: Config::from_env().personality,
        pid_reuse: 0.3,
        faults: Faults::default(),
    });
    simulator::install_default_programs();
    simulator::register_program("sh", shell);

    let deadline = switchy_time::Instant::now() + BUDGET;
    let records = tempfile::tempdir().expect("a directory for session records");
    let mut workload = Workload {
        panes: Panes::recovering(SHELL, records.path()),
        open: BTreeMap::new(),
        violations: Vec::new(),
        stats: Stats::default(),
        records,
        witnesses: Vec::new(),
    };
    for _ in 0..5 + draw(25) {
        if switchy_time::Instant::now() >= deadline {
            violated(
                &mut workload.violations,
                format!("5. the workload did not finish within {BUDGET:?} (livelock?)"),
            );
            break;
        }
        match draw(16) {
            0..=2 if workload.open.len() < 3 => workload.open_pane().await,
            3..=6 => workload.type_line(),
            7 | 8 => workload.resize().await,
            9 | 10 => workload.close_one().await,
            11 if !workload.open.is_empty() => workload.crash().await,
            12 if !workload.open.is_empty() => workload.restart().await,
            _ => switchy_async::time::sleep(Duration::from_millis(draw(50))).await,
        }
    }
    workload.finish().await
}

/// Close `pane`, checking invariants 2 and 4.
async fn close(panes: &mut Panes, pane: Pane, opened: &Opened, violations: &mut Vec<String>) {
    // 2. Every typed line reached the program, in order (unless it ended meanwhile).
    Board::checked(2);
    let pid = opened.pid;
    let reached = settle(|| answered(pid, &opened.typed) || running(pid).is_none()).await;
    if !reached {
        violated(
            violations,
            format!(
                "2. pane process {pid} never answered all of {:?}",
                opened.typed
            ),
        );
    }
    let session = running(pid).map(|(session, _)| session);
    match panes.close(pane).await {
        Ok(closed) => {
            // 4. Closing drains, cleans up, and leaves nothing running in the session.
            Board::checked(4);
            if let Some(error) = &closed.cleanup_error {
                violated(
                    violations,
                    format!("4. closing pane process {pid} failed to clean up: {error}"),
                );
            }
            if let Some(session) = session {
                let survivors: Vec<u32> = simulator::snapshot()
                    .processes
                    .into_iter()
                    .filter(|p| p.session == session && p.state == ProcessState::Running)
                    .map(|p| p.pid)
                    .collect();
                if !survivors.is_empty() {
                    violated(
                        violations,
                        format!(
                            "4. session {session} still runs {survivors:?} after its pane closed"
                        ),
                    );
                }
            }
        }
        Err(error) => violated(violations, format!("4. closing a pane failed: {error:#}")),
    }
}
