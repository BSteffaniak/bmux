//! A terminal interface for watching and replaying simvar simulations, on bmux's TUI stack.
//!
//! [`observer`] gives a [`SimObserver`] to register with simvar and the events it collects;
//! [`Dashboard::run`] shows them until quit:
//!
//! * **Runs**: every run with its seed, progress and result; failures with how to replay them.
//! * **Scheduler**: the selected run's simulated threads and its latest scheduling decisions,
//!   from [`SchedulerView`] telemetry.
//! * **Panels** a simulation adds ([`SimPanel`]), for its own telemetry.
//!
//! With a [`ReplayControl`], space pauses and resumes the run at scheduling decisions, `n`
//! steps one decision and `N` a hundred.
//!
//! This crate depends on `simvar_observer` and bmux's TUI crates only, never on simvar or
//! switchy, so it never enables simulator features in a workspace that uses it.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::{
    any::Any,
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex, PoisonError, mpsc},
    time::Duration,
};

use bmux_keyboard::KeyCode;
use bmux_tui::{
    component::{Component, Constraints, LayoutCx},
    composition::TextBlock,
    event::Event,
    geometry::{Rect, Size},
    paint::PaintCx,
    style::{Color, Modifier, Style},
    terminal::Terminal,
    terminal_io::{TerminalGuard, poll_event, terminal_size},
    text::{Line, Span, Text},
};
use bmux_tui_runtime::{Presenter, TerminalPresenter};
use futures_util::FutureExt as _;
pub use simvar_observer::{ReplayControl, ReplayMode, RunId, SchedulerView, SimEvent, SimObserver};

/// A simulation's own view of its telemetry, shown as a tab of the dashboard.
pub trait SimPanel: Send {
    /// The tab's title.
    fn title(&self) -> String;

    /// Take telemetry a run emitted; ignore what this panel does not show.
    fn telemetry(&mut self, run: RunId, payload: &Arc<dyn Any + Send + Sync>);

    /// The panel's lines for `run` (the selected run), at most `width` columns wide.
    fn lines(&self, run: Option<RunId>, width: u16) -> Vec<Line>;
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

#[derive(Debug, Default)]
struct Run {
    seed: u64,
    props: Vec<(String, String)>,
    step: u64,
    progress: Option<f64>,
    result: Option<(bool, String)>,
}

/// The dashboard's state; see the [crate documentation](crate).
pub struct Dashboard {
    events: mpsc::Receiver<SimEvent>,
    runs: BTreeMap<RunId, Run>,
    selected: usize,
    tab: usize,
    schedulers: BTreeMap<RunId, SchedulerView>,
    panels: Vec<Box<dyn SimPanel>>,
    replay: Option<Arc<ReplayControl>>,
    finished: bool,
    title: String,
}

const fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn colored(text: impl Into<String>, color: Color) -> Span {
    Span::styled(text, Style::new().fg(color))
}

impl Dashboard {
    /// A dashboard titled `title` for `events`, with `panels` after the built-in tabs and an
    /// optional replay control.
    #[must_use]
    pub fn new(
        title: impl Into<String>,
        events: mpsc::Receiver<SimEvent>,
        panels: Vec<Box<dyn SimPanel>>,
        replay: Option<Arc<ReplayControl>>,
    ) -> Self {
        Self {
            events,
            runs: BTreeMap::new(),
            selected: 0,
            tab: 0,
            schedulers: BTreeMap::new(),
            panels,
            replay,
            finished: false,
            title: title.into(),
        }
    }

    /// Take every event collected so far. Returns whether anything changed.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            changed = true;
            match event {
                SimEvent::RunStarted { id, seed, props } => {
                    self.runs.insert(
                        id,
                        Run {
                            seed,
                            props,
                            ..Run::default()
                        },
                    );
                }
                SimEvent::Progress { id, step, progress } => {
                    let run = self.runs.entry(id).or_default();
                    run.step = step;
                    run.progress = progress;
                }
                SimEvent::RunFinished {
                    id,
                    seed,
                    success,
                    report,
                } => {
                    let run = self.runs.entry(id).or_default();
                    run.seed = seed;
                    run.result = Some((success, report));
                }
                SimEvent::Telemetry { id, payload } => {
                    if let Some(view) = payload.downcast_ref::<SchedulerView>() {
                        self.schedulers.insert(id, view.clone());
                    }
                    for panel in &mut self.panels {
                        panel.telemetry(id, &payload);
                    }
                }
                SimEvent::Finished => self.finished = true,
            }
        }
        changed
    }

    fn selected_run(&self) -> Option<RunId> {
        self.runs.keys().nth(self.selected).copied()
    }

    fn tabs(&self) -> Vec<String> {
        let mut tabs = vec!["Runs".to_owned(), "Scheduler".to_owned()];
        tabs.extend(self.panels.iter().map(|p| p.title()));
        tabs
    }

    fn header(&self) -> Vec<Line> {
        let passed = self
            .runs
            .values()
            .filter(|r| matches!(r.result, Some((true, _))))
            .count();
        let failed = self
            .runs
            .values()
            .filter(|r| matches!(r.result, Some((false, _))))
            .count();
        let mut status = vec![
            Span::styled(format!("{}  ", self.title), bold()),
            Span::raw(format!("{} runs  ", self.runs.len())),
            colored(format!("{passed} passed  "), Color::Green),
            colored(
                format!("{failed} failed  "),
                if failed > 0 {
                    Color::Red
                } else {
                    Color::Default
                },
            ),
            Span::raw(if self.finished { "done" } else { "running" }),
        ];
        if let Some(replay) = &self.replay {
            let (mode, decision) = replay.status();
            status.push(Span::raw(format!(
                "   replay: {mode:?} at decision {decision}"
            )));
        }
        let tabs = self
            .tabs()
            .into_iter()
            .enumerate()
            .map(|(i, title)| {
                let label = format!(" {} {title} ", i + 1);
                if i == self.tab {
                    Span::styled(label, bold().fg(Color::Cyan))
                } else {
                    Span::raw(label)
                }
            })
            .collect::<Vec<_>>();
        vec![
            Line::from_spans(status),
            Line::from_spans(tabs),
            Line::raw("keys: 1-9 tab  up/down run  space pause/resume  n step  N step 100  q quit"),
            Line::raw(""),
        ]
    }

    fn runs_tab(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        for (index, (id, run)) in self.runs.iter().enumerate() {
            let marker = if index == self.selected { "> " } else { "  " };
            let (status, color) = match &run.result {
                Some((true, _)) => ("passed".to_owned(), Color::Green),
                Some((false, _)) => ("FAILED".to_owned(), Color::Red),
                None => (
                    run.progress.map_or_else(
                        || format!("step {}", run.step),
                        |p| format!("{:.0}%", p * 100.0),
                    ),
                    Color::Yellow,
                ),
            };
            lines.push(Line::from_spans(vec![
                Span::raw(format!(
                    "{marker}worker {} run {:>4}  seed {:<20}  ",
                    id.worker, id.run, run.seed
                )),
                colored(status, color),
            ]));
        }
        if let Some(run) = self.selected_run().and_then(|id| self.runs.get(&id)) {
            lines.push(Line::raw(""));
            for (name, value) in &run.props {
                lines.push(Line::raw(format!("  {name} = {value}")));
            }
            if let Some((false, report)) = &run.result {
                lines.push(Line::raw(""));
                lines.push(Line::from_spans(vec![Span::styled(
                    "Failure (replay with the command at its end):",
                    bold().fg(Color::Red),
                )]));
                lines.extend(report.lines().map(|l| Line::raw(format!("  {l}"))));
            }
        }
        lines
    }

    fn scheduler_tab(&self) -> Vec<Line> {
        let Some(view) = self.selected_run().and_then(|id| self.schedulers.get(&id)) else {
            return vec![Line::raw(
                "No scheduler telemetry for this run (the simulation emits SchedulerView).",
            )];
        };
        let mut lines = vec![Line::raw(format!(
            "decisions {}   trace {:#018x}   running {}",
            view.decisions, view.trace_hash, view.running
        ))];
        lines.push(Line::raw(""));
        lines.push(Line::from_spans(vec![Span::styled("Threads", bold())]));
        for thread in &view.threads {
            let color = if thread.state.starts_with("Blocked") {
                Color::Yellow
            } else if thread.state == "Finished" {
                Color::Default
            } else {
                Color::Green
            };
            lines.push(Line::from_spans(vec![
                Span::raw(format!("  {:<28} ", thread.name)),
                colored(format!("{:<48}", thread.state), color),
                Span::raw(format!(" at {}", thread.op)),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from_spans(vec![Span::styled(
            "Recent decisions",
            bold(),
        )]));
        for decision in view.recent.iter().rev().take(20) {
            lines.push(Line::raw(format!(
                "  #{:<8} {:<24} {:<14} -> {:<24} step {}",
                decision.number, decision.from, decision.op, decision.to, decision.step
            )));
        }
        lines
    }

    fn body(&self, width: u16) -> Vec<Line> {
        match self.tab {
            0 => self.runs_tab(),
            1 => self.scheduler_tab(),
            n => self
                .panels
                .get(n - 2)
                .map(|p| p.lines(self.selected_run(), width))
                .unwrap_or_default(),
        }
    }

    fn render(&mut self, cx: &mut PaintCx<'_, '_>) {
        let area = cx.area();
        let mut lines = self.header();
        lines.extend(self.body(area.width));
        lines.truncate(usize::from(area.height));
        let block = TextBlock::new(Text::from_lines(lines));
        let layout = block.layout(
            Constraints::tight(Size::new(area.width, area.height)),
            &mut LayoutCx::new(),
        );
        block.paint(&layout, cx);
    }

    /// Handle a key; `false` to quit.
    fn key(&mut self, code: KeyCode, ctrl: bool) -> bool {
        match code {
            KeyCode::Char('q') | KeyCode::Escape => return false,
            KeyCode::Char('c') if ctrl => return false,
            KeyCode::Char(c @ '1'..='9') => {
                let tab = usize::from(u8::try_from(c).unwrap_or(b'1') - b'1');
                if tab < self.tabs().len() {
                    self.tab = tab;
                }
            }
            KeyCode::Tab => self.tab = (self.tab + 1) % self.tabs().len(),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.runs.len().saturating_sub(1));
            }
            KeyCode::Char(' ') => {
                if let Some(replay) = &self.replay {
                    let next = if replay.status().0 == ReplayMode::Run {
                        ReplayMode::Pause
                    } else {
                        ReplayMode::Run
                    };
                    replay.set(next);
                }
            }
            KeyCode::Char('n') => {
                if let Some(replay) = &self.replay {
                    replay.set(ReplayMode::Step(1));
                }
            }
            KeyCode::Char('N') => {
                if let Some(replay) = &self.replay {
                    replay.set(ReplayMode::Step(100));
                }
            }
            _ => {}
        }
        true
    }

    /// Take over the terminal and show the dashboard until the user quits.
    ///
    /// # Errors
    ///
    /// * If the terminal cannot be used or restored
    pub fn run(mut self) -> io::Result<()> {
        let size = terminal_size()?;
        let mut guard = TerminalGuard::enter(io::stdout())?;
        let writer = guard
            .writer_mut()
            .ok_or_else(|| io::Error::other("the terminal guard has no writer"))?;
        let terminal = Terminal::new(writer, Rect::new(0, 0, size.width, size.height));
        let mut presenter = TerminalPresenter::new(terminal, Self::render);
        let result = (|| {
            presenter.present(&mut self)?;
            loop {
                let changed = self.drain();
                let event = poll_event(Duration::from_millis(50))?;
                if let Some(event) = &event {
                    presenter.observe_terminal_event(event);
                    match event {
                        Event::Resize(size) => presenter.resize(*size),
                        Event::Key(key) if !self.key(key.key, key.modifiers.ctrl) => {
                            // Let a paused run finish rather than hold it forever.
                            if let Some(replay) = &self.replay {
                                replay.set(ReplayMode::Run);
                            }
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                if changed || event.is_some() {
                    presenter.present(&mut self)?;
                }
            }
        })();
        let restored = presenter.restore().now_or_never().unwrap_or(Ok(()));
        drop(presenter);
        guard.leave()?;
        result.and(restored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn events_build_runs_failures_and_scheduler_views() {
        let (observer, events) = observer();
        let mut dashboard = Dashboard::new("sim", events, Vec::new(), None);
        let id = RunId { worker: 1, run: 1 };
        observer.event(SimEvent::RunStarted {
            id,
            seed: 42,
            props: vec![("drivers".into(), "2".into())],
        });
        observer.event(SimEvent::Telemetry {
            id,
            payload: Arc::new(SchedulerView {
                running: "driver-0".into(),
                decisions: 7,
                ..SchedulerView::default()
            }),
        });
        observer.event(SimEvent::RunFinished {
            id,
            seed: 42,
            success: false,
            report: "boom\nSIMULATOR_SEED=42 cargo run".into(),
        });
        assert!(dashboard.drain());
        let runs = text(&dashboard.runs_tab());
        assert!(runs.contains("seed 42"), "{runs}");
        assert!(runs.contains("FAILED"), "{runs}");
        assert!(runs.contains("SIMULATOR_SEED=42"), "{runs}");
        let scheduler = text(&dashboard.scheduler_tab());
        assert!(scheduler.contains("decisions 7"), "{scheduler}");
        assert!(scheduler.contains("running driver-0"), "{scheduler}");
    }

    #[test]
    fn keys_switch_tabs_and_drive_replay() {
        let (_observer, events) = observer();
        let replay = Arc::new(ReplayControl::default());
        let mut dashboard = Dashboard::new("sim", events, Vec::new(), Some(Arc::clone(&replay)));
        assert!(dashboard.key(KeyCode::Char('2'), false));
        assert_eq!(dashboard.tab, 1);
        assert!(dashboard.key(KeyCode::Char(' '), false));
        assert_eq!(replay.status().0, ReplayMode::Pause);
        assert!(dashboard.key(KeyCode::Char('n'), false));
        assert_eq!(replay.status().0, ReplayMode::Step(1));
        assert!(!dashboard.key(KeyCode::Char('q'), false));
    }
}
