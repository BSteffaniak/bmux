//! Execution-private ordered persistence. Never acquire capacity under the
//! execution order gate, geometry locks, parser locks, or the runtime manager.
//!
//! Coordinator must declare `mod capture_writer;` and supervise `WriterOwner`
//! alongside the raw capture reader. No terminal parsing or protocol replies
//! belong here. Existing durable APIs wait for tickets; live output does not.
use super::durable_capture::{Boundary, DurableCapture};
use bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::TerminalCaptureEventKind;
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

const OUTPUT_SLOTS: usize = 32;
const CONTROL_SLOTS: usize = 8;
const CHUNK_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ticket {
    pub(super) sequence: u64,
    pub(super) raw_end: u64,
}

#[derive(Clone, Debug)]
pub(super) struct Progress {
    pub(super) admitted: Ticket,
    pub(super) projected: Ticket,
    pub(super) committed: Ticket,
    pub(super) boundary: Boundary,
    pub(super) failure: Option<String>,
}

#[derive(Clone, Copy)]
enum Class {
    Output,
    Control,
    End,
}

enum Record {
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    End(Option<String>),
}

struct Command {
    ticket: Ticket,
    mono_ns: u64,
    record: Record,
    class: Class,
}

struct State {
    queue: VecDeque<Command>,
    outputs: usize,
    controls: usize,
    ends: usize,
    closed: bool,
    progress: Progress,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    origin: std::time::Instant,
    changes: tokio::sync::watch::Sender<()>,
}

#[derive(Clone)]
pub(super) struct CaptureWriter(Arc<Shared>);

/// A capacity claim, including the in-flight disk write. Dropping an unused
/// reservation returns capacity. Admission consumes it without blocking on I/O.
pub(super) struct Reservation {
    writer: CaptureWriter,
    class: Class,
    consumed: bool,
}

/// Dropping ownership stops admission and transfers the actual join to the
/// persistent runtime. It never detaches a writer or manufactures clean EOF.
pub(super) struct WriterOwner {
    writer: CaptureWriter,
    thread: Option<JoinHandle<()>>,
    runtime: tokio::runtime::Handle,
}

fn failed(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn bounded_message(message: &str) -> String {
    message.chars().take(1024).collect()
}

impl CaptureWriter {
    pub(super) fn start(capture: DurableCapture) -> io::Result<(Self, WriterOwner)> {
        Self::start_with(capture, || {})
    }

    fn start_with(
        capture: DurableCapture,
        before_run: impl FnOnce() + Send + 'static,
    ) -> io::Result<(Self, WriterOwner)> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let boundary = capture.committed_boundary();
        if boundary.bytes != 0
            || boundary.stream_bytes != 0
            || boundary.eof
            || boundary.error.is_some()
        {
            return Err(failed(
                "writer requires a fresh execution capture; append recovery unsupported",
            ));
        }
        let zero = Ticket {
            sequence: 0,
            raw_end: 0,
        };
        let writer = Self(Arc::new(Shared {
            state: Mutex::new(State {
                queue: VecDeque::with_capacity(OUTPUT_SLOTS + CONTROL_SLOTS + 1),
                outputs: 0,
                controls: 0,
                ends: 0,
                closed: false,
                progress: Progress {
                    admitted: zero,
                    projected: zero,
                    committed: zero,
                    boundary,
                    failure: None,
                },
            }),
            changed: Condvar::new(),
            origin: std::time::Instant::now(),
            changes: tokio::sync::watch::channel(()).0,
        }));
        let worker = writer.clone();
        let thread = std::thread::Builder::new()
            .name("bmux-capture-writer".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    before_run();
                    worker.run(capture);
                }));
                if let Err(panic) = result {
                    worker.fail("capture writer panicked");
                    std::panic::resume_unwind(panic);
                }
            })?;
        let owner = WriterOwner {
            writer: writer.clone(),
            thread: Some(thread),
            runtime,
        };
        Ok((writer, owner))
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, State>> {
        self.0
            .state
            .lock()
            .map_err(|_| failed("capture writer state poisoned"))
    }

    fn reserve(&self, class: Class) -> io::Result<Reservation> {
        let mut state = self.lock()?;
        loop {
            Self::check(&state)?;
            let available = match class {
                Class::Output => state.outputs < OUTPUT_SLOTS,
                Class::Control => state.controls < CONTROL_SLOTS,
                Class::End => state.ends == 0,
            };
            if available {
                match class {
                    Class::Output => state.outputs += 1,
                    Class::Control => state.controls += 1,
                    Class::End => state.ends += 1,
                }
                return Ok(Reservation {
                    writer: self.clone(),
                    class,
                    consumed: false,
                });
            }
            state = self
                .0
                .changed
                .wait(state)
                .map_err(|_| failed("capture capacity wait poisoned"))?;
        }
    }

    pub(super) fn reserve_output(&self) -> io::Result<Reservation> {
        self.reserve(Class::Output)
    }
    pub(super) fn reserve_resize(&self) -> io::Result<Reservation> {
        self.reserve(Class::Control)
    }
    pub(super) fn reserve_end(&self) -> io::Result<Reservation> {
        self.reserve(Class::End)
    }

    fn check(state: &State) -> io::Result<()> {
        if let Some(error) = &state.progress.failure {
            return Err(failed(error.clone()));
        }
        if state.closed {
            return Err(failed("capture admission closed"));
        }
        Ok(())
    }

    pub(super) fn check_open(&self) -> io::Result<()> {
        let state = self.lock()?;
        Self::check(&state)
    }

    pub(super) fn progress(&self) -> io::Result<Progress> {
        Ok(self.lock()?.progress.clone())
    }

    /// Coalesced bounded wakeups for execution status/cleanup observers. Read
    /// progress after subscribing; notifications are not an event history.
    pub(super) fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.0.changes.subscribe()
    }

    fn notify(&self) {
        self.0.changed.notify_all();
        self.0.changes.send_replace(());
    }

    /// Called after coherent retained output publication, not at raw read time.
    /// The coordinator must pass the actual ordered ticket, never a durable byte
    /// counter. Resize uses `admitted.raw_end` as its projection drain fence.
    pub(super) fn projected(&self, ticket: Ticket) -> io::Result<()> {
        let mut state = self.lock()?;
        if ticket.sequence < state.progress.projected.sequence
            || ticket.sequence > state.progress.admitted.sequence
            || ticket.raw_end < state.progress.projected.raw_end
            || ticket.raw_end > state.progress.admitted.raw_end
        {
            return Err(failed("invalid projection progress"));
        }
        state.progress.projected = ticket;
        self.notify();
        Ok(())
    }

    /// Resize holds the execution order gate, but no parser/geometry/manager
    /// locks, while the already-admitted finite output prefix drains.
    pub(super) fn wait_projected_raw(
        &self,
        raw_end: u64,
        timeout: std::time::Duration,
    ) -> io::Result<()> {
        let state = self.lock()?;
        if raw_end > state.progress.admitted.raw_end {
            return Err(failed("projection fence was not admitted"));
        }
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.progress.projected.raw_end < raw_end && state.progress.failure.is_none()
            })
            .map_err(|_| failed("projection drain wait poisoned"))?;
        if let Some(error) = &state.progress.failure {
            return Err(failed(error.clone()));
        }
        if state.progress.projected.raw_end < raw_end {
            return Err(failed("projection drain timed out"));
        }
        Ok(())
    }

    /// Must be called with no order/parser/manager locks. A committed ticket
    /// remains acknowledged even if a later record fails.
    pub(super) fn wait_committed(&self, ticket: Ticket) -> io::Result<Boundary> {
        let mut state = self.lock()?;
        if ticket.sequence > state.progress.admitted.sequence
            || ticket.raw_end > state.progress.admitted.raw_end
        {
            return Err(failed("ticket was not admitted"));
        }
        loop {
            if state.progress.committed.sequence >= ticket.sequence {
                return Ok(state.progress.boundary.clone());
            }
            if let Some(error) = &state.progress.failure {
                return Err(failed(error.clone()));
            }
            state = self
                .0
                .changed
                .wait(state)
                .map_err(|_| failed("capture commit wait poisoned"))?;
        }
    }

    /// Stops authority immediately. Persisting an error can itself fail; an
    /// unchanged nonterminal disk boundary is never interpreted as clean EOF.
    pub(super) fn fail(&self, reason: &str) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.progress.failure.is_none() {
            state.progress.failure = Some(bounded_message(reason));
        }
        state.closed = true;
        state.queue.clear();
        self.notify();
    }

    fn run(&self, mut capture: DurableCapture) {
        loop {
            let command = {
                let mut state = match self.lock() {
                    Ok(state) => state,
                    Err(error) => {
                        self.fail(&error.to_string());
                        return;
                    }
                };
                while state.queue.is_empty() && state.progress.failure.is_none() {
                    state = match self.0.changed.wait(state) {
                        Ok(state) => state,
                        Err(error) => {
                            drop(error.into_inner());
                            self.fail("capture writer wait poisoned");
                            return;
                        }
                    };
                }
                if state.progress.failure.is_some() {
                    return;
                }
                state.queue.pop_front().expect("nonempty capture queue")
            };
            let terminal = matches!(&command.record, Record::End(_));
            let result = match command.record {
                Record::Output(data) => {
                    let count = u64::try_from(data.len()).expect("bounded chunk");
                    capture.append_admitted(
                        TerminalCaptureEventKind::Output { data },
                        count,
                        command.mono_ns,
                    )
                }
                Record::Resize { cols, rows } => capture.append_admitted(
                    TerminalCaptureEventKind::Resize { cols, rows },
                    0,
                    command.mono_ns,
                ),
                Record::End(error) => capture.end(error),
            };
            if let Err(error) = result {
                self.fail(&format!("capture persistence failed: {error}"));
                return;
            }
            let mut state = match self.lock() {
                Ok(state) => state,
                Err(error) => {
                    self.fail(&error.to_string());
                    return;
                }
            };
            state.progress.committed = command.ticket;
            state.progress.boundary = capture.committed_boundary();
            release(&mut state, command.class);
            self.notify();
            if terminal {
                return;
            }
        }
    }
}

fn release(state: &mut State, class: Class) {
    match class {
        Class::Output => state.outputs -= 1,
        Class::Control => state.controls -= 1,
        Class::End => state.ends -= 1,
    }
}

impl Reservation {
    // Caller holds the execution's single capture_order gate for every admit.
    // No capacity wait, disk operation, or projection send occurs in this call.
    fn admit(mut self, record: Record) -> io::Result<Ticket> {
        let mut state = self.writer.lock()?;
        CaptureWriter::check(&state)?;
        let bytes = match &record {
            Record::Output(data) => data.len(),
            _ => 0,
        };
        let ticket = Ticket {
            sequence: state
                .progress
                .admitted
                .sequence
                .checked_add(1)
                .ok_or_else(|| failed("capture sequence overflow"))?,
            raw_end: state
                .progress
                .admitted
                .raw_end
                .checked_add(u64::try_from(bytes).map_err(io::Error::other)?)
                .ok_or_else(|| failed("capture raw length overflow"))?,
        };
        if ticket.raw_end > 64 * 1024 * 1024 {
            drop(state);
            self.writer.fail("capture capacity exhausted");
            return Err(failed("capture capacity exhausted"));
        }
        let mono_ns =
            u64::try_from(self.writer.0.origin.elapsed().as_nanos()).map_err(io::Error::other)?;
        state.closed = matches!(&record, Record::End(_));
        state.progress.admitted = ticket;
        state.queue.push_back(Command {
            ticket,
            mono_ns,
            record,
            class: self.class,
        });
        self.consumed = true;
        self.writer.notify();
        Ok(ticket)
    }

    pub(super) fn output(self, bytes: Vec<u8>) -> io::Result<Ticket> {
        if !matches!(self.class, Class::Output) || bytes.is_empty() || bytes.len() > CHUNK_BYTES {
            return Err(failed(
                "output requires an output reservation and 1..=8192 bytes",
            ));
        }
        self.admit(Record::Output(bytes))
    }
    pub(super) fn resize(self, cols: u16, rows: u16) -> io::Result<Ticket> {
        if !matches!(self.class, Class::Control)
            || cols == 0
            || rows == 0
            || cols > 1000
            || rows > 1000
        {
            return Err(failed("invalid reserved resize"));
        }
        self.admit(Record::Resize { cols, rows })
    }
    pub(super) fn end(self, error: Option<String>) -> io::Result<Ticket> {
        if !matches!(self.class, Class::End) {
            return Err(failed("end requires terminal reservation"));
        }
        self.admit(Record::End(error.map(|error| bounded_message(&error))))
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.consumed {
            let mut state = self
                .writer
                .0
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            release(&mut state, self.class);
            self.writer.0.changed.notify_all();
        }
    }
}

impl WriterOwner {
    /// Transfers the actual join to existing coordinator supervision. Keep a
    /// `CaptureWriter` handle there so cancellation can call fail before joining.
    pub(super) fn take(&mut self) -> JoinHandle<()> {
        self.thread.take().expect("capture writer owner")
    }
}

impl Drop for WriterOwner {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.writer
                .fail("capture writer owner cancelled before supervised finish");
            self.runtime.spawn(async move {
                let mut join = super::supervision_join(&mut [Some(thread)]);
                if let Err(error) = (&mut join)
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
                {
                    tracing::error!(%error, "capture writer cleanup failed");
                }
            });
        }
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;

    fn fresh() -> (tempfile::TempDir, CaptureWriter, WriterOwner) {
        let root = tempfile::tempdir().unwrap();
        let (writer, owner) =
            CaptureWriter::start(DurableCapture::create(root.path()).unwrap()).unwrap();
        (root, writer, owner)
    }

    #[tokio::test]
    async fn live_admission_precedes_storage_and_timestamps_are_not_writer_time() {
        let root = tempfile::tempdir().unwrap();
        let (release, blocked) = std::sync::mpsc::channel();
        let (writer, mut owner) =
            CaptureWriter::start_with(DurableCapture::create(root.path()).unwrap(), move || {
                blocked.recv().unwrap();
            })
            .unwrap();
        let first = writer
            .reserve_output()
            .unwrap()
            .output(b"live".to_vec())
            .unwrap();
        writer.projected(first).unwrap();
        let progress = writer.progress().unwrap();
        assert_eq!(progress.admitted, first);
        assert_eq!(progress.projected, first);
        assert_eq!(progress.committed.sequence, 0);
        assert_eq!(progress.boundary.bytes, 0);
        let admitted_time = writer
            .0
            .state
            .lock()
            .unwrap()
            .queue
            .front()
            .unwrap()
            .mono_ns;
        let end = writer.reserve_end().unwrap().end(None).unwrap();
        release.send(()).unwrap();
        writer.wait_committed(end).unwrap();
        owner.take().join().unwrap();
        let (_, bytes) = DurableCapture::read_events(root.path(), 0, 262_144).unwrap();
        let length = usize::try_from(u32::from_le_bytes(bytes[..4].try_into().unwrap())).unwrap();
        let event: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::TerminalCaptureEvent =
            serde_json::from_slice(&bytes[4..4 + length]).unwrap();
        assert_eq!(event.mono_ns, admitted_time);
    }

    #[tokio::test]
    async fn output_resize_end_are_ordered_and_eof_requires_commit() {
        let (root, writer, mut owner) = fresh();
        let first = writer
            .reserve_output()
            .unwrap()
            .output(b"before".to_vec())
            .unwrap();
        writer.projected(first).unwrap();
        let resize = writer.reserve_resize().unwrap().resize(100, 30).unwrap();
        let last = writer
            .reserve_output()
            .unwrap()
            .output(b"after".to_vec())
            .unwrap();
        assert_eq!(last.raw_end, 11);
        assert!(resize.sequence > first.sequence && last.sequence > resize.sequence);
        writer.wait_committed(resize).unwrap(); // Existing durable resize barrier.
        assert!(!DurableCapture::read(root.path(), 0, 64).unwrap().0.eof);
        let end = writer.reserve_end().unwrap().end(None).unwrap();
        assert!(writer.reserve_output().is_err());
        let boundary = writer.wait_committed(end).unwrap();
        assert!(boundary.eof);
        assert_eq!(boundary.bytes, 11);
        assert_eq!(
            DurableCapture::read(root.path(), 0, 64).unwrap().1,
            b"beforeafter"
        );
        assert_eq!(writer.progress().unwrap().projected, first);
        owner.take().join().unwrap();
    }

    #[tokio::test]
    async fn reservations_bound_payload_and_controls_and_failure_wakes_waiter() {
        let (_root, writer, mut owner) = fresh();
        let outputs: Vec<_> = (0..OUTPUT_SLOTS)
            .map(|_| writer.reserve_output().unwrap())
            .collect();
        let controls: Vec<_> = (0..CONTROL_SLOTS)
            .map(|_| writer.reserve_resize().unwrap())
            .collect();
        // Terminal capacity does not compete with saturated output or controls.
        let terminal = writer.reserve_end().unwrap();
        let (entered, ready) = std::sync::mpsc::channel();
        let waiting = writer.clone();
        let thread = std::thread::spawn(move || {
            entered.send(()).unwrap();
            waiting.reserve_output().is_err()
        });
        ready.recv().unwrap();
        writer.fail("injected disk failure");
        assert!(thread.join().unwrap());
        assert!(writer.progress().unwrap().failure.is_some());
        drop((outputs, controls, terminal));
        owner.take().join().unwrap();
    }

    #[tokio::test]
    async fn persistence_failure_retains_last_acknowledged_boundary() {
        let (root, writer, mut owner) = fresh();
        let first = writer
            .reserve_output()
            .unwrap()
            .output(b"safe".to_vec())
            .unwrap();
        writer.wait_committed(first).unwrap();
        // atomic_state cannot publish over a directory: deterministic boundary
        // failure after event data has been appended and synchronized.
        let boundary = root.path().join("capture.json");
        let saved = std::fs::read(&boundary).unwrap();
        std::fs::remove_file(&boundary).unwrap();
        std::fs::create_dir(&boundary).unwrap();
        let second = writer
            .reserve_output()
            .unwrap()
            .output(b"uncertain".to_vec())
            .unwrap();
        assert!(writer.wait_committed(second).is_err());
        assert_eq!(writer.progress().unwrap().committed, first);
        assert_eq!(writer.wait_committed(first).unwrap().bytes, 4);
        assert!(writer.reserve_end().is_err());
        owner.take().join().unwrap();
        std::fs::remove_dir(&boundary).unwrap();
        std::fs::write(&boundary, saved).unwrap();
        assert_eq!(DurableCapture::read(root.path(), 0, 64).unwrap().1, b"safe");
        assert!(!DurableCapture::validate_recovery(root.path()).unwrap().eof);
    }

    #[tokio::test]
    async fn ordered_error_end_never_claims_clean_eof() {
        let (root, writer, mut owner) = fresh();
        let ticket = writer
            .reserve_output()
            .unwrap()
            .output(b"tail".to_vec())
            .unwrap();
        let end = writer
            .reserve_end()
            .unwrap()
            .end(Some("PTY failure".into()))
            .unwrap();
        let boundary = writer.wait_committed(end).unwrap();
        assert!(!boundary.eof);
        assert_eq!(boundary.error.as_deref(), Some("PTY failure"));
        assert!(writer.wait_committed(ticket).is_ok());
        assert_eq!(DurableCapture::read(root.path(), 0, 64).unwrap().1, b"tail");
        owner.take().join().unwrap();
    }

    #[tokio::test]
    async fn abandoned_owner_stops_admission_without_eof() {
        let (root, writer, owner) = fresh();
        let ticket = writer
            .reserve_output()
            .unwrap()
            .output(b"retained".to_vec())
            .unwrap();
        writer.wait_committed(ticket).unwrap();
        drop(owner);
        assert!(writer.reserve_output().is_err());
        assert!(!DurableCapture::read(root.path(), 0, 64).unwrap().0.eof);
    }
}
