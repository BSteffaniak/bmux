//! Conventional terminal-session owner, independent authenticated guardian.
use crate::protocol::{self, Bootstrap, Operation, Request, Response};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(serde::Serialize, serde::Deserialize)]
struct OwnerOutcome {
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    drain_error: Option<String>,
    #[serde(default)]
    cleanup_requested: bool,
}

static REQUESTED_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
extern "C" fn request_term(_: libc::c_int) {
    REQUESTED_SIGNAL.store(libc::SIGTERM, std::sync::atomic::Ordering::SeqCst);
}
extern "C" fn request_kill(_: libc::c_int) {
    REQUESTED_SIGNAL.store(libc::SIGKILL, std::sync::atomic::Ordering::SeqCst);
}

fn redirect_stdio() -> io::Result<()> {
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    for fd in [0, 1, 2] {
        // SAFETY: replaces only this helper's owned standard descriptors.
        if unsafe { libc::dup2(null.as_raw_fd(), fd) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn write_outcome(path: &std::path::Path, outcome: &OwnerOutcome) -> io::Result<()> {
    crate::atomic_state::commit(path, outcome)
}

/// A fresh executable owns the controlling session, never the control guardian.
pub(super) fn owner(launch_path: &str, outcome_path: &str) -> io::Result<()> {
    let operation: Operation =
        serde_json::from_slice(&std::fs::read(launch_path)?).map_err(io::Error::other)?;
    let Operation::Start {
        program,
        args,
        cwd,
        env,
        ..
    } = operation
    else {
        return Err(io::Error::other("terminal owner requires start operation"));
    };
    // SAFETY: signal disposition belongs to this fresh helper only. Target resets it.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let mut command = Command::new(program);
    command.args(args).env_clear().envs(env);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // SAFETY: process-group/foreground ownership and signal disposition are
    // established in this fresh direct target before exec; guardian has no ctty.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) < 0 || libc::tcsetpgrp(0, libc::getpid()) < 0 {
                return Err(io::Error::last_os_error());
            }
            for signal in [
                libc::SIGHUP,
                libc::SIGTERM,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGPIPE,
            ] {
                libc::signal(signal, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            write_outcome(
                std::path::Path::new(outcome_path),
                &OwnerOutcome {
                    exit_code: None,
                    exit_signal: None,
                    drain_error: Some(format!("terminal target spawn failed: {error}")),
                    cleanup_requested: false,
                },
            )?;
            return Err(error);
        }
    };
    // Dropping the prepared command releases any retained spawn-side state;
    // only the product child and this terminal owner keep terminal descriptors.
    drop(command);
    // Signal handlers only store a request. The owner retains/reaps its actual
    // target Child and durably publishes its result before terminal retirement.
    unsafe {
        libc::signal(libc::SIGUSR1, request_term as *const () as usize);
        libc::signal(libc::SIGUSR2, request_kill as *const () as usize);
    }
    crate::atomic_state::commit(
        &std::path::Path::new(outcome_path).with_extension("ready"),
        &child.id(),
    )?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let signal = REQUESTED_SIGNAL.load(std::sync::atomic::Ordering::SeqCst);
        if signal != 0 {
            // Kill only the actual unreaped direct target. Never kill the owner
            // before it durably records target outcome; descendant cleanup is
            // explicitly not claimed by this target termination receipt.
            if signal == libc::SIGKILL {
                child.kill()?;
            } else if unsafe {
                libc::kill(i32::try_from(child.id()).map_err(io::Error::other)?, signal)
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut outcome = OwnerOutcome {
        exit_code: status.code(),
        exit_signal: status.signal(),
        drain_error: None,
        cleanup_requested: REQUESTED_SIGNAL.load(std::sync::atomic::Ordering::SeqCst)
            == libc::SIGKILL,
    };
    write_outcome(std::path::Path::new(outcome_path), &outcome)?;
    // Target outcome is durable before drain/retirement. Later final publication
    // records whether the terminal drain was acknowledged.
    // Target exit establishes the new contract's terminal-session end boundary.
    // Drain bytes already queued at that boundary before allowing the OS revoke.
    // An alarm interrupts stalled drain, but MUST NOT retire the terminal owner.
    unsafe extern "C" fn interrupt_drain(_: libc::c_int) {}
    // SAFETY: local helper installs a non-restarting handler and bounds its own ioctl.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = interrupt_drain as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGALRM, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::alarm(5);
        if libc::tcdrain(1) != 0 {
            outcome.drain_error = Some(format!(
                "terminal-session drain not acknowledged: {}",
                io::Error::last_os_error()
            ));
        }
        libc::alarm(0);
    }
    write_outcome(std::path::Path::new(outcome_path), &outcome)?;
    if outcome.drain_error.is_some() {
        // Explicit recovery state: retain ctty authority, never manufacture EOF.
        redirect_stdio()?;
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    Ok(())
}

struct Guardian {
    bootstrap: Bootstrap,
    root: std::path::PathBuf,
    slave: String,
    owner: Option<Child>,
    start: Option<Vec<u8>>,
    consumed: bool,
    signal_failure: Option<String>,
}

impl Guardian {
    fn owner_exited(&self) -> io::Result<bool> {
        let Some(owner) = &self.owner else {
            return Ok(false);
        };
        // WNOWAIT keeps the direct child identity pinned until explicit retirement.
        // SAFETY: waitid inspects only our child, buffer matches siginfo_t.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::waitid(
                libc::P_PID,
                owner.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(info.si_pid() != 0)
        }
    }

    fn status(&self) -> io::Result<Response> {
        let path = self.root.join("terminal-owner-outcome.json");
        let outcome = match std::fs::read(&path) {
            Ok(bytes) => {
                Some(serde_json::from_slice::<OwnerOutcome>(&bytes).map_err(io::Error::other)?)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let owner_exited = if self.consumed || (self.start.is_some() && self.owner.is_none()) {
            true
        } else {
            self.owner_exited()?
        };
        let error = outcome
            .as_ref()
            .and_then(|o| o.drain_error.clone())
            .or_else(|| {
                (owner_exited && outcome.is_none()).then(|| {
                    "terminal owner exited without durable outcome; capture incomplete".into()
                })
            });
        let target_pid = match std::fs::read(self.root.join("terminal-owner-outcome.ready")) {
            Ok(bytes) => Some(serde_json::from_slice::<u32>(&bytes).map_err(io::Error::other)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Response {
            version: protocol::VERSION,
            execution_token: self.bootstrap.execution_token.clone(),
            started: self.start.is_some(),
            target_pid,
            exit_code: outcome.as_ref().and_then(|o| o.exit_code),
            exit_signal: outcome.as_ref().and_then(|o| o.exit_signal),
            group_consumed: false,
            target_terminated: self.consumed,
            terminal_session_ended: Some(owner_exited),
            cleanup_error: self.signal_failure.clone(),
            error,
        })
    }

    fn signal(&mut self, signal: i32) -> io::Result<()> {
        if let Some(error) = &self.signal_failure {
            return Err(io::Error::other(error.clone()));
        }
        if self.consumed {
            return Ok(());
        }
        let owner = self
            .owner
            .as_ref()
            .ok_or_else(|| io::Error::other("terminal owner unavailable"))?;
        let pid = i32::try_from(owner.id()).map_err(io::Error::other)?;
        let ready = self.root.join("terminal-owner-outcome.ready");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() && !self.owner_exited()? {
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "terminal owner signal readiness unavailable",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if self.owner_exited()? {
            // A racing natural/TERM exit is already a terminal target outcome,
            // not a descendant sweep. Require the durable outcome before ack.
            let outcome: OwnerOutcome = serde_json::from_slice(&std::fs::read(
                self.root.join("terminal-owner-outcome.json"),
            )?)
            .map_err(io::Error::other)?;
            if let Some(error) = outcome.drain_error {
                return Err(io::Error::other(error));
            }
            self.consumed = true;
            return Ok(());
        }
        let request_signal = if signal == libc::SIGKILL {
            libc::SIGUSR2
        } else {
            libc::SIGUSR1
        };
        // Unreaped direct owner pins identity; it owns and reaps the target.
        if unsafe { libc::kill(pid, request_signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if signal == libc::SIGKILL {
            while !self.owner_exited()? {
                if Instant::now() >= deadline {
                    return Err(io::Error::other(
                        "target outcome/terminal end not acknowledged",
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let outcome: OwnerOutcome = serde_json::from_slice(&std::fs::read(
                self.root.join("terminal-owner-outcome.json"),
            )?)
            .map_err(io::Error::other)?;
            // The target may naturally exit after the request but before the
            // owner observes it. Its durable outcome is authoritative either way.
            if let Some(error) = outcome.drain_error {
                return Err(io::Error::other(error));
            }
            // This acknowledges the ended target only, not escaped descendants.
            self.consumed = true;
        }
        Ok(())
    }

    fn apply(&mut self, operation: &Operation) -> io::Result<Response> {
        match operation {
            Operation::Hello | Operation::Status => {}
            Operation::Start { mutation_id, .. } => {
                if mutation_id.is_empty() {
                    return Err(io::Error::other("launch mutation identity required"));
                }
                let encoded = serde_json::to_vec(operation).map_err(io::Error::other)?;
                if let Some(previous) = &self.start {
                    if previous != &encoded {
                        return Err(io::Error::other("conflicting launch reservation"));
                    }
                } else {
                    let launch = self.root.join("terminal-owner-launch.json");
                    crate::atomic_state::commit(&launch, operation)?;
                    use std::os::unix::fs::OpenOptionsExt;
                    let slave = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_NOCTTY)
                        .open(&self.slave)?;
                    let mut command = Command::new(std::env::current_exe()?);
                    command
                        .arg("--terminal-owner")
                        .arg(&launch)
                        .arg(self.root.join("terminal-owner-outcome.json"))
                        .stdin(Stdio::from(slave.try_clone()?))
                        .stdout(Stdio::from(slave.try_clone()?))
                        .stderr(Stdio::from(slave));
                    // SAFETY: fresh child uses only async-signal-safe session/ioctl setup.
                    unsafe {
                        command.pre_exec(|| {
                            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                                return Err(io::Error::last_os_error());
                            }
                            if libc::tcsetpgrp(0, libc::getpid()) < 0 {
                                return Err(io::Error::last_os_error());
                            }
                            Ok(())
                        });
                    }
                    self.owner = Some(command.spawn()?);
                    // Drop Command too: stdio clones otherwise keep the slave alive.
                    drop(command);
                    self.start = Some(encoded);
                }
            }
            Operation::Term { .. } => self.signal(libc::SIGTERM)?,
            Operation::Sweep { .. } => self.signal(libc::SIGKILL)?,
            Operation::RetireOwner { .. } => {
                if !self.owner_exited()? {
                    return Err(io::Error::other(
                        "terminal session has not ended; owner retirement refused",
                    ));
                }
                self.owner
                    .as_mut()
                    .ok_or_else(|| io::Error::other("terminal owner missing"))?
                    .wait()?;
                self.owner = None;
                // No claim that escaped descendants were terminated or swept.
            }
        }
        self.status()
    }

    fn operate(&mut self, request: Request) -> io::Result<Response> {
        if request.version != protocol::VERSION
            || request.execution_token != self.bootstrap.execution_token
            || request.authentication != self.bootstrap.authentication
        {
            return Err(io::Error::other("supervisor handshake rejected"));
        }
        let mutation = match &request.operation {
            Operation::Start { mutation_id, .. }
            | Operation::Term { mutation_id }
            | Operation::Sweep { mutation_id }
            | Operation::RetireOwner { mutation_id } => Some(mutation_id),
            Operation::Hello | Operation::Status => None,
        };
        let fingerprint = serde_json::to_vec(&request.operation).map_err(io::Error::other)?;
        let admitted = if let Some(mutation) = mutation {
            let (path, previous) =
                crate::receipts::admit(&self.root, mutation, fingerprint.clone())?;
            if let Some(previous) = previous {
                return serde_json::from_slice(&previous).map_err(io::Error::other);
            }
            Some(path)
        } else {
            None
        };
        let response = self.apply(&request.operation)?;
        if let Some(path) = admitted {
            crate::receipts::complete(
                &path,
                fingerprint,
                serde_json::to_vec(&response).map_err(io::Error::other)?,
            )?;
        }
        Ok(response)
    }
}

pub(super) fn run(bootstrap_path: &str, socket_path: &str, slave: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let path = std::path::Path::new(bootstrap_path);
    let metadata = std::fs::symlink_metadata(path)?;
    let root = path
        .parent()
        .ok_or_else(|| io::Error::other("private root unavailable"))?;
    let directory = std::fs::symlink_metadata(root)?;
    // SAFETY: reading our own effective identity has no side effect.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
        || !directory.is_dir()
        || directory.uid() != uid
        || directory.permissions().mode() & 0o077 != 0
        || metadata.len() > u64::try_from(protocol::MAX_FRAME).map_err(io::Error::other)?
    {
        return Err(io::Error::other(
            "guardian bootstrap must be bounded owner-private non-symlink objects",
        ));
    }
    let bootstrap: Bootstrap =
        serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
    if bootstrap.version != protocol::VERSION {
        return Err(io::Error::other("unsupported supervisor version"));
    }
    // This role is launched with null stdio, never through a PTY spawn adapter.
    // Detach any inherited controlling session before preparing the owner.
    // SAFETY: fresh guardian changes only its own session membership.
    unsafe {
        if libc::isatty(0) != 0 || libc::isatty(1) != 0 || libc::isatty(2) != 0 {
            return Err(io::Error::other(
                "guardian must be launched without terminal descriptors",
            ));
        }
        if libc::getsid(0) != libc::getpid() && libc::setsid() < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    redirect_stdio()?;
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    let mut guardian = Guardian {
        bootstrap,
        root: root.to_path_buf(),
        slave: slave.into(),
        owner: None,
        start: None,
        consumed: false,
        signal_failure: None,
    };
    for connection in listener.incoming() {
        let mut connection = connection?;
        if crate::unix::private_peer(&connection).is_err() {
            continue;
        }
        connection.set_read_timeout(Some(Duration::from_secs(5)))?;
        connection.set_write_timeout(Some(Duration::from_secs(5)))?;
        let request = match protocol::read_frame::<Request>(&mut connection) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let response = guardian.operate(request).unwrap_or_else(|error| {
            let mut response = guardian.status().unwrap_or_else(|_| Response {
                version: protocol::VERSION,
                execution_token: guardian.bootstrap.execution_token.clone(),
                started: guardian.start.is_some(),
                target_pid: None,
                exit_code: None,
                exit_signal: None,
                group_consumed: false,
                target_terminated: guardian.consumed,
                terminal_session_ended: None,
                cleanup_error: guardian.signal_failure.clone(),
                error: None,
            });
            response.error = Some(error.to_string());
            response
        });
        let _ = protocol::write_frame(&mut connection, &response);
        // Keep serving durable receipts after sweep. Parent retirement remains
        // explicit, and retries must not depend on a deleted control endpoint.
    }
    Ok(())
}
