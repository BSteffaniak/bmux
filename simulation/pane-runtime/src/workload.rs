//! The workload: drivers acting on one pane runtime at once, as a server's requests do, and
//! the invariants each action checks.
//!
//! Drivers share a [`Shared`]: the runtime ([`Panes`], swapped for a new one when the server
//! crashes) and what the workload knows about each pane, behind a lock held only between
//! awaits. Each action takes what it needs, awaits, then checks what still applies: a pane
//! another driver closed or restarted meanwhile is recognised, not reported.
//!
//! A crash stops the old server mid-flight: every other driver's action under way is
//! cancelled at its next await (dropped, as a dying server's tasks are), then the old runtime
//! is torn down and a new one recovers from the same runtime directory.
//!
//! Drivers draw their actions from the profile, or follow scripts ([`Step`]s, for the
//! exploration scenarios; see [`crate::explore`]): the same actions and checks, on panes named
//! by the order they opened, so only the schedule varies between runs.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, PoisonError,
        atomic::{AtomicU64, Ordering}, // simulation-safe: the run's own server generation
    },
    time::Duration,
};

// The run's own lock: drivers may run on several workers at once, so waiting for it must be a
// switch point the scheduler controls (a std lock would block the whole run).
use switchy_thread::sync::{Mutex, MutexGuard};
use bmux_pane_runtime_plugin::simulation::{Pane, Panes, Restarted};
use bmux_simvar_tui::Tone;
use switchy_pty::simulator::{self, ProcessState};

use crate::{
    profile::{Kind, Profile},
    telemetry::{Board, PaneView},
};

/// The shell panes run.
pub const SHELL: &str = "/bin/sh";

/// How long a typed line or a resize has to reach the program, in simulated time, at a clock
/// of one millisecond per step (scaled with the clock).
const SETTLE: Duration = Duration::from_secs(5);

/// How long one action may take, in simulated time at a clock of one millisecond per step
/// (scaled with the clock).
const ACTION_LIMIT: Duration = Duration::from_secs(60);

/// Draw a number below `bound` from the workload's stream.
pub fn draw(bound: u64) -> u64 {
    switchy_simulation::stream("pane_runtime_workload")
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .below(bound.max(1))
}

/// One scripted action: what, on which pane (by the order panes opened, splits included), and
/// for [`Kind::Type`], the line.
///
/// A script may act on a pane mid-restart, or on one closed meanwhile, which drawn actions never
/// do: that is what the scenarios race. A line typed into a pane mid-restart is not expected
/// back (which program reads it is undecided), and a restart of a pane closed meanwhile must
/// leave nothing behind (invariant 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The action.
    pub kind: Kind,
    /// Its pane, by the order panes opened.
    pub pane: usize,
    /// For [`Kind::Type`], the line typed (else a drawn one).
    pub line: Option<&'static str>,
}

impl Step {
    /// `kind` on the first pane opened.
    #[must_use]
    pub const fn new(kind: Kind) -> Self {
        Self {
            kind,
            pane: 0,
            line: None,
        }
    }

    /// On the pane opened `pane`th (from 0).
    #[must_use]
    pub const fn on(mut self, pane: usize) -> Self {
        self.pane = pane;
        self
    }

    /// Typing `line`.
    #[must_use]
    pub const fn line(mut self, line: &'static str) -> Self {
        self.line = Some(line);
        self
    }
}

fn random_size() -> (u16, u16) {
    let rows = u16::try_from(5 + draw(60)).unwrap_or(24);
    let cols = u16::try_from(20 + draw(200)).unwrap_or(80);
    (rows, cols)
}

/// What a run did, recorded in its trace (and printed with `PANE_RUNTIME_SIM_STATS`).
#[derive(Debug, Default)]
pub struct Stats {
    opened: u64,
    typed: u64,
    resized: u64,
    closed: u64,
    crashed: u64,
    restarted: u64,
    cancelled: u64,
    failed_opens: u64,
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
            self.cancelled,
            self.failed_opens,
        ] {
            simvar::record(value);
        }
        if crate::host_set("PANE_RUNTIME_SIM_STATS") {
            eprintln!("pane-runtime stats: {self:?}");
        }
    }
}

/// What the workload knows about one open pane.
#[derive(Debug)]
struct Opened {
    /// Its program's id and serial (ids are reused; serials are not).
    pid: u32,
    serial: u64,
    /// Lines typed into its current program.
    typed: Vec<String>,
    /// Programs it has started (one, plus one per restart begun).
    programs: u64,
    /// Whether a restart is under way.
    restarting: bool,
    /// Bumped by each resize, so a resize's check knows when a later one superseded it.
    resizes: u64,
    /// Bumped by whatever may signal its program (an interrupt, a restart), so a check of
    /// what another action did to it knows when one interfered.
    touched: u64,
}

/// A process started outside the pane runtime, after a crash.
struct Witness {
    serial: u64,
    /// Its terminal's name.
    terminal: String,
    _child: switchy_pty::PtyChild,
    _pair: switchy_pty::PtyPair,
}

/// The workload's state, shared by its drivers.
struct State {
    panes: Arc<Panes>,
    open: BTreeMap<Pane, Opened>,
    /// Opens under way, against the pane cap.
    opening: usize,
    /// Actions left to take, across drivers.
    steps_left: u64,
    /// Actions under way that a crash cancels.
    busy: u64,
    /// A crash is under way: drivers start nothing until it is done.
    crashing: bool,
    /// Exits the runtime acted on, per pane.
    exits: BTreeMap<Pane, u64>,
    violations: Vec<String>,
    stats: Stats,
    witnesses: Vec<Witness>,
    /// Panes in the order they opened (splits included), for scripts.
    order: Vec<Pane>,
    /// Each scripted driver's steps left; `None` when drivers draw their actions.
    scripts: Option<Vec<std::collections::VecDeque<Step>>>,
    /// The step each scripted driver is taking.
    targets: BTreeMap<u64, Step>,
}

/// One run's workload, shared by its drivers.
pub struct Shared {
    state: Mutex<State>,
    profile: Profile,
    /// The runtime directory's session records, which outlive a crashed server.
    records: PathBuf,
    /// Bumped when the server crashes; actions under way watch it. (Not a `tokio` watch:
    /// with several receivers, it wakes them in an order drawn from `tokio`'s own per-thread
    /// generator, which no seed decides.)
    generation: AtomicU64,
    /// One crash at a time.
    crash: tokio::sync::Mutex<()>,
    /// Simulated time per unit of [`SETTLE`]: the profile's clock.
    settle: Duration,
}

/// Record an event on the run's timeline (and print it with `PANE_RUNTIME_SIM_EVENTS`).
fn event(kind: &str, label: impl Into<String>, tone: Tone) {
    let label = label.into();
    if crate::host_set("PANE_RUNTIME_SIM_EVENTS") {
        eprintln!("[{:?}] {kind}: {label}", switchy_time::Instant::now());
    }
    sim_support::event(kind, label, tone);
}

/// Record that an invariant failed with `message` (which starts with the invariant's number):
/// as a violation of the run, in the dashboard's invariants, and to a dashboard breaking on
/// failures (which may pause here).
fn violated(violations: &mut Vec<String>, message: String) {
    Board::failed(&message);
    if crate::host_set("PANE_RUNTIME_SIM_EVENTS") {
        eprintln!("[{:?}] VIOLATION: {message}", switchy_time::Instant::now());
    }
    sim_support::failure(&message);
    violations.push(message);
}

/// The process of `serial`, while it runs: its id, session and terminal.
fn alive(serial: u64) -> Option<(u32, u32, Option<String>)> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|p| p.serial == serial && p.state == ProcessState::Running)
        .map(|p| (p.pid, p.session, p.terminal))
}

/// The session of the process of `serial`, running or exited but not yet reaped.
fn leader_of(serial: u64) -> Option<u32> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|p| p.serial == serial)
        .map(|p| p.session)
}

/// The session serial of the process of `serial`, while it runs.
fn session_of(serial: u64) -> Option<u64> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|p| p.serial == serial && p.state == ProcessState::Running)
        .map(|p| p.session_serial)
}

/// The serials of daemons panes' programs detached (which outlive their panes by design): by
/// the kernel's facts, not the programs' word (a program killed as it detaches one never
/// records it). Started by a process, leading a session of its own, with no terminal.
fn detached() -> Vec<u64> {
    simulator::snapshot()
        .processes
        .into_iter()
        .filter(|p| {
            let started_by_a_process = p.parent.is_some();
            let without_a_terminal = p.terminal.is_none();
            let leads_its_session = p.session_serial == p.serial;
            started_by_a_process && without_a_terminal && leads_its_session
        })
        .map(|p| p.serial)
        .collect()
}

/// Whether something outside (the kill fault) killed the process of `serial`.
fn killed_by_fault(serial: u64) -> bool {
    simulator::signal_log()
        .iter()
        .any(|record| record.serial == serial && record.by_fault)
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

/// The run's simulated threads that have not finished (but the main thread), with what each
/// waits for, for a failure's message.
fn live_threads() -> Vec<String> {
    switchy_thread::simulator::scheduler()
        .snapshot()
        .threads
        .into_iter()
        .skip(1)
        .filter(|thread| {
            !thread.daemon && thread.state != switchy_thread::simulator::ThreadState::Finished
        })
        .map(|thread| format!("{} ({:?})", thread.name, thread.state))
        .collect()
}

/// The name of `pid`'s terminal, for events.
fn terminal_of(pid: u32) -> String {
    simulator::process_io(pid)
        .and_then(|io| io.terminal)
        .unwrap_or_else(|| "no terminal".into())
}

/// `pid`'s terminal as the kernel sees it, for a failure's message.
fn terminal_state(pid: u32) -> String {
    let Some(name) = simulator::process_io(pid).and_then(|io| io.terminal) else {
        return "no terminal".into();
    };
    simulator::snapshot()
        .terminals
        .into_iter()
        .find(|t| t.name == name)
        .map_or_else(
            || format!("terminal {name} gone"),
            |t| {
                let written = simulator::terminal_written(&name).unwrap_or_default();
                let tail = &written[written.len().saturating_sub(48)..];
                format!(
                    "terminal {name}: {} bytes unread by the pane, {} typed bytes unread by the \
                     program, {} written, ending \"{}\", masters {}, hung up {}",
                    t.output_buffered,
                    t.input_buffered,
                    t.written,
                    tail.escape_ascii(),
                    t.master_handles,
                    t.hung_up
                )
            },
        )
}

/// Whether `pid`'s terminal shows each of `lines` answered, in order.
fn answered(pid: u32, lines: &[String]) -> bool {
    let Some(terminal) = simulator::process_io(pid).and_then(|io| io.terminal) else {
        return lines.is_empty();
    };
    // An interrupt typed while a program reads raw input (a querier waiting for its answers)
    // is an ordinary byte, so it joins the next line.
    let written =
        String::from_utf8_lossy(&simulator::terminal_written(&terminal).unwrap_or_default())
            .replace('\x03', "");
    let mut rest = written.as_str();
    lines.iter().all(|line| {
        let answer = format!("got:{line}\r\n");
        rest.find(&answer).is_some_and(|at| {
            rest = &rest[at + answer.len()..];
            true
        })
    })
}

/// Releases an open's place against the pane cap, however the open ends.
struct Opening<'a>(&'a Shared, u64);

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        // A crash resets the count for the new server: only an open of the current one counts.
        if self.0.generation.load(Ordering::SeqCst) == self.1 {
            let mut state = self.0.lock();
            state.opening = state.opening.saturating_sub(1);
        }
    }
}

// Each locked section is a deliberate unit (choose a pane and act on it, or update what is
// known about one) that no other driver may interleave with; tightening the guards as the
// lint suggests would split them.
#[allow(clippy::significant_drop_tightening)]
impl Shared {
    /// A workload doing `profile` on a pane runtime recording sessions under `records`.
    #[must_use]
    pub fn new(profile: Profile, records: PathBuf) -> Arc<Self> {
        let settle = SETTLE * u32::try_from(profile.clock).unwrap_or(1);
        Arc::new(Self {
            state: Mutex::new(State {
                panes: {
                    let panes = Arc::new(Panes::recovering(SHELL, &records));
                    panes.set_sweep_on_exit(profile.sweep);
                    panes
                },
                open: BTreeMap::new(),
                opening: 0,
                steps_left: profile.steps,
                busy: 0,
                crashing: false,
                exits: BTreeMap::new(),
                violations: Vec::new(),
                stats: Stats::default(),
                witnesses: Vec::new(),
                order: Vec::new(),
                scripts: None,
                targets: BTreeMap::new(),
            }),
            profile,
            records,
            generation: AtomicU64::new(0),
            crash: tokio::sync::Mutex::new(()),
            settle,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn violated(&self, message: String) {
        violated(&mut self.lock().violations, message);
    }

    /// Wait (in simulated time) until `ready` holds, at most the settling time; whether it did.
    async fn settle(&self, mut ready: impl FnMut() -> bool) -> bool {
        let deadline = switchy_time::Instant::now() + self.settle;
        while !ready() {
            if switchy_time::Instant::now() >= deadline {
                return false;
            }
            switchy_async::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }

    /// Take the exits the runtime acted on, checking that it acts on each program's exit at
    /// most once, and never on that of a program a restart replaced (invariant 8).
    fn collect_exits(&self) {
        let mut state = self.lock();
        let panes = Arc::clone(&state.panes);
        for pane in panes.take_exits() {
            let Some(opened) = state.open.get(&pane) else {
                // A closed pane's last word.
                continue;
            };
            let (pid, programs) = (opened.pid, opened.programs);
            event("exit", format!("pane {pid} exited"), Tone::Info);
            Board::checked(8);
            let count = {
                let count = state.exits.entry(pane).or_default();
                *count += 1;
                *count
            };
            if count > programs {
                let message =
                    format!("8. pane process {pid} got {count} exits for {programs} programs");
                violated(&mut state.violations, message);
            }
        }
    }

    /// One driver: take actions until the steps run out.
    pub async fn drive(self: Arc<Self>, driver: u64, deadline: switchy_time::Instant) {
        loop {
            if switchy_time::Instant::now() >= deadline {
                self.violated(format!(
                    "5. the workload did not finish in time (driver {driver}; livelock?)"
                ));
                return;
            }
            let kind = {
                let mut state = self.lock();
                let kind = if state.crashing {
                    None
                } else if let Some(scripts) = &mut state.scripts {
                    let Some(step) = usize::try_from(driver)
                        .ok()
                        .and_then(|index| scripts.get_mut(index))
                        .and_then(std::collections::VecDeque::pop_front)
                    else {
                        return;
                    };
                    state.targets.insert(driver, step);
                    Some(step.kind)
                } else if state.steps_left == 0 {
                    return;
                } else {
                    state.steps_left -= 1;
                    Some(self.profile.choose(draw))
                };
                // Under way from now on, so a crash beginning after this waits for it (or
                // cancels it); counted later, a crash could pass it and tear the server down
                // under it.
                kind.map(|kind| (kind, self.begin(&mut state, kind)))
            };
            let Some((kind, generation)) = kind else {
                switchy_async::time::sleep(Duration::from_millis(1)).await;
                continue;
            };
            if crate::host_set("PANE_RUNTIME_SIM_TRACE") {
                eprintln!("driver {driver} chose {}", kind.name());
            }
            // Debugging replays that part: a draw from the run's generator (taken, so the
            // probed run differs from an unprobed one, but alike in every probed replay).
            if crate::host_set("PANE_RUNTIME_SIM_RNG_PROBE") {
                eprintln!(
                    "probe driver {driver} {} rng {}",
                    kind.name(),
                    simvar::switchy::random::simulator::rng().next_u64()
                );
            }
            self.take(driver, kind, generation).await;
        }
    }

    /// Count `kind` under way (unless it crashes the server), with the server generation it
    /// acts on, which a crash ends.
    fn begin(&self, state: &mut State, kind: Kind) -> u64 {
        if !matches!(kind, Kind::Crash | Kind::PowerLoss) {
            state.busy += 1;
        }
        self.generation.load(Ordering::SeqCst)
    }

    /// Take one action begun (see [`Self::begin`]) on server generation `generation`, as
    /// driver `driver`.
    async fn take(&self, driver: u64, kind: Kind, generation: u64) {
        if kind == Kind::Crash || kind == Kind::PowerLoss {
            self.crash(driver, kind == Kind::PowerLoss).await;
        } else {
            self.cancellable(driver, kind, generation).await;
        }
        self.collect_exits();
    }

    /// Follow scripts from now on: driver `n` takes `scripts[n]`'s steps, then stops.
    pub fn script(&self, scripts: Vec<Vec<Step>>) {
        self.lock().scripts = Some(scripts.into_iter().map(Into::into).collect());
    }

    /// Take `steps` one after another, as driver 0, before the drivers start (a scenario's
    /// setup).
    pub async fn setup(&self, steps: &[Step]) {
        for step in steps {
            let generation = {
                let mut state = self.lock();
                state.targets.insert(0, *step);
                self.begin(&mut state, step.kind)
            };
            self.take(0, step.kind, generation).await;
        }
        self.lock().targets.remove(&0);
    }

    /// Run `kind` (begun on server generation `current`), cancelled if the server crashes
    /// meanwhile.
    async fn cancellable(&self, driver: u64, kind: Kind, current: u64) {
        let crashed = async {
            while self.generation.load(Ordering::SeqCst) == current {
                switchy_async::time::sleep(Duration::from_millis(1)).await;
            }
        };
        // An action that never ends is a livelock (or a lost wakeup) in the runtime.
        let limit = ACTION_LIMIT * u32::try_from(self.profile.clock).unwrap_or(1);
        let cancelled = switchy_async::select! {
            () = self.act(driver, kind) => false,
            () = crashed => true,
            () = switchy_async::time::sleep(limit) => {
                self.violated(format!(
                    "5. d{driver}: {} did not finish within {limit:?} (threads: {:?})",
                    kind.name(),
                    live_threads()
                ));
                false
            }
        };
        let mut state = self.lock();
        state.busy -= 1;
        if cancelled {
            state.stats.cancelled += 1;
        }
    }

    async fn act(&self, driver: u64, kind: Kind) {
        match kind {
            Kind::Open => self.open(driver).await,
            Kind::Type => self.type_line(driver),
            Kind::Resize => self.resize(driver).await,
            Kind::Close => self.close_one(driver).await,
            Kind::Restart => self.restart(driver).await,
            Kind::Crash | Kind::PowerLoss => {}
            Kind::Split => self.split(driver).await,
            Kind::Interrupt => self.interrupt(driver),
            Kind::Wait => switchy_async::time::sleep(Duration::from_millis(draw(50))).await,
        }
    }

    /// The pane `driver`'s script names (open or not), or else one chosen at random among
    /// those open and not restarting.
    fn pick(state: &State, driver: u64) -> Option<Pane> {
        if let Some(step) = state.targets.get(&driver) {
            return state.order.get(step.pane).copied();
        }
        let candidates: Vec<Pane> = state
            .open
            .iter()
            .filter(|(_, opened)| !opened.restarting)
            .map(|(pane, _)| *pane)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let index = usize::try_from(draw(candidates.len() as u64)).unwrap_or(0);
        candidates.get(index).copied()
    }

    /// What is wrong, if anything, with `pane`'s program (`pid`, `serial`) leading its own
    /// session: it must, unless it already ended and the runtime knows (or the kill fault
    /// killed it).
    async fn leads_own_session(
        &self,
        panes: &Panes,
        pane: Pane,
        pid: u32,
        serial: u64,
    ) -> Option<String> {
        match leader_of(serial) {
            Some(session) if session == pid => None,
            Some(session) => Some(format!("it is in session {session}")),
            None if killed_by_fault(serial) => None,
            // Reaped: the runtime notices a moment later.
            None if self.settle(|| panes.exited(pane) || panes.pid(pane) != Some(pid)).await => {
                None
            }
            None => Some("it is gone and the pane never noticed".into()),
        }
    }

    /// Whether a fault the profile has on may make opening a pane fail, or a program it may
    /// run exits before its pane is ready (the runtime then refuses the pane).
    fn opens_may_fail(&self) -> bool {
        let faults = &self.profile.faults;
        faults.spawn > 0.0
            || faults.thread_spawn > 0.0
            || faults.kill > 0.0
            || faults.read_error > 0.0
            || faults.file > 0
            || self.profile.program_mix().iter().any(|(kind, _)| {
                matches!(
                    kind,
                    crate::programs::Kind::Instant | crate::programs::Kind::Orphaner
                )
            })
    }

    /// Open a pane, checking invariant 1.
    async fn open(&self, driver: u64) {
        let (panes, generation) = {
            let mut state = self.lock();
            if state.open.len() + state.opening >= self.profile.panes {
                return;
            }
            state.opening += 1;
            (Arc::clone(&state.panes), self.generation.load(Ordering::SeqCst))
        };
        let _opening = Opening(self, generation);
        let (rows, cols) = random_size();
        let pane = match panes.open(rows, cols).await {
            Ok(pane) => pane,
            Err(error) => {
                self.lock().stats.failed_opens += 1;
                if !self.opens_may_fail() {
                    self.violated(format!("1. opening a pane failed: {error:#}"));
                }
                return;
            }
        };
        let Some(pid) = panes.pid(pane) else {
            self.violated("1. an opened pane has no process".into());
            return;
        };
        // By the runtime's own record: the id may already be another process's.
        let serial = panes
            .program_serial(pane)
            .or_else(|| simulator::serial_of(pid))
            .unwrap_or(0);
        // 1. Its program leads its own session (and if it already ended, the runtime knows).
        Board::checked(1);
        if let Some(problem) = self.leads_own_session(&panes, pane, pid, serial).await {
            self.violated(format!("1. pane process {pid} does not lead its own session: {problem}"));
        }
        event(
            "open",
            format!(
                "d{driver}: pane {pid} opened at {rows}x{cols} on {}",
                terminal_of(pid)
            ),
            Tone::Info,
        );
        Board::pane(PaneView {
            pane: format!("{pane:?}"),
            pid,
            size: (rows, cols),
            typed: 0,
            open: true,
        });
        let mut state = self.lock();
        state.stats.opened += 1;
        state.order.push(pane);
        state.open.insert(
            pane,
            Opened {
                pid,
                serial,
                typed: Vec::new(),
                programs: 1,
                restarting: false,
                resizes: 0,
                touched: 0,
            },
        );
    }

    /// Type a random line into a pane (sometimes `bg`, starting a job that ignores hang-ups).
    /// Atomic with respect to other drivers: the pane cannot change between choosing it and
    /// recording the line.
    fn type_line(&self, driver: u64) {
        let mut state = self.lock();
        let Some(pane) = Self::pick(&state, driver) else {
            return;
        };
        let scripted = state.targets.get(&driver).and_then(|step| step.line);
        let line: String = scripted.map_or_else(
            || {
                if draw(5) == 0 {
                    "bg".to_owned()
                } else if draw(20) == 0 {
                    // The shell ends: the pane stays, its program exited.
                    "exit".to_owned()
                } else {
                    (0..=draw(20))
                        .map(|_| char::from(b'a' + u8::try_from(draw(26)).unwrap_or(0)))
                        .collect()
                }
            },
            str::to_owned,
        );
        if !state.panes.input(pane, format!("{line}\r").as_bytes()) {
            return;
        }
        state.stats.typed += 1;
        let Some(opened) = state.open.get_mut(&pane) else {
            return;
        };
        event(
            "type",
            format!("d{driver}: typed {line:?} into {}", opened.pid),
            Tone::Neutral,
        );
        // Mid-restart (only scripts type then), the old program or the new one may read it.
        if opened.restarting {
            return;
        }
        opened.typed.push(line);
        Board::typed(&format!("{pane:?}"));
    }

    /// Split a pane: its session gains a pane running a new program, checking invariant 1.
    async fn split(&self, driver: u64) {
        let (pane, panes) = {
            let mut state = self.lock();
            if state.open.len() + state.opening >= self.profile.panes {
                return;
            }
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            state.opening += 1;
            (pane, Arc::clone(&state.panes))
        };
        let _opening = Opening(self, self.generation.load(Ordering::SeqCst));
        let (new, pid) = match panes.split(pane, draw(2) == 0).await {
            Ok((new, Restarted::Running(pid))) => (new, pid),
            Ok((_, Restarted::Gone)) => return,
            Ok((_, Restarted::Failed)) | Err(_) => {
                self.lock().stats.failed_opens += 1;
                // A split of a pane closed meanwhile, or a program failing to start, is refused.
                return;
            }
        };
        let serial = panes
            .program_serial(new)
            .or_else(|| simulator::serial_of(pid))
            .unwrap_or(0);
        Board::checked(1);
        if let Some(problem) = self.leads_own_session(&panes, new, pid, serial).await {
            self.violated(format!("1. split pane process {pid} does not lead its own session: {problem}"));
        }
        event(
            "split",
            format!("d{driver}: pane {pid} split off on {}", terminal_of(pid)),
            Tone::Info,
        );
        let mut state = self.lock();
        state.stats.opened += 1;
        state.order.push(new);
        state.open.insert(
            new,
            Opened {
                pid,
                serial,
                typed: Vec::new(),
                programs: 1,
                restarting: false,
                resizes: 0,
                touched: 0,
            },
        );
    }

    /// Interrupt a pane's program (`ctrl+c`): its foreground program usually ends.
    fn interrupt(&self, driver: u64) {
        let mut state = self.lock();
        let Some(pane) = Self::pick(&state, driver) else {
            return;
        };
        if state.panes.input(pane, b"\x03")
            && let Some(opened) = state.open.get_mut(&pane)
        {
            opened.touched += 1;
            event("interrupt", format!("d{driver}: interrupted {}", opened.pid), Tone::Neutral);
        }
    }

    /// Resize a pane, checking invariant 3.
    async fn resize(&self, driver: u64) {
        let (pane, rows, cols, epoch, panes) = {
            let mut state = self.lock();
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            let (rows, cols) = random_size();
            if !state.panes.resize(pane, rows, cols) {
                return;
            }
            let panes = Arc::clone(&state.panes);
            state.stats.resized += 1;
            let Some(opened) = state.open.get_mut(&pane) else {
                return;
            };
            opened.resizes += 1;
            event(
                "resize",
                format!("d{driver}: pane {} resized to {rows}x{cols}", opened.pid),
                Tone::Neutral,
            );
            (pane, rows, cols, opened.resizes, panes)
        };
        Board::resized(&format!("{pane:?}"), (rows, cols));
        // 3. The terminal ends up the requested size, unless something superseded the resize:
        // a later one, a restart, a close, or the program ending.
        Board::checked(3);
        let current = || {
            let state = self.lock();
            state
                .open
                .get(&pane)
                .filter(|opened| opened.resizes == epoch && !opened.restarting)
                .map(|opened| opened.serial)
        };
        let sized = self
            .settle(|| {
                let Some(serial) = current() else {
                    return true;
                };
                let Some((_, _, Some(terminal))) = alive(serial) else {
                    return true;
                };
                simulator::snapshot().terminals.iter().any(|t| {
                    t.name == terminal && (t.size.rows, t.size.cols) == (rows, cols)
                })
            })
            .await;
        if !sized && current().is_some_and(|serial| alive(serial).is_some()) {
            let pid = panes.pid(pane).unwrap_or(0);
            self.violated(format!(
                "3. pane process {pid}'s terminal never became {rows}x{cols}"
            ));
        }
    }

    /// Close a pane, checking invariants 2 and 4.
    async fn close_one(&self, driver: u64) {
        let (pane, opened, panes) = {
            let mut state = self.lock();
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            let Some(opened) = state.open.remove(&pane) else {
                return;
            };
            state.stats.closed += 1;
            (pane, opened, Arc::clone(&state.panes))
        };
        event(
            "close",
            format!("d{driver}: pane {} closing", opened.pid),
            Tone::Neutral,
        );
        self.close(&panes, pane, &opened).await;
        Board::closed(&format!("{pane:?}"));
    }

    /// Close `pane` (already out of the open set), checking invariants 2 and 4.
    async fn close(&self, panes: &Panes, pane: Pane, opened: &Opened) {
        // 2. Every typed line reached the program, in order (unless it ended meanwhile).
        Board::checked(2);
        let pid = opened.pid;
        let reached = self
            .settle(|| answered(pid, &opened.typed) || alive(opened.serial).is_none())
            .await;
        if !reached {
            self.violated(format!(
                "2. pane process {pid} ({}) never answered all of {:?} ({})",
                crate::programs::kind_of(opened.serial).map_or("unknown program", crate::programs::Kind::name),
                opened.typed,
                terminal_state(pid)
            ));
        }
        // 10. A pane whose program ended says so.
        if alive(opened.serial).is_none() {
            Board::checked(10);
            if !self.settle(|| panes.exited(pane) || panes.pid(pane) != Some(pid)).await {
                self.violated(format!(
                    "10. pane process {pid} ({}) ended but its pane never said so",
                    crate::programs::kind_of(opened.serial)
                        .map_or("unknown program", crate::programs::Kind::name)
                ));
            }
        }
        let session = session_of(opened.serial);
        // 11. Closing a pane leaves its siblings (its session's other panes) alone.
        let siblings = self.siblings_of(panes, pane);
        let signalled_before: usize = simulator::signal_log().len();
        match panes.close(pane).await {
            Ok(closed) => {
                // 4. Closing drains, cleans up, and leaves nothing running in the session.
                Board::checked(4);
                // (A failing disk may keep a session's record from being deleted.)
                if let Some(error) = closed
                    .cleanup_error
                    .as_ref()
                    .filter(|_| self.profile.faults.file == 0)
                {
                    self.violated(format!(
                        "4. closing pane process {pid} failed to clean up: {error}"
                    ));
                }
                if let Some(session) = session {
                    let ended = self.settle(|| running_in(&[session]).is_empty()).await;
                    if !ended {
                        self.violated(format!(
                            "4. pane process {pid}'s session still runs {:?} after it closed",
                            running_in(&[session])
                        ));
                    }
                }
            }
            Err(error) => self.violated(format!("4. closing a pane failed: {error:#}")),
        }
        self.check_siblings(pid, session, siblings, signalled_before).await;
    }

    /// The panes `pane`'s session has besides it, with their programs (id, serial) and how
    /// often they were touched, for invariant 11.
    fn siblings_of(&self, panes: &Panes, pane: Pane) -> Vec<(Pane, u32, u64, u64)> {
        let state = self.lock();
        panes
            .siblings(pane)
            .into_iter()
            .filter(|other| *other != pane)
            .filter_map(|other| {
                state
                    .open
                    .get(&other)
                    .map(|o| (other, o.pid, o.serial, o.touched))
            })
            .collect()
    }

    /// Invariant 11: the close of pane process `pid` (whose session was `session`; signals from
    /// index `signalled_before` of the log on) sent nothing that ends a sibling.
    async fn check_siblings(
        &self,
        pid: u32,
        session: Option<u64>,
        siblings: Vec<(Pane, u32, u64, u64)>,
        signalled_before: usize,
    ) {
        // Only when attribution is exact: no other driver acts meanwhile, and the siblings run
        // shells that end only when told to (so any signal that ends one is the close's).
        // (Scripts also attribute exactly: what another driver does to a sibling touches it.)
        let scripted = self.lock().scripts.is_some();
        let siblings: Vec<_> = if self.profile.drivers == 1 || scripted {
            let state = self.lock();
            siblings
                .into_iter()
                .filter(|(other, _, serial, _)| {
                    matches!(
                        crate::programs::kind_of(*serial),
                        Some(crate::programs::Kind::Shell | crate::programs::Kind::Stubborn)
                    ) && state
                        .open
                        .get(other)
                        .is_some_and(|o| !o.typed.iter().any(|line| line == "exit"))
                })
                .collect()
        } else {
            Vec::new()
        };
        if !siblings.is_empty() {
            Board::checked(11);
            // Let the close finish (another pane's closes in the background).
            let _ = self.settle(|| session.is_none_or(|s| running_in(&[s]).is_empty())).await;
            // Only what would end them counts, and only for siblings nothing else touched
            // meanwhile (another driver may interrupt, restart or close them).
            // (A restart under way when the close began ends the sibling's old program: it then
            // runs another.)
            let untouched = |other: Pane, serial: u64, touched: u64| {
                self.lock().open.get(&other).is_some_and(|o| {
                    o.touched == touched && !o.restarting && o.serial == serial
                })
            };
            for record in simulator::signal_log().iter().skip(signalled_before) {
                if record.by_fault
                    || !matches!(
                        record.signal,
                        switchy_pty::Signal::Kill
                            | switchy_pty::Signal::Hangup
                            | switchy_pty::Signal::Terminate
                    )
                {
                    continue;
                }
                if let Some((_, sibling, _, _)) = siblings.iter().find(|(other, _, serial, touched)| {
                    *serial == record.serial && untouched(*other, *serial, *touched)
                }) {
                    self.violated(format!(
                        "11. closing pane process {pid} sent {:?} to its sibling pane's process {sibling}",
                        record.signal
                    ));
                }
            }
        }
    }

    /// Restart a pane, checking invariant 7.
    async fn restart(&self, driver: u64) {
        let begun = 'begin: {
            let mut state = self.lock();
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            let panes = Arc::clone(&state.panes);
            let Some(opened) = state.open.get_mut(&pane) else {
                break 'begin Err((pane, panes));
            };
            let Some(old_session) = session_of(opened.serial) else {
                return;
            };
            // Lines typed from now on go to the new program.
            opened.restarting = true;
            opened.touched += 1;
            opened.programs += 1;
            opened.typed.clear();
            Ok((pane, opened.pid, old_session, panes))
        };
        let (pane, old, old_session, panes) = match begun {
            Ok(begun) => begun,
            Err((pane, panes)) => {
                // A scripted restart of a pane closed (or closing) meanwhile: the runtime may
                // refuse it or restart it, but nothing it starts may outlive the pane
                // (invariant 9, at the end).
                let restarted = panes.restart(pane).await;
                event(
                    "restart",
                    format!("d{driver}: restarting a closed pane: {restarted:?}"),
                    Tone::Neutral,
                );
                return;
            }
        };
        let restarted = panes.restart(pane).await;
        let pid = match restarted {
            Ok(Restarted::Running(pid)) => pid,
            Ok(Restarted::Gone) => return,
            // A scripted close of the pane meanwhile ends the restart too.
            Ok(Restarted::Failed) | Err(_) if !self.lock().open.contains_key(&pane) => return,
            Ok(Restarted::Failed) => {
                if !self.opens_may_fail() {
                    self.violated(format!(
                        "7. restarting pane process {old}: its new program failed to start"
                    ));
                }
                if let Some(opened) = self.lock().open.get_mut(&pane) {
                    opened.restarting = false;
                }
                return;
            }
            Err(error) => {
                if !self.opens_may_fail() {
                    self.violated(format!(
                        "7. restarting pane process {old} failed: {error:#}"
                    ));
                }
                if let Some(opened) = self.lock().open.get_mut(&pane) {
                    opened.restarting = false;
                }
                return;
            }
        };
        let serial = panes
            .program_serial(pane)
            .or_else(|| simulator::serial_of(pid))
            .unwrap_or(0);
        {
            let mut state = self.lock();
            state.stats.restarted += 1;
            let Some(opened) = state.open.get_mut(&pane) else {
                return;
            };
            opened.pid = pid;
            opened.serial = serial;
            opened.restarting = false;
        }
        event(
            "restart",
            format!(
                "d{driver}: pane {old} restarted as {pid} on {}",
                terminal_of(pid)
            ),
            Tone::Info,
        );
        // 7. The old session ends; the new program leads its own.
        Board::checked(7);
        if let Some(problem) = self.leads_own_session(&panes, pane, pid, serial).await {
            self.violated(format!("7. restarted pane process {pid} does not lead its own session: {problem}"));
        }
        if !self.settle(|| running_in(&[old_session]).is_empty()).await {
            self.violated(format!(
                "7. pane process {old}'s session still runs {:?} after a restart",
                running_in(&[old_session])
            ));
        }
    }

    /// Crash the server, start processes it never owned, then start a new server on the same
    /// runtime directory, checking invariant 6.
    async fn crash(&self, driver: u64, power_loss: bool) {
        let _one_at_a_time = self.crash.lock().await;
        let panes = {
            let mut state = self.lock();
            if state.open.is_empty() && state.opening == 0 {
                return;
            }
            state.crashing = true;
            Arc::clone(&state.panes)
        };
        if power_loss {
            event("power", format!("d{driver}: the power failed"), Tone::Warn);
        } else {
            event("crash", format!("d{driver}: the server crashed"), Tone::Warn);
        }
        // The drivers' work under way stops with the server.
        self.generation.fetch_add(1, Ordering::SeqCst);
        while self.lock().busy > 0 {
            switchy_async::time::sleep(Duration::from_millis(1)).await;
        }
        // Die: every task and thread of the server stops where it is.
        panes.crash();
        drop(panes);
        let filesystem = switchy_fs::simulator::current_filesystem();
        if power_loss {
            // Every process dies, and the disk keeps what was made durable and a random part
            // of the rest.
            simulator::kill_all();
            self.lock().witnesses.clear();
            filesystem.crash(&switchy_fs::simulator::CrashPolicy::power_loss());
        } else {
            // The dead server's open files close; what it wrote stays.
            filesystem.crash(&switchy_fs::simulator::CrashPolicy::process_crash());
        }
        // Everything running now that no outsider started belongs to the dead server.
        // (So do the daemons panes' programs detached, by design.)
        let mut outsiders: Vec<u64> = self.lock().witnesses.iter().map(|w| w.serial).collect();
        outsiders.extend(detached());
        let owned: Vec<u64> = simulator::snapshot()
            .processes
            .iter()
            .filter(|p| p.state == ProcessState::Running && !outsiders.contains(&p.serial))
            .map(|p| p.session_serial)
            .collect();
        // Processes started while the crashed sessions linger (their ids may be reused).
        let mut witnesses = Vec::new();
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
            let terminal = simulator::process_io(child.id())
                .and_then(|io| io.terminal)
                .unwrap_or_default();
            if let Some(serial) = simulator::serial_of(child.id()) {
                witnesses.push(Witness {
                    serial,
                    terminal,
                    _child: child,
                    _pair: pair,
                });
            }
        }
        let fresh = Arc::new(Panes::recovering(SHELL, &self.records));
        fresh.set_sweep_on_exit(self.profile.sweep);
        let outside: Vec<u64> = {
            let mut state = self.lock();
            for pane in state.open.keys() {
                Board::closed(&format!("{pane:?}"));
            }
            state.open.clear();
            state.opening = 0;
            state.panes = fresh;
            state.crashing = false;
            state.stats.crashed += 1;
            state.witnesses.extend(witnesses);
            state.witnesses.iter().map(|w| w.serial).collect()
        };
        // 6. Nothing the crashed server owned runs; everything else does.
        Board::checked(6);
        // (Programs the dead server's closed terminals hung up on take a moment to end.)
        let ended = self.settle(|| running_in(&owned).is_empty()).await;
        let survivors = running_in(&owned);
        if !ended {
            self.violated(format!(
                "6. the crashed server's sessions still run {survivors:?} after recovery"
            ));
        }
        let running: Vec<u64> = simulator::snapshot()
            .processes
            .iter()
            .filter(|p| p.state == ProcessState::Running)
            .map(|p| p.serial)
            .collect();
        for serial in outside {
            if !running.contains(&serial) && !killed_by_fault(serial) {
                self.violated(format!(
                    "6. recovery ended process serial {serial} it never owned"
                ));
            }
        }
    }

    /// Invariant 9: with every pane closed, nothing the runtime had is left: no terminal's
    /// master side open, no process running but those started outside it, no session record.
    async fn check_nothing_leaks(&self) {
        Board::checked(9);
        let mut outsiders: Vec<u64> = self.lock().witnesses.iter().map(|w| w.serial).collect();
        outsiders.extend(detached());
        let strays = || -> Vec<String> {
            simulator::snapshot()
                .processes
                .into_iter()
                .filter(|p| p.state == ProcessState::Running && !outsiders.contains(&p.serial))
                .map(|p| format!("{} ({})", p.pid, p.program))
                .collect()
        };
        // Processes a close or a crash ended may take a moment to exit.
        if !self.settle(|| strays().is_empty()).await {
            self.violated(format!(
                "9. processes still run with every pane closed: {:?}",
                strays()
            ));
        }
        let outsider_terminals: Vec<String> =
            self.lock().witnesses.iter().map(|w| w.terminal.clone()).collect();
        let open = || -> Vec<String> {
            simulator::snapshot()
                .terminals
                .into_iter()
                // The outsiders' own terminals stay open until the run ends.
                .filter(|t| t.master_handles > 0 && !outsider_terminals.contains(&t.name))
                .map(|t| format!("{} ({} master handles)", t.name, t.master_handles))
                .collect()
        };
        if !self.settle(|| open().is_empty()).await {
            self.violated(format!(
                "9. terminals' master sides still open with every pane closed: {:?} (threads: \
                 {:?})",
                open(),
                live_threads()
            ));
        }
        let records: Vec<String> = switchy_fs::sync::read_dir_sorted(&self.records)
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        if !records.is_empty() && self.profile.faults.file == 0 {
            self.violated(format!(
                "9. session records left with every pane closed: {records:?}"
            ));
        }
    }

    /// Close every pane, end every process, and check invariant 5's threads. Returns the
    /// violations found, and the run's shape tags.
    pub async fn finish(&self) -> (Vec<String>, Stats) {
        loop {
            let next = {
                let mut state = self.lock();
                let next = state.open.pop_first();
                next.map(|(pane, opened)| (pane, opened, Arc::clone(&state.panes)))
            };
            let Some((pane, opened, panes)) = next else {
                break;
            };
            let limit = ACTION_LIMIT * u32::try_from(self.profile.clock).unwrap_or(1);
            let closed = switchy_async::select! {
                () = self.close(&panes, pane, &opened) => true,
                () = switchy_async::time::sleep(limit) => false,
            };
            if !closed {
                self.violated(format!(
                    "5. closing pane process {} at the end did not finish within {limit:?} \
                     (threads: {:?})",
                    opened.pid,
                    live_threads()
                ));
            }
            Board::closed(&format!("{pane:?}"));
        }
        self.collect_exits();
        self.lock().stats.record();
        // While the server still runs, so cleanup under way can finish.
        self.check_nothing_leaks().await;
        let panes = Arc::clone(&self.lock().panes);
        panes.shut_down();
        drop(panes);
        simulator::kill_all();
        self.lock().witnesses.clear();
        // 5. Let the panes' threads see their processes end before the run does.
        Board::checked(5);
        let drained = self
            .settle(|| {
                switchy_thread::simulator::try_scheduler()
                    .is_none_or(|scheduler| !scheduler.others_alive())
            })
            .await;
        if !drained {
            self.violated(format!(
                "5. threads still running after every pane closed: {:?}",
                live_threads()
            ));
        }
        let mut state = self.lock();
        (
            std::mem::take(&mut state.violations),
            std::mem::take(&mut state.stats),
        )
    }
}

impl Stats {
    /// What the run did, coarsely (each count bucketed by powers of two), for guided runs to
    /// tell new behaviour from more of the same.
    #[must_use]
    pub fn outcome(&self) -> Vec<String> {
        let bucket = |count: u64| match count {
            0 => 0,
            count => 1 + count.ilog2(),
        };
        [
            ("opened", self.opened),
            ("typed", self.typed),
            ("resized", self.resized),
            ("closed", self.closed),
            ("crashed", self.crashed),
            ("restarted", self.restarted),
            ("cancelled", self.cancelled),
            ("failed_opens", self.failed_opens),
        ]
        .into_iter()
        .map(|(name, count)| format!("{name}:{}", bucket(count)))
        .collect()
    }

    /// Shape tags for what the run did.
    #[must_use]
    pub fn tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        if self.crashed > 0 {
            tags.push("crash".into());
        }
        if self.restarted > 0 {
            tags.push("restart".into());
        }
        if self.cancelled > 0 {
            tags.push("cancelled".into());
        }
        if self.failed_opens > 0 {
            tags.push("failed open".into());
        }
        if self.opened >= 3 {
            tags.push("3+ panes".into());
        }
        tags
    }
}
