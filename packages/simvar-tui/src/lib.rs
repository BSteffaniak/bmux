//! The simulation dashboard: a fleet of simulations running continuously, and a debugger that
//! travels through any one run in time. Built on bmux's TUI stack.
//!
//! * **Fleet**: every worker's live run, throughput, outcomes split by what runs exercised,
//!   the invariants exercised, coverage, and failures, which are kept (not fatal), grouped by
//!   what failed, confirmed by a replay and shrunk in the background, and saved to disk.
//! * **Investigate**: open any run, live or finished. Its timeline shows which simulated
//!   thread ran when and the simulation's events; play, pause, step, scrub and jump to events,
//!   going back by replaying from parked keyframes ([`timetravel`]). Drill from the run into
//!   its threads, invariants, decisions, and whatever the simulation shows ([`SimView`]):
//!   rows, cards on hover, pages when opened.
//!
//! Everything is clickable, hoverable and scrollable with the mouse, and every action has a
//! key (`?` lists them; `:` or `ctrl+k` opens the command palette).
//!
//! The dashboard learns everything from simvar's observer events ([`observer`]), steers the
//! simulation through a [`Controller`], and tells the simulation what it watches through
//! [`telemetry::Interest`]. This crate depends on `simvar_observer` and bmux's TUI crates only,
//! never on simvar or switchy, so it never enables simulator features in a workspace that
//! uses it.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

mod app;
pub mod findings;
pub mod report;
pub mod store;
pub mod telemetry;
pub mod theme;
pub mod timetravel;
pub mod ui;
mod views;

use std::any::Any;
use std::io;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use bmux_tui::terminal_io::{TerminalGuard, poll_event, terminal_size};
use bmux_tui::text::Line;
pub use simvar_observer::{
    Breakpoint, ReplayControl, ReplayMode, RunId, RunKind, SchedulerView, SimEvent, SimObserver,
};

pub use app::App;
pub use telemetry::{
    Interest, InvariantStatus, Invariants, Pulse, RunIndex, Shape, TimelineEvent, Tone,
};

/// One thing a simulation shows inside a run: a process, a terminal, a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    /// Its id, stable within the run.
    pub id: String,
    /// What it is (`process`, `terminal`, `pane`).
    pub kind: String,
    /// Its name, for people.
    pub label: String,
    /// A short status (`running`, `exited 0`, `80x24`).
    pub status: String,
    /// How its status reads.
    pub tone: Tone,
    /// Nesting, for trees (0 for top level).
    pub depth: u16,
}

/// What a simulation shows of itself in a run: a tab of the run's page, listing entities
/// (rows), describing the one pointed at (a card), and drawing one in full (a page).
pub trait SimView: Send {
    /// The tab's title.
    fn title(&self) -> String;

    /// Take telemetry a run emitted; ignore what this view does not show.
    fn telemetry(&mut self, run: RunId, payload: &Arc<dyn Any + Send + Sync>);

    /// The entities of `run`, in display order.
    fn entities(&self, run: RunId) -> Vec<Entity>;

    /// Lines describing `entity` of `run`, at most `width` columns wide.
    fn card(&self, run: RunId, entity: &str, width: u16) -> Vec<Line>;

    /// Draw `entity` of `run` in full into `area` (by default, its card).
    fn page(&self, run: RunId, entity: &str, ui: &mut ui::Ui<'_, '_, '_>, area: Rect) {
        let lines = self.card(run, entity, area.width);
        ui.lines(area, &lines);
    }

    /// The dashboard no longer needs `run`.
    fn forget(&mut self, _run: RunId) {}
}

/// How the fleet is doing, as its controller sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FleetState {
    /// Whether it starts no new runs.
    pub paused: bool,
    /// Runs it makes at once.
    pub max_parallel: u64,
    /// Runs in flight.
    pub active: u64,
}

/// Steers the simulation: the fleet's pace, and replays and shrinks of single runs.
pub trait Controller: Send {
    /// The fleet's state.
    fn status(&self) -> FleetState;
    /// Start no new runs.
    fn pause(&self);
    /// Start runs again.
    fn resume(&self);
    /// Make up to `count` runs at once.
    fn set_max_parallel(&self, count: u64);
    /// End the simulation.
    fn stop(&self);
    /// Replay the run of `seed` and `epoch_offset`, held by `control` when given. `None` when
    /// it cannot start.
    fn replay(
        &self,
        seed: u64,
        epoch_offset: u64,
        control: Option<Arc<ReplayControl>>,
    ) -> Option<RunId>;
    /// Shrink the failing run of `seed` and `epoch_offset`. `None` when it cannot start.
    fn shrink(&self, seed: u64, epoch_offset: u64) -> Option<RunId>;
}

/// Collects simvar's events for a [`Dashboard`]; register it with simvar's `add_observer`.
#[derive(Debug)]
pub struct ChannelObserver(Mutex<mpsc::Sender<SimEvent>>);

impl SimObserver for ChannelObserver {
    fn event(&self, event: SimEvent) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(event);
    }
}

/// An observer and the events it collects.
#[must_use]
pub fn observer() -> (Arc<ChannelObserver>, mpsc::Receiver<SimEvent>) {
    let (sender, receiver) = mpsc::channel();
    (Arc::new(ChannelObserver(Mutex::new(sender))), receiver)
}

/// What a dashboard shows and controls.
pub struct Options {
    /// The title (usually the simulation's name).
    pub title: String,
    /// The simulation's own views, after the built-in run tabs.
    pub views: Vec<Box<dyn SimView>>,
    /// Steers the simulation, when it can be steered.
    pub controller: Option<Box<dyn Controller>>,
    /// What the dashboard watches, shared with the simulation.
    pub interest: Arc<Interest>,
    /// Where failures are saved, when they are.
    pub findings: Option<findings::Findings>,
    /// A run to open at start (seed, epoch offset), paused at its start.
    pub open: Option<(u64, u64)>,
}

/// The dashboard: an [`App`] fed by simvar's events and drawn on the terminal.
pub struct Dashboard {
    app: App,
    events: mpsc::Receiver<SimEvent>,
}

/// Frames per second at most.
const FRAME: Duration = Duration::from_millis(33);

impl Dashboard {
    /// A dashboard for `events`, as `options` say.
    #[must_use]
    pub fn new(options: Options, events: mpsc::Receiver<SimEvent>) -> Self {
        Self {
            app: App::new(options, Instant::now()),
            events,
        }
    }

    /// Show the dashboard until the user quits. The terminal is restored however it ends.
    ///
    /// # Errors
    ///
    /// * If the terminal cannot be used
    pub fn run(mut self) -> io::Result<()> {
        let mut guard = TerminalGuard::enter(io::stdout())?;
        let result = (|| -> io::Result<()> {
            let writer = guard
                .writer_mut()
                .ok_or_else(|| io::Error::other("the terminal guard owns no writer"))?;
            let size = terminal_size()?;
            let mut terminal = Terminal::new(writer, Rect::new(0, 0, size.width, size.height));
            let mut drawn = None::<Instant>;
            loop {
                let now = Instant::now();
                while let Ok(event) = self.events.try_recv() {
                    self.app.event(event, now);
                }
                self.app.tick(now);
                if self.app.quitting() {
                    break;
                }
                let due = drawn.is_none_or(|at| now.duration_since(at) >= FRAME);
                if self.app.needs_draw() && due {
                    terminal.draw(|cx| self.app.render(cx))?;
                    self.app.commit(terminal.hits());
                    drawn = Some(now);
                }
                if let Some(event) = poll_event(FRAME)? {
                    terminal.observe_event(&event);
                    if let Event::Resize(size) = event {
                        terminal.resize(Rect::new(0, 0, size.width, size.height));
                    }
                    self.app.input(&event, Instant::now());
                }
            }
            terminal.restore_pointer()
        })();
        self.app.close();
        let restored = guard.leave().map(|_| ());
        result.and(restored)
    }

    /// The application (for tests and embedding).
    #[must_use]
    pub const fn app(&mut self) -> &mut App {
        &mut self.app
    }
}
