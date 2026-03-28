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
