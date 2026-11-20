//! Stdin belongs to the command caller, even under a plugin-owned nested path.
#![cfg(feature = "bundled-plugin-playbook")]

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn offline_validate_reads_caller_stdin_without_a_server() {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_bmux"));
    for (key, directory) in [
        ("BMUX_CONFIG_DIR", "config"),
        ("BMUX_RUNTIME_DIR", "runtime"),
        ("BMUX_DATA_DIR", "data"),
        ("BMUX_STATE_DIR", "state"),
        ("BMUX_LOG_DIR", "logs"),
    ] {
        let path = root.path().join(directory);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.recursive(true).create(&path).unwrap();
        command.env(key, path);
    }
    let mut child = command
        .args(["playbook", "validate", "--json", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"new-session name='offline'\n")
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        serde_json::json!({"valid":true, "errors":[]})
    );
}
