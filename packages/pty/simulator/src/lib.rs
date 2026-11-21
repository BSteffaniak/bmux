//! Deterministic simulation testing for `bmux_pty`.
//!
//! Each simvar run drives `bmux_pty` against switchy's simulated kernel from a few driver
//! threads. Every driver starts a randomly chosen program on a terminal and then does random
//! things to it, as a real caller might: read events in every way the API allows, stall, type,
//! resize, interrupt, kill, stop output, or drop the process early. Afterwards it checks what
//! it saw against what really happened in the kernel. The run's seed decides everything (the
//! programs, the actions, the thread interleavings, the kernel's faults and personality), so a
//! failing run replays exactly.
//!
//! Invariants checked:
//!
//! 1. Exactly one `Exited` per process, last; nothing after it.
//! 2. With `output_complete`, the terminal output and captured stdout delivered are exactly
//!    what programs wrote, in order; otherwise they are a prefix of it.
//! 3. The control handle reports the exit before (or as) `Exited` is delivered.
//! 4. After a kill or a drop, nothing in the process group is left running.
//! 5. An interrupt, kill or termination never reaches a session no driver signalled.
//! 6. A program reporting window sizes reports the last resize.
//! 7. No deadlock and no simulated thread left behind (checked by the scheduler and simvar),
//!    and the drivers finish within a budget of simulated time (no livelock).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::{collections::BTreeSet, fmt::Write as _, io, time::Duration};

use bmux_pty::{PtyCommand, PtyEvent, PtyExit, PtySize};
use simvar::{Sim, SimBootstrap, SimConfig};
use switchy_pty::{
    Signal,
    simulator::{self, Config, Faults, Personality, ProcessState},
};
use switchy_thread::{
    sync::{Arc, Mutex, mpsc},
    thread,
};

/// Simulated time the drivers have to finish in; more means a livelock.
const BUDGET: Duration = Duration::from_secs(120);

/// The simvar bootstrap: one client per run drives the workload.
#[derive(Debug, Default, Clone, Copy)]
pub struct PtyBootstrap;

impl SimBootstrap for PtyBootstrap {
    fn build_sim(&self, mut config: SimConfig) -> SimConfig {
        config.duration = Duration::from_hours(24);
        config
    }

    fn on_start(&self, sim: &mut impl Sim) {
        sim.client("pty-workload", async move {
            run_workload().map_err(|violations| {
                Box::new(io::Error::other(violations.join("\n")))
                    as Box<dyn std::error::Error + Send>
            })
        });
    }
}

fn lock<T>(mutex: &Mutex<T>) -> switchy_thread::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(switchy_thread::sync::PoisonError::into_inner)
}

/// The run's draws for the workload as a whole.
fn rng() -> Arc<std::sync::Mutex<switchy_simulation::Rng>> {
    switchy_simulation::stream("bmux_pty_workload")
}

fn draw(stream: &std::sync::Mutex<switchy_simulation::Rng>, bound: u64) -> u64 {
    stream
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .below(bound.max(1))
}

fn chance(stream: &std::sync::Mutex<switchy_simulation::Rng>, p: f64) -> bool {
    stream
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .chance(p)
}

/// The kernel configuration: from `SIMULATOR_PTY_*` when set, otherwise drawn from the seed.
fn kernel_config() -> Config {
    let from_env = Config::from_env();
    let stream = rng();
    let personality = if std::env::var("SIMULATOR_PTY_PERSONALITY").is_ok() {
        from_env.personality
    } else if chance(&stream, 0.5) {
        Personality::Linux
    } else {
        Personality::MacOs
    };
    let faults = if std::env::var("SIMULATOR_PTY_FAULTS").is_ok() {
        from_env.faults
    } else if chance(&stream, 0.3) {
        Faults {
            interrupted: 0.05,
            short: 0.1,
            spawn: 0.02,
        }
    } else {
        Faults::default()
    };
    Config {
        personality,
        pid_reuse: from_env.pid_reuse,
        faults,
    }
}

/// What the drivers did that the global invariants need to know.
#[derive(Default)]
struct Shared {
    violations: Vec<String>,
    /// Serials of session leaders some driver interrupted, killed or dropped.
    signalled: BTreeSet<u64>,
    /// What each driver did that can signal a group: driver, its process's id and serial,
    /// the action, and whether its process had already exited.
    signal_calls: Vec<(u64, u32, Option<u64>, &'static str, bool)>,
    summary: u64,
}

/// One run's workload. Returns the violations found, if any.
///
/// # Errors
///
/// * Every invariant violated, one line each
///
/// # Panics
///
/// * If a driver thread cannot be started
pub fn run_workload() -> Result<(), Vec<String>> {
    // Fine-grained time, so 10 ms polls and grace periods mean what they say.
    switchy_time::simulator::set_step_multiplier(1);
    let config = kernel_config();
    simulator::configure(config);
    simulator::install_default_programs();

    let shared = Arc::new(Mutex::new(Shared::default()));
    let drivers = 1 + draw(&rng(), 3);
    let (finished, finishing) = mpsc::channel();
    let handles: Vec<_> = (0..drivers)
        .map(|index| {
            let shared = Arc::clone(&shared);
            let finished = finished.clone();
            thread::Builder::new()
                .name(format!("driver-{index}"))
                .spawn(move || {
                    drive(index, &shared);
                    let _ = finished.send(index);
                })
                .expect("a driver thread starts")
        })
        .collect();
    drop(finished);
    let deadline = switchy_time::instant_now() + BUDGET;
    for _ in 0..drivers {
        let left = deadline.saturating_duration_since(switchy_time::instant_now());
        if finishing.recv_timeout(left).is_err() {
            lock(&shared).violations.push(format!(
                "the drivers did not finish within {BUDGET:?} of simulated time (livelock?)"
            ));
            break;
        }
    }
    // Signals the drivers' processes received, before the end-of-run cleanup kills the rest.
    let signal_log = simulator::signal_log();
    simulator::kill_all();
    for (index, handle) in handles.into_iter().enumerate() {
        if !handle.is_finished() {
            continue;
        }
        if handle.join().is_err() {
            lock(&shared)
                .violations
                .push(format!("driver {index} panicked"));
        }
    }

    // 5. Signals that end processes only reach sessions some driver signalled.
    let signalled = lock(&shared).signalled.clone();
    for record in signal_log {
        let ending = matches!(
            record.signal,
            Signal::Interrupt | Signal::Kill | Signal::Terminate | Signal::Quit
        );
        if ending && !signalled.contains(&record.session_serial) {
            let suspects: Vec<String> = lock(&shared)
                .signal_calls
                .iter()
                .filter(|(_, pid, _, _, _)| *pid == record.pid)
                .map(|(driver, pid, serial, action, exited)| {
                    format!(
                        "driver {driver} {action} its process {pid} (serial {serial:?}) \
                         after it {} exited",
                        if *exited { "had" } else { "had not" }
                    )
                })
                .collect();
            lock(&shared).violations.push(format!(
                "{:?} reached process {} (serial {}) of session {}, which no driver signalled; \
                 drivers that signalled id {}: [{}]",
                record.signal,
                record.pid,
                record.serial,
                record.session_serial,
                record.pid,
                suspects.join("; ")
            ));
        }
    }

    simulator::kill_all();
    switchy_thread::simulator::run_until_idle();

    let shared = lock(&shared);
    simvar::record(shared.summary);
    if shared.violations.is_empty() {
        Ok(())
    } else {
        Err(shared.violations.clone())
    }
}

/// A program for a driver to run, and what it supports.
struct Scenario {
    command: PtyCommand,
    description: String,
    /// It reads what is typed.
    typing: bool,
    /// It reports every window size as `COLSxROWS`.
    reports_resizes: bool,
    /// Its standard output is a pipe (so reports arrive as `Stdout`, without `\r`).
    captured: bool,
}

fn scenario(stream: &std::sync::Mutex<switchy_simulation::Rng>) -> Scenario {
    let mut typing = false;
    let mut reports_resizes = false;
    let (name, args): (&str, Vec<String>) = match draw(stream, 11) {
        0 | 1 => (
            "flood",
            vec![
                (1 + draw(stream, 30_000)).to_string(),
                (1 + draw(stream, 2000)).to_string(),
            ],
        ),
        2 => ("echo", vec!["hello".into(), "world".into()]),
        3 => ("exit", vec![draw(stream, 4).to_string()]),
        4 => {
            typing = true;
            ("cat", vec![])
        }
        5 => ("sleep", vec![draw(stream, 2000).to_string()]),
        6 => ("ignore-hup", vec![]),
        7 => ("daemonize", vec![draw(stream, 800).to_string()]),
        8 => ("shell", vec![(1 + draw(stream, 3)).to_string()]),
        9 => {
            reports_resizes = true;
            ("resize-reporter", vec![])
        }
        _ => {
            typing = true;
            ("never-reads", vec![])
        }
    };
    let mut command = PtyCommand::new(name).args(&args);
    let mut description = format!("{name} {}", args.join(" "));
    let captured = chance(stream, 0.3);
    if captured {
        command = command.capture_stdout();
        description.push_str(" (stdout captured)");
        // Typed input reaches a captured process only through `input`.
        if typing {
            command = command.input("typed through a pipe\n");
            typing = false;
        }
    }
    let grace = [0, 10, 50, 250, 1000][usize::try_from(draw(stream, 5)).unwrap_or(0)];
    command = command.drain_grace(Duration::from_millis(grace));
    let _ = write!(description, ", grace {grace} ms");
    Scenario {
        command,
        description,
        typing,
        reports_resizes,
        captured,
    }
}

fn random_size(stream: &std::sync::Mutex<switchy_simulation::Rng>) -> PtySize {
    PtySize::new(
        u16::try_from(20 + draw(stream, 200)).unwrap_or(80),
        u16::try_from(5 + draw(stream, 60)).unwrap_or(24),
    )
}

/// The kind of thing a driver does next.
enum Action {
    Next(Duration),
    Try,
    Ready(usize),
    Stall(Duration),
    Type,
    Resize,
    Interrupt,
    Kill,
    StopOutput,
    Drop,
}

fn action(stream: &std::sync::Mutex<switchy_simulation::Rng>) -> Action {
    let millis = |bound| Duration::from_millis(draw(stream, bound));
    match draw(stream, 100) {
        0..=34 => Action::Next(millis(100)),
        35..=44 => Action::Try,
        45..=49 => Action::Ready(1 + usize::try_from(draw(stream, 8)).unwrap_or(0)),
        50..=59 => Action::Stall(millis(200)),
        60..=69 => Action::Type,
        70..=75 => Action::Resize,
        76..=78 => Action::Interrupt,
        79..=81 => Action::Kill,
        82..=84 => Action::StopOutput,
        85 => Action::Drop,
        _ => Action::Next(Duration::from_millis(500 + draw(stream, 2000))),
    }
}

/// What a driver collected from its process.
#[derive(Default)]
struct Observed {
    output: Vec<u8>,
    stdout: Vec<u8>,
    exits: Vec<PtyExit>,
    after_exit: usize,
}

impl Observed {
    fn take(&mut self, event: Option<PtyEvent>) {
        match event {
            Some(PtyEvent::Output(bytes)) => {
                if !self.exits.is_empty() {
                    self.after_exit += 1;
                }
                self.output.extend(bytes);
            }
            Some(PtyEvent::Stdout(bytes)) => {
                if !self.exits.is_empty() {
                    self.after_exit += 1;
                }
                self.stdout.extend(bytes);
            }
            Some(PtyEvent::Exited(exit)) => self.exits.push(exit),
            None => {}
        }
    }
}

/// Whether `delivered` is all of `written` (when `complete`) or a prefix of it.
fn check_stream(name: &str, delivered: &[u8], written: &[u8], complete: bool) -> Option<String> {
    let ok = if complete {
        delivered == written
    } else {
        written.starts_with(delivered)
    };
    (!ok).then(|| {
        format!(
            "{name}: delivered {} bytes, {} written (complete: {complete}); first difference at {}",
            delivered.len(),
            written.len(),
            delivered
                .iter()
                .zip(written)
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| delivered.len().min(written.len()))
        )
    })
}

#[allow(clippy::too_many_lines)] // one driver's script, step by step
fn drive(index: u64, shared: &Mutex<Shared>) {
    let stream = switchy_simulation::stream(&format!("bmux_pty_driver_{index}"));
    let scenario = scenario(&stream);
    let violation = |message: String| {
        lock(shared).violations.push(format!(
            "driver {index} ({}): {message}",
            scenario.description
        ));
    };
    let mut process = match scenario.command.clone().spawn(random_size(&stream)) {
        Ok(process) => process,
        Err(error) => {
            // Spawn faults are injected; anything else is a failure.
            if error.kind() != io::ErrorKind::WouldBlock {
                violation(format!("spawn failed: {error}"));
            }
            return;
        }
    };
    let pid = process.id();
    // Exact even if the process already exited and its id was reused by another driver.
    let spawned = simulator::last_spawned().filter(|s| s.pid == pid);
    if spawned.is_none() {
        violation(format!(
            "the kernel has no record of spawning {pid} from this driver"
        ));
    }
    let serial = spawned.as_ref().map(|s| s.serial);
    let io = spawned.map(|s| s.io);
    let control = process.control();
    let signal_self = |action: &'static str| {
        let mut shared = lock(shared);
        if let Some(serial) = serial {
            shared.signalled.insert(serial);
        }
        let exited = control.exit_status().is_some();
        shared
            .signal_calls
            .push((index, pid, serial, action, exited));
    };
    let mut observed = Observed::default();
    let mut typed = false;
    let mut killed = false;
    let mut output_stopped = false;
    let mut writer = None;
    let steps = 10 + draw(&stream, 60);
    for _ in 0..steps {
        if !observed.exits.is_empty() {
            break;
        }
        match action(&stream) {
            Action::Next(timeout) => observed.take(process.next_event_timeout(timeout)),
            Action::Try => observed.take(process.try_event()),
            Action::Ready(limit) => {
                let events = process.ready_events(limit);
                if events.len() > limit {
                    violation(format!("ready_events({limit}) returned {}", events.len()));
                }
                for event in events {
                    observed.take(Some(event));
                }
            }
            Action::Stall(duration) => thread::sleep(duration),
            Action::Type if scenario.typing => {
                if writer.is_none() {
                    writer = control.writer().ok();
                }
                if let Some(writer) = &writer {
                    let line: String = (0..=draw(&stream, 40))
                        .map(|_| char::from(b'a' + u8::try_from(draw(&stream, 26)).unwrap_or(0)))
                        .collect();
                    if writer.write(format!("{line}\r")).is_ok() {
                        typed = true;
                    }
                }
            }
            Action::Type => {}
            Action::Resize => {
                let size = random_size(&stream);
                if let Err(error) = control.resize(size) {
                    violation(format!("resize failed: {error}"));
                }
                if control.size() != size {
                    violation(format!(
                        "size is {:?} after resizing to {size:?}",
                        control.size()
                    ));
                }
                // A captured process has no controlling terminal, so no window changes; and
                // after stop_output nothing more is delivered.
                if scenario.reports_resizes && !scenario.captured && !output_stopped {
                    // 6. The program reports the new size.
                    let wanted = if scenario.captured {
                        format!("{}x{}\n", size.cols, size.rows)
                    } else {
                        format!("{}x{}\r\n", size.cols, size.rows)
                    };
                    let reported = |observed: &Observed| {
                        let stream = if scenario.captured {
                            &observed.stdout
                        } else {
                            &observed.output
                        };
                        String::from_utf8_lossy(stream).ends_with(&wanted)
                    };
                    let deadline = switchy_time::instant_now() + Duration::from_secs(5);
                    while observed.exits.is_empty()
                        && !reported(&observed)
                        && switchy_time::instant_now() < deadline
                    {
                        observed.take(process.next_event_timeout(Duration::from_millis(100)));
                    }
                    if observed.exits.is_empty() && !reported(&observed) {
                        violation(format!(
                            "the program never reported the resize to {wanted:?}"
                        ));
                    }
                }
            }
            Action::Interrupt => {
                signal_self("interrupted");
                if let Err(error) = control.interrupt() {
                    violation(format!("interrupt failed: {error}"));
                }
            }
            Action::Kill => {
                signal_self("killed");
                killed = true;
                if let Err(error) = control.kill() {
                    violation(format!("kill failed: {error}"));
                }
            }
            Action::StopOutput => {
                output_stopped = true;
                control.stop_output();
            }
            Action::Drop => {
                // Dropping a running process kills its group.
                signal_self("dropped");
                drop(process);
                check_group_ended(pid, serial, &violation);
                lock(shared).summary ^= u64::from(pid) << 7;
                return;
            }
        }
    }
    if observed.exits.is_empty() {
        // Done experimenting: end it and collect everything left.
        signal_self("finally killed");
        killed = true;
        if let Err(error) = control.kill() {
            violation(format!("final kill failed: {error}"));
        }
    }
    while observed.exits.is_empty() {
        observed.take(process.next_event());
    }
    drop(writer);

    // 1. Exactly one exit, last, and nothing after it.
    if observed.exits.len() != 1 || observed.after_exit > 0 {
        violation(format!(
            "{} exits, {} events after the exit",
            observed.exits.len(),
            observed.after_exit
        ));
    }
    if process.next_event().is_some() || !process.has_exited() {
        violation("events continue after the exit".into());
    }
    let exit = observed.exits[0];

    // 3. The control handle already knows the exit.
    if control.exit_status() != Some(exit) {
        violation(format!(
            "control reports {:?}, the event {exit:?}",
            control.exit_status()
        ));
    }

    // 2. What was delivered is what was written.
    if let Some(io) = io {
        if !typed
            && let Some(written) = io.terminal.as_deref().and_then(simulator::terminal_written)
            && let Some(problem) = check_stream(
                "terminal output",
                &observed.output,
                &written,
                exit.output_complete(),
            )
        {
            violation(problem);
        }
        if let Some(written) = io.stdout_pipe.and_then(simulator::pipe_written)
            && let Some(problem) =
                check_stream("stdout", &observed.stdout, &written, exit.output_complete())
        {
            violation(problem);
        }
    }

    // 4. A killed process takes its whole group with it.
    if killed {
        check_group_ended(pid, serial, &violation);
    }

    let mut summary = lock(shared);
    summary.summary = summary
        .summary
        .rotate_left(13)
        .wrapping_add(observed.output.len() as u64)
        .wrapping_add((observed.stdout.len() as u64) << 20)
        .wrapping_add(u64::from(exit.code().unwrap_or(-1).cast_unsigned()) << 40);
}

/// 4. Shortly after a kill, nothing in the killed process's group is still running.
fn check_group_ended(pid: u32, serial: Option<u64>, violation: &impl Fn(String)) {
    let Some(serial) = serial else {
        return;
    };
    thread::sleep(Duration::from_secs(1));
    let survivors: Vec<String> = simulator::snapshot()
        .processes
        .into_iter()
        .filter(|p| {
            p.session_serial == serial && p.group == pid && p.state == ProcessState::Running
        })
        .map(|p| format!("{} ({}, received {:?})", p.pid, p.program, p.signals))
        .collect();
    if !survivors.is_empty() {
        let delivered: Vec<String> = simulator::signal_log()
            .into_iter()
            .filter(|r| r.session_serial == serial)
            .map(|r| format!("{:?}->{}", r.signal, r.pid))
            .collect();
        violation(format!(
            "still running a second after the kill: {}; signals to the session: [{}]",
            survivors.join(", "),
            delivered.join(", ")
        ));
    }
}
