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

use bmux_simvar_tui::{InvariantStatus, Invariants, RunId, SimPanel};
use bmux_tui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// The invariants, by number (see the crate documentation).
const INVARIANTS: [&str; 5] = [
    "1. an opened pane leads its own session",
    "2. typed lines reach the program in order",
    "3. the terminal ends up the requested size",
    "4. closing drains, cleans up, leaves nothing running",
    "5. the workload finishes and its threads end",
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

/// The dashboard tab listing the selected run's panes.
#[derive(Debug, Default)]
pub struct PanesPanel {
    latest: BTreeMap<RunId, PanesTelemetry>,
}

impl SimPanel for PanesPanel {
    fn title(&self) -> String {
        "Panes".into()
    }

    fn telemetry(&mut self, run: RunId, payload: &Arc<dyn Any + Send + Sync>) {
        if let Some(telemetry) = payload.downcast_ref::<PanesTelemetry>() {
            self.latest.insert(run, telemetry.clone());
        }
    }

    fn lines(&self, run: Option<RunId>, _width: u16) -> Vec<Line> {
        let Some(telemetry) = run.and_then(|run| self.latest.get(&run)) else {
            return vec![Line::raw("No panes for this run yet.")];
        };
        let open = telemetry.panes.iter().filter(|view| view.open).count();
        let mut lines = vec![Line::from_spans(vec![Span::styled(
            format!("Panes ({open} open, {} opened)", telemetry.panes.len()),
            Style::new().add_modifier(Modifier::BOLD),
        )])];
        for view in &telemetry.panes {
            let (state, color) = if view.open {
                ("open", Color::Green)
            } else {
                ("closed", Color::Default)
            };
            lines.push(Line::from_spans(vec![
                Span::raw(format!(
                    "  {:<40} pid {:>5}  {:>3}x{:<4} typed {:>3}  ",
                    view.pane, view.pid, view.size.1, view.size.0, view.typed
                )),
                Span::styled(state, Style::new().fg(color)),
            ]));
        }
        lines
    }
}
