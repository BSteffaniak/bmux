use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

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

fn gateway_table_header_line(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        if line.contains("candidate")
            && line.contains("preferred")
            && line.contains("stability")
            && line.contains("latency_ms")
            && line.contains("detail")
        {
            Some(line.split_whitespace().collect::<Vec<_>>().join(" "))
        } else {
            None
        }
    })
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

#[test]
fn cluster_status_dry_run_json_is_observational_with_would_mutate_false() {
    let env = CliTestEnv::new("status-dry-run-json");
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
        "--dry-run",
        "--format",
        "json",
    ]);
    let payload: Value = serde_json::from_slice(&output.stdout).expect("dry-run json output");
    assert_eq!(payload["cluster"], "prod");
    assert_eq!(payload["command"], "cluster-status");
    assert_eq!(payload["would_mutate"]["enabled"], false);
    assert_eq!(payload["would_mutate"]["breaker"], false);
    assert_eq!(payload["would_mutate"]["persistence_write"], false);
}

#[test]
fn cluster_status_dry_run_honors_breaker_open_skip_reason() {
    let env = CliTestEnv::new("status-dry-run-breaker-open");
    env.write_cluster_config();

    let state = serde_json::json!({
        "version": 2,
        "clusters": {
            "prod": {
                "last_good": null,
                "cooldown_until_unix_ms": {},
                "candidate_health": {
                    "missing-prod-a": {
                        "successes": 1,
                        "failures": 5,
                        "consecutive_failures": 3,
                        "last_latency_ms": 250,
                        "breaker_state": "open",
                        "breaker_open_until_unix_ms": 4102444800000u64
                    }
                }
            }
        }
    });
    std::fs::write(
        env.gateway_state_path(),
        serde_json::to_string_pretty(&state).expect("serialize gateway state"),
    )
    .expect("write gateway state");
    let before = std::fs::read_to_string(env.gateway_state_path()).expect("read state before run");

    let output = env.run(&[
        "cluster",
        "status",
        "prod",
        "--gateway",
        "missing-prod-a",
        "--gateway-mode",
        "pinned",
        "--dry-run",
        "--format",
        "json",
    ]);
    let payload: Value = serde_json::from_slice(&output.stdout).expect("dry-run json output");
    assert_eq!(payload["result"], "failure");
    let probes = payload["probes"].as_array().expect("probes array");
    let first = probes.first().expect("at least one probe");
    assert_eq!(first["skip_reason"], "breaker_open");

    let after = std::fs::read_to_string(env.gateway_state_path()).expect("read state after run");
    assert_eq!(before, after, "dry-run should not mutate runtime state");
}

#[test]
fn gateway_text_tables_are_consistent_across_status_explain_and_dry_run() {
    let env = CliTestEnv::new("gateway-table-shape-consistency");
    env.write_cluster_config();

    let status_output = env.run(&["cluster", "gateway", "status", "--cluster", "prod"]);
    assert_eq!(status_output.status.code(), Some(0));
    let status_text = String::from_utf8_lossy(&status_output.stdout);
    let status_header = gateway_table_header_line(&status_text)
        .expect("status output should include gateway table header");

    let explain_output = env.run(&["cluster", "gateway", "explain", "--cluster", "prod"]);
    assert_eq!(explain_output.status.code(), Some(1));
    let explain_text = String::from_utf8_lossy(&explain_output.stdout);
    let explain_header = gateway_table_header_line(&explain_text)
        .expect("explain output should include gateway table header");

    let dry_run_output = env.run(&[
        "cluster",
        "status",
        "prod",
        "--gateway",
        "missing-prod-a",
        "--gateway-mode",
        "pinned",
        "--gateway-no-failover",
        "--dry-run",
    ]);
    assert_eq!(dry_run_output.status.code(), Some(1));
    let dry_run_text = String::from_utf8_lossy(&dry_run_output.stdout);
    let dry_run_header = gateway_table_header_line(&dry_run_text)
        .expect("dry-run output should include gateway table header");

    let expected =
        "candidate preferred stability breaker cooldown_ms ok reason latency_ms skip detail";
    assert_eq!(status_header, expected);
    assert_eq!(explain_header, expected);
    assert_eq!(dry_run_header, expected);
}

#[test]
fn gateway_status_json_supports_why_and_policy_preset() {
    let env = CliTestEnv::new("gateway-status-why-policy-json");
    env.write_cluster_config();

    let output = env.run(&[
        "cluster",
        "gateway",
        "status",
        "--cluster",
        "prod",
        "--format",
        "json",
        "--gateway-policy",
        "aggressive",
        "--why",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let payload: Value = serde_json::from_slice(&output.stdout).expect("status json payload");
    assert_eq!(payload["policy"]["preset"], "aggressive");
    assert_eq!(payload["policy"]["breaker_open_after_failures"], 2);
    assert!(payload["decision_summary"].is_object());
}

#[test]
fn gateway_doctor_reports_critical_when_all_candidates_fail() {
    let env = CliTestEnv::new("gateway-doctor-critical");
    env.write_cluster_config();

    let output = env.run(&[
        "cluster",
        "gateway",
        "doctor",
        "--cluster",
        "prod",
        "--format",
        "json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let payload: Value = serde_json::from_slice(&output.stdout).expect("doctor json payload");
    assert_eq!(payload["cluster"], "prod");
    assert_eq!(payload["result"], "critical");
    assert!(payload["findings"].is_array());
}

#[test]
fn gateway_history_json_supports_since_and_limit_filters() {
    let env = CliTestEnv::new("gateway-history-json");
    env.write_cluster_config();

    let now_ms: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_millis()
        .try_into()
        .expect("unix millis should fit into u64");
    let state = serde_json::json!({
        "version": 2,
        "clusters": {
            "prod": {
                "last_good": null,
                "cooldown_until_unix_ms": {},
                "candidate_health": {},
                "history": [
                    {
                        "observed_at_unix_ms": now_ms.saturating_sub(7_200_000),
                        "command": "cluster-status",
                        "candidate": "missing-prod-a",
                        "result": "observed_failure",
                        "reason": "timeout"
                    },
                    {
                        "observed_at_unix_ms": now_ms.saturating_sub(60_000),
                        "command": "cluster-status",
                        "candidate": "missing-prod-b",
                        "result": "observed_success",
                        "reason": null
                    }
                ]
            }
        }
    });
    std::fs::write(
        env.gateway_state_path(),
        serde_json::to_string_pretty(&state).expect("serialize gateway history state"),
    )
    .expect("write gateway state");

    let output = env.run(&[
        "cluster",
        "gateway",
        "history",
        "--cluster",
        "prod",
        "--format",
        "json",
        "--since",
        "5m",
        "--limit",
        "5",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let payload: Value = serde_json::from_slice(&output.stdout).expect("history json payload");
    assert_eq!(payload["cluster"], "prod");
    assert_eq!(payload["count"], 1);
    let entries = payload["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["candidate"], "missing-prod-b");
}
