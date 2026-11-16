//! The host terminal's writer, on its own thread.
//!
//! A host terminal that reads slowly (a remote session, a busy emulator) makes a blocking write
//! stall whoever writes. The attach client renders on its event loop, so it hands frames to a
//! thread that owns the host instead: bytes written are buffered, and `flush` hands them over in
//! order and returns at once. While the host has not taken everything handed to it, the client
//! renders nothing new; when the thread catches up it is woken and renders once, against what the
//! host now shows, so a slow host is shown the newest screen rather than every frame in between.
//! Before anything else writes to the host directly, `drain` waits until the thread has written
//! everything. A failed write is reported by the next write, flush or drain. A writer whose
//! caller does not wait for the host is held back once a few hand-offs are outstanding, so a
//! host that stops reading cannot make it queue without bound.

use std::io::{self, Write};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;

/// Hand-offs outstanding before a flush waits for the host.
const MAX_PENDING: usize = 4;

/// Writes to the host terminal from a dedicated thread (see the module documentation).
pub struct HostWriter {
    buffer: Vec<u8>,
    sender: Option<mpsc::Sender<Vec<u8>>>,
    shared: Arc<Shared>,
    caught_up: Arc<tokio::sync::Notify>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    drained: Condvar,
}

#[derive(Default)]
struct State {
    /// Hand-offs the thread has not finished writing.
    pending: usize,
    error: Option<io::Error>,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl HostWriter {
    /// Start a thread that owns `host` and writes what is handed to it.
    ///
    /// # Errors
    /// Returns an error when the thread cannot be started.
    pub fn spawn<W: Write + Send + 'static>(mut host: W) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<Vec<u8>>();
        let shared = Arc::new(Shared::default());
        let caught_up = Arc::new(tokio::sync::Notify::new());
        let thread = {
            let shared = Arc::clone(&shared);
            let caught_up = Arc::clone(&caught_up);
            std::thread::Builder::new()
                .name("bmux-host-writer".into())
                .spawn(move || {
                    while let Ok(mut bytes) = receiver.recv() {
                        // Everything already handed over goes out in one write.
                        let mut taken = 1;
                        while let Ok(more) = receiver.try_recv() {
                            bytes.extend_from_slice(&more);
                            taken += 1;
                        }
                        let written = host.write_all(&bytes).and_then(|()| host.flush());
                        let mut state = shared.state();
                        state.pending = state.pending.saturating_sub(taken);
                        if let Err(error) = written {
                            state.error.get_or_insert(error);
                        }
                        shared.drained.notify_all();
                        if state.pending == 0 {
                            caught_up.notify_one();
                        }
                    }
                })?
        };
        Ok(Self {
            buffer: Vec::new(),
            sender: Some(sender),
            shared,
            caught_up,
            thread: Some(thread),
        })
    }

    /// Whether the host has not yet taken everything handed to it.
    #[must_use]
    pub fn backlogged(&self) -> bool {
        self.shared.state().pending > 0
    }

    /// Notified when the host has taken everything handed to it.
    #[must_use]
    pub fn caught_up(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.caught_up)
    }

    /// Hand over what is buffered and wait until the host has taken everything.
    ///
    /// # Errors
    /// Returns the first failed write since the last error was reported.
    pub fn drain(&mut self) -> io::Result<()> {
        self.hand_off()?;
        self.wait_until_pending(0)
    }

    fn wait_until_pending(&self, at_most: usize) -> io::Result<()> {
        let mut state = self.shared.state();
        while state.pending > at_most {
            state = self
                .shared
                .drained
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.error.take().map_or(Ok(()), Err)
    }

    fn reported_error(&self) -> io::Result<()> {
        self.shared.state().error.take().map_or(Ok(()), Err)
    }

    fn hand_off(&mut self) -> io::Result<()> {
        self.reported_error()?;
        if self.buffer.is_empty() {
            return Ok(());
        }
        let Some(sender) = self.sender.as_ref() else {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        };
        self.shared.state().pending += 1;
        let bytes = std::mem::take(&mut self.buffer);
        sender.send(bytes).map_err(|_| {
            self.shared.state().pending -= 1;
            io::Error::from(io::ErrorKind::BrokenPipe)
        })?;
        self.wait_until_pending(MAX_PENDING)
    }
}

impl Write for HostWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.reported_error()?;
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Hand what is buffered to the thread without waiting for the host.
    fn flush(&mut self) -> io::Result<()> {
        self.hand_off()
    }
}

impl Drop for HostWriter {
    fn drop(&mut self) {
        let _ = self.hand_off();
        // Closing the channel ends the thread once it has written everything.
        self.sender = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, SyncSender};

    /// A host that takes one write at a time, when the test lets it, and records what it took.
    struct GatedHost {
        gate: Receiver<()>,
        taken: Arc<Mutex<Vec<u8>>>,
        fail: bool,
    }

    impl Write for GatedHost {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.gate
                .recv()
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            if self.fail {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            self.taken.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn gated(fail: bool) -> (HostWriter, SyncSender<()>, Arc<Mutex<Vec<u8>>>) {
        let (open, gate) = mpsc::sync_channel(16);
        let taken = Arc::new(Mutex::new(Vec::new()));
        let writer = HostWriter::spawn(GatedHost {
            gate,
            taken: Arc::clone(&taken),
            fail,
        })
        .unwrap();
        (writer, open, taken)
    }

    #[test]
    fn a_flush_returns_before_the_host_takes_the_bytes_and_order_is_kept() {
        let (mut writer, open, taken) = gated(false);
        writer.write_all(b"one ").unwrap();
        writer.flush().unwrap();
        writer.write_all(b"two").unwrap();
        writer.flush().unwrap();
        // Nothing taken yet, and the flushes did not wait.
        assert!(writer.backlogged());
        assert_eq!(*taken.lock().unwrap(), b"");
        open.send(()).unwrap();
        open.send(()).unwrap();
        writer.drain().unwrap();
        assert!(!writer.backlogged());
        assert_eq!(*taken.lock().unwrap(), b"one two");
    }

    #[test]
    fn a_failed_host_write_is_reported_by_the_next_call() {
        let (mut writer, open, _) = gated(true);
        writer.write_all(b"frame").unwrap();
        open.send(()).unwrap();
        assert_eq!(
            writer.drain().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        // Reported once.
        writer.drain().unwrap();
    }

    #[test]
    fn dropping_the_writer_writes_what_is_left() {
        let (mut writer, open, taken) = gated(false);
        writer.write_all(b"restore").unwrap();
        open.send(()).unwrap();
        drop(writer);
        assert_eq!(*taken.lock().unwrap(), b"restore");
    }

    #[test]
    fn catching_up_wakes_a_waiter() {
        use futures_util::FutureExt as _;
        let (mut writer, open, _) = gated(false);
        let caught_up = writer.caught_up();
        writer.write_all(b"frame").unwrap();
        writer.flush().unwrap();
        assert_eq!(caught_up.notified().now_or_never(), None);
        open.send(()).unwrap();
        writer.drain().unwrap();
        // The wake is kept for a waiter that starts after it.
        assert_eq!(caught_up.notified().now_or_never(), Some(()));
    }
}
