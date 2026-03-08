use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root should exist")
        .to_path_buf()
}

fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be monotonic for test")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("bmux-plugin-e2e-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir should be created");
    dir
}

fn sandbox_paths(root: &Path) -> (PathBuf, PathBuf) {
    if cfg!(target_os = "macos") {
        let app_support = root
            .join("home")
            .join("Library")
            .join("Application Support");
        (app_support.join("bmux"), app_support.join("bmux"))
    } else {
        (
            root.join("config-home").join("bmux"),
            root.join("data-home").join("bmux"),
        )
    }
}

fn configure_bmux_env(
    command: &mut Command,
    home_dir: &Path,
    config_home: &Path,
    data_home: &Path,
) {
    command.env("HOME", home_dir);
    if !cfg!(target_os = "macos") {
        command.env("XDG_CONFIG_HOME", config_home);
        command.env("XDG_DATA_HOME", data_home);
    }
}

fn preserve_toolchain_env(command: &mut Command) {
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }

    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        command.env("CARGO_HOME", cargo_home);
    } else if let Some(home) = &real_home {
        command.env("CARGO_HOME", home.join(".cargo"));
    }

    if let Some(rustup_home) = std::env::var_os("RUSTUP_HOME") {
        command.env("RUSTUP_HOME", rustup_home);
    } else if let Some(home) = &real_home {
        command.env("RUSTUP_HOME", home.join(".rustup"));
    }

    command.env("CARGO_TARGET_DIR", workspace_root().join("target"));
}

#[test]
fn installs_example_plugin_and_runs_command() {
    let root = workspace_root();
    let sandbox = temp_dir();
    let home_dir = sandbox.join("home");
    let config_home = sandbox.join("config-home");
    let data_home = sandbox.join("data-home");
    let runtime_dir = sandbox.join("runtime");
    let tmp_dir = sandbox.join("tmp");
    let (config_dir, data_dir) = sandbox_paths(&sandbox);

    fs::create_dir_all(&home_dir).expect("home dir should be created");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::create_dir_all(&data_dir).expect("data dir should be created");
    fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
    fs::create_dir_all(&tmp_dir).expect("tmp dir should be created");

    fs::write(
        config_dir.join("bmux.toml"),
        "[plugins]\nenabled = [\"example.native\"]\n",
    )
    .expect("config should be written");

    let mut install_command = Command::new(root.join("scripts/install-example-plugin.sh"));
    install_command.current_dir(&root).env("TMPDIR", &tmp_dir);
    configure_bmux_env(&mut install_command, &home_dir, &config_home, &data_home);
    preserve_toolchain_env(&mut install_command);
    let install_status = install_command.status().expect("installer should run");
    assert!(install_status.success(), "installer should succeed");

    let mut run_command = Command::new(env!("CARGO_BIN_EXE_bmux"));
    run_command
        .current_dir(&root)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("TMPDIR", &tmp_dir)
        .args(["plugin", "run", "example.native", "hello", "world"]);
    configure_bmux_env(&mut run_command, &home_dir, &config_home, &data_home);
    let output = run_command.output().expect("plugin command should run");

    assert!(
        output.status.success(),
        "plugin command should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("example.native: hello world"),
        "stdout should contain example plugin output: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
