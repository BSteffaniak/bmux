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
use bmux_pane_runtime_plugin::simulation::{Pane, Panes};
use bmux_simvar_tui::Tone;
use switchy_pty::simulator;
use sim_support::invariants::checked;

use crate::{
    invariants::Invariant,
    profile::{Kind, Profile},
    telemetry::Board,
};

mod executions;
mod ledger;
mod panes;
mod server;

pub use ledger::Stats;
use sim_support::kernel::live_threads;
use ledger::State;

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

/// A server (a pane runtime activated with the runtime directory whose session records are
/// `records`), as `profile` sets one up.
fn server(profile: &Profile, records: &std::path::Path) -> Panes {
    let panes = if profile.managed {
        Panes::recovering_managed(SHELL, records)
    } else {
        Panes::recovering(SHELL, records)
    };
    panes.set_sweep_on_exit(profile.sweep);
    panes
}

fn random_size() -> (u16, u16) {
    let rows = u16::try_from(5 + draw(60)).unwrap_or(24);
    let cols = u16::try_from(20 + draw(200)).unwrap_or(80);
    (rows, cols)
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
    /// One exclusive action at a time.
    exclusive: tokio::sync::Mutex<()>,
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
fn violated(violations: &mut Vec<String>, invariant: Invariant, what: impl std::fmt::Display) {
    let message = sim_support::invariants::violated(invariant, what);
    if crate::host_set("PANE_RUNTIME_SIM_EVENTS") {
        eprintln!("[{:?}] VIOLATION: {message}", switchy_time::Instant::now());
    }
    violations.push(message);
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
                panes: Arc::new(server(&profile, &records)),
                open: BTreeMap::new(),
                opening: 0,
                steps_left: profile.steps,
                busy: 0,
                paused: false,
                exits: BTreeMap::new(),
                violations: Vec::new(),
                stats: Stats::default(),
                witnesses: Vec::new(),
                order: Vec::new(),
                scripts: None,
                targets: BTreeMap::new(),
                executions: BTreeMap::new(),
                mutations: 0,
            }),
            profile,
            records,
            generation: AtomicU64::new(0),
            exclusive: tokio::sync::Mutex::new(()),
            settle,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn violated(&self, invariant: Invariant, what: impl std::fmt::Display) {
        violated(&mut self.lock().violations, invariant, what);
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
            checked(Invariant::ExitOnce);
            let count = {
                let count = state.exits.entry(pane).or_default();
                *count += 1;
                *count
            };
            if count > programs {
                let message =
                    format!("pane process {pid} got {count} exits for {programs} programs");
                violated(&mut state.violations, Invariant::ExitOnce, message);
            }
        }
    }

    /// One driver: take actions until the steps run out.
    pub async fn drive(self: Arc<Self>, driver: u64, deadline: switchy_time::Instant) {
        loop {
            if switchy_time::Instant::now() >= deadline {
                self.violated(
                    Invariant::Finishes,
                    format!("the workload did not finish in time (driver {driver}; livelock?)"),
                );
                return;
            }
            let kind = {
                let mut state = self.lock();
                let kind = if state.paused {
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

    /// Count `kind` under way (unless it is exclusive: it waits for those under way), with the
    /// server generation it acts on, which a crash ends.
    fn begin(&self, state: &mut State, kind: Kind) -> u64 {
        if !kind.exclusive() {
            state.busy += 1;
        }
        self.generation.load(Ordering::SeqCst)
    }

    /// Take one action begun (see [`Self::begin`]) on server generation `generation`, as
    /// driver `driver`.
    async fn take(&self, driver: u64, kind: Kind, generation: u64) {
        if kind.exclusive() {
            self.act(driver, kind).await;
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
                self.violated(Invariant::Finishes, format!(
                    "d{driver}: {} did not finish within {limit:?} (threads: {:?})",
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
            Kind::Crash => self.crash(driver, false).await,
            Kind::PowerLoss => self.crash(driver, true).await,
            Kind::Bystander => self.bystander(driver).await,
            Kind::Split => self.split(driver).await,
            Kind::Interrupt => self.interrupt(driver),
            Kind::Observe => self.observe(driver),
            Kind::Attach => self.attach(driver),
            Kind::Execute => self.execute().await,
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

    /// Close every pane, end every process, and check invariant 5's threads. Returns the
    /// violations found, and the run's shape tags.
    pub async fn finish(&self) -> (Vec<String>, Stats) {
        self.settle_executions().await;
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
                self.violated(
                    Invariant::Finishes,
                    format!(
                        "closing pane process {} at the end did not finish within {limit:?} \
                     (threads: {:?})",
                        opened.pid,
                        live_threads()
                    ),
                );
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
        checked(Invariant::Finishes);
        let drained = self
            .settle(|| {
                switchy_thread::simulator::try_scheduler()
                    .is_none_or(|scheduler| !scheduler.others_alive())
            })
            .await;
        if !drained {
            self.violated(
                Invariant::Finishes,
                format!(
                    "threads still running after every pane closed: {:?}",
                    live_threads()
                ),
            );
        }
        let mut state = self.lock();
        (
            std::mem::take(&mut state.violations),
            std::mem::take(&mut state.stats),
        )
    }
}
