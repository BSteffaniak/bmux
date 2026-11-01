use super::*;
use crate::host::Host;
use std::sync::mpsc;

fn fixture() -> (
    tempfile::TempDir,
    Host,
    String,
    String,
    std::sync::Arc<Entry>,
) {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::new(directory.path().to_owned()).unwrap();
    let key = uuid::Uuid::new_v4().to_string();
    let endpoint = host.prepare(&key, false, true, false).unwrap();
    let entry = host.entry(&key, &endpoint).unwrap();
    (directory, host, key, endpoint, entry)
}

fn identity() -> Identity {
    process::inspect(std::process::id()).unwrap().identity
}

#[test]
fn root_waits_for_bind_but_not_for_command_start() {
    let (_directory, host, key, endpoint, entry) = fixture();
    let (pending, observed) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let mut signal = Some(pending);
            wait_observed(
                &entry,
                identity(),
                Instant::now() + Duration::from_secs(2),
                || {
                    if let Some(signal) = signal.take() {
                        signal.send(()).unwrap();
                    }
                },
            )
        });
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        host.bind(&key, &endpoint, std::process::id()).unwrap();
        waiter.join().unwrap().unwrap();
    });
    assert!(entry.state.lock().unwrap().initial_prompt);
    assert!(entry.state.lock().unwrap().command_phase == CommandPhase::AwaitingBoundary);
}

#[test]
fn failed_startup_and_shutdown_wake_unbound_callbacks() {
    let (_directory, host, key, endpoint, entry) = fixture();
    let (pending, observed) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let mut signal = Some(pending);
            wait_observed(
                &entry,
                identity(),
                Instant::now() + Duration::from_secs(2),
                || {
                    if let Some(signal) = signal.take() {
                        signal.send(()).unwrap();
                    }
                },
            )
        });
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        host.close(&key, &endpoint).unwrap();
        assert_eq!(
            waiter.join().unwrap().unwrap_err().code,
            "execution_stopped"
        );
    });
}

#[test]
fn timeout_is_explicit_and_does_not_mutate_execution() {
    let (_directory, _host, _key, _endpoint, entry) = fixture();
    let invocation = entry.state.lock().unwrap().execution.committed.invocation;
    assert_eq!(
        wait(&entry, identity(), Instant::now()).unwrap_err().code,
        "launch_not_bound"
    );
    assert_eq!(
        entry.state.lock().unwrap().execution.committed.invocation,
        invocation
    );
}

#[test]
fn owner_exit_while_waiting_is_rejected_without_mutation() {
    let (_directory, host, key, endpoint, entry) = fixture();
    let mut owner = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    host.bind(&key, &endpoint, owner.id()).unwrap();
    let (pending, observed) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let mut signal = Some(pending);
            wait_observed(
                &entry,
                identity(),
                Instant::now() + Duration::from_secs(2),
                || {
                    if let Some(signal) = signal.take() {
                        signal.send(()).unwrap();
                    }
                },
            )
        });
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        owner.kill().unwrap();
        owner.wait().unwrap();
        entry.changed.notify_all();
        assert_eq!(
            waiter.join().unwrap().unwrap_err().code,
            "owner_identity_changed"
        );
    });
    assert!(entry.state.lock().unwrap().nested.is_empty());
}

#[test]
fn nested_waits_for_authoritative_start_and_rejects_dead_shell() {
    let (_directory, host, key, endpoint, entry) = fixture();
    host.bind(&key, &endpoint, std::process::id()).unwrap();
    host.boundary(&key, &endpoint, true).unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let shell = process::inspect(child.id()).unwrap().identity;
    assert_eq!(
        wait(&entry, shell, Instant::now()).unwrap_err().code,
        "command_boundary_not_ready"
    );
    let (pending, observed) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let mut signal = Some(pending);
            wait_observed(
                &entry,
                shell,
                Instant::now() + Duration::from_secs(2),
                || {
                    if let Some(signal) = signal.take() {
                        signal.send(()).unwrap();
                    }
                },
            )
        });
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        host.boundary(&key, &endpoint, false).unwrap();
        waiter.join().unwrap().unwrap();
    });
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        wait(&entry, shell, Instant::now()).unwrap_err().code,
        "shell_identity_changed"
    );
}
