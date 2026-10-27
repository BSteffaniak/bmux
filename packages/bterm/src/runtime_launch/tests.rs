//! Startup tests never contact an installed BMUX endpoint. The Unix subprocess
//! fixture is an explicit shell in a fresh root; its output proves launch count.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

mod identity;
#[cfg(unix)]
mod inherited;

#[cfg(unix)]
#[tokio::test]
async fn unrelated_legacy_namespace_never_gates_selected_target() {
    use std::os::unix::net::UnixListener;
    let root = Root::new();
    let legacy = root.0.join("runtime");
    std::fs::create_dir(&legacy).unwrap();
    let claim_path = legacy.join("server-lifetime.lock");
    let owner = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&claim_path)
        .unwrap();
    owner.try_lock().unwrap();
    let socket = legacy.join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut spec = root.spec();
    let target = root.0.join("target-namespace");
    std::fs::create_dir(&target).unwrap();
    spec.ownership_file = target.join("companion-startup.lock");
    assert_eq!(
        ensure_runtime(&spec, || async { Ok(RuntimeProbe::Compatible) })
            .await
            .unwrap(),
        RuntimeLaunchOutcome::AlreadyRunning
    );
    assert!(socket.exists());
    assert!(!spec.ownership_file.exists());
    drop(listener);
    drop(owner);
}

#[test]
fn configuration_snapshot_publication_recovers_interrupted_staging_and_rejects_drift() {
    let root = Root::new();
    let path = root.0.join("configuration.toml");
    std::fs::write(path.with_extension("next"), b"partial").unwrap();
    write_configuration_snapshot(&path, b"[general]\nscrollback_limit = 42\n").unwrap();
    assert!(!path.with_extension("next").exists());
    write_configuration_snapshot(&path, b"[general]\nscrollback_limit = 42\n").unwrap();
    assert!(write_configuration_snapshot(&path, b"different").is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn readiness_diagnostics_distinguish_reused_and_started_without_guessing_cause() {
    let root = Root::new();
    let spec = root.spec();
    let reused = readiness_failure_context(RuntimeLaunchOutcome::AlreadyRunning, &spec);
    let fresh = readiness_failure_context(RuntimeLaunchOutcome::Started { pid: 42 }, &spec);
    assert!(reused.contains("reused an already-running daemon"));
    assert!(!fresh.contains("reused an already-running daemon"));
    assert!(fresh.contains("launched a new companion"));
    for message in [reused, fresh] {
        assert!(message.contains("preserved without restart"));
        assert!(message.contains(spec.executable.to_str().unwrap()));
        assert!(message.contains(spec.diagnostic_file.to_str().unwrap()));
    }
}

#[test]
fn service_incompatibility_is_not_optional_absence_or_starting() {
    for code in [
        bmux_ipc::ErrorCode::ServiceIncompatible,
        bmux_ipc::ErrorCode::UnsupportedRepresentation,
    ] {
        let error = anyhow::Error::new(bmux_client::ClientError::ServerError {
            code,
            message: "unsupported selected contract".into(),
        })
        .context("selected runtime handshake");
        assert_eq!(
            runtime_incompatibility(&error).as_deref(),
            Some("unsupported selected contract")
        );
    }
    for code in [bmux_ipc::ErrorCode::NotFound, bmux_ipc::ErrorCode::Internal] {
        let error = anyhow::Error::new(bmux_client::ClientError::ServerError {
            code,
            message: "ServiceIncompatible is only text".into(),
        });
        assert!(runtime_incompatibility(&error).is_none());
    }
}

pub(super) struct Root(pub(super) PathBuf);

impl Root {
    pub(super) fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "bterm-launch-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn spec(&self) -> RuntimeLaunchSpec {
        RuntimeLaunchSpec {
            executable: self.0.join("missing-companion"),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            working_directory: self.0.clone(),
            ownership_file: self.0.join("startup.lock"),
            diagnostic_file: self.0.join("startup.log"),
            timeout: Duration::from_secs(3),
            poll_interval: Duration::from_millis(5),
        }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        // Delete only this fixture's known files, never recurse into user roots.
        for name in [
            "startup.lock",
            "server-lifetime.lock",
            "startup.log",
            "server.sock",
            "bterm-principal.lock",
            "runtime/principal-id",
            "launched",
            "ready",
            "release",
            "done",
        ] {
            let _ = std::fs::remove_file(self.0.join(name));
        }
        let _ = std::fs::remove_dir(self.0.join("runtime"));
        let _ = std::fs::remove_dir(&self.0);
    }
}

#[cfg(unix)]
fn probe_fixture_paths(root: &Root) -> bmux_config::ConfigPaths {
    bmux_config::ConfigPaths::new(
        root.0.clone(),
        root.0.clone(),
        root.0.clone(),
        root.0.clone(),
    )
}

#[cfg(unix)]
#[tokio::test]
async fn missing_endpoint_is_absent_but_stale_and_dangling_names_are_not() {
    let root = Root::new();
    let paths = probe_fixture_paths(&root);
    assert_eq!(
        probe_paths(&paths, Duration::from_millis(100))
            .await
            .unwrap(),
        RuntimeProbe::Absent
    );
    let stale = std::os::unix::net::UnixListener::bind(paths.server_socket()).unwrap();
    drop(stale);
    assert!(
        probe_paths(&paths, Duration::from_millis(100))
            .await
            .is_err()
    );
    assert!(paths.server_socket().exists());
    std::fs::remove_file(paths.server_socket()).unwrap();
    std::os::unix::fs::symlink(root.0.join("missing-target"), paths.server_socket()).unwrap();
    assert!(
        probe_paths(&paths, Duration::from_millis(100))
            .await
            .is_err()
    );
    assert!(
        std::fs::symlink_metadata(paths.server_socket())
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn listening_endpoint_without_handshake_is_uncertain_not_absent() {
    let root = Root::new();
    let paths = probe_fixture_paths(&root);
    let listener = std::os::unix::net::UnixListener::bind(paths.server_socket()).unwrap();
    let error = probe_paths(&paths, Duration::from_millis(40))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("uncertain"));
    assert!(paths.server_socket().exists());
    drop(listener);
}

#[tokio::test]
async fn contended_inherited_claim_allows_readiness_without_owner_exit() {
    let root = Root::new();
    let spec = root.spec();
    let owner = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&spec.ownership_file)
        .unwrap();
    owner.try_lock().unwrap();
    let mut calls = 0;
    let outcome = ensure_runtime(&spec, || {
        calls += 1;
        let state = if calls == 1 {
            RuntimeProbe::Absent
        } else {
            RuntimeProbe::Compatible
        };
        async move { Ok(state) }
    })
    .await
    .unwrap();
    assert_eq!(outcome, RuntimeLaunchOutcome::AlreadyRunning);
    assert_eq!(
        calls, 2,
        "readiness must be re-probed while the claim is still held"
    );
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&spec.ownership_file)
        .unwrap();
    assert!(matches!(
        contender.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(!spec.diagnostic_file.exists());
    drop(owner);
}

#[tokio::test]
async fn contended_claim_preserves_incompatibility_without_waiting_for_exit() {
    let root = Root::new();
    let spec = root.spec();
    let owner = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&spec.ownership_file)
        .unwrap();
    owner.try_lock().unwrap();
    let mut calls = 0;
    let result = ensure_runtime(&spec, || {
        calls += 1;
        let state = if calls == 1 {
            RuntimeProbe::Absent
        } else {
            RuntimeProbe::Incompatible("selected target mismatch".into())
        };
        async move { Ok(state) }
    })
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("selected target mismatch")
    );
    assert_eq!(calls, 2);
    assert!(!spec.diagnostic_file.exists());
    drop(owner);
}

#[tokio::test]
async fn reconciliation_contention_uses_startup_budget_not_rpc_budget() {
    let root = Root::new();
    let path = root.0.join("reconciliation.lock");
    let owner = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    owner.try_lock().unwrap();
    let waiter = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let startup_deadline = Instant::now() + Duration::from_secs(2);
    let release = async move {
        // Model cold startup exceeding an individual probe's short budget.
        tokio::time::sleep(Duration::from_millis(40)).await;
        drop(owner);
    };
    let (result, ()) = tokio::join!(
        wait_for_reconciliation_claim(&waiter, startup_deadline, Duration::from_millis(2)),
        release,
    );
    result.unwrap();
    assert!(Instant::now() < startup_deadline);
    assert!(MANAGED_STARTUP_TIMEOUT > MANAGED_RPC_TIMEOUT);
    assert!(path.exists());
}

#[tokio::test]
async fn reconciliation_expiry_preserves_the_live_owner_claim() {
    let root = Root::new();
    let path = root.0.join("reconciliation.lock");
    let owner = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    owner.try_lock().unwrap();
    let waiter = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let error = wait_for_reconciliation_claim(&waiter, Instant::now(), Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("startup timed out"));
    assert!(matches!(
        waiter.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(path.exists());
    drop(owner);
    waiter.try_lock().unwrap();
}

#[tokio::test]
async fn concurrent_waiter_reprobes_incompatibility_before_launch() {
    let root = Root::new();
    let spec = root.spec();
    let mut calls = 0;
    let error = ensure_runtime(&spec, || {
        calls += 1;
        let state = if calls == 1 {
            RuntimeProbe::Absent
        } else {
            RuntimeProbe::Incompatible("another launcher won".into())
        };
        async move { Ok(state) }
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("incompatible"));
    assert_eq!(calls, 2);
    assert!(
        !spec.diagnostic_file.exists(),
        "must not spawn after post-lock recheck"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn occupied_named_pipe_without_handshake_never_permits_startup() {
    use tokio::net::windows::named_pipe::ServerOptions;
    let root = Root::new();
    let paths = bmux_config::ConfigPaths::new(
        root.0.clone(),
        root.0.clone(),
        root.0.clone(),
        root.0.clone(),
    );
    let pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(paths.server_named_pipe())
        .unwrap();
    let error = probe_paths(&paths, Duration::from_millis(40))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("uncertain"));
    assert!(
        ServerOptions::new()
            .first_pipe_instance(true)
            .create(paths.server_named_pipe())
            .is_err()
    );
    drop(pipe);
    assert_eq!(
        probe_paths(&paths, Duration::from_millis(100))
            .await
            .unwrap(),
        RuntimeProbe::Absent
    );
}

#[cfg(unix)]
#[tokio::test]
async fn server_claim_survives_lost_frontend_startup_lock_before_readiness() {
    let root = Root::new();
    let paths = probe_fixture_paths(&root);
    let server_claim = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.0.join("server-lifetime.lock"))
        .unwrap();
    server_claim.lock().unwrap();
    let frontend_claim = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.0.join("startup.lock"))
        .unwrap();
    frontend_claim.lock().unwrap();
    drop(frontend_claim); // simulated lost/crashed window launcher
    assert_eq!(
        probe_paths(&paths, Duration::from_millis(100))
            .await
            .unwrap(),
        RuntimeProbe::Starting
    );
    let mut spec = root.spec();
    spec.timeout = Duration::from_millis(30);
    let error = ensure_runtime(&spec, || probe_paths(&paths, Duration::from_millis(100)))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(
        !spec.diagnostic_file.exists(),
        "must not launch while server owns pre-readiness claim"
    );
    drop(server_claim);
    assert_eq!(
        probe_paths(&paths, Duration::from_millis(100))
            .await
            .unwrap(),
        RuntimeProbe::Absent
    );
}

#[cfg(unix)]
#[tokio::test]
async fn owned_stopped_socket_is_reclaimable_but_frontend_never_unlinks() {
    use std::os::unix::fs::MetadataExt as _;
    let root = Root::new();
    let paths = probe_fixture_paths(&root);
    let socket = std::os::unix::net::UnixListener::bind(paths.server_socket()).unwrap();
    let metadata = std::fs::symlink_metadata(paths.server_socket()).unwrap();
    std::fs::write(
        root.0.join("server-lifetime.lock"),
        format!("bmux-endpoint-v1 {} {}\n", metadata.dev(), metadata.ino()),
    )
    .unwrap();
    drop(socket);
    assert_eq!(
        probe_paths(&paths, Duration::from_millis(100))
            .await
            .unwrap(),
        RuntimeProbe::Absent
    );
    let after = std::fs::symlink_metadata(paths.server_socket()).unwrap();
    assert_eq!((after.dev(), after.ino()), (metadata.dev(), metadata.ino()));
    std::fs::write(
        root.0.join("server-lifetime.lock"),
        "unsupported-receipt-v2\n",
    )
    .unwrap();
    assert!(
        probe_paths(&paths, Duration::from_millis(100))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn starting_becomes_compatible_without_spawning() {
    let root = Root::new();
    let mut calls = 0;
    let outcome = ensure_runtime(&root.spec(), || {
        calls += 1;
        let state = if calls < 3 {
            RuntimeProbe::Starting
        } else {
            RuntimeProbe::Compatible
        };
        async move { Ok(state) }
    })
    .await
    .unwrap();
    assert_eq!(outcome, RuntimeLaunchOutcome::AlreadyRunning);
    assert!(!root.0.join("startup.log").exists());
}

#[tokio::test]
async fn existing_compatible_does_not_spawn_or_create_ownership_file() {
    let root = Root::new();
    let result = ensure_runtime(&root.spec(), || async { Ok(RuntimeProbe::Compatible) })
        .await
        .unwrap();
    assert_eq!(result, RuntimeLaunchOutcome::AlreadyRunning);
    assert!(!root.0.join("startup.lock").exists());
}

#[tokio::test]
async fn incompatible_and_uncertain_do_not_start_or_mutate() {
    let root = Root::new();
    let error = ensure_runtime(&root.spec(), || async {
        Ok(RuntimeProbe::Incompatible("bundle identity differs".into()))
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("incompatible"));
    let error = ensure_runtime(&root.spec(), || async { bail!("authentication refused") })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("authentication"));
    assert!(!root.0.join("startup.lock").exists());
}

#[tokio::test]
async fn relative_executable_is_rejected_before_probe() {
    let root = Root::new();
    let mut spec = root.spec();
    spec.executable = "bmux".into();
    let error = ensure_runtime(&spec, || async { panic!("must validate first") })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("absolute"));
}

#[tokio::test]
async fn readiness_probe_is_deadline_bounded() {
    let root = Root::new();
    let mut spec = root.spec();
    spec.timeout = Duration::from_millis(20);
    let error = ensure_runtime(&spec, std::future::pending)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
}

#[tokio::test]
async fn ownership_contention_times_out_without_unlinking_lock() {
    let root = Root::new();
    let mut spec = root.spec();
    spec.timeout = Duration::from_millis(20);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&spec.ownership_file)
        .unwrap();
    lock.lock().unwrap();
    let error = ensure_runtime(&spec, || async { Ok(RuntimeProbe::Absent) })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(spec.ownership_file.exists());
    drop(lock);
    assert!(
        OpenOptions::new()
            .write(true)
            .open(spec.ownership_file)
            .unwrap()
            .try_lock()
            .is_ok()
    );
}

#[cfg(unix)]
fn shell_spec(root: &Root, script: &str) -> RuntimeLaunchSpec {
    let mut spec = root.spec();
    spec.executable = "/bin/sh".into();
    spec.arguments = vec!["-c".into(), script.into()];
    // Production launches deliberately clear the environment. These shell
    // fixtures still need the toolchain's utilities rather than /bin paths.
    spec.environment.insert(
        "PATH".into(),
        std::env::var_os("PATH").expect("test shell utilities on PATH"),
    );
    spec
}

#[cfg(unix)]
async fn await_file(root: &Root, name: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.0.join(name).exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_launchers_spawn_once_and_companion_outlives_result() {
    let root = Root::new();
    let spec = shell_spec(
        &root,
        "printf 'launch\\n' >> launched; : > ready; while [ ! -f release ]; do sleep 0.01; done; : > done",
    );
    let probe = || async {
        Ok(if root.0.join("ready").exists() {
            RuntimeProbe::Compatible
        } else {
            RuntimeProbe::Absent
        })
    };
    let (left, right) = tokio::join!(ensure_runtime(&spec, probe), ensure_runtime(&spec, probe));
    let left = left.unwrap();
    let right = right.unwrap();
    assert!(matches!(
        (left, right),
        (
            RuntimeLaunchOutcome::Started { .. },
            RuntimeLaunchOutcome::AlreadyRunning
        ) | (
            RuntimeLaunchOutcome::AlreadyRunning,
            RuntimeLaunchOutcome::Started { .. }
        )
    ));
    assert_eq!(
        std::fs::read_to_string(root.0.join("launched")).unwrap(),
        "launch\n"
    );
    assert!(
        !root.0.join("done").exists(),
        "child must persist after launch result"
    );
    std::fs::write(root.0.join("release"), b"").unwrap();
    await_file(&root, "done").await;
}

#[cfg(unix)]
#[tokio::test]
async fn child_exit_before_readiness_is_not_absence() {
    let root = Root::new();
    let spec = shell_spec(&root, "exit 23");
    let error = ensure_runtime(&spec, || async { Ok(RuntimeProbe::Absent) })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exited before readiness"));
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_startup_retains_ownership_until_child_exits() {
    let root = Root::new();
    let spec = shell_spec(
        &root,
        ": > launched; while [ ! -f release ]; do sleep 0.01; done; : > done",
    );
    let mut launch = Box::pin(ensure_runtime(&spec, || async { Ok(RuntimeProbe::Absent) }));
    tokio::select! {
        result = &mut launch => panic!("startup unexpectedly settled: {result:?}"),
        () = await_file(&root, "launched") => {}
    }
    drop(launch);
    let lock = OpenOptions::new()
        .write(true)
        .open(&spec.ownership_file)
        .unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    std::fs::write(root.0.join("release"), b"").unwrap();
    await_file(&root, "done").await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while lock.try_lock().is_err() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_keeps_startup_owned_until_child_exits_without_killing_it() {
    let root = Root::new();
    let mut spec = shell_spec(
        &root,
        ": > launched; while [ ! -f release ]; do sleep 0.01; done; : > done",
    );
    spec.timeout = Duration::from_millis(80);
    let error = ensure_runtime(&spec, || async { Ok(RuntimeProbe::Absent) })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(root.0.join("launched").exists());
    let lock = OpenOptions::new()
        .write(true)
        .open(&spec.ownership_file)
        .unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    std::fs::write(root.0.join("release"), b"").unwrap();
    await_file(&root, "done").await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while lock.try_lock().is_err() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
