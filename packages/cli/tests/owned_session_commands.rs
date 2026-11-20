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
        // Interactive attach logs to file only, so its routing line is in the
        // isolated log directory rather than on stderr.
        let routed = format!("plugin_id=\"bmux.sessions\" command_name=\"{name}\"");
        assert!(
            err.contains(&routed) || logs_contain(&root.join("log"), &routed),
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

fn logs_contain(dir: &std::path::Path, needle: &str) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let path = entry.path();
            if path.is_dir() {
                logs_contain(&path, needle)
            } else {
                std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle))
            }
        })
}
