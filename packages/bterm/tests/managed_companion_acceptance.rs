//! Opt-in acceptance using actual immutable Bterm/BMUX bundles, never test-server
//! impersonation. Run explicitly with --ignored and BTERM_TEST_BUNDLE plus
//! BTERM_TEST_BUNDLE_SECOND (absolute bundle directories or frontend paths).
//!
//! Every invocation has private roots, a cleared environment, and an exact binary.
//! No PATH companion discovery, signal termination, host-root cleanup, or artifact
//! mutation occurs. Successful cases await runtime-owned graceful idle retirement.
//! Private roots/diagnostics are retained even on failure for ownership evidence.
#![cfg(unix)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;

// Each scenario still races its own launchers; serializing independent cold-bundle
// scenarios avoids turning host-wide disk contention into a startup deadline test.
static SCENARIO: std::sync::Mutex<()> = std::sync::Mutex::new(());

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
static NEXT_INVOCATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Deserialize)]
struct Roots {
    config_dir: PathBuf,
    runtime_dir: PathBuf,
    data_dir: PathBuf,
    state_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct Report {
    schema_version: u32,
    process_id: u32,
    instance: String,
    target_id: String,
    artifact_set_sha256: String,
    companion_sha256: String,
    configuration_id: String,
    scope_id: String,
    roots: Roots,
    idle_managed: bool,
    idle_grace_seconds: u64,
    idle_blocked: bool,
}

struct Fixture {
    root: PathBuf,
    installation: PathBuf,
    config: PathBuf,
    bterm_config: PathBuf,
    idle_grace_seconds: u64,
}

impl Fixture {
    fn new() -> Self {
        Self::with_idle_grace(30)
    }

    fn with_idle_grace(idle_grace_seconds: u64) -> Self {
        assert!((30..=120).contains(&idle_grace_seconds));
        let base = Path::new("/tmp").canonicalize().unwrap();
        let root = loop {
            let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
            let root = base.join(format!("bta-{}-{id}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => break root,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create private acceptance root: {error}"),
            }
        };
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        for directory in [
            "home", "config", "data", "state", "cache", "runtime", "tmp", "work",
        ] {
            let path = root.join(directory);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let installation = root.join("installation");
        // Bterm owns installation/data; managed_base_roots selects its BMUX
        // domain beneath data/bmux unless a legacy state/bmux/data exists.
        fs::create_dir_all(installation.join("data/bmux")).unwrap();
        fs::create_dir_all(installation.join("runtime")).unwrap();
        fs::write(
            installation.join("data/bmux/sentinel"),
            b"durable installation before any managed launch",
        )
        .unwrap();
        fs::write(
            installation.join("runtime/legacy-evidence"),
            b"legacy runtime evidence is not stale by inference",
        )
        .unwrap();
        // A root that resembles the canonical install must remain untouched.
        fs::create_dir_all(root.join("state/bterm/data")).unwrap();
        fs::write(
            root.join("state/bterm/data/canonical"),
            b"canonical installation",
        )
        .unwrap();
        let config = root.join("bmux.toml");
        fs::write(&config, "[general]\nscrollback_limit = 8123\n").unwrap();
        let bterm_config = root.join("bterm.toml");
        fs::write(
            &bterm_config,
            format!("[bterm.companion]\nidle_after_seconds = {idle_grace_seconds}\n"),
        )
        .unwrap();
        Self {
            root,
            installation,
            config,
            bterm_config,
            idle_grace_seconds,
        }
    }

    fn command(&self, frontend: &Path) -> Command {
        let mut command = Command::new(frontend);
        command.env_clear().current_dir(self.root.join("work"));
        for (key, relative) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("TMPDIR", "tmp"),
        ] {
            command.env(key, self.root.join(relative));
        }
        // No executable may be selected through the caller's PATH.
        command.env("PATH", "/usr/bin:/bin");
        command
            .arg("--managed-root")
            .arg(&self.installation)
            .arg("--no-bmux-config")
            .arg("--config")
            .arg(&self.bterm_config)
            .arg("--bmux-config")
            .arg(&self.config)
            .arg("managed-connect");
        command
    }

    fn invoke(&self, frontend: &Path) -> (bool, Vec<u8>, String) {
        let id = NEXT_INVOCATION.fetch_add(1, Ordering::Relaxed);
        let stdout = self.root.join(format!("connect-{id}.json"));
        let stderr = self.root.join(format!("connect-{id}.stderr"));
        let mut child = self
            .command(frontend)
            .stdin(Stdio::null())
            .stdout(Stdio::from(File::create(&stdout).unwrap()))
            .stderr(Stdio::from(File::create(&stderr).unwrap()))
            .spawn()
            .expect("launch explicitly supplied bundled Bterm");
        let deadline = Instant::now() + Duration::from_secs(45);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            // A timeout preserves the uncertain child and evidence. It never
            // grants permission to kill it or erase a startup fence.
            assert!(
                Instant::now() < deadline,
                "headless startup exceeded bound; child {} preserved, evidence {}",
                child.id(),
                self.root.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        (
            status.success(),
            fs::read(stdout).unwrap(),
            fs::read_to_string(stderr).unwrap(),
        )
    }

    fn connect(&self, frontend: &Path) -> Report {
        let (success, output, diagnostics) = self.invoke(frontend);
        assert!(
            success,
            "managed-connect failed; evidence {}: {diagnostics}",
            self.root.display()
        );
        let report: Report = serde_json::from_slice(&output).expect("managed-connect JSON report");
        assert_eq!(report.schema_version, 1);
        assert_ne!(report.process_id, 0);
        for digest in [
            &report.target_id,
            &report.artifact_set_sha256,
            &report.companion_sha256,
            &report.configuration_id,
            &report.scope_id,
        ] {
            assert_eq!(digest.len(), 64);
            assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
        assert_eq!(report.roots.data_dir, self.installation.join("data/bmux"));
        assert!(report.roots.config_dir.starts_with(&self.installation));
        assert!(report.roots.state_dir.starts_with(&self.installation));
        assert!(report.roots.runtime_dir.is_absolute());
        assert!(
            report.idle_managed,
            "production startup must configure managed idle retirement"
        );
        assert_eq!(
            report.idle_grace_seconds, self.idle_grace_seconds,
            "explicit Bterm companion policy was not selected: {report:?}"
        );
        assert!(
            !report.idle_blocked,
            "headless inspection must create no shells or recovery work"
        );
        self.assert_preserved();
        report
    }

    fn assert_preserved(&self) {
        assert_eq!(
            fs::read(self.installation.join("data/bmux/sentinel")).unwrap(),
            b"durable installation before any managed launch"
        );
        assert_eq!(
            fs::read(self.installation.join("runtime/legacy-evidence")).unwrap(),
            b"legacy runtime evidence is not stale by inference"
        );
        assert_eq!(
            fs::read(self.root.join("state/bterm/data/canonical")).unwrap(),
            b"canonical installation"
        );
    }

    fn await_idle_exit(&self, report: &Report) {
        let deadline = Instant::now() + Duration::from_secs(report.idle_grace_seconds + 20);
        while process_exists(report.process_id) {
            assert!(
                Instant::now() < deadline,
                "managed companion did not retire gracefully: {report:?}; evidence {}",
                self.root.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        self.assert_preserved();
    }
}

fn process_exists(pid: u32) -> bool {
    let pid = i32::try_from(pid).expect("valid platform PID");
    // SAFETY: signal zero only observes existence/permission; it sends no signal.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    assert_eq!(
        error.raw_os_error(),
        Some(libc::ESRCH),
        "cannot prove owned runtime exited: {error}"
    );
    false
}

fn bundle(variable: &str) -> PathBuf {
    let supplied = PathBuf::from(
        std::env::var_os(variable)
            .unwrap_or_else(|| panic!("set {variable} to an explicit complete verified bundle")),
    );
    assert!(supplied.is_absolute(), "{variable} must be absolute");
    let candidates = [
        supplied.clone(),
        supplied.join("Contents/MacOS/bterm"),
        supplied.join("bterm"),
    ];
    let frontend = candidates
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{variable} does not contain a bundled bterm executable"));
    let frontend = frontend.canonicalize().unwrap();
    assert!(
        frontend
            .parent()
            .unwrap()
            .join("bterm-bundle.json")
            .is_file()
    );
    frontend
}

fn assert_same_runtime(first: &Report, second: &Report) {
    assert_eq!(first.target_id, second.target_id);
    assert_eq!(first.instance, second.instance);
    assert_eq!(first.process_id, second.process_id);
    assert_eq!(first.roots.runtime_dir, second.roots.runtime_dir);
}

#[test]
#[ignore = "requires two explicit built immutable Bterm bundles; starts isolated real companions"]
fn two_builds_and_configurations_coexist_reuse_and_preserve_installation() {
    let _scenario = SCENARIO.lock().unwrap();
    let first_binary = bundle("BTERM_TEST_BUNDLE");
    let second_binary = bundle("BTERM_TEST_BUNDLE_SECOND");
    // Allow three cold immutable launch images without synthetic keepalives.
    let fixture = Fixture::with_idle_grace(120);
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| fixture.connect(&first_binary));
        let second = scope.spawn(|| fixture.connect(&second_binary));
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_ne!(
        first.artifact_set_sha256, second.artifact_set_sha256,
        "fixture must supply genuinely different artifact builds"
    );
    assert_ne!(
        first.companion_sha256, second.companion_sha256,
        "two-build evidence requires different companion executable bytes, not only frontend or metadata changes"
    );
    assert_ne!(first.target_id, second.target_id);
    assert_ne!(first.process_id, second.process_id);
    assert_ne!(first.roots.runtime_dir, second.roots.runtime_dir);
    assert_same_runtime(&first, &fixture.connect(&first_binary));
    assert_same_runtime(&second, &fixture.connect(&second_binary));
    assert_eq!(
        first.roots.data_dir, second.roots.data_dir,
        "a rebuild must not select an empty installation"
    );
    // Mutate only BMUX behavior; the authoritative Bterm source retains the
    // fixture's selected companion policy throughout coexistence.
    fs::write(&fixture.config, "[general]\nscrollback_limit = 8124\n").unwrap();
    let changed = fixture.connect(&first_binary);
    assert_ne!(first.configuration_id, changed.configuration_id);
    assert_eq!(
        first.scope_id, changed.scope_id,
        "same roots/source scope, different effective configuration"
    );
    assert_ne!(first.target_id, changed.target_id);
    assert_ne!(first.roots.runtime_dir, changed.roots.runtime_dir);
    assert!(process_exists(first.process_id));
    assert!(process_exists(second.process_id));
    fs::write(&fixture.config, "[general]\nscrollback_limit = 8123\n").unwrap();
    assert_same_runtime(&first, &fixture.connect(&first_binary));
    fixture.await_idle_exit(&first);
    fixture.await_idle_exit(&second);
    fixture.await_idle_exit(&changed);
}

#[test]
#[ignore = "requires an explicit built immutable Bterm bundle; starts an isolated real companion"]
fn concurrent_frontends_reuse_one_authenticated_companion() {
    let _scenario = SCENARIO.lock().unwrap();
    let binary = bundle("BTERM_TEST_BUNDLE");
    let fixture = Fixture::new();
    let reports = std::thread::scope(|scope| {
        let first = scope.spawn(|| fixture.connect(&binary));
        let second = scope.spawn(|| fixture.connect(&binary));
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_same_runtime(&reports.0, &reports.1);
    fixture.await_idle_exit(&reports.0);
}

#[test]
#[ignore = "requires an explicit built immutable Bterm bundle; starts two independently scoped companions"]
fn explicit_installation_scopes_do_not_share_startup_or_endpoints() {
    let _scenario = SCENARIO.lock().unwrap();
    let binary = bundle("BTERM_TEST_BUNDLE");
    let first_fixture = Fixture::new();
    let second_fixture = Fixture::new();
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| first_fixture.connect(&binary));
        let second = scope.spawn(|| second_fixture.connect(&binary));
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(first.artifact_set_sha256, second.artifact_set_sha256);
    assert_ne!(first.scope_id, second.scope_id);
    assert_ne!(first.target_id, second.target_id);
    assert_ne!(first.roots.runtime_dir, second.roots.runtime_dir);
    assert_ne!(first.roots.data_dir, second.roots.data_dir);
    assert_ne!(first.instance, second.instance);
    first_fixture.await_idle_exit(&first);
    second_fixture.await_idle_exit(&second);
}

#[test]
#[ignore = "requires an explicit built immutable Bterm bundle; waits for production managed idle retirement"]
fn idle_retirement_restarts_same_target_without_losing_durable_data() {
    let _scenario = SCENARIO.lock().unwrap();
    let binary = bundle("BTERM_TEST_BUNDLE");
    let fixture = Fixture::new();
    let before = fixture.connect(&binary);
    fixture.await_idle_exit(&before);
    let after = fixture.connect(&binary);
    assert_eq!(before.target_id, after.target_id);
    assert_eq!(before.artifact_set_sha256, after.artifact_set_sha256);
    assert_eq!(before.configuration_id, after.configuration_id);
    assert_eq!(before.roots.runtime_dir, after.roots.runtime_dir);
    assert_ne!(
        before.instance, after.instance,
        "idle restart must be a new authenticated activation"
    );
    fixture.await_idle_exit(&after);
}

#[test]
#[ignore = "requires an explicit built immutable Bterm bundle; rejects a frontend detached from its bundle"]
fn incomplete_bundle_cannot_silently_fall_back_to_another_companion() {
    let _scenario = SCENARIO.lock().unwrap();
    let binary = bundle("BTERM_TEST_BUNDLE");
    let preflight = Fixture::new();
    let ready = preflight.connect(&binary);
    preflight.await_idle_exit(&ready);
    let fixture = Fixture::new();
    let detached = fixture.root.join("bterm");
    fs::copy(&binary, &detached).unwrap();
    // Execute actual frontend bytes with no manifest or sibling companion. The
    // original complete bundle stays untouched, including its live runtimes.
    let (success, output, diagnostics) = fixture.invoke(&detached);
    assert!(
        !success,
        "an incomplete bundle must not select PATH or host BMUX"
    );
    assert!(
        output.is_empty(),
        "an incomplete bundle cannot report live readiness"
    );
    assert!(
        diagnostics.contains("open companion bundle manifest"),
        "expected missing verified bundle classification, not an arbitrary CLI/loader failure: {diagnostics}"
    );
    fixture.assert_preserved();
    assert!(!fixture.installation.join("state/targets").exists());
}

#[test]
#[ignore = "requires an explicit built immutable Bterm bundle; verifies failure without starting a companion"]
fn invalid_configuration_fails_explicitly_without_mutating_legacy_data() {
    let _scenario = SCENARIO.lock().unwrap();
    let binary = bundle("BTERM_TEST_BUNDLE");
    let preflight = Fixture::new();
    let ready = preflight.connect(&binary);
    preflight.await_idle_exit(&ready);
    let fixture = Fixture::new();
    // The explicit Bterm family is authoritative for [bterm] settings;
    // corrupt it rather than a BMUX-family value that it can override.
    fs::write(
        &fixture.bterm_config,
        "[bterm.companion]\nidle_after_seconds = 'invalid'\n",
    )
    .unwrap();
    let (success, output, diagnostics) = fixture.invoke(&binary);
    assert!(
        !success,
        "invalid effective configuration must not become a successful empty installation"
    );
    assert!(
        output.is_empty(),
        "failed diagnostics must not fabricate a readiness report"
    );
    assert!(
        diagnostics.contains("invalid [bterm] configuration (source values redacted)"),
        "expected Bterm schema validation classification, not an arbitrary startup failure: {diagnostics}"
    );
    fixture.assert_preserved();
    assert!(
        !fixture.installation.join("state/targets").exists(),
        "invalid configuration must fail before target activation"
    );
}
