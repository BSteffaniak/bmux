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
use std::time::Duration;

use switchy_thread::sync::atomic::{AtomicBool, Ordering};
use switchy_thread::sync::{Arc, Mutex, PoisonError, mpsc};

pub use switchy_pty::PtySize;
#[cfg(any(unix, windows))]
pub use switchy_pty::{
    Behavior, ChildExit, PtyChild, PtyChildKiller, PtyInput, PtyMaster, PtyPair, PtyReader,
    PtySlave, SpawnCommand, behavior,
};

/// Events queued between the reader threads and the consumer before readers wait.
const QUEUE: usize = 64;
/// Bytes read per output event at most.
const CHUNK: usize = 16 * 1024;
/// How long output still arriving after the process exits is waited for, unless
/// [`PtyCommand::drain_grace`] says otherwise.
pub const DEFAULT_DRAIN_GRACE: Duration = Duration::from_millis(250);
/// How often blocked reads and writes look up whether they should stop.
#[cfg(any(unix, feature = "simulator"))]
const POLL: Duration = Duration::from_millis(10);

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

    #[cfg(any(unix, feature = "simulator"))]
    fn from_child_exit(exit: &ChildExit) -> Self {
        Self {
            code: if exit.signal_number().is_some() {
                None
            } else {
                Some(i32::from_ne_bytes(exit.exit_code().to_ne_bytes()))
            },
            signal: exit.signal_number(),
            complete: true,
        }
    }

    #[cfg(not(any(unix, feature = "simulator")))]
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

    /// The process to start on a terminal: the caller's environment with this command's
    /// changes, and `TERM` unless set.
    #[cfg(any(unix, feature = "simulator"))]
    fn spawn_command(&self) -> SpawnCommand {
        let mut command = SpawnCommand::new(&self.program);
        command.args(&self.args);
        if let Some(cwd) = &self.cwd {
            command.cwd(cwd);
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

    // native-only: begin.
    #[cfg(not(any(unix, feature = "simulator")))]
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
    // native-only: end.
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
#[cfg(not(any(unix, feature = "simulator")))]
fn feed_input(child: &mut std::process::Child, input: Option<&Vec<u8>>) {
    if let (Some(input), Some(mut pipe)) = (input, child.stdin.take()) {
        let input = input.clone();
        spawn_named("bmux-pty-input", move || {
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
        spawn_named("bmux-pty-reader", move || {
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

/// Every process on `switchy_pty`: Unix natively, and the simulation under every personality.
/// What differs between systems follows [`switchy_pty::behavior`]:
///
/// * **Process groups** (Unix): the process leads its own session on a terminal; interrupting
///   and killing signal its group, and only the process may keep the terminal open.
/// * **Jobs** (Windows): the process runs in a kill-on-close job, so killing (and interrupting,
///   which a console cannot be sent from outside) ends everything it started. Its console keeps
///   its own cursor, as nothing answers a startup query, and is closed once the process exits,
///   as a console's output ends only then. With its output captured or its input fed, it runs
///   without the console, on pipes (a console cannot redirect only some of its streams).
#[cfg(any(unix, feature = "simulator"))]
mod platform {
    use std::io;

    use switchy_pty::{
        PtyChild, PtyInput, PtyJob, PtyMaster, PtyOptions, PtyPair, PtyPipeReader, PtyReader,
        Signal, SpawnCommand, Stdio,
    };
    use switchy_thread::sync::atomic::{AtomicBool, Ordering};
    use switchy_thread::sync::{Arc, Mutex, PoisonError, mpsc};

    use super::{
        Events, Exit, POLL, PtyCapabilities, PtyCommand, PtyControl, PtyEvent, PtyExit, PtyProcess,
        PtySize, PtyWriter, QUEUE, spawn_named,
    };

    /// A terminal: everything works.
    const TERMINAL: PtyCapabilities = PtyCapabilities {
        terminal: true,
        resize: true,
        input: true,
        signals: true,
    };

    /// How the process is ended.
    #[derive(Debug)]
    enum Ending {
        /// Signal its process group (Unix).
        Group,
        /// Terminate its job (Windows); dropped with the control, which ends the job too.
        Job(PtyJob),
    }

    #[derive(Debug)]
    pub struct Control {
        /// The terminal's near side, non-blocking; `None` once closed (a console after the
        /// process exits) or for a process on pipes.
        master: Arc<Mutex<Option<Arc<PtyMaster>>>>,
        pid: u32,
        capabilities: PtyCapabilities,
        ending: Ending,
        exited: Arc<AtomicBool>,
        /// Set once the exit is reported; the reader and writer threads then stop.
        closed: Arc<AtomicBool>,
        writer: Mutex<Option<PtyWriter>>,
        /// Held until the last control handle is gone; the exited process is reaped only
        /// then, so its process group id cannot be reused while it may still be signalled.
        _reap_guard: mpsc::Sender<()>,
    }

    impl Control {
        pub const fn id(&self) -> u32 {
            self.pid
        }

        pub const fn capabilities(&self) -> PtyCapabilities {
            self.capabilities
        }

        fn master(&self) -> Option<Arc<PtyMaster>> {
            self.master
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        pub fn resize(&self, size: PtySize) -> io::Result<()> {
            // A closed console, or pipes: nothing to resize.
            self.master().map_or(Ok(()), |master| master.resize(size))
        }

        pub fn interrupt(&self) -> io::Result<()> {
            match &self.ending {
                Ending::Group => self.signal(Signal::Interrupt),
                // A console cannot be interrupted from outside: stopped, as killing does.
                Ending::Job(job) => job.terminate(),
            }
        }

        pub fn stop_output(&self) {
            self.closed.store(true, Ordering::Release);
        }

        pub fn kill(&self) -> io::Result<()> {
            match &self.ending {
                Ending::Group => self.signal(Signal::Kill),
                Ending::Job(job) => job.terminate(),
            }
        }

        pub fn writer(&self) -> io::Result<PtyWriter> {
            let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(writer) = writer.as_ref() {
                return Ok(writer.clone());
            }
            let master = self
                .master()
                .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
            let mut input = master.clone_writer()?;
            let (sender, queued) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
            spawn_named("bmux-pty-writer", move || {
                for bytes in queued {
                    if write_all(&mut input, &bytes).is_err() {
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
        fn signal(&self, signal: Signal) -> io::Result<()> {
            // The process is not reaped while this handle exists (see `_reap_guard`), so its
            // id, which names the group, cannot belong to another process yet. After the exit
            // with nothing holding the terminal there is nothing left to signal.
            if self.exited.load(Ordering::Acquire)
                && !self.master().is_some_and(|master| master.has_peer())
            {
                return Ok(());
            }
            switchy_pty::signal_group(self.pid, signal)
        }
    }

    impl Drop for Control {
        fn drop(&mut self) {
            self.closed.store(true, Ordering::Release);
        }
    }

    /// Write all of `bytes` to the non-blocking terminal, waiting while it is full.
    fn write_all(input: &mut PtyInput, mut bytes: &[u8]) -> io::Result<()> {
        use std::io::Write as _;
        while !bytes.is_empty() {
            match input.write(bytes) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(written) => bytes = &bytes[written..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    input.wait_writable(POLL)?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// A non-blocking stream a reader thread drains.
    enum Source {
        Terminal(PtyReader),
        Pipe(PtyPipeReader),
    }

    /// Reads a non-blocking stream, returning end of file once output is `closed` and the
    /// process has `exited`: a reader thread never outlives the reported exit, even while
    /// descendants keep the far side open, yet keeps draining while the process runs.
    struct Polled {
        source: Source,
        closed: Arc<AtomicBool>,
        exited: Arc<AtomicBool>,
    }

    impl io::Read for Polled {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            loop {
                if self.closed.load(Ordering::Acquire) && self.exited.load(Ordering::Acquire) {
                    return Ok(0);
                }
                let read = match &mut self.source {
                    Source::Terminal(reader) => reader.read(buffer),
                    Source::Pipe(reader) => reader.read(buffer),
                };
                match read {
                    Ok(read) => return Ok(read),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        match &self.source {
                            Source::Terminal(reader) => reader.wait_readable(POLL)?,
                            Source::Pipe(reader) => reader.wait_readable(POLL)?,
                        };
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    /// The command `spec` describes, its streams on the terminal, on pipes alone (`on_pipes`),
    /// or with standard output captured or input fed.
    fn command(spec: &PtyCommand, on_pipes: bool) -> SpawnCommand {
        let mut command = spec.spawn_command();
        if on_pipes {
            command
                .stdin(if spec.input.is_some() {
                    Stdio::Piped
                } else {
                    Stdio::Null
                })
                .stdout(Stdio::Piped)
                .stderr(Stdio::Piped)
                .controlling_terminal(false);
        } else {
            if spec.capture_stdout {
                command
                    .stdin(Stdio::Null)
                    .stdout(Stdio::Piped)
                    .stderr(Stdio::Terminal)
                    .controlling_terminal(false);
            }
            if spec.input.is_some() {
                command.stdin(Stdio::Piped);
            }
        }
        command
    }

    /// Start a reader thread for each of the process's output streams: the terminal (when it
    /// has one) and its stdout and stderr pipes (standard output reported as
    /// [`PtyEvent::Stdout`] when `capture`). How many, and where each reports finishing.
    fn read_streams(
        events: &Events,
        exited: &Arc<AtomicBool>,
        terminal: Option<&PtyMaster>,
        child: &mut PtyChild,
        capture: bool,
    ) -> io::Result<(usize, mpsc::Receiver<bool>)> {
        let (done, finished) = mpsc::channel();
        let mut readers = 0;
        let mut read = |source: Source, event: fn(Vec<u8>) -> PtyEvent| {
            readers += 1;
            events.read(
                Polled {
                    source,
                    closed: Arc::clone(&events.closed),
                    exited: Arc::clone(exited),
                },
                event,
                done.clone(),
            );
        };
        if let Some(terminal) = terminal {
            read(
                Source::Terminal(terminal.try_clone_reader()?),
                PtyEvent::Output,
            );
        }
        if let Some(stdout) = child.take_stdout() {
            stdout.set_nonblocking(true)?;
            let event: fn(Vec<u8>) -> PtyEvent = if capture {
                PtyEvent::Stdout
            } else {
                PtyEvent::Output
            };
            read(Source::Pipe(stdout), event);
        }
        if let Some(stderr) = child.take_stderr() {
            stderr.set_nonblocking(true)?;
            read(Source::Pipe(stderr), PtyEvent::Output);
        }
        Ok((readers, finished))
    }

    pub fn spawn(spec: &PtyCommand, size: PtySize) -> io::Result<PtyProcess> {
        let behavior = switchy_pty::behavior();
        let jobs = !behavior.process_groups;
        // Windows: a console cannot redirect only some streams, so with any redirected the
        // process runs on pipes alone.
        let on_pipes = behavior.console_on_all_streams_or_none
            && (spec.capture_stdout || spec.input.is_some());
        let PtyPair { slave, master } = PtyPair::open_with(
            size,
            PtyOptions {
                // Nothing here answers a console's startup cursor query, and typed bytes are
                // passed on as they are (the terminal they come from is not ours to switch to
                // win32 input mode).
                inherit_cursor: false,
                win32_input_mode: false,
            },
        )?;
        let command = command(spec, on_pipes);
        let (mut child, ending) = if jobs {
            let job = PtyJob::new()?;
            (slave.spawn_in(&command, &job)?, Ending::Job(job))
        } else {
            (slave.spawn(&command)?, Ending::Group)
        };
        if let (Some(input), Some(mut pipe)) = (spec.input.clone(), child.take_stdin()) {
            // On its own thread, so a full pipe cannot block the caller; then closed.
            spawn_named("bmux-pty-input", move || {
                use std::io::Write as _;
                let _ = pipe.write_all(&input);
            });
        }
        // Only the process may keep the terminal's far side open, so its exit closes it.
        drop(command);
        drop(slave);
        let exited = Arc::new(AtomicBool::new(false));
        let (events, receiver) = Events::new();
        let master = if on_pipes {
            // The process has no terminal: its pipes are all there is.
            drop(master);
            None
        } else {
            // Reads and writes poll, so neither can wedge on a terminal kept open elsewhere.
            master.set_nonblocking(true)?;
            Some(Arc::new(master))
        };
        let (readers, finished) = read_streams(
            &events,
            &exited,
            master.as_deref(),
            &mut child,
            spec.capture_stdout,
        )?;
        let capabilities = if on_pipes {
            PtyCapabilities {
                terminal: false,
                resize: false,
                input: false,
                signals: false,
            }
        } else {
            PtyCapabilities {
                signals: !jobs,
                ..TERMINAL
            }
        };
        let closed = Arc::clone(&events.closed);
        let pid = child.id();
        let master = Arc::new(Mutex::new(master));
        let (reap_guard, reap_when_released) = mpsc::channel::<()>();
        let control = PtyControl::new(
            Control {
                master: Arc::clone(&master),
                pid,
                capabilities,
                ending,
                exited: Arc::clone(&exited),
                closed,
                writer: Mutex::new(None),
                _reap_guard: reap_guard,
            },
            size,
        );
        report(
            events,
            Exit {
                readers,
                finished,
                exited,
                grace: spec.drain_grace,
                status: Arc::clone(&control.status),
            },
            child,
            behavior.output_ends_with_console.then_some(master),
            reap_when_released,
        );
        Ok(PtyProcess {
            events: receiver,
            control,
            exited: false,
        })
    }

    /// Report `child`'s exit as it happens, closing `console` then (a console's output ends
    /// only once it is closed), and reap it once `reap_when_released` says no control handle
    /// is left.
    fn report(
        events: Events,
        exit: Exit,
        child: PtyChild,
        console: Option<Arc<Mutex<Option<Arc<PtyMaster>>>>>,
        reap_when_released: mpsc::Receiver<()>,
    ) {
        // Reported as soon as it exits, but reaped only once no control handle could still
        // signal its group: until then the zombie keeps its id (and the group's) from reuse.
        let waiting = Arc::new(Mutex::new(Some(child)));
        let reaping = Arc::clone(&waiting);
        super::report_exit(
            events,
            exit,
            move || {
                let mut child = waiting
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()?;
                let exit = child.wait_unreaped().or_else(|_| child.wait());
                *waiting.lock().unwrap_or_else(PoisonError::into_inner) = Some(child);
                if let Some(console) = &console {
                    console
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take();
                }
                exit.ok().map(|exit| PtyExit::from_child_exit(&exit))
            },
            move || {
                // Every sender is the control handle's: this returns once they are all gone.
                let _ = reap_when_released.recv();
                let child = reaping
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(mut child) = child {
                    let _ = child.wait();
                }
            },
        );
    }
}

// native-only: begin. Windows and other non-Unix hosts are not simulated, so this code may use
// std threads and channels directly.

/// Plain pipes standing in for a terminal: output is streamed, nothing more.
#[cfg(not(any(unix, feature = "simulator")))]
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
        super::report_exit(
            events,
            exit,
            move || loop {
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
            },
            || {},
        );
        Ok(PtyProcess {
            events: receiver,
            control,
            exited: false,
        })
    }
}

#[cfg(all(windows, not(feature = "simulator")))]
mod conpty;

/// Where pseudo-terminals are not available, processes run on pipes; on Windows, on `ConPTY`
/// unless standard output is captured or input is fed (a console cannot redirect only some of
/// its streams).
#[cfg(not(any(unix, feature = "simulator")))]
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

// native-only: end.

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
/// Then run `after` on the same thread.
fn report_exit(
    events: Events,
    exit: Exit,
    wait: impl FnOnce() -> Option<PtyExit> + Send + 'static,
    after: impl FnOnce() + Send + 'static,
) {
    spawn_named("bmux-pty-exit", move || {
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
        let deadline = switchy_time::instant_now() + exit.grace;
        for _ in 0..exit.readers {
            let left = deadline.saturating_duration_since(switchy_time::instant_now());
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
        drop(events);
        after();
    });
}

/// Start a named thread, as [`std::thread::spawn`] does. Names show in traces and in
/// simulation deadlock reports.
fn spawn_named<F, T>(name: &str, f: F) -> switchy_thread::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    switchy_thread::thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .expect("failed to spawn thread")
}
