//! Scenarios every target of the platform layer must pass, written once against its interface.
//!
//! Natively, the plugin's tests run them on the host operating system; in simulation,
//! `simulation/pane-runtime`'s tests run them under every personality of the simulated kernel.
//! Where operating systems differ, the expectation follows [`bmux_pty::behavior`], so passing
//! natively on a system and in simulation under its personality shows that the simulated
//! target behaves as that system's native one.

use std::io::{Read as _, Write as _};
use std::time::Duration;

use super::session::{Session, prepare};

/// How long anything may take (natively; simulated time is shorter).
const TIMEOUT: Duration = Duration::from_secs(20);

/// A scenario: its name, and what it checks.
pub(crate) type Scenario = (&'static str, fn() -> Result<(), String>);

/// Every scenario.
pub(crate) const SCENARIOS: &[Scenario] = &[
    (
        "a sweep ends the leader and its descendants",
        sweep_ends_the_leader_and_its_descendants,
    ),
    (
        "released survivors outlive the release",
        released_survivors_outlive_the_release,
    ),
    (
        "releasing ends the processes only where a job holds them",
        releasing_ends_the_processes_only_where_a_job_holds_them,
    ),
    (
        "cooperative stop only where processes have signals",
        cooperative_stop_only_where_processes_have_signals,
    ),
    (
        "process identities only where processes have them",
        process_identities_only_where_processes_have_them,
    ),
];

/// A program that starts a descendant and runs until stopped.
fn with_descendant() -> bmux_pty::SpawnCommand {
    let mut command;
    if cfg!(feature = "simulator") {
        command = bmux_pty::SpawnCommand::new("shell");
        command.arg("1");
    } else if bmux_pty::behavior().process_groups {
        command = bmux_pty::SpawnCommand::new("/bin/sh");
        command.args(["-c", "sleep 30 & wait"]);
    } else {
        // cmd starts ping as a descendant.
        command = bmux_pty::SpawnCommand::new(
            switchy_env::var_os("COMSPEC").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into()),
        );
        command.args(["/D", "/C", "ping -n 60 127.0.0.1 >nul"]);
    }
    command
}

/// A terminal whose output is drained, answering the console's cursor query where it asks.
fn terminal() -> Result<bmux_pty::PtyPair, String> {
    let pair = bmux_pty::PtyPair::open(bmux_pty::PtySize::new(80, 24)).map_err(text)?;
    let mut reader = pair.master.try_clone_reader().map_err(text)?;
    let mut input = pair.master.take_writer().map_err(text)?;
    let query = bmux_pty::behavior().startup_cursor_query;
    switchy_thread::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                return;
            }
            if query && buffer[..read].windows(4).any(|window| window == b"\x1b[6n") {
                let _ = input.write_all(b"\x1b[1;1R");
                let _ = input.flush();
            }
        }
    });
    Ok(pair)
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// A pane's program, in a session of its own.
struct Started {
    pair: bmux_pty::PtyPair,
    child: Box<dyn crate::runtime::pty::Child + Send + Sync>,
    leader: u32,
    session: Session,
}

fn start() -> Result<Started, String> {
    let pair = terminal()?;
    let preparation = prepare().map_err(text)?;
    let child = preparation
        .spawn(&pair.slave, &with_descendant())
        .map_err(text)?;
    let leader = child.process_id().ok_or("the program has no process id")?;
    let session = Session::adopt(preparation, leader, &pair.master).map_err(text)?;
    Ok(Started {
        pair,
        child,
        leader,
        session,
    })
}

/// Wait until `done` holds, at most [`TIMEOUT`]; whether it did.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let deadline = switchy_time::instant_now() + TIMEOUT;
    while !done() {
        if switchy_time::instant_now() >= deadline {
            return false;
        }
        switchy_thread::thread::sleep(Duration::from_millis(10));
    }
    true
}

fn running(child: &mut Box<dyn crate::runtime::pty::Child + Send + Sync>) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// End what a scenario left running.
fn finish(mut started: Started) {
    let _ = started.session.sweep();
    let _ = crate::runtime::pty::ChildKiller::kill(started.child.as_mut());
    let _ = started.child.wait();
    started.session.release();
    drop(started.pair);
}

fn sweep_ends_the_leader_and_its_descendants() -> Result<(), String> {
    let mut started = start()?;
    if !eventually(|| started.session.occupied().unwrap_or(false)) {
        return Err("the session never had members".into());
    }
    started.session.sweep().map_err(text)?;
    if started.session.occupied().map_err(text)? {
        return Err("members remain after the sweep".into());
    }
    if !eventually(|| !running(&mut started.child)) {
        return Err("the leader survived the sweep".into());
    }
    finish(started);
    Ok(())
}

fn released_survivors_outlive_the_release() -> Result<(), String> {
    let mut started = start()?;
    started.session.release_survivors();
    started.session.release();
    switchy_thread::thread::sleep(Duration::from_millis(200));
    let survived = running(&mut started.child);
    finish(started);
    if survived {
        Ok(())
    } else {
        Err("released survivors ended with the release".into())
    }
}

fn releasing_ends_the_processes_only_where_a_job_holds_them() -> Result<(), String> {
    let mut started = start()?;
    started.session.release();
    let job = !bmux_pty::behavior().process_groups;
    let ended = if job {
        eventually(|| !running(&mut started.child))
    } else {
        switchy_thread::thread::sleep(Duration::from_millis(200));
        !running(&mut started.child)
    };
    finish(started);
    match (job, ended) {
        (true, false) => Err("closing the job left its processes running".into()),
        (false, true) => Err("releasing a session ended its processes".into()),
        _ => Ok(()),
    }
}

fn cooperative_stop_only_where_processes_have_signals() -> Result<(), String> {
    let mut started = start()?;
    let signalled = started
        .session
        .terminate_leader(started.leader)
        .map_err(text)?;
    let expected = bmux_pty::behavior().process_groups;
    if signalled != expected || super::supports_cooperative_stop() != expected {
        finish(started);
        return Err(format!(
            "cooperative stop: signalled {signalled}, supported {}, expected {expected}",
            super::supports_cooperative_stop()
        ));
    }
    if signalled && !eventually(|| !running(&mut started.child)) {
        finish(started);
        return Err("the leader ignored a cooperative stop".into());
    }
    finish(started);
    Ok(())
}

fn process_identities_only_where_processes_have_them() -> Result<(), String> {
    let started = start()?;
    let expected = bmux_pty::behavior().process_groups;
    let identity = super::process_start_identity(started.leader).is_some();
    let directory = super::process_directory(started.leader).is_some();
    finish(started);
    if identity == expected && directory == expected {
        Ok(())
    } else {
        Err(format!(
            "identity {identity}, directory {directory}, expected {expected}"
        ))
    }
}

#[cfg(all(test, not(feature = "simulator")))]
mod tests {
    #[test]
    fn the_native_target_passes_every_scenario() {
        let failures: Vec<String> = super::SCENARIOS
            .iter()
            .filter_map(|(name, scenario)| scenario().err().map(|error| format!("{name}: {error}")))
            .collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
