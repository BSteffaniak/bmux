//! Run a command in a pseudo-terminal and stream its output.
//!
//! [`PtyCommand`] is built like [`std::process::Command`]; [`PtyCommand::spawn`] starts it on a
//! new pseudo-terminal of a given [`PtySize`] and returns a [`PtyProcess`]. Output arrives as
//! [`PtyEvent`]s in the order it was read, ending with exactly one [`PtyEvent::Exited`]. Events
//! pass through a bounded queue: a consumer that falls behind slows the reader threads (and so
//! the process, once its terminal buffer fills) instead of losing or buffering unbounded output.
//!
//! The process sees a real terminal: programs keep their colors, progress displays and
//! line editing. [`PtyCommand::capture_stdout`] keeps standard output on a pipe instead, so it
//! stays byte-exact for parsing while standard error (where most tools write progress and
//! diagnostics) is still a terminal.
//!
//! This crate only runs processes. Interpreting the terminal stream (for example with
//! `bmux_terminal_grid`) and presenting it are the caller's concern.
//!
//! On platforms without pseudo-terminal support here (anything but Unix), processes run on
//! plain pipes: output is still streamed, but programs see no terminal and resizing has no
//! effect.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// Events queued between the reader threads and the consumer before readers wait.
const QUEUE: usize = 64;
/// Bytes read per output event at most.
const CHUNK: usize = 16 * 1024;
/// How long output still arriving after the process exits is waited for. Output written by
/// descendants that outlive the process and keep the terminal open is not waited for.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Terminal size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PtySize {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

impl PtySize {
    /// A size of `cols` columns by `rows` rows; zero is raised to one.
    #[must_use]
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: if cols == 0 { 1 } else { cols },
            rows: if rows == 0 { 1 } else { rows },
        }
    }
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PtyExit {
    code: Option<i32>,
    signal: Option<i32>,
}

impl PtyExit {
    /// Exit code, when the process exited normally.
    #[must_use]
    pub const fn code(self) -> Option<i32> {
        self.code
    }

    /// Terminating signal, when a signal ended the process.
    #[must_use]
    pub const fn signal(self) -> Option<i32> {
        self.signal
    }

    /// Whether the process exited with code zero.
    #[must_use]
    pub const fn success(self) -> bool {
        matches!(self.code, Some(0))
    }

    fn from_status(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
        }
    }
}

/// Something that happened to a running process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    /// Bytes the process wrote to its terminal: a terminal stream with control sequences.
    Output(Vec<u8>),
    /// Bytes the process wrote to standard output, when [`PtyCommand::capture_stdout`] keeps
    /// it on a pipe. Unaltered; not a terminal stream.
    Stdout(Vec<u8>),
    /// The process ended. Always the last event.
    Exited(PtyExit),
}

/// A command to run on a pseudo-terminal.
///
/// Like [`std::process::Command`], the environment is inherited unless changed. `TERM` is set
/// to `xterm-256color` unless [`PtyCommand::env`] sets it: the reader of the stream decides
/// which terminal it emulates.
#[derive(Debug, Clone)]
pub struct PtyCommand {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: Vec<(OsString, Option<OsString>)>,
    capture_stdout: bool,
    input: Option<Vec<u8>>,
}

impl PtyCommand {
    /// A command running `program`.
    #[must_use]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            capture_stdout: false,
            input: None,
        }
    }

    /// Add an argument.
    #[must_use]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Add arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }

    /// Run in `dir`.
    #[must_use]
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_owned());
        self
    }

    /// Set an environment variable.
    #[must_use]
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_owned(), Some(value.as_ref().to_owned())));
        self
    }

    /// Remove an inherited environment variable.
    #[must_use]
    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Self {
        self.env.push((key.as_ref().to_owned(), None));
        self
    }

    /// Keep standard output on a pipe, reported as [`PtyEvent::Stdout`] and left byte-exact.
    /// Standard error stays on the terminal; standard input is closed. The process gets no
    /// controlling terminal, so it cannot prompt through `/dev/tty`.
    #[must_use]
    pub const fn capture_stdout(mut self) -> Self {
        self.capture_stdout = true;
        self
    }

    /// Feed `bytes` to standard input, then close it. Standard input becomes a pipe instead
    /// of the terminal.
    #[must_use]
    pub fn input(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.input = Some(bytes.into());
        self
    }

    /// Start the process on a new terminal of `size`.
    ///
    /// # Errors
    /// * [`io::Error`] when the terminal cannot be created or the program cannot be started.
    pub fn spawn(self, size: PtySize) -> io::Result<PtyProcess> {
        platform::spawn(&self, size)
    }

    fn std_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if !self.env.iter().any(|(key, _)| key == "TERM") {
            command.env("TERM", "xterm-256color");
        }
        for (key, value) in &self.env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command
    }
}

/// A process running on a pseudo-terminal.
///
/// Dropping it while the process runs kills the process and everything it started on its
/// terminal.
#[derive(Debug)]
pub struct PtyProcess {
    events: mpsc::Receiver<PtyEvent>,
    control: platform::Control,
    exited: bool,
}

impl PtyProcess {
    /// Operating system process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.control.id()
    }

    /// Whether [`PtyEvent::Exited`] has been received.
    #[must_use]
    pub const fn has_exited(&self) -> bool {
        self.exited
    }

    /// The next event, waiting for it. `None` once the process has exited and its exit
    /// event was returned.
    pub fn next_event(&mut self) -> Option<PtyEvent> {
        if self.exited {
            return None;
        }
        self.track(self.events.recv().ok())
    }

    /// The next event if one is ready, without waiting.
    pub fn try_event(&mut self) -> Option<PtyEvent> {
        if self.exited {
            return None;
        }
        self.track(self.events.try_recv().ok())
    }

    /// The next event, waiting at most `timeout`.
    pub fn next_event_timeout(&mut self, timeout: Duration) -> Option<PtyEvent> {
        if self.exited {
            return None;
        }
        self.track(self.events.recv_timeout(timeout).ok())
    }

    /// Events that are ready now, at most `limit` of them, without waiting. Bounding the
    /// batch keeps one drain from running indefinitely while output keeps arriving.
    pub fn ready_events(&mut self, limit: usize) -> Vec<PtyEvent> {
        std::iter::from_fn(|| self.try_event())
            .take(limit)
            .collect()
    }

    /// Every remaining event, waiting for each, until the process exits.
    pub fn events(&mut self) -> impl Iterator<Item = PtyEvent> + '_ {
        std::iter::from_fn(|| self.next_event())
    }

    /// Resize the terminal; the process is notified as by a terminal window resize.
    ///
    /// # Errors
    /// * [`io::Error`] when the terminal rejects the size.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        self.control.resize(size)
    }

    /// Interrupt the process and everything it started on its terminal, as `ctrl+c` would.
    ///
    /// # Errors
    /// * [`io::Error`] when the signal cannot be sent.
    pub fn interrupt(&self) -> io::Result<()> {
        self.control.interrupt()
    }

    /// Kill the process and everything it started on its terminal.
    ///
    /// # Errors
    /// * [`io::Error`] when the signal cannot be sent.
    pub fn kill(&self) -> io::Result<()> {
        self.control.kill()
    }

    fn track(&mut self, event: Option<PtyEvent>) -> Option<PtyEvent> {
        if matches!(event, Some(PtyEvent::Exited(_))) {
            self.exited = true;
        }
        event
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.control.kill();
        }
    }
}

/// Write `input` to the child's standard input on its own thread (so a full pipe cannot
/// block the caller), then close it.
fn feed_input(child: &mut std::process::Child, input: Option<&Vec<u8>>) {
    if let (Some(input), Some(mut pipe)) = (input, child.stdin.take()) {
        let input = input.clone();
        std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = pipe.write_all(&input);
        });
    }
}

/// Shared by the threads of one process: the event queue and whether the exit was reported,
/// after which late output is dropped so `Exited` stays last.
#[derive(Clone)]
struct Events {
    sender: mpsc::SyncSender<PtyEvent>,
    closed: Arc<AtomicBool>,
}

impl Events {
    fn new() -> (Self, mpsc::Receiver<PtyEvent>) {
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        (
            Self {
                sender,
                closed: Arc::new(AtomicBool::new(false)),
            },
            receiver,
        )
    }

    /// Forward everything `reader` produces as events, then report on `done`.
    fn read(
        &self,
        mut reader: impl Read + Send + 'static,
        event: fn(Vec<u8>) -> PtyEvent,
        done: mpsc::Sender<()>,
    ) {
        let events = self.clone();
        std::thread::spawn(move || {
            let mut buffer = vec![0; CHUNK];
            loop {
                match reader.read(&mut buffer) {
                    // A pseudo-terminal reports a closed far side as an error (EIO).
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if events.closed.load(Ordering::Acquire)
                            || events.sender.send(event(buffer[..read].to_vec())).is_err()
                        {
                            break;
                        }
                    }
                }
            }
            let _ = done.send(());
        });
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use super::{Events, PtyCommand, PtyEvent, PtyProcess, PtySize};

    #[derive(Debug)]
    pub struct Control {
        master: OwnedFd,
        pid: u32,
        exited: Arc<AtomicBool>,
    }

    impl Control {
        pub const fn id(&self) -> u32 {
            self.pid
        }

        pub fn resize(&self, size: PtySize) -> io::Result<()> {
            set_size(self.master.as_raw_fd(), size)
        }

        pub fn interrupt(&self) -> io::Result<()> {
            self.signal(libc::SIGINT)
        }

        pub fn kill(&self) -> io::Result<()> {
            self.signal(libc::SIGKILL)
        }

        /// Signal the process group the process leads (it started its own session).
        fn signal(&self, signal: libc::c_int) -> io::Result<()> {
            // Once reaped, the id may belong to another process.
            if self.exited.load(Ordering::Acquire) {
                return Ok(());
            }
            let group = libc::pid_t::try_from(self.pid)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // SAFETY: kill has no memory-safety preconditions.
            if unsafe { libc::kill(-group, signal) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
            Ok(())
        }
    }

    fn set_size(fd: RawFd, size: PtySize) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `fd` is an open terminal and `size` outlives the call.
        if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &raw const size) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Make the terminal at `fd` the calling process's controlling terminal. Async-signal-safe.
    // The ioctl request's integer type differs between platforms (`u32` constant against a
    // `c_ulong` parameter on macOS, matching types on Linux), so no single conversion is lossless
    // everywhere without a lint; the value always fits.
    #[allow(clippy::cast_lossless)]
    fn take_controlling_terminal(fd: RawFd) -> io::Result<()> {
        // SAFETY: ioctl on an open terminal descriptor with an integer argument.
        if unsafe { libc::ioctl(fd, libc::TIOCSCTTY as _, 0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn close_on_exec(fd: RawFd) -> io::Result<()> {
        // SAFETY: fcntl on an owned, open descriptor.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn open(size: PtySize) -> io::Result<(OwnedFd, OwnedFd)> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut size = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the out-pointers are valid for writes; null name and termios are allowed.
        let result = unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty returned two new descriptors that nothing else owns.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        close_on_exec(master.as_raw_fd())?;
        close_on_exec(slave.as_raw_fd())?;
        Ok((master, slave))
    }

    pub fn spawn(spec: &PtyCommand, size: PtySize) -> io::Result<PtyProcess> {
        let (master, slave) = open(size)?;
        let mut command = spec.std_command();
        let controlling = !spec.capture_stdout;
        if spec.capture_stdout {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(slave.try_clone()?));
        } else {
            command
                .stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave.try_clone()?));
        }
        if spec.input.is_some() {
            command.stdin(Stdio::piped());
        }
        let slave_fd = slave.as_raw_fd();
        // SAFETY: only async-signal-safe calls (setsid, ioctl) run between fork and exec.
        unsafe {
            command.pre_exec(move || {
                // A new session: the process leads its own group, so signals reach everything
                // it starts, and it is detached from the caller's terminal.
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if controlling {
                    take_controlling_terminal(slave_fd)?;
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        super::feed_input(&mut child, spec.input.as_ref());
        // Only the process may keep the terminal's far side open, so its exit closes it.
        drop(command);
        drop(slave);
        let (events, receiver) = Events::new();
        let (done, finished) = mpsc::channel();
        let mut readers = 1;
        events.read(
            std::fs::File::from(master.try_clone()?),
            PtyEvent::Output,
            done.clone(),
        );
        if let Some(stdout) = child.stdout.take() {
            readers += 1;
            events.read(stdout, PtyEvent::Stdout, done);
        } else {
            drop(done);
        }
        let exited = Arc::new(AtomicBool::new(false));
        let pid = child.id();
        super::report_exit(events, readers, finished, Arc::clone(&exited), move || {
            let mut child = child;
            child.wait().map(super::PtyExit::from_status).ok()
        });
        Ok(PtyProcess {
            events: receiver,
            control: Control {
                master,
                pid,
                exited,
            },
            exited: false,
        })
    }
}

#[cfg(not(unix))]
mod platform {
    use std::io;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::time::Duration;

    use super::{Events, PtyCommand, PtyEvent, PtyExit, PtyProcess, PtySize};

    #[derive(Debug)]
    pub struct Control {
        child: Arc<Mutex<std::process::Child>>,
        pid: u32,
        exited: Arc<AtomicBool>,
    }

    impl Control {
        pub const fn id(&self) -> u32 {
            self.pid
        }

        // Pipes have no terminal size.
        #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
        pub fn resize(&self, _size: PtySize) -> io::Result<()> {
            Ok(())
        }

        pub fn interrupt(&self) -> io::Result<()> {
            self.kill()
        }

        pub fn kill(&self) -> io::Result<()> {
            if self.exited.load(Ordering::Acquire) {
                return Ok(());
            }
            self.child
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .kill()
        }
    }

    pub fn spawn(spec: &PtyCommand, _size: PtySize) -> io::Result<PtyProcess> {
        let mut command = spec.std_command();
        command
            .stdin(if spec.input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        super::feed_input(&mut child, spec.input.as_ref());
        let (events, receiver) = Events::new();
        let (done, finished) = mpsc::channel();
        let stdout_event = if spec.capture_stdout {
            PtyEvent::Stdout
        } else {
            PtyEvent::Output
        };
        if let Some(stdout) = child.stdout.take() {
            events.read(stdout, stdout_event, done.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            events.read(stderr, PtyEvent::Output, done);
        }
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let exited = Arc::new(AtomicBool::new(false));
        let waited = Arc::clone(&child);
        super::report_exit(events, 2, finished, Arc::clone(&exited), move || {
            loop {
                // Polled so `kill` can take the lock between checks.
                match waited
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .try_wait()
                {
                    Ok(Some(status)) => return Some(PtyExit::from_status(status)),
                    Ok(None) => {}
                    Err(_) => return None,
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(PtyProcess {
            events: receiver,
            control: Control { child, pid, exited },
            exited: false,
        })
    }
}

/// Wait for the process with `wait`, give its readers a moment to deliver the last output,
/// and report the exit. Output arriving later is dropped: the exit is the last event.
fn report_exit(
    events: Events,
    readers: usize,
    finished: mpsc::Receiver<()>,
    exited: Arc<AtomicBool>,
    wait: impl FnOnce() -> Option<PtyExit> + Send + 'static,
) {
    std::thread::spawn(move || {
        let exit = wait().unwrap_or(PtyExit {
            code: None,
            signal: None,
        });
        exited.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + DRAIN_GRACE;
        for _ in 0..readers {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if finished.recv_timeout(left).is_err() {
                break;
            }
        }
        events.closed.store(true, Ordering::Release);
        let _ = events.sender.send(PtyEvent::Exited(exit));
    });
}
