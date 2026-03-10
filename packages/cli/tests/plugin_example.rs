use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_ipc::{IpcEndpoint, SessionRole, SessionSelector};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

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

fn plugin_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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
    config_dir: &Path,
    runtime_dir: &Path,
    data_dir: &Path,
) {
    command.env("HOME", home_dir);
    command.env("BMUX_CONFIG_DIR", config_dir);
    command.env("BMUX_RUNTIME_DIR", runtime_dir);
    command.env("BMUX_DATA_DIR", data_dir);
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

fn stage_shipped_bundle(
    root: &Path,
    sandbox: &Path,
    plugin_dir_name: &str,
    library_stem: &str,
) -> PathBuf {
    let shipped_root = sandbox.join("shipped-plugins");
    let plugin_dir = shipped_root.join(plugin_dir_name);
    fs::create_dir_all(&plugin_dir).expect("staged shipped plugin dir should be created");

    let library_name = dynamic_library_file(library_stem);
    fs::copy(
        root.join("target").join("debug").join(&library_name),
        plugin_dir.join(&library_name),
    )
    .expect("shipped plugin library should be staged");

    fs::copy(
        root.join("plugins")
            .join("shipped")
            .join(plugin_dir_name)
            .join("plugin.toml"),
        plugin_dir.join("plugin.toml"),
    )
    .expect("shipped plugin manifest should be staged");

    shipped_root
}

fn config_paths_for_test(config_dir: &Path, runtime_dir: &Path, data_home: &Path) -> ConfigPaths {
    let data_dir = if cfg!(target_os = "macos") {
        config_dir.to_path_buf()
    } else {
        data_home.join("bmux")
    };
    ConfigPaths::new(config_dir.to_path_buf(), runtime_dir.join("bmux"), data_dir)
}

fn config_paths_from_sandbox_env(
    home_dir: &Path,
    config_home: &Path,
    data_home: &Path,
    runtime_dir: &Path,
) -> ConfigPaths {
    let config_dir = if cfg!(target_os = "macos") {
        home_dir
            .join("Library")
            .join("Application Support")
            .join("bmux")
    } else {
        config_home.join("bmux")
    };
    config_paths_for_test(&config_dir, runtime_dir, data_home)
}

fn test_endpoint(paths: &ConfigPaths) -> IpcEndpoint {
    #[cfg(unix)]
    {
        IpcEndpoint::unix_socket(paths.server_socket())
    }

    #[cfg(windows)]
    {
        IpcEndpoint::windows_named_pipe(paths.server_named_pipe())
    }
}

fn with_runtime<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime should build")
        .block_on(future)
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
    let paths = config_paths_from_sandbox_env(home_dir, config_home, data_home, runtime_dir);
    let mut command = Command::new(env!("CARGO_BIN_EXE_bmux"));
    command
        .current_dir(root)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("TMPDIR", tmp_dir)
        .args(args);
    configure_bmux_env(
        &mut command,
        home_dir,
        config_home,
        data_home,
        &paths.config_dir,
        &paths.runtime_dir,
        &paths.data_dir,
    );
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
    let paths = config_paths_from_sandbox_env(home_dir, config_home, data_home, runtime_dir);
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
    configure_bmux_env(
        &mut command,
        home_dir,
        config_home,
        data_home,
        &paths.config_dir,
        &paths.runtime_dir,
        &paths.data_dir,
    );
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
    let _guard = plugin_test_lock()
        .lock()
        .expect("plugin test lock poisoned");
    let root = workspace_root();
    let (_sandbox, home_dir, config_home, data_home, runtime_dir, tmp_dir, config_dir) =
        sandbox_setup();
    let paths = config_paths_for_test(&config_dir, &runtime_dir, &data_home);

    fs::write(
        config_dir.join("bmux.toml"),
        "[plugins]\nenabled = [\"example.native\"]\n",
    )
    .expect("config should be written");

    let mut install_command = Command::new(root.join("scripts/install-example-plugin.sh"));
    install_command.current_dir(&root).env("TMPDIR", &tmp_dir);
    configure_bmux_env(
        &mut install_command,
        &home_dir,
        &config_home,
        &data_home,
        &paths.config_dir,
        &paths.runtime_dir,
        &paths.data_dir,
    );
    preserve_toolchain_env(&mut install_command);
    let install_status = install_command.status().expect("installer should run");
    assert!(install_status.success(), "installer should succeed");

    let plugin_list_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["plugin", "list", "--json"],
    );
    assert!(
        plugin_list_output.status.success(),
        "plugin list should succeed after install: stdout={} stderr={}",
        String::from_utf8_lossy(&plugin_list_output.stdout),
        String::from_utf8_lossy(&plugin_list_output.stderr)
    );
    let plugin_list = String::from_utf8_lossy(&plugin_list_output.stdout);
    assert!(
        plugin_list.contains("example.native") && plugin_list.contains("hello"),
        "plugin list should include installed command metadata: {plugin_list}"
    );

    let output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["hello", "world"],
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
    let _guard = plugin_test_lock()
        .lock()
        .expect("plugin test lock poisoned");
    let root = workspace_root();
    let (sandbox, home_dir, config_home, data_home, runtime_dir, tmp_dir, config_dir) =
        sandbox_setup();
    let paths = config_paths_for_test(&config_dir, &runtime_dir, &data_home);

    let mut build_command = Command::new("cargo");
    build_command
        .current_dir(&root)
        .arg("build")
        .arg("-p")
        .arg("bmux_permissions_plugin")
        .env("TMPDIR", &tmp_dir);
    configure_bmux_env(
        &mut build_command,
        &home_dir,
        &config_home,
        &data_home,
        &paths.config_dir,
        &paths.runtime_dir,
        &paths.data_dir,
    );
    preserve_toolchain_env(&mut build_command);
    let build_status = build_command.status().expect("plugin build should run");
    assert!(
        build_status.success(),
        "permissions plugin build should succeed"
    );

    let shipped_root =
        stage_shipped_bundle(&root, &sandbox, "permissions", "bmux_permissions_plugin");
    fs::write(
        config_dir.join("bmux.toml"),
        format!(
            "[plugins]\nenabled = [\"bmux.permissions\"]\nsearch_paths = [\"{}\"]\n",
            shipped_root.display()
        ),
    )
    .expect("config should be written");

    let help_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["permissions", "--help"],
    );
    assert!(
        help_output.status.success(),
        "permissions help should succeed"
    );
    let permissions_help = String::from_utf8_lossy(&help_output.stdout);
    assert!(
        permissions_help.contains("--session") && permissions_help.contains("--watch"),
        "permissions help should include plugin-defined flags: {permissions_help}"
    );

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
        &["session", "permissions", "--session", "demo"],
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

    let endpoint = test_endpoint(&paths);
    let (target_tx, target_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    let target_thread = {
        let endpoint = endpoint.clone();
        std::thread::spawn(move || {
            with_runtime(async move {
                let mut target = BmuxClient::connect_with_principal(
                    &endpoint,
                    Duration::from_secs(2),
                    "plugin-e2e-target",
                    Uuid::new_v4(),
                )
                .await
                .expect("target client should connect");
                let target_id = target.whoami().await.expect("target whoami should succeed");
                target_tx.send(target_id).expect("target id should send");
                shutdown_rx
                    .recv()
                    .expect("target shutdown signal should arrive");
                drop(target);
            });
        })
    };
    let target_client_id = target_rx
        .recv()
        .expect("target client id should be received");

    let grant_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &[
            "session",
            "grant",
            "--session",
            "demo",
            "--client",
            &target_client_id.to_string(),
            "--role",
            "writer",
        ],
    );
    assert!(
        grant_output.status.success(),
        "grant command should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&grant_output.stdout),
        String::from_utf8_lossy(&grant_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&grant_output.stdout).contains("granted role writer"),
        "grant output should confirm writer role: {}",
        String::from_utf8_lossy(&grant_output.stdout)
    );

    let permissions_after_grant = with_runtime(async {
        let mut owner = BmuxClient::connect_with_paths(&paths, "plugin-e2e-owner-list")
            .await
            .expect("owner list client should connect");
        owner
            .list_permissions(SessionSelector::ByName("demo".to_string()))
            .await
            .expect("permissions should list after grant")
    });
    assert!(
        permissions_after_grant.iter().any(|entry| {
            entry.client_id == target_client_id && entry.role == SessionRole::Writer
        }),
        "granted writer role should be visible in permission list"
    );

    let grouped_permissions_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["permissions", "--session", "demo", "--json"],
    );
    assert!(
        grouped_permissions_output.status.success(),
        "top-level permissions command should succeed after grant: stdout={} stderr={}",
        String::from_utf8_lossy(&grouped_permissions_output.stdout),
        String::from_utf8_lossy(&grouped_permissions_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&grouped_permissions_output.stdout)
            .contains(&target_client_id.to_string()),
        "top-level permissions json should include granted client: {}",
        String::from_utf8_lossy(&grouped_permissions_output.stdout)
    );

    let revoke_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &[
            "session",
            "revoke",
            "--session",
            "demo",
            "--client",
            &target_client_id.to_string(),
        ],
    );
    assert!(
        revoke_output.status.success(),
        "revoke command should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&revoke_output.stdout),
        String::from_utf8_lossy(&revoke_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&revoke_output.stdout).contains("revoked explicit role"),
        "revoke output should confirm role removal: {}",
        String::from_utf8_lossy(&revoke_output.stdout)
    );

    let permissions_after_revoke = with_runtime(async {
        let mut owner = BmuxClient::connect_with_paths(&paths, "plugin-e2e-owner-list-after")
            .await
            .expect("owner list client should reconnect");
        owner
            .list_permissions(SessionSelector::ByName("demo".to_string()))
            .await
            .expect("permissions should list after revoke")
    });
    assert!(
        !permissions_after_revoke
            .iter()
            .any(|entry| entry.client_id == target_client_id),
        "revoked role should disappear from explicit permission list"
    );

    shutdown_tx
        .send(())
        .expect("target client shutdown signal should send");
    target_thread
        .join()
        .expect("target client thread should join cleanly");

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

#[test]
fn shipped_windows_plugin_handles_window_commands() {
    let _guard = plugin_test_lock()
        .lock()
        .expect("plugin test lock poisoned");
    let root = workspace_root();
    let (sandbox, home_dir, config_home, data_home, runtime_dir, tmp_dir, config_dir) =
        sandbox_setup();
    let paths = config_paths_for_test(&config_dir, &runtime_dir, &data_home);

    let mut build_command = Command::new("cargo");
    build_command
        .current_dir(&root)
        .arg("build")
        .arg("-p")
        .arg("bmux_windows_plugin")
        .env("TMPDIR", &tmp_dir);
    configure_bmux_env(
        &mut build_command,
        &home_dir,
        &config_home,
        &data_home,
        &paths.config_dir,
        &paths.runtime_dir,
        &paths.data_dir,
    );
    preserve_toolchain_env(&mut build_command);
    let build_status = build_command.status().expect("plugin build should run");
    assert!(
        build_status.success(),
        "windows plugin build should succeed"
    );

    let shipped_root = stage_shipped_bundle(&root, &sandbox, "windows", "bmux_windows_plugin");
    fs::write(
        config_dir.join("bmux.toml"),
        format!(
            "[plugins]\nenabled = [\"bmux.windows\"]\nsearch_paths = [\"{}\"]\n",
            shipped_root.display()
        ),
    )
    .expect("config should be written");

    let help_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["window", "new", "--help"],
    );
    assert!(
        help_output.status.success(),
        "window new help should succeed"
    );
    let window_help = String::from_utf8_lossy(&help_output.stdout);
    assert!(
        window_help.contains("--session") && window_help.contains("--name"),
        "window new help should include plugin-defined flags: {window_help}"
    );

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

    let new_window_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["window", "new", "--session", "demo", "--name", "notes"],
    );
    assert!(
        new_window_output.status.success(),
        "window new should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&new_window_output.stdout),
        String::from_utf8_lossy(&new_window_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&new_window_output.stdout).contains("notes"),
        "window new output should mention notes window: {}",
        String::from_utf8_lossy(&new_window_output.stdout)
    );

    let list_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &["list-windows", "--session", "demo", "--json"],
    );
    assert!(
        list_output.status.success(),
        "list-windows should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&list_output.stdout),
        String::from_utf8_lossy(&list_output.stderr)
    );
    let list_json = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_json.contains("notes") && list_json.contains("\"active\": true"),
        "list-windows json should include active notes window: {list_json}"
    );

    let paths = config_paths_for_test(&config_dir, &runtime_dir, &data_home);
    let windows_after_create = with_runtime(async {
        let mut client = BmuxClient::connect_with_paths(&paths, "windows-plugin-e2e-list")
            .await
            .expect("client should connect");
        client
            .list_windows(Some(SessionSelector::ByName("demo".to_string())))
            .await
            .expect("window list should succeed")
    });
    let notes_window = windows_after_create
        .iter()
        .find(|window| window.name.as_deref() == Some("notes"))
        .expect("notes window should exist");

    let switch_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &[
            "window",
            "switch",
            &notes_window.id.to_string(),
            "--session",
            "demo",
        ],
    );
    assert!(
        switch_output.status.success(),
        "window switch should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&switch_output.stdout),
        String::from_utf8_lossy(&switch_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&switch_output.stdout).contains("notes"),
        "window switch output should mention notes window: {}",
        String::from_utf8_lossy(&switch_output.stdout)
    );

    let kill_output = run_bmux(
        &root,
        &home_dir,
        &config_home,
        &data_home,
        &runtime_dir,
        &tmp_dir,
        &[
            "kill-window",
            &notes_window.id.to_string(),
            "--session",
            "demo",
        ],
    );
    assert!(
        kill_output.status.success(),
        "kill-window should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&kill_output.stdout),
        String::from_utf8_lossy(&kill_output.stderr)
    );

    let windows_after_kill = with_runtime(async {
        let mut client = BmuxClient::connect_with_paths(&paths, "windows-plugin-e2e-list-after")
            .await
            .expect("client should reconnect");
        client
            .list_windows(Some(SessionSelector::ByName("demo".to_string())))
            .await
            .expect("window list after kill should succeed")
    });
    assert!(
        !windows_after_kill
            .iter()
            .any(|window| window.id == notes_window.id),
        "notes window should be removed after kill"
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
