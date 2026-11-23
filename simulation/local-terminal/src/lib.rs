//! Deterministic simulation testing for bmux's plugin-less local terminal.
//!
//! Each simvar run serves one shell with `bmux_local_terminal::serve_attachment` over
//! in-memory streams, through several attachments: a client says hello with a size, types
//! lines, sometimes resizes, and detaches at a random point; the last attachment ends the
//! shell. The shell is a simulated program on switchy's kernel, and the server and client run
//! on the run's executor, so the seed decides every interleaving.
//!
//! Invariants checked:
//!
//! 1. Every attachment's handshake succeeds.
//! 2. The shell answers every line typed, in order, across detaching and reattaching; and what
//!    clients receive is, line by line, an in-order part of what it wrote (output already sent
//!    to a client that detaches without reading it is that client's to lose).
//! 3. A resize reaches the shell's terminal.
//! 4. Ending the shell delivers its exit code, empties the slot, and leaves nothing running.
//! 5. Every simulated thread ends with the run.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::{io, time::Duration};

use bmux_local_terminal::{HELLO, serve_attachment, size_to_bytes};
use sim_support::invariants::{checked, violated};
use simvar::{Sim, SimBootstrap, SimConfig};
use switchy_pty::{
    PtySize,
    simulator::{self, Config, Faults, Personality, Process, ProcessState},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};

/// The invariants a run checks (see the crate documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Invariant {
    /// 1.
    Handshake,
    /// 2.
    AnsweredInOrder,
    /// 3.
    ResizeReaches,
    /// 4.
    ExitCleansUp,
    /// 5.
    ThreadsEnd,
    /// Not numbered: an attachment's server ends without error (all but the last).
    Attachment,
}

impl sim_support::invariants::Invariant for Invariant {
    const ALL: &'static [Self] = &[
        Self::Handshake,
        Self::AnsweredInOrder,
        Self::ResizeReaches,
        Self::ExitCleansUp,
        Self::ThreadsEnd,
        Self::Attachment,
    ];

    fn statement(self) -> &'static str {
        match self {
            Self::Handshake => "every attachment's handshake succeeds",
            Self::AnsweredInOrder => "the shell answers every typed line, in order",
            Self::ResizeReaches => "a resize reaches the shell's terminal",
            Self::ExitCleansUp => "ending the shell delivers its code and leaves nothing",
            Self::ThreadsEnd => "every simulated thread ends with the run",
            Self::Attachment => "an attachment's server ends without error",
        }
    }

    fn numbered(self) -> bool {
        self != Self::Attachment
    }
}

/// Record that `invariant` failed, saying `what`, in the run's `violations`.
fn fail(violations: &mut Vec<String>, invariant: Invariant, what: impl std::fmt::Display) {
    violations.push(violated(invariant, what));
}

/// How long anything has to settle, in simulated time.
const SETTLE: Duration = Duration::from_secs(5);

/// The simvar bootstrap: one client per run drives the workload.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalTerminalBootstrap;

impl SimBootstrap for LocalTerminalBootstrap {
    fn build_sim(&self, mut config: SimConfig) -> SimConfig {
        config.duration = Duration::from_hours(24);
        config
    }

    fn on_start(&self, sim: &mut impl Sim) {
        sim.client("local-terminal-workload", async move {
            run_workload().await.map_err(|violations| {
                Box::new(io::Error::other(violations.join("\n")))
                    as Box<dyn std::error::Error + Send>
            })
        });
    }
}

/// The shell: answers each line with `got:<line>`, and exits with the code given by `exit N`.
fn shell(process: &mut Process) -> i32 {
    let mut line = Vec::new();
    let mut buffer = [0_u8; 256];
    loop {
        match process.read(&mut buffer) {
            Ok(0) | Err(_) => return 0,
            Ok(read) => {
                for &byte in &buffer[..read] {
                    if byte != b'\n' {
                        line.push(byte);
                        continue;
                    }
                    let text = String::from_utf8_lossy(&line).into_owned();
                    line.clear();
                    if let Some(code) = text.strip_prefix("exit ") {
                        return code.parse().unwrap_or(1);
                    }
                    if process.write(format!("got:{text}\n").as_bytes()).is_err() {
                        return 1;
                    }
                }
            }
        }
    }
}

fn draw(bound: u64) -> u64 {
    switchy_simulation::stream("local_terminal_workload")
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .below(bound.max(1))
}

fn random_size() -> PtySize {
    PtySize::new(
        u16::try_from(20 + draw(200)).unwrap_or(80),
        u16::try_from(5 + draw(60)).unwrap_or(24),
    )
}

/// What the client has seen across attachments.
#[derive(Default)]
struct Seen {
    output: Vec<u8>,
    exit: Option<u8>,
}

/// Read the server's frames until it ends or `stop` holds, at most [`SETTLE`].
async fn read_frames(client: &mut DuplexStream, seen: &mut Seen, stop: impl Fn(&Seen) -> bool) {
    let deadline = switchy_time::Instant::now() + SETTLE;
    while !stop(seen) && seen.exit.is_none() {
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return;
        }
        match switchy_async::time::timeout(remaining, client.read_u32()).await {
            Ok(Ok(0)) => {
                seen.exit = client.read_u8().await.ok();
            }
            Ok(Ok(len)) => {
                let mut bytes = vec![0; usize::try_from(len).unwrap_or(0)];
                if client.read_exact(&mut bytes).await.is_err() {
                    return;
                }
                seen.output.extend(bytes);
            }
            _ => return,
        }
    }
}

/// The shell's terminal, while it runs.
fn shell_terminal() -> Option<String> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|p| p.program == "sh" && p.state == ProcessState::Running)
        .and_then(|p| p.terminal)
}

/// Whether every complete answer line (`got:...`) of `received` appears in `written`, in the
/// same order. (The terminal's echo of typed input is not part of what the shell wrote.)
fn in_order_part(received: &[u8], written: &[u8]) -> bool {
    let written = String::from_utf8_lossy(written);
    let mut rest = written.as_ref();
    String::from_utf8_lossy(received)
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n') && line.starts_with("got:"))
        .all(|line| {
            rest.find(line).is_some_and(|at| {
                rest = &rest[at + line.len()..];
                true
            })
        })
}

/// Whether `output` answers each of `lines`, in order.
fn answered(output: &[u8], lines: &[String]) -> bool {
    let text = String::from_utf8_lossy(output);
    let mut rest = text.as_ref();
    lines.iter().all(|line| {
        let answer = format!("got:{line}\r\n");
        rest.find(&answer).is_some_and(|at| {
            rest = &rest[at + answer.len()..];
            true
        })
    })
}

/// Invariants 2 and 4, once the shell has exited.
fn final_checks(
    violations: &mut Vec<String>,
    terminal: Option<&str>,
    typed: &[String],
    seen: &Seen,
    code: u8,
    slot_full: bool,
) {
    // 2. The shell answered every typed line in order, and clients got an in-order part of it.
    checked(Invariant::AnsweredInOrder);
    let written = terminal
        .and_then(simulator::terminal_written)
        .unwrap_or_default();
    if !answered(&written, typed) {
        fail(
            violations,
            Invariant::AnsweredInOrder,
            format!(
                "the shell did not answer every typed line in order: {typed:?}; it wrote {:?}",
                String::from_utf8_lossy(&written)
            ),
        );
    }
    if !in_order_part(&seen.output, &written) {
        fail(
            violations,
            Invariant::AnsweredInOrder,
            format!(
                "clients received {:?}, not an in-order part of what the shell wrote, {:?}",
                String::from_utf8_lossy(&seen.output),
                String::from_utf8_lossy(&written)
            ),
        );
    }
    // 4. The shell's exit code arrived, the slot is empty, and nothing runs.
    checked(Invariant::ExitCleansUp);
    if seen.exit != Some(code) {
        fail(
            violations,
            Invariant::ExitCleansUp,
            format!("exit code {:?}, not {code}", seen.exit),
        );
    }
    if slot_full {
        fail(
            violations,
            Invariant::ExitCleansUp,
            "the shell's slot is not empty after it exited",
        );
    }
    let running: Vec<u32> = simulator::snapshot()
        .processes
        .into_iter()
        .filter(|p| p.state == ProcessState::Running)
        .map(|p| p.pid)
        .collect();
    if !running.is_empty() {
        fail(
            violations,
            Invariant::ExitCleansUp,
            format!("processes still running: {running:?}"),
        );
    }
}

/// An input frame typing `line` and Enter.
fn input_frame(line: &str) -> Vec<u8> {
    let bytes = format!("{line}\r");
    let mut frame = vec![1];
    frame.extend(u16::try_from(bytes.len()).unwrap_or(0).to_be_bytes());
    frame.extend(bytes.as_bytes());
    frame
}

/// Resize the terminal and check invariant 3.
async fn resize(client: &mut DuplexStream, seen: &mut Seen, violations: &mut Vec<String>) {
    // 3. A resize reaches the terminal.
    checked(Invariant::ResizeReaches);
    let size = random_size();
    let mut frame = vec![2];
    frame.extend(size_to_bytes(size));
    let _ = client.write_all(&frame).await;
    read_frames(client, seen, |_| false).await;
    let sized = simulator::snapshot()
        .terminals
        .iter()
        .any(|t| t.size == size && t.master_handles > 0);
    if !sized && seen.exit.is_none() {
        fail(
            violations,
            Invariant::ResizeReaches,
            format!("the terminal never became {size:?}"),
        );
    }
}

/// Invariant 5: let the terminal's threads end with the run.
async fn drain_threads(violations: &mut Vec<String>) {
    checked(Invariant::ThreadsEnd);
    let deadline = switchy_time::Instant::now() + SETTLE;
    while switchy_thread::simulator::try_scheduler().is_some_and(|s| s.others_alive()) {
        if switchy_time::Instant::now() >= deadline {
            fail(
                violations,
                Invariant::ThreadsEnd,
                "simulated threads still running at the end",
            );
            return;
        }
        switchy_async::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Put attachment `attachment` on the run's timeline.
fn attach_event(attachment: u64) {
    sim_support::event(
        "attach",
        format!("attachment {attachment}"),
        bmux_simvar_tui::Tone::Info,
    );
}

/// Set up the run's kernel and telemetry.
fn start_run() {
    switchy_time::simulator::set_step_multiplier(1);
    sim_support::watch_run();
    simulator::configure(Config {
        personality: Personality::Linux,
        pid_reuse: 0.3,
        faults: Faults::default(),
    });
    simulator::install_default_programs();
    simulator::register_program("sh", shell);
}

/// Say hello on `client` with a random size, checking invariant 1.
async fn handshake(client: &mut DuplexStream, attachment: u64, violations: &mut Vec<String>) {
    // 1. The handshake.
    checked(Invariant::Handshake);
    let size = random_size();
    let mut hello = HELLO.to_vec();
    hello.extend(size_to_bytes(size));
    let mut ack = [0_u8; 8];
    let shook = client.write_all(&hello).await.is_ok()
        && switchy_async::time::timeout(SETTLE, client.read_exact(&mut ack))
            .await
            .is_ok_and(|read| read.is_ok())
        && &ack == HELLO;
    if !shook {
        fail(
            violations,
            Invariant::Handshake,
            format!("attachment {attachment}'s handshake failed"),
        );
    }
}

/// One run's workload. Returns the violations found, if any.
///
/// # Errors
///
/// * Every invariant violated, one line each
///
/// # Panics
///
/// * If the server task panics
pub async fn run_workload() -> Result<(), Vec<String>> {
    start_run();

    let mut violations = Vec::new();
    let mut slot = None;
    let mut seen = Seen::default();
    let mut typed: Vec<String> = Vec::new();
    let mut terminal: Option<String> = None;
    let attachments = 1 + draw(4);
    sim_support::shape(vec![
        "personality:linux".into(),
        format!("attachments:{attachments}"),
    ]);
    let code = u8::try_from(draw(100)).unwrap_or(0);
    for attachment in 0..attachments {
        let last = attachment + 1 == attachments;
        attach_event(attachment);
        let (mut client, mut server) = tokio::io::duplex(4096);
        let mut server_slot = slot.take();
        let server_task = switchy_async::task::spawn(async move {
            let result = serve_attachment(&mut server, "/bin/sh", &mut server_slot).await;
            (result, server_slot)
        });
        sim_support::event(
            "attach",
            format!("attachment {attachment}"),
            bmux_simvar_tui::Tone::Info,
        );
        handshake(&mut client, attachment, &mut violations).await;
        if terminal.is_none() {
            terminal = shell_terminal();
        }
        for _ in 0..draw(6) {
            if draw(4) == 0 {
                resize(&mut client, &mut seen, &mut violations).await;
            } else {
                let line: String = (0..=draw(20))
                    .map(|_| char::from(b'a' + u8::try_from(draw(26)).unwrap_or(0)))
                    .collect();
                if client.write_all(&input_frame(&line)).await.is_ok() {
                    typed.push(line);
                }
            }
        }
        if last {
            let _ = client
                .write_all(&input_frame(&format!("exit {code}")))
                .await;
            read_frames(&mut client, &mut seen, |_| false).await;
        } else {
            // Read some of the output (perhaps none), then detach.
            if draw(2) == 0 {
                read_frames(&mut client, &mut seen, |seen| {
                    answered(&seen.output, &typed)
                })
                .await;
            }
            let _ = client.write_all(&[0]).await;
        }
        let (result, returned) = server_task.await.expect("the server task ends");
        if !last {
            checked(Invariant::Attachment);
        }
        if let Err(error) = result
            && !last
        {
            fail(
                &mut violations,
                Invariant::Attachment,
                format!("attachment {attachment} failed: {error:#}"),
            );
        }
        slot = returned;
    }
    final_checks(
        &mut violations,
        terminal.as_deref(),
        &typed,
        &seen,
        code,
        slot.is_some(),
    );
    drop(slot);
    simulator::kill_all();
    drain_threads(&mut violations).await;
    sim_support::finish_run();
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

#[cfg(test)]
mod tests {
    use sim_support::invariants::Invariant as _;

    use super::Invariant;

    #[test]
    fn the_documentation_lists_every_invariant_in_order() {
        let docs = include_str!("lib.rs");
        for invariant in Invariant::ALL {
            if let Some(number) = invariant.number() {
                assert!(docs.contains(&format!("//! {number}. ")), "{invariant:?}");
            }
        }
    }
}
