//! Conventional terminal-session owner, independent authenticated guardian.
use crate::protocol::{self, Bootstrap, Operation, Request, Response};
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
#[path = "lifecycle.rs"]
mod lifecycle;
use lifecycle::Lifecycle;

#[derive(serde::Serialize, serde::Deserialize)]
enum OwnerEvent {
    Ready(u32),
    Outcome(OwnerOutcome),
}

enum Wake {
    Signal(i32),
    Exited(io::Result<()>),
}
unsafe extern "C" fn interrupt_drain(_: libc::c_int) {}

fn wait_child_identity(pid: u32) -> io::Result<()> {
    // WNOWAIT pins identity until the owner explicitly reaps; no PID reuse race.
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waiting for our direct child into a correctly sized buffer.
        if unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) } == 0
        {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
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
    // SAFETY: guardian explicitly hands off this owned socket descriptor at exec.
    let mut events = unsafe { UnixStream::from_raw_fd(3) };
    events.set_write_timeout(Some(Duration::from_secs(5)))?;
    // Do not leak the control socket into the product or descendants.
    if unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
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
            let outcome = OwnerOutcome {
                exit_code: None,
                exit_signal: None,
                drain_error: Some(format!("terminal target spawn failed: {error}")),
                cleanup_requested: false,
            };
            write_outcome(std::path::Path::new(outcome_path), &outcome)?;
            protocol::write_frame(&mut events, &OwnerEvent::Outcome(outcome))?;
            return Err(error);
        }
    };
    // Dropping the prepared command releases any retained spawn-side state;
    // only the product child and this terminal owner keep terminal descriptors.
    drop(command);
    let target_pid = child.id();
    crate::atomic_state::commit(
        &std::path::Path::new(outcome_path).with_extension("ready"),
        &target_pid,
    )?;
    protocol::write_frame(&mut events, &OwnerEvent::Ready(target_pid))?;
    // Worker threads inherit SIGALRM blocked; only the drain owner unblocks it.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGALRM);
        let result = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
    }
    let (send, receive) = std::sync::mpsc::sync_channel(4);
    let waiter = send.clone();
    std::thread::spawn(move || {
        let _ = waiter.send(Wake::Exited(wait_child_identity(target_pid)));
    });
    let mut controls = events.try_clone()?;
    std::thread::spawn(move || {
        while let Ok(signal) = protocol::read_frame::<i32>(&mut controls) {
            if send.send(Wake::Signal(signal)).is_err() {
                break;
            }
        }
        // Guardian disconnect is not target completion or termination authority.
    });
    let mut cleanup_requested = false;
    let status = loop {
        match receive.recv().map_err(io::Error::other)? {
            Wake::Exited(result) => {
                result?;
                break child.wait()?;
            }
            Wake::Signal(signal) if matches!(signal, libc::SIGTERM | libc::SIGKILL) => {
                // Direct child remains unreaped until the Exited wake is consumed.
                cleanup_requested |= signal == libc::SIGKILL;
                if unsafe {
                    libc::kill(i32::try_from(target_pid).map_err(io::Error::other)?, signal)
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Wake::Signal(_) => return Err(io::Error::other("unsupported target signal")),
        }
    };
    let mut outcome = OwnerOutcome {
        exit_code: status.code(),
        exit_signal: status.signal(),
        drain_error: None,
        cleanup_requested,
    };
    write_outcome(std::path::Path::new(outcome_path), &outcome)?;
    protocol::write_frame(
        &mut events,
        &OwnerEvent::Outcome(OwnerOutcome {
            exit_code: outcome.exit_code,
            exit_signal: outcome.exit_signal,
            drain_error: None,
            cleanup_requested: outcome.cleanup_requested,
        }),
    )?;
    // Target outcome is durable before drain/retirement. Later final publication
    // records whether the terminal drain was acknowledged.
    // Target exit establishes the new contract's terminal-session end boundary.
    // Drain bytes already queued at that boundary before allowing the OS revoke.
    // An alarm interrupts stalled drain, but MUST NOT retire the terminal owner.
    // SAFETY: local helper installs a non-restarting handler and bounds its own ioctl.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = interrupt_drain as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGALRM, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGALRM);
        let result = libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
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
    // A failed drain is durable recovery state, not an idle ctty anchor.
    // Exiting releases the terminal; capture must retain the failure as incomplete.
    protocol::write_frame(&mut events, &OwnerEvent::Outcome(outcome))?;
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
    lifecycle: Arc<Lifecycle>,
    owner_control: Option<UnixStream>,
    // Only set after a failed launch is known to have created no terminal owner.
    preowner_failure: bool,
    #[cfg(test)]
    fail_owner_spawn: bool,
}

impl Guardian {
    fn owner_exited(&self) -> io::Result<bool> {
        Ok(self.lifecycle.snapshot().status.terminal_session_ended == Some(true))
    }

    fn status(&self) -> io::Result<Response> {
        let mut status = self.lifecycle.snapshot().status;
        status.target_terminated = self.consumed;
        status.cleanup_error = self.signal_failure.clone().or(status.cleanup_error);
        Ok(status)
    }

    fn signal(&mut self, signal: i32) -> io::Result<()> {
        if self.preowner_failure {
            // There is no target to signal, not a fabricated target exit.
            self.consumed = true;
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = self.lifecycle.wait_until(deadline, |s| {
            s.target_pid.is_some() || s.terminal_session_ended == Some(true) || s.error.is_some()
        })?;
        if status.terminal_session_ended != Some(true) {
            protocol::write_frame(
                self.owner_control
                    .as_mut()
                    .ok_or_else(|| io::Error::other("terminal owner control unavailable"))?,
                &signal,
            )?;
        }
        if signal == libc::SIGKILL || status.terminal_session_ended == Some(true) {
            let status = self
                .lifecycle
                .wait_until(deadline, |s| s.terminal_session_ended == Some(true))?;
            if let Some(error) = status.error {
                return Err(io::Error::other(error));
            }
            self.consumed = true;
        }
        Ok(())
    }

    fn apply(&mut self, operation: &Operation) -> io::Result<Response> {
        match operation {
            Operation::Hello | Operation::Status => {}
            Operation::Subscribe { .. } => {
                return Err(io::Error::other(
                    "subscription requires separate connection",
                ));
            }
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
                    // Reserve before effects: even a failed pre-owner launch may
                    // never become permission for a second execution.
                    self.start = Some(encoded);
                    let launch = self.root.join("terminal-owner-launch.json");
                    crate::atomic_state::commit(&launch, operation)?;
                    let slave = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_NOCTTY)
                        .open(&self.slave)?;
                    let (mut events, owner_events) = UnixStream::pair()?;
                    let event_fd = owner_events.as_raw_fd();
                    events.set_write_timeout(Some(Duration::from_secs(3)))?;
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
                        command.pre_exec(move || {
                            if libc::dup2(event_fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                                return Err(io::Error::last_os_error());
                            }
                            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                                return Err(io::Error::last_os_error());
                            }
                            if libc::tcsetpgrp(0, libc::getpid()) < 0 {
                                return Err(io::Error::last_os_error());
                            }
                            Ok(())
                        });
                    }
                    #[cfg(test)]
                    if self.fail_owner_spawn {
                        return Err(io::Error::from_raw_os_error(libc::EAGAIN));
                    }
                    self.owner = Some(command.spawn()?);
                    drop(owner_events);
                    let owner_pid = self.owner.as_ref().expect("owner just spawned").id();
                    self.owner_control = Some(events.try_clone()?);
                    let lifecycle = Arc::clone(&self.lifecycle);
                    lifecycle.update(|s| s.started = true)?;
                    std::thread::spawn(move || {
                        let mut observed_outcome = false;
                        let result = (|| -> io::Result<()> {
                            loop {
                                match protocol::read_frame::<OwnerEvent>(&mut events) {
                                    Ok(OwnerEvent::Ready(pid)) => {
                                        lifecycle.update(|s| s.target_pid = Some(pid))?
                                    }
                                    Ok(OwnerEvent::Outcome(outcome)) => {
                                        observed_outcome = true;
                                        lifecycle.update(|s| {
                                            s.exit_code = outcome.exit_code;
                                            s.exit_signal = outcome.exit_signal;
                                            s.error = outcome.drain_error.clone();
                                            s.cleanup_error = outcome.drain_error;
                                        })?;
                                    }
                                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                                        break;
                                    }
                                    Err(error) => return Err(error),
                                }
                            }
                            // EOF is not completion: wait for our direct owner identity.
                            wait_child_identity(owner_pid)?;
                            lifecycle.update(|s| {
                                s.terminal_session_ended = Some(true);
                                if !observed_outcome {
                                    s.error = Some("terminal owner exited without durable outcome; capture incomplete".into());
                                }
                            })
                        })();
                        if let Err(error) = result
                            && lifecycle
                                .update(|s| {
                                    s.error = Some(format!("owner lifecycle unavailable: {error}"))
                                })
                                .is_err()
                        {
                            // Failed durability must disconnect as uncertain, not look healthy.
                            std::process::exit(1);
                        }
                    });
                    // Drop Command too: stdio clones otherwise keep the slave alive.
                    drop(command);
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
                if let Some(owner) = self.owner.as_mut() {
                    owner.wait()?;
                    self.owner = None;
                } else if !self.preowner_failure {
                    return Err(io::Error::other("terminal owner missing"));
                }
                // No claim that escaped descendants were terminated or swept.
            }
        }
        let mut response = self.status()?;
        if self.preowner_failure
            && matches!(
                operation,
                Operation::Term { .. } | Operation::Sweep { .. } | Operation::RetireOwner { .. }
            )
        {
            // Cleanup acknowledgment is separate from the retained launch failure.
            // Keep the original error in lifecycle/status and the Start receipt.
            response.error = None;
        }
        Ok(response)
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
            Operation::Hello | Operation::Status | Operation::Subscribe { .. } => None,
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
        let retire = matches!(&request.operation, Operation::RetireOwner { .. });
        let mut failed_before_owner = false;
        let response = match self.apply(&request.operation) {
            Ok(response) => response,
            Err(error)
                if matches!(&request.operation, Operation::Start { .. })
                    && self.owner.is_none()
                    && self.start.as_ref() == Some(&fingerprint) =>
            {
                // std::process::Command::spawn returned an error without a Child;
                // its failed exec child is reaped by std. No execution authority
                // exists, so this failure can be settled, not left uncertain.
                failed_before_owner = true;
                let mut response = self.status()?;
                response.started = true;
                response.target_pid = None;
                response.exit_code = None;
                response.exit_signal = None;
                response.terminal_session_ended = Some(true);
                response.error = Some(error.to_string());
                response
            }
            Err(error) => return Err(error),
        };
        if let Some(path) = admitted {
            crate::receipts::complete(
                &path,
                fingerprint,
                serde_json::to_vec(&response).map_err(io::Error::other)?,
            )?;
        }
        if failed_before_owner {
            // Both receipt and retained outcome are durable before the wakeup.
            self.lifecycle.update(|status| *status = response.clone())?;
            self.preowner_failure = true;
        }
        if retire {
            // Only a committed retirement receipt arms owned ChildKiller's SIGHUP.
            // Until then, attachment loss must not destroy the control authority.
            unsafe {
                libc::signal(libc::SIGHUP, libc::SIG_DFL);
            }
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
    // Guardian is not a terminal owner. Unsolicited hangup is not retirement.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    let lifecycle = Arc::new(Lifecycle::new(
        root.join("lifecycle.json"),
        Response {
            version: protocol::VERSION,
            execution_token: bootstrap.execution_token.clone(),
            started: false,
            target_pid: None,
            exit_code: None,
            exit_signal: None,
            group_consumed: false,
            target_terminated: false,
            terminal_session_ended: Some(false),
            cleanup_error: None,
            error: None,
        },
    )?);
    let guardian = Arc::new(Mutex::new(Guardian {
        bootstrap,
        root: root.to_path_buf(),
        slave: slave.into(),
        owner: None,
        start: None,
        consumed: false,
        signal_failure: None,
        lifecycle: Arc::clone(&lifecycle),
        owner_control: None,
        preowner_failure: false,
        #[cfg(test)]
        fail_owner_spawn: false,
    }));
    // Inherited startup socket: endpoint and retained authority exist before ack.
    let mut ready = unsafe { UnixStream::from_raw_fd(3) };
    protocol::write_frame(&mut ready, &protocol::VERSION)?;
    drop(ready);
    let peers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let subscriptions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for connection in listener.incoming() {
        let mut connection = connection?;
        if crate::unix::private_peer(&connection).is_err() {
            continue;
        }
        let peers = Arc::clone(&peers);
        if peers.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 16 {
            peers.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            continue;
        }
        let guardian = Arc::clone(&guardian);
        let lifecycle = Arc::clone(&lifecycle);
        let subscriptions = Arc::clone(&subscriptions);
        std::thread::spawn(move || {
            struct PeerGuard(Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for PeerGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _peer = PeerGuard(peers);
            let _ = (|| -> io::Result<()> {
                connection.set_read_timeout(Some(Duration::from_secs(5)))?;
                connection.set_write_timeout(Some(Duration::from_secs(5)))?;
                let request: Request = protocol::read_frame(&mut connection)?;
                {
                    let g = guardian.lock().expect("guardian lock");
                    if request.version != protocol::VERSION
                        || request.execution_token != g.bootstrap.execution_token
                        || request.authentication != g.bootstrap.authentication
                    {
                        return Err(io::Error::other("supervisor handshake rejected"));
                    }
                }
                if let Operation::Subscribe { capability } = request.operation {
                    if capability != protocol::LIFECYCLE_CAPABILITY {
                        return Err(io::Error::other("unsupported lifecycle capability"));
                    }
                    if subscriptions.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 8 {
                        subscriptions.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        return Err(io::Error::other("lifecycle subscriber capacity exhausted"));
                    }
                    let _subscription = PeerGuard(subscriptions);
                    drop(_peer);
                    connection.set_read_timeout(None)?;
                    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let mut reader = connection.try_clone()?;
                    let stop = Arc::clone(&cancelled);
                    let wake = Arc::clone(&lifecycle);
                    let monitor = std::thread::spawn(move || {
                        use std::io::Read;
                        let mut byte = [0];
                        let _ = reader.read(&mut byte);
                        wake.cancel(&stop);
                    });
                    let result = (|| -> io::Result<()> {
                        let mut snapshot = lifecycle.snapshot();
                        loop {
                            protocol::write_frame(&mut connection, &snapshot)?;
                            let Some(next) = lifecycle.next(snapshot.revision, &cancelled) else {
                                break;
                            };
                            snapshot = next;
                        }
                        Ok(())
                    })();
                    let _ = connection.shutdown(std::net::Shutdown::Both);
                    let _ = monitor.join();
                    return result;
                }
                let mut g = guardian.lock().expect("guardian lock");
                let response = g.operate(request).unwrap_or_else(|error| {
                    let mut response = lifecycle.snapshot().status;
                    response.error = Some(error.to_string());
                    response
                });
                // Mutation receipts commit before retained changes and notification.
                lifecycle.update(|s| {
                    s.target_terminated = g.consumed;
                    s.cleanup_error = g.signal_failure.clone().or(s.cleanup_error.clone());
                })?;
                protocol::write_frame(&mut connection, &response)
            })();
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn request(operation: Operation) -> Request {
        Request {
            version: protocol::VERSION,
            execution_token: "execution".into(),
            authentication: "secret".into(),
            operation,
        }
    }
    fn start() -> Operation {
        Operation::Start {
            mutation_id: "00000000-0000-4000-8000-000000000001".into(),
            program: "/bin/true".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
        }
    }
    #[test]
    fn failed_owner_spawn_is_durable_retry_safe_and_retirable() {
        let root = tempfile::tempdir().unwrap();
        let status = Response {
            version: protocol::VERSION,
            execution_token: "execution".into(),
            started: false,
            target_pid: None,
            exit_code: None,
            exit_signal: None,
            group_consumed: false,
            target_terminated: false,
            terminal_session_ended: Some(false),
            cleanup_error: None,
            error: None,
        };
        let lifecycle =
            Arc::new(Lifecycle::new(root.path().join("lifecycle.json"), status).unwrap());
        let mut guardian = Guardian {
            bootstrap: Bootstrap {
                version: protocol::VERSION,
                execution_token: "execution".into(),
                authentication: "secret".into(),
            },
            root: root.path().to_path_buf(),
            slave: "/dev/null".into(),
            owner: None,
            start: None,
            consumed: false,
            signal_failure: None,
            lifecycle: Arc::clone(&lifecycle),
            owner_control: None,
            preowner_failure: false,
            fail_owner_spawn: true,
        };
        let expected = io::Error::from_raw_os_error(libc::EAGAIN).to_string();
        let failed = guardian.operate(request(start())).unwrap();
        assert_eq!(failed.error.as_deref(), Some(expected.as_str()));
        assert!(failed.started);
        assert_eq!(failed.target_pid, None);
        assert_eq!(failed.exit_code, None);
        assert_eq!(failed.exit_signal, None);
        assert_eq!(failed.terminal_session_ended, Some(true));
        assert!(guardian.owner.is_none());
        let retained: protocol::LifecycleSnapshot =
            serde_json::from_slice(&std::fs::read(root.path().join("lifecycle.json")).unwrap())
                .unwrap();
        assert_eq!(retained.status.error, failed.error);
        guardian.fail_owner_spawn = false;
        let retry = guardian.operate(request(start())).unwrap();
        assert_eq!(retry.error, failed.error);
        assert!(guardian.owner.is_none());
        let mut conflict = start();
        if let Operation::Start { program, .. } = &mut conflict {
            *program = "/bin/false".into();
        }
        assert!(guardian.operate(request(conflict)).is_err());
        let swept = guardian
            .operate(request(Operation::Sweep {
                mutation_id: "00000000-0000-4000-8000-000000000002".into(),
            }))
            .unwrap();
        assert!(swept.target_terminated);
        assert!(!swept.group_consumed);
        assert!(swept.error.is_none());
        assert_eq!(swept.exit_code, None);
        let retired = guardian
            .operate(request(Operation::RetireOwner {
                mutation_id: "00000000-0000-4000-8000-000000000003".into(),
            }))
            .unwrap();
        assert!(retired.error.is_none());
        assert!(retired.target_terminated);
        assert_eq!(guardian.status().unwrap().error, failed.error);
        assert_eq!(
            guardian.operate(request(start())).unwrap().error,
            failed.error
        );
    }
}
