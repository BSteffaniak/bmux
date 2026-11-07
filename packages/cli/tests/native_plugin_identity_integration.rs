//! Real `cdylib` plugin against a real, fully sandboxed bmux server.
//!
//! Builds `examples/native-plugin` as a dynamic library, loads it into an
//! isolated server, and checks that its tab-bar service calls are relayed by
//! the host and attributed to `example.native` (never `bmux.core`): from CLI
//! commands, from `activate`, and from a background thread holding the
//! activation context. Also checks that the activation context stops working
//! once deactivation begins and that calls fail once the server is gone.
#![cfg(unix)]
#[allow(dead_code)] // Reuse isolated paths and binary selection only.
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(100);

#[cfg(target_os = "macos")]
const LIBRARY: &str = "libbmux_example_native_plugin.dylib";
#[cfg(not(target_os = "macos"))]
const LIBRARY: &str = "libbmux_example_native_plugin.so";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// Build the example as a cdylib into the same target directory and profile
/// as the `bmux` binary under test, and return the library path.
fn build_example_cdylib() -> PathBuf {
    let profile_dir = support::bmux_binary()
        .parent()
        .expect("bmux binary directory")
        .to_path_buf();
    let target_dir = profile_dir.parent().expect("cargo target directory");
    let mut build = Command::new(env!("CARGO"));
    build
        .current_dir(workspace_root())
        .args(["build", "-p", "bmux_example_native_plugin", "--lib"])
        .arg("--target-dir")
        .arg(target_dir);
    if profile_dir
        .file_name()
        .is_some_and(|name| name == "release")
    {
        build.arg("--release");
    }
    let output = build.output().expect("run cargo build for example plugin");
    support::assert_success(&output, "build example cdylib");
    let library = profile_dir.join(LIBRARY);
    assert!(library.is_file(), "missing {}", library.display());
    library
}

struct Sandbox {
    env: support::ServerEnv,
    server: Option<Child>,
}

impl Sandbox {
    fn new(library: &Path) -> Self {
        let env = support::ServerEnv::new("native-plugin-identity");
        // The runtime socket directory's parent must be private.
        std::fs::set_permissions(env.root(), std::fs::Permissions::from_mode(0o700))
            .expect("make sandbox root private");
        for dir in ["home", "tmp", "cache", "plugins/example-native"] {
            std::fs::create_dir_all(env.root().join(dir)).expect("create sandbox dir");
        }
        let manifest =
            std::fs::read_to_string(workspace_root().join("examples/native-plugin/plugin.toml"))
                .expect("read example manifest")
                .lines()
                .map(|line| {
                    if line.starts_with("entry ") {
                        format!("entry = {:?}", library.display().to_string())
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        std::fs::write(
            env.root().join("plugins/example-native/plugin.toml"),
            manifest,
        )
        .expect("write sandbox manifest");
        env.write_config(&format!(
            "[plugins]\nenabled = [\"example.native\"]\nsearch_paths = [{:?}]\n\n\
             [plugins.settings.\"example.native\"]\nbackground_status = true\n",
            env.root().join("plugins").display().to_string()
        ));
        Self { env, server: None }
    }

    /// A bmux command that sees only the sandbox: no inherited config,
    /// runtime, data, or state locations, and no real user home.
    fn command(&self, args: &[&str]) -> Command {
        let env = &self.env;
        let mut command = env.command(args);
        command
            .env_clear()
            .current_dir(env.root())
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", env.root().join("home"))
            .env("TMPDIR", env.root().join("tmp"))
            .env("XDG_CONFIG_HOME", &env.config_dir)
            .env("XDG_RUNTIME_DIR", &env.runtime_dir)
            .env("XDG_DATA_HOME", &env.data_dir)
            .env("XDG_STATE_HOME", &env.state_dir)
            .env("XDG_CACHE_HOME", env.root().join("cache"))
            .env("BMUX_CONFIG_DIR", &env.config_dir)
            .env("BMUX_RUNTIME_DIR", &env.runtime_dir)
            .env("BMUX_DATA_DIR", &env.data_dir)
            .env("BMUX_STATE_DIR", &env.state_dir)
            .env("BMUX_LOG_DIR", &env.log_dir)
            .env("BMUX_CONFIG", env.config_dir.join("bmux.toml"))
            .env("BMUX_NO_BASE_CONFIG", "1")
            .env("BMUX_RUNTIME_NAME", "default")
            .env("BMUX_TARGET", "local")
            .env("TERM", "xterm-256color")
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run sandboxed bmux")
    }

    fn plugin(&self, args: &[&str]) -> Output {
        let mut full = vec!["plugin", "run", "example.native"];
        full.extend_from_slice(args);
        self.run(&full)
    }

    fn server_log(&self, name: &str) -> String {
        std::fs::read_to_string(self.env.root().join(name)).unwrap_or_default()
    }

    fn evidence(&self) -> String {
        format!(
            "root={}\nserver stdout:\n{}\nserver stderr (tail):\n{}",
            self.env.root().display(),
            self.server_log("server.stdout"),
            self.server_log("server.stderr")
                .lines()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    fn start(&mut self) {
        let stdout = std::fs::File::create(self.env.root().join("server.stdout")).unwrap();
        let stderr = std::fs::File::create(self.env.root().join("server.stderr")).unwrap();
        let child = self
            .command(&["server", "start", "--foreground-internal"])
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .expect("spawn sandboxed server");
        self.server = Some(child);
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if self.run(&["server", "status"]).status.success() {
                return;
            }
            let exited = self
                .server
                .as_mut()
                .and_then(|child| child.try_wait().expect("inspect server"));
            assert!(exited.is_none(), "server exited early: {}", self.evidence());
            std::thread::sleep(POLL);
        }
        panic!("server not ready: {}", self.evidence());
    }

    fn stop(&mut self) {
        let stop = self.run(&["server", "stop"]);
        support::assert_success(&stop, "server stop");
        let mut child = self.server.take().expect("server running");
        let deadline = Instant::now() + STOP_TIMEOUT;
        while child.try_wait().expect("inspect server").is_none() {
            assert!(
                Instant::now() < deadline,
                "server did not stop: {}",
                self.evidence()
            );
            std::thread::sleep(POLL);
        }
    }

    /// Poll `status-list` until every expected line is present.
    fn wait_for_items(&self, expected: &[&str]) -> String {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let output = self.plugin(&["status-list"]);
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            if output.status.success() && expected.iter().all(|line| stdout.contains(line)) {
                return stdout;
            }
            assert!(
                Instant::now() < deadline,
                "status items {expected:?} not listed; last: {}\n{}",
                support::combined_output(&output),
                self.evidence()
            );
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(mut child) = self.server.take() {
            let _ = self.run(&["server", "stop"]);
            let deadline = Instant::now() + STOP_TIMEOUT;
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(POLL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn cdylib_plugin_service_calls_are_relayed_and_attributed_to_the_plugin() {
    let library = build_example_cdylib();
    let mut sandbox = Sandbox::new(&library);
    sandbox.start();

    // Calls made inside `activate` and from a background thread started there.
    sandbox.wait_for_items(&[
        "example.native/activate: set during activate",
        "example.native/background: set from a background thread",
    ]);

    let set = sandbox.plugin(&["status-set", "build", "build ok", "--severity", "success"]);
    support::assert_success(&set, "status-set");
    assert!(
        String::from_utf8_lossy(&set.stdout).contains("status item 'build' set: build ok"),
        "{}",
        support::combined_output(&set)
    );
    let notify = sandbox.plugin(&["notify", "Deploy finished", "--body", "all green"]);
    support::assert_success(&notify, "notify");
    assert!(
        String::from_utf8_lossy(&notify.stdout).contains("toast queued: "),
        "{}",
        support::combined_output(&notify)
    );

    let listed = sandbox.wait_for_items(&["example.native/build: build ok"]);
    assert!(
        !listed.lines().any(|line| line.starts_with("bmux.core/")),
        "plugin items misattributed to the host:\n{listed}"
    );
    let own = sandbox.plugin(&["status-list", "example.native"]);
    support::assert_success(&own, "status-list example.native");
    let own = String::from_utf8_lossy(&own.stdout).into_owned();
    assert_eq!(own.lines().count(), 3, "{own}");
    assert!(
        own.lines().all(|line| line.starts_with("example.native/")),
        "{own}"
    );

    sandbox.stop();
    let stdout = sandbox.server_log("server.stdout");
    for expected in [
        "example.native: status item set during activate",
        "example.native: status item set from background thread",
        "example.native: background call after deactivation was rejected",
        "example.native: deactivated example.native",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?}:\n{}",
            sandbox.evidence()
        );
    }

    // No server: the host has nowhere to relay to.
    let late = sandbox.plugin(&["status-set", "late", "after stop"]);
    assert!(
        !late.status.success(),
        "{}",
        support::combined_output(&late)
    );
}
