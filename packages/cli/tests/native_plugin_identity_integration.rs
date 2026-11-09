//! Real dynamically packaged plugin against a real, fully sandboxed server.
//!
//! Packages the export-free `examples/native-plugin` implementation through
//! the shared generator (`scripts/package-plugin.py`) into a thin `cdylib`
//! wrapper, loads it into an isolated server through the shared v2 ABI, and
//! checks that its tab-bar service calls are attributed to `example.native`
//! (never `bmux.core`): from CLI commands, from `activate`, and from a
//! background thread that was handed the activation's host services. Also
//! checks that those activation services fail closed once deactivation
//! begins and that calls fail once the server is gone.
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

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// Generate the thin dynamic wrapper for the example with the shared
/// packaging generator, build it with the same profile as the `bmux` binary
/// under test, and return the library Cargo reported.
///
/// The implementation crate stays an export-free rlib; only the generated
/// wrapper is a `cdylib`. The wrapper reuses the workspace lockfile so it
/// links exactly the dependency versions the host was built with. It builds
/// into a target directory dedicated to generated wrappers, never the
/// workspace's own: the out-of-tree wrapper workspace spells path crates'
/// sources absolutely while the workspace spells its members relatively, so
/// sharing one directory lets either build reuse or overwrite the other's
/// artifacts and mixes two copies of one crate (e.g. `bmux_tui`) in a graph.
fn build_example_cdylib() -> PathBuf {
    let root = workspace_root();
    let profile_dir = support::bmux_binary()
        .parent()
        .expect("bmux binary directory")
        .to_path_buf();
    let target_dir = profile_dir.parent().expect("cargo target directory");
    let release = profile_dir
        .file_name()
        .is_some_and(|name| name == "release");
    let wrapper = target_dir.join("plugin-packaging/native-plugin-identity/backend");
    // Shared with other generated wrappers (same source spelling), kept across
    // runs for incremental builds, and outside the regenerated wrapper.
    let wrapper_target_dir = target_dir.join("plugin-packaging/wrapper-target");
    // Regenerate from scratch: the generator refuses to overwrite a
    // directory it does not own, and stale sources must never be reused.
    if wrapper.exists() {
        std::fs::remove_dir_all(&wrapper).expect("remove previous generated wrapper");
    }
    let generate = Command::new("python3")
        .current_dir(&root)
        .arg("-B")
        .arg("scripts/package-plugin.py")
        .args([
            "--implementation-manifest",
            "examples/native-plugin/Cargo.toml",
            "--sdk-manifest",
            "packages/plugin-sdk/Cargo.toml",
            "--component",
            "backend",
            "--artifact-id",
            "native-plugin-identity-test",
            "--output",
        ])
        .arg(&wrapper)
        .output()
        .expect("run shared plugin packaging generator (python3 3.11+ required)");
    support::assert_success(&generate, "generate example dynamic wrapper");
    std::fs::copy(root.join("Cargo.lock"), wrapper.join("Cargo.lock"))
        .expect("pin wrapper to the workspace lockfile");
    let wrapper_package = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(wrapper.join("packaging.json")).expect("read packaging.json"),
    )
    .expect("parse packaging.json")["wrapper_package"]
        .as_str()
        .expect("wrapper package name")
        .to_owned();

    let mut build = Command::new(env!("CARGO"));
    build
        .current_dir(&root)
        .args([
            "build",
            "--message-format=json-render-diagnostics",
            "--manifest-path",
        ])
        .arg(wrapper.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&wrapper_target_dir)
        .stderr(Stdio::inherit());
    if release {
        build.arg("--release");
    }
    let output = build.output().expect("run cargo build for example wrapper");
    assert!(
        output.status.success(),
        "build example dynamic wrapper failed: {}",
        output.status
    );
    // Cargo reports the emitted library; never guess a platform file name.
    let libraries = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| {
            message["reason"] == "compiler-artifact"
                && message["target"]["name"] == wrapper_package.as_str()
                && message["target"]["crate_types"]
                    .as_array()
                    .is_some_and(|types| types.iter().any(|kind| kind == "cdylib"))
        })
        .flat_map(|message| {
            message["filenames"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|name| name.as_str().map(PathBuf::from))
                .collect::<Vec<_>>()
        })
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "dylib" || extension == "so")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        libraries.len(),
        1,
        "expected one wrapper library: {libraries:?}"
    );
    let library = libraries.into_iter().next().expect("wrapper library");
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
fn native_plugin_identity_dynamic_service_calls_are_attributed_to_the_plugin() {
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
    // Lifecycle diagnostics are stderr: command-local activations run in the
    // issuing CLI, whose stdout carries only command output (checked above).
    let stderr = sandbox.server_log("server.stderr");
    for expected in [
        "example.native: status item set during activate",
        "example.native: status item set from background thread",
        "example.native: background call after deactivation was rejected",
        "example.native: deactivated example.native",
    ] {
        assert!(
            stderr.contains(expected),
            "missing {expected:?}:\n{}",
            sandbox.evidence()
        );
    }

    // No server: the command's selected connection is gone; never a fallback.
    let late = sandbox.plugin(&["status-set", "late", "after stop"]);
    assert!(
        !late.status.success(),
        "{}",
        support::combined_output(&late)
    );
}
