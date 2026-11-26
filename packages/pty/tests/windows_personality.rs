//! Processes run as Windows runs them, in simulation on any host: the simulator's Windows
//! personality, through the same code that runs them on Windows. `tests/windows.rs` checks the
//! same behaviour natively on Windows.
//!
//! `cargo test -p bmux_pty --no-default-features --features simulator --test windows_personality`
//! runs each scenario under several seeds: a console the process runs on (which cannot be
//! signalled, only stopped), a kill-on-close job holding what it starts, and pipes in place of
//! the console when its output is captured or its input fed.
#![cfg(feature = "simulator")]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::time::Duration;

use bmux_pty::{PtyCommand, PtyEvent, PtyExit, PtyProcess, PtySize};
use switchy_pty::simulator::{self, Config, Faults, Personality, ProcessState};

fn now() -> std::time::Instant {
    switchy_time::instant_now()
}

/// Output, captured stdout and the exit, waiting at most twenty seconds.
fn run_to_end(process: &mut PtyProcess) -> (Vec<u8>, Vec<u8>, PtyExit) {
    let deadline = now() + Duration::from_secs(20);
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

fn spawn(command: PtyCommand) -> PtyProcess {
    command.spawn(PtySize::new(80, 24)).unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Wait until `process` has printed `wanted`.
fn wait_for_output(process: &mut PtyProcess, wanted: &str) {
    let deadline = now() + Duration::from_secs(20);
    let mut output = Vec::new();
    while !text(&output).contains(wanted) {
        let left = deadline.saturating_duration_since(now());
        match process.next_event_timeout(left) {
            Some(PtyEvent::Output(bytes)) => output.extend(bytes),
            Some(event) => panic!("{event:?} before {wanted:?}: {:?}", text(&output)),
            None => panic!("no {wanted:?} in time: {:?}", text(&output)),
        }
    }
}

fn running_processes() -> usize {
    simulator::snapshot()
        .processes
        .iter()
        .filter(|p| p.state == ProcessState::Running)
        .count()
}

/// Under several seeds, as Windows.
fn scenario(test: impl Fn()) {
    for seed in 0..8 {
        let outcome = switchy_thread::simulator::run(seed, || {
            // Fine-grained time, so millisecond polls and grace periods mean what they say.
            switchy_time::simulator::set_step_multiplier(1);
            simulator::configure(Config {
                personality: Personality::Windows,
                pid_reuse: 0.5,
                faults: Faults::default(),
            });
            simulator::install_default_programs();
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

#[test]
fn output_and_the_exit_code_arrive_and_the_console_closes() {
    scenario(|| {
        // Nothing answers a cursor query: the console keeps its own cursor and asks none.
        let (output, _, exit) = run_to_end(&mut spawn(PtyCommand::new("exit").arg("3")));
        assert_eq!(text(&output), "bye\r\n");
        assert_eq!(exit.code(), Some(3));
        assert_eq!(exit.signal(), None);
        assert!(exit.output_complete());
    });
}

#[test]
fn a_console_can_be_resized_and_typed_into_but_not_signalled() {
    scenario(|| {
        let process = spawn(PtyCommand::new("shell").arg("1"));
        let capabilities = process.capabilities();
        assert!(capabilities.terminal && capabilities.resize && capabilities.input);
        assert!(!capabilities.signals);
        process.resize(PtySize::new(100, 40)).unwrap();
        process.kill().unwrap();
    });
}

#[test]
fn killing_ends_everything_the_command_started() {
    scenario(|| {
        let mut process = spawn(PtyCommand::new("shell").arg("2"));
        wait_for_output(&mut process, "started");
        assert_eq!(running_processes(), 3);
        process.kill().unwrap();
        let (_, _, exit) = run_to_end(&mut process);
        assert_eq!(exit.code(), Some(1));
        switchy_thread::simulator::run_until_idle();
        assert_eq!(running_processes(), 0, "the job's processes all ended");
    });
}

#[test]
fn interrupting_stops_the_command_and_what_it_started() {
    scenario(|| {
        let mut process = spawn(PtyCommand::new("shell").arg("2"));
        wait_for_output(&mut process, "started");
        process.interrupt().unwrap();
        let (_, _, exit) = run_to_end(&mut process);
        assert_eq!(exit.code(), Some(1));
        switchy_thread::simulator::run_until_idle();
        assert_eq!(running_processes(), 0);
    });
}

#[test]
fn dropping_the_process_ends_its_job() {
    scenario(|| {
        let mut process = spawn(PtyCommand::new("shell").arg("2"));
        wait_for_output(&mut process, "started");
        drop(process);
        switchy_thread::simulator::run_until_idle();
        assert_eq!(running_processes(), 0);
    });
}

#[test]
fn typed_input_reaches_the_program() {
    scenario(|| {
        let mut process = spawn(PtyCommand::new("cat"));
        process
            .writer()
            .unwrap()
            .write(b"typed\r".to_vec())
            .unwrap();
        wait_for_output(&mut process, "typed");
        process.kill().unwrap();
        run_to_end(&mut process);
    });
}

#[test]
fn captured_output_runs_on_pipes_without_the_console() {
    scenario(|| {
        let mut process = spawn(
            PtyCommand::new("out-err")
                .args(["out", "err"])
                .capture_stdout(),
        );
        let capabilities = process.capabilities();
        assert!(!capabilities.terminal && !capabilities.resize && !capabilities.input);
        let (output, stdout, exit) = run_to_end(&mut process);
        assert_eq!(text(&stdout), "out\n");
        assert_eq!(text(&output), "err\n");
        assert_eq!(exit.code(), Some(0));
    });
}

#[test]
fn fed_input_reaches_the_program_through_a_pipe() {
    scenario(|| {
        let (output, _, exit) =
            run_to_end(&mut spawn(PtyCommand::new("cat").input(b"fed\n".to_vec())));
        assert_eq!(text(&output), "fed\n");
        assert_eq!(exit.code(), Some(0));
    });
}
