//! Native display admission must precede any companion connection or launch.
#![cfg(target_os = "linux")]
use std::process::Command;

fn command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bterm"));
    command
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("PATH", root.join("tools"))
        .arg("--no-bmux-config")
        .arg("--runtime-root")
        .arg(root.join("external"));
    command
}

#[test]
fn missing_desktop_is_reported_before_runtime_connection() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let tools = root.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    std::fs::write(tools.join("loginctl"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(
        tools.join("loginctl"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let output = command(root.path()).output().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("no active local desktop"), "{error}");
    assert!(!error.contains("selected runtime"), "{error}");
    assert!(!root.path().join("external/runtime/server.sock").exists());
}

#[test]
fn explicit_display_is_not_replaced_by_desktop_discovery() {
    let root = tempfile::tempdir().unwrap();
    let output = command(root.path())
        .env("DISPLAY", "invalid-display-name")
        .env("WINIT_UNIX_BACKEND", "x11")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("connect native desktop before companion startup"),
        "{error}"
    );
    assert!(!error.contains("loginctl"), "{error}");
    assert!(!error.contains("selected runtime"), "{error}");
}
