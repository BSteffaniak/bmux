#[cfg(unix)]
#[test]
fn long_runtime_parent_does_not_lengthen_control_endpoint() {
    let parent = tempfile::tempdir().unwrap();
    let long = parent.path().join("runtime-".repeat(32));
    assert!(long.as_os_str().len() > 104);
    let endpoint = super::short_endpoint_directory().unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(endpoint.path().join("control.sock")).unwrap();
    assert!(
        listener
            .local_addr()
            .unwrap()
            .as_pathname()
            .unwrap()
            .as_os_str()
            .len()
            < 104
    );
}

#[cfg(unix)]
fn snapshot(revision: u64) -> crate::supervisor_protocol::LifecycleSnapshot {
    use crate::supervisor_protocol::{self as protocol, Response};
    protocol::LifecycleSnapshot {
        capability: protocol::LIFECYCLE_CAPABILITY,
        revision,
        status: Response {
            version: protocol::VERSION,
            execution_token: "test".into(),
            started: true,
            target_pid: Some(12),
            exit_code: None,
            exit_signal: None,
            group_consumed: false,
            target_terminated: false,
            terminal_session_ended: Some(false),
            cleanup_error: None,
            error: None,
        },
    }
}
#[cfg(unix)]
#[test]
fn lifecycle_rejects_stale_foreign_and_unnegotiated_snapshots() {
    let mut value = snapshot(2);
    assert!(super::validate_snapshot(&value, "test", 1).is_ok());
    assert!(super::validate_snapshot(&value, "test", 2).is_err());
    assert!(super::validate_snapshot(&value, "foreign", 1).is_err());
    value.capability += 1;
    assert!(super::validate_snapshot(&value, "test", 1).is_err());
}
#[cfg(unix)]
#[test]
fn watcher_retains_initial_and_shutdown_is_error_not_completion() {
    let (socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let initial = snapshot(1);
    let mut watcher = super::LifecycleWatcher {
        socket,
        initial: initial.clone(),
        current: initial,
    };
    let interrupt = watcher.interrupter().unwrap();
    let waiter = std::thread::spawn(move || {
        assert_eq!(watcher.initial().revision, 1);
        assert_eq!(watcher.initial().status.terminal_session_ended, Some(false));
        watcher.next().is_err()
    });
    interrupt.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(waiter.join().unwrap());
    drop(peer);
}
