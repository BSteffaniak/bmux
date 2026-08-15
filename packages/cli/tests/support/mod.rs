use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(30);
const SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[must_use]
pub fn bmux_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bmux"))
}

#[derive(Debug)]
pub struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    #[must_use]
    pub fn new(label: &str) -> Self {
        let path = PathBuf::from("/tmp")
            .join(format!("bmux-cp-{label}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path).expect("create isolated process root");
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
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
    gateway_child: Option<Child>,
}

impl ServerEnv {
    #[must_use]
    pub fn new(label: &str) -> Self {
        let root = TempDirGuard::new(label);
        let runtime_dir = root.path().join("runtime");
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        let log_dir = root.path().join("logs");
        for path in [&runtime_dir, &config_dir, &data_dir, &state_dir, &log_dir] {
            std::fs::create_dir_all(path).expect("create isolated bmux directory");
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
            gateway_child: None,
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
        let mut command = Command::new(bmux_binary());
        command
            .args(args)
            .env("BMUX_RUNTIME_DIR", &self.runtime_dir)
            .env("BMUX_CONFIG_DIR", &self.config_dir)
            .env("BMUX_DATA_DIR", &self.data_dir)
            .env("BMUX_STATE_DIR", &self.state_dir)
            .env("BMUX_LOG_DIR", &self.log_dir);
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args)
            .output()
            .expect("run isolated bmux command")
    }

    pub fn start(&mut self) {
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
        wait_for_exit(&mut child, SERVER_STOP_TIMEOUT, self.root());
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

    pub fn start_iroh_gateway(&mut self) -> String {
        assert!(self.gateway_child.is_none(), "iroh gateway already started");
        let mut child = self
            .command(&[
                "server",
                "gateway",
                "--listen",
                "127.0.0.1:0",
                "--host",
                "--host-mode",
                "iroh",
            ])
            .env("BMUX_IROH_DIRECT_BIND", "127.0.0.1:0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn isolated direct iroh gateway");
        let stdout = child.stdout.take().expect("capture iroh gateway stdout");
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(url) = line.strip_prefix("connect URL: ") {
                    let _ = sender.send(url.to_string());
                    return;
                }
            }
        });
        let url = receiver
            .recv_timeout(SERVER_READY_TIMEOUT)
            .expect("direct iroh gateway publishes connect URL");
        self.gateway_child = Some(child);
        url
    }

    fn wait_until_ready(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.run(&["server", "status"]).status.success() {
                return;
            }
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
            "isolated server did not become ready; root={}; output={}",
            self.root().display(),
            self.server_output()
        );
    }
}

impl Drop for ServerEnv {
    fn drop(&mut self) {
        if self.server_child.is_some() {
            let _ = self.run(&["server", "stop"]);
        }
        if let Some(mut child) = self.server_child.take() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        if let Some(mut child) = self.gateway_child.take() {
            let _ = child.kill();
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
            node.stop();
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

#[must_use]
pub fn reserve_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve loopback port")
        .local_addr()
        .expect("read loopback address")
        .port()
}

pub fn generate_ed25519_key(path: &Path) {
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .output()
        .expect("run ssh-keygen");
    assert_success(&output, "generate SSH key");
}

fn wait_for_exit(child: &mut Child, timeout: Duration, root: &Path) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().expect("inspect stopping server").is_some() {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!(
        "isolated server did not stop in time; root={}",
        root.display()
    );
}
