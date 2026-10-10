//! Real CLI acceptance tests. Never use an installed binary or a user's runtime.
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
            runs: std::cell::RefCell::new(Vec::new()),
            command_sequence: std::cell::Cell::new(0),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_bmux"));
        assert!(
            binary.is_absolute(),
            "Cargo must supply an explicit local binary"
        );
        let mut command = Command::new(binary);
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
        let path = self.script("program.sh", script);
        let cwd = self.root.to_str().expect("test root UTF-8");
        let mut args = vec![
            "recording",
            "process",
            "start",
            "--json",
            "--profile",
            "functional",
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
        let status = env.wait_exit(&id);
        assert_eq!(status["exit_code"].as_u64(), Some(7));
        assert_eq!(status["output_drained"].as_bool(), Some(true));
        let expected = format!("FIRST{}LAST", "0123456789abcdef".repeat(4096)).into_bytes();
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
    env.wait_exit(&id);
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
    let status = env.wait_exit(&id);
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
    env.wait_exit(&id);
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
    first.wait_exit(&id);
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
    env.wait_exit(&id);
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
    let output = env.run(&[
        "recording",
        "process",
        "run",
        "--json",
        "--profile",
        "functional",
        "--viewport",
        "100x30",
        "--",
        "/bin/sh",
        "-c",
        "printf nonzero; exit 7",
    ]);
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
    env.wait_exit(&id);
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
    env.wait_exit(&id);
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
    env.wait_exit(&id);
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
    let status = env.wait_exit(&id);
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
    let status = env.wait_exit(&id);
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
    env.wait_exit(&id);
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
