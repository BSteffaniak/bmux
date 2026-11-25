//! End-to-end checks of `bmux drive`: a program started in its own server,
//! driven with input, observed after it settles, checked with `expect`,
//! linted when it exits, and its recorded script replayed at two sizes.
#![cfg(all(unix, feature = "bundled-plugin-playbook"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The program under test: a prompt that echoes one line, then leaves the
/// alternate screen on when it exits (a bug lint must report).
const PROGRAM: &str =
    r#"printf '\033[?1049hready> '; read line; printf 'got %s\n' "$line"; read rest"#;

struct Home {
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bmux-drive-it-{}", uuid::Uuid::new_v4()));
        for dir in ["config", "runtime", "data", "state", "logs", "drive"] {
            std::fs::create_dir_all(root.join(dir)).expect("create test directory");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(root.join(dir), std::fs::Permissions::from_mode(0o700))
                    .expect("make test directory private");
            }
        }
        Self { root }
    }

    fn drive(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bmux"))
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

    fn json(&self, args: &[&str]) -> serde_json::Value {
        // Before the subcommand: after `--`, a flag belongs to the program.
        let mut args = args.to_vec();
        args.insert(0, "--json");
        let output = self.drive(&args);
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "bmux drive {args:?} printed no JSON ({error}): stdout {} stderr {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // Stop anything a failed test left running, then remove its files.
        let _ = self.drive(&["stop", "--no-screen"]);
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn screen_text(report: &serde_json::Value) -> String {
    report["screen"]["lines"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[test]
fn drive_acts_observes_lints_and_replays() {
    let home = Home::new();

    let start = home.json(&[
        "start", "--name", "prompt", "--size", "60x10", "--", "sh", "-c", PROGRAM,
    ]);
    assert_eq!(start["ok"], true, "{start:#}");
    assert!(screen_text(&start).contains("ready>"), "{start:#}");
    assert_eq!(start["screen"]["alternate_screen"], true);

    let typed = home.json(&["type", "hello world", "--enter"]);
    assert!(screen_text(&typed).contains("got hello world"), "{typed:#}");
    assert!(
        typed["screen"]["changed_rows"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "{typed:#}"
    );

    assert!(
        home.drive(&["expect", "--text", "got hello world"])
            .status
            .success()
    );
    let failed = home.drive(&["expect", "--text", "not on screen"]);
    assert_eq!(failed.status.code(), Some(1));

    let ended = home.json(&["type", "", "--enter"]);
    assert_eq!(ended["settled"]["state"], "exited", "{ended:#}");
    assert_eq!(ended["settled"]["exit_code"], 0, "{ended:#}");
    let new: Vec<&str> = ended["new_findings"]
        .as_array()
        .map(|findings| findings.iter().filter_map(|f| f["id"].as_str()).collect())
        .unwrap_or_default();
    assert!(new.contains(&"left-on:alternate screen"), "{ended:#}");

    let stopped = home.json(&["stop"]);
    let script = stopped["script"]
        .as_str()
        .expect("stop reports the script")
        .to_owned();
    assert!(script.contains("type 'hello world' --enter"), "{script}");
    assert!(
        script.contains("expect --text 'got hello world'"),
        "{script}"
    );
    assert!(
        !script.contains("not on screen"),
        "failed checks are not recorded: {script}"
    );
    assert!(
        home.drive(&["list"])
            .stdout
            .starts_with(b"no drive sessions"),
        "stop removes the session"
    );

    let file = home.path().join("prompt.drive");
    std::fs::write(&file, &script).expect("save script");
    let replay = home.drive(&[
        "run",
        file.to_str().expect("utf-8 path"),
        "--size",
        "60x10,30x6",
    ]);
    let report = String::from_utf8_lossy(&replay.stdout);
    assert!(replay.status.success(), "{report}");
    assert!(
        report.contains("PASS at 60x10") && report.contains("PASS at 30x6"),
        "{report}"
    );
}
