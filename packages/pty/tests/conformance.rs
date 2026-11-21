//! Behaviour of processes run on a pseudo-terminal, checked the same way against the real
//! operating system and against the simulator.
//!
//! Each scenario names a shell script for native runs and a simulated program that behaves the
//! same way; the assertions are shared. Natively, `cargo test -p bmux_pty` runs them once
//! against real processes. With the simulator,
//! `cargo test -p bmux_pty --no-default-features --features simulator --test conformance`
//! runs each scenario under several seeds against the simulated kernel, failing on any leaked
//! thread. A difference between the two is a simulator bug, or a platform difference both
//! sides spell out.
#![cfg(unix)]

use std::time::Duration;

use bmux_pty::{PtyCommand, PtyEvent, PtyExit, PtySize};
use switchy_thread::{sync::mpsc, thread};

/// A program to run: a shell script natively, a simulated program in simulation.
fn program(script: &str, simulated: &str, args: &[&str]) -> PtyCommand {
    if cfg!(feature = "simulator") {
        PtyCommand::new(simulated).args(args)
    } else {
        PtyCommand::new("/bin/sh").args(["-c", script])
    }
}

fn now() -> std::time::Instant {
    switchy_time::instant_now()
}

/// Terminal output, captured stdout and the exit, waiting at most ten seconds.
fn run(command: PtyCommand, size: PtySize) -> (Vec<u8>, Vec<u8>, PtyExit) {
    let mut process = command.spawn(size).unwrap();
    let deadline = now() + Duration::from_secs(10);
    let (mut output, mut stdout) = (Vec::new(), Vec::new());
    loop {
        let left = deadline.saturating_duration_since(now());
        match process
            .next_event_timeout(left)
            .expect("process exits in time")
        {
            PtyEvent::Output(bytes) => output.extend(bytes),
            PtyEvent::Stdout(bytes) => stdout.extend(bytes),
            PtyEvent::Exited(exit) => return (output, stdout, exit),
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(not(feature = "simulator"))]
mod world {
    /// Run a scenario against the real operating system.
    pub fn scenario(test: impl Fn()) {
        test();
    }

    /// Run a scenario that depends on Linux terminal lifetimes (Linux hosts only).
    #[cfg(target_os = "linux")]
    pub fn linux_scenario(test: impl Fn()) {
        test();
    }

    /// Whether process `pid` is alive.
    pub fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    pub fn working_directory() -> std::path::PathBuf {
        std::env::temp_dir().canonicalize().unwrap()
    }
}

#[cfg(feature = "simulator")]
mod world {
    use std::time::Duration;

    use switchy_pty::simulator::{
        self, Config, Disposition, Faults, Personality, Process, ProcessState,
    };
    use switchy_pty::{PtySize, Signal};

    const SEEDS: u64 = 8;

    /// Simulated programs that do what each scenario's shell script does.
    fn install() {
        simulator::install_default_programs();
        simulator::register_program("exit-3", |p: &mut Process| {
            let _ = p.write(b"tty\n");
            let size = p.terminal_size().unwrap_or(PtySize::new(0, 0));
            let term = p.env("TERM").unwrap_or("").to_owned();
            let _ = p.write(format!("{} {}\n{term}\n", size.rows, size.cols).as_bytes());
            3
        });
        simulator::register_program("streams", |p: &mut Process| {
            let _ = p.write(b"a\nb\n");
            let _ = p.write_fd(2, b"stdout is a pipe\n");
            let _ = p.write_fd(2, b"stderr is a terminal\n");
            0
        });
        simulator::register_program("env-report", |p: &mut Process| {
            let cwd = p.cwd().display().to_string();
            let greeting = p.env("GREETING").unwrap_or("").to_owned();
            let term = p.env("TERM").unwrap_or("").to_owned();
            let _ = p.write(format!("{cwd}\n{greeting}-{term}\n").as_bytes());
            0
        });
        simulator::register_program("winch", |p: &mut Process| {
            p.handle(Signal::WindowChange, Disposition::Catch);
            let _ = p.write(b"ready\n");
            loop {
                if p.wait_signal(Duration::from_secs(3600)) == Some(Signal::WindowChange) {
                    let size = p.terminal_size().unwrap_or(PtySize::new(0, 0));
                    let _ = p.write(format!("{} {}\n", size.rows, size.cols).as_bytes());
                    return 0;
                }
            }
        });
        simulator::register_program("seq", |p: &mut Process| {
            let last: u32 = p.args().get(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            let lines: String = (1..=last).map(|n| format!("{n}\n")).collect();
            i32::from(p.write(lines.as_bytes()).is_err())
        });
        simulator::register_program("early-late", |p: &mut Process| {
            // As `trap '' HUP; (sleep 0.4; echo late) & echo early`.
            p.handle(Signal::Hangup, Disposition::Ignore);
            let _ = p.spawn(&["late-writer"], false);
            let _ = p.write(b"early\n");
            0
        });
        simulator::register_program("late-writer", |p: &mut Process| {
            p.sleep(Duration::from_millis(400));
            let _ = p.write(b"late\n");
            0
        });
        simulator::register_program("background-pid", |p: &mut Process| {
            // As `trap '' HUP; sleep 30 & echo "pid=$!"`.
            p.handle(Signal::Hangup, Disposition::Ignore);
            let Ok(child) = p.spawn(&["sleep", "30000"], false) else {
                return 1;
            };
            let _ = p.write(format!("pid={child}\n").as_bytes());
            0
        });
        simulator::register_program("first-second", |p: &mut Process| {
            let _ = p.write(b"first\n");
            p.sleep(Duration::from_millis(300));
            let _ = p.write(b"second\n");
            0
        });
        simulator::register_program("read-line", |p: &mut Process| {
            let _ = p.write(b"ready\n");
            let mut line = Vec::new();
            let mut byte = [0_u8; 1];
            while p.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            let _ = p.write(format!("got <{}>\n", String::from_utf8_lossy(&line)).as_bytes());
            0
        });
        simulator::register_program("background-sleeper", |p: &mut Process| {
            // As `sleep 30 & wait`: a child in the shell's group, and a shell waiting for it.
            let _ = p.spawn(&["sleep", "30000"], false);
            loop {
                p.sleep(Duration::from_secs(60));
            }
        });
    }

    /// Run a scenario under several seeds against the simulated kernel, modelling the host's
    /// operating system.
    pub fn scenario(test: impl Fn()) {
        let personality = if cfg!(target_os = "linux") {
            Personality::Linux
        } else {
            Personality::MacOs
        };
        scenario_as(personality, test);
    }

    /// Run a scenario that depends on Linux terminal lifetimes, modelling Linux on any host.
    pub fn linux_scenario(test: impl Fn()) {
        scenario_as(Personality::Linux, test);
    }

    fn scenario_as(personality: Personality, test: impl Fn()) {
        for seed in 0..SEEDS {
            let outcome = switchy_thread::simulator::run(seed, || {
                // Fine-grained time, so millisecond polls and grace periods mean what they say.
                switchy_time::simulator::set_step_multiplier(1);
                simulator::configure(Config {
                    personality,
                    pid_reuse: 0.5,
                    faults: Faults::default(),
                });
                install();
                test();
                simulator::kill_all();
                switchy_thread::simulator::run_until_idle();
            });
            if let Err(panic) = outcome.result {
                eprintln!("seed {seed} failed");
                std::panic::resume_unwind(panic);
            }
            assert!(
                outcome.report.leaked.is_empty(),
                "seed {seed}: simulated threads never finished: {:?}",
                outcome.report.leaked
            );
        }
    }

    /// Whether process `pid` is alive.
    pub fn alive(pid: u32) -> bool {
        simulator::snapshot()
            .processes
            .iter()
            .any(|p| p.pid == pid && p.state == ProcessState::Running)
    }

    pub fn working_directory() -> std::path::PathBuf {
        "/sim/work".into()
    }
}

use world::{alive, scenario};

#[test]
fn programs_see_a_terminal_of_the_given_size() {
    scenario(|| {
        let (output, stdout, exit) = run(
            program(
                "test -t 0 && test -t 1 && test -t 2 && echo tty; stty size; echo \"$TERM\"; exit 3",
                "exit-3",
                &[],
            ),
            PtySize::new(100, 40),
        );
        assert!(stdout.is_empty());
        let output = text(&output);
        // The terminal translates newlines, as a real one does.
        assert!(
            output.contains("tty\r\n40 100\r\nxterm-256color\r\n"),
            "{output:?}"
        );
        assert_eq!(exit.code(), Some(3));
        assert!(!exit.success());
    });
}

#[test]
fn captured_stdout_stays_exact_while_stderr_is_a_terminal() {
    scenario(|| {
        let (output, stdout, exit) = run(
            program(
                "printf 'a\\nb\\n'; test -t 1 || echo 'stdout is a pipe' >&2; test -t 2 && echo 'stderr is a terminal' >&2",
                "streams",
                &[],
            )
            .capture_stdout(),
            PtySize::new(80, 24),
        );
        assert_eq!(stdout, b"a\nb\n");
        let output = text(&output);
        assert!(output.contains("stdout is a pipe\r\n"), "{output:?}");
        assert!(output.contains("stderr is a terminal\r\n"), "{output:?}");
        assert!(exit.success());
    });
}

#[test]
fn environment_and_directory_apply() {
    scenario(|| {
        let dir = world::working_directory();
        let (output, _, _) = run(
            program("pwd; echo \"$GREETING-$TERM\"", "env-report", &[])
                .current_dir(&dir)
                .env("GREETING", "hi")
                .env("TERM", "dumb"),
            PtySize::new(200, 24),
        );
        let output = text(&output);
        assert!(
            output.contains(&format!("{}\r\n", dir.display())),
            "{output:?}"
        );
        assert!(output.contains("hi-dumb"), "{output:?}");
    });
}

#[test]
fn interrupt_and_kill_reach_everything_the_command_started() {
    scenario(|| {
        for interrupt in [true, false] {
            let started = now();
            // The shell waits on a child; signalling only the shell would leave `sleep` running.
            let mut process = program("sleep 30 & wait", "background-sleeper", &[])
                .spawn(PtySize::new(80, 24))
                .unwrap();
            thread::sleep(Duration::from_millis(200));
            if interrupt {
                process.interrupt().unwrap();
            } else {
                process.kill().unwrap();
            }
            let exit = process
                .events()
                .find_map(|event| match event {
                    PtyEvent::Exited(exit) => Some(exit),
                    _ => None,
                })
                .unwrap();
            assert!(exit.signal().is_some() || exit.code().is_some_and(|code| code != 0));
            assert!(now() - started < Duration::from_secs(10));
            assert!(process.has_exited());
            assert_eq!(process.next_event(), None);
        }
    });
}

#[test]
fn resizing_notifies_the_process() {
    scenario(|| {
        let mut process = program(
            "trap 'stty size; exit 0' WINCH; echo ready; while :; do sleep 0.05; done",
            "winch",
            &[],
        )
        .spawn(PtySize::new(80, 24))
        .unwrap();
        let mut output = Vec::new();
        while !text(&output).contains("ready") {
            match process.next_event_timeout(Duration::from_secs(10)).unwrap() {
                PtyEvent::Output(bytes) => output.extend(bytes),
                event => panic!("{event:?}"),
            }
        }
        process.resize(PtySize::new(120, 50)).unwrap();
        for event in process.events() {
            if let PtyEvent::Output(bytes) = event {
                output.extend(bytes);
            }
        }
        assert!(text(&output).contains("50 120"), "{:?}", text(&output));
    });
}

#[test]
fn output_is_complete_and_ordered_under_backpressure() {
    scenario(|| {
        let mut process = program("seq 1 20000", "seq", &["20000"])
            .capture_stdout()
            .spawn(PtySize::new(80, 24))
            .unwrap();
        // A slow consumer: the reader waits instead of dropping output.
        thread::sleep(Duration::from_millis(300));
        let mut stdout = Vec::new();
        let mut exited = false;
        for event in process.events() {
            match event {
                PtyEvent::Stdout(bytes) => stdout.extend(bytes),
                PtyEvent::Exited(exit) => {
                    assert!(exit.success());
                    exited = true;
                }
                PtyEvent::Output(_) => {}
            }
        }
        assert!(exited);
        let expected: String = (1..=20000).map(|n| format!("{n}\n")).collect();
        assert_eq!(text(&stdout), expected);
    });
}

#[test]
fn ready_events_is_bounded_and_does_not_wait() {
    scenario(|| {
        let mut process = program("sleep 5", "sleep", &["5000"])
            .spawn(PtySize::new(80, 24))
            .unwrap();
        let started = now();
        assert!(process.ready_events(10).is_empty());
        assert!(now() - started < Duration::from_secs(1));
        // Dropping a running process kills it.
        let id = process.id();
        drop(process);
        thread::sleep(Duration::from_millis(200));
        assert!(!alive(id));
    });
}

#[test]
fn missing_programs_fail_to_spawn() {
    scenario(|| {
        assert!(
            PtyCommand::new("/definitely/not/a/program")
                .spawn(PtySize::new(80, 24))
                .is_err()
        );
    });
}

#[test]
fn input_is_fed_through_a_pipe_and_closed() {
    scenario(|| {
        let (_, stdout, exit) = run(
            program("cat", "cat", &[])
                .input("from input\n")
                .capture_stdout(),
            PtySize::new(80, 24),
        );
        assert_eq!(stdout, b"from input\n");
        assert!(exit.success());
    });
}

// Linux keeps a terminal open while anything inherited it; macOS ends it with the session. The
// simulator models Linux on any host.
#[cfg(any(target_os = "linux", feature = "simulator"))]
#[test]
fn descendants_keeping_the_terminal_open_are_drained_within_the_grace() {
    world::linux_scenario(|| {
        // The background job ignores the hangup the shell's exit sends (inherited at the fork,
        // so it cannot lose a race with the exit), and keeps writing.
        let script = "trap '' HUP; (sleep 0.4; echo late) & echo early";
        let started = now();
        let (output, _, exit) = run(program(script, "early-late", &[]), PtySize::new(80, 24));
        assert!(!text(&output).contains("late"), "{:?}", text(&output));
        assert!(!exit.output_complete());
        assert!(now() - started < Duration::from_secs(2));
        let (output, _, exit) = run(
            program(script, "early-late", &[]).drain_grace(Duration::from_secs(5)),
            PtySize::new(80, 24),
        );
        let output = text(&output);
        assert!(
            output.contains("early") && output.contains("late"),
            "{output:?}"
        );
        assert!(exit.output_complete());
    });
}

#[cfg(any(target_os = "linux", feature = "simulator"))]
#[test]
fn signals_reach_what_keeps_the_terminal_open_after_the_exit() {
    world::linux_scenario(|| {
        let mut process = program(
            "trap '' HUP; sleep 30 & echo \"pid=$!\"",
            "background-pid",
            &[],
        )
        .drain_grace(Duration::from_millis(50))
        .spawn(PtySize::new(80, 24))
        .unwrap();
        let mut output = Vec::new();
        for event in process.events() {
            if let PtyEvent::Output(bytes) = event {
                output.extend(bytes);
            }
        }
        let pid: u32 = text(&output)
            .split("pid=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("background pid")
            .parse()
            .unwrap();
        assert!(alive(pid));
        process.kill().unwrap();
        let deadline = now() + Duration::from_secs(5);
        while alive(pid) {
            assert!(now() < deadline, "the background process survived");
            thread::sleep(Duration::from_millis(20));
        }
    });
}

#[test]
fn stopping_output_reports_the_exit_without_waiting_for_the_rest() {
    scenario(|| {
        let mut process = program("echo first; sleep 0.3; echo second", "first-second", &[])
            .drain_grace(Duration::from_secs(30))
            .spawn(PtySize::new(80, 24))
            .unwrap();
        let mut output = Vec::new();
        while !text(&output).contains("first") {
            if let Some(PtyEvent::Output(bytes)) =
                process.next_event_timeout(Duration::from_secs(10))
            {
                output.extend(bytes);
            }
        }
        process.stop_output();
        let started = now();
        let mut exit = None;
        for event in process.events() {
            match event {
                PtyEvent::Output(bytes) => output.extend(bytes),
                PtyEvent::Exited(status) => exit = Some(status),
                PtyEvent::Stdout(_) => {}
            }
        }
        let exit = exit.expect("an exit");
        assert!(!text(&output).contains("second"), "{:?}", text(&output));
        assert!(!exit.output_complete());
        assert!(exit.success());
        assert!(now() - started < Duration::from_secs(5));
    });
}

#[test]
fn complete_output_is_reported_complete() {
    scenario(|| {
        let (output, _, exit) = run(program("echo all", "echo", &["all"]), PtySize::new(80, 24));
        assert!(text(&output).contains("all"));
        assert!(exit.output_complete());
    });
}

#[test]
fn the_writer_types_into_the_running_process() {
    scenario(|| {
        let mut process = program(
            "echo ready; read line; echo \"got <$line>\"",
            "read-line",
            &[],
        )
        .spawn(PtySize::new(80, 24))
        .unwrap();
        assert_eq!(
            process.capabilities(),
            bmux_pty::PtyCapabilities {
                terminal: true,
                resize: true,
                input: true,
                signals: true,
            }
        );
        let writer = process.writer().unwrap();
        let mut output = Vec::new();
        let mut sent = false;
        let deadline = now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(now());
            match process
                .next_event_timeout(left)
                .expect("process exits in time")
            {
                PtyEvent::Output(bytes) => output.extend(bytes),
                PtyEvent::Stdout(_) => {}
                PtyEvent::Exited(exit) => {
                    assert!(exit.success());
                    break;
                }
            }
            if !sent && text(&output).contains("ready") {
                // Typed as at a keyboard: the terminal turns the carriage return into a newline.
                writer.write("hello\r").unwrap();
                sent = true;
            }
        }
        assert!(text(&output).contains("got <hello>"), "{:?}", text(&output));
    });
}

#[test]
fn a_control_handle_works_from_another_thread_than_the_reader() {
    scenario(|| {
        let mut process = program(
            "trap 'stty size; exit 0' WINCH; echo ready; while :; do sleep 0.05; done",
            "winch",
            &[],
        )
        .spawn(PtySize::new(80, 24))
        .unwrap();
        let control = process.control();
        assert_eq!(control.size(), PtySize::new(80, 24));
        assert_eq!(control.exit_status(), None);
        let (ready, ready_seen) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut output = Vec::new();
            let mut exit = None;
            for event in process.events() {
                match event {
                    PtyEvent::Output(bytes) => {
                        output.extend(bytes);
                        if text(&output).contains("ready") {
                            let _ = ready.send(());
                        }
                    }
                    PtyEvent::Exited(status) => exit = Some(status),
                    PtyEvent::Stdout(_) => {}
                }
            }
            (output, exit)
        });
        ready_seen.recv_timeout(Duration::from_secs(10)).unwrap();
        control.resize(PtySize::new(120, 50)).unwrap();
        assert_eq!(control.size(), PtySize::new(120, 50));
        let (output, exit) = reader.join().unwrap();
        assert!(text(&output).contains("50 120"), "{:?}", text(&output));
        let exit = exit.expect("an exit");
        assert!(exit.success() && exit.output_complete());
        assert_eq!(control.exit_status(), Some(exit));
    });
}

#[test]
fn typed_lines_are_edited_before_programs_read_them() {
    scenario(|| {
        let mut process = program("exec cat", "cat", &[])
            .spawn(PtySize::new(80, 24))
            .unwrap();
        let writer = process.writer().unwrap();
        // Erase, kill and word erase edit the line being typed; end of file sends a partial
        // line, then ends the input.
        writer
            .write(&b"abc\x7f\x7fd\rxyz\x15hi\rone two\x17three\rpart\x04\x04"[..])
            .unwrap();
        let deadline = now() + Duration::from_secs(10);
        let mut output = Vec::new();
        let exit = loop {
            let left = deadline.saturating_duration_since(now());
            match process
                .next_event_timeout(left)
                .expect("cat exits at end of input")
            {
                PtyEvent::Output(bytes) => output.extend(bytes),
                PtyEvent::Stdout(_) => {}
                PtyEvent::Exited(exit) => break exit,
            }
        };
        assert_eq!(exit.code(), Some(0));
        let output = text(&output);
        // The echo erases characters; cat copies only the edited lines, a line at a time.
        assert!(output.contains("\x08 \x08"), "{output:?}");
        assert!(output.contains("ad\r\n"), "{output:?}");
        assert_eq!(output.matches("hi\r\n").count(), 2, "{output:?}");
        assert!(output.contains("one three\r\n"), "{output:?}");
        assert_eq!(output.matches("part").count(), 2, "{output:?}");
        assert!(!output.contains("part\r\n"), "{output:?}");
    });
}
