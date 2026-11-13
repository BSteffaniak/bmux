//! Behaviour of processes run on a Windows pseudo-console.
#![cfg(windows)]

use std::time::{Duration, Instant};

use bmux_pty::{PtyCapabilities, PtyCommand, PtyEvent, PtyExit, PtySize};

fn powershell(script: &str) -> PtyCommand {
    PtyCommand::new("powershell.exe").args(["-NoLogo", "-NoProfile", "-Command", script])
}

/// Terminal output, captured stdout and the exit, waiting at most thirty seconds.
fn run(command: PtyCommand, size: PtySize) -> (String, Vec<u8>, PtyExit) {
    let mut process = command.spawn(size).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut output, mut stdout) = (Vec::new(), Vec::new());
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match process
            .next_event_timeout(left)
            .expect("process exits in time")
        {
            PtyEvent::Output(bytes) => output.extend(bytes),
            PtyEvent::Stdout(bytes) => stdout.extend(bytes),
            PtyEvent::Exited(exit) => {
                return (String::from_utf8_lossy(&output).into_owned(), stdout, exit);
            }
        }
    }
}

#[test]
fn programs_see_a_console_of_the_given_size() {
    let (output, stdout, exit) = run(
        powershell(
            "Write-Output (\"redirected=\" + [Console]::IsOutputRedirected); \
             Write-Output (\"width=\" + [Console]::WindowWidth); exit 3",
        ),
        PtySize::new(100, 40),
    );
    assert!(stdout.is_empty());
    assert!(output.contains("redirected=False"), "{output:?}");
    assert!(output.contains("width=100"), "{output:?}");
    assert_eq!(exit.code(), Some(3));
}

#[test]
fn console_processes_report_full_capabilities() {
    let process = PtyCommand::new("cmd.exe")
        .args(["/c", "exit"])
        .spawn(PtySize::new(80, 24))
        .unwrap();
    assert_eq!(
        process.capabilities(),
        PtyCapabilities {
            terminal: true,
            resize: true,
            input: true,
            // Interrupting stops the process tree, as killing does.
            signals: false,
        }
    );
}

#[test]
fn the_writer_types_into_the_running_process() {
    let mut process = powershell(
        "Write-Output ready; $line = Read-Host; Write-Output (\"got <\" + $line + \">\")",
    )
    .spawn(PtySize::new(80, 24))
    .unwrap();
    let writer = process.writer().unwrap();
    let mut output = Vec::new();
    let mut sent = false;
    let deadline = Instant::now() + Duration::from_secs(30);
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
        if !sent && String::from_utf8_lossy(&output).contains("ready") {
            writer.write("hello\r").unwrap();
            sent = true;
        }
    }
    let output = String::from_utf8_lossy(&output);
    assert!(output.contains("got <hello>"), "{output:?}");
}

#[test]
fn kill_reaches_everything_the_command_started() {
    let started = Instant::now();
    // `cmd` waits on a child; killing only `cmd` would leave `ping` running.
    let mut process = PtyCommand::new("cmd.exe")
        .args(["/c", "ping -n 60 127.0.0.1 > nul"])
        .spawn(PtySize::new(80, 24))
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    process.kill().unwrap();
    let exit = process
        .events()
        .find_map(|event| match event {
            PtyEvent::Exited(exit) => Some(exit),
            _ => None,
        })
        .unwrap();
    assert!(!exit.success());
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn interrupt_stops_a_console_program_and_what_it_started() {
    let started = Instant::now();
    let mut process = PtyCommand::new("ping.exe")
        .args(["-n", "60", "127.0.0.1"])
        .spawn(PtySize::new(80, 24))
        .unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    process.interrupt().unwrap();
    let exit = process
        .events()
        .find_map(|event| match event {
            PtyEvent::Exited(exit) => Some(exit),
            _ => None,
        })
        .unwrap();
    assert!(!exit.success());
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn resizing_reaches_the_console() {
    // Resizing through a control handle while events are read elsewhere.
    let mut process = powershell(
        "Write-Output ready; Start-Sleep -Milliseconds 1500; \
         Write-Output (\"width=\" + [Console]::WindowWidth)",
    )
    .spawn(PtySize::new(80, 24))
    .unwrap();
    let control = process.control();
    let mut output = Vec::new();
    let mut resized = false;
    for event in process.events() {
        if let PtyEvent::Output(bytes) = event {
            output.extend(bytes);
        }
        if !resized && String::from_utf8_lossy(&output).contains("ready") {
            control.resize(PtySize::new(120, 30)).unwrap();
            resized = true;
        }
    }
    let output = String::from_utf8_lossy(&output);
    assert!(output.contains("width=120"), "{output:?}");
}

#[test]
fn captured_stdout_still_runs_on_pipes() {
    let (_, stdout, exit) = run(
        PtyCommand::new("cmd.exe")
            .args(["/c", "echo a"])
            .capture_stdout(),
        PtySize::new(80, 24),
    );
    assert_eq!(stdout, b"a\r\n");
    assert!(exit.success());
}
