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
//! steps one decision and `N` a hundred, and `:` opens a command line: `goto N` runs to a
//! decision, `back N` goes back, and `break` sets breakpoints (`decision N`, `step N`,
//! `block THREAD [ON]`, `fail`). Going back reruns the selected run's seed from the start in a
//! fresh process (see [`seek`]), which determinism makes exact; `b` and `B` go back one and a
//! hundred decisions.
//!
//! A simulation reports its invariants as [`Invariants`] telemetry; the Scheduler tab shows
//! each with how often it was checked and its failures, next to the decision rate.
//!
//! This crate depends on `simvar_observer` and bmux's TUI crates only, never on simvar or
//! switchy, so it never enables simulator features in a workspace that uses it.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::{
    any::Any,
    collections::BTreeMap,
    io,
    process::Command,
    sync::{Arc, Mutex, PoisonError, mpsc},
    time::{Duration, Instant},
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
use bmux_tui_components::sparkline::SparklineComponent;
use bmux_tui_runtime::{Presenter, TerminalPresenter};
use futures_util::FutureExt as _;
pub use simvar_observer::{
    Breakpoint, ReplayControl, ReplayMode, RunId, SchedulerView, SimEvent, SimObserver,
};

/// One invariant of a simulation: how often a run checked it, and how it failed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InvariantStatus {
    /// What it says.
    pub name: String,
    /// Checks so far.
    pub checked: u64,
    /// Each failure, as reported.
    pub failures: Vec<String>,
}

/// Telemetry a simulation emits with every invariant's status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Invariants(pub Vec<InvariantStatus>);

/// `samples` as a sparkline at most `width` columns wide, newest last.
#[must_use]
pub fn sparkline(samples: &[u64], width: u16) -> String {
    let line = SparklineComponent::new("sparkline", samples);
    let visible = line.visible_samples(width);
    let max = visible.iter().copied().max().unwrap_or(0);
    visible.iter().map(|&v| line.glyph_for(v, max)).collect()
}

/// The environment variable [`replay_control`] reads: a decision to pause at.
pub const PAUSE_AT_ENV: &str = "SIMVAR_TUI_PAUSE_AT";

/// A replay control that pauses at the decision in [`PAUSE_AT_ENV`] if it is set (as in a
/// process [`seek`] started), and otherwise runs.
///
/// # Panics
///
/// * If [`PAUSE_AT_ENV`] is set to something other than a decision number
#[must_use]
pub fn replay_control() -> Arc<ReplayControl> {
    let mode = std::env::var(PAUSE_AT_ENV)
        .ok()
        .map_or(ReplayMode::Run, |value| {
            ReplayMode::Until(
                value
                    .parse()
                    .unwrap_or_else(|_| panic!("{PAUSE_AT_ENV} must be a decision, got '{value}'")),
            )
        });
    Arc::new(ReplayControl::new(mode))
}

/// Run this program again, as one run of `seed` that pauses at `decision`.
///
/// It keeps the arguments and environment, setting `SIMULATOR_SEED`, one run at a time, and
/// [`PAUSE_AT_ENV`]. On Unix it replaces this process; elsewhere it waits for the new one and
/// exits with its status. Returns only if it cannot start it.
#[must_use]
pub fn seek(seed: u64, decision: u64) -> io::Error {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return error,
    };
    let mut command = Command::new(executable);
    command
        .args(std::env::args_os().skip(1))
        .env("SIMULATOR_SEED", seed.to_string())
        .env("SIMULATOR_RUNS", "1")
        .env("SIMULATOR_MAX_PARALLEL", "1")
        .env(PAUSE_AT_ENV, decision.to_string())
        .env_remove("SIMULATOR_VERIFY_DETERMINISM")
        .env_remove("SIMULATOR_SHRINK");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.exec()
    }
    #[cfg(not(unix))]
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => error,
    }
}

/// The key as typed: strokes record a capital letter as the lowercase letter with Shift.
const fn typed(code: KeyCode, shift: bool) -> KeyCode {
    match code {
        KeyCode::Char(c) if shift && c.is_ascii_lowercase() => {
            KeyCode::Char(c.to_ascii_uppercase())
        }
        other => other,
    }
}

/// How a [`Dashboard`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The user quit.
    Quit,
    /// The user went back in a run: rerun `seed` up to `decision` (see [`seek`]).
    Seek {
        /// The run's seed.
        seed: u64,
        /// The decision to pause at.
        decision: u64,
    },
}

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
    /// Decisions per second, sampled as scheduler telemetry arrives.
    rates: Vec<u64>,
    last_sample: Option<(Instant, u64)>,
    invariants: Option<Invariants>,
}

/// How often the decision rate is sampled, at most.
const RATE_INTERVAL: Duration = Duration::from_millis(250);
/// Decision-rate samples kept per run.
const RATE_SAMPLES: usize = 240;

impl Run {
    fn sample_rate(&mut self, decisions: u64, now: Instant) {
        match self.last_sample {
            Some((at, before)) if now.duration_since(at) >= RATE_INTERVAL => {
                let seconds = now.duration_since(at).as_secs_f64();
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )] // a rate for display
                let rate = (decisions.saturating_sub(before) as f64 / seconds) as u64;
                if self.rates.len() == RATE_SAMPLES {
                    self.rates.remove(0);
                }
                self.rates.push(rate);
                self.last_sample = Some((now, decisions));
            }
            Some(_) => {}
            None => self.last_sample = Some((now, decisions)),
        }
    }
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
    /// Runs in the simulation, once known.
    total: Option<u64>,
    /// The command being typed after `:`.
    command: Option<String>,
    /// The last command's outcome.
    message: Option<String>,
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
            total: None,
            command: None,
            message: None,
        }
    }

    /// Take every event collected so far. Returns whether anything changed.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            changed = true;
            match event {
                SimEvent::Started { runs, .. } => self.total = Some(runs),
                SimEvent::RunStarted {
                    id, seed, props, ..
                } => {
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
                    ..
                } => {
                    let run = self.runs.entry(id).or_default();
                    run.seed = seed;
                    run.result = Some((success, report));
                }
                SimEvent::Telemetry { id, payload } => {
                    if let Some(view) = payload.downcast_ref::<SchedulerView>() {
                        self.runs
                            .entry(id)
                            .or_default()
                            .sample_rate(view.decisions, Instant::now());
                        self.schedulers.insert(id, view.clone());
                    }
                    if let Some(invariants) = payload.downcast_ref::<Invariants>() {
                        self.runs.entry(id).or_default().invariants = Some(invariants.clone());
                    }
                    for panel in &mut self.panels {
                        panel.telemetry(id, &payload);
                    }
                }
                SimEvent::Finished => self.finished = true,
                _ => {}
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
        let runs = self.total.map_or_else(
            || format!("{} runs  ", self.runs.len()),
            |total| format!("{}/{total} runs  ", self.runs.len()),
        );
        let mut status = vec![
            Span::styled(format!("{}  ", self.title), bold()),
            Span::raw(runs),
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
        let mut replay_line = Vec::new();
        if let Some(replay) = &self.replay {
            let (mode, decision) = replay.status();
            status.push(Span::raw(format!(
                "   replay: {mode:?} at decision {decision}"
            )));
            if let Some(hit) = replay.hit() {
                replay_line.push(colored(format!("paused: {hit}  "), Color::Yellow));
            }
            let breakpoints = replay.breakpoints();
            if !breakpoints.is_empty() {
                replay_line.push(Span::raw(format!(
                    "breakpoints: {}",
                    breakpoints
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
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
        let prompt = self.command.as_ref().map_or_else(
            || {
                self.message.as_ref().map_or_else(
                    || {
                        Line::raw(
                            "keys: 1-9 tab  up/down run  space pause/resume  n/N step 1/100  \
                             b/B back 1/100  : command  q quit",
                        )
                    },
                    |message| Line::raw(message.clone()),
                )
            },
            |command| {
                Line::from_spans(vec![
                    Span::styled(format!(":{command}"), bold()),
                    Span::raw(
                        "   goto N | back N | break decision N | break step N | \
                         break block THREAD [ON] | break fail | clear",
                    ),
                ])
            },
        );
        vec![
            Line::from_spans(status),
            Line::from_spans(replay_line),
            Line::from_spans(tabs),
            prompt,
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

    fn invariant_lines(run: &Run) -> Vec<Line> {
        let Some(Invariants(invariants)) = &run.invariants else {
            return Vec::new();
        };
        let mut lines = vec![
            Line::raw(""),
            Line::from_spans(vec![Span::styled("Invariants", bold())]),
        ];
        for invariant in invariants {
            let (status, color) = if invariant.failures.is_empty() {
                ("ok", Color::Green)
            } else {
                ("FAILED", Color::Red)
            };
            lines.push(Line::from_spans(vec![
                Span::raw("  "),
                colored(format!("{status:<7}"), color),
                Span::raw(format!(
                    "{:<64} checked {}",
                    invariant.name, invariant.checked
                )),
            ]));
            for failure in invariant.failures.iter().take(3) {
                lines.push(Line::from_spans(vec![colored(
                    format!("           {failure}"),
                    Color::Red,
                )]));
            }
        }
        lines
    }

    fn scheduler_tab(&self, width: u16) -> Vec<Line> {
        let run = self.selected_run().and_then(|id| self.runs.get(&id));
        let Some(view) = self.selected_run().and_then(|id| self.schedulers.get(&id)) else {
            let mut lines = vec![Line::raw(
                "No scheduler telemetry for this run (the simulation emits SchedulerView).",
            )];
            lines.extend(run.map(Self::invariant_lines).unwrap_or_default());
            return lines;
        };
        let mut lines = vec![Line::raw(format!(
            "decisions {}   trace {:#018x}   running {}",
            view.decisions, view.trace_hash, view.running
        ))];
        if let Some(run) = run
            && !run.rates.is_empty()
        {
            lines.push(Line::from_spans(vec![
                Span::raw(format!(
                    "decisions/s {:>8}  ",
                    run.rates.last().copied().unwrap_or(0)
                )),
                colored(sparkline(&run.rates, width.saturating_sub(24)), Color::Cyan),
            ]));
        }
        lines.extend(run.map(Self::invariant_lines).unwrap_or_default());
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
            1 => self.scheduler_tab(width),
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

    /// The selected run's seed and its latest decision, when known.
    fn position(&self) -> Option<(u64, u64)> {
        let id = self.selected_run()?;
        let seed = self.runs.get(&id)?.seed;
        let replayed = self
            .replay
            .as_ref()
            .filter(|_| self.runs.len() == 1)
            .map(|replay| replay.status().1);
        let decision = replayed.or_else(|| self.schedulers.get(&id).map(|v| v.decisions))?;
        Some((seed, decision))
    }

    /// Go to `target` in the selected run: forward by running to it, backward by seeking.
    fn goto(&mut self, target: u64) -> Option<Exit> {
        let Some((seed, decision)) = self.position() else {
            self.message = Some("no run to go to a decision in".into());
            return None;
        };
        let ended = self
            .selected_run()
            .and_then(|id| self.runs.get(&id))
            .is_some_and(|run| run.result.is_some());
        if target > decision && ended {
            self.message = Some(format!("the run ended at decision {decision}"));
            None
        } else if target > decision {
            if let Some(replay) = &self.replay {
                replay.set(ReplayMode::Until(target));
            }
            self.message = Some(format!("running to decision {target}"));
            None
        } else {
            Some(Exit::Seek {
                seed,
                decision: target,
            })
        }
    }

    fn back(&mut self, by: u64) -> Option<Exit> {
        let decision = self.position()?.1;
        self.goto(decision.saturating_sub(by))
    }

    /// Run a `:` command.
    fn execute(&mut self, command: &str) -> Option<Exit> {
        let words: Vec<&str> = command.split_whitespace().collect();
        let number = |word: Option<&&str>| word.and_then(|w| w.parse::<u64>().ok());
        let breakpoint = match words.as_slice() {
            ["goto" | "g", n] => {
                let Some(target) = number(Some(n)) else {
                    self.message = Some(format!("not a decision: {n}"));
                    return None;
                };
                return self.goto(target);
            }
            ["back" | "b", n] => {
                return number(Some(n)).and_then(|n| self.back(n));
            }
            ["clear"] => {
                if let Some(replay) = &self.replay {
                    replay.clear_breakpoints();
                }
                self.message = Some("breakpoints cleared".into());
                return None;
            }
            ["break", "decision", n] => number(Some(n)).map(Breakpoint::Decision),
            ["break", "step", n] => number(Some(n)).map(Breakpoint::Step),
            ["break", "block", thread] => Some(Breakpoint::Blocked {
                thread: (*thread).to_owned(),
                on: String::new(),
            }),
            ["break", "block", thread, on] => Some(Breakpoint::Blocked {
                thread: (*thread).to_owned(),
                on: (*on).to_owned(),
            }),
            ["break", "fail"] => Some(Breakpoint::Failure),
            _ => None,
        };
        match (breakpoint, &self.replay) {
            (Some(breakpoint), Some(replay)) => {
                self.message = Some(format!("break at {breakpoint}"));
                replay.add_breakpoint(breakpoint);
            }
            (Some(_), None) => self.message = Some("this simulation cannot pause".into()),
            (None, _) => self.message = Some(format!("unknown command: {command}")),
        }
        None
    }

    /// Handle a key typed on the command line.
    fn command_key(&mut self, code: KeyCode) -> Option<Exit> {
        let command = self.command.as_mut()?;
        match code {
            KeyCode::Escape => self.command = None,
            KeyCode::Enter => {
                let command = self.command.take().unwrap_or_default();
                return self.execute(&command);
            }
            KeyCode::Backspace => {
                command.pop();
            }
            KeyCode::Char(c) => command.push(c),
            _ => {}
        }
        None
    }

    /// Handle a key: `Some` to end the dashboard.
    fn key(&mut self, code: KeyCode, ctrl: bool) -> Option<Exit> {
        if self.command.is_some() {
            return self.command_key(code);
        }
        self.message = None;
        match code {
            KeyCode::Char('q') | KeyCode::Escape => return Some(Exit::Quit),
            KeyCode::Char('c') if ctrl => return Some(Exit::Quit),
            KeyCode::Char(':') => self.command = Some(String::new()),
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
            KeyCode::Char('b') => return self.back(1),
            KeyCode::Char('B') => return self.back(100),
            _ => {}
        }
        None
    }

    /// Take over the terminal and show the dashboard until the user quits or seeks; the
    /// terminal is restored either way.
    ///
    /// # Errors
    ///
    /// * If the terminal cannot be used or restored
    pub fn run(mut self) -> io::Result<Exit> {
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
                        Event::Key(key) => {
                            let code = typed(key.key, key.modifiers.shift);
                            if let Some(exit) = self.key(code, key.modifiers.ctrl) {
                                // Let a paused run finish rather than hold it forever.
                                if let Some(replay) = &self.replay {
                                    replay.clear_breakpoints();
                                    replay.set(ReplayMode::Run);
                                }
                                return Ok(exit);
                            }
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
        restored.and(result)
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
            steps: None,
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
            steps: 3,
        });
        assert!(dashboard.drain());
        let runs = text(&dashboard.runs_tab());
        assert!(runs.contains("seed 42"), "{runs}");
        assert!(runs.contains("FAILED"), "{runs}");
        assert!(runs.contains("SIMULATOR_SEED=42"), "{runs}");
        let scheduler = text(&dashboard.scheduler_tab(80));
        assert!(scheduler.contains("decisions 7"), "{scheduler}");
        assert!(scheduler.contains("running driver-0"), "{scheduler}");
    }

    #[test]
    fn keys_switch_tabs_and_drive_replay() {
        let (_observer, events) = observer();
        let replay = Arc::new(ReplayControl::default());
        let mut dashboard = Dashboard::new("sim", events, Vec::new(), Some(Arc::clone(&replay)));
        assert_eq!(dashboard.key(KeyCode::Char('2'), false), None);
        assert_eq!(dashboard.tab, 1);
        assert_eq!(dashboard.key(KeyCode::Char(' '), false), None);
        assert_eq!(replay.status().0, ReplayMode::Pause);
        assert_eq!(dashboard.key(KeyCode::Char('n'), false), None);
        assert_eq!(replay.status().0, ReplayMode::Step(1));
        assert_eq!(dashboard.key(KeyCode::Char('q'), false), Some(Exit::Quit));
    }

    fn command(dashboard: &mut Dashboard, command: &str) -> Option<Exit> {
        assert_eq!(dashboard.key(KeyCode::Char(':'), false), None);
        for c in command.chars() {
            assert_eq!(dashboard.key(KeyCode::Char(c), false), None);
        }
        dashboard.key(KeyCode::Enter, false)
    }

    /// A dashboard showing one run of seed 42 at decision 500.
    fn at_decision_500() -> (Dashboard, Arc<ReplayControl>) {
        let (observer, events) = observer();
        let replay = Arc::new(ReplayControl::default());
        let mut dashboard = Dashboard::new("sim", events, Vec::new(), Some(Arc::clone(&replay)));
        let id = RunId { worker: 1, run: 1 };
        observer.event(SimEvent::RunStarted {
            id,
            seed: 42,
            props: Vec::new(),
            steps: None,
        });
        replay.checkpoint(500);
        dashboard.drain();
        (dashboard, replay)
    }

    #[test]
    fn shifted_letters_are_capitals() {
        assert_eq!(typed(KeyCode::Char('n'), true), KeyCode::Char('N'));
        assert_eq!(typed(KeyCode::Char('n'), false), KeyCode::Char('n'));
        assert_eq!(typed(KeyCode::Char(':'), true), KeyCode::Char(':'));
    }

    #[test]
    fn commands_run_forward_set_breakpoints_and_seek_back() {
        let (mut dashboard, replay) = at_decision_500();
        assert_eq!(command(&mut dashboard, "goto 900"), None);
        assert_eq!(replay.status().0, ReplayMode::Until(900));
        assert_eq!(
            command(&mut dashboard, "goto 120"),
            Some(Exit::Seek {
                seed: 42,
                decision: 120
            })
        );
        assert_eq!(command(&mut dashboard, "break fail"), None);
        assert_eq!(command(&mut dashboard, "break block driver-1 Io"), None);
        assert_eq!(
            replay.breakpoints(),
            [
                Breakpoint::Failure,
                Breakpoint::Blocked {
                    thread: "driver-1".into(),
                    on: "Io".into()
                }
            ]
        );
        assert_eq!(command(&mut dashboard, "clear"), None);
        assert_eq!(replay.breakpoints(), []);
        assert_eq!(command(&mut dashboard, "frobnicate"), None);
        assert_eq!(
            dashboard.message.as_deref(),
            Some("unknown command: frobnicate")
        );
        assert_eq!(
            dashboard.key(KeyCode::Char('B'), false),
            Some(Exit::Seek {
                seed: 42,
                decision: 400
            })
        );
    }

    #[test]
    fn invariants_and_the_decision_rate_show_with_the_scheduler() {
        let mut run = Run {
            invariants: Some(Invariants(vec![
                InvariantStatus {
                    name: "one exit".into(),
                    checked: 3,
                    failures: Vec::new(),
                },
                InvariantStatus {
                    name: "output delivered".into(),
                    checked: 2,
                    failures: vec!["short by 3 bytes".into()],
                },
            ])),
            ..Run::default()
        };
        let start = Instant::now();
        run.sample_rate(0, start);
        run.sample_rate(100, start + Duration::from_millis(100));
        run.sample_rate(500, start + Duration::from_millis(500));
        assert_eq!(run.rates, [1000]);
        let lines = text(&Dashboard::invariant_lines(&run));
        assert!(
            lines.contains("ok") && lines.contains("one exit"),
            "{lines}"
        );
        assert!(
            lines.contains("FAILED") && lines.contains("short by 3 bytes"),
            "{lines}"
        );
        assert_eq!(sparkline(&[0, 4, 8], 10), "▁▄█");
        assert_eq!(sparkline(&[1, 2, 3, 4], 2), "▆█");
    }
}
