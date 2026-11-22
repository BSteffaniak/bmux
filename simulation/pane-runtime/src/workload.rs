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

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use bmux_pane_runtime_plugin::simulation::{Pane, Panes, Restarted};
use bmux_simvar_tui::Tone;
use switchy_pty::simulator::{self, ProcessState};
use tokio::sync::watch;

use crate::{
    profile::{Kind, Profile},
    telemetry::{Board, PaneView},
};

/// The shell panes run.
pub const SHELL: &str = "/bin/sh";

/// How long a typed line or a resize has to reach the program, in simulated time, at a clock
/// of one millisecond per step (scaled with the clock).
const SETTLE: Duration = Duration::from_secs(5);

/// Draw a number below `bound` from the workload's stream.
pub fn draw(bound: u64) -> u64 {
    switchy_simulation::stream("pane_runtime_workload")
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .below(bound.max(1))
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
        if std::env::var_os("PANE_RUNTIME_SIM_STATS").is_some() {
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
    /// Programs it has run (one, plus one per restart).
    programs: u64,
    /// Whether a restart is under way.
    restarting: bool,
    /// Bumped by each resize, so a resize's check knows when a later one superseded it.
    resizes: u64,
}

/// A process started outside the pane runtime, after a crash.
struct Witness {
    serial: u64,
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
    /// Exit notices the runtime sent, per pane.
    exits: BTreeMap<Pane, u64>,
    violations: Vec<String>,
    stats: Stats,
    witnesses: Vec<Witness>,
}

/// One run's workload, shared by its drivers.
pub struct Shared {
    state: Mutex<State>,
    profile: Profile,
    /// The runtime directory's session records, which outlive a crashed server.
    records: PathBuf,
    /// Bumped when the server crashes; actions under way watch it.
    generation: watch::Sender<u64>,
    /// One crash at a time.
    crash: tokio::sync::Mutex<()>,
    /// Simulated time per unit of [`SETTLE`]: the profile's clock.
    settle: Duration,
}

/// Record that an invariant failed with `message` (which starts with the invariant's number):
/// as a violation of the run, in the dashboard's invariants, and to a dashboard breaking on
/// failures (which may pause here).
fn violated(violations: &mut Vec<String>, message: String) {
    Board::failed(&message);
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

/// The session serial of the process of `serial`, while it runs.
fn session_of(serial: u64) -> Option<u64> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|p| p.serial == serial && p.state == ProcessState::Running)
        .map(|p| p.session_serial)
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

/// Releases an open's place against the pane cap, however the open ends.
struct Opening<'a>(&'a Shared, u64);

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        // A crash resets the count for the new server: only an open of the current one counts.
        if *self.0.generation.borrow() == self.1 {
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
                panes: Arc::new(Panes::recovering(SHELL, &records)),
                open: BTreeMap::new(),
                opening: 0,
                steps_left: profile.steps,
                busy: 0,
                crashing: false,
                exits: BTreeMap::new(),
                violations: Vec::new(),
                stats: Stats::default(),
                witnesses: Vec::new(),
            }),
            profile,
            records,
            generation: watch::channel(0).0,
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

    /// Take the exit notices the runtime sent, checking that no program's exit is reported
    /// twice (invariant 8).
    fn collect_exits(&self) {
        let mut state = self.lock();
        let panes = Arc::clone(&state.panes);
        for pane in panes.take_exits() {
            let count = {
                let count = state.exits.entry(pane).or_default();
                *count += 1;
                *count
            };
            Board::checked(8);
            if let Some(opened) = state.open.get(&pane)
                && count > opened.programs
            {
                let message = format!(
                    "8. pane process {} got {count} exit notices for {} programs",
                    opened.pid, opened.programs
                );
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
                if state.crashing {
                    None
                } else if state.steps_left == 0 {
                    return;
                } else {
                    state.steps_left -= 1;
                    Some(self.profile.choose(draw))
                }
            };
            let Some(kind) = kind else {
                switchy_async::time::sleep(Duration::from_millis(1)).await;
                continue;
            };
            if std::env::var_os("PANE_RUNTIME_SIM_TRACE").is_some() {
                eprintln!("driver {driver} chose {}", kind.name());
            }
            if kind == Kind::Crash {
                self.crash(driver).await;
            } else {
                self.cancellable(driver, kind).await;
            }
            self.collect_exits();
        }
    }

    /// Run `kind`, cancelled if the server crashes meanwhile.
    async fn cancellable(&self, driver: u64, kind: Kind) {
        let mut generation = self.generation.subscribe();
        let current = *generation.borrow_and_update();
        self.lock().busy += 1;
        let cancelled = switchy_async::select! {
            () = self.act(driver, kind) => false,
            _ = generation.wait_for(|g| *g != current) => true,
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
            Kind::Crash => {}
            Kind::Wait => switchy_async::time::sleep(Duration::from_millis(draw(50))).await,
        }
    }

    /// A pane chosen at random among those open and not restarting.
    fn pick(state: &State) -> Option<Pane> {
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

    /// Whether a fault the profile has on may make opening a pane fail.
    fn opens_may_fail(&self) -> bool {
        let faults = &self.profile.faults;
        faults.spawn > 0.0 || faults.kill > 0.0 || faults.read_error > 0.0
    }

    /// Open a pane, checking invariant 1.
    async fn open(&self, driver: u64) {
        let (panes, generation) = {
            let mut state = self.lock();
            if state.open.len() + state.opening >= self.profile.panes {
                return;
            }
            state.opening += 1;
            (Arc::clone(&state.panes), *self.generation.borrow())
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
        let serial = simulator::serial_of(pid).unwrap_or(0);
        // 1. Its program runs and leads its own session.
        Board::checked(1);
        match alive(serial) {
            Some((_, session, _)) if session == pid => {}
            None if killed_by_fault(serial) => {}
            other => self.violated(format!(
                "1. pane process {pid} does not lead its own session: {other:?}"
            )),
        }
        sim_support::event(
            "open",
            format!("d{driver}: pane {pid} opened at {rows}x{cols}"),
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
        state.open.insert(
            pane,
            Opened {
                pid,
                serial,
                typed: Vec::new(),
                programs: 1,
                restarting: false,
                resizes: 0,
            },
        );
    }

    /// Type a random line into a pane (sometimes `bg`, starting a job that ignores hang-ups).
    /// Atomic with respect to other drivers: the pane cannot change between choosing it and
    /// recording the line.
    fn type_line(&self, driver: u64) {
        let mut state = self.lock();
        let Some(pane) = Self::pick(&state) else {
            return;
        };
        let line: String = if draw(5) == 0 {
            "bg".to_owned()
        } else {
            (0..=draw(20))
                .map(|_| char::from(b'a' + u8::try_from(draw(26)).unwrap_or(0)))
                .collect()
        };
        if !state.panes.input(pane, format!("{line}\r").as_bytes()) {
            return;
        }
        state.stats.typed += 1;
        let Some(opened) = state.open.get_mut(&pane) else {
            return;
        };
        sim_support::event(
            "type",
            format!("d{driver}: typed {line:?} into {}", opened.pid),
            Tone::Neutral,
        );
        opened.typed.push(line);
        Board::typed(&format!("{pane:?}"));
    }

    /// Resize a pane, checking invariant 3.
    async fn resize(&self, driver: u64) {
        let (pane, rows, cols, epoch, panes) = {
            let mut state = self.lock();
            let Some(pane) = Self::pick(&state) else {
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
            sim_support::event(
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
            let Some(pane) = Self::pick(&state) else {
                return;
            };
            let Some(opened) = state.open.remove(&pane) else {
                return;
            };
            state.stats.closed += 1;
            (pane, opened, Arc::clone(&state.panes))
        };
        sim_support::event(
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
                "2. pane process {pid} never answered all of {:?}",
                opened.typed
            ));
        }
        let session = session_of(opened.serial);
        match panes.close(pane).await {
            Ok(closed) => {
                // 4. Closing drains, cleans up, and leaves nothing running in the session.
                Board::checked(4);
                if let Some(error) = &closed.cleanup_error {
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
    }

    /// Restart a pane, checking invariant 7.
    async fn restart(&self, driver: u64) {
        let (pane, old, old_session, panes) = {
            let mut state = self.lock();
            let Some(pane) = Self::pick(&state) else {
                return;
            };
            let panes = Arc::clone(&state.panes);
            let Some(opened) = state.open.get_mut(&pane) else {
                return;
            };
            let Some(old_session) = session_of(opened.serial) else {
                return;
            };
            // Lines typed from now on go to the new program.
            opened.restarting = true;
            opened.typed.clear();
            (pane, opened.pid, old_session, panes)
        };
        let restarted = panes.restart(pane).await;
        let pid = match restarted {
            Ok(Restarted::Running(pid)) => pid,
            Ok(Restarted::Gone) => return,
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
        let serial = simulator::serial_of(pid).unwrap_or(0);
        {
            let mut state = self.lock();
            state.stats.restarted += 1;
            let Some(opened) = state.open.get_mut(&pane) else {
                return;
            };
            opened.pid = pid;
            opened.serial = serial;
            opened.programs += 1;
            opened.restarting = false;
        }
        sim_support::event(
            "restart",
            format!("d{driver}: pane {old} restarted as {pid}"),
            Tone::Info,
        );
        // 7. The old session ends; the new program leads its own.
        Board::checked(7);
        match alive(serial) {
            Some((_, session, _)) if session == pid => {}
            None if killed_by_fault(serial) => {}
            other => self.violated(format!(
                "7. restarted pane process {pid} does not lead its own session: {other:?}"
            )),
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
    async fn crash(&self, driver: u64) {
        let _one_at_a_time = self.crash.lock().await;
        let panes = {
            let mut state = self.lock();
            if state.open.is_empty() && state.opening == 0 {
                return;
            }
            state.crashing = true;
            Arc::clone(&state.panes)
        };
        sim_support::event("crash", format!("d{driver}: the server crashed"), Tone::Warn);
        // Die: nothing the old server still does ends a session, and its work under way stops.
        panes.abandon();
        self.generation.send_modify(|generation| *generation += 1);
        while self.lock().busy > 0 {
            switchy_async::time::sleep(Duration::from_millis(1)).await;
        }
        // Everything running now that no outsider started belongs to the dead server.
        let outsiders: Vec<u64> = self.lock().witnesses.iter().map(|w| w.serial).collect();
        let owned: Vec<u64> = simulator::snapshot()
            .processes
            .iter()
            .filter(|p| p.state == ProcessState::Running && !outsiders.contains(&p.serial))
            .map(|p| p.session_serial)
            .collect();
        panes.crash().await;
        drop(panes);
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
            if let Some(serial) = simulator::serial_of(child.id()) {
                witnesses.push(Witness {
                    serial,
                    _child: child,
                    _pair: pair,
                });
            }
        }
        let fresh = Arc::new(Panes::recovering(SHELL, &self.records));
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
        let survivors = running_in(&owned);
        if !survivors.is_empty() {
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
            self.close(&panes, pane, &opened).await;
            Board::closed(&format!("{pane:?}"));
        }
        self.collect_exits();
        self.lock().stats.record();
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
            let left: Vec<String> = switchy_thread::simulator::scheduler()
                .snapshot()
                .threads
                .into_iter()
                .skip(1)
                .filter(|thread| thread.state != switchy_thread::simulator::ThreadState::Finished)
                .map(|thread| format!("{} ({:?})", thread.name, thread.state))
                .collect();
            self.violated(format!(
                "5. threads still running after every pane closed: {left:?}"
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
