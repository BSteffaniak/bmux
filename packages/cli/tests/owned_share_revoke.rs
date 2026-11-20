//! A standalone command must not acquire server identity or ingress authority.
#![cfg(feature = "bundled-plugin-connections")]

use std::process::Command;

#[test]
fn revoke_without_name_on_a_fresh_install_reports_usage_not_lifecycle_failure() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let config = root.join("config.toml");
    std::fs::write(&config, "[plugins]\ndisabled = []\n").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_bmux"));
    command
        .current_dir(&root)
        .env_clear()
        .env("HOME", &root)
        .env("BMUX_CONFIG", config)
        .env("BMUX_LOG_LEVEL", "error")
        .env("TOKIO_WORKER_THREADS", "1")
        .env("NO_COLOR", "1");
    for kind in ["config", "runtime", "data", "state", "log"] {
        command.env(
            format!("BMUX_{}_DIR", kind.to_ascii_uppercase()),
            root.join(kind),
        );
    }
    let output = command.args(["share", "revoke"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "bmux error: Reason: missing share name\nFix: bmux unshare <name>\nAdvanced: bmux hosts\n"
    );
    assert!(
        !root.join("runtime").exists(),
        "validation created a server runtime"
    );
}
