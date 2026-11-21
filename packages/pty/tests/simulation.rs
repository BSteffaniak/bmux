//! Behaviour only the simulator can force: races the real system makes rare.
//!
//! Run with `cargo test -p bmux_pty --no-default-features --features simulator --test simulation`.
#![cfg(all(unix, feature = "simulator"))]

use std::time::Duration;

use bmux_pty::{PtyCommand, PtyEvent, PtySize};
use switchy_pty::{
    Signal,
    simulator::{self, Config, Faults, Personality, ProcessState},
};

/// A process's group must never be signalled after its id could have been given to another
/// process: `kill` and `interrupt` after the exit would otherwise reach whichever newer
/// process reused the id. Found by the bmux_pty simulation (a kill reaching an unrelated
/// session after the first process was reaped and its id reused).
#[test]
fn signalling_an_exited_process_never_reaches_a_newer_process_with_its_id() {
    for seed in 0..20 {
        let outcome = switchy_thread::simulator::run(seed, || {
            switchy_time::simulator::set_step_multiplier(1);
            simulator::configure(Config {
                personality: Personality::Linux,
                // Every new process reuses a free id when there is one.
                pid_reuse: 1.0,
                faults: Faults::default(),
            });
            simulator::install_default_programs();

            let mut first = PtyCommand::new("exit")
                .arg("0")
                .spawn(PtySize::new(80, 24))
                .unwrap();
            let control = first.control();
            let first_id = first.id();
            while !matches!(first.next_event(), Some(PtyEvent::Exited(_))) {}

            // Newer processes, started while the exited one's handle still exists.
            let others: Vec<_> = (0..10)
                .map(|_| {
                    PtyCommand::new("sleep")
                        .arg("60000")
                        .spawn(PtySize::new(80, 24))
                        .unwrap()
                })
                .collect();
            assert!(
                others.iter().all(|p| p.id() != first_id),
                "seed {seed}: an exited process's id was reused while its handle exists"
            );

            control.kill().unwrap();
            control.interrupt().unwrap();
            switchy_thread::thread::sleep(Duration::from_millis(100));
            let hit: Vec<u32> = simulator::signal_log()
                .into_iter()
                .filter(|r| matches!(r.signal, Signal::Kill | Signal::Interrupt))
                .map(|r| r.pid)
                .collect();
            assert!(hit.is_empty(), "seed {seed}: signals reached {hit:?}");
            let running = simulator::snapshot()
                .processes
                .iter()
                .filter(|p| p.state == ProcessState::Running)
                .count();
            assert_eq!(running, others.len(), "seed {seed}");

            drop((first, control));
            drop(others);
            simulator::kill_all();
            switchy_thread::simulator::run_until_idle();
        });
        if let Err(panic) = outcome.result {
            std::panic::resume_unwind(panic);
        }
        assert!(outcome.report.leaked.is_empty(), "{:?}", outcome.report);
    }
}
