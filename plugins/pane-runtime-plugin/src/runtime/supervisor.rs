//! Owner-private control client for the installed supervisor artifact.
use crate::supervisor_protocol::{self as protocol, Bootstrap, Operation, Request, Response};
use anyhow::Context as _;
use sha2::{Digest, Sha256};
use std::fmt::Write as FmtWrite;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;

static INSTALLATION: OnceLock<Mutex<Option<Installation>>> = OnceLock::new();
static SETTLED: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();

/// Retained locators not settled by this owner require explicit recovery.
pub(crate) fn idle_blocker() -> Option<String> {
    let installation = match INSTALLATION.get() {
        Some(installation) => match installation.lock() {
            Ok(installation) => installation,
            Err(_) => return Some("supervisor installation authority poisoned".into()),
        },
        None => return Some("supervisor installation authority unavailable".into()),
    };
    let Some(installation) = installation.as_ref() else {
        return Some("execution supervisor installation unavailable".into());
    };
    let Ok(settled) = SETTLED.get_or_init(Mutex::default).lock() else {
        return Some("supervisor settlement authority unavailable".into());
    };
    let entries = match std::fs::read_dir(&installation.root) {
        Ok(entries) => entries,
        Err(error) => return Some(format!("supervisor authority cannot be inspected: {error}")),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return Some(error.to_string()),
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(token) = name
            .strip_prefix("supervisor-")
            .and_then(|name| name.strip_suffix(".json"))
            && !settled.contains(token)
        {
            return Some("retained supervisor authority requires execution or recovery".into());
        }
    }
    None
}

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

// Every execution requires the server-owned lifetime backend. A direct PTY
// fallback cannot clean HUP-ignoring shell jobs after server death.
pub(super) fn select_backend(
    preparation: anyhow::Result<(SupervisorControl, portable_pty::CommandBuilder)>,
    _required: bool,
) -> anyhow::Result<Option<SupervisorControl>> {
    match preparation {
        Ok((control, _)) => Ok(Some(control)),
        Err(error) => Err(error),
    }
}

pub(super) struct SupervisorControl {
    _admission: crate::runtime::managed_idle::Admission,
    directory: PathBuf,
    token: String,
    authentication: String,
    version: u32,
    artifact: PathBuf,
    #[cfg(unix)]
    lifetime: Mutex<Lifetime>,
}
#[cfg(unix)]
#[derive(Default)]
struct Lifetime {
    revoked: bool,
    socket: Option<std::os::unix::net::UnixStream>,
}
#[cfg(unix)]
impl Lifetime {
    fn revoke(&mut self) {
        self.revoked = true;
        self.socket.take();
    }
    fn install(&mut self, socket: std::os::unix::net::UnixStream) -> anyhow::Result<()> {
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
#[cfg(unix)]
struct StartupChild(Option<std::process::Child>);
#[cfg(unix)]
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

#[cfg(unix)]
fn negotiate_guardian(
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
    /// End pane lifetime without depending on PTY hangup or a numeric PID.
    #[cfg(unix)]
    pub(super) fn end_lifetime(&self) -> anyhow::Result<()> {
        self.lifetime
            .lock()
            .map_err(|_| anyhow::anyhow!("supervisor lifetime lock poisoned"))?
            .revoke();
        Ok(())
    }
    pub(super) fn prepare(
        token: uuid::Uuid,
    ) -> anyhow::Result<(Self, portable_pty::CommandBuilder)> {
        #[cfg(test)]
        install_test_artifact()?;
        super::reservations::check_authority()?;
        let admission = super::managed_idle::admit()?;
        crate::readiness::check_admission()?;
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
        for byte in Sha256::digest(std::fs::read(&installation.path).with_context(|| {
            format!(
                "cannot read retained execution supervisor {}",
                installation.path.display()
            )
        })?) {
            write!(&mut digest, "{byte:02x}")?;
        }
        anyhow::ensure!(
            digest == installation.hash,
            "supervisor helper artifact hash mismatch: {}",
            installation.path.display()
        );
        // Match the existing resurrection host's short, owner-private endpoint
        // mechanism. Persist the generated locator; never reconstruct by hashing.
        let endpoint_directory = short_endpoint_directory()
            .context("cannot create supervisor endpoint directory in /tmp")?;
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
        let mut file = options
            .open(&path)
            .with_context(|| format!("cannot create supervisor bootstrap {}", path.display()))?;
        file.write_all(&serde_json::to_vec(&bootstrap)?)
            .with_context(|| format!("cannot write supervisor bootstrap {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot sync supervisor bootstrap {}", path.display()))?;
        std::fs::File::open(&directory)?.sync_all()?;
        let locator = installation.root.join(format!("supervisor-{token}.json"));
        crate::atomic_state::commit(
            &locator,
            &serde_json::json!({
                "version": version, "execution_token": token,
                "endpoint": directory.join("control.sock"), "bootstrap": path,
                "artifact_sha256": installation.hash,
            }),
        )
        .with_context(|| format!("cannot commit supervisor locator {}", locator.display()))?;
        // Keep the locator until explicit retirement. The execution itself is
        // bounded by the server-owned lifetime socket, not this directory.
        let _retained_directory = endpoint_directory.keep();
        let mut command = portable_pty::CommandBuilder::new(&installation.path);
        command.arg("--guardian-terminal-session-v4");
        command.arg(path);
        command.arg(directory.join("control.sock"));
        Ok((
            Self {
                _admission: admission,
                directory,
                token: token.to_string(),
                authentication,
                version,
                artifact: installation.path.clone(),
                #[cfg(unix)]
                lifetime: Mutex::new(Lifetime::default()),
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
        if response.terminal_session_ended == Some(true)
            && response.cleanup_error.is_none()
            && response.capture_error.is_none()
            && let Ok(mut settled) = SETTLED.get_or_init(Mutex::default).lock()
        {
            settled.insert(self.token.clone());
        }
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
        environment: Option<
            Vec<bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry>,
        >,
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
            native_env: environment.map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| (entry.key, entry.value))
                    .collect()
            }),
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
