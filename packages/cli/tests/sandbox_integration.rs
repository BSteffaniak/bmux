use serial_test::serial;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        let mut command = Command::new(bmux_binary());
        command
            .current_dir(workspace_root())
            .env("BMUX_CONFIG_DIR", self.root.path().join("config"))
            .env("BMUX_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("BMUX_DATA_DIR", self.root.path().join("data"))
            .env("BMUX_STATE_DIR", self.root.path().join("state"))
            .env("BMUX_LOG_DIR", self.root.path().join("logs"))
            .env("BMUX_TARGET", "")
            .env("TMPDIR", self.root.path().join("tmp-root"));
        command
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

    let repro =
        std::fs::read_to_string(bundle_dir.join("repro.txt")).expect("read bundled repro command");
    assert!(
        repro.contains("bmux sandbox run"),
        "repro command should include sandbox run"
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
    assert_eq!(entries.len(), 1, "only recording-verify should match");
    assert_eq!(entries[0]["source"].as_str(), Some("recording-verify"));
    let path = entries[0]["path"]
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
        1,
        "config defaults should narrow to one entry"
    );
    assert_eq!(entries[0]["source"].as_str(), Some("recording-verify"));
    assert_eq!(entries[0]["status"].as_str(), Some("failed"));
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
        1,
        "failed-only should include aborted-running"
    );
    assert_eq!(entries[0]["status"].as_str(), Some("failed"));
    let path = entries[0]["path"]
        .as_str()
        .expect("cleanup entry should include path");
    assert!(path.contains("bmux-sbx-aborted-running"));
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
