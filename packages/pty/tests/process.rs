//! Behaviour of processes run on a pseudo-terminal.
#![cfg(unix)]

use std::time::{Duration, Instant};

use bmux_pty::{PtyCommand, PtyEvent, PtyExit, PtySize};

fn sh(script: &str) -> PtyCommand {
    PtyCommand::new("/bin/sh").args(["-c", script])
}

/// Terminal output, captured stdout and the exit, waiting at most ten seconds.
fn run(command: PtyCommand, size: PtySize) -> (Vec<u8>, Vec<u8>, PtyExit) {
    let mut process = command.spawn(size).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut output, mut stdout) = (Vec::new(), Vec::new());
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
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

#[test]
fn programs_see_a_terminal_of_the_given_size() {
    let (output, stdout, exit) = run(
        sh("test -t 0 && test -t 1 && test -t 2 && echo tty; stty size; echo \"$TERM\"; exit 3"),
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
}

#[test]
fn captured_stdout_stays_exact_while_stderr_is_a_terminal() {
    let (output, stdout, exit) = run(
        sh("printf 'a\\nb\\n'; test -t 1 || echo 'stdout is a pipe' >&2; test -t 2 && echo 'stderr is a terminal' >&2")
            .capture_stdout(),
        PtySize::new(80, 24),
    );
    assert_eq!(stdout, b"a\nb\n");
    let output = text(&output);
    assert!(output.contains("stdout is a pipe\r\n"), "{output:?}");
    assert!(output.contains("stderr is a terminal\r\n"), "{output:?}");
    assert!(exit.success());
}

#[test]
fn environment_and_directory_apply() {
    let dir = std::env::temp_dir().canonicalize().unwrap();
    let (output, _, _) = run(
        sh("pwd; echo \"$GREETING-$TERM\"")
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
}

#[test]
fn interrupt_and_kill_reach_everything_the_command_started() {
    for interrupt in [true, false] {
        let started = Instant::now();
        // The shell waits on a child; signalling only the shell would leave `sleep` running.
        let mut process = sh("sleep 30 & wait").spawn(PtySize::new(80, 24)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
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
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(process.has_exited());
        assert_eq!(process.next_event(), None);
    }
}

#[test]
fn resizing_notifies_the_process() {
    let mut process =
        sh("trap 'stty size; exit 0' WINCH; echo ready; while :; do sleep 0.05; done")
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
}

#[test]
fn output_is_complete_and_ordered_under_backpressure() {
    let mut process = sh("seq 1 20000")
        .capture_stdout()
        .spawn(PtySize::new(80, 24))
        .unwrap();
    // A slow consumer: the reader waits instead of dropping output.
    std::thread::sleep(Duration::from_millis(300));
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
}

#[test]
fn ready_events_is_bounded_and_does_not_wait() {
    let mut process = sh("sleep 5").spawn(PtySize::new(80, 24)).unwrap();
    let started = Instant::now();
    assert!(process.ready_events(10).is_empty());
    assert!(started.elapsed() < Duration::from_secs(1));
    // Dropping a running process kills it.
    let id = process.id();
    drop(process);
    std::thread::sleep(Duration::from_millis(200));
    let alive = std::process::Command::new("kill")
        .args(["-0", &id.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive);
}

#[test]
fn missing_programs_fail_to_spawn() {
    assert!(
        PtyCommand::new("/definitely/not/a/program")
            .spawn(PtySize::new(80, 24))
            .is_err()
    );
}

#[test]
fn input_is_fed_through_a_pipe_and_closed() {
    let (output, stdout, exit) = run(
        sh("test -t 0 || echo 'stdin is a pipe' >&2; cat")
            .input("from input\n")
            .capture_stdout(),
        PtySize::new(80, 24),
    );
    assert_eq!(stdout, b"from input\n");
    assert!(
        text(&output).contains("stdin is a pipe"),
        "{:?}",
        text(&output)
    );
    assert!(exit.success());
}

#[cfg(target_os = "linux")]
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[cfg(target_os = "linux")]
fn pid_in(output: &[u8]) -> String {
    text(output)
        .split("pid=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("background pid")
        .to_owned()
}

// Linux keeps a terminal open while anything inherited it; macOS ends it with the session.
#[cfg(target_os = "linux")]
#[test]
fn descendants_keeping_the_terminal_open_are_drained_within_the_grace() {
    // The background job ignores the hangup the shell's exit sends (inherited at the fork, so it
    // cannot lose a race with the exit), and keeps writing.
    let script = "trap '' HUP; (sleep 0.4; echo late) & echo early";
    let started = Instant::now();
    let (output, _, exit) = run(sh(script), PtySize::new(80, 24));
    assert!(!text(&output).contains("late"), "{:?}", text(&output));
    assert!(!exit.output_complete());
    assert!(started.elapsed() < Duration::from_secs(2));

    let (output, _, exit) = run(
        sh(script).drain_grace(Duration::from_secs(5)),
        PtySize::new(80, 24),
    );
    let output = text(&output);
    assert!(
        output.contains("early") && output.contains("late"),
        "{output:?}"
    );
    assert!(exit.output_complete());
}

#[cfg(target_os = "linux")]
#[test]
fn signals_reach_what_keeps_the_terminal_open_after_the_exit() {
    let mut process = sh("trap '' HUP; sleep 30 & echo \"pid=$!\"")
        .drain_grace(Duration::from_millis(50))
        .spawn(PtySize::new(80, 24))
        .unwrap();
    let mut output = Vec::new();
    for event in process.events() {
        if let PtyEvent::Output(bytes) = event {
            output.extend(bytes);
        }
    }
    let pid = pid_in(&output);
    assert!(alive(&pid));
    process.kill().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(&pid) {
        assert!(Instant::now() < deadline, "the background process survived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn stopping_output_reports_the_exit_without_waiting_for_the_rest() {
    let mut process = sh("echo first; sleep 0.3; echo second")
        .drain_grace(Duration::from_secs(30))
        .spawn(PtySize::new(80, 24))
        .unwrap();
    let mut output = Vec::new();
    while !text(&output).contains("first") {
        if let Some(PtyEvent::Output(bytes)) = process.next_event_timeout(Duration::from_secs(10)) {
            output.extend(bytes);
        }
    }
    process.stop_output();
    let started = Instant::now();
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
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn complete_output_is_reported_complete() {
    let (output, _, exit) = run(sh("echo all"), PtySize::new(80, 24));
    assert!(text(&output).contains("all"));
    assert!(exit.output_complete());
}

#[test]
fn the_writer_types_into_the_running_process() {
    let mut process = sh("echo ready; read line; echo \"got <$line>\"")
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
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
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
}

#[test]
fn a_control_handle_works_from_another_thread_than_the_reader() {
    let mut process =
        sh("trap 'stty size; exit 0' WINCH; echo ready; while :; do sleep 0.05; done")
            .spawn(PtySize::new(80, 24))
            .unwrap();
    let control = process.control();
    assert_eq!(control.size(), PtySize::new(80, 24));
    assert_eq!(control.exit_status(), None);
    let (ready, ready_seen) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
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
}

#[test]
fn programs_start_with_default_signal_handling_and_no_stray_descriptors() {
    use std::os::fd::AsRawFd as _;
    // An inheritable descriptor in the caller, and an ignored interrupt.
    let (reader, writer) = std::io::pipe().unwrap();
    // SAFETY: fcntl and signal on owned descriptors and a valid signal.
    unsafe {
        let flags = libc::fcntl(writer.as_raw_fd(), libc::F_GETFD);
        libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC);
    }
    let leaked = writer.as_raw_fd();
    let previous = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let mut process = sh(&format!(
        "if [ -e /dev/fd/{leaked} ]; then echo leaked; else echo closed; fi; echo ready; while :; do sleep 0.05; done"
    ))
    .spawn(PtySize::new(80, 24))
    .unwrap();
    unsafe { libc::signal(libc::SIGINT, previous) };
    let mut output = Vec::new();
    while !text(&output).contains("ready") {
        if let Some(PtyEvent::Output(bytes)) = process.next_event_timeout(Duration::from_secs(10)) {
            output.extend(bytes);
        }
    }
    assert!(text(&output).contains("closed"), "{:?}", text(&output));
    // Interrupting works although the caller ignores interrupts.
    process.interrupt().unwrap();
    let exit = process
        .events()
        .find_map(|event| match event {
            PtyEvent::Exited(exit) => Some(exit),
            _ => None,
        })
        .unwrap();
    assert_eq!(exit.signal(), Some(libc::SIGINT));
    drop((reader, writer));
}
