//! Fresh-process supervisor. Never runs a post-fork Rust loop in the daemon.
#[path = "../atomic_state.rs"]
mod atomic_state;
#[path = "../supervisor_protocol.rs"]
mod protocol;
#[path = "../supervisor_receipts.rs"]
mod receipts;

#[cfg(unix)]
mod unix {
    use super::protocol::{self, Bootstrap, Operation, Request, Response};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, Stdio};

    struct Supervisor {
        receipt_root: std::path::PathBuf,
        bootstrap: Bootstrap,
        anchor: Option<Child>,
        target: Option<Child>,
        start: Option<Vec<u8>>,
        outcome: Option<std::process::ExitStatus>,
        consumed: bool,
        signal_failure: Option<String>,
        terminal_released: bool,
    }

    impl Supervisor {
        fn status(&mut self) -> std::io::Result<Response> {
            if self.outcome.is_none()
                && let Some(target) = &mut self.target
            {
                self.outcome = target.try_wait()?;
            }
            if self.outcome.is_some() && !self.terminal_released {
                // Target and descendants own their inherited slave descriptors.
                // Closing ONLY our copies allows real EOF when no product work
                // retains the slave, without sweeping any surviving process.
                let null = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/null")?;
                use std::os::fd::AsRawFd;
                for fd in [0, 1, 2] {
                    // SAFETY: dup2 replaces only this helper's terminal copies;
                    // retaining valid standard descriptors prevents later socket
                    // allocation from reusing them during status/error handling.
                    if unsafe { libc::dup2(null.as_raw_fd(), fd) } < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                self.terminal_released = true;
            }
            Ok(Response {
                version: protocol::VERSION,
                execution_token: self.bootstrap.execution_token.clone(),
                started: self.start.is_some(),
                exit_code: self.outcome.and_then(|status| status.code()),
                exit_signal: self.outcome.and_then(|status| status.signal()),
                group_consumed: self.consumed,
                error: None,
            })
        }

        fn signal(&mut self, signal: i32) -> std::io::Result<()> {
            if let Some(error) = &self.signal_failure {
                return Err(std::io::Error::other(error.clone()));
            }
            if self.consumed {
                return self
                    .signal_failure
                    .as_ref()
                    .map_or(Ok(()), |error| Err(std::io::Error::other(error.clone())));
            }
            let anchor = self
                .anchor
                .as_ref()
                .ok_or_else(|| std::io::Error::other("group anchor unavailable"))?;
            let group = i32::try_from(anchor.id()).map_err(std::io::Error::other)?;
            // The helper owns an unreaped group-leader child, not a remembered
            // PID from a previous execution. Never reap it before consuming KILL.
            if signal == libc::SIGKILL {
                self.consumed = true;
            }
            // SAFETY: negative PID selects the group pinned by our owned child.
            if unsafe { libc::kill(-group, signal) } != 0 {
                let error = std::io::Error::last_os_error();
                self.signal_failure = Some(error.to_string());
                return Err(error);
            }
            Ok(())
        }

        fn reap(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::other(
                        "supervisor cleanup not acknowledged; recovery required",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        fn operate(&mut self, request: Request) -> std::io::Result<Response> {
            if request.version != protocol::VERSION
                || request.execution_token != self.bootstrap.execution_token
                || request.authentication != self.bootstrap.authentication
            {
                return Err(std::io::Error::other("supervisor handshake rejected"));
            }
            let mutation = match &request.operation {
                Operation::Start { mutation_id, .. }
                | Operation::Term { mutation_id }
                | Operation::Sweep { mutation_id } => Some(mutation_id.clone()),
                Operation::Hello | Operation::Status => None,
            };
            let fingerprint =
                serde_json::to_vec(&request.operation).map_err(std::io::Error::other)?;
            let admitted = if let Some(mutation) = mutation {
                let (path, previous) =
                    super::receipts::admit(&self.receipt_root, &mutation, fingerprint.clone())?;
                if let Some(previous) = previous {
                    return serde_json::from_slice(&previous).map_err(std::io::Error::other);
                }
                Some(path)
            } else {
                None
            };
            let response = self.apply(request)?;
            if let Some(path) = admitted {
                super::receipts::complete(
                    &path,
                    fingerprint,
                    serde_json::to_vec(&response).map_err(std::io::Error::other)?,
                )?;
            }
            Ok(response)
        }

        fn apply(&mut self, request: Request) -> std::io::Result<Response> {
            if request.version != protocol::VERSION
                || request.execution_token != self.bootstrap.execution_token
                || request.authentication != self.bootstrap.authentication
            {
                return Err(std::io::Error::other("supervisor handshake rejected"));
            }
            match request.operation {
                Operation::Hello | Operation::Status => {}
                Operation::Start {
                    ref mutation_id,
                    ref program,
                    ref args,
                    ref cwd,
                    ref env,
                } => {
                    if mutation_id.is_empty() {
                        return Err(std::io::Error::other("launch mutation identity required"));
                    }
                    let encoded =
                        serde_json::to_vec(&request.operation).map_err(std::io::Error::other)?;
                    if let Some(previous) = &self.start {
                        if previous != &encoded {
                            return Err(std::io::Error::other("conflicting launch reservation"));
                        }
                    } else {
                        let group = i32::try_from(
                            self.anchor
                                .as_ref()
                                .ok_or_else(|| std::io::Error::other("anchor unavailable"))?
                                .id(),
                        )
                        .map_err(std::io::Error::other)?;
                        let mut command = Command::new(program);
                        command.args(args).envs(env);
                        if let Some(cwd) = cwd {
                            command.current_dir(cwd);
                        }
                        // SAFETY: only async-signal-safe group setup occurs between
                        // fork and exec. argv/env/cwd allocations happened above.
                        unsafe {
                            command.pre_exec(move || {
                                if libc::setpgid(0, group) != 0 {
                                    return Err(std::io::Error::last_os_error());
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
                        // std::process spawn waits for its exec-error-pipe handshake.
                        let child = command.spawn()?;
                        self.target = Some(child);
                        self.start = Some(encoded);
                    }
                }
                Operation::Term { .. } => self.signal(libc::SIGTERM)?,
                Operation::Sweep { .. } => {
                    self.signal(libc::SIGKILL)?;
                    if let Some(anchor) = &mut self.anchor {
                        Self::reap(anchor)?;
                    }
                    if let Some(target) = &mut self.target {
                        self.outcome = Some(Self::reap(target)?);
                    }
                }
            }
            self.status()
        }
    }

    fn private_peer(socket: &std::os::unix::net::UnixStream) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        #[cfg(target_os = "macos")]
        {
            let mut uid = 0;
            let mut gid = 0;
            // SAFETY: valid socket descriptor and initialized output pointers.
            if unsafe { libc::getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: reading own effective UID has no side effect.
            if uid != unsafe { libc::geteuid() } {
                return Err(std::io::Error::other("supervisor peer owner mismatch"));
            }
        }
        #[cfg(target_os = "linux")]
        {
            let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
            let mut length =
                u32::try_from(std::mem::size_of::<libc::ucred>()).map_err(std::io::Error::other)?;
            // SAFETY: buffer/length match SO_PEERCRED representation.
            if unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    std::ptr::from_mut(&mut credential).cast(),
                    &mut length,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if credential.uid != unsafe { libc::geteuid() } {
                return Err(std::io::Error::other("supervisor peer owner mismatch"));
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return Err(std::io::Error::other(
            "supervisor peer authentication unsupported on this platform",
        ));
        Ok(())
    }

    pub fn run() -> std::io::Result<()> {
        let args: Vec<_> = std::env::args().collect();
        if args.get(1).is_some_and(|value| value == "--anchor") {
            // SAFETY: this fresh helper role owns only its own signal state.
            unsafe {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            std::io::stdout().write_all(b"R")?;
            std::io::stdout().flush()?;
            loop {
                std::thread::park();
            }
        }
        if args.len() != 4 || args[1] != "--serve" {
            return Err(std::io::Error::other(
                "expected --serve <private-bootstrap> <private-socket>",
            ));
        }
        let metadata = std::fs::symlink_metadata(&args[2])?;
        let parent = std::path::Path::new(&args[2])
            .parent()
            .ok_or_else(|| std::io::Error::other("bootstrap parent missing"))?;
        let parent_metadata = std::fs::symlink_metadata(parent)?;
        use std::os::unix::fs::MetadataExt;
        // SAFETY: retrieving the helper's own effective identity has no side effect.
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_file()
            || metadata.uid() != uid
            || !parent_metadata.is_dir()
            || parent_metadata.uid() != uid
            || parent_metadata.permissions().mode() & 0o077 != 0
        {
            return Err(std::io::Error::other(
                "bootstrap and parent must be owner-private, non-symlink objects",
            ));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::other("bootstrap must be owner-private"));
        }
        if metadata.len() > u64::try_from(protocol::MAX_FRAME).map_err(std::io::Error::other)? {
            return Err(std::io::Error::other(
                "bootstrap exceeds supervisor frame limit",
            ));
        }
        let bootstrap: Bootstrap =
            serde_json::from_slice(&std::fs::read(&args[2])?).map_err(std::io::Error::other)?;
        if bootstrap.version != protocol::VERSION {
            return Err(std::io::Error::other("unsupported supervisor version"));
        }
        // The caller must launch this helper through the retained PTY primitive.
        // SAFETY: inspecting our own session identity has no side effect.
        if unsafe { libc::getsid(0) }
            != i32::try_from(std::process::id()).map_err(std::io::Error::other)?
        {
            return Err(std::io::Error::other(
                "supervisor requires PTY session leadership",
            ));
        }
        let listener = UnixListener::bind(&args[3])?;
        std::fs::set_permissions(&args[3], std::fs::Permissions::from_mode(0o600))?;
        let mut anchor = Command::new(std::env::current_exe()?)
            .arg("--anchor")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut ready = [0];
        std::io::Read::read_exact(
            anchor
                .stdout
                .as_mut()
                .ok_or_else(|| std::io::Error::other("anchor handshake pipe missing"))?,
            &mut ready,
        )?;
        if ready != [b'R'] {
            return Err(std::io::Error::other("anchor handshake failed"));
        }
        drop(anchor.stdout.take());
        let group = i32::try_from(anchor.id()).map_err(std::io::Error::other)?;
        // SAFETY: supervisor owns this controlling terminal and retained group.
        unsafe {
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            if libc::tcsetpgrp(0, group) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
        let mut supervisor = Supervisor {
            receipt_root: std::path::Path::new(&args[2])
                .parent()
                .ok_or_else(|| std::io::Error::other("private receipt root missing"))?
                .to_path_buf(),
            bootstrap,
            anchor: Some(anchor),
            target: None,
            start: None,
            outcome: None,
            consumed: false,
            signal_failure: None,
            terminal_released: false,
        };
        for connection in listener.incoming() {
            let mut connection = match connection {
                Ok(connection) => connection,
                Err(_) => continue,
            };
            if private_peer(&connection).is_err() {
                continue;
            }
            connection.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
            connection.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
            let request = match protocol::read_frame::<Request>(&mut connection) {
                Ok(request) => request,
                Err(_) => continue,
            };
            let mut sweep_completed = false;
            let response = match supervisor.operate(request) {
                Ok(response) => {
                    sweep_completed = supervisor.consumed;
                    response
                }
                Err(error) => Response {
                    version: protocol::VERSION,
                    execution_token: String::new(),
                    started: false,
                    exit_code: None,
                    exit_signal: None,
                    group_consumed: false,
                    error: Some(error.to_string()),
                },
            };
            let _ = protocol::write_frame(&mut connection, &response);
            if sweep_completed {
                break;
            }
        }
        Ok(())
    }
}

fn main() {
    #[cfg(unix)]
    if unix::run().is_err() {
        // Do not contaminate the target's raw terminal capture with helper logs.
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        eprintln!("execution supervisor requires Unix PTY support");
        std::process::exit(1);
    }
}
