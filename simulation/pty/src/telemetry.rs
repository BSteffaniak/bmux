//! What the `bmux_pty` simulation shows in `bmux_simvar_tui`, and how it pauses for it.
//!
//! Telemetry is the scheduler's threads and decisions, the invariants' status, and the
//! simulated kernel: its processes, terminals, signals, and what each terminal's programs
//! wrote next to what `bmux_pty` delivered from it. It is taken from `switchy_thread`'s
//! decision hook, throttled by real time, and only reads state, so watching a run never
//! changes it. A [`ReplayControl`] holds the run at decisions, breakpoints and failures; the
//! run sends its state before it waits, so the dashboard shows where it stopped.

use std::{
    any::Any,
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use bmux_simvar_tui::{RunId, SimPanel, sparkline};
use bmux_tui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use bmux_tui_components::terminal_view::TerminalState;
use simvar::observer::{DecisionView, ReplayControl, SchedulerView, ThreadView};
use switchy_pty::simulator::{self, KernelSnapshot, ProcessSnapshot, ProcessState, SignalRecord};
use switchy_thread::simulator::{Decision, SchedulerSnapshot};

/// How often telemetry is sent, in real time.
const INTERVAL: Duration = Duration::from_millis(100);

static ENABLED: OnceLock<Arc<ReplayControl>> = OnceLock::new();

/// Turn telemetry on for every run from now on, with `replay` holding runs at decisions.
pub fn enable(replay: Arc<ReplayControl>) {
    let _ = ENABLED.set(replay);
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
    if ENABLED.get().is_none() {
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
                state: format!("{:?}", t.state),
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

/// Send everything a dashboard shows: the scheduler, the invariants and the kernel.
fn emit(scheduler: SchedulerView) {
    simvar::emit(Arc::new(scheduler));
    simvar::emit(Arc::new(crate::invariants()));
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
    let delivered = switchy_simulation::state::<Delivered>()
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.clone()))
        .collect();
    simvar::emit(Arc::new(PtyTelemetry {
        kernel,
        output,
        delivered,
        signals: simulator::signal_log(),
    }));
}

/// Watch the current run, when telemetry is on: hold it at decisions as the replay control
/// says (sending its state first), and send telemetry every [`INTERVAL`] of real time.
pub fn watch_run() {
    let Some(replay) = ENABLED.get().cloned() else {
        return;
    };
    let last = Mutex::new(None::<Instant>);
    switchy_thread::simulator::set_decision_hook(Some(Arc::new(
        move |decision: &Decision, snapshot: &dyn Fn() -> SchedulerSnapshot| {
            if replay.arrive(decision.number, decision.step, &|| view(&snapshot())) {
                emit(view(&snapshot()));
                replay.wait();
                return;
            }
            let stepping = replay.status().0 != simvar::observer::ReplayMode::Run;
            let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
            let due = last.is_none_or(|at| at.elapsed() >= INTERVAL);
            if !stepping && !due {
                return;
            }
            *last = Some(Instant::now());
            drop(last);
            emit(view(&snapshot()));
        },
    )));
}

/// A failure the run found (`what`): pause for it if the dashboard breaks on failures. Only
/// reads state.
pub fn failure(what: &str) {
    if let Some(replay) = ENABLED.get()
        && replay.failure(what)
    {
        emit(view(&switchy_thread::simulator::scheduler().snapshot()));
        replay.wait();
    }
}

/// Send the run's state now, when telemetry is on (for example at the end of a run, which
/// may come before the next decision would have sent it). Only reads state.
pub fn emit_now() {
    if ENABLED.get().is_some() {
        emit(view(&switchy_thread::simulator::scheduler().snapshot()));
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

/// `left` and `right` next to each other, `left` padded to `width` columns.
fn side_by_side(left: Vec<Line>, right: Vec<Line>, width: usize) -> Vec<Line> {
    let rows = left.len().max(right.len());
    let mut left = left.into_iter();
    let mut right = right.into_iter();
    (0..rows)
        .map(|_| {
            let mut spans = vec![Span::raw("  ")];
            let left = left.next().unwrap_or_default();
            let pad = width.saturating_sub(left.width());
            spans.extend(left.spans);
            spans.push(Span::raw(format!("{} │ ", " ".repeat(pad))));
            spans.extend(right.next().unwrap_or_default().spans);
            Line::from_spans(spans)
        })
        .collect()
}

impl SimPanel for PtyPanel {
    fn title(&self) -> String {
        "Processes & terminals".into()
    }

    fn telemetry(&mut self, run: RunId, payload: &Arc<dyn Any + Send + Sync>) {
        if let Some(telemetry) = payload.downcast_ref::<PtyTelemetry>() {
            self.update(run, telemetry);
        }
    }

    fn lines(&self, run: Option<RunId>, width: u16) -> Vec<Line> {
        let Some(view) = run.and_then(|run| self.runs.get(&run)) else {
            return vec![Line::raw("No kernel telemetry for this run yet.")];
        };
        let telemetry = &view.telemetry;
        let width = usize::from(width);
        let mut lines = vec![Line::from_spans(vec![Span::styled(
            format!(
                "Processes ({} kernel)",
                telemetry.kernel.personality.to_lowercase()
            ),
            bold(),
        )])];
        lines.extend(process_tree(&telemetry.kernel.processes));

        lines.push(Line::raw(""));
        lines.push(Line::from_spans(vec![Span::styled(
            format!(
                "Signals ({} delivered, latest last)",
                telemetry.signals.len()
            ),
            bold(),
        )]));
        for record in telemetry.signals.iter().rev().take(8).rev() {
            lines.push(Line::raw(format!(
                "  {:?} -> {} (serial {}, session {})",
                record.signal, record.pid, record.serial, record.session_serial
            )));
        }

        for t in &telemetry.kernel.terminals {
            let Some(screens) = view.terminals.get(&t.name) else {
                continue;
            };
            lines.push(Line::raw(""));
            lines.push(Line::from_spans(vec![
                Span::styled(format!("{} ", t.name), bold()),
                Span::raw(format!(
                    "{}x{}  master handles {}  slave holders {}  {}  in {}  ",
                    t.size.cols,
                    t.size.rows,
                    t.master_handles,
                    t.slave_holders,
                    if t.hung_up { "hung up" } else { "open" },
                    t.input_buffered,
                )),
                comparison(
                    bytes_of(&telemetry.output, &t.name),
                    bytes_of(&telemetry.delivered, &t.name),
                ),
            ]));
            lines.push(Line::from_spans(vec![
                Span::raw(format!("  buffered {:>6} B  ", t.output_buffered)),
                Span::styled(
                    sparkline(&screens.buffered, u16::try_from(width / 2).unwrap_or(40)),
                    Style::new().fg(Color::Cyan),
                ),
            ]));
            let column = width.saturating_sub(5) / 2;
            if column >= 20 {
                let mut heading = side_by_side(
                    vec![Line::raw("written by programs")],
                    vec![Line::raw("delivered by bmux_pty")],
                    column,
                );
                for line in &mut heading {
                    for span in &mut line.spans {
                        span.style = bold();
                    }
                }
                lines.extend(heading);
                lines.extend(side_by_side(
                    screens.written.lines(column),
                    screens.delivered.lines(column),
                    column,
                ));
            } else {
                lines.push(Line::raw("  written by programs:"));
                lines.extend(screens.written.lines(width.saturating_sub(2)));
                lines.push(Line::raw("  delivered by bmux_pty:"));
                lines.extend(screens.delivered.lines(width.saturating_sub(2)));
            }
        }
        lines
    }
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

    #[test]
    fn screens_sit_side_by_side() {
        let lines = side_by_side(
            vec![Line::raw("ab"), Line::raw("c")],
            vec![Line::raw("x")],
            4,
        );
        assert_eq!(text(&lines), ["  ab   │ x", "  c    │ "]);
    }
}
