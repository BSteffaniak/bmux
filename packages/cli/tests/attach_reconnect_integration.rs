//! An attach client survives its server being killed and restarted: it shows
//! the reconnecting banner, keeps the terminal in attach mode, and reconnects
//! to the restored session in place.
//!
//! The client runs under `bmux drive` (a real terminal); its server is an
//! isolated fixture server.
#![cfg(all(unix, feature = "bundled-plugin-playbook"))]

#[allow(dead_code)]
mod support;

use std::path::PathBuf;
use std::process::{Command, Output};

/// `bmux drive`'s own private directories.
struct Drive {
    root: PathBuf,
}

impl Drive {
    fn new() -> Self {
        // Short: the drive server's socket path must fit a Unix socket address.
        let id = uuid::Uuid::new_v4().simple().to_string();
        let root = std::env::temp_dir().join(format!("brc-{}", &id[..8]));
        for dir in ["config", "runtime", "data", "state", "logs", "drive"] {
            let path = root.join(dir);
            std::fs::create_dir_all(&path).expect("create drive directory");
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("make drive directory private");
        }
        Self { root }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(support::bmux_binary())
            .arg("drive")
            .args(args)
            .current_dir(&self.root)
            .env("BMUX_CONFIG_DIR", self.root.join("config"))
            .env("BMUX_RUNTIME_DIR", self.root.join("runtime"))
            .env("BMUX_DATA_DIR", self.root.join("data"))
            .env("BMUX_STATE_DIR", self.root.join("state"))
            .env("BMUX_LOG_DIR", self.root.join("logs"))
            .env("BMUX_DRIVE_HOME", self.root.join("drive"))
            .env("BMUX_LOG_LEVEL", "error")
            .output()
            .expect("run bmux drive")
    }

    fn ok(&self, args: &[&str], what: &str) -> String {
        let output = self.run(args);
        let report = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{what} failed: {report}");
        report
    }
}

impl Drop for Drive {
    fn drop(&mut self) {
        let _ = self.run(&["stop", "--no-screen"]);
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

#[test]
fn attach_survives_a_server_kill_and_restart() {
    let mut server = support::ServerEnv::new("attach-reconnect");
    server.isolate_shell_config();
    server.start();
    support::assert_success(&server.run(&["session", "new", "survivor"]), "create session");
    // A tab in the default view, so input has somewhere to go.
    support::assert_success(
        &server.run(&["new-tab", "--session", "survivor", "--name", "lifeboat"]),
        "create tab",
    );

    // The attach client sees exactly the fixture server's runtime.
    let env: Vec<String> = [
        ("BMUX_NO_BASE_CONFIG", "1".to_owned()),
        ("BMUX_RUNTIME_NAME", "default".to_owned()),
        ("BMUX_TARGET", "local".to_owned()),
        ("BMUX_RUNTIME_DIR", server.runtime_dir.display().to_string()),
        ("BMUX_CONFIG_DIR", server.config_dir.display().to_string()),
        ("BMUX_DATA_DIR", server.data_dir.display().to_string()),
        ("BMUX_STATE_DIR", server.state_dir.display().to_string()),
        ("BMUX_LOG_DIR", server.log_dir.display().to_string()),
    ]
    .into_iter()
    .map(|(key, value)| format!("{key}={value}"))
    .collect();
    let binary = support::bmux_binary();
    let mut start = vec!["start", "--name", "attach", "--size", "100x30", "--timeout", "60", "--no-screen"];
    for entry in &env {
        start.extend(["--env", entry.as_str()]);
    }
    start.extend(["--", binary.to_str().expect("utf-8 binary path"), "attach", "survivor"]);
    let drive = Drive::new();
    drive.ok(&start, "start the attach client");
    drive.ok(&["wait", "--text", "lifeboat", "--timeout", "60"], "attach");

    server.kill();
    let lost = drive.ok(
        &["wait", "--text", "connection lost — reconnecting", "--timeout", "30"],
        "the reconnecting banner",
    );
    assert!(!lost.contains("EXITED"), "the client keeps running: {lost}");

    server.start();
    let back = drive.ok(
        &["wait", "--gone", "connection lost", "--timeout", "60"],
        "reconnecting to the restarted server",
    );
    assert!(!back.contains("EXITED"), "the client is still attached: {back}");

    // Input reaches the restored session.
    drive.ok(&["type", "echo $((6*7))reconnected", "--enter", "--no-screen"], "type");
    drive.ok(
        &["wait", "--text", "42reconnected", "--timeout", "30", "--no-screen"],
        "output from the restored session",
    );

    drop(drive);
    server.finish();
}
