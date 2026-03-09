use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
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
    let dir = std::env::temp_dir().join(format!("bp-{nanos:x}"));
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

fn dynamic_library_file(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

fn stage_shipped_permissions_bundle(root: &Path, sandbox: &Path) -> PathBuf {
    let shipped_root = sandbox.join("shipped-plugins");
    let plugin_dir = shipped_root.join("permissions");
    fs::create_dir_all(&plugin_dir).expect("staged shipped plugin dir should be created");

    let library_name = dynamic_library_file("bmux_permissions_plugin");
    fs::copy(
        root.join("target").join("debug").join(&library_name),
        plugin_dir.join(&library_name),
    )
    .expect("permissions plugin library should be staged");

    fs::copy(
        root.join("plugins")
            .join("shipped")
            .join("permissions")
            .join("plugin.toml"),
        plugin_dir.join("plugin.toml"),
    )
    .expect("permissions plugin manifest should be staged");

    shipped_root
}

fn sandbox_setup() -> (
    PathBuf,
    PathBuf,
    PathBuf,
    PathBuf,
    PathBuf,
    PathBuf,
    PathBuf,
) {
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

    (
        sandbox,
        home_dir,
        config_home,
        data_home,
        runtime_dir,
        tmp_dir,
        config_dir,
    )
}

fn run_bmux(
    root: &Path,
    home_dir: &Path,
    config_home: &Path,
    data_home: &Path,
    runtime_dir: &Path,
    tmp_dir: &Path,
    args: &[&str],
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bmux"));
    command
        .current_dir(root)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("TMPDIR", tmp_dir)
        .args(args);
    configure_bmux_env(&mut command, home_dir, config_home, data_home);
    command.output().expect("bmux command should run")
}

fn spawn_bmux(
    root: &Path,
    home_dir: &Path,
    config_home: &Path,
    data_home: &Path,
    runtime_dir: &Path,
    tmp_dir: &Path,
    args: &[&str],
) -> (Child, PathBuf, PathBuf) {
    let stdout_path = tmp_dir.join(format!("bmux-stdout-{}.log", args.join("-")));
    let stderr_path = tmp_dir.join(format!("bmux-stderr-{}.log", args.join("-")));
    let stdout = fs::File::create(&stdout_path).expect("stdout log should be created");
    let stderr = fs::File::create(&stderr_path).expect("stderr log should be created");
    let mut command = Command::new(env!("CARGO_BIN_EXE_bmux"));
    command
        .current_dir(root)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("TMPDIR", tmp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .args(args);
    configure_bmux_env(&mut command, home_dir, config_home, data_home);
    (
        command.spawn().expect("bmux command should spawn"),
        stdout_path,
        stderr_path,
    )
}

fn wait_for_server_ready(
    server: &mut Child,
    stdout_path: &Path,
    stderr_path: &Path,
    root: &Path,
    home_dir: &Path,
    config_home: &Path,
    data_home: &Path,
    runtime_dir: &Path,
    tmp_dir: &Path,
) {
    for _ in 0..25 {
        if let Some(status) = server
            .try_wait()
            .expect("server process should be queryable")
        {
            panic!(
                "server exited before becoming ready: status={status} stdout={} stderr={}",
                fs::read_to_string(stdout_path).unwrap_or_default(),
                fs::read_to_string(stderr_path).unwrap_or_default()
            );
        }
        let status_output = run_bmux(
            root,
            home_dir,
            config_home,
            data_home,
            runtime_dir,
            tmp_dir,
            &["server", "status", "--json"],
        );
        if status_output.status.success() {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }

    let _ = server.kill();
    let _ = server.wait();
    panic!(
        "server did not become ready in time: stdout={} stderr={}",
        fs::read_to_string(stdout_path).unwrap_or_default(),
        fs::read_to_string(stderr_path).unwrap_or_default()
    );
}

#[test]
fn installs_example_plugin_and_runs_command() {
    let root = workspace_root();
    let (_sandbox, home_dir, config_home, data_home, runtime_dir, tmp_dir, config_dir) =
        sandbox_setup();

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

    let output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["plugin", "run", "example.native", "hello", "world"],
    );

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

#[test]
fn shipped_permissions_plugin_handles_permissions_command() {
    let root = workspace_root();
    let (sandbox, home_dir, config_home, data_home, runtime_dir, tmp_dir, config_dir) =
        sandbox_setup();

    let mut build_command = Command::new("cargo");
    build_command
        .current_dir(&root)
        .arg("build")
        .arg("-p")
        .arg("bmux_permissions_plugin")
        .env("TMPDIR", &tmp_dir);
    configure_bmux_env(&mut build_command, &home_dir, &config_home, &data_home);
    preserve_toolchain_env(&mut build_command);
    let build_status = build_command.status().expect("plugin build should run");
    assert!(
        build_status.success(),
        "permissions plugin build should succeed"
    );

    let shipped_root = stage_shipped_permissions_bundle(&root, &sandbox);
    fs::write(
        config_dir.join("bmux.toml"),
        format!(
            "[plugins]\nenabled = [\"bmux.permissions\"]\nsearch_paths = [\"{}\"]\n",
            shipped_root.display()
        ),
    )
    .expect("config should be written");

    let (mut server, server_stdout, server_stderr) = spawn_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["server", "start", "--foreground-internal"],
    );
    wait_for_server_ready(
        &mut server,
        &server_stdout,
        &server_stderr,
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
    );

    let session_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["new-session", "demo"],
    );
    assert!(
        session_output.status.success(),
        "new-session should succeed"
    );

    let permissions_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["permissions", "--session", "demo"],
    );
    assert!(
        permissions_output.status.success(),
        "permissions command should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&permissions_output.stdout),
        String::from_utf8_lossy(&permissions_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&permissions_output.stdout).contains("ROLE")
            && String::from_utf8_lossy(&permissions_output.stdout).contains("owner"),
        "permissions output should include owner role table: {}",
        String::from_utf8_lossy(&permissions_output.stdout)
    );

    let stop_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["server", "stop"],
    );
    assert!(stop_output.status.success(), "server stop should succeed");
    let server_status = server.wait().expect("server process should exit");
    assert!(
        server_status.success(),
        "server process should exit cleanly"
    );
}
