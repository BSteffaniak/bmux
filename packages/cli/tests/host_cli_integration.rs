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

struct CliTestEnv {
    _root: TempDirGuard,
    runtime_dir: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    state_dir: PathBuf,
    log_dir: PathBuf,
}

impl CliTestEnv {
    fn new(label: &str) -> Self {
        let root = TempDirGuard::new(label);
        let runtime_dir = root.path().join("runtime");
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        let log_dir = root.path().join("logs");
        std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        std::fs::create_dir_all(&log_dir).expect("create log dir");
        Self {
            _root: root,
            runtime_dir,
            config_dir,
            data_dir,
            state_dir,
            log_dir,
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(bmux_binary())
            .args(args)
            .env("BMUX_RUNTIME_DIR", &self.runtime_dir)
            .env("BMUX_CONFIG_DIR", &self.config_dir)
            .env("BMUX_DATA_DIR", &self.data_dir)
            .env("BMUX_STATE_DIR", &self.state_dir)
            .env("BMUX_LOG_DIR", &self.log_dir)
            .output()
            .expect("run bmux command")
    }
}

fn stdout_lines(output: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn host_status_prints_runtime_state_from_file() {
    let root = TempDirGuard::new("status-output");
    let runtime_dir = root.path().join("runtime");
    let config_dir = root.path().join("config");
    let data_dir = root.path().join("data");
    let state_dir = root.path().join("state");
    let log_dir = root.path().join("logs");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    std::fs::create_dir_all(&log_dir).expect("create log dir");

    let current_pid = std::process::id();
    let host_state = serde_json::json!({
        "pid": current_pid,
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
        .env("BMUX_STATE_DIR", &state_dir)
        .env("BMUX_LOG_DIR", &log_dir)
        .output()
        .expect("run bmux host --status");

    assert!(
        output.status.success(),
        "expected success, status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Status: ready"));
    assert!(stdout.contains("host runtime: running"));
    assert!(stdout.contains("name: demo-host"));
    assert!(stdout.contains(&format!("pid: {current_pid}")));
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
    let state_dir = root.path().join("state");
    let log_dir = root.path().join("logs");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime dir");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    std::fs::create_dir_all(&log_dir).expect("create log dir");

    let output = Command::new(bmux_binary())
        .args(["host", "--status"])
        .env("BMUX_RUNTIME_DIR", &runtime_dir)
        .env("BMUX_CONFIG_DIR", &config_dir)
        .env("BMUX_DATA_DIR", &data_dir)
        .env("BMUX_STATE_DIR", &state_dir)
        .env("BMUX_LOG_DIR", &log_dir)
        .output()
        .expect("run bmux host --status");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Status: not ready"));
    assert!(stdout.contains("Reason: host runtime is not running"));
    assert!(stdout.contains("Fix: bmux setup"));
    assert!(stdout.contains("Advanced: bmux host --daemon"));
}

#[test]
fn hosts_output_is_concise_by_default() {
    let env = CliTestEnv::new("hosts-default");
    std::fs::write(
        env.config_dir.join("bmux.toml"),
        r#"[connections]
recent_targets = ["dev"]

[connections.share_links]
demo = "iroh://endpoint-123"

[connections.targets.dev]
transport = "iroh"
host = "endpoint-123"
endpoint_id = "endpoint-123"
relay_url = "https://relay.example.com"
"#,
    )
    .expect("write bmux config");

    let output = env.run(&["hosts"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Status: not ready"));
    assert!(stdout.contains("Fix: bmux setup"));
    assert!(stdout.contains("share links:"));
    assert!(stdout.contains("configured targets:"));
    assert!(stdout.contains("recent:"));
    assert!(!stdout.contains("runtime:"));
    assert!(!stdout.contains("(detailed)"));
    assert!(!stdout.contains("transport:"));
    assert!(!stdout.contains("target:"));
}

#[test]
fn hosts_verbose_output_includes_runtime_and_endpoint_diagnostics() {
    let env = CliTestEnv::new("hosts-verbose");
    std::fs::write(
        env.config_dir.join("bmux.toml"),
        r#"[connections]
recent_targets = ["dev"]

[connections.share_links]
demo = "iroh://endpoint-123"

[connections.targets.dev]
transport = "iroh"
host = "endpoint-123"
endpoint_id = "endpoint-123"
relay_url = "https://relay.example.com"
"#,
    )
    .expect("write bmux config");

    let auth_state = serde_json::json!({
        "access_token": "token",
        "account_id": "acct-1",
        "account_name": "demo",
        "expires_at_unix": 2000000000
    });
    std::fs::write(
        env.runtime_dir.join("auth-state.json"),
        serde_json::to_string_pretty(&auth_state).expect("serialize auth state"),
    )
    .expect("write auth state");

    let host_state = serde_json::json!({
        "pid": std::process::id(),
        "target": "iroh://endpoint-123",
        "share_link": "bmux://demo",
        "name": "demo",
        "started_at_unix": 1700000123
    });
    std::fs::write(
        env.runtime_dir.join("host-state.json"),
        serde_json::to_string_pretty(&host_state).expect("serialize host state"),
    )
    .expect("write host state");

    let output = env.run(&["hosts", "--verbose"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Status: ready"));
    assert!(stdout.contains("Next: bmux join bmux://demo"));
    assert!(stdout.contains("runtime:"));
    assert!(stdout.contains("- auth: ready"));
    assert!(stdout.contains("- host: running"));
    assert!(stdout.contains("- local ipc endpoint:"));
    assert!(stdout.contains("- target: iroh://endpoint-123"));
    assert!(stdout.contains("- share link: bmux://demo"));
    assert!(stdout.contains("share links (detailed):"));
    assert!(stdout.contains("configured targets (detailed):"));
    assert!(stdout.contains("  transport: iroh"));
    assert!(stdout.contains("  endpoint id: endpoint-123"));
    assert!(stdout.contains("  relay: https://relay.example.com"));
    assert!(stdout.contains("  join: bmux join dev"));
}

#[test]
fn setup_check_not_ready_output_contract_is_stable() {
    let env = CliTestEnv::new("setup-check-not-ready");
    std::fs::write(
        env.config_dir.join("bmux.toml"),
        r#"[connections]
hosted_mode = "control_plane"
"#,
    )
    .expect("write bmux config");

    let output = env.run(&["setup", "--check"]);
    assert_eq!(output.status.code(), Some(1));
    let lines = stdout_lines(&output);
    assert_eq!(lines.first().map(String::as_str), Some("Status: not ready"));
    assert!(lines.iter().any(|line| line.starts_with("Reason: ")));
    assert!(lines.iter().any(|line| line == "Fix: bmux setup"));
    assert!(lines.iter().any(|line| line.starts_with("Advanced: ")));
}

#[test]
fn doctor_hosted_not_ready_output_contract_is_stable() {
    let env = CliTestEnv::new("doctor-hosted-not-ready");

    let output = env.run(&["doctor", "--hosted"]);
    assert_eq!(output.status.code(), Some(1));
    let lines = stdout_lines(&output);
    assert_eq!(lines.first().map(String::as_str), Some("Status: not ready"));
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("Reason: failed checks:"))
    );
    assert!(lines.iter().any(|line| line == "Fix: bmux setup"));
    assert!(lines.iter().any(|line| line.starts_with("auth: fail (")));
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("control-plane: fail ("))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("host-runtime: fail ("))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("share-lookup: fail ("))
    );
}
