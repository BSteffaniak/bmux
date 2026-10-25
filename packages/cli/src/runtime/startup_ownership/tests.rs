//! Isolated lifecycle protocol tests. Never select an installed runtime.
use super::*;

fn paths(root: &tempfile::TempDir) -> ConfigPaths {
    ConfigPaths::new(
        root.path().join("config"),
        root.path().join("runtime"),
        root.path().join("data"),
        root.path().join("state"),
    )
}

#[test]
fn clean_release_is_not_extended_by_an_inherited_open_description() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    let first = StartupOwnership::acquire(&paths).unwrap();
    // try_clone shares the same open file description as a descriptor inherited
    // by fork, without racing another thread or relying on exec timing.
    let inherited = first.claim_file.lock().unwrap().try_clone().unwrap();
    assert!(StartupOwnership::acquire(&paths).is_err());
    drop(first);
    let replacement = StartupOwnership::acquire(&paths).unwrap();
    drop(inherited);
    // Closing the old description must not unlock the replacement's claim.
    assert!(StartupOwnership::acquire(&paths).is_err());
    drop(replacement);
    assert!(StartupOwnership::acquire(&paths).is_ok());
}

#[test]
fn lifetime_exclusion_includes_before_readiness_and_releases_on_exit() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    let claim = StartupOwnership::acquire(&paths).unwrap();
    // No socket or PID exists: the server is starting, not absent.
    assert!(!paths.server_socket().exists());
    assert!(StartupOwnership::acquire(&paths).is_err());
    drop(claim);
    assert!(StartupOwnership::acquire(&paths).is_ok());
}

#[test]
fn simultaneous_server_starts_have_one_lifetime_owner() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    let barrier = std::sync::Barrier::new(3);
    std::thread::scope(|scope| {
        let enter = || {
            barrier.wait();
            StartupOwnership::acquire(&paths)
        };
        let left = scope.spawn(enter);
        let right = scope.spawn(enter);
        barrier.wait();
        let left = left.join().unwrap();
        let right = right.join().unwrap();
        assert_ne!(left.is_ok(), right.is_ok());
    });
}

#[cfg(unix)]
#[test]
fn clean_stop_socket_owner_fixture() {
    let Some(root) = std::env::var_os("BMUX_CLEAN_STOP_FIXTURE_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let paths = ConfigPaths::new(
        root.join("config"),
        root.join("runtime"),
        root.join("data"),
        root.join("state"),
    );
    let claim = StartupOwnership::acquire(&paths).unwrap();
    let socket = std::os::unix::net::UnixListener::bind(paths.server_socket()).unwrap();
    claim.record_bound_endpoint().unwrap();
    drop(socket);
    drop(claim);
}

#[cfg(unix)]
#[tokio::test]
async fn clean_stop_then_restart_reclaims_only_recorded_socket() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    // The listener must never exist in the parallel test runner: unrelated
    // concurrent forks can inherit it until exec, correctly making it look live.
    // An isolated owner has no sibling threads forking; waiting for exit is the
    // deterministic close fence, not a refusal retry or relaxed ownership check.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime::startup_ownership::tests::clean_stop_socket_owner_fixture",
            "--nocapture",
        ])
        .env("BMUX_CLEAN_STOP_FIXTURE_ROOT", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "socket owner fixture failed: {output:?}"
    );
    let second = StartupOwnership::acquire(&paths).unwrap();
    second.prepare_endpoint().await.unwrap();
    assert!(!paths.server_socket().exists());
    let socket = tokio::net::UnixListener::bind(paths.server_socket()).unwrap();
    second.record_bound_endpoint().unwrap();
    assert!(
        tokio::net::UnixStream::connect(paths.server_socket())
            .await
            .is_ok()
    );
    drop(socket);
}

#[cfg(unix)]
#[tokio::test]
async fn live_and_replacement_and_unknown_endpoints_are_preserved() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    let first = StartupOwnership::acquire(&paths).unwrap();
    let original = tokio::net::UnixListener::bind(paths.server_socket()).unwrap();
    first.record_bound_endpoint().unwrap();
    drop(first);
    let next = StartupOwnership::acquire(&paths).unwrap();
    assert!(next.prepare_endpoint().await.is_err()); // live even with receipt
    std::fs::remove_file(paths.server_socket()).unwrap();
    let replacement = tokio::net::UnixListener::bind(paths.server_socket()).unwrap();
    assert!(next.prepare_endpoint().await.is_err()); // new inode, not old receipt
    drop(replacement);
    assert!(next.prepare_endpoint().await.is_err()); // refused is not ownership
    std::fs::remove_file(paths.server_socket()).unwrap();
    std::fs::write(paths.server_socket(), b"preserve").unwrap();
    assert!(next.prepare_endpoint().await.is_err());
    assert_eq!(std::fs::read(paths.server_socket()).unwrap(), b"preserve");
    std::fs::remove_file(paths.server_socket()).unwrap();
    symlink(root.path().join("missing"), paths.server_socket()).unwrap();
    assert!(next.prepare_endpoint().await.is_err());
    assert!(
        std::fs::symlink_metadata(paths.server_socket())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    drop(original);
}

#[test]
fn lifecycle_child_before_readiness() {
    let Some(root) = std::env::var_os("BMUX_LIFETIME_TEST_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let paths = ConfigPaths::new(
        root.join("config"),
        root.join("runtime"),
        root.join("data"),
        root.join("state"),
    );
    let claim = StartupOwnership::acquire(&paths).unwrap();
    std::fs::write(root.join("child-claimed"), b"").unwrap();
    // This is an isolated fixture, not a production wait. The test parent kills
    // exactly its own child to prove the kernel releases ownership on crash.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !root.join("release-child").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    drop(claim);
    std::fs::write(root.join("child-released"), b"").unwrap();
}

#[test]
fn lifecycle_frontend_before_readiness() {
    let Some(root) = std::env::var_os("BMUX_LIFETIME_FRONTEND_TEST_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime::startup_ownership::tests::lifecycle_child_before_readiness",
            "--nocapture",
        ])
        .env("BMUX_LIFETIME_TEST_ROOT", &root)
        .env_remove("BMUX_LIFETIME_FRONTEND_TEST_ROOT")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // The test parent crashes this frontend. Child retains its independent claim.
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("server fixture exited: {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn real_frontend_process_crash_before_readiness_keeps_server_owned() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    let mut frontend = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime::startup_ownership::tests::lifecycle_frontend_before_readiness",
            "--nocapture",
        ])
        .env("BMUX_LIFETIME_FRONTEND_TEST_ROOT", root.path())
        .env_remove("BMUX_LIFETIME_TEST_ROOT")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.path().join("child-claimed").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let claimed = root.path().join("child-claimed").exists();
    frontend.kill().unwrap();
    frontend.wait().unwrap();
    let excluded_after_frontend_crash = StartupOwnership::acquire(&paths).is_err();
    // Release only this fixture's detached child; never signal an inferred PID.
    std::fs::write(root.path().join("release-child"), b"").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.path().join("child-released").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(claimed, "server fixture failed to claim");
    assert!(excluded_after_frontend_crash);
    assert!(root.path().join("child-released").exists());
    assert!(StartupOwnership::acquire(&paths).is_ok());
}

#[test]
fn frontend_loss_does_not_release_child_claim_and_server_crash_does() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    let launch_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.path().join("frontend.lock"))
        .unwrap();
    launch_lock.lock().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime::startup_ownership::tests::lifecycle_child_before_readiness",
            "--nocapture",
        ])
        .env("BMUX_LIFETIME_TEST_ROOT", root.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.path().join("child-claimed").exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("fixture exited before claim: {status}");
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("fixture claim timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    drop(launch_lock); // all frontend-owned startup authority is now gone
    let before_readiness = !paths.server_socket().exists();
    let child_still_owns = StartupOwnership::acquire(&paths).is_err();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(before_readiness);
    assert!(child_still_owns);
    assert!(StartupOwnership::acquire(&paths).is_ok());
}

#[cfg(unix)]
#[test]
fn corrupt_and_future_receipts_fail_even_without_an_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    paths.ensure_runtime_dir().unwrap();
    for receipt in [
        "future-claim-v2\n",
        "bmux-endpoint-v1 1 broken\n",
        "bmux-endpoint-v1 1 2 extra\n",
    ] {
        std::fs::write(paths.runtime_dir.join(CLAIM_NAME), receipt).unwrap();
        let claim = StartupOwnership::acquire(&paths).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(runtime.block_on(claim.prepare_endpoint()).is_err());
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_lifetime_claim_prevents_startup_and_pipe_name_remains_exclusive() {
    use tokio::net::windows::named_pipe::ServerOptions;
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    let claim = StartupOwnership::acquire(&paths).unwrap();
    assert!(StartupOwnership::acquire(&paths).is_err());
    claim.prepare_endpoint().await.unwrap();
    let pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(paths.server_named_pipe())
        .unwrap();
    assert!(claim.prepare_endpoint().await.is_err());
    assert!(
        ServerOptions::new()
            .first_pipe_instance(true)
            .create(paths.server_named_pipe())
            .is_err()
    );
    drop(pipe);
    drop(claim);
    let restarted = StartupOwnership::acquire(&paths).unwrap();
    restarted.prepare_endpoint().await.unwrap();
}

#[test]
fn claim_path_must_not_be_symlink_or_directory() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(&root);
    std::fs::create_dir_all(paths.runtime_dir.join(CLAIM_NAME)).unwrap();
    assert!(StartupOwnership::acquire(&paths).is_err());
    #[cfg(unix)]
    {
        std::fs::remove_dir(paths.runtime_dir.join(CLAIM_NAME)).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("missing"),
            paths.runtime_dir.join(CLAIM_NAME),
        )
        .unwrap();
        assert!(StartupOwnership::acquire(&paths).is_err());
    }
}
