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
}

fn parse_json_stdout(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let start = stdout
        .find('{')
        .unwrap_or_else(|| panic!("stdout did not include json object: {stdout}"));
    serde_json::from_str(&stdout[start..])
        .unwrap_or_else(|error| panic!("stdout was not json: {error}; stdout={stdout}"))
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
