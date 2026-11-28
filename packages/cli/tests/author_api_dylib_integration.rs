//! The generated author API across the dylib boundary, in a real server.
//!
//! Packages `examples/counter-plugin` (a provider written against the
//! BPDL-generated traits) and `examples/watcher-plugin` (a consumer using the
//! generated clients) as dynamic libraries straight from their own crates (no
//! wrapper), loads both into one isolated
//! server, and checks that the watcher's typed calls reach the counter through
//! the host's checked codec, and that the watcher's event subscription sees
//! the counter's changes. The same contract runs bundled and as dylibs under
//! `TestHost` in `examples/watcher-plugin/tests/both_modes.rs`.
#![cfg(unix)]
#[path = "support/packaging.rs"]
mod packaging;
#[allow(dead_code)] // Reuse isolated paths and binary selection only.
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = support::HANG_GUARD;
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(100);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// Package one example crate as a dylib: the crate exports its own entry
/// (`plugin!` + `dynamic-backend`); no wrapper crate is generated.
fn build_cdylib(example: &str) -> PathBuf {
    packaging::package_plugin_dylib(
        &workspace_root().join(format!("examples/{example}/Cargo.toml")),
        &format!("author-api-{example}"),
    )
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

    /// A sandbox whose plugins are copied directories (a process plugin's
    /// manifest and entry script) rather than packaged libraries.
    fn with_plugin_dirs(plugins: &[(&str, &Path)]) -> Self {
        let sandbox = Self::new(&[]);
        for (id, source) in plugins {
            let dir = sandbox.env.root().join("plugins").join(id);
            std::fs::create_dir_all(&dir).expect("create plugin dir");
            for entry in std::fs::read_dir(source).expect("read plugin source") {
                let entry = entry.expect("plugin source entry");
                if entry.file_type().expect("entry type").is_file() {
                    std::fs::copy(entry.path(), dir.join(entry.file_name()))
                        .expect("copy plugin file");
                }
            }
        }
        let enabled = plugins
            .iter()
            .map(|(id, _)| format!("{id:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        sandbox.env.write_config(&format!(
            "[plugins]\nenabled = [{enabled}]\nsearch_paths = [{:?}]\n",
            sandbox.env.root().join("plugins").display().to_string()
        ));
        sandbox
    }

    /// The first presentation `plugin_id` publishes, as a streaming client
    /// receives it (replayed when it was published before the client connected).
    fn published_presentation(
        &self,
        plugin_id: &str,
    ) -> bmux_plugin::surface::PublishedPresentation {
        let paths = bmux_config::ConfigPaths::new(
            self.env.config_dir.clone(),
            self.env.runtime_dir.clone(),
            self.env.data_dir.clone(),
            self.env.state_dir.clone(),
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let kind = bmux_plugin::surface::presentation_event_kind(plugin_id);
        runtime.block_on(async {
            let client = bmux_client::BmuxClient::connect_with_paths_and_timeout(
                &paths,
                "presentation-test",
                Duration::from_secs(5),
            )
            .await
            .expect("connect");
            let mut client =
                bmux_client::StreamingBmuxClient::from_client(client).expect("streaming client");
            client.enable_event_push().await.expect("enable event push");
            loop {
                let event =
                    tokio::time::timeout(Duration::from_secs(10), client.event_receiver().recv())
                        .await
                        .expect("the retained presentation is replayed")
                        .expect("event stream open");
                if let bmux_client::ServerEvent::PluginBusEvent {
                    kind: event_kind,
                    payload,
                } = event
                    && event_kind == kind
                {
                    break serde_json::from_slice(&payload).expect("a published presentation");
                }
            }
        })
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

    // A fresh CLI invocation runs the command, not the watcher's service lifecycle.
    let watch = sandbox.run(&["plugin", "run", "example.watcher", "watch", "--by", "4"]);
    let stderr = String::from_utf8_lossy(&watch.stderr);
    assert!(!stderr.contains("watcher: activation"), "{stderr}");
    // Server activation 2 + 3, then the command's 4.
    support::assert_success(&watch, "watch command body");
    let stdout = String::from_utf8_lossy(&watch.stdout);
    assert!(
        stdout.contains("watcher: bumped to 9, read back 9"),
        "{}\n{}",
        support::combined_output(&watch),
        sandbox.evidence()
    );

    // The server-side watcher follows the counter's retained change channel
    // through its generated `subscribe_changed()`: the counter (a dylib)
    // publishes on the host bus and the watcher's activation-owned task (a
    // dylib) receives it, including the change the CLI caused.
    sandbox.wait_for_log(&["watcher: observed 9"]);

    // The separate CLI bumps the same server-owned counter.
    let bump = sandbox.run(&["plugin", "run", "example.counter", "bump", "--by", "1"]);
    support::assert_success(&bump, "counter bump");
    assert!(
        String::from_utf8_lossy(&bump.stdout).contains("counter: 10"),
        "{}\n{}",
        support::combined_output(&bump),
        sandbox.evidence()
    );
    sandbox.wait_for_log(&["watcher: observed 10"]);
}

/// A process plugin (a Python script speaking the stdio protocol) publishes its
/// presentation while answering activation; the server forwards it to
/// streaming clients like any plugin's.
#[test]
fn a_process_plugin_presentation_reaches_streaming_clients() {
    let source = workspace_root().join("examples/process-plugin-python");
    let mut sandbox = Sandbox::with_plugin_dirs(&[("example.process.python", &source)]);
    sandbox.start();
    let published = sandbox.published_presentation("example.process.python");
    assert_eq!(published.layouts.len(), 1);
    assert_eq!(published.surfaces.len(), 1);
    let surfaces = bmux_plugin::surface::PluginSurfaceRegistry::new(8);
    surfaces
        .publish(
            "example.process.python",
            bmux_plugin::surface::PluginSurfaceSnapshot {
                revision: published.revision,
                surfaces: published.surfaces,
            },
        )
        .expect("the plugin's own surfaces");
}

/// A dylib plugin draws attach UI by publishing its presentation as retained
/// state: the server forwards it to streaming clients (replayed to a client
/// that connects after it was published), and the snapshot it sends lowers
/// into the same layout and surface registries attach clients draw from.
#[test]
fn a_dylib_plugin_presentation_reaches_streaming_clients() {
    let presentation = build_cdylib("presentation-plugin");
    let mut sandbox = Sandbox::new(&[("example.presentation", &presentation)]);
    sandbox.start();
    sandbox.wait_for_log(&["presentation: published"]);

    let paths = bmux_config::ConfigPaths::new(
        sandbox.env.config_dir.clone(),
        sandbox.env.runtime_dir.clone(),
        sandbox.env.data_dir.clone(),
        sandbox.env.state_dir.clone(),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let published = runtime.block_on(async {
        let client = bmux_client::BmuxClient::connect_with_paths_and_timeout(
            &paths,
            "presentation-test",
            Duration::from_secs(5),
        )
        .await
        .expect("connect");
        let mut client =
            bmux_client::StreamingBmuxClient::from_client(client).expect("streaming client");
        client.enable_event_push().await.expect("enable event push");
        let kind = bmux_plugin::surface::presentation_event_kind("example.presentation");
        loop {
            let event =
                tokio::time::timeout(Duration::from_secs(10), client.event_receiver().recv())
                    .await
                    .expect("the retained presentation is replayed")
                    .expect("event stream open");
            if let bmux_client::ServerEvent::PluginBusEvent {
                kind: event_kind,
                payload,
            } = event
                && event_kind == kind
            {
                break serde_json::from_slice::<bmux_plugin::surface::PublishedPresentation>(
                    &payload,
                )
                .expect("a published presentation");
            }
        }
    });

    let layouts = bmux_plugin::layout::PluginLayoutRegistry::new(8);
    layouts
        .publish(
            "example.presentation",
            bmux_plugin::layout::PluginLayoutSnapshot {
                revision: published.revision,
                requests: published.layouts.clone(),
            },
        )
        .expect("the plugin's own layout");
    let surfaces = bmux_plugin::surface::PluginSurfaceRegistry::new(8);
    surfaces
        .publish(
            "example.presentation",
            bmux_plugin::surface::PluginSurfaceSnapshot {
                revision: published.revision,
                surfaces: published.surfaces.clone(),
            },
        )
        .expect("the plugin's own surfaces");
    let resolved = bmux_plugin::layout::resolve_plugin_layout(
        bmux_plugin::ExtensionRect::new(0, 0, 80, 24),
        (1, 1),
        &layouts.requests(),
    )
    .expect("layout resolves");
    assert_eq!(
        resolved.allocations[0].rect,
        bmux_plugin::ExtensionRect::new(62, 0, 18, 24)
    );
    assert_eq!(published.surfaces[0].ops.len(), 2);
}
