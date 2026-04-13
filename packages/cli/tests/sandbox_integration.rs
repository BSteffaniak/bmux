use serial_test::serial;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

fn bmux_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bmux"))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root from package path")
        .to_path_buf()
}

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bmux-sandbox-integration-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct CommandSandbox {
    root: TempDirGuard,
}

impl CommandSandbox {
    fn new(label: &str) -> Self {
        let root = TempDirGuard::new(label);
        for dir in ["config", "runtime", "data", "state", "logs", "tmp-root"] {
            std::fs::create_dir_all(root.path().join(dir)).expect("create sandbox dir");
        }
        Self { root }
    }

    fn command(&self) -> Command {
        sandbox_command_for_root(self.root.path())
    }

    fn write_config(&self, toml: &str) {
        std::fs::write(self.root.path().join("config").join("bmux.toml"), toml)
            .expect("write sandbox config file");
    }

    fn sandbox_index_path(&self) -> PathBuf {
        self.root
            .path()
            .join("state")
            .join("sandbox")
            .join("index.json")
    }
}

fn sandbox_command_for_root(root: &Path) -> Command {
    let mut command = Command::new(bmux_binary());
    command
        .current_dir(workspace_root())
        .env("BMUX_CONFIG_DIR", root.join("config"))
        .env("BMUX_RUNTIME_DIR", root.join("runtime"))
        .env("BMUX_DATA_DIR", root.join("data"))
        .env("BMUX_STATE_DIR", root.join("state"))
        .env("BMUX_LOG_DIR", root.join("logs"))
        .env("BMUX_TARGET", "")
        .env("TMPDIR", root.join("tmp-root"));
    command
}

fn parse_json_stdout(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let start = stdout
        .find('{')
        .unwrap_or_else(|| panic!("stdout did not include json object: {stdout}"));
    serde_json::from_str(&stdout[start..])
        .unwrap_or_else(|error| panic!("stdout was not json: {error}; stdout={stdout}"))
}

fn assert_schema_version(payload: &serde_json::Value) {
    assert_eq!(
        payload["schema_version"].as_u64(),
        Some(1),
        "json payload should include schema_version=1"
    );
}

fn create_manifest_sandbox(root: &Path, dir_name: &str, source: &str, status: &str) {
    let dir = root.join(dir_name);
    let logs = dir.join("logs");
    let runtime = dir.join("runtime");
    let state = dir.join("state");
    std::fs::create_dir_all(&logs).expect("create logs dir");
    std::fs::create_dir_all(&runtime).expect("create runtime dir");
    std::fs::create_dir_all(&state).expect("create state dir");

    let manifest = serde_json::json!({
        "id": dir_name,
        "source": source,
        "created_at_unix_ms": 1,
        "updated_at_unix_ms": 1,
        "pid": 999_999,
        "bmux_bin": "bmux",
        "command": ["--version"],
        "env_mode": "clean",
        "status": status,
        "exit_code": if status == "failed" { serde_json::json!(1) } else { serde_json::json!(0) },
        "kept": true,
        "paths": {
            "root": dir.to_string_lossy(),
            "logs": logs.to_string_lossy(),
            "runtime": runtime.to_string_lossy(),
            "state": state.to_string_lossy(),
        }
    });
    std::fs::write(
        dir.join("sandbox.json"),
        serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write sandbox manifest");
}

fn write_index_entries(path: &Path, entries: serde_json::Value) {
    let parent = path.parent().expect("index path parent should exist");
    std::fs::create_dir_all(parent).expect("create sandbox index dir");
    let payload = serde_json::json!({
        "schema_version": 1,
        "entries": entries,
    });
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&payload).expect("serialize index payload"),
    )
    .expect("write sandbox index payload");
}

fn write_stale_lock(root: &Path, pid: u32) {
    let lock = serde_json::json!({
        "pid": pid,
        "updated_at_unix_ms": 1,
    });
    std::fs::write(
        root.join("sandbox.lock"),
        serde_json::to_vec(&lock).expect("serialize lock payload"),
    )
    .expect("write stale sandbox lock");
}

#[test]
#[serial]
fn sandbox_dev_prefers_workspace_debug_binary() {
    let sandbox = CommandSandbox::new("dev-prefers-debug-binary");
    let output = sandbox
        .command()
        .args(["sandbox", "dev", "--json", "--", "--version"])
        .output()
        .expect("run bmux sandbox dev");
    assert!(
        output.status.success(),
        "sandbox dev should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    let expected = workspace_root().join("target").join("debug").join("bmux");
    if expected.exists() {
        let bmux_bin = json["bmux_bin"]
            .as_str()
            .expect("sandbox dev json should include bmux_bin");
        assert_eq!(
            Path::new(bmux_bin),
            expected,
            "sandbox dev should prefer workspace debug binary"
        );
    }
}

#[test]
#[serial]
fn sandbox_inspect_latest_and_latest_failed_resolve_expected_runs() {
    let sandbox = CommandSandbox::new("inspect-latest");

    let success_output = sandbox
        .command()
        .args([
            "sandbox",
            "run",
            "--json",
            "--keep",
            "--name",
            "latest-success",
            "--",
            "--version",
        ])
        .output()
        .expect("run successful sandbox command");
    assert!(
        success_output.status.success(),
        "successful sandbox run should pass"
    );

    let failed_output = sandbox
        .command()
        .args([
            "sandbox",
            "run",
            "--json",
            "--name",
            "latest-failed",
            "--",
            "no-such-command",
        ])
        .output()
        .expect("run failed sandbox command");
    assert!(
        !failed_output.status.success(),
        "failed sandbox run should return nonzero"
    );

    let latest = sandbox
        .command()
        .args(["sandbox", "inspect", "--latest", "--json"])
        .output()
        .expect("inspect latest sandbox");
    assert!(latest.status.success(), "inspect latest should succeed");
    let latest_json = parse_json_stdout(&latest);
    assert_schema_version(&latest_json);
    let latest_status = latest_json["manifest"]["status"]
        .as_str()
        .expect("latest inspect should include manifest status");
    assert_eq!(latest_status, "failed");

    let latest_failed = sandbox
        .command()
        .args(["sandbox", "inspect", "--latest-failed", "--json"])
        .output()
        .expect("inspect latest failed sandbox");
    assert!(
        latest_failed.status.success(),
        "inspect latest failed should succeed"
    );
    let latest_failed_json = parse_json_stdout(&latest_failed);
    assert_schema_version(&latest_failed_json);
    let command = latest_failed_json["manifest"]["command"]
        .as_array()
        .expect("latest failed inspect should include command array");
    assert_eq!(
        command.first().and_then(serde_json::Value::as_str),
        Some("no-such-command")
    );
}

#[test]
#[serial]
fn sandbox_bundle_writes_manifest_logs_and_repro() {
    let sandbox = CommandSandbox::new("bundle-contents");

    let run_output = sandbox
        .command()
        .args([
            "sandbox",
            "run",
            "--json",
            "--name",
            "bundle-source",
            "--",
            "no-such-command",
        ])
        .output()
        .expect("run failed sandbox for bundle source");
    assert!(
        !run_output.status.success(),
        "source sandbox should fail and be kept"
    );
    let run_json = parse_json_stdout(&run_output);
    assert_schema_version(&run_json);
    let sandbox_id = run_json["sandbox_id"]
        .as_str()
        .expect("sandbox run json should include sandbox_id")
        .to_string();

    let bundle_output = sandbox
        .command()
        .args([
            "sandbox",
            "bundle",
            sandbox_id.as_str(),
            "--output",
            sandbox
                .root
                .path()
                .join("bundles")
                .to_string_lossy()
                .as_ref(),
            "--json",
        ])
        .output()
        .expect("bundle sandbox artifacts");
    assert!(
        bundle_output.status.success(),
        "bundle should succeed; stderr={}; stdout={}",
        String::from_utf8_lossy(&bundle_output.stderr),
        String::from_utf8_lossy(&bundle_output.stdout)
    );

    let bundle_json = parse_json_stdout(&bundle_output);
    assert_schema_version(&bundle_json);
    let bundle_dir = PathBuf::from(
        bundle_json["bundle_dir"]
            .as_str()
            .expect("bundle json should include bundle_dir"),
    );
    assert!(
        bundle_dir.join("sandbox.json").exists(),
        "bundle should include manifest"
    );
    assert!(
        bundle_dir.join("logs").exists(),
        "bundle should include logs directory"
    );
    assert!(
        bundle_dir.join("repro.txt").exists(),
        "bundle should include repro command"
    );
    assert!(
        bundle_dir.join("bundle_manifest.json").exists(),
        "bundle should include bundle manifest metadata"
    );

    let repro =
        std::fs::read_to_string(bundle_dir.join("repro.txt")).expect("read bundled repro command");
    assert!(
        repro.contains("bmux sandbox run"),
        "repro command should include sandbox run"
    );
}

#[test]
#[serial]
fn sandbox_bundle_includes_optional_artifacts_when_requested() {
    let sandbox = CommandSandbox::new("bundle-optional-artifacts");

    let run_output = sandbox
        .command()
        .args([
            "sandbox",
            "run",
            "--json",
            "--name",
            "bundle-optional-source",
            "--",
            "no-such-command",
        ])
        .output()
        .expect("run failed sandbox for optional bundle source");
    assert!(
        !run_output.status.success(),
        "source sandbox should fail and be kept"
    );
    let run_json = parse_json_stdout(&run_output);
    assert_schema_version(&run_json);
    let sandbox_id = run_json["sandbox_id"]
        .as_str()
        .expect("sandbox run json should include sandbox_id")
        .to_string();

    let bundle_output = sandbox
        .command()
        .args([
            "sandbox",
            "bundle",
            sandbox_id.as_str(),
            "--include-env",
            "--include-index-state",
            "--include-doctor",
            "--output",
            sandbox
                .root
                .path()
                .join("bundles")
                .to_string_lossy()
                .as_ref(),
            "--json",
        ])
        .output()
        .expect("bundle sandbox artifacts with optional files");
    assert!(
        bundle_output.status.success(),
        "bundle should succeed; stderr={}; stdout={}",
        String::from_utf8_lossy(&bundle_output.stderr),
        String::from_utf8_lossy(&bundle_output.stdout)
    );

    let bundle_json = parse_json_stdout(&bundle_output);
    assert_schema_version(&bundle_json);
    let bundle_dir = PathBuf::from(
        bundle_json["bundle_dir"]
            .as_str()
            .expect("bundle json should include bundle_dir"),
    );

    assert!(
        bundle_dir.join("env.json").exists(),
        "bundle should include env snapshot"
    );
    assert!(
        bundle_dir.join("sandbox-index-entry.json").exists(),
        "bundle should include index entry snapshot"
    );
    assert!(
        bundle_dir.join("doctor.json").exists(),
        "bundle should include doctor snapshot"
    );

    let metadata: serde_json::Value = serde_json::from_slice(
        &std::fs::read(bundle_dir.join("bundle_manifest.json")).expect("read bundle metadata"),
    )
    .expect("parse bundle metadata");
    assert_eq!(metadata["includes"]["env"].as_bool(), Some(true));
    assert_eq!(metadata["includes"]["index_state"].as_bool(), Some(true));
    assert_eq!(metadata["includes"]["doctor"].as_bool(), Some(true));

    let artifact_metadata = metadata["artifact_metadata"]
        .as_array()
        .expect("bundle metadata should include artifact metadata");
    let env_entry = artifact_metadata
        .iter()
        .find(|entry| entry["path"].as_str() == Some("env.json"))
        .expect("env metadata entry should exist");
    assert_eq!(env_entry["kind"].as_str(), Some("file"));
    assert_eq!(env_entry["exists"].as_bool(), Some(true));
    assert!(
        env_entry["bytes"].as_u64().is_some_and(|bytes| bytes > 0),
        "env metadata should include file bytes"
    );

    let logs_entry = artifact_metadata
        .iter()
        .find(|entry| entry["path"].as_str() == Some("logs/"))
        .expect("logs metadata entry should exist");
    assert_eq!(logs_entry["kind"].as_str(), Some("directory"));
    assert_eq!(logs_entry["exists"].as_bool(), Some(true));
    assert!(
        logs_entry["file_count"]
            .as_u64()
            .is_some_and(|count| count >= 1),
        "logs metadata should include at least one file"
    );
}

#[test]
#[serial]
fn sandbox_bundle_verify_flag_runs_post_bundle_verification() {
    let sandbox = CommandSandbox::new("bundle-verify-flag");

    let run_output = sandbox
        .command()
        .args([
            "sandbox",
            "run",
            "--json",
            "--name",
            "bundle-verify-source",
            "--",
            "no-such-command",
        ])
        .output()
        .expect("run failed sandbox for bundle verify source");
    assert!(
        !run_output.status.success(),
        "source sandbox should fail and be kept"
    );
    let run_json = parse_json_stdout(&run_output);
    assert_schema_version(&run_json);
    let sandbox_id = run_json["sandbox_id"]
        .as_str()
        .expect("sandbox run json should include sandbox_id")
        .to_string();

    let bundle_output = sandbox
        .command()
        .args([
            "sandbox",
            "bundle",
            sandbox_id.as_str(),
            "--include-env",
            "--verify",
            "--output",
            sandbox
                .root
                .path()
                .join("bundles")
                .to_string_lossy()
                .as_ref(),
            "--json",
        ])
        .output()
        .expect("bundle sandbox artifacts with --verify");
    assert!(
        bundle_output.status.success(),
        "bundle --verify should succeed; stderr={}; stdout={}",
        String::from_utf8_lossy(&bundle_output.stderr),
        String::from_utf8_lossy(&bundle_output.stdout)
    );

    let bundle_json = parse_json_stdout(&bundle_output);
    assert_schema_version(&bundle_json);
    assert_eq!(bundle_json["verify"]["ok"].as_bool(), Some(true));
    assert_eq!(bundle_json["verify"]["issue_count"].as_u64(), Some(0));
    assert_eq!(
        bundle_json["verify"]["mode"].as_str(),
        Some("strict_metadata")
    );
}

#[test]
#[serial]
fn sandbox_verify_bundle_reports_ok_for_fresh_bundle() {
    let sandbox = CommandSandbox::new("verify-bundle-ok");

    let run_output = sandbox
        .command()
        .args([
            "sandbox",
            "run",
            "--json",
            "--name",
            "verify-bundle-source",
            "--",
            "no-such-command",
        ])
        .output()
        .expect("run failed sandbox for verify-bundle source");
    assert!(
        !run_output.status.success(),
        "source sandbox should fail and be kept"
    );
    let run_json = parse_json_stdout(&run_output);
    assert_schema_version(&run_json);
    let sandbox_id = run_json["sandbox_id"]
        .as_str()
        .expect("sandbox run json should include sandbox_id")
        .to_string();

    let bundle_output = sandbox
        .command()
        .args([
            "sandbox",
            "bundle",
            sandbox_id.as_str(),
            "--include-env",
            "--include-index-state",
            "--include-doctor",
            "--output",
            sandbox
                .root
                .path()
                .join("bundles")
                .to_string_lossy()
                .as_ref(),
            "--json",
        ])
        .output()
        .expect("bundle sandbox artifacts for verify-bundle");
    assert!(bundle_output.status.success(), "bundle should succeed");
    let bundle_json = parse_json_stdout(&bundle_output);
    assert_schema_version(&bundle_json);
    let bundle_dir = bundle_json["bundle_dir"]
        .as_str()
        .expect("bundle json should include bundle_dir")
        .to_string();

    let verify_output = sandbox
        .command()
        .args(["sandbox", "verify-bundle", bundle_dir.as_str(), "--json"])
        .output()
        .expect("verify fresh bundle");
    assert!(
        verify_output.status.success(),
        "verify-bundle should succeed for untouched bundle: {}",
        String::from_utf8_lossy(&verify_output.stderr)
    );

    let verify_json = parse_json_stdout(&verify_output);
    assert_schema_version(&verify_json);
    assert_eq!(verify_json["ok"].as_bool(), Some(true));
    assert_eq!(verify_json["mode"].as_str(), Some("strict_metadata"));
    assert_eq!(verify_json["issue_count"].as_u64(), Some(0));
}

#[test]
#[serial]
fn sandbox_verify_bundle_detects_artifact_drift() {
    let sandbox = CommandSandbox::new("verify-bundle-drift");

    let run_output = sandbox
        .command()
        .args([
            "sandbox",
            "run",
            "--json",
            "--name",
            "verify-bundle-drift-source",
            "--",
            "no-such-command",
        ])
        .output()
        .expect("run failed sandbox for verify-bundle drift source");
    assert!(
        !run_output.status.success(),
        "source sandbox should fail and be kept"
    );
    let run_json = parse_json_stdout(&run_output);
    assert_schema_version(&run_json);
    let sandbox_id = run_json["sandbox_id"]
        .as_str()
        .expect("sandbox run json should include sandbox_id")
        .to_string();

    let bundle_output = sandbox
        .command()
        .args([
            "sandbox",
            "bundle",
            sandbox_id.as_str(),
            "--include-env",
            "--output",
            sandbox
                .root
                .path()
                .join("bundles")
                .to_string_lossy()
                .as_ref(),
            "--json",
        ])
        .output()
        .expect("bundle sandbox artifacts for drift verification");
    assert!(bundle_output.status.success(), "bundle should succeed");
    let bundle_json = parse_json_stdout(&bundle_output);
    assert_schema_version(&bundle_json);
    let bundle_dir = PathBuf::from(
        bundle_json["bundle_dir"]
            .as_str()
            .expect("bundle json should include bundle_dir"),
    );

    std::fs::remove_file(bundle_dir.join("env.json")).expect("remove env artifact for drift");

    let verify_output = sandbox
        .command()
        .args([
            "sandbox",
            "verify-bundle",
            bundle_dir.to_string_lossy().as_ref(),
            "--json",
        ])
        .output()
        .expect("verify drifted bundle");
    assert!(
        !verify_output.status.success(),
        "verify-bundle should fail when artifact drift is present"
    );

    let verify_json = parse_json_stdout(&verify_output);
    assert_schema_version(&verify_json);
    assert_eq!(verify_json["ok"].as_bool(), Some(false));
    assert_eq!(verify_json["mode"].as_str(), Some("strict_metadata"));
    assert!(
        verify_json["issue_count"]
            .as_u64()
            .is_some_and(|count| count >= 1),
        "verify-bundle should report at least one issue"
    );
    let issues = verify_json["issues"]
        .as_array()
        .expect("issues should be an array");
    assert!(
        issues.iter().any(|issue| {
            issue["path"].as_str() == Some("env.json") && issue["field"].as_str() == Some("exists")
        }),
        "issues should include missing env.json existence mismatch"
    );
}

#[test]
#[serial]
fn sandbox_inspect_explicit_id_resolves_target_manifest() {
    let sandbox = CommandSandbox::new("inspect-explicit-id");

    let run_output = sandbox
        .command()
        .args([
            "sandbox",
            "run",
            "--json",
            "--keep",
            "--name",
            "explicit-id",
            "--",
            "--version",
        ])
        .output()
        .expect("run sandbox for explicit id inspect");
    assert!(
        run_output.status.success(),
        "sandbox run should succeed: {}",
        String::from_utf8_lossy(&run_output.stderr)
    );

    let run_json = parse_json_stdout(&run_output);
    assert_schema_version(&run_json);
    let sandbox_id = run_json["sandbox_id"]
        .as_str()
        .expect("sandbox run json should include sandbox_id")
        .to_string();

    let inspect_output = sandbox
        .command()
        .args(["sandbox", "inspect", sandbox_id.as_str(), "--json"])
        .output()
        .expect("inspect explicit sandbox id");
    assert!(
        inspect_output.status.success(),
        "inspect explicit id should succeed: {}",
        String::from_utf8_lossy(&inspect_output.stderr)
    );

    let inspect_json = parse_json_stdout(&inspect_output);
    assert_schema_version(&inspect_json);
    let manifest_id = inspect_json["manifest"]["id"]
        .as_str()
        .expect("inspect output should include manifest id");
    assert_eq!(manifest_id, sandbox_id);
}

#[test]
#[serial]
fn sandbox_list_source_filter_returns_matching_source_only() {
    let sandbox = CommandSandbox::new("list-source-filter");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bpb-source-playbook", "playbook", "succeeded");
    create_manifest_sandbox(
        &tmp_root,
        "brv-source-recording",
        "recording-verify",
        "failed",
    );
    create_manifest_sandbox(&tmp_root, "bmux-sbx-source-cli", "sandbox-cli", "succeeded");

    let output = sandbox
        .command()
        .args([
            "sandbox", "list", "--source", "playbook", "--limit", "50", "--json",
        ])
        .output()
        .expect("run sandbox list with source filter");
    assert!(
        output.status.success(),
        "sandbox list should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    let sandboxes = json["sandboxes"]
        .as_array()
        .expect("sandbox list should include sandboxes array");
    assert_eq!(sandboxes.len(), 1, "only playbook source should be listed");
    assert_eq!(sandboxes[0]["source"].as_str(), Some("playbook"));
    assert_eq!(sandboxes[0]["id"].as_str(), Some("bpb-source-playbook"));
}

#[test]
#[serial]
fn sandbox_cleanup_source_filter_only_reports_requested_source() {
    let sandbox = CommandSandbox::new("cleanup-source-filter");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bpb-cleanup-playbook", "playbook", "failed");
    create_manifest_sandbox(
        &tmp_root,
        "brv-cleanup-recording",
        "recording-verify",
        "failed",
    );

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "cleanup",
            "--dry-run",
            "--older-than",
            "0",
            "--source",
            "recording-verify",
            "--json",
        ])
        .output()
        .expect("run sandbox cleanup with source filter");
    assert!(
        output.status.success(),
        "sandbox cleanup should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    let entries = json["entries"]
        .as_array()
        .expect("sandbox cleanup should include entries array");
    assert_eq!(entries.len(), 2, "cleanup should report filtered decisions");
    assert_eq!(json["skipped_source_mismatch"].as_u64(), Some(1));
    let matched = entries
        .iter()
        .find(|entry| entry["source"].as_str() == Some("recording-verify"))
        .expect("cleanup should include recording-verify entry");
    assert_eq!(matched["reason"].as_str(), Some("would_remove"));
    let path = matched["path"]
        .as_str()
        .expect("cleanup entry path should be present");
    assert!(
        path.contains("brv-cleanup-recording"),
        "cleanup entry should point to recording sandbox"
    );
}

#[test]
#[serial]
fn sandbox_inspect_latest_source_filter_resolves_matching_source() {
    let sandbox = CommandSandbox::new("inspect-source-filter");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bpb-inspect-playbook", "playbook", "succeeded");
    create_manifest_sandbox(
        &tmp_root,
        "brv-inspect-recording",
        "recording-verify",
        "failed",
    );

    let output = sandbox
        .command()
        .args([
            "sandbox", "inspect", "--latest", "--source", "playbook", "--json",
        ])
        .output()
        .expect("run sandbox inspect latest with source filter");
    assert!(
        output.status.success(),
        "sandbox inspect should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    let id = json["manifest"]["id"]
        .as_str()
        .expect("sandbox inspect should include manifest id");
    assert_eq!(id, "bpb-inspect-playbook");
    assert_eq!(json["manifest"]["source"].as_str(), Some("playbook"));
}

#[test]
#[serial]
fn sandbox_cleanup_uses_config_defaults_when_flags_are_omitted() {
    let sandbox = CommandSandbox::new("cleanup-config-defaults");
    sandbox.write_config(
        "[sandbox.cleanup]\nfailed_only = true\nolder_than_secs = 0\nsource = 'recording_verify'\n",
    );

    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "brv-config-failed", "recording-verify", "failed");
    create_manifest_sandbox(
        &tmp_root,
        "brv-config-succeeded",
        "recording-verify",
        "succeeded",
    );
    create_manifest_sandbox(&tmp_root, "bpb-config-failed", "playbook", "failed");

    let output = sandbox
        .command()
        .args(["sandbox", "cleanup", "--dry-run", "--json"])
        .output()
        .expect("run sandbox cleanup with config defaults");
    assert!(
        output.status.success(),
        "sandbox cleanup should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    let entries = json["entries"]
        .as_array()
        .expect("sandbox cleanup should include entries array");
    assert_eq!(
        entries.len(),
        3,
        "cleanup should report matched and skipped decisions"
    );
    assert_eq!(json["skipped_source_mismatch"].as_u64(), Some(1));
    assert_eq!(json["skipped_not_failed"].as_u64(), Some(1));
    assert_eq!(json["orphaned"].as_u64(), Some(1));
}

#[test]
#[serial]
fn sandbox_cleanup_cli_flags_override_config_defaults() {
    let sandbox = CommandSandbox::new("cleanup-config-overrides");
    sandbox.write_config(
        "[sandbox.cleanup]\nfailed_only = true\nolder_than_secs = 86_400\nsource = 'recording_verify'\n",
    );

    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(
        &tmp_root,
        "bpb-config-override-succeeded",
        "playbook",
        "succeeded",
    );

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "cleanup",
            "--dry-run",
            "--all-status",
            "--older-than",
            "0",
            "--source",
            "playbook",
            "--json",
        ])
        .output()
        .expect("run sandbox cleanup with explicit overrides");
    assert!(
        output.status.success(),
        "sandbox cleanup should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    let entries = json["entries"]
        .as_array()
        .expect("sandbox cleanup should include entries array");
    assert_eq!(
        entries.len(),
        1,
        "explicit flags should include playbook entry"
    );
    assert_eq!(entries[0]["source"].as_str(), Some("playbook"));
    assert_eq!(entries[0]["status"].as_str(), Some("stopped"));
    assert_eq!(entries[0]["reason"].as_str(), Some("would_remove"));
}

#[test]
#[serial]
fn sandbox_clean_uses_failed_only_defaults() {
    let sandbox = CommandSandbox::new("clean-defaults");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-clean-failed", "sandbox-cli", "failed");
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-clean-succeeded",
        "sandbox-cli",
        "succeeded",
    );

    write_index_entries(
        &sandbox.sandbox_index_path(),
        serde_json::json!([
            {
                "id": "bmux-sbx-clean-failed",
                "root": tmp_root.join("bmux-sbx-clean-failed").to_string_lossy().to_string(),
                "source": "sandbox-cli",
                "status": "failed",
                "created_at_unix_ms": 1,
                "updated_at_unix_ms": 1,
                "last_seen_unix_ms": 1
            },
            {
                "id": "bmux-sbx-clean-succeeded",
                "root": tmp_root
                    .join("bmux-sbx-clean-succeeded")
                    .to_string_lossy()
                    .to_string(),
                "source": "sandbox-cli",
                "status": "succeeded",
                "created_at_unix_ms": 1,
                "updated_at_unix_ms": 1,
                "last_seen_unix_ms": 1
            }
        ]),
    );

    let output = sandbox
        .command()
        .args(["sandbox", "clean", "--dry-run", "--json"])
        .output()
        .expect("run sandbox clean with defaults");
    assert!(
        output.status.success(),
        "sandbox clean should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["orphaned"].as_u64(), Some(1));
    assert_eq!(json["skipped_not_failed"].as_u64(), Some(1));
    let entries = json["entries"]
        .as_array()
        .expect("sandbox clean should include entries array");
    assert_eq!(entries.len(), 2);
}

#[test]
#[serial]
fn sandbox_cleanup_failed_only_includes_aborted_running_manifests() {
    let sandbox = CommandSandbox::new("cleanup-aborted-running");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-aborted-running",
        "sandbox-cli",
        "running",
    );
    create_manifest_sandbox(&tmp_root, "bmux-sbx-succeeded", "sandbox-cli", "succeeded");

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "cleanup",
            "--dry-run",
            "--failed-only",
            "--older-than",
            "0",
            "--json",
        ])
        .output()
        .expect("run sandbox cleanup for aborted-running manifests");
    assert!(
        output.status.success(),
        "sandbox cleanup should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    let entries = json["entries"]
        .as_array()
        .expect("sandbox cleanup should include entries array");
    assert_eq!(
        entries.len(),
        2,
        "cleanup should include failed match and not_failed skip"
    );
    let failed_entry = entries
        .iter()
        .find(|entry| entry["status"].as_str() == Some("failed"))
        .expect("cleanup should include failed status entry");
    assert_eq!(failed_entry["reason"].as_str(), Some("would_remove"));
    let path = failed_entry["path"]
        .as_str()
        .expect("cleanup entry should include path");
    assert!(path.contains("bmux-sbx-aborted-running"));
}

#[test]
#[serial]
fn sandbox_cleanup_reports_missing_manifest_reason() {
    let sandbox = CommandSandbox::new("cleanup-missing-manifest-reason");
    let tmp_root = sandbox.root.path().join("tmp-root");
    let missing_manifest = tmp_root.join("bmux-sbx-missing-manifest");
    std::fs::create_dir_all(missing_manifest.join("logs")).expect("create logs dir");
    std::fs::create_dir_all(missing_manifest.join("runtime")).expect("create runtime dir");
    std::fs::create_dir_all(missing_manifest.join("state")).expect("create state dir");

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "cleanup",
            "--dry-run",
            "--failed-only",
            "--older-than",
            "0",
            "--json",
        ])
        .output()
        .expect("run sandbox cleanup for missing-manifest reason");
    assert!(
        output.status.success(),
        "sandbox cleanup should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["skipped_missing_manifest"].as_u64(), Some(1));
    let entries = json["entries"]
        .as_array()
        .expect("sandbox cleanup should include entries array");
    let missing_manifest_entry = entries
        .iter()
        .find(|entry| entry["reason"].as_str() == Some("missing_manifest"))
        .expect("cleanup should include missing_manifest reason");
    assert_eq!(
        missing_manifest_entry["path"]
            .as_str()
            .expect("entry path should be present"),
        missing_manifest.to_string_lossy()
    );
}

#[test]
#[serial]
fn sandbox_run_spawn_failure_marks_manifest_aborted() {
    let sandbox = CommandSandbox::new("spawn-failure-aborted");
    let non_exec = sandbox.root.path().join("not-executable-bmux");
    std::fs::write(&non_exec, "#!/bin/sh\nexit 0\n").expect("write non-executable file");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(&non_exec)
            .expect("stat non-executable file")
            .permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&non_exec, permissions).expect("set non-executable permissions");
    }

    let run_output = sandbox
        .command()
        .args([
            "sandbox",
            "run",
            "--name",
            "spawn-failure",
            "--bmux-bin",
            non_exec.to_string_lossy().as_ref(),
            "--",
            "--version",
        ])
        .output()
        .expect("run sandbox with non-executable bmux binary");
    assert!(
        !run_output.status.success(),
        "sandbox run should fail to spawn: {}",
        String::from_utf8_lossy(&run_output.stderr)
    );

    let inspect_output = sandbox
        .command()
        .args(["sandbox", "inspect", "--latest-failed", "--json"])
        .output()
        .expect("inspect latest failed sandbox after spawn error");
    assert!(
        inspect_output.status.success(),
        "inspect latest failed should succeed: {}",
        String::from_utf8_lossy(&inspect_output.stderr)
    );

    let inspect_json = parse_json_stdout(&inspect_output);
    assert_schema_version(&inspect_json);
    assert_eq!(inspect_json["manifest"]["status"].as_str(), Some("aborted"));
    assert_eq!(inspect_json["running"].as_bool(), Some(false));
}

#[test]
#[serial]
fn sandbox_list_falls_back_to_scan_when_index_is_corrupt() {
    let sandbox = CommandSandbox::new("index-corrupt-fallback");
    let index_path = sandbox.sandbox_index_path();
    std::fs::create_dir_all(
        index_path
            .parent()
            .expect("sandbox index parent should exist"),
    )
    .expect("create sandbox index directory");
    std::fs::write(index_path, b"{not-json").expect("write corrupt sandbox index");

    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-index-fallback",
        "sandbox-cli",
        "succeeded",
    );

    let output = sandbox
        .command()
        .args(["sandbox", "list", "--limit", "50", "--json"])
        .output()
        .expect("run sandbox list with corrupt index");
    assert!(
        output.status.success(),
        "sandbox list should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(
        json["reconcile"]["scan_fallback_used"].as_bool(),
        Some(true)
    );
    assert!(
        json["reconcile"]["healed_entries"].as_u64().unwrap_or(0) >= 1,
        "corrupt index fallback should report healed entries"
    );
    let sandboxes = json["sandboxes"]
        .as_array()
        .expect("sandbox list should include sandboxes array");
    assert!(
        sandboxes
            .iter()
            .any(|entry| entry["id"].as_str() == Some("bmux-sbx-index-fallback")),
        "list should fall back to temp scan when index is corrupt"
    );
}

#[test]
#[serial]
fn sandbox_cleanup_removes_deleted_index_entry() {
    let sandbox = CommandSandbox::new("index-cleanup-prune");
    let tmp_root = sandbox.root.path().join("tmp-root");
    let sandbox_dir_name = "bmux-sbx-index-cleanup-delete";
    create_manifest_sandbox(&tmp_root, sandbox_dir_name, "sandbox-cli", "failed");
    let sandbox_path = tmp_root.join(sandbox_dir_name);
    let sandbox_path_string = sandbox_path.to_string_lossy().to_string();

    write_index_entries(
        &sandbox.sandbox_index_path(),
        serde_json::json!([
            {
                "id": sandbox_dir_name,
                "root": sandbox_path_string,
                "source": "sandbox-cli",
                "status": "failed",
                "created_at_unix_ms": 1,
                "updated_at_unix_ms": 1,
                "last_seen_unix_ms": 1
            }
        ]),
    );

    let cleanup_output = sandbox
        .command()
        .args([
            "sandbox",
            "cleanup",
            "--failed-only",
            "--older-than",
            "0",
            "--json",
        ])
        .output()
        .expect("run sandbox cleanup to remove indexed sandbox");
    assert!(
        cleanup_output.status.success(),
        "sandbox cleanup should succeed: {}",
        String::from_utf8_lossy(&cleanup_output.stderr)
    );

    let cleanup_json = parse_json_stdout(&cleanup_output);
    assert_schema_version(&cleanup_json);
    let entries = cleanup_json["entries"]
        .as_array()
        .expect("sandbox cleanup should include entries array");
    assert_eq!(entries.len(), 1, "cleanup should remove one sandbox");
    assert_eq!(entries[0]["removed"].as_bool(), Some(true));
    assert!(
        !sandbox_path.exists(),
        "sandbox directory should be removed"
    );

    let index_contents = std::fs::read_to_string(sandbox.sandbox_index_path())
        .expect("read sandbox index after cleanup");
    let index_json: serde_json::Value =
        serde_json::from_str(&index_contents).expect("parse sandbox index json");
    let index_entries = index_json["entries"]
        .as_array()
        .expect("index should contain entries array");
    assert!(index_entries.is_empty(), "cleanup should prune index entry");
}

#[test]
#[serial]
fn sandbox_inspect_latest_prefers_index_updated_timestamp_ordering() {
    let sandbox = CommandSandbox::new("index-latest-ordering");
    let tmp_root = sandbox.root.path().join("tmp-root");

    create_manifest_sandbox(&tmp_root, "bmux-sbx-index-old", "sandbox-cli", "succeeded");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-index-new", "sandbox-cli", "succeeded");

    write_index_entries(
        &sandbox.sandbox_index_path(),
        serde_json::json!([
            {
                "id": "bmux-sbx-index-old",
                "root": tmp_root.join("bmux-sbx-index-old").to_string_lossy().to_string(),
                "source": "sandbox-cli",
                "status": "succeeded",
                "created_at_unix_ms": 1,
                "updated_at_unix_ms": 10,
                "last_seen_unix_ms": 10
            },
            {
                "id": "bmux-sbx-index-new",
                "root": tmp_root.join("bmux-sbx-index-new").to_string_lossy().to_string(),
                "source": "sandbox-cli",
                "status": "succeeded",
                "created_at_unix_ms": 1,
                "updated_at_unix_ms": 20,
                "last_seen_unix_ms": 20
            }
        ]),
    );

    let output = sandbox
        .command()
        .args(["sandbox", "inspect", "--latest", "--json"])
        .output()
        .expect("run sandbox inspect latest with indexed order");
    assert!(
        output.status.success(),
        "sandbox inspect should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(
        json["manifest"]["id"].as_str(),
        Some("bmux-sbx-index-new"),
        "latest inspect should pick highest index updated_at"
    );
}

#[test]
#[serial]
fn sandbox_list_prunes_missing_index_entries_during_reconcile() {
    let sandbox = CommandSandbox::new("index-prune-missing-roots");
    let tmp_root = sandbox.root.path().join("tmp-root");

    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-index-existing",
        "sandbox-cli",
        "succeeded",
    );

    write_index_entries(
        &sandbox.sandbox_index_path(),
        serde_json::json!([
            {
                "id": "bmux-sbx-index-missing",
                "root": tmp_root.join("bmux-sbx-index-missing").to_string_lossy().to_string(),
                "source": "sandbox-cli",
                "status": "succeeded",
                "created_at_unix_ms": 1,
                "updated_at_unix_ms": 5,
                "last_seen_unix_ms": 5
            },
            {
                "id": "bmux-sbx-index-existing",
                "root": tmp_root
                    .join("bmux-sbx-index-existing")
                    .to_string_lossy()
                    .to_string(),
                "source": "sandbox-cli",
                "status": "succeeded",
                "created_at_unix_ms": 1,
                "updated_at_unix_ms": 10,
                "last_seen_unix_ms": 10
            }
        ]),
    );

    let output = sandbox
        .command()
        .args(["sandbox", "list", "--limit", "50", "--json"])
        .output()
        .expect("run sandbox list for index reconcile");
    assert!(
        output.status.success(),
        "sandbox list should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    let sandboxes = json["sandboxes"]
        .as_array()
        .expect("sandbox list should include sandboxes array");
    assert_eq!(sandboxes.len(), 1, "only existing sandbox should remain");
    assert_eq!(sandboxes[0]["id"].as_str(), Some("bmux-sbx-index-existing"));

    let index_contents = std::fs::read_to_string(sandbox.sandbox_index_path())
        .expect("read sandbox index after list reconcile");
    let index_json: serde_json::Value =
        serde_json::from_str(&index_contents).expect("parse sandbox index json");
    let index_entries = index_json["entries"]
        .as_array()
        .expect("index should contain entries array");
    assert_eq!(
        index_entries.len(),
        1,
        "missing index entry should be pruned"
    );
    assert_eq!(
        index_entries[0]["id"].as_str(),
        Some("bmux-sbx-index-existing")
    );
}

#[test]
#[serial]
fn sandbox_rebuild_index_recreates_missing_index() {
    let sandbox = CommandSandbox::new("rebuild-index-missing");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-rebuild-a", "sandbox-cli", "succeeded");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-rebuild-b", "playbook", "failed");

    let output = sandbox
        .command()
        .args(["sandbox", "rebuild-index", "--json"])
        .output()
        .expect("run sandbox rebuild-index with missing index");
    assert!(
        output.status.success(),
        "sandbox rebuild-index should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["rebuilt_count"].as_u64(), Some(2));
    assert_eq!(json["missing_manifest"].as_u64(), Some(0));

    let index_contents =
        std::fs::read_to_string(sandbox.sandbox_index_path()).expect("read rebuilt sandbox index");
    let index_json: serde_json::Value =
        serde_json::from_str(&index_contents).expect("parse sandbox index json");
    let index_entries = index_json["entries"]
        .as_array()
        .expect("index should contain entries array");
    assert_eq!(
        index_entries.len(),
        2,
        "rebuild should write both manifests"
    );
}

#[test]
#[serial]
fn sandbox_rebuild_index_recovers_from_corrupt_index() {
    let sandbox = CommandSandbox::new("rebuild-index-corrupt");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-rebuild-corrupt",
        "recording-verify",
        "failed",
    );

    let index_path = sandbox.sandbox_index_path();
    std::fs::create_dir_all(
        index_path
            .parent()
            .expect("sandbox index parent should exist"),
    )
    .expect("create sandbox index directory");
    std::fs::write(&index_path, b"{not-json").expect("write corrupt index file");

    let output = sandbox
        .command()
        .args(["sandbox", "rebuild-index", "--json"])
        .output()
        .expect("run sandbox rebuild-index with corrupt index");
    assert!(
        output.status.success(),
        "sandbox rebuild-index should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["rebuilt_count"].as_u64(), Some(1));
    assert_eq!(json["scan_fallback_used"].as_bool(), Some(true));

    let index_contents = std::fs::read_to_string(index_path).expect("read repaired sandbox index");
    let index_json: serde_json::Value =
        serde_json::from_str(&index_contents).expect("parse repaired index json");
    let index_entries = index_json["entries"]
        .as_array()
        .expect("index should contain entries array");
    assert_eq!(
        index_entries.len(),
        1,
        "rebuild should repair corrupt index"
    );
    assert_eq!(
        index_entries[0]["id"].as_str(),
        Some("bmux-sbx-rebuild-corrupt")
    );
}

#[test]
#[serial]
fn sandbox_parallel_runs_and_cleanup_keep_index_and_locks_consistent() {
    let sandbox = CommandSandbox::new("parallel-runs-cleanup");
    let root = sandbox.root.path().to_path_buf();
    let run_threads = 4usize;

    let mut handles = Vec::new();
    for index in 0..run_threads {
        let root_clone = root.clone();
        handles.push(thread::spawn(move || {
            let name = format!("parallel-run-{index}");
            sandbox_command_for_root(&root_clone)
                .args([
                    "sandbox",
                    "run",
                    "--keep",
                    "--name",
                    &name,
                    "--",
                    "--version",
                ])
                .output()
                .expect("run sandbox command in parallel")
        }));
    }

    let cleanup_root = root.clone();
    let cleanup_handle = thread::spawn(move || {
        for _ in 0..12 {
            let output = sandbox_command_for_root(&cleanup_root)
                .args([
                    "sandbox",
                    "cleanup",
                    "--dry-run",
                    "--older-than",
                    "0",
                    "--json",
                ])
                .output()
                .expect("run cleanup while runs are active");
            assert!(
                output.status.success(),
                "cleanup should succeed during parallel runs: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            thread::sleep(Duration::from_millis(15));
        }
    });

    for handle in handles {
        let output = handle.join().expect("join run thread");
        assert!(
            output.status.success(),
            "parallel sandbox run should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    cleanup_handle.join().expect("join cleanup thread");

    let tmp_root = root.join("tmp-root");
    for entry in std::fs::read_dir(&tmp_root).expect("read tmp-root entries") {
        let path = entry.expect("read tmp-root entry path").path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let name = name.to_string_lossy();
        if name.starts_with("bmux-sbx-") || name.starts_with("bpb-") || name.starts_with("brv-") {
            assert!(
                !path.join("sandbox.lock").exists(),
                "completed sandbox should not keep stale lock: {}",
                path.display()
            );
        }
    }

    let list_output = sandbox
        .command()
        .args(["sandbox", "list", "--limit", "100", "--json"])
        .output()
        .expect("run sandbox list after parallel operations");
    assert!(
        list_output.status.success(),
        "sandbox list should succeed: {}",
        String::from_utf8_lossy(&list_output.stderr)
    );
    let list_json = parse_json_stdout(&list_output);
    assert_schema_version(&list_json);
    let entries = list_json["sandboxes"]
        .as_array()
        .expect("sandbox list should include sandboxes array");
    assert!(
        entries.len() >= run_threads,
        "list should include completed parallel sandboxes"
    );

    let rebuild_output = sandbox
        .command()
        .args(["sandbox", "rebuild-index", "--json"])
        .output()
        .expect("rebuild index after parallel operations");
    assert!(
        rebuild_output.status.success(),
        "rebuild-index should succeed: {}",
        String::from_utf8_lossy(&rebuild_output.stderr)
    );
    let rebuild_json = parse_json_stdout(&rebuild_output);
    assert_schema_version(&rebuild_json);
    assert!(
        rebuild_json["rebuilt_count"].as_u64().unwrap_or(0) >= run_threads as u64,
        "rebuilt index should retain parallel sandbox entries"
    );
}

#[test]
#[serial]
fn sandbox_status_reports_source_counts_and_health() {
    let sandbox = CommandSandbox::new("status-summary");
    let tmp_root = sandbox.root.path().join("tmp-root");

    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-status-running",
        "sandbox-cli",
        "running",
    );
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-status-failed",
        "recording-verify",
        "failed",
    );
    create_manifest_sandbox(&tmp_root, "bpb-status-stopped", "playbook", "succeeded");

    let stale_lock_root = tmp_root.join("bmux-sbx-status-running");
    write_stale_lock(&stale_lock_root, 999_999);

    let output = sandbox
        .command()
        .args(["sandbox", "status", "--json"])
        .output()
        .expect("run sandbox status");
    assert!(
        output.status.success(),
        "sandbox status should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["totals"]["total"].as_u64(), Some(3));
    assert_eq!(json["totals"]["failed"].as_u64(), Some(2));
    assert_eq!(json["totals"]["stopped"].as_u64(), Some(1));

    let by_source = json["by_source"]
        .as_array()
        .expect("status json should include by_source array");
    assert_eq!(by_source.len(), 3);
    assert!(
        by_source
            .iter()
            .any(|source| source["source"].as_str() == Some("sandbox-cli")
                && source["failed"].as_u64() == Some(1))
    );
    assert!(by_source.iter().any(
        |source| source["source"].as_str() == Some("recording-verify")
            && source["failed"].as_u64() == Some(1)
    ));
    assert!(
        by_source
            .iter()
            .any(|source| source["source"].as_str() == Some("playbook")
                && source["stopped"].as_u64() == Some(1))
    );

    assert_eq!(json["health"]["stale_lock_count"].as_u64(), Some(0));
    assert_eq!(json["health"]["index_exists"].as_bool(), Some(true));
    assert_eq!(
        json["reconcile"]["scan_fallback_used"].as_bool(),
        Some(true)
    );
    assert!(
        json["reconcile"]["healed_entries"].as_u64().unwrap_or(0) >= 1,
        "status should surface reconcile heal count"
    );
    assert_eq!(
        json["reconcile"]["normalized_running"].as_u64(),
        Some(1),
        "status recovery should normalize stale running manifest"
    );
    assert_eq!(
        json["reconcile"]["cleared_stale_locks"].as_u64(),
        Some(1),
        "status recovery should clear stale lock files"
    );

    let recovered_manifest_path = stale_lock_root.join("sandbox.json");
    let recovered_manifest =
        std::fs::read_to_string(&recovered_manifest_path).expect("read recovered manifest");
    let recovered_json: serde_json::Value =
        serde_json::from_str(&recovered_manifest).expect("parse recovered manifest json");
    assert_eq!(recovered_json["status"].as_str(), Some("aborted"));
    assert!(
        !stale_lock_root.join("sandbox.lock").exists(),
        "stale lock should be removed during recovery"
    );
}

#[test]
#[serial]
fn sandbox_inspect_reports_reconcile_when_auto_heal_runs() {
    let sandbox = CommandSandbox::new("inspect-reconcile");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-inspect-reconcile",
        "sandbox-cli",
        "failed",
    );

    let index_path = sandbox.sandbox_index_path();
    std::fs::create_dir_all(
        index_path
            .parent()
            .expect("sandbox index parent should exist"),
    )
    .expect("create sandbox index directory");
    std::fs::write(index_path, b"{broken").expect("write corrupt index");

    let output = sandbox
        .command()
        .args(["sandbox", "inspect", "--latest-failed", "--json"])
        .output()
        .expect("run sandbox inspect latest-failed with reconcile");
    assert!(
        output.status.success(),
        "sandbox inspect should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(
        json["manifest"]["id"].as_str(),
        Some("bmux-sbx-inspect-reconcile")
    );
    assert_eq!(
        json["reconcile"]["scan_fallback_used"].as_bool(),
        Some(true)
    );
    assert!(
        json["reconcile"]["healed_entries"].as_u64().unwrap_or(0) >= 1,
        "inspect should surface reconcile heal count"
    );
}

#[test]
#[serial]
fn sandbox_doctor_fix_dry_run_reports_repairs_without_mutation() {
    let sandbox = CommandSandbox::new("doctor-fix-dry-run");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-doctor-dry", "sandbox-cli", "running");
    let root = tmp_root.join("bmux-sbx-doctor-dry");
    write_stale_lock(&root, 999_999);

    let output = sandbox
        .command()
        .args(["sandbox", "doctor", "--fix", "--dry-run", "--json"])
        .output()
        .expect("run sandbox doctor --fix --dry-run");
    assert!(
        output.status.success(),
        "sandbox doctor should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["fix"]["applied"].as_bool(), Some(false));
    assert_eq!(json["fix"]["dry_run"].as_bool(), Some(true));
    assert_eq!(json["fix"]["normalized_running"].as_u64(), Some(1));
    assert_eq!(json["fix"]["cleared_stale_locks"].as_u64(), Some(1));

    let manifest_contents = std::fs::read_to_string(root.join("sandbox.json"))
        .expect("read manifest after dry-run doctor fix");
    let manifest_json: serde_json::Value =
        serde_json::from_str(&manifest_contents).expect("parse manifest json");
    assert_eq!(manifest_json["status"].as_str(), Some("running"));
    assert!(
        root.join("sandbox.lock").exists(),
        "dry-run fix should not remove stale lock"
    );
}

#[test]
#[serial]
fn sandbox_doctor_fix_applies_recovery_and_rebuilds_index() {
    let sandbox = CommandSandbox::new("doctor-fix-apply");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-doctor-apply", "sandbox-cli", "running");
    let root = tmp_root.join("bmux-sbx-doctor-apply");
    write_stale_lock(&root, 999_999);

    let index_path = sandbox.sandbox_index_path();
    std::fs::create_dir_all(
        index_path
            .parent()
            .expect("sandbox index parent should exist"),
    )
    .expect("create sandbox index parent dir");
    std::fs::write(&index_path, b"{bad-json").expect("write corrupt index");

    let output = sandbox
        .command()
        .args(["sandbox", "doctor", "--fix", "--json"])
        .output()
        .expect("run sandbox doctor --fix");
    assert!(
        output.status.success(),
        "sandbox doctor should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["fix"]["applied"].as_bool(), Some(true));
    assert_eq!(json["fix"]["dry_run"].as_bool(), Some(false));
    assert_eq!(json["fix"]["normalized_running"].as_u64(), Some(1));
    assert_eq!(json["fix"]["cleared_stale_locks"].as_u64(), Some(1));
    assert_eq!(json["fix"]["index_rebuilt"].as_bool(), Some(true));
    assert!(
        json["fix"]["rebuilt_count"].as_u64().unwrap_or(0) >= 1,
        "doctor fix should rebuild index entries"
    );

    let manifest_contents =
        std::fs::read_to_string(root.join("sandbox.json")).expect("read manifest after doctor fix");
    let manifest_json: serde_json::Value =
        serde_json::from_str(&manifest_contents).expect("parse manifest json");
    assert_eq!(manifest_json["status"].as_str(), Some("aborted"));
    assert!(
        !root.join("sandbox.lock").exists(),
        "doctor fix should clear stale lock"
    );

    let index_contents = std::fs::read_to_string(index_path).expect("read repaired index");
    let index_json: serde_json::Value =
        serde_json::from_str(&index_contents).expect("parse repaired index json");
    let entries = index_json["entries"]
        .as_array()
        .expect("index should contain entries array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"].as_str(), Some("bmux-sbx-doctor-apply"));
}

#[test]
#[serial]
fn sandbox_tail_returns_log_lines_for_target() {
    let sandbox = CommandSandbox::new("tail-shortcut");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-tail-target", "sandbox-cli", "failed");
    let log_path = tmp_root
        .join("bmux-sbx-tail-target")
        .join("logs")
        .join("run.log");
    std::fs::write(&log_path, "line-a\nline-b\nline-c\n").expect("write sandbox log file");

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "tail",
            "bmux-sbx-tail-target",
            "--tail",
            "2",
            "--json",
        ])
        .output()
        .expect("run sandbox tail");
    assert!(
        output.status.success(),
        "sandbox tail should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["id"].as_str(), Some("bmux-sbx-tail-target"));
    assert_eq!(json["source"].as_str(), Some("sandbox-cli"));
    assert_eq!(json["status"].as_str(), Some("failed"));
    let log_tail = json["log_tail"]
        .as_array()
        .expect("tail output should include log_tail array");
    assert_eq!(log_tail.len(), 2);
    assert_eq!(log_tail[0].as_str(), Some("line-b"));
    assert_eq!(log_tail[1].as_str(), Some("line-c"));
}

#[test]
#[serial]
fn sandbox_open_returns_paths_and_repro() {
    let sandbox = CommandSandbox::new("open-shortcut");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-open-target", "sandbox-cli", "failed");
    let log_path = tmp_root
        .join("bmux-sbx-open-target")
        .join("logs")
        .join("run.log");
    std::fs::write(&log_path, "open-log\n").expect("write sandbox log file");

    let output = sandbox
        .command()
        .args(["sandbox", "open", "bmux-sbx-open-target", "--json"])
        .output()
        .expect("run sandbox open");
    assert!(
        output.status.success(),
        "sandbox open should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["id"].as_str(), Some("bmux-sbx-open-target"));
    assert_eq!(json["source"].as_str(), Some("sandbox-cli"));
    assert_eq!(json["status"].as_str(), Some("failed"));
    let root = json["root"]
        .as_str()
        .expect("open output should include root");
    assert!(root.contains("bmux-sbx-open-target"));
    assert_eq!(
        json["latest_log"].as_str(),
        Some(log_path.to_string_lossy().as_ref())
    );
    assert!(
        json["repro"]
            .as_str()
            .unwrap_or_default()
            .contains("--version"),
        "open output should include repro command"
    );
}

#[test]
#[serial]
fn sandbox_rerun_executes_command_from_manifest() {
    let sandbox = CommandSandbox::new("rerun-shortcut");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-rerun-target", "sandbox-cli", "failed");
    let bmux_bin = bmux_binary();
    let bmux_bin_arg = bmux_bin.to_string_lossy().to_string();

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "rerun",
            "bmux-sbx-rerun-target",
            "--bmux-bin",
            bmux_bin_arg.as_str(),
            "--json",
        ])
        .output()
        .expect("run sandbox rerun");
    assert!(
        output.status.success(),
        "sandbox rerun should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["status"].as_str(), Some("succeeded"));
}

#[test]
#[serial]
fn sandbox_tail_requires_target_or_selector() {
    let sandbox = CommandSandbox::new("tail-target-required");

    let output = sandbox
        .command()
        .args(["sandbox", "tail", "--json"])
        .output()
        .expect("run sandbox tail without target");

    assert!(
        !output.status.success(),
        "sandbox tail should fail when no target selector is provided"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tail target required"),
        "tail failure should provide target selector guidance: {stderr}"
    );
}

#[test]
#[serial]
fn sandbox_open_json_reports_missing_latest_log_as_null() {
    let sandbox = CommandSandbox::new("open-missing-latest-log");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-open-missing-log",
        "sandbox-cli",
        "failed",
    );

    let output = sandbox
        .command()
        .args(["sandbox", "open", "bmux-sbx-open-missing-log", "--json"])
        .output()
        .expect("run sandbox open without log files");
    assert!(
        output.status.success(),
        "sandbox open should succeed without logs: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert!(
        json["latest_log"].is_null(),
        "latest_log should be null when no log files are present"
    );
}

#[test]
#[serial]
fn sandbox_rerun_fails_when_manifest_command_is_empty() {
    let sandbox = CommandSandbox::new("rerun-empty-command");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-rerun-empty-command",
        "sandbox-cli",
        "failed",
    );

    let manifest_path = tmp_root
        .join("bmux-sbx-rerun-empty-command")
        .join("sandbox.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&manifest_path).expect("read manifest for empty command test"),
    )
    .expect("parse manifest json");
    manifest["command"] = serde_json::json!([]);
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("serialize mutated manifest"),
    )
    .expect("write mutated manifest");

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "rerun",
            "bmux-sbx-rerun-empty-command",
            "--bmux-bin",
            bmux_binary().to_string_lossy().as_ref(),
        ])
        .output()
        .expect("run sandbox rerun with empty command manifest");

    assert!(
        !output.status.success(),
        "sandbox rerun should fail for empty command manifest"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has no command to rerun"),
        "rerun failure should explain missing command: {stderr}"
    );
}

#[test]
#[serial]
fn sandbox_triage_defaults_to_latest_failed_and_reports_target() {
    let sandbox = CommandSandbox::new("triage-default-latest-failed");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-triage-target", "sandbox-cli", "failed");
    let log_path = tmp_root
        .join("bmux-sbx-triage-target")
        .join("logs")
        .join("run.log");
    std::fs::write(&log_path, "triage-line\n").expect("write triage log file");

    let output = sandbox
        .command()
        .args(["sandbox", "triage", "--tail", "10", "--json"])
        .output()
        .expect("run sandbox triage with defaults");
    assert!(
        output.status.success(),
        "sandbox triage should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = parse_json_stdout(&output);
    assert_schema_version(&json);
    assert_eq!(json["selection"]["defaulted_to_latest_failed"], true);
    assert_eq!(
        json["target"]["id"].as_str(),
        Some("bmux-sbx-triage-target")
    );
    assert_eq!(json["target"]["source"].as_str(), Some("sandbox-cli"));
    assert_eq!(json["target"]["status"].as_str(), Some("failed"));
    assert_eq!(json["rerun"]["requested"], false);
    assert_eq!(json["rerun"]["executed"], false);
}

#[test]
#[serial]
fn sandbox_triage_rerun_executes_manifest_command() {
    let sandbox = CommandSandbox::new("triage-rerun");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-triage-rerun", "sandbox-cli", "failed");

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "triage",
            "bmux-sbx-triage-rerun",
            "--rerun",
            "--bmux-bin",
            bmux_binary().to_string_lossy().as_ref(),
        ])
        .output()
        .expect("run sandbox triage with rerun");
    assert!(
        output.status.success(),
        "sandbox triage rerun should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("rerun_exit_code: 0"),
        "triage rerun output should include rerun exit code: {stdout}"
    );
}

#[test]
#[serial]
fn sandbox_inspect_rejects_ambiguous_prefix_target() {
    let sandbox = CommandSandbox::new("inspect-ambiguous-prefix");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-ambiguous-one", "sandbox-cli", "failed");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-ambiguous-two", "sandbox-cli", "failed");

    let output = sandbox
        .command()
        .args(["sandbox", "inspect", "bmux-sbx-ambiguous", "--json"])
        .output()
        .expect("run sandbox inspect with ambiguous prefix");
    assert!(
        !output.status.success(),
        "sandbox inspect should fail for ambiguous prefix"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is ambiguous"),
        "error should mention ambiguity: {stderr}"
    );
    assert!(
        stderr.contains("bmux-sbx-ambiguous-one") && stderr.contains("bmux-sbx-ambiguous-two"),
        "error should include matching ids: {stderr}"
    );
}

#[test]
#[serial]
fn sandbox_inspect_not_found_suggests_similar_target() {
    let sandbox = CommandSandbox::new("inspect-similar-suggest");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-suggest-target",
        "sandbox-cli",
        "failed",
    );

    let output = sandbox
        .command()
        .args(["sandbox", "inspect", "suggest-target", "--json"])
        .output()
        .expect("run sandbox inspect with near-miss target");
    assert!(
        !output.status.success(),
        "sandbox inspect should fail for unknown target"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("did you mean") && stderr.contains("bmux-sbx-suggest-target"),
        "error should suggest a similar sandbox id: {stderr}"
    );
}

#[test]
#[serial]
fn sandbox_latest_source_error_lists_available_sources() {
    let sandbox = CommandSandbox::new("latest-source-hint");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(&tmp_root, "bmux-sbx-playbook-only", "playbook", "failed");

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "inspect",
            "--latest",
            "--source",
            "recording-verify",
            "--json",
        ])
        .output()
        .expect("run sandbox inspect latest by missing source");
    assert!(
        !output.status.success(),
        "sandbox inspect should fail for missing source"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("available sources: playbook"),
        "error should list available sources: {stderr}"
    );
}

#[test]
#[serial]
fn sandbox_latest_failed_source_error_suggests_latest() {
    let sandbox = CommandSandbox::new("latest-failed-source-hint");
    let tmp_root = sandbox.root.path().join("tmp-root");
    create_manifest_sandbox(
        &tmp_root,
        "bmux-sbx-playbook-stopped",
        "playbook",
        "stopped",
    );

    let output = sandbox
        .command()
        .args([
            "sandbox",
            "inspect",
            "--latest-failed",
            "--source",
            "playbook",
            "--json",
        ])
        .output()
        .expect("run sandbox inspect latest-failed by source without failures");
    assert!(
        !output.status.success(),
        "sandbox inspect should fail when source has no failed sandboxes"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("try --latest --source playbook"),
        "error should suggest source-scoped latest helper: {stderr}"
    );
}
