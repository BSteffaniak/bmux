//! The generated author API across the dylib boundary, in a real server.
//!
//! Packages `examples/counter-plugin` (a provider written against the
//! BPDL-generated traits) and `examples/watcher-plugin` (a consumer using the
//! generated clients) as dynamic libraries, loads both into one isolated
//! server, and checks that the watcher's typed calls reach the counter through
//! the host's checked codec. The same contract runs bundled in
//! `examples/watcher-plugin/tests/both_modes.rs`.
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

/// Append `--cfg bmux_plugin_dylib` to the inherited flags, as the packager does.
fn dynamic_mode_rustflags() -> String {
    let mut flags: Vec<String> = match std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        Ok(encoded) if !encoded.is_empty() => encoded.split('\x1f').map(str::to_owned).collect(),
        _ => std::env::var("RUSTFLAGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    };
    flags.extend(["--cfg".to_owned(), "bmux_plugin_dylib".to_owned()]);
    flags.join("\x1f")
}

/// Generate and build the dynamic wrapper of one example with the shared
/// packager, using the profile of the `bmux` binary under test.
fn build_cdylib(example: &str) -> PathBuf {
    let root = workspace_root();
    let profile_dir = support::bmux_binary()
        .parent()
        .expect("bmux binary directory")
        .to_path_buf();
    let target_dir = profile_dir.parent().expect("cargo target directory");
    let release = profile_dir
        .file_name()
        .is_some_and(|name| name == "release");
    let wrapper = target_dir.join(format!("plugin-packaging/author-api-{example}/backend"));
    let wrapper_target_dir = target_dir.join("plugin-packaging/wrapper-target");
    if wrapper.exists() {
        std::fs::remove_dir_all(&wrapper).expect("remove previous generated wrapper");
    }
    let generate = Command::new("python3")
        .current_dir(&root)
        .arg("-B")
        .arg("scripts/package-plugin.py")
        .arg("--implementation-manifest")
        .arg(format!("examples/{example}/Cargo.toml"))
        .args([
            "--sdk-manifest",
            "packages/plugin-sdk/Cargo.toml",
            "--component",
            "backend",
            "--artifact-id",
        ])
        .arg(format!("author-api-{example}"))
        .arg("--output")
        .arg(&wrapper)
        .output()
        .expect("run shared plugin packaging generator (python3 3.11+ required)");
    support::assert_success(&generate, "generate dynamic wrapper");
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
        .env("CARGO_ENCODED_RUSTFLAGS", dynamic_mode_rustflags())
        .stderr(Stdio::inherit());
    if release {
        build.arg("--release");
    }
    let output = build.output().expect("run cargo build for wrapper");
    assert!(output.status.success(), "build {example} wrapper failed");
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
    libraries.into_iter().next().expect("wrapper library")
}

/// The sidecar packaging publishes: the artifact's embedded manifest, with
/// `entry` pointing at the library.
fn sidecar(library: &Path) -> String {
    let declaration = Command::new("python3")
        .current_dir(workspace_root())
        .arg("-B")
        .arg("scripts/package-plugin.py")
        .arg("--declaration-of")
        .arg(library)
        .output()
        .expect("read the artifact's embedded declaration");
    support::assert_success(&declaration, "read embedded declaration");
    let manifest = String::from_utf8(declaration.stdout).expect("declaration is UTF-8");
    let mut lines = manifest
        .lines()
        .filter(|line| !line.starts_with("entry "))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.insert(0, format!("entry = {:?}", library.display().to_string()));
    lines.join("\n")
}

struct Sandbox {
    env: support::ServerEnv,
    server: Option<Child>,
}

impl Sandbox {
    fn new(plugins: &[(&str, &Path)]) -> Self {
        let env = support::ServerEnv::new("author-api-dylib");
        std::fs::set_permissions(env.root(), std::fs::Permissions::from_mode(0o700))
            .expect("make sandbox root private");
        for dir in ["home", "tmp", "cache"] {
            std::fs::create_dir_all(env.root().join(dir)).expect("create sandbox dir");
        }
        for (id, library) in plugins {
            let dir = env.root().join("plugins").join(id);
            std::fs::create_dir_all(&dir).expect("create plugin dir");
            std::fs::write(dir.join("plugin.toml"), sidecar(library)).expect("write sidecar");
        }
        let enabled = plugins
            .iter()
            .map(|(id, _)| format!("{id:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        env.write_config(&format!(
            "[plugins]\nenabled = [{enabled}]\nsearch_paths = [{:?}]\n",
            env.root().join("plugins").display().to_string()
        ));
        Self { env, server: None }
    }

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

    fn server_log(&self, name: &str) -> String {
        std::fs::read_to_string(self.env.root().join(name)).unwrap_or_default()
    }

    fn evidence(&self) -> String {
        let stderr = self.server_log("server.stderr");
        let tail = stderr.lines().rev().take(40).collect::<Vec<_>>();
        format!(
            "root={}\nserver stderr (tail):\n{}",
            self.env.root().display(),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
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

    /// Poll the server's stderr until every expected line appears.
    fn wait_for_log(&self, expected: &[&str]) {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let stderr = self.server_log("server.stderr");
            if expected.iter().all(|line| stderr.contains(line)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "server never logged {expected:?}: {}",
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
fn generated_clients_reach_a_generated_provider_across_dylibs() {
    let counter = build_cdylib("counter-plugin");
    let watcher = build_cdylib("watcher-plugin");
    let mut sandbox = Sandbox::new(&[("example.counter", &counter), ("example.watcher", &watcher)]);
    sandbox.start();

    // The watcher's server-side activation calls the counter through the
    // host's checked codec, dylib to dylib.
    sandbox.wait_for_log(&[
        "watcher: activation bumped to 2, read back 2",
        "watcher: activation bumped to 5, read back 5",
    ]);

    // A CLI command-local activation of the dylib watcher in the CLI
    // process: its activation also reaches the counter through
    // `Client::remote(host)` on the asynchronous host route. (The `watch`
    // command body itself is a callback-entry call path; see the known gaps
    // in docs/plugin-author-api.md.)
    let watch = sandbox.run(&["plugin", "run", "example.watcher", "watch", "--by", "4"]);
    let stderr = String::from_utf8_lossy(&watch.stderr);
    let bumps = stderr.matches("watcher: activation bumped to").count();
    assert_eq!(
        bumps,
        2,
        "command-local activation reached the counter twice: {stderr}\n{}",
        sandbox.evidence()
    );
    assert!(!stderr.contains("activation call failed"), "{stderr}");
}
