//! Server-lifetime-bound terminal session and authenticated control guardian.
//!
//! Two peers share one session so either can clean HUP-ignoring shell jobs after
//! the other is SIGKILLed. Neither has an independent execution lifetime: server
//! lease EOF closes the guardian, guardian control EOF closes the owner. A sole
//! owner could not run cleanup after SIGKILL. No permanent third watchdog exists.
//! Same-session cleanup is not containment for deliberately setsid()-escaped work.
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
    AuthorityLost,
}

struct CleanupOnDrop(bool);
impl Drop for CleanupOnDrop {
    fn drop(&mut self) {
        if self.0 {
            let _ = crate::session_cleanup::sweep(None);
        }
    }
}

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
    cleanup_error: Option<String>,
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
    let mut cleanup_on_error = CleanupOnDrop(true);
    let operation: Operation =
        serde_json::from_slice(&std::fs::read(launch_path)?).map_err(io::Error::other)?;
    let Operation::Start {
        program,
        args,
        cwd,
        env,
        native_env,
        native_env_encoding,
        ..
    } = operation
    else {
        return Err(io::Error::other("terminal owner requires start operation"));
    };
    if !matches!(native_env_encoding.as_deref(), None | Some("unix-bytes")) {
        return Err(io::Error::other("unsupported Unix environment encoding"));
    }
    // SAFETY: signal disposition belongs to this fresh helper only. Target resets it.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let mut command = Command::new(program);
    command.args(args).env_clear();
    if let Some(entries) = native_env {
        use std::os::unix::ffi::OsStringExt;
        for (key, value) in entries {
            command.env(
                std::ffi::OsString::from_vec(key),
                std::ffi::OsString::from_vec(value),
            );
        }
    } else {
        command.envs(env);
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // SAFETY: process-group/foreground ownership and signal disposition are
    // established in this fresh direct target before exec; guardian anchors ctty.
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
                libc::SIGTTOU,
                libc::SIGTTIN,
                libc::SIGTSTP,
                libc::SIGCHLD,
                libc::SIGALRM,
            ] {
                if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
            }
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            if libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
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
                drain_error: None,
                cleanup_error: Some(format!("terminal target spawn failed: {error}")),
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
        // The authenticated guardian is the sole owner of this channel.
        // EOF (including guardian SIGKILL) ends execution authority.
        let _ = send.send(Wake::AuthorityLost);
    });
    let mut cleanup_requested = false;
    let status = loop {
        match receive.recv().map_err(io::Error::other)? {
            Wake::Exited(result) => {
                result?;
                break child.wait()?;
            }
            Wake::AuthorityLost => {
                cleanup_requested = true;
                // Guardian death is covered by the still-living terminal owner.
                crate::session_cleanup::sweep(None)?;
                // Direct child identity is pinned until wait(), even if it
                // moved out of the terminal session itself.
                child.kill()?;
            }
            Wake::Signal(signal) if matches!(signal, libc::SIGTERM | libc::SIGKILL) => {
                // Direct child remains unreaped until the Exited wake is consumed.
                cleanup_requested |= signal == libc::SIGKILL;
                if signal == libc::SIGKILL {
                    crate::session_cleanup::sweep(None)?;
                }
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
    // End all ordinary job-control groups even on natural shell exit. Keep
    // target exit distinct from capture/drain and cleanup failures.
    let cleanup_result = crate::session_cleanup::sweep(None);
    cleanup_on_error.0 = cleanup_result.is_err();
    let mut outcome = OwnerOutcome {
        exit_code: status.code(),
        exit_signal: status.signal(),
        drain_error: None,
        cleanup_error: cleanup_result
            .err()
            .map(|error| format!("terminal session cleanup failed: {error}")),
        cleanup_requested,
    };
    write_outcome(std::path::Path::new(outcome_path), &outcome)?;
    protocol::write_frame(
        &mut events,
        &OwnerEvent::Outcome(OwnerOutcome {
            exit_code: outcome.exit_code,
            exit_signal: outcome.exit_signal,
            drain_error: outcome.drain_error.clone(),
            cleanup_error: outcome.cleanup_error.clone(),
            cleanup_requested: outcome.cleanup_requested,
        }),
    )?;
    // No tcdrain/alarm: an unread PTY can stall Darwin close/exit after
    // interrupted tcdrain. Poll queued output for a bounded capture grace,
    // then explicitly report failed capture before discarding queued bytes.
    if let Err(error) = drain_terminal(1, Duration::from_millis(250)) {
        outcome.drain_error.get_or_insert_with(|| error.to_string());
    }
    write_outcome(std::path::Path::new(outcome_path), &outcome)?;
    // A failed drain is durable recovery state, not an idle ctty anchor.
    // Exiting releases the terminal; capture must retain the failure as incomplete.
    protocol::write_frame(&mut events, &OwnerEvent::Outcome(outcome))?;
    Ok(())
}

fn drain_terminal(fd: libc::c_int, budget: Duration) -> io::Result<()> {
    let deadline = Instant::now() + budget;
    loop {
        let mut queued: libc::c_int = 0;
        // SAFETY: caller owns this terminal descriptor; initialized ioctl output.
        if unsafe { libc::ioctl(fd, libc::TIOCOUTQ as _, &raw mut queued) } < 0 {
            return Err(io::Error::other(format!(
                "terminal-session drain not acknowledged: {}",
                io::Error::last_os_error()
            )));
        }
        if queued == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Failure is returned even if flushing succeeds. This is teardown,
            // not output-drained acknowledgement. No producer remains after
            // target wait and the explicit same-session sweep.
            let flushed = unsafe { libc::tcflush(fd, libc::TCOFLUSH) };
            return Err(io::Error::other(if flushed == 0 {
                format!(
                    "terminal-session drain not acknowledged: {queued} queued bytes discarded after deadline"
                )
            } else {
                format!(
                    "terminal-session drain not acknowledged; output flush failed: {}",
                    io::Error::last_os_error()
                )
            }));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn retain_owner_outcome(status: &mut Response, outcome: OwnerOutcome) {
    status.exit_code = outcome.exit_code;
    status.exit_signal = outcome.exit_signal;
    // Never overwrite an earlier error with a subsequent partial publication.
    if status.capture_error.is_none() {
        status.capture_error = outcome.drain_error;
    }
    if status.cleanup_error.is_none() {
        status.cleanup_error = outcome.cleanup_error;
    }
}

fn acknowledge_cleanup(status: &Response) -> io::Result<()> {
    if let Some(error) = status.error.as_ref().or(status.cleanup_error.as_ref()) {
        return Err(io::Error::other(error.clone()));
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
        self.signal_with(
            signal,
            Instant::now() + Duration::from_secs(3),
            |stream, signal| protocol::write_frame(stream, &signal),
        )
    }

    fn signal_with(
        &mut self,
        signal: i32,
        deadline: Instant,
        send: impl FnOnce(&mut UnixStream, i32) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.preowner_failure {
            // There is no target to signal, not a fabricated target exit.
            self.consumed = true;
            return Ok(());
        }
        let status = self.lifecycle.wait_until(deadline, |s| {
            s.target_pid.is_some() || s.terminal_session_ended == Some(true) || s.error.is_some()
        })?;
        let mut owner_disconnected = false;
        if status.terminal_session_ended != Some(true) {
            match send(
                self.owner_control
                    .as_mut()
                    .ok_or_else(|| io::Error::other("terminal owner control unavailable"))?,
                signal,
            ) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    // The owner may close control before the observer has
                    // waited for its exact identity and finished session cleanup.
                    // Disconnection alone proves neither: require that observer's
                    // retained completion, within the original operation budget.
                    owner_disconnected = true;
                }
                Err(error) => return Err(error),
            }
        }
        if owner_disconnected
            || signal == libc::SIGKILL
            || status.terminal_session_ended == Some(true)
        {
            let status = self.lifecycle.wait_until(deadline, |s| {
                s.terminal_session_ended == Some(true) || s.error.is_some()
            })?;
            acknowledge_cleanup(&status)?;
            self.consumed = true;
        }
        Ok(())
    }

    fn apply(&mut self, operation: &Operation) -> io::Result<Response> {
        match operation {
            Operation::BindLifetime => {
                return Err(io::Error::other(
                    "Unix guardian uses its inherited lifetime lease",
                ));
            }
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
                            if libc::setpgid(0, 0) < 0 {
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
                                            retain_owner_outcome(s, outcome);
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
                            crate::session_cleanup::sweep(Some(owner_pid))?;
                            // Owner exit closed every helper-held slave descriptor.
                            // Guardian retains only control sockets, not a terminal
                            // FD. Do not revoke here: Darwin revoke can block on
                            // job-control teardown. Explicit kills enforce lifetime;
                            // retirement/lease EOF releases the session anchor.
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
                if !self.preowner_failure {
                    acknowledge_cleanup(&self.status()?)?;
                }
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
            Operation::Hello
            | Operation::Status
            | Operation::Subscribe { .. }
            | Operation::BindLifetime => None,
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
    // Guardian is the session identity anchor, but holds no slave descriptor.
    // The owner shares this session in a distinct process group. Either helper
    // can clean its peer's jobs without joining an unrelated session.
    let terminal = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(slave)?;
    if unsafe { libc::ioctl(terminal.as_raw_fd(), libc::TIOCSCTTY as _, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    drop(terminal);
    redirect_stdio()?;
    // Guardian anchors the session. Unsolicited hangup is not retirement;
    // only the private server lifetime socket supplies that authority.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
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
            capture_error: None,
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
    if unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    protocol::write_frame(&mut ready, &protocol::VERSION)?;
    // This private socket belongs to the server, never an attachment. Server
    // SIGKILL closes it in the kernel. Guardian death separately closes the
    // owner-control socket, so neither helper is an independent lifetime root.
    let lifetime_guardian = Arc::clone(&guardian);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut byte = [0];
        loop {
            match ready.read(&mut byte) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                _ => break,
            }
        }
        let result = lifetime_guardian
            .lock()
            .map_err(|_| io::Error::other("guardian lock poisoned"))
            .and_then(|mut guardian| {
                if guardian.owner.is_some() {
                    guardian.signal(libc::SIGKILL)?;
                    if let Some(owner) = guardian.owner.as_mut() {
                        owner.wait()?;
                    }
                }
                Ok(())
            });
        // A failed cleanup is not acknowledged as successful retirement.
        std::process::exit(i32::from(result.is_err()));
    });
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
                let retire = matches!(&request.operation, Operation::RetireOwner { .. });
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
                protocol::write_frame(&mut connection, &response)?;
                if retire && response.error.is_none() {
                    // Receipt and reply precede retirement; do not accept forever
                    // after the terminal owner has been explicitly retired.
                    std::process::exit(0);
                }
                Ok(())
            })();
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn unread_terminal_drain_is_bounded_and_never_acknowledged_as_capture() {
        use std::io::{Read, Write};
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: private fixture PTY with initialized descriptor outputs.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let mut slave = unsafe { std::fs::File::from_raw_fd(slave) };
        slave.write_all(b"x").unwrap();
        #[cfg(target_os = "macos")]
        {
            let start = Instant::now();
            let error = drain_terminal(slave.as_raw_fd(), Duration::from_millis(20)).unwrap_err();
            assert!(error.to_string().contains("queued bytes discarded"));
            assert!(start.elapsed() < Duration::from_secs(1));
        }
        // Linux PTYs acknowledge handing output to the master queue; Darwin
        // acknowledges master consumption. Exercise successful drain on both.
        slave.write_all(b"y").unwrap();
        let mut byte = [0];
        master.read_exact(&mut byte).unwrap();
        if byte != *b"y" {
            master.read_exact(&mut byte).unwrap();
        }
        assert_eq!(byte, *b"y");
        drain_terminal(slave.as_raw_fd(), Duration::from_millis(100)).unwrap();
    }

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
            native_env: None,
            native_env_encoding: None,
        }
    }
    fn disconnected_owner_fixture() -> (tempfile::TempDir, Guardian) {
        let root = tempfile::tempdir().unwrap();
        let lifecycle = Arc::new(
            Lifecycle::new(
                root.path().join("lifecycle.json"),
                Response {
                    version: protocol::VERSION,
                    execution_token: "execution".into(),
                    started: true,
                    target_pid: Some(42),
                    exit_code: None,
                    exit_signal: None,
                    group_consumed: false,
                    target_terminated: false,
                    terminal_session_ended: Some(false),
                    error: None,
                    capture_error: None,
                    cleanup_error: None,
                },
            )
            .unwrap(),
        );
        let (control, owner) = UnixStream::pair().unwrap();
        drop(owner);
        let guardian = Guardian {
            bootstrap: Bootstrap {
                version: protocol::VERSION,
                execution_token: "execution".into(),
                authentication: "secret".into(),
            },
            root: root.path().to_path_buf(),
            slave: String::new(),
            owner: None,
            start: None,
            consumed: false,
            signal_failure: None,
            lifecycle,
            owner_control: Some(control),
            preowner_failure: false,
            fail_owner_spawn: false,
        };
        (root, guardian)
    }

    #[test]
    fn disconnected_owner_signal_requires_retained_session_cleanup() {
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            for failure in [None, Some("cleanup"), Some("lifecycle")] {
                let (_root, mut guardian) = disconnected_owner_fixture();
                let lifecycle = Arc::clone(&guardian.lifecycle);
                let result = guardian.signal_with(
                    signal,
                    Instant::now() + Duration::from_secs(3),
                    |stream, signal| {
                        // Deterministically close control before completion is
                        // published, after signal_with's initial stale snapshot.
                        let error = protocol::write_frame(stream, &signal).unwrap_err();
                        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                        assert_eq!(
                            lifecycle.snapshot().status.terminal_session_ended,
                            Some(false)
                        );
                        lifecycle
                            .update(|s| {
                                if failure == Some("lifecycle") {
                                    s.error = Some("injected owner lifecycle failure".into());
                                } else {
                                    s.terminal_session_ended = Some(true);
                                    s.exit_signal = Some(libc::SIGTERM);
                                    if failure == Some("cleanup") {
                                        s.cleanup_error = Some("injected cleanup failure".into());
                                    }
                                }
                            })
                            .unwrap();
                        Err(error)
                    },
                );
                if let Some(failure) = failure {
                    assert!(result.unwrap_err().to_string().contains(failure));
                    assert!(!guardian.consumed);
                } else {
                    result.unwrap();
                    assert!(guardian.consumed);
                }
            }
        }
    }

    #[test]
    fn disconnected_owner_without_completion_is_not_consumed() {
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let (_root, mut guardian) = disconnected_owner_fixture();
            let error = guardian
                .signal_with(signal, Instant::now(), |stream, signal| {
                    protocol::write_frame(stream, &signal)
                })
                .unwrap_err();
            assert!(error.to_string().contains("outcome uncertain"));
            assert!(!guardian.consumed);
            assert_eq!(
                guardian.lifecycle.snapshot().status.terminal_session_ended,
                Some(false)
            );
        }
    }

    #[test]
    fn owner_signal_transport_errors_are_not_completion() {
        let (_root, mut guardian) = disconnected_owner_fixture();
        let error = guardian
            .signal_with(libc::SIGTERM, Instant::now(), |_, _| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "injected transport",
                ))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(!guardian.consumed);
    }

    #[test]
    fn capture_and_cleanup_errors_remain_independent_and_durable() {
        for (capture, cleanup) in [(true, false), (false, true), (true, true)] {
            let root = tempfile::tempdir().unwrap();
            let mut status = Response {
                version: protocol::VERSION,
                execution_token: "execution".into(),
                started: true,
                target_pid: Some(42),
                exit_code: None,
                exit_signal: None,
                group_consumed: false,
                target_terminated: false,
                terminal_session_ended: Some(true),
                error: None,
                capture_error: None,
                cleanup_error: None,
            };
            retain_owner_outcome(
                &mut status,
                OwnerOutcome {
                    exit_code: None,
                    exit_signal: Some(libc::SIGKILL),
                    drain_error: capture.then(|| "injected capture failure".into()),
                    cleanup_error: cleanup.then(|| "injected sweep failure".into()),
                    cleanup_requested: true,
                },
            );
            // A later publication without errors cannot clear either category.
            retain_owner_outcome(
                &mut status,
                OwnerOutcome {
                    exit_code: None,
                    exit_signal: Some(libc::SIGKILL),
                    drain_error: None,
                    cleanup_error: None,
                    cleanup_requested: true,
                },
            );
            let lifecycle =
                Arc::new(Lifecycle::new(root.path().join("lifecycle.json"), status).unwrap());
            let mut guardian = Guardian {
                bootstrap: Bootstrap {
                    version: protocol::VERSION,
                    execution_token: "execution".into(),
                    authentication: "secret".into(),
                },
                root: root.path().to_path_buf(),
                slave: String::new(),
                owner: None,
                start: None,
                consumed: false,
                signal_failure: None,
                lifecycle: Arc::clone(&lifecycle),
                owner_control: None,
                preowner_failure: false,
                fail_owner_spawn: false,
            };
            assert_eq!(guardian.signal(libc::SIGKILL).is_err(), cleanup);
            assert_eq!(guardian.consumed, !cleanup);
            let retained: protocol::LifecycleSnapshot =
                serde_json::from_slice(&std::fs::read(root.path().join("lifecycle.json")).unwrap())
                    .unwrap();
            assert_eq!(retained.status.exit_signal, Some(libc::SIGKILL));
            assert_eq!(retained.status.capture_error.is_some(), capture);
            assert_eq!(retained.status.cleanup_error.is_some(), cleanup);
            assert!(retained.status.error.is_none());
            assert_eq!(acknowledge_cleanup(&retained.status).is_err(), cleanup);
            if cleanup {
                assert!(
                    guardian
                        .operate(request(Operation::RetireOwner {
                            mutation_id: "00000000-0000-4000-8000-000000000003".into(),
                        }))
                        .is_err()
                );
            }
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
            capture_error: None,
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
