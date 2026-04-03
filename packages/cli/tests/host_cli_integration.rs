use std::path::{Path, PathBuf};
use std::process::Command;

fn bmux_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bmux"))
}

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bmux-host-cli-tests-{label}-{}",
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

#[test]
fn host_status_prints_runtime_state_from_file() {
    let root = TempDirGuard::new("status-output");
    let runtime_dir = root.path().join("runtime");
    let config_dir = root.path().join("config");
    let data_dir = root.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let host_state = serde_json::json!({
        "pid": 9001,
        "target": "iroh://endpoint-123",
        "share_link": "bmux://demo-host",
        "name": "demo-host",
        "started_at_unix": 1700000123
    });
    std::fs::write(
        runtime_dir.join("host-state.json"),
        serde_json::to_string_pretty(&host_state).expect("serialize host state"),
    )
    .expect("write host-state file");

    let output = Command::new(bmux_binary())
        .args(["host", "--status"])
        .env("BMUX_RUNTIME_DIR", &runtime_dir)
        .env("BMUX_CONFIG_DIR", &config_dir)
        .env("BMUX_DATA_DIR", &data_dir)
        .output()
        .expect("run bmux host --status");

    assert!(
        output.status.success(),
        "expected success, status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("host runtime: running"));
    assert!(stdout.contains("name: demo-host"));
    assert!(stdout.contains("pid: 9001"));
    assert!(stdout.contains("target: iroh://endpoint-123"));
    assert!(stdout.contains("share link: bmux://demo-host"));
    assert!(stdout.contains("started_at_unix: 1700000123"));
}

#[test]
fn host_status_without_state_returns_not_running_message() {
    let root = TempDirGuard::new("status-empty");
    let runtime_dir = root.path().join("runtime");
    let config_dir = root.path().join("config");
    let data_dir = root.path().join("data");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let output = Command::new(bmux_binary())
        .args(["host", "--status"])
        .env("BMUX_RUNTIME_DIR", &runtime_dir)
        .env("BMUX_CONFIG_DIR", &config_dir)
        .env("BMUX_DATA_DIR", &data_dir)
        .output()
        .expect("run bmux host --status");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("host runtime is not running"));
}
