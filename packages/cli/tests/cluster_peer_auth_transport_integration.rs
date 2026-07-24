use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

fn bmux_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bmux"))
}

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(label: &str) -> Self {
        let path = PathBuf::from("/tmp").join(format!("bmux-pa-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct ServerEnv {
    _root: TempDirGuard,
    runtime_dir: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    state_dir: PathBuf,
    log_dir: PathBuf,
    started: bool,
    server_child: Option<std::process::Child>,
}

impl ServerEnv {
    fn new(label: &str) -> Self {
        let root = TempDirGuard::new(label);
        let runtime_dir = root.path().join("runtime");
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        let log_dir = root.path().join("logs");
        for path in [&runtime_dir, &config_dir, &data_dir, &state_dir, &log_dir] {
            std::fs::create_dir_all(path).expect("create isolated bmux directory");
        }
        Self {
            _root: root,
            runtime_dir,
            config_dir,
            data_dir,
            state_dir,
            log_dir,
            started: false,
            server_child: None,
        }
    }

    fn write_config(&self, contents: &str) {
        std::fs::write(self.config_dir.join("bmux.toml"), contents)
            .expect("write isolated bmux config");
    }

    fn command(&self, args: &[&str]) -> Command {
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

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run bmux command")
    }

    fn start(&mut self) {
        let child = self
            .command(&["server", "start", "--foreground-internal"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn isolated server");
        self.started = true;
        self.server_child = Some(child);
        for _ in 0..200 {
            if self.run(&["server", "status"]).status.success() {
                return;
            }
            let child = self
                .server_child
                .as_mut()
                .expect("server child should be retained");
            if let Some(status) = child.try_wait().expect("inspect isolated server") {
                panic!(
                    "isolated server exited before readiness with {status}; root={}",
                    self._root.path().display(),
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "isolated server did not become ready; root={}",
            self._root.path().display()
        );
    }
}

impl Drop for ServerEnv {
    fn drop(&mut self) {
        if self.started {
            let _ = self.run(&["server", "stop"]);
        }
        if let Some(child) = self.server_child.as_mut() {
            let _ = child.wait();
        }
    }
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed with {:?}: {}",
        output.status.code(),
        combined_output(output)
    );
}

fn reserve_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve loopback port")
        .local_addr()
        .expect("read loopback address")
        .port()
}

#[test]
fn tls_join_and_leave_require_real_mutual_peer_authentication() {
    let port = reserve_tcp_port();
    let mut issuer = ServerEnv::new("tls-issuer");
    issuer.write_config(&format!(
        "[server.gateway]\nenabled = true\nlisten = \"127.0.0.1:{port}\"\nquick = true\n"
    ));
    let mut joiner = ServerEnv::new("tls-joiner");
    joiner.write_config(&format!(
        "[connections.tls_trust]\nmode = \"trust_new\"\n\n[connections.targets.issuer]\ntransport = \"tls\"\nhost = \"127.0.0.1\"\nport = {port}\nserver_name = \"localhost\"\nserver_start_mode = \"require_running\"\n"
    ));

    issuer.start();
    joiner.start();

    let init = issuer.run(&["cluster", "init"]);
    assert_success(&init, "initialize issuer cluster");
    let enrollment = issuer.run(&[
        "cluster",
        "enrollment-token",
        "create",
        "--endpoint",
        "issuer",
    ]);
    assert_success(&enrollment, "create enrollment token");
    let enrollment_stdout = String::from_utf8(enrollment.stdout).expect("token output is UTF-8");
    let token = enrollment_stdout
        .lines()
        .find(|line| line.starts_with("bmux-enroll-v1:"))
        .expect("enrollment output contains token");

    let join = joiner.run(&[
        "cluster",
        "join",
        token,
        "--issuer",
        "issuer",
        "--endpoint",
        "joiner",
    ]);
    assert_success(&join, "join over TLS with mutual peer authentication");
    assert!(
        combined_output(&join).contains("joined cluster:"),
        "unexpected join output: {}",
        combined_output(&join)
    );

    let issuer_members = issuer.run(&["cluster", "members"]);
    assert_success(&issuer_members, "list issuer members");
    let joiner_members = joiner.run(&["cluster", "members"]);
    assert_success(&joiner_members, "list joiner members");
    assert_eq!(
        String::from_utf8_lossy(&issuer_members.stdout),
        String::from_utf8_lossy(&joiner_members.stdout),
        "both nodes should adopt the same public membership snapshot"
    );

    let leave = joiner.run(&["cluster", "leave"]);
    assert_success(&leave, "leave over TLS with mutual peer authentication");
    assert!(
        combined_output(&leave).contains("left cluster:"),
        "unexpected leave output: {}",
        combined_output(&leave)
    );
}
