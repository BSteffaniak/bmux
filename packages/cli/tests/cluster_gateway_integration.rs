use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bmux_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bmux"))
}

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bmux-cluster-gateway-tests-{label}-{}",
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

    fn write_cluster_config(&self) {
        std::fs::write(
            self.config_dir.join("bmux.toml"),
            r#"[plugins.settings."bmux.cluster".clusters.prod]
targets = ["missing-prod-a", "missing-prod-b"]
gateway_mode = "auto"

[plugins.settings."bmux.cluster".clusters.staging]
targets = ["missing-staging-a"]
gateway_mode = "auto"
"#,
        )
        .expect("write cluster config");
    }

    fn run(&self, args: &[&str]) -> Output {
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

    fn gateway_state_path(&self) -> PathBuf {
        self.runtime_dir.join("cluster-gateway-state.json")
    }
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn gateway_reset_requires_cluster_unless_all_is_passed() {
    let env = CliTestEnv::new("reset-requires-cluster");
    env.write_cluster_config();

    let output = env.run(&["cluster", "gateway", "reset"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        combined_output(&output).contains("requires --cluster unless --all is passed"),
        "unexpected output: {}",
        combined_output(&output)
    );
}

#[test]
fn gateway_reset_rejects_mixing_all_and_cluster() {
    let env = CliTestEnv::new("reset-mixed-scope");
    env.write_cluster_config();

    let output = env.run(&["cluster", "gateway", "reset", "--all", "--cluster", "prod"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        combined_output(&output).contains("either --all or --cluster"),
        "unexpected output: {}",
        combined_output(&output)
    );
}

#[test]
fn gateway_reset_cluster_only_removes_requested_cluster_state() {
    let env = CliTestEnv::new("reset-single-cluster");
    env.write_cluster_config();

    let state = serde_json::json!({
        "clusters": {
            "prod": {
                "last_good": {
                    "target": "missing-prod-b",
                    "observed_at_unix_ms": 1700000000000u64
                },
                "cooldown_until_unix_ms": {
                    "missing-prod-a": 1800000000000u64
                }
            },
            "staging": {
                "last_good": {
                    "target": "missing-staging-a",
                    "observed_at_unix_ms": 1700000000000u64
                },
                "cooldown_until_unix_ms": {}
            }
        }
    });
    std::fs::write(
        env.gateway_state_path(),
        serde_json::to_string_pretty(&state).expect("serialize gateway state"),
    )
    .expect("write gateway state");

    let output = env.run(&["cluster", "gateway", "reset", "--cluster", "prod"]);
    assert_eq!(output.status.code(), Some(0));

    let updated = std::fs::read_to_string(env.gateway_state_path()).expect("read updated state");
    let decoded: Value = serde_json::from_str(&updated).expect("decode updated gateway state");
    let clusters = decoded["clusters"]
        .as_object()
        .expect("clusters should be object");
    assert!(!clusters.contains_key("prod"));
    assert!(clusters.contains_key("staging"));
}

#[test]
fn gateway_reset_all_removes_persisted_runtime_state_file() {
    let env = CliTestEnv::new("reset-all");
    env.write_cluster_config();

    let state = serde_json::json!({
        "clusters": {
            "prod": {
                "last_good": {
                    "target": "missing-prod-b",
                    "observed_at_unix_ms": 1700000000000u64
                },
                "cooldown_until_unix_ms": {
                    "missing-prod-a": 1800000000000u64
                }
            }
        }
    });
    std::fs::write(
        env.gateway_state_path(),
        serde_json::to_string_pretty(&state).expect("serialize gateway state"),
    )
    .expect("write gateway state");

    let output = env.run(&["cluster", "gateway", "reset", "--all"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        !env.gateway_state_path().exists(),
        "gateway state file should be removed"
    );
}

#[test]
fn gateway_status_and_explain_support_json_format_and_gateway_flags() {
    let env = CliTestEnv::new("status-explain-json");
    env.write_cluster_config();

    let status_output = env.run(&[
        "cluster",
        "gateway",
        "status",
        "--cluster",
        "prod",
        "--format",
        "json",
        "--gateway",
        "missing-prod-a",
    ]);
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "unexpected status output: {}",
        combined_output(&status_output)
    );
    let status_json: Value = serde_json::from_slice(&status_output.stdout).expect("status json");
    assert_eq!(status_json["cluster"], "prod");
    assert!(status_json["candidates"].is_array());
    assert_eq!(status_json["overrides"]["gateway"], "missing-prod-a");

    let explain_output = env.run(&[
        "cluster",
        "gateway",
        "explain",
        "--cluster",
        "prod",
        "--format",
        "json",
        "--gateway",
        "missing-prod-a",
        "--gateway-no-failover",
    ]);
    assert_eq!(explain_output.status.code(), Some(1));
    let explain_json: Value = serde_json::from_slice(&explain_output.stdout).expect("explain json");
    assert_eq!(explain_json["cluster"], "prod");
    assert_eq!(explain_json["result"], "failure");
    assert!(explain_json["probes"].is_array());
    assert!(explain_json["failures"].is_array());
}

#[test]
fn cluster_status_gateway_flags_route_through_gateway_path() {
    let env = CliTestEnv::new("status-routed-through-gateway");
    env.write_cluster_config();

    let output = env.run(&[
        "cluster",
        "status",
        "prod",
        "--gateway",
        "missing-prod-a",
        "--gateway-mode",
        "pinned",
        "--gateway-no-failover",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        combined_output(&output).contains("all gateway candidates failed for cluster 'prod'"),
        "unexpected output: {}",
        combined_output(&output)
    );
}
