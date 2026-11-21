//! What the `bmux_pty` simulation shows in `bmux_simvar_tui`.
//!
//! That is the scheduler's threads and decisions, and the simulated kernel's processes and
//! terminals. Telemetry is taken from `switchy_thread`'s decision hook, throttled by real time,
//! and only reads state, so watching a run never changes it.

use std::{
    any::Any,
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use bmux_simvar_tui::{RunId, SimPanel};
use bmux_tui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use simvar::observer::{DecisionView, ReplayControl, SchedulerView, ThreadView};
use switchy_pty::simulator::{self, KernelSnapshot, ProcessState};
use switchy_thread::simulator::{Decision, SchedulerSnapshot};

/// How often telemetry is sent, in real time.
const INTERVAL: Duration = Duration::from_millis(100);

static ENABLED: OnceLock<Option<Arc<ReplayControl>>> = OnceLock::new();

/// Turn telemetry on for every run from now on, with `replay` holding runs at decisions.
pub fn enable(replay: Arc<ReplayControl>) {
    let _ = ENABLED.set(Some(replay));
}

/// The kernel and its terminals' output, as of one moment.
#[derive(Debug, Clone)]
pub struct PtyTelemetry {
    /// Processes and terminals.
    pub kernel: KernelSnapshot,
    /// Each terminal's output so far.
    pub output: Vec<(String, Vec<u8>)>,
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

/// Watch the current run, when telemetry is on: hold it at decisions as the replay control
/// says, and send telemetry every [`INTERVAL`] of real time.
pub fn watch_run() {
    let Some(Some(replay)) = ENABLED.get().cloned() else {
        return;
    };
    let last = Mutex::new(None::<Instant>);
    switchy_thread::simulator::set_decision_hook(Some(Arc::new(
        move |decision: &Decision, snapshot: &dyn Fn() -> SchedulerSnapshot| {
            replay.checkpoint(decision.number);
            let paused = replay.status().0 != simvar::observer::ReplayMode::Run;
            let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
            let due = decision.number.is_multiple_of(64)
                || last.is_none_or(|at| at.elapsed() >= INTERVAL);
            if !paused && !due {
                return;
            }
            *last = Some(Instant::now());
            drop(last);
            simvar::emit(Arc::new(view(&snapshot())));
            emit_kernel();
        },
    )));
}

/// Send the kernel's state now, when telemetry is on (for example at the end of a run, which
/// may come before the next decision would have sent it). Only reads state.
pub fn emit_now() {
    if !matches!(ENABLED.get(), Some(Some(_))) {
        return;
    }
    simvar::emit(Arc::new(view(
        &switchy_thread::simulator::scheduler().snapshot(),
    )));
    emit_kernel();
}

fn emit_kernel() {
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
    simvar::emit(Arc::new(PtyTelemetry { kernel, output }));
}

/// The dashboard tab showing the selected run's processes and terminals.
#[derive(Debug, Default)]
pub struct PtyPanel {
    latest: BTreeMap<RunId, PtyTelemetry>,
}

/// The last `rows` lines of a terminal's output, with escape sequences and carriage returns
/// dropped.
fn screen(output: &[u8], rows: usize, width: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(output);
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip a CSI sequence up to its final byte.
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() || next == '~' {
                        break;
                    }
                }
            }
        } else if c != '\r' {
            plain.push(c);
        }
    }
    let lines: Vec<String> = plain
        .lines()
        .map(|l| l.chars().take(width).collect())
        .collect();
    lines[lines.len().saturating_sub(rows)..].to_vec()
}

impl SimPanel for PtyPanel {
    fn title(&self) -> String {
        "Processes & terminals".into()
    }

    fn telemetry(&mut self, run: RunId, payload: &Arc<dyn Any + Send + Sync>) {
        if let Some(telemetry) = payload.downcast_ref::<PtyTelemetry>() {
            self.latest.insert(run, telemetry.clone());
        }
    }

    fn lines(&self, run: Option<RunId>, width: u16) -> Vec<Line> {
        let Some(telemetry) = run.and_then(|run| self.latest.get(&run)) else {
            return vec![Line::raw("No kernel telemetry for this run yet.")];
        };
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let mut lines = vec![Line::from_spans(vec![Span::styled(
            format!(
                "Processes ({} kernel)",
                telemetry.kernel.personality.to_lowercase()
            ),
            bold,
        )])];
        for p in &telemetry.kernel.processes {
            let (state, color) = match &p.state {
                ProcessState::Running => ("running".to_owned(), Color::Green),
                ProcessState::Zombie(exit) => (format!("exited ({exit})"), Color::Yellow),
            };
            lines.push(Line::from_spans(vec![
                Span::raw(format!(
                    "  {:>5} {:<16} group {:>5} session {:>5} {:<16} ",
                    p.pid,
                    p.program,
                    p.group,
                    p.session,
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
        for t in &telemetry.kernel.terminals {
            lines.push(Line::raw(""));
            lines.push(Line::from_spans(vec![
                Span::styled(format!("{} ", t.name), bold),
                Span::raw(format!(
                    "{}x{}  master handles {}  slave holders {}  {}  buffered {} out / {} in  written {}",
                    t.size.cols,
                    t.size.rows,
                    t.master_handles,
                    t.slave_holders,
                    if t.hung_up { "hung up" } else { "open" },
                    t.output_buffered,
                    t.input_buffered,
                    t.written,
                )),
            ]));
            let output = telemetry
                .output
                .iter()
                .find(|(name, _)| *name == t.name)
                .map_or(&[][..], |(_, bytes)| bytes.as_slice());
            for line in screen(output, 6, usize::from(width.saturating_sub(4))) {
                lines.push(Line::raw(format!("  | {line}")));
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screens_drop_escapes_and_keep_the_last_rows() {
        let output = b"one\r\ntwo\x1b[31mred\x1b[0m\r\nthree\r\n";
        assert_eq!(screen(output, 2, 80), vec!["twored", "three"]);
        assert_eq!(screen(b"abcdef", 5, 3), vec!["abc"]);
    }
}
