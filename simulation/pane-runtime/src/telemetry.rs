//! What the pane runtime simulation shows in `bmux_simvar_tui`: the shared simulation telemetry
//! (`sim_support`: scheduler, kernel, terminals), plus each pane and the invariants' status.
//!
//! The workload keeps a [`Board`] in its run's state (behind a std lock, never a switch point,
//! so keeping it never changes the run), which telemetry reads.

use std::{
    any::Any,
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};

use bmux_simvar_tui::{Entity, InvariantStatus, Invariants, RunId, SimView, Tone};
use bmux_tui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// The invariants, by number (see the crate documentation).
const INVARIANTS: [&str; 9] = [
    "1. an opened pane leads its own session",
    "2. typed lines reach the program in order",
    "3. the terminal ends up the requested size",
    "4. closing drains, cleans up, leaves nothing running",
    "5. the workload finishes and its threads end",
    "6. a crash's sessions end, and only they",
    "7. restarting ends the old session",
    "8. each program's exit is reported at most once",
    "9. nothing is left once every pane closed",
];

/// One pane as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneView {
    /// Its name (its session).
    pub pane: String,
    /// Its program's process.
    pub pid: u32,
    /// Its size, last requested.
    pub size: (u16, u16),
    /// Lines typed into it.
    pub typed: u64,
    /// Whether it is still open.
    pub open: bool,
}

/// Telemetry with every pane of a run.
#[derive(Debug, Clone, Default)]
pub struct PanesTelemetry {
    /// The panes, in the order they opened.
    pub panes: Vec<PaneView>,
}

#[derive(Debug, Default)]
struct BoardState {
    panes: Vec<PaneView>,
    checks: BTreeMap<usize, (u64, Vec<String>)>,
}

/// The run's panes and checks.
#[derive(Debug, Default)]
pub struct Board(Mutex<BoardState>);

impl Board {
    fn with<R>(f: impl FnOnce(&mut BoardState) -> R) -> R {
        let board = switchy_simulation::state::<Self>();
        let mut state = board.0.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut state)
    }

    /// Count a check of invariant `number`.
    pub fn checked(number: usize) {
        Self::with(|state| state.checks.entry(number).or_default().0 += 1);
    }

    /// Record a failure, `message` starting with its invariant's number.
    pub fn failed(message: &str) {
        let number = message
            .split('.')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(5);
        Self::with(|state| {
            state
                .checks
                .entry(number)
                .or_default()
                .1
                .push(message.to_owned());
        });
    }

    /// A pane opened.
    pub fn pane(view: PaneView) {
        Self::with(|state| state.panes.push(view));
    }

    fn update(pane: &str, f: impl FnOnce(&mut PaneView)) {
        Self::with(|state| {
            if let Some(view) = state.panes.iter_mut().find(|view| view.pane == pane) {
                f(view);
            }
        });
    }

    /// A line was typed into `pane`.
    pub fn typed(pane: &str) {
        Self::update(pane, |view| view.typed += 1);
    }

    /// `pane` was resized.
    pub fn resized(pane: &str, size: (u16, u16)) {
        Self::update(pane, |view| view.size = size);
    }

    /// `pane` closed.
    pub fn closed(pane: &str) {
        Self::update(pane, |view| view.open = false);
    }
}

/// This simulation's own telemetry: its panes and its invariants' status.
#[must_use]
pub fn extra() -> Vec<Arc<dyn Any + Send + Sync>> {
    Board::with(|state| {
        let invariants = Invariants(
            INVARIANTS
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    let (checked, failures) =
                        state.checks.get(&(index + 1)).cloned().unwrap_or_default();
                    InvariantStatus {
                        name: (*name).to_owned(),
                        checked,
                        failures,
                    }
                })
                .collect(),
        );
        let panes = PanesTelemetry {
            panes: state.panes.clone(),
        };
        vec![
            Arc::new(invariants) as Arc<dyn Any + Send + Sync>,
            Arc::new(panes),
        ]
    })
}

/// The dashboard's Panes view: each pane of the run, with its process, size and typing.
#[derive(Debug, Default)]
pub struct PanesView {
    latest: BTreeMap<RunId, PanesTelemetry>,
}

impl SimView for PanesView {
    fn title(&self) -> String {
        "Panes".into()
    }

    fn telemetry(&mut self, run: RunId, payload: &Arc<dyn Any + Send + Sync>) {
        if let Some(telemetry) = payload.downcast_ref::<PanesTelemetry>() {
            self.latest.insert(run, telemetry.clone());
        }
    }

    fn entities(&self, run: RunId) -> Vec<Entity> {
        self.latest
            .get(&run)
            .map(|telemetry| {
                telemetry
                    .panes
                    .iter()
                    .enumerate()
                    .map(|(index, view)| Entity {
                        id: format!("pane.{index}"),
                        kind: "pane".into(),
                        label: format!("pid {} {}", view.pid, short(&view.pane)),
                        status: if view.open {
                            format!("{}x{} · {} typed", view.size.1, view.size.0, view.typed)
                        } else {
                            "closed".into()
                        },
                        tone: if view.open { Tone::Good } else { Tone::Neutral },
                        depth: 0,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn card(&self, run: RunId, entity: &str, _width: u16) -> Vec<Line> {
        let view = entity
            .strip_prefix("pane.")
            .and_then(|index| index.parse::<usize>().ok())
            .and_then(|index| self.latest.get(&run)?.panes.get(index));
        let Some(view) = view else {
            return vec![Line::raw(" No such pane in this run.")];
        };
        let field = |name: &str, value: String| {
            Line::from_spans(vec![
                Span::styled(format!(" {name:<9}"), Style::new().fg(Color::BrightBlack)),
                Span::raw(value),
            ])
        };
        let (state, color) = if view.open {
            ("open", Color::Green)
        } else {
            ("closed", Color::BrightBlack)
        };
        vec![
            Line::from_spans(vec![
                Span::styled(" pane ", Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(state, Style::new().fg(color)),
            ]),
            field("session", view.pane.clone()),
            field("program", format!("pid {} (see the Kernel view)", view.pid)),
            field(
                "size",
                format!("{} rows × {} columns", view.size.1, view.size.0),
            ),
            field("typed", format!("{} lines", view.typed)),
        ]
    }

    fn forget(&mut self, run: RunId) {
        self.latest.remove(&run);
    }
}

/// A pane's name, short.
fn short(pane: &str) -> String {
    pane.chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
