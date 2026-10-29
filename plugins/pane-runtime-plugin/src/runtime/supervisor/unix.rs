use super::{SupervisorControl, protocol};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(super) const HELPER_ARGUMENT: &str = "--guardian-terminal-session-v4";
pub(super) const NATIVE_ENV_ENCODING: &str = "unix-bytes";
pub(super) type Stream = std::os::unix::net::UnixStream;
pub(super) fn short_endpoint_directory() -> anyhow::Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("bmux-exec-")
        .tempdir_in("/tmp")?;
    anyhow::ensure!(
        directory.path().join("control.sock").as_os_str().len() < 104,
        "supervisor control endpoint exceeds platform Unix socket path limit"
    );
    Ok(directory)
}

pub(super) fn endpoint(directory: &Path) -> PathBuf {
    directory.join("control.sock")
}
pub(super) fn connect(endpoint: &Path, timeout: std::time::Duration) -> std::io::Result<Stream> {
    let socket = Stream::connect(endpoint)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    Ok(socket)
}
pub(super) fn secure_bootstrap(
    directory: &Path,
    bootstrap: &protocol::Bootstrap,
) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("bootstrap.json"))?;
    file.write_all(&serde_json::to_vec(bootstrap)?)?;
    file.sync_all()?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}
pub(super) struct Interrupter(Stream);
impl Interrupter {
    pub(super) fn new(stream: &Stream) -> std::io::Result<Self> {
        Ok(Self(stream.try_clone()?))
    }
    pub(super) fn interrupt(&self) -> std::io::Result<()> {
        self.0.shutdown(std::net::Shutdown::Both)
    }
}
#[derive(Default)]
pub(super) struct Lifetime {
    pub(super) revoked: bool,
    pub(super) socket: Option<std::os::unix::net::UnixStream>,
}
impl Lifetime {
    pub(super) fn revoke(&mut self) {
        self.revoked = true;
        self.socket.take();
    }
    pub(super) fn install(&mut self, socket: std::os::unix::net::UnixStream) -> anyhow::Result<()> {
        anyhow::ensure!(!self.revoked, "execution lifetime already revoked");
        anyhow::ensure!(
            self.socket.is_none(),
            "execution lifetime already installed"
        );
        self.socket = Some(socket);
        Ok(())
    }
}

/// Own the unreaped child throughout readiness negotiation. Drop never waits
/// indefinitely on a wedged kernel process; a reaper retains Child ownership.
pub(super) struct StartupChild(pub(super) Option<std::process::Child>);
impl Drop for StartupChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    _ => break,
                }
            }
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

pub(super) fn negotiate_guardian(
    mut child: StartupChild,
    mut readiness: std::os::unix::net::UnixStream,
    lifetime: &Mutex<Lifetime>,
) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
    let version: u32 = protocol::read_frame(&mut readiness)?;
    anyhow::ensure!(
        version == protocol::VERSION,
        "guardian startup negotiation rejected"
    );
    // Install and revocation share one lock. Prior revocation is sticky even
    // when readiness arrived after pane close; rejected sockets close here.
    lifetime
        .lock()
        .map_err(|_| anyhow::anyhow!("supervisor lifetime lock poisoned"))?
        .install(readiness)?;
    Ok(Box::new(child.0.take().expect("negotiated guardian child")))
}

impl SupervisorControl {
    pub(in crate::runtime) fn spawn_terminal(
        &self,
        master: &dyn portable_pty::MasterPty,
        _slave: &dyn portable_pty::SlavePty,
        _command: portable_pty::CommandBuilder,
    ) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
        let slave = master
            .tty_name()
            .ok_or_else(|| anyhow::anyhow!("PTY slave path unavailable"))?;
        self.spawn_guardian(&slave)
    }

    pub(in crate::runtime) fn spawn_guardian(
        &self,
        slave: &std::path::Path,
    ) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        anyhow::ensure!(
            self.version == protocol::VERSION,
            "guardian requires explicit terminal-session protocol"
        );
        let (readiness, ready_child) = std::os::unix::net::UnixStream::pair()?;
        readiness.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        let ready_fd = ready_child.as_raw_fd();
        let mut command = std::process::Command::new(&self.artifact);
        command
            .arg("--guardian-terminal-session-v4")
            .arg(self.directory.join("bootstrap.json"))
            .arg(self.directory.join("control.sock"))
            .arg(slave)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: only async-signal-safe descriptor operations before exec.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(ready_fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = StartupChild(Some(command.spawn()?));
        drop(command);
        drop(ready_child);
        negotiate_guardian(child, readiness, &self.lifetime)
    }
}
