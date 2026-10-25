//! Real subprocess tests of managed startup fencing, without host runtime discovery.
//!
//! The executable fixture is this exact Cargo integration-test artifact. Children
//! have finite lifetimes and private roots; no test kills or adopts another runtime.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bterm::runtime_launch::{
    RuntimeLaunchOutcome, RuntimeLaunchSpec, RuntimeProbe, ensure_runtime,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
const CHILD_ROOT: &str = "BTERM_STARTUP_SUBPROCESS_ROOT";
const CHILD_MODE: &str = "BTERM_STARTUP_SUBPROCESS_MODE";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = Path::new("/tmp").canonicalize().unwrap();
        let root = loop {
            let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
            let root = base.join(format!("bts-{}-{id}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => break root,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture root: {error}"),
            }
        };
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn spec(&self, mode: &str) -> RuntimeLaunchSpec {
        RuntimeLaunchSpec {
            executable: std::env::current_exe().unwrap().canonicalize().unwrap(),
            arguments: ["--exact", "companion_process_fixture", "--nocapture"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            environment: BTreeMap::from([
                (CHILD_ROOT.into(), self.root.as_os_str().to_owned()),
                (CHILD_MODE.into(), mode.into()),
            ]),
            working_directory: self.root.clone(),
            ownership_file: self.root.join("startup.lock"),
            diagnostic_file: self.root.join("startup.log"),
            timeout: Duration::from_secs(4),
            poll_interval: Duration::from_millis(10),
        }
    }

    async fn wait_for(&self, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.root.join(name).exists() {
            assert!(Instant::now() < deadline, "missing child marker {name}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn release(&self) {
        fs::write(self.root.join("release"), b"release fixture only").unwrap();
        self.wait_for("exited").await;
        // `exited` is written just before process exit. Ownership is the proof
        // that the production reaper has observed exit and released its fence.
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("startup.lock"))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    assert!(
                        Instant::now() < deadline,
                        "startup fence retained after exit"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("inspect fixture fence: {error}"),
            }
        }
    }

    fn spawn_count(&self) -> usize {
        fs::read_to_string(self.root.join("spawns"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

// Roots intentionally remain available for failure diagnostics. In particular,
// unwinding never removes ownership evidence while a finite-lived child runs.

fn probe(root: &Path) -> RuntimeProbe {
    if root.join("ready").exists() {
        RuntimeProbe::Compatible
    } else {
        RuntimeProbe::Absent
    }
}

/// Invoked only by ensure_runtime using this exact test executable and private env.
#[test]
fn companion_process_fixture() {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(root.is_absolute());
    let mode = std::env::var(CHILD_MODE).unwrap();
    use std::io::Write as _;
    let mut spawns = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("spawns"))
        .unwrap();
    writeln!(spawns, "{}", std::process::id()).unwrap();
    spawns.sync_all().unwrap();
    if mode == "exit" {
        eprintln!("fixture injected early child failure");
        fs::write(root.join("exited"), b"early exit").unwrap();
        std::process::exit(23);
    }
    fs::write(root.join("entered"), b"child owns lifetime").unwrap();
    // Every child exits on its own even if the parent test panics or is cancelled.
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if mode == "ready" || (mode == "delayed" && root.join("allow-ready").exists()) {
            fs::write(root.join("ready"), b"fixture readiness").unwrap();
        }
        if root.join("release").exists() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::write(root.join("exited"), b"normal exit").unwrap();
}

#[tokio::test]
async fn concurrent_launchers_spawn_one_child_and_reuse_after_readiness() {
    let fixture = Fixture::new();
    let spec = fixture.spec("delayed");
    let first = ensure_runtime(&spec, || async { Ok(probe(&fixture.root)) });
    let second = ensure_runtime(&spec, || async { Ok(probe(&fixture.root)) });
    let allow = async {
        fixture.wait_for("entered").await;
        assert_eq!(fixture.spawn_count(), 1);
        fs::write(fixture.root.join("allow-ready"), b"publish readiness").unwrap();
    };
    let (first, second, ()) = tokio::join!(first, second, allow);
    let outcomes = [first.unwrap(), second.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, RuntimeLaunchOutcome::Started { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == RuntimeLaunchOutcome::AlreadyRunning)
            .count(),
        1
    );
    assert_eq!(
        ensure_runtime(&spec, || async { Ok(probe(&fixture.root)) })
            .await
            .unwrap(),
        RuntimeLaunchOutcome::AlreadyRunning
    );
    assert_eq!(fixture.spawn_count(), 1);
    fixture.release().await;
}

#[tokio::test]
async fn timeout_retains_fence_until_real_child_exit_then_allows_restart() {
    let fixture = Fixture::new();
    let mut spec = fixture.spec("never-ready");
    fs::write(
        fixture.root.join("durable-sentinel"),
        b"keep selected installation",
    )
    .unwrap();
    spec.timeout = Duration::from_millis(350);
    let first = ensure_runtime(&spec, || async { Ok(RuntimeProbe::Absent) }).await;
    assert!(first.is_err());
    fixture.wait_for("entered").await;
    let second = ensure_runtime(&spec, || async { Ok(RuntimeProbe::Absent) }).await;
    assert!(second.is_err());
    assert_eq!(
        fixture.spawn_count(),
        1,
        "timeout must not authorize replacement"
    );
    fixture.release().await;
    fs::remove_file(fixture.root.join("release")).unwrap();
    fs::remove_file(fixture.root.join("exited")).unwrap();
    let restarted = ensure_runtime(&fixture.spec("ready"), || async {
        Ok(probe(&fixture.root))
    })
    .await
    .unwrap();
    assert!(matches!(restarted, RuntimeLaunchOutcome::Started { .. }));
    assert_eq!(fixture.spawn_count(), 2);
    assert_eq!(
        fs::read(fixture.root.join("durable-sentinel")).unwrap(),
        b"keep selected installation"
    );
    fixture.release().await;
}

#[tokio::test]
async fn cancellation_retains_startup_fence_while_child_is_alive() {
    let fixture = Fixture::new();
    let spec = fixture.spec("never-ready");
    let mut startup = Box::pin(ensure_runtime(&spec, || async { Ok(RuntimeProbe::Absent) }));
    tokio::select! {
        result = &mut startup => panic!("startup unexpectedly settled: {result:?}"),
        () = fixture.wait_for("entered") => {}
    }
    // Dropping the actual future, not merely a reference to a pinned future,
    // is the cancellation boundary under test.
    drop(startup);
    let mut contender = fixture.spec("ready");
    contender.timeout = Duration::from_millis(100);
    assert!(
        ensure_runtime(&contender, || async { Ok(RuntimeProbe::Absent) })
            .await
            .is_err()
    );
    assert_eq!(fixture.spawn_count(), 1);
    fixture.release().await;
}

#[tokio::test]
async fn independent_target_authorities_start_and_remain_live_together() {
    let first = Fixture::new();
    let second = Fixture::new();
    let first_spec = first.spec("ready");
    let second_spec = second.spec("ready");
    let (a, b) = tokio::join!(
        ensure_runtime(&first_spec, || async { Ok(probe(&first.root)) }),
        ensure_runtime(&second_spec, || async { Ok(probe(&second.root)) }),
    );
    let RuntimeLaunchOutcome::Started { pid: first_pid } = a.unwrap() else {
        panic!("first private authority did not start");
    };
    let RuntimeLaunchOutcome::Started { pid: second_pid } = b.unwrap() else {
        panic!("second private authority did not start");
    };
    assert_ne!(first_pid, second_pid);
    first.release().await;
    assert!(!second.root.join("exited").exists());
    assert_eq!(
        ensure_runtime(&second_spec, || async { Ok(probe(&second.root)) })
            .await
            .unwrap(),
        RuntimeLaunchOutcome::AlreadyRunning
    );
    assert_eq!(second.spawn_count(), 1);
    second.release().await;
}

#[tokio::test]
async fn early_exit_and_incompatible_target_are_explicit_failures() {
    let fixture = Fixture::new();
    let result = ensure_runtime(&fixture.spec("exit"), || async { Ok(RuntimeProbe::Absent) }).await;
    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("exit"), "{message}");
    fixture.release().await;
    let result = ensure_runtime(&fixture.spec("ready"), || async {
        Ok(RuntimeProbe::Incompatible(
            "different selected artifact".into(),
        ))
    })
    .await;
    assert!(format!("{:#}", result.unwrap_err()).contains("different selected artifact"));
    assert_eq!(
        fixture.spawn_count(),
        1,
        "incompatibility never permits a new child"
    );
}
