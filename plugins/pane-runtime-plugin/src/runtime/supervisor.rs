//! Owner-private control client for the installed supervisor artifact.
use crate::supervisor_protocol::{self as protocol, Bootstrap, Operation, Request, Response};
use sha2::{Digest, Sha256};
use std::fmt::Write as FmtWrite;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;

static INSTALLATION: OnceLock<Mutex<Option<Installation>>> = OnceLock::new();
struct Installation {
    path: PathBuf,
    hash: String,
    root: PathBuf,
}

fn short_endpoint_directory() -> anyhow::Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("bmux-exec-")
        .tempdir_in("/tmp")?;
    anyhow::ensure!(
        directory.path().join("control.sock").as_os_str().len() < 104,
        "supervisor control endpoint exceeds platform Unix socket path limit"
    );
    Ok(directory)
}

pub(crate) fn activate(context: &bmux_plugin_sdk::NativeLifecycleContext) {
    let resolved = context.auxiliary_executables.get("execution-supervisor");
    let installation = resolved
        .filter(|artifact| artifact.protocol_version == protocol::VERSION)
        .map(|artifact| Installation {
            path: PathBuf::from(&artifact.path),
            hash: artifact.artifact_sha256.clone(),
            root: PathBuf::from(&context.connection.runtime_dir),
        });
    *INSTALLATION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("supervisor installation lock") = installation;
}

#[cfg(test)]
pub(super) fn install_test_artifact() -> anyhow::Result<()> {
    static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
    static ARTIFACT: OnceLock<(PathBuf, String)> = OnceLock::new();
    static FIXTURE_LOCK: Mutex<()> = Mutex::new(());
    let _fixture_guard = FIXTURE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("fixture lock poisoned"))?;
    if ARTIFACT.get().is_none() {
        // Cargo builds this binary because tests/execution_helper_artifact.rs
        // references CARGO_BIN_EXE. Resolve only our test profile, never PATH.
        let executable = std::env::current_exe()?;
        let profile = executable
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| anyhow::anyhow!("Cargo test profile directory unavailable"))?;
        let source = profile.join("bmux-pane-execution-supervisor");
        let bytes = std::fs::read(&source)?;
        let root = ROOT.get_or_init(|| tempfile::tempdir().expect("owned helper fixture root"));
        let path = root.path().join("execution-supervisor");
        // Per-process immutable snapshot protects tests from later builds.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let mut hash = String::with_capacity(64);
        for byte in Sha256::digest(std::fs::read(&path)?) {
            write!(&mut hash, "{byte:02x}")?;
        }
        let _ = ARTIFACT.set((path, hash));
    }
    let (path, hash) = ARTIFACT.get().expect("fixture snapshot initialized");
    *INSTALLATION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("supervisor installation lock poisoned"))? =
        Some(Installation {
            path: path.clone(),
            hash: hash.clone(),
            root: ROOT
                .get()
                .expect("test artifact root initialized")
                .path()
                .to_path_buf(),
        });
    Ok(())
}

pub(super) struct SupervisorControl {
    directory: PathBuf,
    token: String,
    authentication: String,
    version: u32,
    artifact: PathBuf,
}
impl SupervisorControl {
    pub(super) fn prepare(
        token: uuid::Uuid,
    ) -> anyhow::Result<(Self, portable_pty::CommandBuilder)> {
        #[cfg(test)]
        install_test_artifact()?;
        let version = protocol::VERSION;
        let installation = INSTALLATION
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| anyhow::anyhow!("supervisor installation lock poisoned"))?;
        let installation = installation.as_ref().ok_or_else(|| {
            anyhow::anyhow!("execution supervisor helper artifact is unavailable")
        })?;
        anyhow::ensure!(
            installation.path.is_absolute(),
            "supervisor helper path must be absolute"
        );
        let mut digest = String::with_capacity(64);
        for byte in Sha256::digest(std::fs::read(&installation.path)?) {
            write!(&mut digest, "{byte:02x}")?;
        }
        anyhow::ensure!(
            digest == installation.hash,
            "supervisor helper artifact hash mismatch"
        );
        // Match the existing resurrection host's short, owner-private endpoint
        // mechanism. Persist the generated locator; never reconstruct by hashing.
        let endpoint_directory = short_endpoint_directory()?;
        let directory = endpoint_directory.path().to_path_buf();
        anyhow::ensure!(
            directory.join("control.sock").as_os_str().len() < 104,
            "supervisor control endpoint exceeds platform Unix socket path limit"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let authentication = uuid::Uuid::new_v4().to_string();
        let bootstrap = Bootstrap {
            version,
            execution_token: token.to_string(),
            authentication: authentication.clone(),
        };
        let path = directory.join("bootstrap.json");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(&serde_json::to_vec(&bootstrap)?)?;
        file.sync_all()?;
        std::fs::File::open(&directory)?.sync_all()?;
        let locator = installation.root.join(format!("supervisor-{token}.json"));
        crate::atomic_state::commit(
            &locator,
            &serde_json::json!({
                "version": version, "execution_token": token,
                "endpoint": directory.join("control.sock"), "bootstrap": path,
                "artifact_sha256": installation.hash,
            }),
        )?;
        // Execution-owned endpoint survives client/daemon disconnect; explicit
        // retirement, not this stack frame, owns eventual removal.
        let _retained_directory = endpoint_directory.keep();
        let mut command = portable_pty::CommandBuilder::new(&installation.path);
        command.arg("--guardian-terminal-session-v4");
        command.arg(path);
        command.arg(directory.join("control.sock"));
        Ok((
            Self {
                directory,
                token: token.to_string(),
                authentication,
                version,
                artifact: installation.path.clone(),
            },
            command,
        ))
    }

    pub(super) fn spawn_guardian(
        &self,
        slave: &std::path::Path,
    ) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        anyhow::ensure!(
            self.version == protocol::VERSION,
            "guardian requires explicit terminal-session protocol"
        );
        let (mut readiness, ready_child) = std::os::unix::net::UnixStream::pair()?;
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
        let child = command.spawn()?;
        drop(command);
        drop(ready_child);
        let version: u32 = protocol::read_frame(&mut readiness)?;
        anyhow::ensure!(
            version == protocol::VERSION,
            "guardian startup negotiation rejected"
        );
        Ok(Box::new(child))
    }

    #[cfg(unix)]
    pub(super) fn request(&self, operation: Operation) -> anyhow::Result<Response> {
        let mut socket =
            std::os::unix::net::UnixStream::connect(self.directory.join("control.sock"))?;
        let timeout = if matches!(&operation, Operation::Hello | Operation::Status) {
            std::time::Duration::from_millis(500)
        } else {
            std::time::Duration::from_secs(5)
        };
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        protocol::write_frame(
            &mut socket,
            &Request {
                version: self.version,
                execution_token: self.token.clone(),
                authentication: self.authentication.clone(),
                operation,
            },
        )?;
        let response: Response = protocol::read_frame(&mut socket)?;
        anyhow::ensure!(
            response.version == self.version && response.execution_token == self.token,
            "supervisor response identity mismatch"
        );
        anyhow::ensure!(
            response.error.is_none(),
            "supervisor operation failed: {:?}",
            response.error
        );
        Ok(response)
    }

    #[cfg(not(unix))]
    pub(super) fn request(&self, _operation: Operation) -> anyhow::Result<Response> {
        anyhow::bail!("supervisor requires Unix")
    }

    /// Blocking startup handshake; callers on an async runtime must dispatch this
    /// with `spawn_blocking`, owning the control, launch, and authorization handle.
    pub(super) fn start(
        &self,
        launch: &super::PaneLaunchSpec,
        authorize: impl Fn() -> anyhow::Result<()>,
    ) -> anyhow::Result<Option<u32>> {
        // Startup readiness was acknowledged over inherited IPC in spawn_guardian.
        let mut watcher = self.subscribe()?;
        authorize()?;
        self.request(Operation::Start {
            mutation_id: self.token.clone(),
            program: launch.program.clone(),
            args: launch.args.clone(),
            cwd: launch.cwd.clone(),
            env: launch.env.clone(),
        })?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut snapshot = watcher.initial().clone();
        loop {
            if let Some(error) = snapshot.status.error {
                anyhow::bail!(error);
            }
            if snapshot.status.target_pid.is_some()
                || snapshot.status.terminal_session_ended == Some(true)
            {
                return Ok(snapshot.status.target_pid);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "target readiness deadline exceeded; launch must not be repeated"
            );
            watcher.socket.set_read_timeout(Some(remaining))?;
            snapshot = watcher.next()?;
        }
    }

    #[cfg(unix)]
    pub(super) fn subscribe(&self) -> anyhow::Result<LifecycleWatcher> {
        let mut socket =
            std::os::unix::net::UnixStream::connect(self.directory.join("control.sock"))?;
        socket.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
        socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        protocol::write_frame(
            &mut socket,
            &Request {
                version: self.version,
                execution_token: self.token.clone(),
                authentication: self.authentication.clone(),
                operation: Operation::Subscribe {
                    capability: protocol::LIFECYCLE_CAPABILITY,
                },
            },
        )?;
        let initial: protocol::LifecycleSnapshot = protocol::read_frame(&mut socket)?;
        validate_snapshot(&initial, &self.token, 0)?;
        socket.set_read_timeout(None)?;
        Ok(LifecycleWatcher {
            socket,
            initial: initial.clone(),
            current: initial,
        })
    }
}

fn validate_snapshot(
    snapshot: &protocol::LifecycleSnapshot,
    token: &str,
    previous: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        snapshot.capability == protocol::LIFECYCLE_CAPABILITY
            && snapshot.status.version == protocol::VERSION
            && snapshot.status.execution_token == token
            && snapshot.revision > previous,
        "invalid or stale supervisor lifecycle snapshot"
    );
    Ok(())
}

#[cfg(unix)]
pub(super) struct LifecycleWatcher {
    socket: std::os::unix::net::UnixStream,
    initial: protocol::LifecycleSnapshot,
    current: protocol::LifecycleSnapshot,
}
#[cfg(unix)]
impl LifecycleWatcher {
    pub(super) fn initial(&self) -> &protocol::LifecycleSnapshot {
        &self.initial
    }
    pub(super) fn interrupter(&self) -> anyhow::Result<std::os::unix::net::UnixStream> {
        Ok(self.socket.try_clone()?)
    }
    pub(super) fn next(&mut self) -> anyhow::Result<protocol::LifecycleSnapshot> {
        let snapshot: protocol::LifecycleSnapshot = protocol::read_frame(&mut self.socket)?;
        validate_snapshot(
            &snapshot,
            &self.current.status.execution_token,
            self.current.revision,
        )?;
        self.current = snapshot.clone();
        Ok(snapshot)
    }
}
