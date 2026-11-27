//! What bmux's simulations show in `bmux_simvar_tui`, and how they pause for it.
//!
//! Every run records a timeline index (which simulated thread ran at each scheduling decision,
//! and the simulation's [`event`]s) and sends a heartbeat ([`Pulse`]); those are cheap, so the
//! fleet can run continuously. Runs the dashboard watches, and replays, also send the full
//! picture: the scheduler, the invariants, and the simulated kernel (processes, terminals,
//! signals, and what each terminal's programs wrote next to what `bmux_pty` delivered). All of
//! it only reads state, so watching a run never changes it. A replay's own [`ReplayControl`]
//! holds it at decisions, breakpoints and failures; the run sends its state before it waits.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::{
    any::Any,
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use bmux_simvar_tui::{
    Controller, Entity, FleetState, Interest, Pulse, RunId, RunIndex, Shape, SimView,
    TimelineEvent, Tone,
    findings::{Findings, default_dir},
    report,
    store::Store,
    ui::{Size, Ui, columns, row},
};
use bmux_tui::{
    geometry::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use bmux_tui_components::terminal_view::TerminalState;
use simvar::observer::{DecisionView, ReplayControl, ReplayMode, SchedulerView, ThreadView};
use switchy_pty::simulator::{self, KernelSnapshot, ProcessSnapshot, ProcessState, SignalRecord};
use switchy_thread::simulator::{Decision, SchedulerSnapshot};

/// How often watched runs send their full state, in real time.
const INTERVAL: Duration = Duration::from_millis(100);
/// How often every run sends its heartbeat, in real time.
const PULSE: Duration = Duration::from_millis(250);

pub mod invariants;
pub mod kernel;

static INTEREST: OnceLock<Arc<Interest>> = OnceLock::new();

/// A simulation's own telemetry, taken whenever the rest is (for example its invariants).
pub type Extra = fn() -> Vec<Arc<dyn Any + Send + Sync>>;

static EXTRA: OnceLock<Extra> = OnceLock::new();

/// Turn telemetry on for every run from now on: `interest` says which runs the dashboard
/// watches, and `extra` adds the simulation's own telemetry.
pub fn enable(interest: Arc<Interest>, extra: Extra) {
    let _ = INTEREST.set(interest);
    let _ = EXTRA.set(extra);
}

/// The kernel, its terminals' output and its signals, as of one moment.
#[derive(Debug, Clone)]
pub struct PtyTelemetry {
    /// Processes and terminals.
    pub kernel: KernelSnapshot,
    /// What programs wrote to each terminal so far.
    pub output: Vec<(String, Vec<u8>)>,
    /// What `bmux_pty` delivered from each terminal so far.
    pub delivered: Vec<(String, Vec<u8>)>,
    /// Every signal delivered so far, in order.
    pub signals: Vec<SignalRecord>,
}

/// What `bmux_pty` delivered from each terminal in the current run (only kept while
/// telemetry is on). A std mutex, never a switch point.
#[derive(Default)]
struct Delivered(Mutex<BTreeMap<String, Vec<u8>>>);

/// Note that `bmux_pty` delivered `bytes` from `terminal`, when telemetry is on.
pub fn delivered(terminal: &str, bytes: &[u8]) {
    if INTEREST.get().is_none() {
        return;
    }
    let delivered = switchy_simulation::state::<Delivered>();
    delivered
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(terminal.to_owned())
        .or_default()
        .extend_from_slice(bytes);
}

/// The current run's recording. Std locks, never switch points.
#[derive(Default)]
struct Recording {
    index: Mutex<RunIndex>,
    last_full: Mutex<Option<Instant>>,
    last_pulse: Mutex<Option<Instant>>,
}

fn recording() -> Arc<Recording> {
    switchy_simulation::state::<Recording>()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether the current run sends its full state: the dashboard watches it, or it is a replay.
fn detailed() -> bool {
    simvar::current_replay().is_some()
        || INTEREST
            .get()
            .zip(simvar::current_run())
            .is_some_and(|(interest, run)| interest.watching(run))
}

fn due(last: &Mutex<Option<Instant>>, every: Duration) -> bool {
    let mut last = lock(last);
    let now = Instant::now();
    if last.is_some_and(|at| now.duration_since(at) < every) {
        return false;
    }
    *last = Some(now);
    true
}

fn view(snapshot: &SchedulerSnapshot) -> SchedulerView {
    let name = |index: usize| {
        snapshot
            .threads
            .get(index)
            .map_or_else(|| format!("#{index}"), |t| t.name.clone())
    };
    SchedulerView {
        threads: snapshot
            .threads
            .iter()
            .map(|t| ThreadView {
                name: t.name.clone(),
                state: format!("{:?}", t.state).to_lowercase(),
                op: format!("{:?}", t.op),
            })
            .collect(),
        running: name(snapshot.running),
        decisions: snapshot.decisions,
        trace_hash: snapshot.trace_hash,
        recent: snapshot
            .recent
            .iter()
            .map(|d| DecisionView {
                number: d.number,
                from: name(d.from),
                op: format!("{:?}", d.op),
                to: name(d.to),
                step: d.step,
            })
            .collect(),
    }
}

/// Send the run's timeline index as recorded so far.
fn emit_index(trace_hash: u64) {
    let recording = recording();
    let mut index = lock(&recording.index);
    index.trace_hash = trace_hash;
    let copy = index.clone();
    drop(index);
    simvar::emit(Arc::new(copy));
}

/// Send everything a dashboard shows: the scheduler, the invariants, the kernel, the index.
fn emit(scheduler: SchedulerView) {
    let trace_hash = scheduler.trace_hash;
    simvar::emit(Arc::new(scheduler));
    for payload in EXTRA.get().map(|extra| extra()).unwrap_or_default() {
        simvar::emit(payload);
    }
    let kernel = simulator::snapshot();
    let output = kernel
        .terminals
        .iter()
        .map(|t| {
            (
                t.name.clone(),
                simulator::terminal_written(&t.name).unwrap_or_default(),
            )
        })
        .collect();
    let delivered = lock(&switchy_simulation::state::<Delivered>().0)
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.clone()))
        .collect();
    simvar::emit(Arc::new(PtyTelemetry {
        kernel,
        output,
        delivered,
        signals: simulator::signal_log(),
    }));
    emit_index(trace_hash);
}

/// Record and watch the current run, when telemetry is on.
///
/// Every decision goes into its timeline index; a replay is held at decisions as its control says (sending its state
/// first); watched runs send their state every [`INTERVAL`] of real time, every run its
/// heartbeat every [`PULSE`].
pub fn watch_run() {
    if INTEREST.get().is_none() {
        return;
    }
    let replay = simvar::current_replay();
    let recording = recording();
    switchy_thread::simulator::set_decision_hook(Some(Arc::new(
        move |decision: &Decision, snapshot: &dyn Fn() -> SchedulerSnapshot| {
            lock(&recording.index).record(decision.number, decision.to, || {
                snapshot()
                    .threads
                    .get(decision.to)
                    .map_or_else(|| format!("#{}", decision.to), |t| t.name.clone())
            });
            if let Some(replay) = &replay {
                if replay.arrive(decision.number, decision.step, &|| view(&snapshot())) {
                    emit(view(&snapshot()));
                    replay.wait();
                    return;
                }
                if replay.status().0 != ReplayMode::Run {
                    // Seeking: the state is sent where it stops.
                    return;
                }
            }
            if due(&recording.last_pulse, PULSE) {
                simvar::emit(Arc::new(Pulse {
                    decisions: decision.number + 1,
                }));
            }
            if detailed() && due(&recording.last_full, INTERVAL) {
                emit(view(&snapshot()));
            }
        },
    )));
}

/// Record that something happened in the run, at the current decision.
///
/// `kind` is for filters and breakpoints, `label` for people. A watched replay pauses at it when the dashboard
/// breaks on its kind. Only reads state.
pub fn event(kind: &str, label: impl Into<String>, tone: Tone) {
    let Some(interest) = INTEREST.get() else {
        return;
    };
    let recording = recording();
    let mut index = lock(&recording.index);
    let decision = index.decisions;
    index.event(TimelineEvent {
        decision,
        kind: kind.to_owned(),
        label: label.into(),
        tone,
        entity: None,
    });
    drop(index);
    if interest.breaks_on(kind)
        && let Some(replay) = simvar::current_replay()
    {
        replay.set(ReplayMode::Pause);
    }
}

/// Describe what the run exercised (`personality:windows`, `crash`, …); the fleet counts runs
/// by these and splits its outcomes by them.
pub fn shape(tags: Vec<String>) {
    if INTEREST.get().is_some() {
        simvar::emit(Arc::new(Shape(tags)));
    }
}

/// A failure the run found (`what`): recorded on its timeline, and a replay pauses for it if
/// the dashboard breaks on failures. Only reads state.
pub fn failure(what: &str) {
    if INTEREST.get().is_none() {
        return;
    }
    event("failure", what, Tone::Bad);
    if let Some(replay) = simvar::current_replay()
        && replay.failure(what)
    {
        emit(view(&switchy_thread::simulator::scheduler().snapshot()));
        replay.wait();
    }
}

/// The run is ending: send its finished index (every run), and its full state when watched.
/// Only reads state.
pub fn finish_run() {
    if INTEREST.get().is_none() {
        return;
    }
    let snapshot = switchy_thread::simulator::scheduler().snapshot();
    lock(&recording().index).finished = true;
    if detailed() {
        emit(view(&snapshot));
    } else {
        // Every run's own telemetry (its invariants, for one) feeds the fleet's totals.
        for payload in EXTRA.get().map(|extra| extra()).unwrap_or_default() {
            simvar::emit(payload);
        }
        emit_index(snapshot.trace_hash);
    }
}

/// Rows of history each terminal view keeps.
const SCROLLBACK: usize = 200;
/// Rows of each screen shown.
const SCREEN_ROWS: usize = 12;
/// Samples of each terminal's buffered output kept.
const BUFFERED_SAMPLES: usize = 120;
/// Runs whose views are kept.
const RUNS_KEPT: usize = 32;

/// A terminal emulator fed with a growing byte log.
struct Screen {
    state: TerminalState,
    fed: usize,
}

impl Screen {
    fn new((cols, rows): (u16, u16)) -> Self {
        Self {
            state: TerminalState::new(cols, rows, SCROLLBACK),
            fed: 0,
        }
    }

    /// Feed what `log` gained since last time (starting over if it is not the same log).
    fn update(&mut self, log: &[u8], size: (u16, u16)) {
        if log.len() < self.fed {
            *self = Self::new(size);
        }
        if self.state.size() != size {
            self.state.resize(size.0, size.1);
        }
        self.state.feed(&log[self.fed..]);
        self.fed = log.len();
    }

    /// Every row of the screen down to its last non-blank one, at most `columns` wide.
    fn all_lines(&self, columns: usize) -> Vec<Line> {
        let (cols, rows) = self.state.size();
        let mut lines = self
            .state
            .screen_window(0..usize::from(cols).min(columns), 0..usize::from(rows))
            .map(|window| window.lines)
            .unwrap_or_default();
        while lines
            .last()
            .is_some_and(|line| line.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            lines.pop();
        }
        lines
    }

    /// The screen's last non-blank rows, at most `columns` wide.
    fn lines(&self, columns: usize) -> Vec<Line> {
        let (cols, rows) = self.state.size();
        let mut lines = self
            .state
            .screen_window(0..usize::from(cols).min(columns), 0..usize::from(rows))
            .map(|window| window.lines)
            .unwrap_or_default();
        while lines
            .last()
            .is_some_and(|line| line.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            lines.pop();
        }
        lines.split_off(lines.len().saturating_sub(SCREEN_ROWS))
    }
}

/// One terminal as the panel shows it.
struct TerminalView {
    written: Screen,
    delivered: Screen,
    buffered: Vec<u64>,
}

/// One run as the panel shows it.
struct RunView {
    telemetry: PtyTelemetry,
    terminals: BTreeMap<String, TerminalView>,
}

/// The dashboard tab showing the selected run's processes, signals and terminals.
#[derive(Default)]
pub struct PtyPanel {
    runs: BTreeMap<RunId, RunView>,
}

fn bytes_of<'a>(logs: &'a [(String, Vec<u8>)], name: &str) -> &'a [u8] {
    logs.iter()
        .find(|(n, _)| n == name)
        .map_or(&[][..], |(_, bytes)| bytes.as_slice())
}

impl PtyPanel {
    fn update(&mut self, run: RunId, telemetry: &PtyTelemetry) {
        let view = self.runs.entry(run).or_insert_with(|| RunView {
            telemetry: telemetry.clone(),
            terminals: BTreeMap::new(),
        });
        view.telemetry = telemetry.clone();
        for terminal in &telemetry.kernel.terminals {
            let size = (terminal.size.cols, terminal.size.rows);
            let entry = view
                .terminals
                .entry(terminal.name.clone())
                .or_insert_with(|| TerminalView {
                    written: Screen::new(size),
                    delivered: Screen::new(size),
                    buffered: Vec::new(),
                });
            entry
                .written
                .update(bytes_of(&telemetry.output, &terminal.name), size);
            entry
                .delivered
                .update(bytes_of(&telemetry.delivered, &terminal.name), size);
            if entry.buffered.len() == BUFFERED_SAMPLES {
                entry.buffered.remove(0);
            }
            entry
                .buffered
                .push(u64::try_from(terminal.output_buffered).unwrap_or(u64::MAX));
        }
        while self.runs.len() > RUNS_KEPT {
            self.runs.pop_first();
        }
    }
}

const fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Processes by session and then process group.
fn process_tree(processes: &[ProcessSnapshot]) -> Vec<Line> {
    let mut sessions: BTreeMap<(u64, u32), BTreeMap<u32, Vec<&ProcessSnapshot>>> = BTreeMap::new();
    for process in processes {
        sessions
            .entry((process.session_serial, process.session))
            .or_default()
            .entry(process.group)
            .or_default()
            .push(process);
    }
    let mut lines = Vec::new();
    for ((serial, session), groups) in sessions {
        lines.push(Line::raw(format!(
            "  session {session} (leader serial {serial})"
        )));
        for (group, members) in groups {
            lines.push(Line::raw(format!("    group {group}")));
            for p in members {
                let (state, color) = match &p.state {
                    ProcessState::Running => ("running".to_owned(), Color::Green),
                    ProcessState::Zombie(exit) => (format!("exited ({exit})"), Color::Yellow),
                };
                lines.push(Line::from_spans(vec![
                    Span::raw(format!(
                        "      {:>5} {:<14} {:<14} ",
                        p.pid,
                        p.program,
                        p.terminal.as_deref().unwrap_or("-"),
                    )),
                    Span::styled(state, Style::new().fg(color)),
                    Span::raw(if p.signals.is_empty() {
                        String::new()
                    } else {
                        format!("  signals {:?}", p.signals)
                    }),
                ]));
            }
        }
    }
    lines
}

/// How what was delivered compares with what was written.
fn comparison(written: &[u8], delivered: &[u8]) -> Span {
    if written.starts_with(delivered) {
        let color = if delivered.len() == written.len() {
            Color::Green
        } else {
            Color::Yellow
        };
        Span::styled(
            format!("delivered {} of {} bytes", delivered.len(), written.len()),
            Style::new().fg(color),
        )
    } else {
        let at = delivered
            .iter()
            .zip(written)
            .position(|(a, b)| a != b)
            .unwrap_or(written.len());
        Span::styled(
            format!("delivered output DIFFERS from what was written at byte {at}"),
            bold().fg(Color::Red),
        )
    }
}

impl PtyPanel {
    /// A view of the kernel of each run.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn process_card(process: &ProcessSnapshot) -> Vec<Line> {
        let (state, color) = match &process.state {
            ProcessState::Running => ("running".to_owned(), Color::Green),
            ProcessState::Zombie(exit) => (format!("exited ({exit})"), Color::Yellow),
        };
        let field = |name: &str, value: String| {
            Line::from_spans(vec![
                Span::styled(format!(" {name:<10}"), Style::new().fg(Color::BrightBlack)),
                Span::raw(value),
            ])
        };
        let mut lines = vec![
            Line::from_spans(vec![
                Span::styled(format!(" {} ", process.program), bold()),
                Span::styled(state, Style::new().fg(color)),
            ]),
            field("pid", process.pid.to_string()),
            field("serial", process.serial.to_string()),
            field(
                "session",
                format!("{} (serial {})", process.session, process.session_serial),
            ),
            field("group", process.group.to_string()),
            field(
                "parent",
                process.parent.map_or_else(|| "-".into(), |p| p.to_string()),
            ),
            field(
                "terminal",
                process.terminal.clone().unwrap_or_else(|| "-".into()),
            ),
            field("cwd", process.cwd.display().to_string()),
        ];
        if !process.signals.is_empty() {
            lines.push(field("signals", format!("{:?}", process.signals)));
        }
        lines
    }

    fn terminal_card(view: &RunView, name: &str, width: u16) -> Vec<Line> {
        let telemetry = &view.telemetry;
        let Some(terminal) = telemetry.kernel.terminals.iter().find(|t| t.name == name) else {
            return vec![Line::raw(" gone")];
        };
        let tracked = telemetry.delivered.iter().any(|(n, _)| n == name);
        let compared = if tracked {
            comparison(
                bytes_of(&telemetry.output, name),
                bytes_of(&telemetry.delivered, name),
            )
        } else {
            Span::raw(format!(
                "written {} bytes",
                bytes_of(&telemetry.output, name).len()
            ))
        };
        let mut lines = vec![
            Line::from_spans(vec![Span::styled(format!(" {name}"), bold())]),
            Line::raw(format!(
                " {}x{}  master handles {}  slave holders {}  {}",
                terminal.size.cols,
                terminal.size.rows,
                terminal.master_handles,
                terminal.slave_holders,
                if terminal.hung_up { "hung up" } else { "open" },
            )),
            Line::from_spans(vec![Span::raw(" "), compared]),
        ];
        if let Some(screens) = view.terminals.get(name) {
            lines.push(Line::from_spans(vec![
                Span::raw(format!(" buffered {:>6} B ", terminal.output_buffered)),
                Span::styled(
                    bmux_simvar_tui_sparkline(&screens.buffered, width.saturating_sub(20)),
                    Style::new().fg(Color::Cyan),
                ),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from_spans(vec![Span::styled(
                " screen (programs' output)",
                Style::new().fg(Color::BrightBlack),
            )]));
            lines.extend(screens.written.lines(usize::from(width.saturating_sub(2))));
        }
        lines
    }
}

/// `samples` as a sparkline at most `width` columns wide, newest last.
fn bmux_simvar_tui_sparkline(samples: &[u64], width: u16) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let start = samples.len().saturating_sub(usize::from(width));
    let visible = &samples[start..];
    let max = visible.iter().copied().max().unwrap_or(0).max(1);
    visible
        .iter()
        .map(|v| LEVELS[usize::try_from(v * 7 / max).unwrap_or(7).min(7)])
        .collect()
}

impl SimView for PtyPanel {
    fn title(&self) -> String {
        "Kernel".into()
    }

    fn telemetry(&mut self, run: RunId, payload: &Arc<dyn Any + Send + Sync>) {
        if let Some(telemetry) = payload.downcast_ref::<PtyTelemetry>() {
            self.update(run, telemetry);
        }
    }

    fn entities(&self, run: RunId) -> Vec<Entity> {
        let Some(view) = self.runs.get(&run) else {
            return Vec::new();
        };
        let telemetry = &view.telemetry;
        let mut entities = Vec::new();
        // Processes by session, then process group.
        let mut sessions: BTreeMap<(u64, u32), BTreeMap<u32, Vec<&ProcessSnapshot>>> =
            BTreeMap::new();
        for process in &telemetry.kernel.processes {
            sessions
                .entry((process.session_serial, process.session))
                .or_default()
                .entry(process.group)
                .or_default()
                .push(process);
        }
        for ((serial, session), groups) in sessions {
            let members: usize = groups.values().map(Vec::len).sum();
            let running = groups
                .values()
                .flatten()
                .filter(|p| p.state == ProcessState::Running)
                .count();
            entities.push(Entity {
                id: format!("session.{serial}"),
                kind: "session".into(),
                label: session.to_string(),
                status: format!("{running}/{members} running"),
                tone: if running > 0 {
                    Tone::Good
                } else {
                    Tone::Neutral
                },
                depth: 0,
            });
            for (group, members) in groups {
                for process in members {
                    let (status, tone) = match &process.state {
                        ProcessState::Running => ("running".to_owned(), Tone::Good),
                        ProcessState::Zombie(exit) => (format!("exited {exit}"), Tone::Warn),
                    };
                    entities.push(Entity {
                        id: format!("process.{}", process.serial),
                        kind: if process.pid == group {
                            "group"
                        } else {
                            "process"
                        }
                        .into(),
                        label: format!("{} {}", process.pid, process.program),
                        status,
                        tone,
                        depth: 1,
                    });
                }
            }
        }
        for terminal in &telemetry.kernel.terminals {
            entities.push(Entity {
                id: format!("terminal.{}", terminal.name),
                kind: "terminal".into(),
                label: terminal.name.clone(),
                status: format!(
                    "{}x{} {}",
                    terminal.size.cols,
                    terminal.size.rows,
                    if terminal.hung_up { "hung up" } else { "open" }
                ),
                tone: if terminal.hung_up {
                    Tone::Warn
                } else {
                    Tone::Info
                },
                depth: 0,
            });
        }
        entities.push(Entity {
            id: "signals".into(),
            kind: "signals".into(),
            label: format!("{} delivered", telemetry.signals.len()),
            status: telemetry
                .signals
                .last()
                .map_or_else(String::new, |s| format!("last {:?}", s.signal)),
            tone: Tone::Neutral,
            depth: 0,
        });
        entities
    }

    fn card(&self, run: RunId, entity: &str, width: u16) -> Vec<Line> {
        let Some(view) = self.runs.get(&run) else {
            return vec![Line::raw(" No kernel telemetry for this run yet.")];
        };
        let telemetry = &view.telemetry;
        if let Some(serial) = entity.strip_prefix("process.") {
            return telemetry
                .kernel
                .processes
                .iter()
                .find(|p| p.serial.to_string() == serial)
                .map_or_else(|| vec![Line::raw(" gone")], Self::process_card);
        }
        if let Some(serial) = entity.strip_prefix("session.") {
            let members: Vec<ProcessSnapshot> = telemetry
                .kernel
                .processes
                .iter()
                .filter(|p| p.session_serial.to_string() == serial)
                .cloned()
                .collect();
            let mut lines = vec![Line::from_spans(vec![Span::styled(
                format!(" session (leader serial {serial})"),
                bold(),
            )])];
            lines.extend(process_tree(&members));
            return lines;
        }
        if let Some(name) = entity.strip_prefix("terminal.") {
            return Self::terminal_card(view, name, width);
        }
        let mut lines = vec![Line::from_spans(vec![Span::styled(
            format!(
                " signals ({} delivered, latest last)",
                telemetry.signals.len()
            ),
            bold(),
        )])];
        for record in telemetry.signals.iter().rev().take(40).rev() {
            lines.push(Line::raw(format!(
                " {:?} → {} (serial {}, session {})",
                record.signal, record.pid, record.serial, record.session_serial
            )));
        }
        lines
    }

    fn page(&self, run: RunId, entity: &str, ui: &mut Ui<'_, '_, '_>, area: Rect) {
        let terminal = entity.strip_prefix("terminal.");
        let (Some(name), Some(view)) = (terminal, self.runs.get(&run)) else {
            let lines = self.card(run, entity, area.width);
            ui.lines(area, &lines);
            return;
        };
        // A terminal in full: what programs wrote beside what bmux_pty delivered.
        let parts = columns(area, &[Size::Fill(1), Size::Fixed(1), Size::Fill(1)]);
        let Some(screens) = view.terminals.get(name) else {
            return;
        };
        let tracked = view.telemetry.delivered.iter().any(|(n, _)| n == name);
        ui.line(
            row(parts[0], 0),
            &Line::from_spans(vec![Span::styled(" written by programs", bold())]),
        );
        ui.lines(
            Rect::new(
                parts[0].x,
                parts[0].y + 1,
                parts[0].width,
                parts[0].height.saturating_sub(1),
            ),
            &screens.written.all_lines(usize::from(parts[0].width)),
        );
        for y in 0..area.height {
            ui.cell(
                parts[1].x,
                area.y + y,
                "│",
                Style::new().fg(Color::BrightBlack),
            );
        }
        ui.line(
            row(parts[2], 0),
            &Line::from_spans(vec![Span::styled(
                if tracked {
                    " delivered by bmux_pty"
                } else {
                    " (delivery not tracked)"
                },
                bold(),
            )]),
        );
        if tracked {
            ui.lines(
                Rect::new(
                    parts[2].x,
                    parts[2].y + 1,
                    parts[2].width,
                    parts[2].height.saturating_sub(1),
                ),
                &screens.delivered.all_lines(usize::from(parts[2].width)),
            );
        }
    }

    fn forget(&mut self, run: RunId) {
        self.runs.remove(&run);
    }
}

/// Steers simvar's fleet for the dashboard.
#[derive(Debug, Clone, Copy, Default)]
pub struct FleetController;

impl Controller for FleetController {
    fn status(&self) -> FleetState {
        let status = simvar::control().status();
        FleetState {
            paused: status.paused,
            max_parallel: status.max_parallel,
            active: status.active,
        }
    }

    fn pause(&self) {
        simvar::control().pause();
    }

    fn resume(&self) {
        simvar::control().resume();
    }

    fn set_max_parallel(&self, count: u64) {
        simvar::control().set_max_parallel(count);
    }

    fn stop(&self) {
        simvar::control().stop();
    }

    fn replay(
        &self,
        seed: u64,
        epoch_offset: u64,
        control: Option<Arc<ReplayControl>>,
    ) -> Option<RunId> {
        simvar::control()
            .replay(simvar::ReplayRequest {
                seed,
                epoch_offset,
                control,
            })
            .ok()
    }

    fn verify(&self, seed: u64, epoch_offset: u64) -> Option<RunId> {
        simvar::control().verify(seed, epoch_offset).ok()
    }

    fn shrink(&self, seed: u64, epoch_offset: u64) -> Option<RunId> {
        simvar::control().shrink(seed, epoch_offset).ok()
    }
}

/// Vary scheduling per run: unless the environment fixes them, each run draws its scheduling
/// strategy (random, or PCT with a drawn depth) and whether weak memory is modelled from its
/// seed (`swarm`; see `switchy_thread`'s `Config::from_env`), and how many workers its async
/// runtimes poll tasks with at once (see `switchy_async`'s simulator workers).
fn scheduling_defaults() {
    for name in [
        "SIMULATOR_SCHEDULE_STRATEGY",
        "SIMULATOR_WEAK_MEMORY",
        "SIMULATOR_ASYNC_WORKERS",
    ] {
        if std::env::var_os(name).is_none() {
            // SAFETY: called before the simulation (or anything else) starts any thread.
            unsafe { std::env::set_var(name, "swarm") };
        }
    }
}

/// Run the simulation continuously (unless `SIMULATOR_RUNS` says otherwise), keeping going
/// after failures (unless `SIMULATOR_ON_FAILURE` says otherwise).
fn fleet_defaults() {
    for (name, value) in [
        ("SIMULATOR_RUNS", "forever"),
        ("SIMULATOR_ON_FAILURE", "continue"),
    ] {
        if std::env::var_os(name).is_none() {
            // SAFETY: called before the simulation (or anything else) starts any thread.
            unsafe { std::env::set_var(name, value) };
        }
    }
}

/// Run `bootstrap`'s simulation while the dashboard shows it, until the user quits.
///
/// Runs go on continuously and past failures (unless `SIMULATOR_RUNS` and
/// `SIMULATOR_ON_FAILURE` say otherwise); failures are saved under the findings directory for
/// `name`. The dashboard shows `views` in each run after its own pages, and opens the run of
/// `open` (seed, epoch offset) at start when given.
///
/// # Errors
///
/// * If the dashboard cannot use the terminal, or the simulation fails to run
pub fn run_dashboard<B: simvar::SimBootstrap + Send + 'static>(
    bootstrap: B,
    name: &str,
    title: &str,
    views: Vec<Box<dyn SimView>>,
    extra: Extra,
    open: Option<(u64, u64)>,
) -> Result<Vec<simvar::SimResult>, Box<dyn std::error::Error>> {
    fleet_defaults();
    let interest = Arc::new(Interest::default());
    enable(Arc::clone(&interest), extra);
    let (observer, events) = bmux_simvar_tui::observer();
    simvar::add_observer(observer);
    simvar::set_quiet(true);
    let simulation =
        std::thread::spawn(move || simvar::run_simulation(bootstrap).map_err(|e| e.to_string()));
    let dashboard = bmux_simvar_tui::Dashboard::new(
        bmux_simvar_tui::Options {
            title: title.into(),
            views,
            controller: Some(Box::new(FleetController)),
            interest,
            findings: Some(Findings::new(default_dir(name))),
            open,
        },
        events,
    );
    let shown = dashboard.run();
    // The user left: start no more runs, and end the ones in flight.
    simvar::control().stop();
    shown?;
    Ok(simulation
        .join()
        .map_err(|_| "the simulation thread panicked")??)
}

/// Run `bootstrap`'s simulation continuously without a dashboard.
///
/// It prints a summary line every few seconds and each kind of failure as it first appears, until interrupted (ctrl+c)
/// or the runs `SIMULATOR_RUNS` asks for are done. Failures are saved like the dashboard's.
///
/// # Errors
///
/// * If the simulation fails to run
pub fn run_report<B: simvar::SimBootstrap + Send + 'static>(
    bootstrap: B,
    name: &str,
    extra: Extra,
) -> Result<Vec<simvar::SimResult>, Box<dyn std::error::Error>> {
    fleet_defaults();
    enable(Arc::new(Interest::default()), extra);
    let (observer, events) = bmux_simvar_tui::observer();
    simvar::add_observer(observer);
    simvar::set_quiet(true);
    let simulation =
        std::thread::spawn(move || simvar::run_simulation(bootstrap).map_err(|e| e.to_string()));
    let findings = Findings::new(default_dir(name));
    let build = bmux_simvar_tui::findings::build_identity();
    let mut store = Store::new(Instant::now());
    let mut last = Instant::now();
    let mut reported = 0;
    loop {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => {
                let finished = matches!(event, simvar::observer::SimEvent::Finished);
                store.apply(event, Instant::now());
                if finished {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let now = Instant::now();
        store.tick(now);
        while reported < store.failures.len() {
            for line in report::failure(&store.failures[reported]) {
                println!("{line}");
            }
            let _ = findings.save(&bmux_simvar_tui::findings::Finding::of(
                &store.failures[reported],
                &build,
            ));
            reported += 1;
        }
        if now.duration_since(last) >= Duration::from_secs(5) {
            println!("{}", report::summary(&store, now));
            last = now;
        }
    }
    println!("{}", report::summary(&store, Instant::now()));
    Ok(simulation
        .join()
        .map_err(|_| "the simulation thread panicked")??)
}

/// A simulation, as [`main`] runs it.
pub struct Simulation<B> {
    /// Its name (`pane-runtime`), for its findings directory.
    pub name: &'static str,
    /// Its dashboard title.
    pub title: &'static str,
    /// Its bootstrap.
    pub bootstrap: B,
    /// Its views in the dashboard, after the kernel's.
    pub views: Vec<Box<dyn SimView>>,
    /// Its own telemetry.
    pub extra: Extra,
    /// Its regression seeds file, for `--regression` (see [`regression`]).
    pub regression_seeds: &'static str,
}

/// Run every seed in the regression seeds file at `path`, each alone in a fresh process of
/// this executable, replayed to verify it is deterministic.
///
/// Each line not blank or a comment is `SEED [EPOCH] [NAME=VALUE ...] [# note]`: the run's
/// seed, its clock's epoch offset, and environment variables it needs (a forced personality or
/// profile, say).
///
/// # Errors
///
/// * If the file cannot be read, a line cannot be parsed, or any seed fails
pub fn regression(name: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::fmt::Write as _;
    let seeds = std::fs::read_to_string(path)?;
    let executable = std::env::current_exe()?;
    let mut failed = Vec::new();
    for line in seeds.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let mut words = line.split_whitespace();
        let Some(seed) = words.next() else {
            continue;
        };
        let mut command = std::process::Command::new(&executable);
        command
            .env("SIMULATOR_SEED", seed)
            .env("SIMULATOR_RUNS", "1")
            .env("SIMULATOR_MAX_PARALLEL", "1")
            .env("SIMULATOR_VERIFY_DETERMINISM", "process")
            .env_remove("SIMULATOR_EPOCH_OFFSET");
        let mut replay = format!("SIMULATOR_SEED={seed}");
        for word in words {
            if let Some((variable, value)) = word.split_once('=') {
                command.env(variable, value);
                let _ = write!(replay, " {variable}={value}");
            } else if word.parse::<u64>().is_ok() {
                command.env("SIMULATOR_EPOCH_OFFSET", word);
                let _ = write!(replay, " SIMULATOR_EPOCH_OFFSET={word}");
            } else {
                return Err(format!(
                    "{path}: seed {seed}: {word:?} is neither VARIABLE=VALUE nor an epoch \
                     offset (notes go after a #)"
                )
                .into());
            }
        }
        let status = command.stdout(std::process::Stdio::null()).status()?;
        if !status.success() {
            eprintln!("regression seed failed; replay with: {replay} cargo sim {name}");
            failed.push(seed.to_owned());
        }
    }
    if failed.is_empty() {
        println!("all regression seeds passed");
        Ok(())
    } else {
        Err(format!("{} regression seeds failed", failed.len()).into())
    }
}

/// Run a simulation as `cargo sim NAME [FLAGS]` asks (`args` are the flags):
///
/// * no flags: the runs `SIMULATOR_RUNS` asks for, then their results; failing exits 1.
/// * `--tui [SEED[:EPOCH]]`: the dashboard, running continuously and keeping failures, with
///   the run of `SEED` (and epoch offset `EPOCH`) open when given.
/// * `--fleet`: continuously without the dashboard, printing a summary and each kind of
///   failure as it appears; failing exits 1.
///
/// # Errors
///
/// * For an unknown flag, or when the simulation cannot run
pub fn main<B: simvar::SimBootstrap + Send + 'static>(
    simulation: Simulation<B>,
    args: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let Simulation {
        name,
        title,
        bootstrap,
        mut views,
        extra,
        regression_seeds,
    } = simulation;
    if args.first().map(String::as_str) == Some("--regression") {
        return regression(name, regression_seeds);
    }
    bmux_simvar_tui::register_predicates(kernel::PREDICATES);
    scheduling_defaults();
    let results = match args.first().map(String::as_str) {
        None => simvar::run_simulation(bootstrap)?,
        Some("--tui") => {
            let open = match args.get(1) {
                Some(run) => {
                    let (seed, epoch) = run.split_once(':').unwrap_or((run.as_str(), "0"));
                    Some((seed.parse()?, epoch.parse()?))
                }
                None => None,
            };
            views.insert(0, Box::new(PtyPanel::new()));
            run_dashboard(bootstrap, name, title, views, extra, open)?;
            return Ok(());
        }
        Some("--fleet") => run_report(bootstrap, name, extra)?,
        Some(other) => {
            return Err(format!(
                "unknown option {other}; expected --tui [SEED[:EPOCH]] or --fleet"
            )
            .into());
        }
    };
    let failed: Vec<_> = results.iter().filter(|r| !r.is_success()).collect();
    for result in &failed {
        println!("{result}");
    }
    println!("{} runs, {} failed", results.len(), failed.len());
    if !failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_str()).collect())
            .collect()
    }

    #[test]
    fn screens_interpret_escapes_and_keep_the_last_rows() {
        let mut screen = Screen::new((20, 5));
        screen.update(b"one\r\ntwo\x1b[31mred\x1b[0m\r\nthree", (20, 5));
        let rows = text(&screen.lines(20));
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows[1].starts_with("twored"), "{rows:?}");
        screen.update(
            b"one\r\ntwo\x1b[31mred\x1b[0m\r\nthree\x1b[2J\x1b[Hcleared",
            (20, 5),
        );
        assert!(text(&screen.lines(20))[0].starts_with("cleared"));
    }

    #[test]
    fn delivered_output_is_compared_with_what_was_written() {
        assert_eq!(comparison(b"abc", b"abc").content, "delivered 3 of 3 bytes");
        assert_eq!(comparison(b"abc", b"ab").content, "delivered 2 of 3 bytes");
        assert!(comparison(b"abc", b"ax").content.contains("DIFFERS"));
        assert!(comparison(b"abc", b"ax").content.ends_with("byte 1"));
    }
}
