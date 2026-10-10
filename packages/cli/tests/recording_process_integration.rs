//! Real CLI acceptance tests. Never use a PATH binary or a user's runtime.
//! Defaults to CARGO_BIN_EXE_bmux; installed acceptance requires both
//! BMUX_RECORDING_TEST_BINARY and BMUX_RECORDING_TEST_INSTALL_ROOT, pointing
//! inside an explicitly owned staged/relocated installation.
//! The coordinator builds/validates this target; this fixture never invokes Cargo.
//! CLI JSON assertions follow process_commands.rs and recording-process-v1.
//! Start must automatically bootstrap an owned isolated runtime; this fixture never
//! starts a server manually. Cleanup explicitly requests finish --cleanup-server.
#![cfg(unix)]

use serde_json::Value;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

struct RecordingEnv {
    root: PathBuf,
    binary: PathBuf,
    runs: std::cell::RefCell<Vec<String>>,
    command_sequence: std::cell::Cell<u64>,
}

impl RecordingEnv {
    fn new() -> Self {
        // /tmp keeps Unix socket paths below macOS's small sockaddr_un limit.
        let root = PathBuf::from("/tmp").join(format!("bmux-rp-{}", uuid::Uuid::new_v4().simple()));
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
        Self {
            root,
            binary: acceptance_binary(),
            runs: std::cell::RefCell::new(Vec::new()),
            command_sequence: std::cell::Cell::new(0),
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
        command
            .current_dir(&self.root)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("home/cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("TMPDIR", self.root.join("tmp"))
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
        let mut child = command
            .stdin(stdin)
            .stdout(File::create(&stdout_path).expect("create command stdout"))
            .stderr(File::create(&stderr_path).expect("create command stderr"))
            .spawn()
            .expect("spawn explicit test binary");
        let deadline = Instant::now() + COMMAND_TIMEOUT;
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
        // Mirrors recording-process-v1 run-status; no heuristic identity lookup.
        assert_eq!(value["schema_version"].as_u64(), Some(1));
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
        let base = PathBuf::from("/tmp")
            .canonicalize()
            .expect("canonical temp base");
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
        uuid::Uuid::parse_str(
            name.strip_prefix("bmux-recorded-")
                .expect("dedicated runtime prefix"),
        )
        .expect("dedicated runtime UUID");
        let locator_path = self
            .root
            .join("data/recording-process-v1")
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

    fn wait_leader_exit(&self, id: &str, expected_code: u64) -> Value {
        // Read-only observation: never terminate implicitly when polling status.
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            let status = self.json(&["recording", "process", "status", id, "--json"]);
            if status["phase"].as_str() == Some("exited") {
                assert_eq!(status["exit_code"].as_u64(), Some(expected_code));
                assert_eq!(status["capture_finalized"].as_bool(), Some(false));
                return status;
            }
            assert!(Instant::now() < deadline, "leader did not exit: {status}");
            thread::sleep(POLL_INTERVAL);
        }
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
        assert_eq!(value["schema_version"].as_u64(), Some(1));
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
            text.contains("--json"),
            "missing JSON discoverability: {text}"
        );
    }
}

#[test]
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
fn relocated_installed_payload_runs_with_committed_supervisor_inventory() {
    let env = RecordingEnv::relocated_installation();
    let id = env.start("stty -opost\nprintf INSTALLED_RELOCATED\n", &[]);
    let before_disposal = env.wait_leader_exit(&id, 0);
    // This export test explicitly disposes only its exited legacy execution.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
    assert_eq!(status["exit_code"].as_u64(), Some(0));
    assert_eq!(read_all(&env, &id), b"INSTALLED_RELOCATED");
    let artifacts = env.finish(&id, "installed-export", &["raw"]);
    assert_eq!(
        fs::read(artifacts.join("output.pty")).expect("installed raw"),
        b"INSTALLED_RELOCATED"
    );
}

#[test]
#[ignore = "requires coordinator-built real installed payload and explicit binary/root selection"]
fn relocated_installed_native_terminal_contract_exports_and_remains_offline() {
    let env = RecordingEnv::relocated_installation();
    let script = env.script(
        "installed-native.sh",
        "stty -opost\nprintf '\\033[41mINSTALLED_NATIVE\\033[0m'\nexit 7\n",
    );
    let output = env.run(&[
        "recording",
        "process",
        "run",
        "--contract-version",
        "3",
        "--lifetime",
        "terminal-session",
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
    let status = native_result(&output);
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
        native_result(&finished)["status"]["run_id"].as_str(),
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
    assert_eq!(
        native_result(&offline)["recording_id"],
        status["recording_id"]
    );
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

fn native_result(output: &Output) -> Value {
    let value = parse_json(output);
    assert_eq!(value["schema_version"].as_u64(), Some(1));
    assert_eq!(
        value["contract"],
        serde_json::json!({
            "interface": "recording-process-terminal-v3",
            "lifecycle": "conventional-terminal-session",
            "status_schema_version": 1,
        })
    );
    value
        .get("result")
        .expect("explicit native result envelope")
        .clone()
}

#[test]
fn native_terminal_profiles_use_explicit_contract_and_complete_capture() {
    for profile in ["full", "functional", "visual"] {
        let env = RecordingEnv::new();
        let script = env.script("native-input.sh", "stty -echo -opost\nprintf 'Ready>'\nIFS= read -r line\nprintf '\\033[41mReply:%s\\033[0m' \"$line\"\nexit 7\n");
        let start = env.run(&[
            "recording",
            "process",
            "start",
            "--contract-version",
            "3",
            "--lifetime",
            "terminal-session",
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
        let status = native_result(&start);
        assert_eq!(status["schema_version"].as_u64(), Some(1));
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
        assert_eq!(native_result(&ready)["matched"].as_bool(), Some(true));
        let input = env.run_with_input(
            &["recording", "process", "send", &id, "--stdin", "--json"],
            Some("héllo".as_bytes()),
        );
        assert_success(&input);
        assert_eq!(native_result(&input)["accepted_bytes"].as_u64(), Some(6));
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
        assert_eq!(native_result(&key)["accepted_bytes"].as_u64(), Some(1));
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
        let status = native_result(&waited);
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
        let read = native_result(&read);
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
        let finished = native_result(&finished);
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
            native_result(&cleanup)["capture_finalized"].as_bool(),
            Some(true)
        );
        let offline = env.run(&["recording", "process", "status", &id, "--json"]);
        assert_success(&offline);
        assert_eq!(
            native_result(&offline)["run_id"].as_str(),
            Some(id.as_str())
        );
    }
}

#[test]
fn legacy_profiles_preserve_output_and_input_capture_policy() {
    for profile in ["full", "functional", "visual"] {
        let env = RecordingEnv::new();
        let id = env.start_profile(
            "stty -echo -opost\nprintf 'Ready>'\nIFS= read -r line\nprintf '\\033[41mReply:%s\\033[0m' \"$line\"\n",
            &[],
            profile,
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
        assert_success(&env.run_with_input(
            &["recording", "process", "send", &id, "--stdin", "--json"],
            Some("héllo".as_bytes()),
        ));
        env.json(&[
            "recording",
            "process",
            "send",
            &id,
            "--key",
            "Enter",
            "--json",
        ]);
        let before_disposal = env.wait_leader_exit(&id, 0);
        // Disposal is explicit here; observing exit alone must not imply PTY EOF.
        env.json(&[
            "recording",
            "process",
            "terminate",
            &id,
            "--grace-period",
            "1s",
            "--json",
        ]);
        let status = env.wait_exit(&id);
        assert_eq!(status["execution"], before_disposal["execution"]);
        assert_eq!(status["exit_code"], before_disposal["exit_code"]);
        assert_eq!(
            status["output_watermark"],
            before_disposal["output_watermark"]
        );
        assert_eq!(status["schema_version"].as_u64(), Some(1));
        assert_eq!(status["exit_code"].as_u64(), Some(0));
        let expected = "Ready>\x1b[41mReply:héllo\x1b[0m".as_bytes();
        assert_eq!(read_all(&env, &id), expected);
        let artifacts = env.finish(&id, "profile-exports", &["raw", "text", "events", "gif"]);
        assert_eq!(
            fs::read(artifacts.join("output.pty")).expect("profile raw"),
            expected
        );
        assert!(
            fs::read_to_string(artifacts.join("output.txt"))
                .expect("profile text")
                .contains("Reply:héllo")
        );
        let events = fs::read_to_string(artifacts.join("events.jsonl")).expect("profile events");
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
            gif_dominant_colors(&artifacts.join("output.gif")).contains(&0),
            "profile GIF lost final red reply"
        );
    }
}

#[test]
fn stale_finish_revision_does_not_finalize_or_publish() {
    let env = RecordingEnv::new();
    let id = env.start("stty -opost\nprintf STALE\n", &[]);
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
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
fn whole_frame_loss_never_publishes_or_overwrites_live_capture() {
    let env = RecordingEnv::new();
    let id = env.start(
        "stty -opost\nprintf FRAME_ONE\nsleep 0.2\nprintf FRAME_TWO\n",
        &[],
    );
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
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
fn finite_launch_status_before_wait_reports_drain() {
    let env = RecordingEnv::new();
    let id = env.start("stty -opost\nprintf FINITE\ni=0\nwhile [ \"$i\" -lt 4096 ]; do printf '0123456789abcdef'; i=$((i+1)); done\nprintf LAST\nexit 7\n", &[]);
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
    assert_eq!(status["exit_code"].as_u64(), Some(7));
    assert_eq!(
        status["output_watermark"].as_u64(),
        Some(expected.len() as u64)
    );
    let path = env.finish(&id, "finite-status", &["raw"]);
    assert_eq!(
        fs::read(path.join("output.pty")).expect("raw artifact"),
        expected
    );
}

#[test]
fn immediate_exit_and_startup_burst_are_not_lost() {
    for iteration in 0..4 {
        let env = RecordingEnv::new();
        let id = env.start("stty -opost\nprintf FIRST\ni=0\nwhile [ \"$i\" -lt 4096 ]; do printf '0123456789abcdef'; i=$((i+1)); done\nprintf LAST\nexit 7\n", &[]);
        let expected = format!("FIRST{}LAST", "0123456789abcdef".repeat(4096)).into_bytes();
        let before_disposal = env.wait_leader_exit(&id, 7);
        // Leader exit can precede reading its final queued output. Observe the
        // known complete fixture watermark before explicitly disposing the owner.
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let before_disposal = loop {
            let observed = env.json(&["recording", "process", "status", &id, "--json"]);
            assert_eq!(observed["execution"], before_disposal["execution"]);
            assert_eq!(observed["exit_code"].as_u64(), Some(7));
            assert!(
                !observed["capture_finalized"]
                    .as_bool()
                    .expect("capture state")
            );
            let watermark = observed["output_watermark"].as_u64().expect("watermark");
            let expected_watermark = u64::try_from(expected.len()).expect("fixture length");
            assert!(
                watermark <= expected_watermark,
                "unexpected extra startup output"
            );
            if watermark == expected_watermark {
                break observed;
            }
            assert!(
                Instant::now() < deadline,
                "startup capture missing suffix: {observed}"
            );
            thread::sleep(POLL_INTERVAL);
        };
        // Disposal is explicit here; observing exit alone must not imply PTY EOF.
        env.json(&[
            "recording",
            "process",
            "terminate",
            &id,
            "--grace-period",
            "1s",
            "--json",
        ]);
        let status = env.wait_exit(&id);
        assert_eq!(status["execution"], before_disposal["execution"]);
        assert_eq!(status["exit_code"], before_disposal["exit_code"]);
        assert_eq!(
            status["output_watermark"],
            before_disposal["output_watermark"]
        );
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
fn argv_environment_and_working_directory_are_literal() {
    let env = RecordingEnv::new();
    let literal = "spaces 'quotes' $(touch SHOULD_NOT_EXIST); * --flag";
    let id = env.start(
        "stty -opost\nprintf '%s\\n%s\\n%s' \"$1\" \"$BMUX_TEST_VALUE\" \"$PWD\"\n",
        &[literal],
    );
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
    let expected = format!("{literal}\nliteral\n{}", env.root.display()).into_bytes();
    assert_eq!(read_all(&env, &id), expected);
    assert!(
        !env.root.join("SHOULD_NOT_EXIST").exists(),
        "implicit shell expansion occurred"
    );
}

#[test]
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
    assert_eq!(screen["schema_version"].as_u64(), Some(1));
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
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
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
fn raw_bytes_transcript_and_export_retries_preserve_evidence() {
    let env = RecordingEnv::new();
    let id = env.start(
        "stty -opost\nprintf '\\033[31mé界é🙂\\033[0m\\nabc\\rXY\\nA\\000B\\n'\n",
        &[],
    );
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
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
fn unknown_ids_and_invalid_cursors_never_fall_back_to_another_runtime() {
    let first = RecordingEnv::new();
    let id = first.start("stty -opost\nprintf evidence\n", &[]);
    let before_disposal = first.wait_leader_exit(&id, 0);
    // Explicitly dispose the first fixture's exited execution, never the second runtime.
    first.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = first.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
    let other = RecordingEnv::new();
    for verb in ["status", "read", "screen", "terminate", "finish"] {
        let output = other.run(&["recording", "process", verb, &id, "--json"]);
        assert!(
            !output.status.success(),
            "cross-runtime {verb} unexpectedly succeeded"
        );
    }
    for cursor in [
        "-1",
        "not-a-cursor",
        "18446744073709551616",
        "18446744073709551615",
    ] {
        let output = first.run(&[
            "recording",
            "process",
            "read",
            &id,
            "--since",
            cursor,
            "--json",
        ]);
        assert!(
            !output.status.success(),
            "invalid cursor accepted: {cursor}"
        );
    }
    let output = first.run(&[
        "recording",
        "process",
        "read",
        &id,
        "--max-bytes",
        "0",
        "--json",
    ]);
    assert!(!output.status.success(), "zero read budget accepted");
    assert_eq!(read_all(&first, &id), b"evidence");
}

#[test]
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
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
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
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
    assert_eq!(read_all(&env, &id), b"Ready>FIRST:one\nSECOND:two");
    assert_eq!(
        fs::read(env.root.join("launch-count")).expect("one launch"),
        b"launch"
    );
    env.finish(&id, "exports", &["raw"]);
}

#[test]
fn finalized_evidence_remains_available_after_explicit_server_cleanup() {
    let env = RecordingEnv::new();
    let id = env.start(
        "stty -opost\nprintf '\\033[2J\\033[HOFFLINE_EVIDENCE'\n",
        &[],
    );
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
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
fn immediate_output_is_replayed_into_gif_without_screen_polling() {
    let env = RecordingEnv::new();
    // No sleeps, screen queries, or screen-condition waits before exit.
    let id = env.start(
        "stty -opost\nprintf '\\033[2J\\033[H\\033[41m          \\033[0m'\n",
        &[],
    );
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
    let artifacts = env.finish(&id, "immediate-gif", &["gif"]);
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options
        .read_info(File::open(artifacts.join("output.gif")).expect("immediate GIF"))
        .expect("decode GIF");
    let mut has_red_output = false;
    let mut frames = 0;
    while let Some(frame) = decoder.read_next_frame().expect("decode immediate frame") {
        has_red_output |= frame.buffer.chunks_exact(4).any(|pixel| {
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
fn animated_gif_preserves_ordered_output_and_final_exit_frame() {
    let env = RecordingEnv::new();
    // Deliberately separated source frames exercise animation, while the last
    // frame exits immediately and must survive drain without screen polling.
    let script = "stty -opost\nprintf '\\033[2J\\033[H\\033[41mRED\\033[0m'\nsleep 0.3\nprintf '\\033[2J\\033[H\\033[42mGREEN\\033[0m'\nsleep 0.3\nprintf '\\033[2J\\033[H\\033[44mBLUE\\033[0m'\n";
    let id = env.start(script, &[]);
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
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
        for pixel in frame.buffer.chunks_exact(4).filter(|pixel| pixel[3] != 0) {
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

#[test]
fn incomplete_capture_refuses_finalization_and_retries_without_relaunch() {
    let env = RecordingEnv::new();
    let id = env.start("stty -opost\nprintf EXACT_CAPTURE\n", &[]);
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
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
    assert!(
        parse_json(&refused)["error"]
            .as_str()
            .expect("explicit gap error")
            .contains("output watermark")
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
    let before_disposal = env.wait_leader_exit(&id, 0);
    // Disposal is explicit here; observing exit alone must not imply PTY EOF.
    env.json(&[
        "recording",
        "process",
        "terminate",
        &id,
        "--grace-period",
        "1s",
        "--json",
    ]);
    let status = env.wait_exit(&id);
    assert_eq!(status["execution"], before_disposal["execution"]);
    assert_eq!(status["exit_code"], before_disposal["exit_code"]);
    assert_eq!(
        status["output_watermark"],
        before_disposal["output_watermark"]
    );
    assert_eq!(read_all(&env, &id), b"Ready>DONE");
    env.finish(&id, "long-wait", &["raw"]);
}

#[test]
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
