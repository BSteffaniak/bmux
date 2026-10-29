#[test]
fn missing_helper_fails_closed_even_for_ordinary_shells() {
    for required in [false, true] {
        let result =
            super::select_backend(Err(anyhow::anyhow!("missing lifetime helper")), required);
        assert!(result.is_err());
    }
}

#[test]
fn selected_backend_retains_guardian_command_not_application_argv() {
    let artifact = std::env::temp_dir().join("bmux-test-execution-supervisor");
    let control = super::SupervisorControl {
        _admission: crate::runtime::managed_idle::admit().unwrap(),
        directory: std::path::PathBuf::new(),
        endpoint: std::path::PathBuf::new(),
        token: "test".into(),
        authentication: "test".into(),
        version: crate::supervisor_protocol::VERSION,
        artifact: artifact.clone(),
        lifetime: std::sync::Mutex::new(super::Lifetime::default()),
    };
    let mut command = portable_pty::CommandBuilder::new(&artifact);
    command.arg("--guardian-test");
    command.arg("bootstrap.json");
    let expected = command.get_argv().clone();
    let selected = super::select_backend(Ok((control, command)), true)
        .unwrap()
        .unwrap();
    assert_eq!(selected.0.artifact, artifact);
    assert_eq!(selected.1.get_argv(), &expected);
    assert_ne!(selected.1.get_argv()[0], std::ffi::OsStr::new("cmd.exe"));
}

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
#[test]
fn pane_lifetime_close_is_idempotent_and_control_drop_closes_lease() {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    fn control(socket: UnixStream) -> super::SupervisorControl {
        super::SupervisorControl {
            _admission: crate::runtime::managed_idle::admit().unwrap(),
            directory: std::path::PathBuf::new(),
            endpoint: std::path::PathBuf::new(),
            token: "test".into(),
            authentication: "test".into(),
            version: crate::supervisor_protocol::VERSION,
            artifact: std::path::PathBuf::new(),
            lifetime: std::sync::Mutex::new(super::Lifetime {
                revoked: false,
                socket: Some(socket),
            }),
        }
    }
    for explicit_close in [false, true] {
        let (server, mut helper) = UnixStream::pair().unwrap();
        helper
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let server = control(server);
        if explicit_close {
            server.end_lifetime().unwrap();
            server.end_lifetime().unwrap();
        } else {
            drop(server);
        }
        assert_eq!(helper.read(&mut [0]).unwrap(), 0);
    }
}

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
            capture_error: None,
            error: None,
        },
    }
}
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
    interrupt.interrupt().unwrap();
    assert!(waiter.join().unwrap());
    drop(peer);
}

#[cfg(unix)]
#[test]
fn negotiation_failure_or_revocation_kills_and_reaps_guardian() {
    use crate::supervisor_protocol as protocol;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    // Exercise EOF, malformed framing, incompatible version, timeout, and a
    // successful reply racing a previous end_lifetime. Only private children.
    for case in 0..5 {
        let (server, mut peer) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(25)))
            .unwrap();
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let lifetime = std::sync::Mutex::new(super::Lifetime::default());
        match case {
            0 => {
                peer.shutdown(std::net::Shutdown::Write).unwrap();
            }
            1 => {
                use std::io::Write;
                peer.write_all(&[0, 4, 0, 1]).unwrap();
            }
            2 => protocol::write_frame(&mut peer, &(protocol::VERSION - 1)).unwrap(),
            3 => {}
            _ => {
                lifetime.lock().unwrap().revoke();
                protocol::write_frame(&mut peer, &protocol::VERSION).unwrap();
            }
        }
        assert!(
            super::negotiate_guardian(super::StartupChild(Some(child)), server, &lifetime).is_err()
        );
        // ECHILD proves reaping, not merely signal delivery to the child.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let mut status = 0;
            let result = unsafe {
                libc::waitpid(i32::try_from(pid).unwrap(), &raw mut status, libc::WNOHANG)
            };
            if result == -1 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD)
                );
                break;
            }
            assert_eq!(result, 0, "startup guard left child unreaped");
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}

#[cfg(unix)]
#[test]
fn lifetime_revocation_is_sticky_before_and_after_installation() {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    for installed in [false, true] {
        let mut lifetime = super::Lifetime::default();
        if installed {
            let (socket, mut peer) = UnixStream::pair().unwrap();
            lifetime.install(socket).unwrap();
            lifetime.revoke();
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        } else {
            lifetime.revoke();
        }
        let (socket, mut peer) = UnixStream::pair().unwrap();
        assert!(lifetime.install(socket).is_err());
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        assert!(lifetime.socket.is_none());
    }
}
