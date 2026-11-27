mod lifecycle;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Bounds a wait for something that must eventually happen. Never a budget:
/// reaching it means a hang, however loaded the machine.
pub const HANG_GUARD: Duration = Duration::from_secs(120);
const SERVER_READY_TIMEOUT: Duration = HANG_GUARD;
/// Above the product's own owned-stop exit bound (120 s), so a stuck stop is
/// reported by the product rather than cut short by the fixture.
const SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(180);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[must_use]
pub fn bmux_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bmux"))
}

/// Short root for isolated process trees: `/tmp` unless `BMUX_TEST_TMP_ROOT` names
/// another short directory, for volumes where `/tmp` can't hold a full run.
fn test_tmp_root() -> PathBuf {
    std::env::var_os("BMUX_TEST_TMP_ROOT").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}

/// 64 random bits: unique per run while keeping server socket paths under the
/// 104-byte macOS `SUN_LEN` limit for long labels and relocated roots.
fn short_unique_suffix() -> String {
    let simple = uuid::Uuid::new_v4().simple().to_string();
    simple[..16].to_owned()
}

#[derive(Debug)]
pub struct TempDirGuard {
    path: PathBuf,
    preserve_runtime: bool,
    retirement_fence: Option<std::fs::File>,
}

impl TempDirGuard {
    #[must_use]
    pub fn new(label: &str) -> Self {
        let path = test_tmp_root().join(format!("bmux-cp-{label}-{}", short_unique_suffix()));
        std::fs::create_dir_all(&path).expect("create isolated process root");
        Self {
            path,
            preserve_runtime: false,
            retirement_fence: None,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if self.preserve_runtime || lifecycle::keep_artifacts() {
            eprintln!("test runtime artifacts retained at {}", self.path.display());
            return;
        }
        if std::thread::panicking()
            && let Err(error) = lifecycle::retain_diagnostics(&self.path)
        {
            eprintln!("failed retaining bounded diagnostics: {error}");
            return;
        }
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            eprintln!("failed removing fixture {}: {error}", self.path.display());
        }
    }
}

#[derive(Debug)]
pub struct ServerEnv {
    root: TempDirGuard,
    pub runtime_dir: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
    server_stdout: PathBuf,
    server_stderr: PathBuf,
    server_child: Option<Child>,
    shell_home: Option<PathBuf>,
}

impl ServerEnv {
    #[must_use]
    pub fn new(label: &str) -> Self {
        let mut root = TempDirGuard::new(label);
        // Cleared only after this fixture verifies retirement, not merely process exit.
        root.preserve_runtime = true;
        let runtime_dir = root.path().join("runtime");
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        let log_dir = root.path().join("logs");
        for path in [&runtime_dir, &config_dir, &data_dir, &state_dir, &log_dir] {
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt as _;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            builder
                .create(path)
                .expect("create isolated private bmux directory");
        }
        let server_stdout = root.path().join("server.stdout.log");
        let server_stderr = root.path().join("server.stderr.log");
        Self {
            root,
            runtime_dir,
            config_dir,
            data_dir,
            state_dir,
            log_dir,
            server_stdout,
            server_stderr,
            server_child: None,
            shell_home: None,
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn write_config(&self, contents: &str) {
        std::fs::write(self.config_dir.join("bmux.toml"), contents)
            .expect("write isolated bmux config");
    }

    #[must_use]
    pub fn command(&self, args: &[&str]) -> Command {
        self.command_with_binary(&bmux_binary(), args)
    }

    #[allow(dead_code)] // This shared fixture module is compiled separately by each integration target.
    pub fn isolate_shell_config(&mut self) {
        let home = self.root().join("home");
        std::fs::create_dir_all(home.join(".config")).unwrap();
        self.shell_home = Some(home);
    }

    pub fn command_with_binary(&self, binary: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(binary);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BMUX_") {
                command.env_remove(key);
            }
        }
        if let Some(home) = &self.shell_home {
            command
                .env("HOME", home)
                .env("XDG_CONFIG_HOME", home.join(".config"));
        }
        command
            .args(args)
            .env("BMUX_NO_BASE_CONFIG", "1")
            .env("BMUX_RUNTIME_NAME", "default")
            .env("BMUX_TARGET", "local")
            .env("BMUX_RUNTIME_DIR", &self.runtime_dir)
            .env("BMUX_CONFIG_DIR", &self.config_dir)
            .env("BMUX_DATA_DIR", &self.data_dir)
            .env("BMUX_STATE_DIR", &self.state_dir)
            .env("BMUX_LOG_DIR", &self.log_dir);
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        lifecycle::run(
            &mut self.command(args),
            self.root(),
            HANG_GUARD,
        )
        .expect("run isolated bmux command")
    }

    fn retire(&mut self) -> std::io::Result<()> {
        let Some(child) = self.server_child.as_mut() else {
            return Err(std::io::Error::other(
                "no owned server handle; retirement unconfirmed",
            ));
        };
        let pid = child.id();
        if child.try_wait()?.is_some() {
            return Err(std::io::Error::other(
                "server exited before execution retirement",
            ));
        }
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.runtime_dir.join("server-meta.json"))?)?;
        if metadata["pid"].as_u64() != Some(u64::from(pid)) {
            return Err(std::io::Error::other(
                "server identity does not match owned handle",
            ));
        }
        // Only the isolated, still-owned server can retire its execution domain.
        let killed = lifecycle::run(
            &mut self.command(&["session", "kill-all", "--force-local"]),
            self.root(),
            SERVER_STOP_TIMEOUT,
        )?;
        if !killed.status.success() {
            return Err(std::io::Error::other("execution retirement failed"));
        }
        let stopped = lifecycle::run(
            &mut self.command(&["server", "stop"]),
            self.root(),
            SERVER_STOP_TIMEOUT,
        )?;
        if !stopped.status.success() {
            return Err(std::io::Error::other("shutdown completion unverified"));
        }
        let child = self.server_child.as_mut().unwrap();
        let deadline = Instant::now() + SERVER_STOP_TIMEOUT;
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                return Err(std::io::Error::other("server process exit unconfirmed"));
            }
            thread::sleep(POLL_INTERVAL);
        }
        self.verify_retirement(&metadata)
    }

    fn verify_retirement(&mut self, metadata: &serde_json::Value) -> std::io::Result<()> {
        let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(
            self.runtime_dir.join("shutdown-result.json"),
        )?)?;
        if receipt["pid"] != metadata["pid"]
            || receipt["started_at_epoch_ms"] != metadata["started_at_epoch_ms"]
            || receipt.get("error") != Some(&serde_json::Value::Null)
        {
            return Err(std::io::Error::other(
                "matching successful shutdown receipt missing",
            ));
        }
        match std::fs::read_dir(self.state_dir.join("pane-executions-v4")) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Err(std::io::Error::other("execution recovery records remain"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let fence = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.runtime_dir.join("server-lifetime.lock"))?;
        fence.try_lock().map_err(std::io::Error::other)?;
        self.root.retirement_fence = Some(fence);
        self.root.preserve_runtime = false;
        Ok(())
    }

    pub fn start(&mut self) {
        self.root.retirement_fence = None;
        self.root.preserve_runtime = true;
        assert!(
            self.server_child.is_none(),
            "isolated server already started"
        );
        let stdout = std::fs::File::create(&self.server_stdout).expect("create server stdout log");
        let stderr = std::fs::File::create(&self.server_stderr).expect("create server stderr log");
        let child = self
            .command(&["server", "start", "--foreground-internal"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn isolated server");
        self.server_child = Some(child);
        self.wait_until_ready(SERVER_READY_TIMEOUT);
    }

    /// Final teardown, unlike stop(), retires sessions instead of preserving restart state.
    #[allow(dead_code)] // Shared support is compiled separately by integration tests without final teardown calls.
    pub fn finish(&mut self) {
        self.retire().unwrap_or_else(|error| {
            panic!(
                "fixture retirement failed: {error}; root={}",
                self.root().display()
            )
        });
        self.server_child.take();
    }

    pub fn stop(&mut self) {
        let Some(mut child) = self.server_child.take() else {
            return;
        };
        let stop = self.run(&["server", "stop"]);
        if !stop.status.success() {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "isolated server stop failed with {:?}: {}; root={}",
                stop.status.code(),
                combined_output(&stop),
                self.root().display()
            );
        }
        let deadline = Instant::now() + SERVER_STOP_TIMEOUT;
        loop {
            if child.try_wait().expect("inspect stopping server").is_some() {
                return;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "isolated server exit unconfirmed; root={}",
                    self.root().display()
                );
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    #[must_use]
    #[allow(dead_code)] // Used by hang diagnostics, not every consumer of this shared fixture.
    pub fn server_pid(&self) -> Option<u32> {
        self.server_child.as_ref().map(Child::id)
    }

    pub fn kill(&mut self) {
        if let Some(mut child) = self.server_child.take() {
            child.kill().expect("kill isolated server");
            child.wait().expect("reap killed isolated server");
        }
    }

    pub fn restart(&mut self) {
        self.kill();
        self.start();
    }

    #[must_use]
    pub fn is_running(&mut self) -> bool {
        self.server_child
            .as_mut()
            .is_some_and(|child| child.try_wait().expect("inspect isolated server").is_none())
    }

    #[must_use]
    pub fn server_output(&self) -> String {
        format!(
            "{}{}",
            std::fs::read_to_string(&self.server_stdout).unwrap_or_default(),
            std::fs::read_to_string(&self.server_stderr).unwrap_or_default()
        )
    }

    fn wait_until_ready(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut last_probe = None;
        while Instant::now() < deadline {
            let probe = self.run(&["server", "status"]);
            if probe.status.success() {
                return;
            }
            last_probe = Some(probe);
            let child = self.server_child.as_mut().expect("server child retained");
            if let Some(status) = child.try_wait().expect("inspect isolated server") {
                panic!(
                    "isolated server exited before readiness with {status}; root={}; output={}",
                    self.root().display(),
                    self.server_output()
                );
            }
            thread::sleep(POLL_INTERVAL);
        }
        panic!(
            "isolated server did not become ready; root={}; last_probe={last_probe:?}; output={}",
            self.root().display(),
            self.server_output()
        );
    }
}

impl Drop for ServerEnv {
    fn drop(&mut self) {
        if self.server_child.is_some()
            && let Err(error) = self.retire()
        {
            eprintln!(
                "fixture retirement unconfirmed at {}: {error}",
                self.root().display()
            );
        }
        if let Some(mut child) = self.server_child.take() {
            if child.try_wait().ok().flatten().is_none() {
                // Only our child handle, never a PID-file-selected process.
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

#[derive(Debug)]
pub struct ClusterProcessHarness {
    nodes: Vec<ServerEnv>,
}

impl ClusterProcessHarness {
    #[must_use]
    pub fn new(labels: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            nodes: labels
                .into_iter()
                .map(|label| ServerEnv::new(label.as_ref()))
                .collect(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[must_use]
    pub fn node(&self, index: usize) -> &ServerEnv {
        &self.nodes[index]
    }

    #[must_use]
    pub fn node_mut(&mut self, index: usize) -> &mut ServerEnv {
        &mut self.nodes[index]
    }

    pub fn start_all(&mut self) {
        for node in &mut self.nodes {
            node.start();
        }
    }

    pub fn stop_all(&mut self) {
        for node in self.nodes.iter_mut().rev() {
            node.finish();
        }
    }
}

pub struct ProcessGuard {
    child: Child,
}

impl ProcessGuard {
    #[must_use]
    pub const fn new(child: Child) -> Self {
        Self { child }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[must_use]
pub fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed with {:?}: {}",
        output.status.code(),
        combined_output(output)
    );
}
