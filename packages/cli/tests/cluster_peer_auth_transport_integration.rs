use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
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
    gateway_child: Option<std::process::Child>,
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
            gateway_child: None,
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

    fn start_iroh_gateway(&mut self) -> String {
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
            .recv_timeout(Duration::from_secs(10))
            .expect("direct iroh gateway publishes connect URL");
        self.gateway_child = Some(child);
        url
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
        if let Some(child) = self.gateway_child.as_mut() {
            let _ = child.kill();
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

fn create_enrollment_token(issuer: &ServerEnv, endpoint: &str) -> String {
    let init = issuer.run(&["cluster", "init"]);
    assert_success(&init, "initialize issuer cluster");
    let enrollment = issuer.run(&[
        "cluster",
        "enrollment-token",
        "create",
        "--endpoint",
        endpoint,
    ]);
    assert_success(&enrollment, "create enrollment token");
    String::from_utf8(enrollment.stdout)
        .expect("token output is UTF-8")
        .lines()
        .find(|line| line.starts_with("bmux-enroll-v1:"))
        .expect("enrollment output contains token")
        .to_string()
}

fn assert_join_and_leave(issuer: &ServerEnv, joiner: &ServerEnv, token: &str, target: &str) {
    let join = joiner.run(&[
        "cluster",
        "join",
        token,
        "--issuer",
        target,
        "--endpoint",
        "joiner",
    ]);
    assert_success(&join, "join with mutual peer authentication");
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

    let status = joiner.run(&["cluster", "status"]);
    assert_success(
        &status,
        "report membership status without liveness mutation",
    );
    let status_output = combined_output(&status);
    assert!(status_output.contains("cluster status identity"));
    assert!(status_output.contains("liveness=Unchecked"));
    let members_after_status = joiner.run(&["cluster", "members"]);
    assert_success(&members_after_status, "list members after status");
    assert_eq!(joiner_members.stdout, members_after_status.stdout);

    let doctor = joiner.run(&["cluster", "doctor"]);
    assert_success(&doctor, "probe membership trust and reachability");
    let doctor_output = combined_output(&doctor);
    assert!(doctor_output.contains("cluster doctor identity"));
    assert!(doctor_output.contains("liveness=Reachable"));
    assert!(doctor_output.contains("compatible=true"));
    assert!(doctor_output.contains("trusted=true"));
    let members_after_doctor = joiner.run(&["cluster", "members"]);
    assert_success(&members_after_doctor, "list members after doctor");
    assert_eq!(joiner_members.stdout, members_after_doctor.stdout);

    let leave = joiner.run(&["cluster", "leave"]);
    assert_success(&leave, "leave with mutual peer authentication");
    assert!(
        combined_output(&leave).contains("left cluster:"),
        "unexpected leave output: {}",
        combined_output(&leave)
    );
}

struct ProcessGuard {
    child: Child,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn generate_ed25519_key(path: &Path) {
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .output()
        .expect("run ssh-keygen");
    assert_success(&output, "generate SSH key");
}

#[test]
fn ssh_join_and_leave_require_real_mutual_peer_authentication() {
    if !Path::new("/usr/sbin/sshd").exists() {
        eprintln!("skipping SSH transport integration: /usr/sbin/sshd unavailable");
        return;
    }
    let port = reserve_tcp_port();
    let mut issuer = ServerEnv::new("ssh-issuer");
    issuer.write_config("");
    let mut joiner = ServerEnv::new("ssh-joiner");
    let ssh_root = joiner._root.path().join("sshd");
    std::fs::create_dir_all(&ssh_root).expect("create SSH test root");
    let host_key = ssh_root.join("host-key");
    let client_key = ssh_root.join("client-key");
    let authorized_keys = ssh_root.join("authorized_keys");
    let known_hosts = ssh_root.join("known_hosts");
    let remote_bmux = ssh_root.join("bmux-issuer");
    generate_ed25519_key(&host_key);
    generate_ed25519_key(&client_key);
    std::fs::copy(client_key.with_extension("pub"), &authorized_keys)
        .expect("write SSH authorized_keys");
    let mut permissions = std::fs::metadata(&authorized_keys)
        .expect("read authorized_keys metadata")
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o600);
        std::fs::set_permissions(&authorized_keys, permissions)
            .expect("set authorized_keys permissions");
    }
    let sshd_config = ssh_root.join("sshd_config");
    std::fs::write(
        &sshd_config,
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nChallengeResponseAuthentication no\nUsePAM no\nPermitRootLogin no\nStrictModes no\nLogLevel ERROR\n",
            host_key.display(),
            ssh_root.join("sshd.pid").display(),
            authorized_keys.display(),
        ),
    )
    .expect("write sshd config");
    let sshd = Command::new("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(&sshd_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn isolated sshd");
    let _sshd = ProcessGuard { child: sshd };
    thread::sleep(Duration::from_millis(250));

    std::fs::write(
        &remote_bmux,
        format!(
            "#!/bin/sh\nBMUX_RUNTIME_DIR='{}' BMUX_CONFIG_DIR='{}' BMUX_DATA_DIR='{}' BMUX_STATE_DIR='{}' BMUX_LOG_DIR='{}' exec '{}' \"$@\"\n",
            issuer.runtime_dir.display(),
            issuer.config_dir.display(),
            issuer.data_dir.display(),
            issuer.state_dir.display(),
            issuer.log_dir.display(),
            bmux_binary().display(),
        ),
    )
    .expect("write remote bmux wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut wrapper_permissions = std::fs::metadata(&remote_bmux)
            .expect("read remote wrapper metadata")
            .permissions();
        wrapper_permissions.set_mode(0o700);
        std::fs::set_permissions(&remote_bmux, wrapper_permissions)
            .expect("set remote wrapper permissions");
    }
    joiner.write_config(&format!(
        "[connections.targets.issuer]\ntransport = \"ssh\"\nhost = \"127.0.0.1\"\nuser = \"{}\"\nport = {port}\nidentity_file = \"{}\"\nknown_hosts_file = \"{}\"\nstrict_host_key_checking = false\nremote_bmux_path = \"{}\"\nserver_start_mode = \"require_running\"\n",
        std::env::var("USER").expect("USER is set"),
        client_key.display(),
        known_hosts.display(),
        remote_bmux.display(),
    ));
    issuer.start();
    joiner.start();

    let token = create_enrollment_token(&issuer, "issuer");
    assert_join_and_leave(&issuer, &joiner, &token, "issuer");
}

#[test]
#[ignore = "blocked on macOS iroh/netwatch CoreWLAN endpoint-bind stall"]
fn iroh_join_and_leave_require_real_mutual_peer_authentication() {
    let mut issuer = ServerEnv::new("iroh-issuer");
    let issuer_url = issuer.start_iroh_gateway();
    let mut joiner = ServerEnv::new("iroh-joiner");
    joiner.write_config(&format!(
        "[connections.targets.issuer]\ntransport = \"iroh\"\nendpoint_id = \"{}\"\niroh_ip_addr = \"{}\"\nserver_start_mode = \"require_running\"\n",
        issuer_url
            .strip_prefix("iroh://")
            .expect("iroh URL scheme")
            .split(['?', '/'])
            .next()
            .expect("iroh endpoint ID"),
        issuer_url
            .split_once("addr=")
            .map(|(_, addr)| addr.split('&').next().unwrap_or(addr))
            .expect("direct iroh URL contains socket address")
    ));
    joiner.start();

    let token = create_enrollment_token(&issuer, "issuer");
    assert_join_and_leave(&issuer, &joiner, &token, "issuer");
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

    let token = create_enrollment_token(&issuer, "issuer");
    assert_join_and_leave(&issuer, &joiner, &token, "issuer");
}
