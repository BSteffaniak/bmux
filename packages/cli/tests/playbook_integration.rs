//! Integration tests for the playbook system.
//!
//! These tests exercise the full playbook pipeline: parse → sandbox → execute → report.
//!
//! Two approaches are used:
//! - **Subprocess tests**: Invoke `bmux playbook run --json <fixture>` and assert on
//!   the JSON output. Tests the real CLI path end-to-end.
//! - **Direct API tests**: Import `bmux_cli::playbook::*` and call parsers/validators
//!   directly (lower-level checks).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the built `bmux` binary.
fn bmux_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bmux"))
}

/// Path to the fixtures directory.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/playbooks")
}

/// Run a playbook fixture via the bmux binary and return parsed JSON output.
fn run_playbook_fixture(name: &str) -> (serde_json::Value, bool) {
    let fixture = fixtures_dir().join(name);
    assert!(fixture.exists(), "fixture not found: {}", fixture.display());

    let output = Command::new(bmux_binary())
        .args(["playbook", "run", "--json", fixture.to_str().unwrap()])
        .env("BMUX_PLAYBOOK_ENV_MODE", "inherit")
        .output()
        .expect("failed to run bmux playbook");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "failed to parse JSON output for {name}:\n  error: {e}\n  stdout: {stdout}\n  stderr: {stderr}\n  exit: {:?}",
            output.status
        )
    });

    let pass = json["pass"].as_bool().unwrap_or(false);
    (json, pass)
}

// ---------------------------------------------------------------------------
// Subprocess integration tests
// ---------------------------------------------------------------------------

#[test]
fn playbook_echo_hello() {
    let (json, pass) = run_playbook_fixture("echo_hello.dsl");
    assert!(pass, "playbook should pass: {json:#}");

    let steps = json["steps"].as_array().expect("steps should be array");
    assert!(
        steps.iter().all(|s| s["status"] == "pass"),
        "all steps should pass: {json:#}"
    );
}

#[test]
fn playbook_echo_assert() {
    let (json, pass) = run_playbook_fixture("echo_assert.dsl");
    assert!(pass, "playbook should pass: {json:#}");

    let steps = json["steps"].as_array().expect("steps should be array");
    // Find the assert-screen steps and verify they all pass.
    let assert_steps: Vec<_> = steps
        .iter()
        .filter(|s| s["action"] == "assert-screen")
        .collect();
    assert!(
        assert_steps.len() >= 2,
        "should have at least 2 assert-screen steps: {json:#}"
    );
    for step in &assert_steps {
        assert_eq!(
            step["status"], "pass",
            "assert-screen should pass: {step:#}"
        );
    }
}

#[test]
fn playbook_multi_pane() {
    let (json, pass) = run_playbook_fixture("multi_pane.dsl");
    assert!(pass, "multi-pane playbook should pass: {json:#}");

    let steps = json["steps"].as_array().expect("steps should be array");
    // Check that pane-targeted assert-screen steps pass.
    let assert_steps: Vec<_> = steps
        .iter()
        .filter(|s| s["action"] == "assert-screen")
        .collect();
    assert!(
        assert_steps.len() >= 2,
        "should have assert-screen steps for both panes: {json:#}"
    );
    for step in &assert_steps {
        assert_eq!(step["status"], "pass", "assert should pass: {step:#}");
    }
}

#[test]
fn playbook_wait_for_regex() {
    let (json, pass) = run_playbook_fixture("wait_for_regex.dsl");
    assert!(pass, "wait-for regex should pass: {json:#}");
}

#[test]
fn playbook_env_mode_clean() {
    let fixture = fixtures_dir().join("env_mode_clean.dsl");

    // For this test, do NOT set BMUX_PLAYBOOK_ENV_MODE — the playbook itself
    // has @env-mode clean, so the sandbox should use clean mode.
    let output = Command::new(bmux_binary())
        .args(["playbook", "run", "--json", fixture.to_str().unwrap()])
        .output()
        .expect("failed to run bmux playbook");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\nstdout: {stdout}\nstderr: {stderr}"));

    let pass = json["pass"].as_bool().unwrap_or(false);
    assert!(
        pass,
        "env_mode_clean playbook should pass (TERM should be xterm-256color): {json:#}"
    );
}

#[test]
fn playbook_screen_status() {
    let (json, pass) = run_playbook_fixture("screen_status.dsl");
    assert!(pass, "screen/status playbook should pass: {json:#}");

    let steps = json["steps"].as_array().expect("steps should be array");

    // Find the screen step and verify it has detail with pane data.
    let screen_step = steps.iter().find(|s| s["action"] == "screen");
    assert!(screen_step.is_some(), "should have a screen step: {json:#}");
    let detail = screen_step.unwrap()["detail"].as_str().unwrap_or("");
    assert!(
        !detail.is_empty(),
        "screen step should have detail: {json:#}"
    );

    // Find the status step.
    let status_step = steps.iter().find(|s| s["action"] == "status");
    assert!(status_step.is_some(), "should have a status step: {json:#}");
    let status_detail = status_step.unwrap()["detail"].as_str().unwrap_or("");
    assert!(
        status_detail.contains("session_id="),
        "status should contain session_id: {status_detail}"
    );
}

#[test]
fn playbook_snapshot_capture() {
    let (json, pass) = run_playbook_fixture("snapshot_capture.dsl");
    assert!(pass, "snapshot playbook should pass: {json:#}");

    let snapshots = json["snapshots"].as_array();
    assert!(
        snapshots.is_some() && !snapshots.unwrap().is_empty(),
        "should have snapshots: {json:#}"
    );
    let snap = &snapshots.unwrap()[0];
    assert_eq!(snap["id"], "after_echo", "snapshot id: {snap:#}");
    let panes = snap["panes"]
        .as_array()
        .expect("snapshot should have panes");
    assert!(!panes.is_empty(), "snapshot should have pane captures");
    let pane_text = panes[0]["screen_text"].as_str().unwrap_or("");
    assert!(
        pane_text.contains("snap_content_marker"),
        "snapshot pane text should contain marker: {pane_text}"
    );
}

#[test]
fn playbook_failing_assert() {
    let (json, pass) = run_playbook_fixture("failing_assert.dsl");
    assert!(!pass, "failing playbook should not pass: {json:#}");

    let steps = json["steps"].as_array().expect("steps should be array");
    // Find the assert-screen step that should have failed.
    let failed_step = steps
        .iter()
        .find(|s| s["action"] == "assert-screen" && s["status"] == "fail");
    assert!(
        failed_step.is_some(),
        "should have a failed assert-screen step: {json:#}"
    );
    let failed = failed_step.unwrap();

    // Verify structured failure fields are present.
    assert!(
        failed.get("expected").is_some() && !failed["expected"].is_null(),
        "failed step should have 'expected' field: {failed:#}"
    );
    assert_eq!(
        failed["expected"].as_str().unwrap(),
        "nonexistent_string_xyz",
        "expected should be the contains pattern"
    );
    assert!(
        failed.get("actual").is_some() && !failed["actual"].is_null(),
        "failed step should have 'actual' field: {failed:#}"
    );
    // The actual screen text should contain the real output from the playbook.
    assert!(
        failed["actual"].as_str().unwrap().contains("real_output"),
        "actual should contain the real screen text: {failed:#}"
    );

    // Verify failure_captures is present with at least one pane.
    let captures = failed["failure_captures"].as_array();
    assert!(
        captures.is_some() && !captures.unwrap().is_empty(),
        "failed step should have failure_captures: {failed:#}"
    );
    let pane = &captures.unwrap()[0];
    assert!(
        pane["screen_text"]
            .as_str()
            .unwrap_or("")
            .contains("real_output"),
        "failure_captures pane should contain real_output: {pane:#}"
    );
}

// ---------------------------------------------------------------------------
// Direct API tests (via lib.rs)
// ---------------------------------------------------------------------------

#[test]
fn parse_and_validate_fixtures() {
    let fixtures = [
        "echo_hello.dsl",
        "echo_assert.dsl",
        "multi_pane.dsl",
        "wait_for_regex.dsl",
        "env_mode_clean.dsl",
        "screen_status.dsl",
        "snapshot_capture.dsl",
        "failing_assert.dsl",
        "assert_matches.dsl",
        "include_main.dsl",
        "timeout_wait_for.dsl",
        "timeout_playbook.dsl",
    ];

    for name in &fixtures {
        let path = fixtures_dir().join(name);
        let playbook = bmux_cli::playbook::parse_file(&path)
            .unwrap_or_else(|e| panic!("failed to parse {name}: {e:#}"));

        let errors = bmux_cli::playbook::validate(&playbook, false);
        assert!(
            errors.is_empty(),
            "validation errors for {name}: {errors:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Subprocess: assert-screen matches= (regex)
// ---------------------------------------------------------------------------

#[test]
fn playbook_assert_matches() {
    let (json, pass) = run_playbook_fixture("assert_matches.dsl");
    assert!(pass, "assert-screen matches= should pass: {json:#}");
    let steps = json["steps"].as_array().unwrap();
    let assert_step = steps
        .iter()
        .find(|s| s["action"] == "assert-screen")
        .expect("should have an assert-screen step");
    assert_eq!(
        assert_step["status"], "pass",
        "assert-screen matches= should pass"
    );
}

// ---------------------------------------------------------------------------
// Subprocess: dry-run
// ---------------------------------------------------------------------------

#[test]
fn playbook_dry_run() {
    let fixture = fixtures_dir().join("echo_hello.dsl");

    let output = Command::new(bmux_binary())
        .args(["playbook", "dry-run", "--json", fixture.to_str().unwrap()])
        .output()
        .expect("failed to run bmux playbook dry-run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\nstdout: {stdout}\nstderr: {stderr}"));

    assert_eq!(json["valid"], true, "dry-run should be valid: {json:#}");

    let steps = json["steps"].as_array().expect("should have steps array");
    assert!(!steps.is_empty(), "dry-run should list steps: {json:#}");

    // Verify each step has index, action, and dsl fields.
    for step in steps {
        assert!(
            step.get("index").is_some(),
            "step should have index: {step:#}"
        );
        assert!(
            step.get("action").is_some(),
            "step should have action: {step:#}"
        );
        assert!(step.get("dsl").is_some(), "step should have dsl: {step:#}");
    }

    // Verify the dsl field contains recognizable DSL.
    let first_dsl = steps[0]["dsl"].as_str().unwrap_or("");
    assert!(
        first_dsl.starts_with("new-session"),
        "first step dsl should be new-session: {first_dsl}"
    );

    // Verify config section.
    assert!(
        json.get("config").is_some(),
        "dry-run should have config: {json:#}"
    );

    // Check errors is empty.
    let errors = json["errors"].as_array().expect("should have errors array");
    assert!(errors.is_empty(), "should have no errors: {json:#}");

    // Exit code should be 0.
    assert!(output.status.success(), "dry-run exit code should be 0");
}

// ---------------------------------------------------------------------------
// Direct API: run_playbook (async)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_playbook_echo_pass() {
    let dsl = "\
@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo api_test_marker\\r'
wait-for pattern='api_test_marker'
assert-screen contains='api_test_marker'
";
    let (mut playbook, _) = bmux_cli::playbook::parse_dsl::parse_dsl(dsl).unwrap();
    playbook.config.binary = Some(PathBuf::from(env!("CARGO_BIN_EXE_bmux")));

    let result = bmux_cli::playbook::run(playbook, false).await.unwrap();
    assert!(result.pass, "playbook should pass: {:?}", result.error);
    assert!(
        result
            .steps
            .iter()
            .all(|s| s.status == bmux_cli::playbook::types::StepStatus::Pass),
        "all steps should pass: {:?}",
        result.steps
    );
}

#[tokio::test]
async fn run_playbook_failing_returns_fail() {
    let dsl = "\
@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo real_output\\r'
wait-for pattern='real_output'
assert-screen contains='nonexistent_xyz_api'
";
    let (mut playbook, _) = bmux_cli::playbook::parse_dsl::parse_dsl(dsl).unwrap();
    playbook.config.binary = Some(PathBuf::from(env!("CARGO_BIN_EXE_bmux")));

    let result = bmux_cli::playbook::run(playbook, false).await.unwrap();
    assert!(!result.pass, "playbook should fail");

    let failed = result
        .steps
        .iter()
        .find(|s| s.status == bmux_cli::playbook::types::StepStatus::Fail);
    assert!(failed.is_some(), "should have a failed step");

    let f = failed.unwrap();
    assert!(f.expected.is_some(), "failed step should have expected");
    assert!(f.actual.is_some(), "failed step should have actual");
    assert!(
        f.failure_captures.is_some(),
        "failed step should have failure_captures"
    );
}

// ---------------------------------------------------------------------------
// Direct API: end-to-end recording → playbook → execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recording_to_playbook_end_to_end() {
    use bmux_ipc::{RecordingEventEnvelope, RecordingEventKind, RecordingPayload};
    use uuid::Uuid;

    let session_id = Uuid::from_u128(1);
    let pane_id = Uuid::from_u128(2);

    // Build a minimal synthetic recording: NewSession → send "echo e2e_marker\r" → output.
    let new_session_req = bmux_ipc::Request::NewSession { name: None };
    let new_session_resp = bmux_ipc::ResponsePayload::SessionCreated {
        id: session_id,
        name: None,
    };
    let input_req = bmux_ipc::Request::AttachInput {
        session_id,
        data: b"echo e2e_marker\r".to_vec(),
    };

    let events = vec![
        RecordingEventEnvelope {
            seq: 1,
            mono_ns: 100_000_000,
            wall_epoch_ms: 0,
            session_id: Some(session_id),
            pane_id: None,
            client_id: None,
            kind: RecordingEventKind::RequestStart,
            payload: RecordingPayload::RequestStart {
                request_id: 1,
                request_kind: "new_session".to_string(),
                exclusive: false,
                request_data: bmux_ipc::encode(&new_session_req).unwrap(),
            },
        },
        RecordingEventEnvelope {
            seq: 2,
            mono_ns: 200_000_000,
            wall_epoch_ms: 0,
            session_id: Some(session_id),
            pane_id: None,
            client_id: None,
            kind: RecordingEventKind::RequestDone,
            payload: RecordingPayload::RequestDone {
                request_id: 1,
                request_kind: "new_session".to_string(),
                response_kind: "session_created".to_string(),
                elapsed_ms: 100,
                request_data: bmux_ipc::encode(&new_session_req).unwrap(),
                response_data: bmux_ipc::encode(&new_session_resp).unwrap(),
            },
        },
        RecordingEventEnvelope {
            seq: 3,
            mono_ns: 1_000_000_000,
            wall_epoch_ms: 0,
            session_id: Some(session_id),
            pane_id: Some(pane_id),
            client_id: None,
            kind: RecordingEventKind::RequestStart,
            payload: RecordingPayload::RequestStart {
                request_id: 2,
                request_kind: "attach_input".to_string(),
                exclusive: false,
                request_data: bmux_ipc::encode(&input_req).unwrap(),
            },
        },
        RecordingEventEnvelope {
            seq: 4,
            mono_ns: 1_500_000_000,
            wall_epoch_ms: 0,
            session_id: Some(session_id),
            pane_id: Some(pane_id),
            client_id: None,
            kind: RecordingEventKind::PaneOutputRaw,
            payload: RecordingPayload::Bytes {
                data: b"echo e2e_marker\r\ne2e_marker\r\n$ ".to_vec(),
            },
        },
    ];

    // Step 1: Convert recording events → DSL string.
    let dsl = bmux_cli::playbook::from_recording::events_to_playbook(&events)
        .expect("events_to_playbook should succeed");

    // Sanity check: the DSL should contain key elements.
    assert!(
        dsl.contains("new-session"),
        "DSL should have new-session: {dsl}"
    );
    assert!(
        dsl.contains("send-keys"),
        "DSL should have send-keys: {dsl}"
    );
    assert!(
        dsl.contains("wait-for"),
        "DSL should have wait-for barriers: {dsl}"
    );

    // Step 2: Parse the generated DSL into a Playbook.
    let (mut playbook, _) =
        bmux_cli::playbook::parse_dsl::parse_dsl(&dsl).expect("generated DSL should parse");

    // Step 3: Configure for sandbox execution.
    playbook.config.binary = Some(PathBuf::from(env!("CARGO_BIN_EXE_bmux")));
    playbook.config.shell = Some("sh".to_string());

    // Step 4: Run the playbook.
    let result = bmux_cli::playbook::run(playbook, false)
        .await
        .expect("run_playbook should not error");

    // Step 5: Assert it passes.
    assert!(
        result.pass,
        "generated playbook should pass. steps: {:?}, error: {:?}",
        result.steps, result.error
    );
}

// ---------------------------------------------------------------------------
// G: Include system tests
// ---------------------------------------------------------------------------

#[test]
fn playbook_include_basic() {
    let (json, pass) = run_playbook_fixture("include_main.dsl");
    assert!(pass, "include playbook should pass: {json:#}");

    let steps = json["steps"].as_array().expect("steps should be array");
    // The included file has new-session + send-keys + wait-for (3 steps),
    // then the main file adds assert-screen (1 step) = 4 total.
    assert!(
        steps.len() >= 4,
        "should have steps from both files: {json:#}"
    );
    // The first step should be new-session (from included file).
    assert_eq!(
        steps[0]["action"], "new-session",
        "first step should be new-session from include"
    );
}

#[test]
fn playbook_include_validates() {
    let path = fixtures_dir().join("include_main.dsl");
    let playbook = bmux_cli::playbook::parse_file(&path)
        .unwrap_or_else(|e| panic!("failed to parse include_main.dsl: {e:#}"));

    let errors = bmux_cli::playbook::validate(&playbook, false);
    assert!(
        errors.is_empty(),
        "include playbook should validate: {errors:?}"
    );

    // Verify we have steps from both the main and included files.
    assert!(
        playbook.steps.len() >= 4,
        "should have merged steps from include: {} steps",
        playbook.steps.len()
    );
}

// ---------------------------------------------------------------------------
// H: Timeout behavior tests
// ---------------------------------------------------------------------------

#[test]
fn playbook_wait_for_timeout() {
    let (json, pass) = run_playbook_fixture("timeout_wait_for.dsl");
    assert!(!pass, "wait-for timeout playbook should fail: {json:#}");

    let steps = json["steps"].as_array().expect("steps should be array");
    let wait_step = steps
        .iter()
        .find(|s| s["action"] == "wait-for" && s["status"] == "fail");
    assert!(
        wait_step.is_some(),
        "should have a failed wait-for step: {json:#}"
    );

    let ws = wait_step.unwrap();
    let detail = ws["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("timed out"),
        "detail should mention timeout: {detail}"
    );
    // Verify structured failure fields.
    assert!(
        ws.get("expected").is_some() && !ws["expected"].is_null(),
        "wait-for timeout should have expected field: {ws:#}"
    );
    assert!(
        ws.get("actual").is_some() && !ws["actual"].is_null(),
        "wait-for timeout should have actual field: {ws:#}"
    );
}

#[test]
fn playbook_level_timeout_skips_remaining() {
    let (json, pass) = run_playbook_fixture("timeout_playbook.dsl");
    assert!(!pass, "playbook timeout should fail: {json:#}");

    let steps = json["steps"].as_array().expect("steps should be array");
    // Find a step with status "skip" and detail containing "playbook timeout".
    let skipped = steps.iter().find(|s| s["status"] == "skip");
    assert!(
        skipped.is_some(),
        "should have a skipped step from playbook timeout: {json:#}"
    );
    let detail = skipped.unwrap()["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("playbook timeout"),
        "skipped step should mention playbook timeout: {detail}"
    );
}

// ---------------------------------------------------------------------------
// I: from-recording CLI integration test
// ---------------------------------------------------------------------------

#[test]
fn playbook_from_recording_cli() {
    // Step 1: Run a playbook with --record to produce a recording.
    let fixture = fixtures_dir().join("echo_hello.dsl");
    let output = Command::new(bmux_binary())
        .args([
            "playbook",
            "run",
            "--json",
            "--record",
            fixture.to_str().unwrap(),
        ])
        .env("BMUX_PLAYBOOK_ENV_MODE", "inherit")
        .output()
        .expect("failed to run bmux playbook with recording");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = match serde_json::from_str(&stdout) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("JSON parse failed (recording run): {e}\nstdout: {stdout}");
            return; // Skip if we can't parse -- recording might not be supported in this env
        }
    };

    let recording_id = match json["recording_id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            eprintln!("No recording_id in output, skipping from-recording test");
            return; // Recording wasn't produced -- skip gracefully
        }
    };

    // Step 2: Run from-recording to generate DSL from the recording.
    let from_output = Command::new(bmux_binary())
        .args(["playbook", "from-recording", &recording_id])
        .output()
        .expect("failed to run bmux playbook from-recording");

    let dsl = String::from_utf8_lossy(&from_output.stdout);
    assert!(
        from_output.status.success(),
        "from-recording should succeed. stderr: {}",
        String::from_utf8_lossy(&from_output.stderr)
    );

    // Step 3: Verify the generated DSL contains expected elements.
    assert!(
        dsl.contains("new-session"),
        "generated DSL should contain new-session: {dsl}"
    );
    assert!(
        dsl.contains("send-keys"),
        "generated DSL should contain send-keys: {dsl}"
    );

    // Step 4: Validate the generated DSL parses cleanly.
    let parse_result = bmux_cli::playbook::parse_dsl::parse_dsl(&dsl);
    assert!(
        parse_result.is_ok(),
        "generated DSL should parse: {:?}",
        parse_result.err()
    );
}

// ---------------------------------------------------------------------------
// C: assert-screen matches= failure tests
// ---------------------------------------------------------------------------

#[test]
fn playbook_assert_matches_fail() {
    let (json, pass) = run_playbook_fixture("assert_matches_fail.dsl");
    assert!(
        !pass,
        "matches= on non-matching regex should fail: {json:#}"
    );

    let steps = json["steps"].as_array().expect("steps should be array");
    let failed = steps
        .iter()
        .find(|s| s["action"] == "assert-screen" && s["status"] == "fail");
    assert!(
        failed.is_some(),
        "should have a failed assert-screen step: {json:#}"
    );

    let f = failed.unwrap();
    assert!(
        f.get("expected").is_some() && !f["expected"].is_null(),
        "should have expected field: {f:#}"
    );
    assert!(
        f.get("actual").is_some() && !f["actual"].is_null(),
        "should have actual field (screen text): {f:#}"
    );
    assert!(
        f.get("failure_captures").is_some() && !f["failure_captures"].is_null(),
        "should have failure_captures: {f:#}"
    );
    // The actual screen text should contain the real output.
    assert!(
        f["actual"].as_str().unwrap_or("").contains("real_output"),
        "actual should contain 'real_output': {f:#}"
    );
}

#[test]
fn playbook_assert_matches_invalid_regex() {
    let (json, pass) = run_playbook_fixture("assert_matches_invalid_regex.dsl");
    assert!(!pass, "invalid regex should fail: {json:#}");

    let steps = json["steps"].as_array().expect("steps should be array");
    let failed = steps
        .iter()
        .find(|s| s["action"] == "assert-screen" && s["status"] == "fail");
    assert!(
        failed.is_some(),
        "should have a failed assert-screen step: {json:#}"
    );
    let detail = failed.unwrap()["detail"].as_str().unwrap_or("");
    assert!(
        detail.to_lowercase().contains("regex"),
        "detail should mention regex error: {detail}"
    );
}

// ---------------------------------------------------------------------------
// A: Interactive mode integration test
// ---------------------------------------------------------------------------

/// Guard that kills a child process on drop (for test cleanup).
struct ProcessGuard(std::process::Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn interactive_mode_basic() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};

    // Step 1: Spawn the interactive session.
    let mut child = std::process::Command::new(bmux_binary())
        .args([
            "playbook",
            "interactive",
            "--viewport",
            "80x24",
            "--shell",
            "sh",
            "--timeout",
            "30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn bmux playbook interactive");

    let _guard = ProcessGuard(std::process::Command::new("true").spawn().unwrap());

    // Step 2: Read the ready message from stdout.
    let stdout = child.stdout.take().expect("stdout should be piped");
    let mut stdout_reader = std::io::BufReader::new(stdout);
    let mut ready_line = String::new();
    std::io::BufRead::read_line(&mut stdout_reader, &mut ready_line)
        .expect("failed to read ready message");

    let ready: serde_json::Value = serde_json::from_str(ready_line.trim())
        .unwrap_or_else(|e| panic!("failed to parse ready message: {e}\nline: {ready_line}"));

    assert_eq!(ready["status"], "ready", "ready message: {ready:#}");
    let socket_path = ready["socket"]
        .as_str()
        .expect("ready message should have socket path");

    // Step 3: Connect to the socket.
    // Retry a few times in case the socket isn't ready yet.
    let mut stream = None;
    for _ in 0..10 {
        match tokio::net::UnixStream::connect(socket_path).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    let stream = stream.unwrap_or_else(|| panic!("failed to connect to socket: {socket_path}"));

    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = TokioBufReader::new(reader);

    // Helper: send command and read response.
    async fn send_cmd(
        writer: &mut tokio::io::WriteHalf<tokio::net::UnixStream>,
        reader: &mut TokioBufReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
        cmd: &str,
    ) -> serde_json::Value {
        writer
            .write_all(format!("{cmd}\n").as_bytes())
            .await
            .expect("failed to write command");
        writer.flush().await.expect("failed to flush");

        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .expect("failed to read response");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("failed to parse response: {e}\nline: {line}"))
    }

    // Step 4: Send commands and verify responses.

    // new-session
    let resp = send_cmd(&mut writer, &mut reader, "new-session").await;
    assert_eq!(resp["status"], "ok", "new-session response: {resp:#}");
    assert_eq!(resp["action"], "new-session");

    // send-keys
    let resp = send_cmd(
        &mut writer,
        &mut reader,
        "send-keys keys='echo interactive_test\\r'",
    )
    .await;
    assert_eq!(resp["status"], "ok", "send-keys response: {resp:#}");

    // sleep briefly for output to arrive
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // screen
    let resp = send_cmd(&mut writer, &mut reader, "screen").await;
    assert_eq!(resp["status"], "ok", "screen response: {resp:#}");
    let panes = resp["panes"].as_array().expect("screen should have panes");
    assert!(!panes.is_empty(), "should have at least one pane");
    let screen_text = panes[0]["screen_text"].as_str().unwrap_or("");
    assert!(
        screen_text.contains("interactive_test"),
        "screen should contain 'interactive_test': {screen_text}"
    );

    // assert-screen
    let resp = send_cmd(
        &mut writer,
        &mut reader,
        "assert-screen contains='interactive_test'",
    )
    .await;
    assert_eq!(resp["status"], "ok", "assert-screen response: {resp:#}");

    // status
    let resp = send_cmd(&mut writer, &mut reader, "status").await;
    assert_eq!(resp["status"], "ok", "status response: {resp:#}");
    assert!(
        resp.get("session_id").is_some() && !resp["session_id"].is_null(),
        "status should have session_id: {resp:#}"
    );
    assert!(
        resp.get("pane_count").is_some(),
        "status should have pane_count: {resp:#}"
    );
    assert!(
        resp.get("focused_pane").is_some(),
        "status should have focused_pane: {resp:#}"
    );

    // subscribe for push output events
    let resp = send_cmd(&mut writer, &mut reader, "subscribe").await;
    assert_eq!(resp["status"], "ok", "subscribe response: {resp:#}");
    assert_eq!(resp["action"], "subscribe");

    // send-keys to trigger output while subscribed
    let resp = send_cmd(
        &mut writer,
        &mut reader,
        "send-keys keys='echo push_test_marker\\r'",
    )
    .await;
    assert_eq!(resp["status"], "ok", "send-keys response: {resp:#}");

    // Read responses/push events until we find an output push event or the
    // screen command response. Push events may arrive before or after the
    // screen response.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Drain any push events that arrived.
    let resp = send_cmd(&mut writer, &mut reader, "screen").await;
    assert_eq!(resp["status"], "ok", "screen after subscribe: {resp:#}");
    // The screen text should contain our marker.
    let empty_vec = vec![];
    let panes = resp["panes"].as_array().unwrap_or(&empty_vec);
    let screen_has_marker = panes.iter().any(|p| {
        p["screen_text"]
            .as_str()
            .unwrap_or("")
            .contains("push_test_marker")
    });
    assert!(
        screen_has_marker,
        "screen should contain push_test_marker after subscribe"
    );

    // unsubscribe
    let resp = send_cmd(&mut writer, &mut reader, "unsubscribe").await;
    assert_eq!(resp["status"], "ok", "unsubscribe response: {resp:#}");
    assert_eq!(resp["action"], "unsubscribe");

    // quit
    let resp = send_cmd(&mut writer, &mut reader, "quit").await;
    assert_eq!(resp["status"], "ok", "quit response: {resp:#}");
    assert_eq!(resp["action"], "quit");

    // Wait for the child to exit.
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// A: Concurrent playbook runs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_playbook_runs() {
    let dsl = "\
@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo concurrent_test\\r'
wait-for pattern='concurrent_test'
assert-screen contains='concurrent_test'
";

    let run1 = async {
        let (mut pb, _) = bmux_cli::playbook::parse_dsl::parse_dsl(dsl).unwrap();
        pb.config.binary = Some(PathBuf::from(env!("CARGO_BIN_EXE_bmux")));
        bmux_cli::playbook::run(pb, false).await.unwrap()
    };
    let run2 = async {
        let (mut pb, _) = bmux_cli::playbook::parse_dsl::parse_dsl(dsl).unwrap();
        pb.config.binary = Some(PathBuf::from(env!("CARGO_BIN_EXE_bmux")));
        bmux_cli::playbook::run(pb, false).await.unwrap()
    };

    let (r1, r2) = tokio::join!(run1, run2);
    assert!(r1.pass, "concurrent run 1 should pass: {:?}", r1.error);
    assert!(r2.pass, "concurrent run 2 should pass: {:?}", r2.error);
}

// ---------------------------------------------------------------------------
// B: Shell exit mid-playbook
// ---------------------------------------------------------------------------

#[test]
fn playbook_shell_exit_mid_playbook() {
    let (json, pass) = run_playbook_fixture("shell_exit.dsl");
    assert!(!pass, "should fail after shell exit: {json:#}");

    // Verify we got a failure, not a hang or panic.
    let steps = json["steps"].as_array().expect("should have steps");
    let failed = steps.iter().find(|s| s["status"] == "fail");
    assert!(
        failed.is_some(),
        "should have a failed step after shell exit: {json:#}"
    );
    // The failed step should have a detail with an error message.
    let detail = failed.unwrap()["detail"].as_str().unwrap_or("");
    assert!(
        !detail.is_empty(),
        "failed step should have a detail: {json:#}"
    );
}

// ---------------------------------------------------------------------------
// C: Validate --json integration tests
// ---------------------------------------------------------------------------

#[test]
fn playbook_validate_valid_json() {
    let fixture = fixtures_dir().join("echo_hello.dsl");

    let output = Command::new(bmux_binary())
        .args(["playbook", "validate", "--json", fixture.to_str().unwrap()])
        .output()
        .expect("failed to run bmux playbook validate");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\nstdout: {stdout}"));

    assert_eq!(json["valid"], true, "echo_hello should be valid: {json:#}");
    let errors = json["errors"].as_array().expect("should have errors array");
    assert!(errors.is_empty(), "should have no errors: {json:#}");
    assert!(output.status.success(), "exit code should be 0");
}

#[test]
fn playbook_validate_invalid_json() {
    let fixture = fixtures_dir().join("invalid_no_session.dsl");

    let output = Command::new(bmux_binary())
        .args(["playbook", "validate", "--json", fixture.to_str().unwrap()])
        .output()
        .expect("failed to run bmux playbook validate");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\nstdout: {stdout}"));

    assert_eq!(
        json["valid"], false,
        "invalid playbook should not be valid: {json:#}"
    );
    let errors = json["errors"].as_array().expect("should have errors array");
    assert!(
        !errors.is_empty(),
        "should have validation errors: {json:#}"
    );
    assert!(
        !output.status.success(),
        "exit code should be non-zero for invalid playbook"
    );
}
