//! Exercise the public spellings against an isolated, deliberately stopped runtime.
#![cfg(feature = "bundled-plugin-sessions")]

#[test]
fn offline_commands_route_to_sessions_without_starting_a_server() {
    use std::process::{Command, Stdio};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let config = root.join("config.toml");
    std::fs::write(&config, "[plugins]\ndisabled = []\n").unwrap();
    for (args, name, status, message) in [
        (
            &["kill-all-sessions"][..],
            "kill-all-sessions",
            0,
            "no sessions",
        ),
        (
            &["session", "kill", "missing"][..],
            "kill-session",
            1,
            "session not found: missing",
        ),
        (
            &["session", "detach"][..],
            "detach",
            1,
            "server is not running",
        ),
        (
            &["session", "attach", "missing"][..],
            "session-attach",
            1,
            "server is not running",
        ),
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bmux"));
        cmd.env_clear()
            .current_dir(&root)
            .env("HOME", &root)
            .env("BMUX_CONFIG", &config)
            .env("BMUX_LOG_LEVEL", "info")
            .env("TOKIO_WORKER_THREADS", "2")
            .stdin(Stdio::null());
        for kind in ["config", "data", "state", "runtime", "log"] {
            cmd.env(
                format!("BMUX_{}_DIR", kind.to_ascii_uppercase()),
                root.join(kind),
            );
        }
        let output = cmd.args(args).output().unwrap();
        let err = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(status), "{args:?}: {err}");
        assert!(
            err.contains(&format!(
                "plugin_id=\"bmux.sessions\" command_name=\"{name}\""
            )),
            "{args:?}: {err}"
        );
        let text = if status == 0 {
            String::from_utf8(output.stdout).unwrap()
        } else {
            err
        };
        assert!(text.contains(message), "{args:?}: {text}");
    }
}
