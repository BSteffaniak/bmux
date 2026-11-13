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
//! [`PtyProcess::writer`] writes to the terminal while the process runs: replies to terminal
//! queries, or a user's keystrokes.
//!
//! This crate only runs processes. Interpreting the terminal stream (for example with
//! `bmux_terminal_grid`) and presenting it are the caller's concern.
//!
//! What a platform supports is reported by [`PtyProcess::capabilities`]. On platforms without
//! pseudo-terminal support here (anything but Unix), processes run on plain pipes: output is
//! still streamed, but programs see no terminal, resizing has no effect, and nothing can be
//! written while they run.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

/// Events queued between the reader threads and the consumer before readers wait.
const QUEUE: usize = 64;
/// Bytes read per output event at most.
const CHUNK: usize = 16 * 1024;
/// How long output still arriving after the process exits is waited for, unless
/// [`PtyCommand::drain_grace`] says otherwise.
pub const DEFAULT_DRAIN_GRACE: Duration = Duration::from_millis(250);
/// How often blocked reads and writes look up whether they should stop.
#[cfg(unix)]
const POLL: Duration = Duration::from_millis(10);

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
    complete: bool,
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

    /// Whether every byte written to the terminal (and captured standard output) was
    /// delivered before this exit. `false` when output was still arriving when the drain
    /// grace ran out, typically from descendants that outlive the process and keep its
    /// terminal open; that output was dropped.
    #[must_use]
    pub const fn output_complete(self) -> bool {
        self.complete
    }

    fn from_status(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
            complete: true,
        }
    }
}

/// What a process's platform supports. Pipes stand in for a terminal where pseudo-terminals
/// are not supported, and then only output streaming works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// Each flag is an independent capability.
#[allow(clippy::struct_excessive_bools)]
pub struct PtyCapabilities {
    /// The process sees a terminal: colors, progress displays and line editing work.
    pub terminal: bool,
    /// [`PtyProcess::resize`] reaches the process.
    pub resize: bool,
    /// [`PtyProcess::writer`] can write while the process runs.
    pub input: bool,
    /// [`PtyProcess::interrupt`] interrupts rather than kills, and signals reach everything
    /// the process started.
    pub signals: bool,
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
    drain_grace: Duration,
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
            drain_grace: DEFAULT_DRAIN_GRACE,
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

    /// Wait at most `grace` after the process exits for output still arriving
    /// ([`DEFAULT_DRAIN_GRACE`] unless set). Output after that is dropped and the exit says so
    /// ([`PtyExit::output_complete`]).
    #[must_use]
    pub const fn drain_grace(mut self, grace: Duration) -> Self {
        self.drain_grace = grace;
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
    control: PtyControl,
    exited: bool,
}

/// Controls a running process from anywhere: a cloneable, thread-safe handle for a caller that
/// reads [`PtyProcess`] events on one thread and resizes, signals, writes or checks for the
/// exit on another. See [`PtyProcess::control`].
#[derive(Debug, Clone)]
pub struct PtyControl {
    platform: Arc<platform::Control>,
    size: Arc<Mutex<PtySize>>,
    status: Arc<Mutex<Option<PtyExit>>>,
}

impl PtyControl {
    fn new(platform: platform::Control, size: PtySize) -> Self {
        Self {
            platform: Arc::new(platform),
            size: Arc::new(Mutex::new(size)),
            status: Arc::new(Mutex::new(None)),
        }
    }

    /// Operating system process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.platform.id()
    }

    /// What the process's platform supports.
    #[must_use]
    pub fn capabilities(&self) -> PtyCapabilities {
        self.platform.capabilities()
    }

    /// The terminal's current size.
    #[must_use]
    pub fn size(&self) -> PtySize {
        *self.size.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How the process ended, as soon as it is reaped: before its remaining output is
    /// delivered and [`PtyEvent::Exited`] reports it. [`PtyExit::output_complete`] on it says
    /// whether all output had been delivered when it was taken.
    #[must_use]
    pub fn exit_status(&self) -> Option<PtyExit> {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// See [`PtyProcess::resize`].
    ///
    /// # Errors
    /// * [`io::Error`] when the terminal rejects the size.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        self.platform.resize(size)?;
        *self.size.lock().unwrap_or_else(PoisonError::into_inner) = size;
        Ok(())
    }

    /// See [`PtyProcess::interrupt`].
    ///
    /// # Errors
    /// * [`io::Error`] when the signal cannot be sent.
    pub fn interrupt(&self) -> io::Result<()> {
        self.platform.interrupt()
    }

    /// See [`PtyProcess::kill`].
    ///
    /// # Errors
    /// * [`io::Error`] when the signal cannot be sent.
    pub fn kill(&self) -> io::Result<()> {
        self.platform.kill()
    }

    /// See [`PtyProcess::stop_output`].
    pub fn stop_output(&self) {
        self.platform.stop_output();
    }

    /// See [`PtyProcess::writer`].
    ///
    /// # Errors
    /// * [`io::ErrorKind::Unsupported`] where processes run on pipes.
    /// * [`io::Error`] when the terminal cannot be written.
    pub fn writer(&self) -> io::Result<PtyWriter> {
        self.platform.writer()
    }
}

impl PtyProcess {
    /// Operating system process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.control.id()
    }

    /// A handle controlling this process from other threads.
    #[must_use]
    pub fn control(&self) -> PtyControl {
        self.control.clone()
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

    /// Stop delivering output: nothing more is delivered (it is still drained, so the process
    /// cannot block on it), and once the process has exited its exit is reported without
    /// waiting out the drain grace ([`PtyExit::output_complete`] is
    /// then `false`). For giving up on output that descendants keep producing, for example
    /// when a caller's deadline passes. The process itself is left running; see
    /// [`Self::kill`].
    pub fn stop_output(&self) {
        self.control.stop_output();
    }

    /// What this process's platform supports.
    #[must_use]
    pub fn capabilities(&self) -> PtyCapabilities {
        self.control.capabilities()
    }

    /// A handle that writes to the terminal while the process runs, as typing would: the
    /// process reads it from its terminal (with [`PtyCommand::input`], standard input is a
    /// pipe instead and does not see it). Handles may be cloned and sent to other threads;
    /// writes are queued in order and carried out on a thread of their own, so a process that
    /// does not read cannot block the caller for long.
    ///
    /// # Errors
    /// * [`io::ErrorKind::Unsupported`] where processes run on pipes
    ///   ([`PtyCapabilities::input`]).
    /// * [`io::Error`] when the terminal cannot be written.
    pub fn writer(&self) -> io::Result<PtyWriter> {
        self.control.writer()
    }

    /// Resize the terminal; the process is notified as by a terminal window resize.
    ///
    /// # Errors
    /// * [`io::Error`] when the terminal rejects the size.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        self.control.resize(size)
    }

    /// Interrupt the process and everything it started on its terminal, as `ctrl+c` would.
    /// After the process exits, this still reaches what it started while that keeps its
    /// terminal open.
    ///
    /// # Errors
    /// * [`io::Error`] when the signal cannot be sent.
    pub fn interrupt(&self) -> io::Result<()> {
        self.control.interrupt()
    }

    /// Kill the process and everything it started on its terminal. After the process exits,
    /// this still reaches what it started while that keeps its terminal open (for example a
    /// command a shell put in the background).
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

/// Writes to a process's terminal while it runs; see [`PtyProcess::writer`].
#[derive(Debug, Clone)]
pub struct PtyWriter {
    sender: mpsc::SyncSender<Vec<u8>>,
}

impl PtyWriter {
    /// Queue `bytes` to be written to the terminal, after everything queued before. Waits
    /// only while the queue is full.
    ///
    /// # Errors
    /// * [`io::ErrorKind::BrokenPipe`] once the terminal can no longer be written (the process
    ///   and everything that kept its terminal open have ended).
    pub fn write(&self, bytes: impl Into<Vec<u8>>) -> io::Result<()> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Ok(());
        }
        self.sender
            .send(bytes)
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
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

    /// Forward everything `reader` produces as events, then report on `done`. The reader
    /// should return end of file once [`Self::closed`] is set and the process has exited, or
    /// its thread lingers until the far side closes.
    fn read(
        &self,
        mut reader: impl Read + Send + 'static,
        event: fn(Vec<u8>) -> PtyEvent,
        done: mpsc::Sender<bool>,
    ) {
        let events = self.clone();
        std::thread::spawn(move || {
            let mut buffer = vec![0; CHUNK];
            loop {
                match reader.read(&mut buffer) {
                    // A pseudo-terminal reports a closed far side as an error (EIO).
                    Ok(0) | Err(_) => break,
                    // Once closed, output is still read (a process whose terminal output is
                    // not drained can block, even in its exit) but no longer delivered.
                    Ok(_) if events.closed.load(Ordering::Acquire) => {}
                    Ok(read) => {
                        if events.sender.send(event(buffer[..read].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
            // Reaching the end while not told to stop means everything was read.
            let _ = done.send(!events.closed.load(Ordering::Acquire));
        });
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};

    use super::{
        Events, Exit, PtyCapabilities, PtyCommand, PtyControl, PtyEvent, PtyProcess, PtySize,
        PtyWriter, QUEUE,
    };

    pub const CAPABILITIES: PtyCapabilities = PtyCapabilities {
        terminal: true,
        resize: true,
        input: true,
        signals: true,
    };

    #[derive(Debug)]
    pub struct Control {
        /// The terminal's near side, non-blocking.
        master: Arc<OwnedFd>,
        pid: u32,
        exited: Arc<AtomicBool>,
        /// Set once the exit is reported; the reader and writer threads then stop.
        closed: Arc<AtomicBool>,
        writer: Mutex<Option<PtyWriter>>,
    }

    impl Control {
        pub const fn id(&self) -> u32 {
            self.pid
        }

        // Uniform with platforms whose capabilities depend on the process.
        #[allow(clippy::unused_self)]
        pub const fn capabilities(&self) -> PtyCapabilities {
            CAPABILITIES
        }

        pub fn resize(&self, size: PtySize) -> io::Result<()> {
            set_size(self.master.as_raw_fd(), size)
        }

        pub fn interrupt(&self) -> io::Result<()> {
            self.signal(libc::SIGINT)
        }

        pub fn stop_output(&self) {
            self.closed.store(true, Ordering::Release);
        }

        pub fn kill(&self) -> io::Result<()> {
            self.signal(libc::SIGKILL)
        }

        // Fallible on every platform: where processes run on pipes, nothing can be written.
        #[allow(clippy::unnecessary_wraps)]
        pub fn writer(&self) -> io::Result<PtyWriter> {
            let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(writer) = writer.as_ref() {
                return Ok(writer.clone());
            }
            let (sender, queued) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
            let master = Arc::clone(&self.master);
            std::thread::spawn(move || {
                for bytes in queued {
                    if write_all(master.as_fd(), &bytes).is_err() {
                        break;
                    }
                }
            });
            let created = PtyWriter { sender };
            *writer = Some(created.clone());
            drop(writer);
            Ok(created)
        }

        /// Signal the process group the process leads (it started its own session).
        fn signal(&self, signal: libc::c_int) -> io::Result<()> {
            // Once the process is reaped its id may be reused, but not while its group still
            // has members: something holding the terminal open is taken as one.
            if self.exited.load(Ordering::Acquire) && !terminal_open(self.master.as_fd()) {
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

    impl Drop for Control {
        fn drop(&mut self) {
            self.closed.store(true, Ordering::Release);
        }
    }

    /// Whether anything still holds the terminal's far side open.
    fn terminal_open(master: BorrowedFd<'_>) -> bool {
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        // SAFETY: one valid pollfd, no waiting.
        let ready = unsafe { libc::poll(&raw mut poll, 1, 0) };
        ready == 0 || (poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) == 0
    }

    /// Wait until `fd` is ready for `events`, at most [`POLL`]; whether it is.
    fn wait_ready(fd: BorrowedFd<'_>, events: libc::c_short) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        let timeout = libc::c_int::try_from(super::POLL.as_millis()).unwrap_or(10);
        // SAFETY: one valid pollfd.
        match unsafe { libc::poll(&raw mut poll, 1, timeout) } {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
            0 => Ok(false),
            _ => Ok(true),
        }
    }

    /// Write all of `bytes` to the non-blocking `fd`, waiting while it is full.
    fn write_all(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            // SAFETY: `bytes` is valid for reads of its length.
            let written =
                unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if written >= 0 {
                bytes = &bytes[usize::try_from(written).unwrap_or(0)..];
                continue;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::WouldBlock => {
                    wait_ready(fd, libc::POLLOUT)?;
                }
                io::ErrorKind::Interrupted => {}
                _ => return Err(error),
            }
        }
        Ok(())
    }

    /// Reads a non-blocking descriptor, returning end of file once output is `closed` and the
    /// process has `exited`: a reader thread never outlives the reported exit, even while
    /// descendants keep the far side open, yet keeps draining while the process runs.
    struct Polled {
        fd: OwnedFd,
        closed: Arc<AtomicBool>,
        exited: Arc<AtomicBool>,
    }

    impl io::Read for Polled {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            loop {
                if self.closed.load(Ordering::Acquire) && self.exited.load(Ordering::Acquire) {
                    return Ok(0);
                }
                // SAFETY: `buffer` is valid for writes of its length.
                let read = unsafe {
                    libc::read(
                        self.fd.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if read >= 0 {
                    return Ok(usize::try_from(read).unwrap_or(0));
                }
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::WouldBlock => {
                        wait_ready(self.fd.as_fd(), libc::POLLIN)?;
                    }
                    io::ErrorKind::Interrupted => {}
                    _ => return Err(error),
                }
            }
        }
    }

    fn set_nonblocking(fd: RawFd) -> io::Result<()> {
        // SAFETY: fcntl on an owned, open descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: as above.
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
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

    /// Most descriptors scanned for [`close_inherited_on_exec`] where they cannot be closed in
    /// one call.
    const DESCRIPTOR_SCAN_LIMIT: libc::c_int = 65_536;

    /// Mark every descriptor above standard error close-on-exec, so descriptors the caller left
    /// inheritable do not leak into the program. Marked rather than closed: a failed exec still
    /// reports its error through the spawning library's own descriptor. Async-signal-safe.
    fn close_inherited_on_exec() {
        #[cfg(target_os = "linux")]
        {
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
            // SAFETY: close_range with the close-on-exec flag only changes descriptor flags.
            if unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    3,
                    libc::c_uint::MAX,
                    CLOSE_RANGE_CLOEXEC,
                )
            } == 0
            {
                return;
            }
        }
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit writes one rlimit.
        let highest = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } == 0 {
            libc::c_int::try_from(limit.rlim_cur)
                .unwrap_or(DESCRIPTOR_SCAN_LIMIT)
                .min(DESCRIPTOR_SCAN_LIMIT)
        } else {
            DESCRIPTOR_SCAN_LIMIT
        };
        for fd in 3..highest {
            // SAFETY: fcntl on a descriptor number; unused numbers fail harmlessly.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
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

    /// Set up the child between fork and exec. Async-signal-safe (signal, setsid, ioctl,
    /// getrlimit, fcntl, syscall only).
    fn prepare_child(slave_fd: RawFd, controlling: bool) -> io::Result<()> {
        // Start from default handling: dispositions the caller ignores (a terminal UI often
        // ignores interrupts) would otherwise be inherited. The signal mask and SIGPIPE are
        // already reset.
        for signal in [
            libc::SIGCHLD,
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTERM,
            libc::SIGALRM,
        ] {
            // SAFETY: restoring a valid signal's default disposition.
            unsafe { libc::signal(signal, libc::SIG_DFL) };
        }
        // A new session: the process leads its own group, so signals reach everything it
        // starts, and it is detached from the caller's terminal.
        // SAFETY: setsid has no memory-safety preconditions.
        if unsafe { libc::setsid() } == -1 {
            return Err(io::Error::last_os_error());
        }
        if controlling {
            take_controlling_terminal(slave_fd)?;
        }
        close_inherited_on_exec();
        Ok(())
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
        // SAFETY: `prepare_child` makes only async-signal-safe calls.
        unsafe {
            command.pre_exec(move || prepare_child(slave_fd, controlling));
        }
        let mut child = command.spawn()?;
        super::feed_input(&mut child, spec.input.as_ref());
        // Only the process may keep the terminal's far side open, so its exit closes it.
        drop(command);
        drop(slave);
        // Reads and writes poll, so neither can wedge on a terminal kept open elsewhere.
        set_nonblocking(master.as_raw_fd())?;
        let master = Arc::new(master);
        let exited = Arc::new(AtomicBool::new(false));
        let (events, receiver) = Events::new();
        let (done, finished) = mpsc::channel();
        let mut readers = 1;
        events.read(
            Polled {
                fd: master.try_clone()?,
                closed: Arc::clone(&events.closed),
                exited: Arc::clone(&exited),
            },
            PtyEvent::Output,
            done.clone(),
        );
        if let Some(stdout) = child.stdout.take() {
            let stdout = OwnedFd::from(stdout);
            set_nonblocking(stdout.as_raw_fd())?;
            readers += 1;
            events.read(
                Polled {
                    fd: stdout,
                    closed: Arc::clone(&events.closed),
                    exited: Arc::clone(&exited),
                },
                PtyEvent::Stdout,
                done,
            );
        } else {
            drop(done);
        }
        let closed = Arc::clone(&events.closed);
        let pid = child.id();
        let control = PtyControl::new(
            Control {
                master,
                pid,
                exited: Arc::clone(&exited),
                closed,
                writer: Mutex::new(None),
            },
            size,
        );
        super::report_exit(
            events,
            Exit {
                readers,
                finished,
                exited,
                grace: spec.drain_grace,
                status: Arc::clone(&control.status),
            },
            move || {
                let mut child = child;
                child.wait().map(super::PtyExit::from_status).ok()
            },
        );
        Ok(PtyProcess {
            events: receiver,
            control,
            exited: false,
        })
    }
}

/// Plain pipes standing in for a terminal: output is streamed, nothing more.
#[cfg(not(unix))]
mod pipes {
    use std::io;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::time::Duration;

    use super::{
        Events, Exit, PtyCapabilities, PtyCommand, PtyControl, PtyEvent, PtyExit, PtyProcess,
        PtySize, PtyWriter,
    };

    pub const CAPABILITIES: PtyCapabilities = PtyCapabilities {
        terminal: false,
        resize: false,
        input: false,
        signals: false,
    };

    #[derive(Debug)]
    pub struct Control {
        child: Arc<Mutex<std::process::Child>>,
        pid: u32,
        exited: Arc<AtomicBool>,
        closed: Arc<AtomicBool>,
    }

    impl Control {
        pub const fn id(&self) -> u32 {
            self.pid
        }

        #[allow(clippy::unused_self)]
        pub const fn capabilities(&self) -> PtyCapabilities {
            CAPABILITIES
        }

        // Pipes have no terminal size.
        #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
        pub fn resize(&self, _size: PtySize) -> io::Result<()> {
            Ok(())
        }

        pub fn interrupt(&self) -> io::Result<()> {
            self.kill()
        }

        pub fn stop_output(&self) {
            self.closed.store(true, Ordering::Release);
        }

        // Pipes stand in for the terminal; nothing is written while the process runs.
        #[allow(clippy::unused_self)]
        pub fn writer(&self) -> io::Result<PtyWriter> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
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

    pub fn spawn(spec: &PtyCommand, size: PtySize) -> io::Result<PtyProcess> {
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
        let closed = Arc::clone(&events.closed);
        let child = Arc::new(Mutex::new(child));
        let exited = Arc::new(AtomicBool::new(false));
        let waited = Arc::clone(&child);
        let control = PtyControl::new(
            super::platform::Control::Pipes(Control {
                child: Arc::clone(&child),
                pid,
                exited: Arc::clone(&exited),
                closed,
            }),
            size,
        );
        let exit = Exit {
            readers: 2,
            finished,
            exited,
            grace: spec.drain_grace,
            status: Arc::clone(&control.status),
        };
        super::report_exit(events, exit, move || {
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
            control,
            exited: false,
        })
    }
}

#[cfg(windows)]
mod conpty;

/// Where pseudo-terminals are not available, processes run on pipes; on Windows, on `ConPTY`
/// unless standard output is captured or input is fed (a console cannot redirect only some of
/// its streams).
#[cfg(not(unix))]
mod platform {
    use std::io;

    use super::{PtyCapabilities, PtyCommand, PtyProcess, PtySize, PtyWriter, pipes};

    #[derive(Debug)]
    pub enum Control {
        Pipes(pipes::Control),
        #[cfg(windows)]
        Conpty(super::conpty::Control),
    }

    impl Control {
        pub const fn id(&self) -> u32 {
            match self {
                Self::Pipes(control) => control.id(),
                #[cfg(windows)]
                Self::Conpty(control) => control.id(),
            }
        }

        pub const fn capabilities(&self) -> PtyCapabilities {
            match self {
                Self::Pipes(control) => control.capabilities(),
                #[cfg(windows)]
                Self::Conpty(control) => control.capabilities(),
            }
        }

        pub fn resize(&self, size: PtySize) -> io::Result<()> {
            match self {
                Self::Pipes(control) => control.resize(size),
                #[cfg(windows)]
                Self::Conpty(control) => control.resize(size),
            }
        }

        pub fn interrupt(&self) -> io::Result<()> {
            match self {
                Self::Pipes(control) => control.interrupt(),
                #[cfg(windows)]
                Self::Conpty(control) => control.interrupt(),
            }
        }

        pub fn kill(&self) -> io::Result<()> {
            match self {
                Self::Pipes(control) => control.kill(),
                #[cfg(windows)]
                Self::Conpty(control) => control.kill(),
            }
        }

        pub fn stop_output(&self) {
            match self {
                Self::Pipes(control) => control.stop_output(),
                #[cfg(windows)]
                Self::Conpty(control) => control.stop_output(),
            }
        }

        pub fn writer(&self) -> io::Result<PtyWriter> {
            match self {
                Self::Pipes(control) => control.writer(),
                #[cfg(windows)]
                Self::Conpty(control) => control.writer(),
            }
        }
    }

    pub fn spawn(spec: &PtyCommand, size: PtySize) -> io::Result<PtyProcess> {
        #[cfg(windows)]
        if !spec.capture_stdout && spec.input.is_none() {
            return super::conpty::spawn(spec, size);
        }
        pipes::spawn(spec, size)
    }
}

/// How the threads of one process learn that it ended.
struct Exit {
    readers: usize,
    /// Whether each reader read to the end of its stream.
    finished: mpsc::Receiver<bool>,
    exited: Arc<AtomicBool>,
    grace: Duration,
    /// Where the status is published as soon as the process is reaped.
    status: Arc<Mutex<Option<PtyExit>>>,
}

/// Wait for the process with `wait`, give its readers `exit.grace` to deliver the last output,
/// and report the exit. Output arriving later is dropped (the exit says so): the exit is the
/// last event.
fn report_exit(
    events: Events,
    exit: Exit,
    wait: impl FnOnce() -> Option<PtyExit> + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut status = wait().unwrap_or(PtyExit {
            code: None,
            signal: None,
            complete: true,
        });
        *exit.status.lock().unwrap_or_else(PoisonError::into_inner) = Some(PtyExit {
            complete: false,
            ..status
        });
        exit.exited.store(true, Ordering::Release);
        let deadline = Instant::now() + exit.grace;
        for _ in 0..exit.readers {
            let left = deadline.saturating_duration_since(Instant::now());
            match exit.finished.recv_timeout(left) {
                Ok(true) => {}
                Ok(false) => status.complete = false,
                Err(_) => {
                    status.complete = false;
                    break;
                }
            }
        }
        events.closed.store(true, Ordering::Release);
        *exit.status.lock().unwrap_or_else(PoisonError::into_inner) = Some(status);
        let _ = events.sender.send(PtyEvent::Exited(status));
    });
}
