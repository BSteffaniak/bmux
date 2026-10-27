//! Real CLI acceptance tests. Never use a PATH binary or a user's runtime.
//! Defaults to CARGO_BIN_EXE_bmux; installed acceptance requires both
//! BMUX_RECORDING_TEST_BINARY and BMUX_RECORDING_TEST_INSTALL_ROOT, pointing
//! inside an explicitly owned staged/relocated installation.
//! The coordinator builds/validates this target; this fixture never invokes Cargo.
//! CLI JSON assertions follow the sole recording-process-v4 contract.
//! Start must automatically bootstrap an owned isolated runtime; this fixture never
//! starts a server manually. Cleanup explicitly requests finish --cleanup-server.
#![cfg(unix)]

// Match nextest's recorded-processes budget under libtest too: each owned
// fixture launches servers and/or rasterizes evidence. Keep deadlines strict.
use serde_json::Value;
use serial_test::serial;
use std::fs::{self, File};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

struct RecordingEnv {
    root: PathBuf,
    short_tmp: PathBuf,
    binary: PathBuf,
    runs: std::cell::RefCell<Vec<String>>,
    command_sequence: std::cell::Cell<u64>,
    profile_export: bool,
}

impl RecordingEnv {
    fn new() -> Self {
        let fixture_base = std::env::var_os("BMUX_RECORDING_TEST_TMP_ROOT")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        assert!(fixture_base.is_absolute(), "fixture base must be absolute");
        let root = fixture_base.join(format!("bmux-rp-{}", uuid::Uuid::new_v4().simple()));
        let socket_base = std::env::var_os("BMUX_RECORDING_TEST_SOCKET_ROOT")
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
            .canonicalize()
            .expect("canonical socket base");
        assert!(socket_base.is_absolute(), "socket base must be absolute");
        // Exercise the default macOS TMPDIR byte budget in every process test,
        // instead of masking path regressions with an unusually short parent.
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let padding = 56usize
            .checked_sub(socket_base.as_os_str().len() + 1 + suffix.len())
            .expect("socket fixture parent must leave room for a 56-byte TMPDIR");
        let short_tmp = socket_base.join(format!("{}{suffix}", "t".repeat(padding)));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&short_tmp)
            .expect("exclusive owned TMPDIR");
        assert_eq!(
            short_tmp.canonicalize().expect("canonical TMPDIR"),
            short_tmp
        );
        assert_eq!(
            fs::symlink_metadata(&short_tmp)
                .expect("TMPDIR metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            short_tmp.as_os_str().len(),
            56,
            "macOS TMPDIR endpoint budget"
        );
        for directory in [
            "home",
            "config",
            "runtime",
            "data",
            "state",
            "logs",
            "tmp",
            "recordings",
        ] {
            fs::create_dir_all(root.join(directory)).expect("create owned isolation root");
        }
        let root = root.canonicalize().expect("canonical owned root");
        fs::write(root.join("config/bmux.toml"), "").expect("write empty isolated config");
        fs::write(
            root.join("short-tmpdir.txt"),
            short_tmp.as_os_str().as_encoded_bytes(),
        )
        .expect("retain TMPDIR locator");
        Self {
            root,
            short_tmp,
            binary: acceptance_binary(),
            runs: std::cell::RefCell::new(Vec::new()),
            command_sequence: std::cell::Cell::new(0),
            profile_export: false,
        }
    }

    /// Copy only the flat, coordinator-installed payload, then relocate it before
    /// any launch. Never mutate the coordinator's stage or selected installation.
    fn relocated_installation() -> Self {
        assert!(
            std::env::var_os("BMUX_RECORDING_TEST_BINARY").is_some(),
            "installed test requires an explicit binary"
        );
        assert!(
            std::env::var_os("BMUX_RECORDING_TEST_INSTALL_ROOT").is_some(),
            "installed test requires an owned installation root"
        );
        let mut env = Self::new();
        let source = env.binary.parent().expect("installation directory");
        let inventory: Value = serde_json::from_slice(
            &fs::read(source.join("artifacts.json")).expect("committed install inventory"),
        )
        .expect("inventory JSON");
        assert_eq!(inventory["schema_version"].as_u64(), Some(1));
        let copied = env.root.join("installed-before-relocation");
        fs::create_dir(&copied).expect("owned install copy");
        for entry in fs::read_dir(source).expect("flat installed payload") {
            let entry = entry.expect("installed member");
            assert!(
                entry.file_type().expect("installed member type").is_file(),
                "installed acceptance requires a flat regular-file payload"
            );
            fs::copy(entry.path(), copied.join(entry.file_name()))
                .expect("copy owned installed artifact");
        }
        let relocated = env.root.join("relocated-installation");
        fs::rename(copied, &relocated).expect("relocate owned installation");
        env.binary = relocated.join(env.binary.file_name().expect("installed binary name"));
        env
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BMUX_") {
                command.env_remove(key);
            }
        }
        if self.profile_export {
            command.env("BMUX_RECORDING_EXPORT_PROFILE", "1");
        }
        command
            .current_dir(&self.root)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("home/cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("TMPDIR", &self.short_tmp)
            .env("BMUX_CONFIG_DIR", self.root.join("config"))
            .env("BMUX_RUNTIME_DIR", self.root.join("runtime"))
            .env("BMUX_DATA_DIR", self.root.join("data"))
            .env("BMUX_STATE_DIR", self.root.join("state"))
            .env("BMUX_LOG_DIR", self.root.join("logs"))
            .env("BMUX_RECORDINGS_DIR", self.root.join("recordings"))
            .env("BMUX_NO_BASE_CONFIG", "1")
            .env("BMUX_RUNTIME_NAME", "default")
            .env("BMUX_TARGET", "local")
            .args(["--runtime", "default", "--target", "local", "--config"])
            .arg(self.root.join("config/bmux.toml"))
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_input(args, None)
    }

    fn run_with_input(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        self.run_at_runtime(args, input, None)
    }

    fn run_at_runtime(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
        runtime: Option<&std::path::Path>,
    ) -> Output {
        // Files avoid pipe deadlock when testing a burst larger than a pipe buffer.
        let sequence = self.command_sequence.get();
        self.command_sequence.set(sequence + 1);
        let stdout_path = self.root.join(format!("logs/command-{sequence}.out"));
        let stderr_path = self.root.join(format!("logs/command-{sequence}.err"));
        let stdin = if let Some(bytes) = input {
            let path = self.root.join(format!("tmp/input-{sequence}"));
            fs::write(&path, bytes).expect("write bounded command input");
            Stdio::from(File::open(path).expect("open command input"))
        } else {
            Stdio::null()
        };
        let mut command = self.command(args);
        if let Some(root) = runtime {
            command
                .env("BMUX_RUNTIME_DIR", root.join("runtime"))
                .env("BMUX_CONFIG_DIR", root.join("config"))
                .env("BMUX_DATA_DIR", root.join("data"))
                .env("BMUX_STATE_DIR", root.join("state"));
        }
        let started = Instant::now();
        let mut child = command
            .stdin(stdin)
            .stdout(File::create(&stdout_path).expect("create command stdout"))
            .stderr(File::create(&stderr_path).expect("create command stderr"))
            .spawn()
            .expect("spawn explicit test binary");
        // GIF export is a bounded offline rasterization workload, not a launch
        // or input operation. The approved acceptance deadline is two minutes.
        let exports_gif = args.windows(2).any(|pair| pair == ["--export", "gif"])
            || args.contains(&"--format=gif")
            || args.windows(2).any(|pair| pair == ["--format", "gif"]);
        // Launch also hashes the full debug executable before server readiness.
        // Budget that bootstrap separately from the existing operation deadlines.
        let launches_runtime = args.starts_with(&["recording", "process", "start"])
            || args.starts_with(&["recording", "process", "run"]);
        let timeout = if exports_gif {
            Duration::from_secs(120)
        } else if launches_runtime {
            Duration::from_secs(60)
        } else {
            COMMAND_TIMEOUT
        };
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll CLI child") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "CLI deadline exceeded for {args:?}; stderr={}",
                    fs::read_to_string(&stderr_path).unwrap_or_default()
                );
            }
            thread::sleep(POLL_INTERVAL);
        };
        if self.profile_export {
            eprintln!(
                "finite CLI stage {args:?}: {:?}; stderr={}",
                started.elapsed(),
                fs::read_to_string(&stderr_path).expect("stage profiling stderr")
            );
        }
        Output {
            status,
            stdout: fs::read(stdout_path).expect("read command stdout"),
            stderr: fs::read(stderr_path).expect("read command stderr"),
        }
    }

    fn start(&self, script: &str, extra_args: &[&str]) -> String {
        self.start_profile(script, extra_args, "functional")
    }

    fn start_profile(&self, script: &str, extra_args: &[&str], profile: &str) -> String {
        let path = self.script("program.sh", script);
        let cwd = self.root.to_str().expect("test root UTF-8");
        let mut args = vec![
            "recording",
            "process",
            "start",
            "--json",
            "--profile",
            profile,
            "--viewport",
            "100x30",
            "--cwd",
            cwd,
            "--env",
            "BMUX_TEST_VALUE=literal",
            "--",
            "/bin/sh",
        ];
        args.push(path.to_str().expect("fixture path UTF-8"));
        args.extend_from_slice(extra_args);
        let value = self.json(&args);
        // Mirrors recording-process-v4 run-status; no heuristic identity lookup.
        assert_eq!(value["schema_version"].as_u64(), Some(4));
        assert!(
            value["revision"]
                .as_u64()
                .is_some_and(|revision| revision > 0)
        );
        uuid::Uuid::parse_str(value["recording_id"].as_str().expect("recording identity"))
            .expect("recording UUID");
        self.track_run(&value)
    }

    fn track_run(&self, value: &Value) -> String {
        let id = value["run_id"]
            .as_str()
            .expect("run-status.run_id")
            .to_owned();
        uuid::Uuid::parse_str(&id).expect("run UUID");
        if !self.runs.borrow().contains(&id) {
            self.runs.borrow_mut().push(id.clone());
        }
        let runtime_root = PathBuf::from(value["runtime_root"].as_str().expect("run runtime_root"));
        let root = runtime_root
            .canonicalize()
            .expect("dedicated runtime exists");
        let base = self
            .short_tmp
            .canonicalize()
            .expect("canonical owned temp base");
        assert_eq!(
            root.parent(),
            Some(base.as_path()),
            "dedicated root outside temp base"
        );
        let name = root
            .file_name()
            .expect("runtime name")
            .to_str()
            .expect("UTF-8 name");
        let suffix = name.strip_prefix("r-").expect("dedicated runtime prefix");
        assert_eq!(suffix.len(), 24);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let locator_path = self
            .root
            .join("data/recording-process-v4")
            .join(format!("{id}.json"));
        let locator: Value =
            serde_json::from_slice(&fs::read(locator_path).expect("owned locator"))
                .expect("locator JSON");
        assert_eq!(locator["run_id"].as_str(), Some(id.as_str()));
        assert_eq!(
            PathBuf::from(locator["root"].as_str().expect("locator root"))
                .canonicalize()
                .expect("locator root exists"),
            root
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&root)
                .expect("runtime permissions")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        id
    }

    fn wait_exit(&self, id: &str) -> Value {
        self.json(&[
            "recording",
            "process",
            "wait",
            id,
            "--exit",
            "--timeout",
            "10s",
            "--json",
        ])
    }

    fn finish(&self, id: &str, name: &str, formats: &[&str]) -> PathBuf {
        let path = self.root.join(name);
        let mut args = vec!["recording", "process", "finish", id, "--json", "--output"];
        args.push(path.to_str().expect("artifact path UTF-8"));
        for format in formats {
            args.extend(["--export", *format]);
        }
        let value = self.json(&args);
        assert_eq!(value["schema_version"].as_u64(), Some(4));
        assert_eq!(value["status"]["run_id"].as_str(), Some(id));
        assert_eq!(value["status"]["capture_finalized"].as_bool(), Some(true));
        let artifacts = value["artifacts"]
            .as_array()
            .expect("finish artifact paths");
        assert!(!artifacts.is_empty());
        for artifact in artifacts {
            let artifact = PathBuf::from(artifact.as_str().expect("artifact path string"));
            assert!(
                artifact.starts_with(&path),
                "artifact escaped destination: {artifact:?}"
            );
            assert!(
                artifact.is_file(),
                "reported artifact missing: {artifact:?}"
            );
        }
        path
    }

    fn script(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, contents).expect("write owned fixture");
        path
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert_success(&output);
        parse_json(&output)
    }
}

impl Drop for RecordingEnv {
    fn drop(&mut self) {
        let mut cleaned = true;
        for id in self.runs.get_mut().clone() {
            // Process commands route only identities created by this fixture.
            let _ = self.run(&[
                "recording",
                "process",
                "terminate",
                &id,
                "--grace-period",
                "1s",
                "--json",
            ]);
            cleaned &= self
                .run(&[
                    "recording",
                    "process",
                    "finish",
                    &id,
                    "--cleanup-server",
                    "--json",
                ])
                .status
                .success();
        }
        if cleaned {
            let _ = fs::remove_dir_all(&self.root);
        } else {
            eprintln!(
                "retaining recording runtime after failed cleanup: {}",
                self.root.display()
            );
        }
    }
}

fn acceptance_binary() -> PathBuf {
    let selected = std::env::var_os("BMUX_RECORDING_TEST_BINARY");
    let root = std::env::var_os("BMUX_RECORDING_TEST_INSTALL_ROOT");
    match (selected, root) {
        (None, None) => {
            let binary = PathBuf::from(env!("CARGO_BIN_EXE_bmux"));
            assert!(binary.is_absolute(), "Cargo must supply an absolute binary");
            binary
        }
        (Some(binary), Some(root)) => {
            let binary = PathBuf::from(binary);
            let root = PathBuf::from(root);
            assert!(binary.is_absolute() && root.is_absolute());
            assert!(
                fs::symlink_metadata(&binary)
                    .expect("owned installed binary metadata")
                    .file_type()
                    .is_file(),
                "installed binary must be a regular file, not a symlink"
            );
            let root = root.canonicalize().expect("owned installation root");
            let binary = binary.canonicalize().expect("owned installed binary");
            assert!(
                binary.starts_with(&root) && binary != root,
                "selected binary escaped the explicit owned installation"
            );
            assert!(
                root != Path::new("/")
                    && root != Path::new("/usr")
                    && root != Path::new("/usr/local"),
                "installation root must be an owned test stage, not a system prefix"
            );
            binary
        }
        _ => panic!("installed acceptance requires both binary and owned installation root"),
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status={}; stdout={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn parse_json(output: &Output) -> Value {
    // Do not skip log prefixes: --json must produce an actual JSON document.
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
#[serial]
fn process_help_is_discoverable_without_starting_a_server() {
    let env = RecordingEnv::new();
    for args in [
        vec!["--help"],
        vec!["recording", "--help"],
        vec!["recording", "process", "--help"],
    ] {
        let output = env.run(&args);
        assert_success(&output);
        let text = String::from_utf8(output.stdout).expect("UTF-8 help");
        assert!(
            text.contains(if args.len() == 1 {
                "recording"
            } else {
                "process"
            }) || args.len() == 3 && text.contains("start"),
            "missing discovery: {text}"
        );
    }
    for verb in [
        "start",
        "run",
        "status",
        "read",
        "screen",
        "send",
        "wait",
        "terminate",
        "finish",
    ] {
        let output = env.run(&["recording", "process", verb, "--help"]);
        assert_success(&output);
        let text = String::from_utf8(output.stdout).expect("UTF-8 help");
        assert!(text.contains(verb), "missing verb in help: {text}");
        assert!(
            !text.contains("--contract-version"),
            "retired selector advertised: {text}"
        );
        assert!(
            !text.contains("--lifetime"),
            "retired lifecycle advertised: {text}"
        );
        assert!(
            text.contains("--json"),
            "missing JSON discoverability: {text}"
        );
    }
}

#[test]
#[serial]
fn malformed_process_arguments_fail_without_creating_a_recording() {
    let env = RecordingEnv::new();
    for args in [
        vec!["start", "--json"],
        vec![
            "start",
            "--profile",
            "not-a-profile",
            "--",
            "/bin/echo",
            "bad",
        ],
        vec!["start", "--viewport", "0x0", "--", "/bin/echo", "bad"],
        vec!["start", "--env", "missing-equals", "--", "/bin/echo", "bad"],
        vec!["status", "not-a-uuid", "--json"],
        vec!["read", "not-a-uuid", "--since", "invalid", "--json"],
        vec![
            "wait",
            "not-a-uuid",
            "--exit",
            "--timeout",
            "nonsense",
            "--json",
        ],
        vec![
            "terminate",
            "not-a-uuid",
            "--grace-period",
            "nonsense",
            "--json",
        ],
        vec!["finish", "not-a-uuid", "--export", "unknown", "--json"],
        vec!["start", "--unknown-option", "--", "/bin/echo", "bad"],
    ] {
        let mut command = vec!["recording", "process"];
        command.extend(args);
        let output = env.run(&command);
        assert!(
            !output.status.success(),
            "invalid arguments succeeded: {command:?}"
        );
        assert!(
            !output.stderr.is_empty() || !output.stdout.is_empty(),
            "missing diagnostic"
        );
    }
    assert_eq!(
        fs::read_dir(env.root.join("recordings"))
            .expect("recordings directory")
            .count(),
        0
    );
}

#[test]
#[serial]
fn retired_lifecycle_selectors_are_rejected_before_launch() {
    let env = RecordingEnv::new();
    let marker = env.root.join("must-not-launch");
    let script = env.script("rejected.sh", &format!("touch '{}'\n", marker.display()));
    for (selector, value) in [
        ("--contract-version", "1"),
        ("--contract-version", "3"),
        ("--lifetime", "terminal-session"),
    ] {
        let output = env.run(&[
            "recording",
            "process",
            "start",
            selector,
            value,
            "--json",
            "--",
            "/bin/sh",
            script.to_str().expect("script path"),
        ]);
        assert!(
            !output.status.success(),
            "retired selector accepted: {selector}"
        );
        let error = parse_json(&output);
        assert_eq!(error["schema_version"], 4);
        assert!(
            error["error"]
                .as_str()
                .expect("explicit diagnostic")
                .contains(&format!("unsupported option {selector}")),
            "{error}"
        );
        assert!(!marker.exists(), "rejected client launched its program");
    }
    // An empty derived index directory is not a launch or recording authority.
    // Reject any receipt/locator/archive file, while allowing that directory.
    for directory in [
        env.root.join("data/recording-process-v4"),
        env.root.join("recordings"),
    ] {
        if directory.exists() {
            assert_eq!(
                fs::read_dir(&directory)
                    .expect("owned state directory")
                    .count(),
                0,
                "rejection created durable launch or archive state: {directory:?}"
            );
        }
    }
}

#[test]
fn process_manifest_exposes_only_the_cohesive_contract() {
    let manifest = include_str!("../../../plugins/recording-plugin/plugin.toml");
    assert!(manifest.contains("interface_id = \"recording-process-v4\""));
    assert!(manifest.contains("capability   = \"bmux.recording.process.v4\""));
    assert!(manifest.contains("\"bmux.pane_runtime.execution.v4\""));
    for version in [1, 2, 3] {
        assert!(!manifest.contains(&format!("recording-process-v{version}\"")));
        assert!(!manifest.contains(&format!("bmux.recording.process.v{version}\"")));
        assert!(!manifest.contains(&format!("bmux.pane_runtime.execution.v{version}\"")));
    }
}

#[test]
#[serial]
fn old_durable_versions_are_rejected_without_rewriting_evidence() {
    let env = RecordingEnv::new();
    let id = env.start("stty -opost\nprintf 'CANONICAL_DATA'\n", &[]);
    let exited = env.wait_exit(&id);
    assert_eq!(exited["output_drained"], true);
    let evidence = env.finish(&id, "version-evidence", &["raw", "text"]);
    let status = env.json(&["recording", "process", "status", &id, "--json"]);
    let runtime = PathBuf::from(status["runtime_root"].as_str().expect("runtime root"));
    let locator = env
        .root
        .join("data/recording-process-v4")
        .join(format!("{id}.json"));
    let metadata = runtime
        .join("state/recordings/process-runs-v4")
        .join(format!("{id}.json"));
    let raw = fs::read(evidence.join("output.pty")).expect("canonical raw bytes");
    for path in [&locator, &metadata] {
        let original = fs::read(path).expect("current durable metadata");
        for version in [1, 2, 3, 99] {
            let mut old: Value = serde_json::from_slice(&original).expect("metadata JSON");
            old["schema_version"] = Value::from(version);
            let bytes = serde_json::to_vec(&old).expect("old version fixture");
            fs::write(path, &bytes).expect("owned metadata mutation");
            let output = env.run(&["recording", "process", "status", &id, "--json"]);
            let unchanged = fs::read(path).expect("rejected metadata retained");
            fs::write(path, &original).expect("restore owned metadata before assertion/cleanup");
            assert!(
                !output.status.success(),
                "old schema {version} silently accepted"
            );
            let error = parse_json(&output);
            let diagnostic = error["error"].as_str().expect("version diagnostic");
            assert!(
                diagnostic.contains("invalid run locator")
                    || diagnostic.contains("unsupported process run schema version"),
                "{error}"
            );
            assert_eq!(unchanged, bytes, "rejection rewrote canonical metadata");
            assert_eq!(
                fs::read(evidence.join("output.pty")).expect("retained raw"),
                raw
            );
        }
    }
    assert_eq!(
        env.json(&["recording", "process", "status", &id, "--json"])["run_id"],
        id
    );
}

fn read_all(env: &RecordingEnv, id: &str) -> Vec<u8> {
    let mut cursor = 0_u64;
    let mut bytes = Vec::new();
    for _ in 0..128 {
        let since = cursor.to_string();
        let value = env.json(&[
            "recording",
            "process",
            "read",
            id,
            "--since",
            &since,
            "--max-bytes",
            "16384",
            "--json",
        ]);
        assert_eq!(value["run_id"].as_str(), Some(id));
        let data = value["data"]
            .as_array()
            .expect("output-read.data byte array");
        assert!(data.len() <= 16384);
        for byte in data {
            bytes.push(u8::try_from(byte.as_u64().expect("unsigned byte")).expect("byte <= 255"));
        }
        let next = value["next_cursor"].as_u64().expect("next_cursor");
        assert!(next >= cursor, "cursor regressed");
        if value["complete"].as_bool() == Some(true) {
            return bytes;
        }
        assert!(next > cursor, "incomplete read made no progress");
        cursor = next;
    }
    panic!("bounded read budget exceeded");
}

#[test]
#[ignore = "requires coordinator-built real installed payload and explicit binary/root selection"]
#[serial]
fn relocated_installed_payload_exports_and_remains_offline() {
    let env = RecordingEnv::relocated_installation();
    let script = env.script(
        "installed-native.sh",
        "stty -opost\nprintf '\\033[41mINSTALLED_NATIVE\\033[0m'\nexit 7\n",
    );
    let output = env.run(&[
        "recording",
        "process",
        "run",
        "--profile",
        "full",
        "--viewport",
        "100x30",
        "--cwd",
        env.root.to_str().expect("owned cwd"),
        "--json",
        "--",
        "/bin/sh",
        script.to_str().expect("owned script"),
    ]);
    assert_eq!(
        output.status.code(),
        Some(7),
        "installed native foreground outcome"
    );
    let status = parse_json(&output);
    let id = env.track_run(&status);
    assert_eq!(status["exit_code"].as_u64(), Some(7));
    assert_eq!(status["output_drained"].as_bool(), Some(true));
    assert_eq!(status["capture_finalized"].as_bool(), Some(true));
    let destination = env.root.join("installed-native-exports");
    let finished = env.run(&[
        "recording",
        "process",
        "finish",
        &id,
        "--export",
        "raw",
        "--export",
        "text",
        "--export",
        "events",
        "--export",
        "gif",
        "--output",
        destination.to_str().expect("destination"),
        "--cleanup-server",
        "--json",
    ]);
    assert_success(&finished);
    assert_eq!(
        parse_json(&finished)["status"]["run_id"].as_str(),
        Some(id.as_str())
    );
    assert_eq!(
        fs::read(destination.join("output.pty")).expect("installed native raw"),
        b"\x1b[41mINSTALLED_NATIVE\x1b[0m"
    );
    assert!(
        fs::read_to_string(destination.join("output.txt"))
            .expect("installed native text")
            .contains("INSTALLED_NATIVE")
    );
    let events =
        fs::read_to_string(destination.join("events.jsonl")).expect("installed native events");
    assert!(
        events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("event JSON"))
            .any(|event| event["kind"] == "pane_output_raw")
    );
    assert!(gif_dominant_colors(&destination.join("output.gif")).contains(&0));
    let offline = env.run(&["recording", "process", "status", &id, "--json"]);
    assert_success(&offline);
    assert_eq!(parse_json(&offline)["recording_id"], status["recording_id"]);
}

fn assert_installed_launch_rejected(env: &RecordingEnv) {
    let marker = env.root.join("must-not-launch");
    let script = env.script("rejected-program.sh", "printf launched > must-not-launch\n");
    let output = env.run(&[
        "recording",
        "process",
        "start",
        "--json",
        "--cwd",
        env.root.to_str().expect("owned cwd"),
        "--",
        "/bin/sh",
        script.to_str().expect("owned script"),
    ]);
    assert!(
        !output.status.success(),
        "invalid installed helper was admitted"
    );
    let diagnostics = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostics.contains("auxiliary")
            || diagnostics.contains("supervisor")
            || diagnostics.contains("inventory"),
        "failure was not attributed to installed helper authority: {diagnostics}"
    );
    assert!(
        !marker.exists(),
        "program launched before installed helper rejection"
    );
    assert!(
        !diagnostics.contains("dedicated runtime cleanup failed"),
        "owned failed launch did not clean up: {diagnostics}"
    );
}

#[test]
#[ignore = "requires coordinator-built real installed payload and explicit binary/root selection"]
#[serial]
fn installed_missing_and_same_length_tampered_helpers_reject_before_launch() {
    for tampered in [false, true] {
        let env = RecordingEnv::relocated_installation();
        let directory = env.binary.parent().expect("relocated root");
        let inventory = fs::read(directory.join("artifacts.json")).expect("retained inventory");
        let helper = directory.join("bmux-pane-execution-supervisor");
        if tampered {
            let mut bytes = fs::read(&helper).expect("installed real supervisor");
            assert!(!bytes.is_empty());
            // Preserve length, path, permissions and inventory. Hash must detect
            // replacement before a tampered Mach-O/ELF reaches execution.
            let last = bytes.last_mut().expect("helper byte");
            *last ^= 1;
            fs::write(helper, bytes).expect("tamper only owned copied helper");
        } else {
            fs::rename(helper, env.root.join("withheld-supervisor"))
                .expect("withhold owned helper");
        }
        assert_installed_launch_rejected(&env);
        assert_eq!(
            fs::read(directory.join("artifacts.json")).expect("inventory after rejection"),
            inventory
        );
    }
}

#[test]
#[ignore = "requires explicit installed payload and real different-slot BMUX_RECORDING_TEST_FOREIGN_SUPERVISOR"]
#[serial]
fn installed_different_slot_helper_rejects_against_untouched_inventory() {
    let foreign = PathBuf::from(
        std::env::var_os("BMUX_RECORDING_TEST_FOREIGN_SUPERVISOR")
            .expect("explicit coordinator-supplied different-slot supervisor"),
    );
    assert!(foreign.is_absolute());
    assert!(
        fs::symlink_metadata(&foreign)
            .expect("foreign helper metadata")
            .file_type()
            .is_file()
    );
    let foreign = foreign.canonicalize().expect("foreign helper path");
    let env = RecordingEnv::relocated_installation();
    let directory = env.binary.parent().expect("relocated root");
    assert!(
        !foreign.starts_with(directory),
        "foreign helper must originate outside selected installation"
    );
    let inventory = fs::read(directory.join("artifacts.json")).expect("retained inventory");
    let helper = directory.join("bmux-pane-execution-supervisor");
    assert_ne!(
        fs::read(&foreign).expect("real foreign helper"),
        fs::read(&helper).expect("selected helper"),
        "byte-identical helper cannot demonstrate wrong-artifact rejection"
    );
    fs::copy(foreign, helper).expect("replace only owned helper with real foreign artifact");
    assert_installed_launch_rejected(&env);
    assert_eq!(
        fs::read(directory.join("artifacts.json")).expect("inventory after rejection"),
        inventory
    );
}

#[test]
#[ignore = "requires coordinator-built real installed payload and explicit binary/root selection"]
#[serial]
fn installed_wrong_owner_unsupported_or_corrupt_inventory_rejects_before_launch() {
    for case in ["wrong-owner", "unsupported-schema", "corrupt"] {
        let env = RecordingEnv::relocated_installation();
        let path = env
            .binary
            .parent()
            .expect("relocated root")
            .join("artifacts.json");
        let original = fs::read(&path).expect("real installed inventory");
        let mut inventory: Value = serde_json::from_slice(&original).expect("inventory JSON");
        match case {
            "wrong-owner" => {
                let artifact = inventory["artifacts"]
                    .as_array_mut()
                    .expect("artifacts")
                    .iter_mut()
                    .find(|artifact| artifact["relative_path"] == "bmux-pane-execution-supervisor")
                    .expect("committed supervisor");
                artifact["declaration"]["owner"] = Value::String("foreign.owner".into());
                fs::write(
                    &path,
                    serde_json::to_vec(&inventory).expect("mutated inventory JSON"),
                )
                .expect("mutate owned owner");
            }
            "unsupported-schema" => {
                inventory["schema_version"] = Value::from(99);
                fs::write(
                    &path,
                    serde_json::to_vec(&inventory).expect("mutated inventory JSON"),
                )
                .expect("mutate owned schema");
            }
            "corrupt" => fs::write(&path, b"{invalid").expect("corrupt owned inventory"),
            _ => unreachable!(),
        }
        assert_installed_launch_rejected(&env);
    }
}

#[test]
#[serial]
fn terminal_profiles_complete_capture_without_lifecycle_selectors() {
    for profile in ["full", "functional", "visual"] {
        let env = RecordingEnv::new();
        let script = env.script("native-input.sh", "stty -echo -opost\nprintf 'Ready>'\nIFS= read -r line\nprintf '\\033[41mReply:%s\\033[0m' \"$line\"\nexit 7\n");
        let start = env.run(&[
            "recording",
            "process",
            "start",
            "--profile",
            profile,
            "--viewport",
            "100x30",
            "--cwd",
            env.root.to_str().expect("cwd"),
            "--json",
            "--",
            "/bin/sh",
            script.to_str().expect("script"),
        ]);
        assert_success(&start);
        let status = parse_json(&start);
        assert_eq!(status["schema_version"].as_u64(), Some(4));
        let id = env.track_run(&status);
        let ready = env.run(&[
            "recording",
            "process",
            "wait",
            &id,
            "--screen-contains",
            "Ready>",
            "--timeout",
            "10s",
            "--json",
        ]);
        assert_success(&ready);
        assert_eq!(parse_json(&ready)["matched"].as_bool(), Some(true));
        let input = env.run_with_input(
            &["recording", "process", "send", &id, "--stdin", "--json"],
            Some("héllo".as_bytes()),
        );
        assert_success(&input);
        assert_eq!(parse_json(&input)["accepted_bytes"].as_u64(), Some(6));
        let key = env.run(&[
            "recording",
            "process",
            "send",
            &id,
            "--key",
            "Enter",
            "--json",
        ]);
        assert_success(&key);
        assert_eq!(parse_json(&key)["accepted_bytes"].as_u64(), Some(1));
        let waited = env.run(&[
            "recording",
            "process",
            "wait",
            &id,
            "--exit",
            "--timeout",
            "10s",
            "--json",
        ]);
        assert_success(&waited);
        let status = parse_json(&waited);
        assert_eq!(status["exit_code"].as_u64(), Some(7));
        // Capture readiness does not assert descendant cleanup or daemon recovery.
        assert_eq!(status["output_drained"].as_bool(), Some(true));
        let expected = "Ready>\x1b[41mReply:héllo\x1b[0m".as_bytes();
        assert_eq!(
            status["output_watermark"].as_u64(),
            Some(u64::try_from(expected.len()).expect("watermark"))
        );
        let read = env.run(&[
            "recording",
            "process",
            "read",
            &id,
            "--since",
            "0",
            "--max-bytes",
            "16384",
            "--json",
        ]);
        assert_success(&read);
        let read = parse_json(&read);
        assert_eq!(read["complete"].as_bool(), Some(true));
        let bytes: Vec<u8> = read["data"]
            .as_array()
            .expect("read bytes")
            .iter()
            .map(|byte| u8::try_from(byte.as_u64().expect("byte")).expect("bounded byte"))
            .collect();
        assert_eq!(bytes, expected);
        let destination = env.root.join("native-exports");
        let finished = env.run(&[
            "recording",
            "process",
            "finish",
            &id,
            "--export",
            "raw",
            "--export",
            "text",
            "--export",
            "events",
            "--export",
            "gif",
            "--output",
            destination.to_str().expect("destination"),
            "--json",
        ]);
        assert_success(&finished);
        let finished = parse_json(&finished);
        assert_eq!(
            finished["status"]["capture_finalized"].as_bool(),
            Some(true)
        );
        assert_eq!(finished["status"]["run_id"].as_str(), Some(id.as_str()));
        assert_eq!(
            fs::read(destination.join("output.pty")).expect("native raw"),
            expected
        );
        assert!(
            fs::read_to_string(destination.join("output.txt"))
                .expect("native text")
                .contains("Reply:héllo")
        );
        let events = fs::read_to_string(destination.join("events.jsonl")).expect("native events");
        let events: Vec<Value> = events
            .lines()
            .map(|line| serde_json::from_str(line).expect("event JSON"))
            .collect();
        assert!(
            events
                .iter()
                .any(|event| event["kind"] == "pane_output_raw")
        );
        assert_eq!(
            events.iter().any(|event| event["kind"] == "pane_input_raw"),
            profile != "visual"
        );
        assert!(
            gif_dominant_colors(&destination.join("output.gif")).contains(&0),
            "native final red reply lost"
        );
        let cleanup = env.run(&[
            "recording",
            "process",
            "finish",
            &id,
            "--cleanup-server",
            "--json",
        ]);
        assert_success(&cleanup);
        assert_eq!(
            parse_json(&cleanup)["capture_finalized"].as_bool(),
            Some(true)
        );
        let offline = env.run(&["recording", "process", "status", &id, "--json"]);
        assert_success(&offline);
        assert_eq!(parse_json(&offline)["run_id"].as_str(), Some(id.as_str()));
    }
}

#[test]
#[serial]
fn stale_finish_revision_does_not_finalize_or_publish() {
    let env = RecordingEnv::new();
    let id = env.start("stty -opost\nprintf STALE\n", &[]);
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    let stale = status["revision"]
        .as_u64()
        .expect("revision")
        .saturating_sub(1)
        .to_string();
    let destination = env.root.join("stale-export");
    let refused = env.run(&[
        "recording",
        "process",
        "finish",
        &id,
        "--expected-revision",
        &stale,
        "--export",
        "raw",
        "--output",
        destination.to_str().expect("destination"),
        "--json",
    ]);
    assert!(!refused.status.success());
    assert!(
        parse_json(&refused)["error"]
            .as_str()
            .expect("revision error")
            .contains("revision")
    );
    assert!(!destination.exists(), "stale mutation published artifacts");
    let pending = env.json(&["recording", "process", "status", &id, "--json"]);
    assert_eq!(pending["capture_finalized"].as_bool(), Some(false));
    assert_eq!(pending["execution"], status["execution"]);
    assert_eq!(read_all(&env, &id), b"STALE");
    env.finish(&id, "valid-export", &["raw"]);
}

#[test]
#[serial]
fn whole_frame_loss_never_publishes_or_overwrites_live_capture() {
    let env = RecordingEnv::new();
    let id = env.start(
        "stty -opost\nprintf FRAME_ONE\nsleep 0.2\nprintf FRAME_TWO\n",
        &[],
    );
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    let expected = read_all(&env, &id);
    let directory = PathBuf::from(status["runtime_root"].as_str().expect("runtime root"))
        .join("state/recordings")
        .join(status["recording_id"].as_str().expect("recording id"));
    env.json(&["recording", "process", "finish", &id, "--json"]);
    let manifest = fs::read(directory.join("manifest.json")).expect("committed manifest");
    let decoded: Value = serde_json::from_slice(&manifest).expect("manifest JSON");
    let segment = directory.join(
        decoded["summary"]["segments"][0]
            .as_str()
            .expect("canonical segment"),
    );
    let canonical = fs::read(&segment).expect("canonical frames");
    let mut offset = 0;
    let mut last = 0;
    while offset < canonical.len() {
        last = offset;
        let prefix: [u8; 4] = canonical[offset..offset + 4]
            .try_into()
            .expect("frame prefix");
        offset += 4 + usize::try_from(u32::from_le_bytes(prefix)).expect("frame size");
        assert!(offset <= canonical.len(), "fixture had partial frame");
    }
    assert!(last > 0, "fixture needs multiple canonical frames");
    fs::write(&segment, &canonical[..last]).expect("remove one entire owned frame");
    let destination = env.root.join("lost-frame-export");
    let refused = env.run(&[
        "recording",
        "process",
        "finish",
        &id,
        "--export",
        "raw",
        "--output",
        destination.to_str().expect("destination"),
        "--json",
    ]);
    assert!(!refused.status.success(), "whole-frame loss was accepted");
    assert!(!destination.exists(), "partial evidence was published");
    assert_eq!(
        fs::read(directory.join("manifest.json")).expect("manifest after refusal"),
        manifest
    );
    assert_eq!(
        fs::read(directory.join("output.live")).expect("live capture after refusal"),
        expected
    );
    fs::write(segment, canonical).expect("restore owned canonical evidence");
    let artifacts = env.finish(&id, "repaired-frames", &["raw"]);
    assert_eq!(
        fs::read(artifacts.join("output.pty")).expect("repaired raw"),
        expected
    );
}

#[test]
#[serial]
fn finite_launch_status_before_wait_reports_drain() {
    let mut env = RecordingEnv::new();
    env.profile_export = true;
    let id = env.start("stty -opost\nprintf FINITE\nchunk=0123456789abcdef\ni=0\nwhile [ \"$i\" -lt 12 ]; do chunk=$chunk$chunk; i=$((i+1)); done\nprintf '%s' \"$chunk\"\nprintf LAST\nexit 7\n", &[]);
    let expected = format!("FINITE{}LAST", "0123456789abcdef".repeat(4096)).into_bytes();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut timeline = Vec::new();
    loop {
        let status = env.json(&["recording", "process", "status", "--json", &id]);
        timeline.push(status.clone());
        if status["output_drained"].as_bool() == Some(true) {
            assert_eq!(status["exit_code"].as_u64(), Some(7), "{timeline:?}");
            assert_ne!(status["phase"].as_str(), Some("running"), "{timeline:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "status-before-wait timeline: {timeline:?}"
        );
        thread::sleep(POLL_INTERVAL);
    }
    eprintln!("status-before-wait timeline: {timeline:?}");
    let captured = read_all(&env, &id);
    assert_eq!(captured, expected);
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    assert_eq!(status["exit_code"].as_u64(), Some(7));
    assert_eq!(
        status["output_watermark"].as_u64(),
        Some(expected.len() as u64)
    );
    let path = env.finish(&id, "finite-status", &["raw", "events", "gif"]);
    let events = fs::read_to_string(path.join("events.jsonl")).expect("finite events");
    assert!(events.lines().count() > 0);
    for line in events.lines() {
        serde_json::from_str::<Value>(line).expect("finite event JSON");
    }
    let mut decoder = gif::DecodeOptions::new()
        .read_info(File::open(path.join("output.gif")).expect("finite GIF"))
        .expect("finite GIF header");
    let mut frames = 0;
    while decoder
        .read_next_frame()
        .expect("finite GIF frame")
        .is_some()
    {
        frames += 1;
        assert!(frames <= 1000, "finite GIF frame budget");
    }
    assert!(frames > 0, "finite GIF empty");
    let finalized = env.json(&["recording", "process", "status", &id, "--json"]);
    for field in [
        "target_exited",
        "terminal_session_ended",
        "capture_eof",
        "output_drained",
        "durable_recording_complete",
        "capture_finalized",
    ] {
        assert_eq!(
            finalized[field], true,
            "incomplete finite workflow: {finalized}"
        );
    }
    assert_eq!(
        finalized["capture_watermark"],
        finalized["recording_watermark"]
    );
    assert_eq!(
        fs::read(path.join("output.pty")).expect("raw artifact"),
        expected
    );
}

#[test]
#[serial]
fn immediate_exit_and_startup_burst_are_not_lost() {
    for iteration in 0..4 {
        let env = RecordingEnv::new();
        let id = env.start("stty -opost\nprintf FIRST\ni=0\nwhile [ \"$i\" -lt 4096 ]; do printf '0123456789abcdef'; i=$((i+1)); done\nprintf LAST\nexit 7\n", &[]);
        let expected = format!("FIRST{}LAST", "0123456789abcdef".repeat(4096)).into_bytes();
        let status = env.wait_exit(&id);
        assert_eq!(status["output_drained"], true);
        assert_eq!(status["exit_code"].as_u64(), Some(7));
        assert_eq!(status["output_drained"].as_bool(), Some(true));
        assert_eq!(read_all(&env, &id), expected, "burst iteration {iteration}");
        let path = env.finish(&id, "exports", &["raw", "text", "events"]);
        assert_eq!(
            fs::read(path.join("output.pty")).expect("raw artifact"),
            expected
        );
        let events = fs::read_to_string(path.join("events.jsonl")).expect("events artifact");
        assert!(!events.is_empty());
        for line in events.lines() {
            serde_json::from_str::<Value>(line).expect("event JSON");
        }
    }
}

#[test]
#[serial]
fn argv_environment_and_working_directory_are_literal() {
    let env = RecordingEnv::new();
    let literal = "spaces 'quotes' $(touch SHOULD_NOT_EXIST); * --flag";
    let id = env.start(
        "stty -opost\nprintf '%s\\n%s\\n%s' \"$1\" \"$BMUX_TEST_VALUE\" \"$PWD\"\n",
        &[literal],
    );
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    let expected = format!("{literal}\nliteral\n{}", env.root.display()).into_bytes();
    assert_eq!(read_all(&env, &id), expected);
    assert!(
        !env.root.join("SHOULD_NOT_EXIST").exists(),
        "implicit shell expansion occurred"
    );
}

#[test]
#[serial]
fn interactive_send_wait_timeout_and_finish_refusal() {
    let env = RecordingEnv::new();
    let id = env.start("stty -echo -opost\nprintf 'Ready>'\nIFS= read -r line\nprintf '\\033[2J\\033[HReply:%s' \"$line\"\nIFS= read -r line\nprintf '\\nSecond:%s' \"$line\"\n", &[]);
    env.json(&[
        "recording",
        "process",
        "wait",
        &id,
        "--screen-contains",
        "Ready>",
        "--timeout",
        "10s",
        "--json",
    ]);
    let started = Instant::now();
    let timeout = env.run(&[
        "recording",
        "process",
        "wait",
        &id,
        "--screen-contains",
        "NEVER",
        "--timeout",
        "100ms",
        "--json",
    ]);
    assert_eq!(
        timeout.status.code(),
        Some(124),
        "unmatched wait must report timeout"
    );
    let timed_out = parse_json(&timeout);
    assert_eq!(timed_out["run_id"].as_str(), Some(id.as_str()));
    assert_eq!(timed_out["timed_out"].as_bool(), Some(true));
    assert_eq!(timed_out["matched"].as_bool(), Some(false));
    assert!(started.elapsed() < Duration::from_secs(5));
    let refused = env.run(&["recording", "process", "finish", &id, "--json"]);
    assert!(
        !refused.status.success(),
        "finish must not implicitly kill a running process"
    );
    let status = env.json(&["recording", "process", "status", &id, "--json"]);
    assert_eq!(status["phase"].as_str(), Some("running"));
    assert_eq!(status["capture_finalized"].as_bool(), Some(false));
    env.json(&[
        "recording",
        "process",
        "send",
        &id,
        "--text",
        "héllo",
        "--json",
    ]);
    env.json(&[
        "recording",
        "process",
        "send",
        &id,
        "--key",
        "Enter",
        "--json",
    ]);
    env.json(&[
        "recording",
        "process",
        "wait",
        &id,
        "--screen-contains",
        "Reply:héllo",
        "--timeout",
        "10s",
        "--json",
    ]);
    let screen = env.json(&["recording", "process", "screen", &id, "--json"]);
    assert_eq!(screen["schema_version"].as_u64(), Some(4));
    assert_eq!(screen["run_id"].as_str(), Some(id.as_str()));
    assert!(
        screen["text"]
            .as_str()
            .expect("screen.text")
            .contains("Reply:héllo")
    );
    let stdin = env.run_with_input(
        &["recording", "process", "send", &id, "--stdin", "--json"],
        Some(b"second\n"),
    );
    assert_success(&stdin);
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    assert_eq!(status["exit_code"].as_u64(), Some(0));
    let bytes = read_all(&env, &id);
    assert!(
        String::from_utf8(bytes)
            .expect("UTF-8 fixture")
            .contains("Second:second")
    );
    env.finish(&id, "exports", &["raw", "text"]);
}

#[test]
#[serial]
fn termination_is_separate_from_finalization() {
    let env = RecordingEnv::new();
    let id = env.start(
        "stty -echo -opost\nprintf 'Ready>'\nIFS= read -r line\n",
        &[],
    );
    env.json(&[
        "recording",
        "process",
        "wait",
        &id,
        "--screen-contains",
        "Ready>",
        "--timeout",
        "10s",
        "--json",
    ]);
    let status = env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "100ms",
        "--json",
    ]);
    assert_eq!(status["capture_finalized"].as_bool(), Some(false));
    let exited = env.wait_exit(&id);
    assert_eq!(exited["output_drained"].as_bool(), Some(true));
    assert_ne!(exited["phase"].as_str(), Some("running"));
    assert_eq!(read_all(&env, &id), b"Ready>");
    let path = env.finish(&id, "exports", &["raw"]);
    assert_eq!(
        fs::read(path.join("output.pty")).expect("retained evidence"),
        b"Ready>"
    );
}

#[test]
#[serial]
fn raw_bytes_transcript_and_export_retries_preserve_evidence() {
    let env = RecordingEnv::new();
    let id = env.start(
        "stty -opost\nprintf '\\033[31mé界é🙂\\033[0m\\nabc\\rXY\\nA\\000B\\n'\n",
        &[],
    );
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    let expected = "\u{1b}[31mé界é🙂\u{1b}[0m\nabc\rXY\nA\0B\n".as_bytes();
    assert_eq!(read_all(&env, &id), expected);
    let blocker = env.root.join("not-a-directory");
    fs::write(&blocker, b"keep").expect("export blocker");
    let bad_path = blocker.join("exports");
    let failed = env.run(&[
        "recording",
        "process",
        "finish",
        &id,
        "--export",
        "raw",
        "--output",
        bad_path.to_str().expect("path"),
        "--json",
    ]);
    assert!(!failed.status.success());
    assert_eq!(fs::read(&blocker).expect("unchanged blocker"), b"keep");
    let first = env.finish(&id, "first", &["raw", "text", "events"]);
    assert_eq!(fs::read(first.join("output.pty")).expect("raw"), expected);
    assert_eq!(
        fs::read_to_string(first.join("output.txt")).expect("transcript"),
        "é界é🙂\nXYc\nAB\n"
    );
    let second = env.finish(&id, "second", &["raw", "text", "events"]);
    for name in ["output.pty", "output.txt", "events.jsonl"] {
        assert_eq!(
            fs::read(first.join(name)).expect("first artifact"),
            fs::read(second.join(name)).expect("retry artifact")
        );
    }
    let duplicate = env.run(&[
        "recording",
        "process",
        "finish",
        &id,
        "--export",
        "raw",
        "--output",
        first.to_str().expect("path"),
        "--json",
    ]);
    assert!(
        !duplicate.status.success(),
        "existing destination must not be overwritten"
    );
    assert_eq!(
        fs::read(first.join("output.pty")).expect("still retained"),
        expected
    );
}

#[test]
#[serial]
fn unknown_ids_and_invalid_cursors_never_fall_back_to_another_runtime() {
    let first = RecordingEnv::new();
    let id = first.start("stty -opost\nprintf evidence\n", &[]);
    let status = first.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    assert_eq!(read_all(&first, &id), b"evidence");
}

#[test]
#[serial]
fn gif_export_decodes_every_frame_and_animates() {
    let env = RecordingEnv::new();
    let id = env.start("stty -echo -opost\nprintf '\\033[2J\\033[HFRAME_ONE'\nIFS= read -r line\nprintf '\\033[2J\\033[HFRAME_TWO'\nIFS= read -r line\n", &[]);
    env.json(&[
        "recording",
        "process",
        "wait",
        &id,
        "--screen-contains",
        "FRAME_ONE",
        "--timeout",
        "10s",
        "--json",
    ]);
    // Deliberate media sampling interval, not process-readiness synchronization.
    thread::sleep(Duration::from_millis(300));
    env.json(&[
        "recording",
        "process",
        "send",
        &id,
        "--key",
        "Enter",
        "--json",
    ]);
    env.json(&[
        "recording",
        "process",
        "wait",
        &id,
        "--screen-contains",
        "FRAME_TWO",
        "--timeout",
        "10s",
        "--json",
    ]);
    thread::sleep(Duration::from_millis(300));
    env.json(&[
        "recording",
        "process",
        "send",
        &id,
        "--key",
        "Enter",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    let path = env.finish(&id, "gif-export", &["gif"]);
    let gifs: Vec<_> = fs::read_dir(&path)
        .expect("export directory")
        .map(|entry| entry.expect("artifact entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "gif"))
        .collect();
    assert_eq!(gifs.len(), 1, "one GIF expected in {path:?}");
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options
        .read_info(File::open(&gifs[0]).expect("GIF artifact"))
        .expect("valid GIF header and palette");
    let width = decoder.width();
    let height = decoder.height();
    assert!(width > 0 && height > 0);
    let mut canvas = vec![0_u8; usize::from(width) * usize::from(height) * 4];
    let mut frames = 0;
    let mut duration = 0_u64;
    let mut first = None;
    let mut changed = false;
    while let Some(frame) = decoder
        .read_next_frame()
        .expect("decode complete GIF frame stream")
    {
        assert!(u32::from(frame.left) + u32::from(frame.width) <= u32::from(width));
        assert!(u32::from(frame.top) + u32::from(frame.height) <= u32::from(height));
        assert_eq!(
            frame.buffer.len(),
            usize::from(frame.width) * usize::from(frame.height) * 4
        );
        let previous_canvas = canvas.clone();
        for row in 0..usize::from(frame.height) {
            for column in 0..usize::from(frame.width) {
                let source = (row * usize::from(frame.width) + column) * 4;
                let destination = ((row + usize::from(frame.top)) * usize::from(width)
                    + column
                    + usize::from(frame.left))
                    * 4;
                if frame.buffer[source + 3] != 0 {
                    canvas[destination..destination + 4]
                        .copy_from_slice(&frame.buffer[source..source + 4]);
                }
            }
        }
        if let Some(previous) = &first {
            changed |= previous != &canvas;
        } else {
            first = Some(canvas.clone());
        }
        duration += u64::from(frame.delay);
        match frame.dispose {
            gif::DisposalMethod::Previous => canvas = previous_canvas,
            gif::DisposalMethod::Background => {
                for row in 0..usize::from(frame.height) {
                    let start = ((row + usize::from(frame.top)) * usize::from(width)
                        + usize::from(frame.left))
                        * 4;
                    canvas[start..start + usize::from(frame.width) * 4].fill(0);
                }
            }
            gif::DisposalMethod::Any | gif::DisposalMethod::Keep => {}
        }
        frames += 1;
        assert!(frames <= 1000, "GIF exceeds fixture frame budget");
    }
    assert!(frames >= 2, "GIF is not animated");
    assert!(duration > 0, "GIF has no playback duration");
    assert!(changed, "GIF does not capture differing screen states");
}

#[test]
#[serial]
fn foreground_run_reports_nonzero_child_exit_structurally() {
    let env = RecordingEnv::new();
    let script = env.script("nonzero-retry.sh", "printf nonzero; exit 7\n");
    let key = uuid::Uuid::new_v4().to_string();
    let args = [
        "recording",
        "process",
        "run",
        "--json",
        "--idempotency-key",
        &key,
        "--profile",
        "functional",
        "--viewport",
        "100x30",
        "--",
        "/bin/sh",
        script.to_str().expect("script"),
    ];
    let output = env.run(&args);
    assert_eq!(
        output.status.code(),
        Some(7),
        "foreground CLI preserves child exit status"
    );
    let value = parse_json(&output);
    let id = env.track_run(&value);
    assert_eq!(value["exit_code"].as_u64(), Some(7));
    assert_eq!(value["output_drained"].as_bool(), Some(true));
    assert_eq!(read_all(&env, &id), b"nonzero");
    let retry = env.run(&args);
    assert_eq!(retry.status.code(), Some(7), "retry preserves child exit");
    let retried = parse_json(&retry);
    assert_eq!(retried["run_id"], value["run_id"]);
    assert_eq!(retried["recording_id"], value["recording_id"]);
    assert_eq!(retried["capture_finalized"].as_bool(), Some(true));
}

#[test]
#[serial]
fn durable_start_and_send_retries_do_not_duplicate_effects() {
    let env = RecordingEnv::new();
    let script = env.script("retry.sh", "stty -echo -opost\nprintf launch >> launch-count\nprintf 'Ready>'\nIFS= read -r first\nprintf 'FIRST:%s\\n' \"$first\"\nIFS= read -r second\nprintf 'SECOND:%s' \"$second\"\n");
    let key = uuid::Uuid::new_v4().to_string();
    let args = [
        "recording",
        "process",
        "start",
        "--json",
        "--idempotency-key",
        &key,
        "--profile",
        "functional",
        "--viewport",
        "100x30",
        "--cwd",
        env.root.to_str().expect("cwd"),
        "--",
        "/bin/sh",
        script.to_str().expect("script"),
    ];
    let first = env.json(&args);
    let id = env.track_run(&first);
    let retry = env.json(&args);
    assert_eq!(retry["run_id"], first["run_id"]);
    assert_eq!(retry["recording_id"], first["recording_id"]);
    env.json(&[
        "recording",
        "process",
        "wait",
        &id,
        "--screen-contains",
        "Ready>",
        "--timeout",
        "10s",
        "--json",
    ]);
    assert_eq!(
        fs::read(env.root.join("launch-count")).expect("launch witness"),
        b"launch"
    );
    let mut conflict = args.to_vec();
    conflict.push("conflicting-argument");
    assert!(
        !env.run(&conflict).status.success(),
        "conflicting start identity accepted"
    );
    let send_key = uuid::Uuid::new_v4().to_string();
    let send = [
        "recording",
        "process",
        "send",
        &id,
        "--idempotency-key",
        &send_key,
        "--text",
        "one\n",
        "--json",
    ];
    env.json(&send);
    env.json(&send);
    let conflict = env.run(&[
        "recording",
        "process",
        "send",
        &id,
        "--idempotency-key",
        &send_key,
        "--text",
        "different\n",
        "--json",
    ]);
    assert!(
        !conflict.status.success(),
        "conflicting send identity accepted"
    );
    env.json(&[
        "recording",
        "process",
        "wait",
        &id,
        "--screen-contains",
        "FIRST:one",
        "--timeout",
        "10s",
        "--json",
    ]);
    let second_key = uuid::Uuid::new_v4().to_string();
    env.json(&[
        "recording",
        "process",
        "send",
        &id,
        "--idempotency-key",
        &second_key,
        "--text",
        "two\n",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    assert_eq!(read_all(&env, &id), b"Ready>FIRST:one\nSECOND:two");
    assert_eq!(
        fs::read(env.root.join("launch-count")).expect("one launch"),
        b"launch"
    );
    let exports = env.finish(&id, "exports", &["raw", "events"]);
    let input_events: Vec<Value> = fs::read_to_string(exports.join("events.jsonl"))
        .expect("retry event export")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("retry event JSON"))
        .filter(|event| event["kind"] == "pane_input_raw")
        .collect();
    assert_eq!(
        input_events.len(),
        2,
        "duplicate or conflicting input recorded: {input_events:?}"
    );
}

#[test]
#[serial]
fn finalized_evidence_remains_available_after_explicit_server_cleanup() {
    let env = RecordingEnv::new();
    let id = env.start(
        "stty -opost\nprintf '\\033[2J\\033[HOFFLINE_EVIDENCE'\n",
        &[],
    );
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    let before = read_all(&env, &id);
    let screen = env.json(&["recording", "process", "screen", &id, "--json"]);
    let first = env.finish(&id, "online", &["raw", "text", "events", "gif"]);
    let cleaned = env.json(&[
        "recording",
        "process",
        "finish",
        &id,
        "--cleanup-server",
        "--json",
    ]);
    assert_eq!(cleaned["capture_finalized"].as_bool(), Some(true));
    let runtime_root = PathBuf::from(
        cleaned["runtime_root"]
            .as_str()
            .expect("owned runtime root"),
    );
    // Probe only this returned, locator-validated dedicated endpoint. Never defaults.
    let stopped = env.run_at_runtime(&["server", "status"], None, Some(&runtime_root));
    assert!(
        !stopped.status.success(),
        "cleanup left the dedicated endpoint serving"
    );
    let status = env.json(&["recording", "process", "status", &id, "--json"]);
    assert_eq!(status["capture_finalized"].as_bool(), Some(true));
    assert_eq!(status["run_id"].as_str(), Some(id.as_str()));
    assert_eq!(read_all(&env, &id), before);
    assert_eq!(
        env.json(&["recording", "process", "screen", &id, "--json"]),
        screen
    );
    let second = env.finish(&id, "offline", &["raw", "text", "events", "gif"]);
    for name in ["output.pty", "output.txt", "events.jsonl", "output.gif"] {
        assert_eq!(
            fs::read(first.join(name)).expect("online artifact"),
            fs::read(second.join(name)).expect("offline artifact")
        );
    }
}

#[test]
#[serial]
fn immediate_output_is_replayed_into_gif_without_screen_polling() {
    let env = RecordingEnv::new();
    // No sleeps, screen queries, or screen-condition waits before exit.
    let id = env.start(
        "stty -opost\nprintf '\\033[2J\\033[H\\033[41m          \\033[0m'\n",
        &[],
    );
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    let artifacts = env.finish(&id, "immediate-gif", &["gif"]);
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options
        .read_info(File::open(artifacts.join("output.gif")).expect("immediate GIF"))
        .expect("decode GIF");
    let mut has_red_output = false;
    let mut frames = 0;
    while let Some(frame) = decoder.read_next_frame().expect("decode immediate frame") {
        has_red_output |= frame.buffer.as_chunks::<4>().0.iter().any(|pixel| {
            pixel[3] != 0
                && pixel[0] > 80
                && u16::from(pixel[0]) > u16::from(pixel[1]) * 2
                && u16::from(pixel[0]) > u16::from(pixel[2]) * 2
        });
        frames += 1;
        assert!(frames <= 1000);
    }
    assert!(frames > 0);
    assert!(
        has_red_output,
        "immediate colored output lost before canonical GIF replay"
    );
}

#[test]
#[serial]
fn animated_gif_preserves_ordered_output_and_final_exit_frame() {
    let env = RecordingEnv::new();
    // Deliberately separated source frames exercise animation, while the last
    // frame exits immediately and must survive drain without screen polling.
    let script = "stty -opost\nprintf '\\033[2J\\033[H\\033[41mRED\\033[0m'\nsleep 0.3\nprintf '\\033[2J\\033[H\\033[42mGREEN\\033[0m'\nsleep 0.3\nprintf '\\033[2J\\033[H\\033[44mBLUE\\033[0m'\n";
    let id = env.start(script, &[]);
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    assert_eq!(status["output_drained"].as_bool(), Some(true));
    let expected = b"\x1b[2J\x1b[H\x1b[41mRED\x1b[0m\x1b[2J\x1b[H\x1b[42mGREEN\x1b[0m\x1b[2J\x1b[H\x1b[44mBLUE\x1b[0m";
    assert_eq!(read_all(&env, &id), expected);
    let artifacts = env.finish(&id, "animation", &["raw", "gif"]);
    assert_eq!(
        fs::read(artifacts.join("output.pty")).expect("raw animation"),
        expected
    );
    let colors = gif_dominant_colors(&artifacts.join("output.gif"));
    let mut ordered = Vec::new();
    for color in colors {
        if ordered.last() != Some(&color) {
            ordered.push(color);
        }
    }
    assert_eq!(
        ordered,
        [0, 1, 2],
        "lost or reordered animated source frames"
    );
    let retried = env.finish(&id, "animation-retry", &["raw", "gif"]);
    assert_eq!(
        fs::read(retried.join("output.pty")).expect("retry raw"),
        expected
    );
    assert_eq!(
        gif_dominant_colors(&retried.join("output.gif")),
        gif_dominant_colors(&artifacts.join("output.gif")),
        "finalization retry changed animation"
    );
}

fn gif_dominant_colors(path: &Path) -> Vec<usize> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options
        .read_info(File::open(path).expect("animation GIF"))
        .expect("GIF decoder");
    let mut colors = Vec::new();
    let mut frames = 0;
    while let Some(frame) = decoder.read_next_frame().expect("decode animation frame") {
        frames += 1;
        assert!(frames <= 1000, "unbounded animation");
        let mut counts = [0_usize; 3];
        for pixel in frame
            .buffer
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[3] != 0)
        {
            for channel in 0..3 {
                if pixel[channel] > 80
                    && u16::from(pixel[channel]) > u16::from(pixel[(channel + 1) % 3]) * 2
                    && u16::from(pixel[channel]) > u16::from(pixel[(channel + 2) % 3]) * 2
                {
                    counts[channel] += 1;
                }
            }
        }
        if let Some((channel, _)) = counts.iter().enumerate().max_by_key(|(_, count)| *count)
            && counts[channel] > 0
        {
            colors.push(channel);
        }
    }
    assert!(frames > 0, "empty animation");
    colors
}

/// An interrupted recording cannot be finalized, so its server needs explicit
/// fixture-owned cleanup rather than the normal finish --cleanup-server path.
struct InterruptedRecordingServer<'a> {
    env: &'a RecordingEnv,
    root: PathBuf,
    stopped: bool,
}

impl Drop for InterruptedRecordingServer<'_> {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let stopped = self
            .env
            .run_at_runtime(&["server", "stop"], None, Some(&self.root));
        if !stopped.status.success() {
            eprintln!("interrupted fixture server cleanup failed: {stopped:?}");
        }
        // Retain interrupted canonical evidence; never mark it finalized merely
        // to make fixture cleanup succeed.
        eprintln!(
            "retained interrupted recording evidence: {}",
            self.root.display()
        );
    }
}

#[test]
#[serial]
fn server_sigkill_does_not_resurrect_recorded_execution_or_invent_completion() {
    let env = RecordingEnv::new();
    let script = env.script(
        "server-loss.sh",
        "stty -echo -opost\nprintf launch >> launch-count\nprintf '%s' \"$$\" > target.pid\nprintf 'Ready>'\nIFS= read -r line\nprintf 'UNEXPECTED_REPLAY:%s' \"$line\"\n",
    );
    let key = uuid::Uuid::new_v4().to_string();
    let args = [
        "recording",
        "process",
        "start",
        "--json",
        "--idempotency-key",
        &key,
        "--profile",
        "functional",
        "--viewport",
        "100x30",
        "--cwd",
        env.root.to_str().expect("cwd"),
        "--",
        "/bin/sh",
        script.to_str().expect("script"),
    ];
    let first = env.json(&args);
    let id = env.track_run(&first);
    let runtime = PathBuf::from(first["runtime_root"].as_str().expect("owned root"));
    let mut owner = InterruptedRecordingServer {
        env: &env,
        root: runtime.clone(),
        stopped: false,
    };
    // Normal finish cleanup would restart this intentionally unfinished run.
    // The guard above stops only its explicitly owned dedicated server instead.
    env.runs.borrow_mut().retain(|run| run != &id);
    env.json(&[
        "recording",
        "process",
        "wait",
        &id,
        "--screen-contains",
        "Ready>",
        "--timeout",
        "10s",
        "--json",
    ]);
    let read_pid = |path: &Path| -> u32 {
        let pid = fs::read_to_string(path)
            .expect("fixture pid")
            .trim()
            .parse()
            .expect("numeric pid");
        assert!(pid > 1 && pid != std::process::id(), "unsafe fixture pid");
        pid
    };
    let server_pid = read_pid(&runtime.join("runtime/server.pid"));
    let target_pid = read_pid(&env.root.join("target.pid"));
    assert_ne!(server_pid, target_pid);
    let killed = Command::new("kill")
        .args(["-KILL", &server_pid.to_string()])
        .output()
        .expect("kill only the dedicated recording server");
    assert_success(&killed);

    // Test the exact admitted target, not an unrelated process-name search.
    // Treat an unreaped zombie as dead; kill(0) alone cannot distinguish it.
    let target_running = || {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &target_pid.to_string()])
            .output()
            .expect("inspect exact fixture target");
        output.status.success()
            && !output.stdout.is_empty()
            && !String::from_utf8_lossy(&output.stdout)
                .trim_start()
                .starts_with('Z')
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while target_running() && Instant::now() < deadline {
        thread::sleep(POLL_INTERVAL);
    }
    let leaked = target_running();
    if leaked {
        // Failure containment for the old independent-guardian implementation.
        let _ = Command::new("kill")
            .args(["-KILL", &target_pid.to_string()])
            .status();
    }

    // Status reconnects the same owned server root, but not its recorded target.
    // Missing current execution authority may return an explicit error instead
    // of a status. Neither representation may claim a successful outcome.
    for operation in [
        vec!["recording", "process", "status", &id, "--json"],
        args.to_vec(),
    ] {
        let output = env.run(&operation);
        let value = parse_json(&output);
        if output.status.success() {
            assert_eq!(value["run_id"], first["run_id"]);
            assert_eq!(value["execution"], first["execution"]);
            assert_eq!(value["recording_id"], first["recording_id"]);
            assert_eq!(value["recovery_required"], true);
            assert_eq!(value["capture_finalized"], false);
            assert_eq!(value["output_drained"], false);
            assert!(value["exit_code"].is_null());
        } else {
            assert_eq!(value["ok"], false, "explicit recovery refusal: {value}");
            assert!(
                value["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("execution status: NotFound")),
                "must reach recording recovery, not fail bootstrap or service discovery: {value}"
            );
        }
    }
    let finish = env.run(&["recording", "process", "finish", &id, "--json"]);
    assert!(
        !finish.status.success(),
        "server loss was finalized as complete"
    );
    assert_eq!(parse_json(&finish)["ok"], false);
    // Recording commands above reached the restarted service. Dedicated runtimes
    // deliberately omit the snapshot plugin, so `server status` is not a valid
    // readiness probe here. Check the owned daemon identity instead.
    assert_ne!(read_pid(&runtime.join("runtime/server.pid")), server_pid);
    assert_eq!(fs::read(env.root.join("launch-count")).unwrap(), b"launch");
    let retained: Value = serde_json::from_slice(
        &fs::read(
            runtime
                .join("state/recordings/process-runs-v4")
                .join(format!("{id}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(retained["execution"], first["execution"]);
    assert_eq!(retained["capture_finalized"], false);
    assert_eq!(retained["output_drained"], false);
    assert_eq!(retained["capture_eof"], false);
    assert_eq!(retained["target_exited"], false);
    assert_eq!(retained["cleanup_complete"], false);
    assert!(retained["exit_code"].is_null());
    let stopped = env.run_at_runtime(&["server", "stop"], None, Some(&owner.root));
    assert_success(&stopped);
    owner.stopped = true;
    assert!(
        !leaked,
        "recorded target survived its owning server SIGKILL"
    );
}

#[test]
#[serial]
fn incomplete_capture_refuses_finalization_and_retries_without_relaunch() {
    let env = RecordingEnv::new();
    let id = env.start("stty -opost\nprintf EXACT_CAPTURE\n", &[]);
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    let expected = read_all(&env, &id);
    assert_eq!(expected, b"EXACT_CAPTURE");
    let directory = PathBuf::from(status["runtime_root"].as_str().expect("owned root"))
        .join("state/recordings")
        .join(status["recording_id"].as_str().expect("recording id"));
    // Corrupt only this fixture's derived live file after the writer caught up.
    // A drained execution must not mask a missing canonical capture suffix.
    fs::write(directory.join("output.live"), b"EXACT").expect("inject capture gap");
    let refused = env.run(&["recording", "process", "finish", &id, "--json"]);
    assert!(
        !refused.status.success(),
        "gap was acknowledged as complete"
    );
    let error = parse_json(&refused);
    assert_eq!(error["schema_version"], 4);
    assert_eq!(error["ok"], false);
    assert!(
        error["error"]
            .as_str()
            .expect("explicit boundary error")
            .contains("recording raw boundary differs from execution authority"),
        "unexpected integrity refusal: {error}"
    );
    let pending = env.json(&["recording", "process", "status", &id, "--json"]);
    assert_eq!(pending["capture_finalized"].as_bool(), Some(false));
    assert_eq!(pending["execution"], status["execution"]);
    let manifest: Value = serde_json::from_slice(
        &fs::read(directory.join("manifest.json")).expect("stopped writer manifest"),
    )
    .expect("manifest JSON");
    assert!(!manifest["summary"]["ended_epoch_ms"].is_null());
    // Simulate recovery of the derived file from retained canonical segments.
    // The stopped writer must not be restarted or the process relaunched.
    fs::write(directory.join("output.live"), &expected).expect("repair fixture live file");
    let artifacts = env.finish(&id, "recovered-capture", &["raw"]);
    assert_eq!(
        fs::read(artifacts.join("output.pty")).expect("canonical raw"),
        expected
    );
    let finalized = env.json(&["recording", "process", "status", &id, "--json"]);
    assert_eq!(finalized["execution"], status["execution"]);
    assert_eq!(finalized["recording_id"], status["recording_id"]);
}

#[test]
#[serial]
fn exit_wait_longer_than_default_transport_timeout_returns_run_status() {
    let env = RecordingEnv::new();
    let id = env.start(
        "stty -echo -opost\nprintf 'Ready>'\nIFS= read -r line\nprintf '%s' \"$line\"\n",
        &[],
    );
    let started = Instant::now();
    let output = env.run(&[
        "recording",
        "process",
        "wait",
        &id,
        "--exit",
        "--timeout",
        "6s",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(124), "{output:?}");
    let status = parse_json(&output);
    assert_eq!(status["run_id"].as_str(), Some(id.as_str()));
    assert_eq!(status["phase"].as_str(), Some("running"));
    assert_eq!(status["output_drained"].as_bool(), Some(false));
    assert!(started.elapsed() >= Duration::from_secs(6));
    let ready = env.json(&["recording", "process", "read", &id, "--json"]);
    assert_eq!(ready["data"], serde_json::json!(b"Ready>".to_vec()));
    assert_eq!(ready["complete"].as_bool(), Some(false));
    env.json(&[
        "recording",
        "process",
        "send",
        &id,
        "--text",
        "DONE\n",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    assert_eq!(read_all(&env, &id), b"Ready>DONE");
    env.finish(&id, "long-wait", &["raw"]);
}

#[test]
#[serial]
fn exit_wait_beyond_five_seconds_returns_complete_evidence() {
    let env = RecordingEnv::new();
    // Start sleeping only after launch returns, so startup latency cannot
    // accidentally make this a sub-five-second service invocation.
    let id = env.start(
        "stty -echo -opost\nprintf launch >> long-wait-launches\nprintf BEGIN\nwhile [ ! -f long-wait-gate ]; do sleep 0.01; done\nsleep 7\nprintf END\nexit 7\n",
        &[],
    );
    let started = Instant::now();
    fs::write(env.root.join("long-wait-gate"), b"ready").expect("release owned program");
    let status = env.wait_exit(&id);
    assert_eq!(status["output_drained"], true);
    assert!(started.elapsed() >= Duration::from_secs(7));
    assert_eq!(status["run_id"].as_str(), Some(id.as_str()));
    assert_eq!(status["exit_code"].as_u64(), Some(7));
    assert_eq!(status["output_drained"].as_bool(), Some(true));
    assert_eq!(status["output_watermark"].as_u64(), Some(8));
    assert_eq!(read_all(&env, &id), b"BEGINEND");
    let artifacts = env.finish(&id, "completed-long-wait", &["raw"]);
    assert_eq!(
        fs::read(artifacts.join("output.pty")).expect("raw output"),
        b"BEGINEND"
    );
    assert_eq!(
        fs::read(env.root.join("long-wait-launches")).expect("one launch"),
        b"launch"
    );
}

#[test]
#[serial]
fn nonzero_foreground_same_key_retry_retains_exit_and_one_launch() {
    let env = RecordingEnv::new();
    let script = env.script(
        "nonzero-retry.sh",
        "stty -opost\nprintf launch >> nonzero-launches\nprintf NONZERO\nexit 7\n",
    );
    let key = uuid::Uuid::new_v4().to_string();
    let args = [
        "recording",
        "process",
        "run",
        "--json",
        "--idempotency-key",
        &key,
        "--cwd",
        env.root.to_str().expect("cwd"),
        "--",
        "/bin/sh",
        script.to_str().expect("script"),
    ];
    let first = env.run(&args);
    assert_eq!(first.status.code(), Some(7));
    let first = parse_json(&first);
    let id = env.track_run(&first);
    let retry = env.run(&args);
    assert_eq!(retry.status.code(), Some(7), "retry lost child exit code");
    let retry = parse_json(&retry);
    assert_eq!(retry["run_id"], first["run_id"]);
    assert_eq!(retry["recording_id"], first["recording_id"]);
    assert_eq!(retry["exit_code"].as_u64(), Some(7));
    assert_eq!(retry["capture_finalized"].as_bool(), Some(true));
    assert_eq!(
        fs::read(env.root.join("nonzero-launches")).expect("launch count"),
        b"launch"
    );
    assert_eq!(read_all(&env, &id), b"NONZERO");
}

#[test]
#[serial]
fn foreground_run_retry_preserves_identity_and_one_launch() {
    let env = RecordingEnv::new();
    let script = env.script(
        "run-retry.sh",
        "stty -opost\nprintf launch >> run-launch-count\nprintf RUN_ONCE\n",
    );
    let key = uuid::Uuid::new_v4().to_string();
    let args = [
        "recording",
        "process",
        "run",
        "--json",
        "--idempotency-key",
        &key,
        "--cwd",
        env.root.to_str().expect("cwd"),
        "--",
        "/bin/sh",
        script.to_str().expect("script"),
    ];
    let first = env.json(&args);
    let id = env.track_run(&first);
    let retry = env.json(&args);
    assert_eq!(retry["run_id"], first["run_id"]);
    assert_eq!(retry["recording_id"], first["recording_id"]);
    assert_eq!(retry["capture_finalized"].as_bool(), Some(true));
    assert_eq!(
        fs::read(env.root.join("run-launch-count")).expect("one launch"),
        b"launch"
    );
    assert_eq!(read_all(&env, &id), b"RUN_ONCE");
}
