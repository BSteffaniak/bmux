//! Bounded raw capture, independent of publication and terminal projection.
use std::io::{self, Read};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread::JoinHandle;

pub(super) struct CaptureOwner {
    threads: Vec<JoinHandle<()>>,
    writer: Option<super::capture_writer::CaptureWriter>,
    runtime: tokio::runtime::Handle,
}
impl CaptureOwner {
    pub(super) fn new(thread: JoinHandle<()>) -> Self {
        Self {
            threads: vec![thread],
            writer: None,
            runtime: tokio::runtime::Handle::current(),
        }
    }
    pub(super) fn add_writer(
        &mut self,
        mut owner: super::capture_writer::WriterOwner,
        writer: super::capture_writer::CaptureWriter,
    ) {
        self.writer = Some(writer);
        self.threads.push(owner.take());
    }
    pub(super) fn take(&mut self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut self.threads)
    }
}
impl Drop for CaptureOwner {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            if let Some(writer) = &self.writer {
                writer.fail("capture supervision cancelled");
            }
            let threads = self.take();
            // Cancellation transfers ownership to the persistent runtime rather
            // than detaching the blocking thread or claiming completed cleanup.
            self.runtime.spawn(async move {
                let mut threads: Vec<_> = threads.into_iter().map(Some).collect();
                let mut join = super::supervision_join(&mut threads);
                if let Err(error) = (&mut join)
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
                {
                    tracing::error!(%error, "cancelled startup capture cleanup failed");
                }
            });
        }
    }
}

struct StartupChunk {
    bytes: Vec<u8>,
    ticket: Option<super::capture_writer::Ticket>,
}

pub(super) struct StartupReader {
    receiver: Receiver<io::Result<StartupChunk>>,
    pending: Vec<u8>,
    offset: usize,
    publication_ticket: Option<super::capture_writer::Ticket>,
    pending_ticket: Option<super::capture_writer::Ticket>,
    ended: bool,
}

impl StartupReader {
    /// Mark this ticket projected only after processing the returned bytes.
    /// None means that an ordered chunk has only been partially consumed.
    pub(super) fn publication_ticket(&self) -> Option<super::capture_writer::Ticket> {
        self.publication_ticket
    }
}

impl Read for StartupReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.publication_ticket = None;
        if output.is_empty() || self.ended {
            return Ok(0);
        }
        if self.offset == self.pending.len() {
            let chunk = self.receiver.recv().map_err(|_| {
                io::Error::other("capture producer disconnected without terminal boundary")
            })??;
            self.pending = chunk.bytes;
            self.pending_ticket = chunk.ticket;
            self.offset = 0;
            if self.pending.is_empty() {
                self.ended = true;
                self.publication_ticket = self.pending_ticket;
                return Ok(0);
            }
        }
        let count = output.len().min(self.pending.len() - self.offset);
        output[..count].copy_from_slice(&self.pending[self.offset..self.offset + count]);
        self.offset += count;
        if self.offset == self.pending.len() {
            self.publication_ticket = self.pending_ticket;
        }
        Ok(count)
    }
}

#[cfg(test)]
pub(super) fn start(raw: Box<dyn Read + Send>) -> io::Result<(StartupReader, JoinHandle<()>)> {
    start_captured(
        raw,
        std::sync::Arc::new(super::execution::ExecutionCapture::default()),
    )
}
pub(super) fn start_captured(
    mut raw: Box<dyn Read + Send>,
    execution: std::sync::Arc<super::execution::ExecutionCapture>,
) -> io::Result<(StartupReader, JoinHandle<()>)> {
    // At most 32 * 8192 raw bytes queued; saturation backpressures the PTY.
    let (sender, receiver) = sync_channel(32);
    let thread = std::thread::Builder::new()
        .name("bmux-pty-startup-capture".into())
        .spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                let chunk = match raw.read(&mut buffer) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => result.map(|count| buffer[..count].to_vec()),
                };
                let chunk = chunk.and_then(|bytes| {
                    // Never hold this gate across bounded-channel send: resize
                    // must let the projection reader drain already captured bytes.
                    let _order = execution.capture_order.lock()
                        .map_err(|_| io::Error::other("capture ordering lock poisoned"))?;
                    let mut capture = execution.durable_capture.lock().map_err(|_| io::Error::other("capture authority lock poisoned"))?;
                    if let Some(capture) = capture.as_mut() {
                        if bytes.is_empty() { capture.end(None)?; } else { capture.append(&bytes)?; }
                    }
                    Ok(bytes)
                });
                if let Err(error) = &chunk
                    && let Ok(mut capture) = execution.durable_capture.lock()
                    && let Some(capture) = capture.as_mut() {
                    let _ = capture.end(Some(error.to_string()));
                }
                let done = chunk.as_ref().is_err() || chunk.as_ref().is_ok_and(Vec::is_empty);
                if done {
                    #[cfg(test)]
                    eprintln!("startup raw capture boundary: eof={} error={:?}", chunk.as_ref().is_ok_and(Vec::is_empty), chunk.as_ref().err());
                    tracing::debug!(eof = chunk.as_ref().is_ok_and(Vec::is_empty), error = ?chunk.as_ref().err(), "startup raw capture reached terminal read boundary");
                }
                if sender.send(chunk.map(|bytes| StartupChunk { bytes, ticket: None })).is_err() || done {
                    break;
                }
            }
        })?;
    Ok((
        StartupReader {
            receiver,
            pending: Vec::new(),
            offset: 0,
            publication_ticket: None,
            pending_ticket: None,
            ended: false,
        },
        thread,
    ))
}

/// Async persistence path. The coordinator installs the same writer on the
/// execution before starting this reader and retains `WriterOwner` in supervision.
/// Resize must use `execution.capture_order` and wait for the admitted raw prefix
/// to be projected before changing geometry. Disk waits never hold that gate.
pub(super) fn start_ordered(
    mut raw: Box<dyn Read + Send>,
    execution: std::sync::Arc<super::execution::ExecutionCapture>,
    writer: super::capture_writer::CaptureWriter,
) -> io::Result<(StartupReader, JoinHandle<()>)> {
    if execution
        .durable_capture
        .lock()
        .map_err(|_| io::Error::other("capture authority lock poisoned"))?
        .is_some()
    {
        return Err(io::Error::other(
            "ordered writer and legacy synchronous capture cannot coexist",
        ));
    }
    let (sender, receiver) = sync_channel(32);
    let thread = std::thread::Builder::new()
        .name("bmux-pty-ordered-capture".into())
        .spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                let read = loop {
                    match raw.read(&mut buffer) {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        // PTY adapters use EIO for last-slave closure. This is
                        // the sole normalization point for the ordered path.
                        #[cfg(unix)]
                        Err(error) if error.raw_os_error() == Some(libc::EIO) => break Ok(0),
                        result => break result,
                    }
                };
                let terminal = !matches!(&read, Ok(count) if *count > 0);
                let mut end_admitted = false;
                let result = (|| {
                    // The single raw producer reserves outside capture_order.
                    // Live send is also outside the gate; resize fences on the
                    // admitted prefix so it cannot overtake this publication.
                    let reservation = if terminal {
                        writer.reserve_end()?
                    } else {
                        writer.reserve_output()?
                    };
                    let order = execution
                        .capture_order
                        .lock()
                        .map_err(|_| io::Error::other("capture ordering lock poisoned"))?;
                    let chunk = match read {
                        Ok(0) => StartupChunk {
                            bytes: Vec::new(),
                            ticket: Some(reservation.end(None)?),
                        },
                        Ok(count) => {
                            let bytes = buffer[..count].to_vec();
                            let ticket = reservation.output(bytes.clone())?;
                            StartupChunk {
                                bytes,
                                ticket: Some(ticket),
                            }
                        }
                        Err(error) => {
                            let end = reservation.end(Some(error.to_string()))?;
                            end_admitted = true;
                            drop(order);
                            // Terminal error publication is allowed to wait:
                            // preserve the captured prefix/error before the
                            // projection supervisor reacts by stopping authority.
                            writer.wait_committed(end)?;
                            return Err(error);
                        }
                    };
                    drop(order);
                    Ok(chunk)
                })();
                // A raw read error is represented by an ordered error end, so
                // allow that prefix to drain. Admission failures instead stop
                // writer authority and wake all blocked capacity/barrier waits.
                if let Err(error) = &result
                    && !end_admitted
                {
                    writer.fail(&error.to_string());
                }
                let failed = result.is_err();
                if sender.send(result).is_err() {
                    writer.fail("live projection receiver disconnected");
                    break;
                }
                if terminal || failed {
                    break;
                }
            }
        })?;
    Ok((
        StartupReader {
            receiver,
            pending: Vec::new(),
            offset: 0,
            publication_ticket: None,
            pending_ticket: None,
            ended: false,
        },
        thread,
    ))
}

pub(super) async fn acknowledge_cleanup(
    threads: Vec<JoinHandle<()>>,
    cleanup: &tokio::sync::watch::Sender<Option<Result<(), String>>>,
    previous: Option<Result<(), String>>,
    reason: String,
) -> String {
    let mut threads: Vec<_> = threads.into_iter().map(Some).collect();
    let join = super::supervision_join(&mut threads);
    let capture_result = join
        .await
        .map_err(|error| format!("startup capture join failed: {error}"))
        .and_then(std::convert::identity);
    if let Err(error) = bmux_plugin::shutdown_progress().advance("startup capture settled") {
        tracing::warn!(%error, "shutdown progress publication failed");
    }
    let result = match (previous.unwrap_or(Ok(())), capture_result) {
        (Ok(()), capture) => capture,
        (Err(process), Ok(())) => Err(process),
        (Err(process), Err(capture)) => Err(format!("{process}; {capture}")),
    };
    let reason = match &result {
        Ok(()) => reason,
        Err(error) => format!("{reason}; startup capture cleanup failed: {error}"),
    };
    cleanup.send_replace(Some(result));
    reason
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    #[test]
    fn channel_disconnect_is_not_eof_and_explicit_eof_is_sticky() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let mut reader = super::StartupReader {
            receiver,
            pending: Vec::new(),
            offset: 0,
            publication_ticket: None,
            pending_ticket: None,
            ended: false,
        };
        drop(sender);
        assert!(reader.read(&mut [0; 1]).is_err());
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let mut reader = super::StartupReader {
            receiver,
            pending: Vec::new(),
            offset: 0,
            publication_ticket: None,
            pending_ticket: None,
            ended: false,
        };
        sender
            .send(Ok(super::StartupChunk {
                bytes: Vec::new(),
                ticket: None,
            }))
            .unwrap();
        drop(sender);
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn partial_reads_do_not_publish_a_whole_chunk_ticket() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let ticket = super::super::capture_writer::Ticket {
            sequence: 1,
            raw_end: 4,
        };
        sender
            .send(Ok(super::StartupChunk {
                bytes: b"tail".to_vec(),
                ticket: Some(ticket),
            }))
            .unwrap();
        let mut reader = super::StartupReader {
            receiver,
            pending: Vec::new(),
            offset: 0,
            publication_ticket: None,
            pending_ticket: None,
            ended: false,
        };
        assert_eq!(reader.read(&mut [0; 2]).unwrap(), 2);
        assert!(reader.publication_ticket().is_none());
        assert_eq!(reader.read(&mut [0; 2]).unwrap(), 2);
        assert_eq!(reader.publication_ticket(), Some(ticket));
    }

    #[tokio::test]
    async fn ordered_reader_publishes_exact_tail_and_durable_end_separately() {
        let root = tempfile::tempdir().unwrap();
        let capture = super::super::durable_capture::DurableCapture::create(root.path()).unwrap();
        let (writer, mut owner) =
            super::super::capture_writer::CaptureWriter::start(capture).unwrap();
        let (mut reader, raw_thread) = super::start_ordered(
            Box::new(std::io::Cursor::new(b"tail".to_vec())),
            std::sync::Arc::new(super::super::execution::ExecutionCapture::default()),
            writer.clone(),
        )
        .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"tail");
        raw_thread.join().unwrap();
        let end = writer.progress().unwrap().admitted;
        assert!(writer.wait_committed(end).unwrap().eof);
        owner.take().join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ordered_pty_eio_is_one_real_terminal_boundary() {
        struct ClosedPty;
        impl Read for ClosedPty {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            }
        }
        let root = tempfile::tempdir().unwrap();
        let capture = super::super::durable_capture::DurableCapture::create(root.path()).unwrap();
        let (writer, mut owner) =
            super::super::capture_writer::CaptureWriter::start(capture).unwrap();
        let (mut reader, raw_thread) = super::start_ordered(
            Box::new(ClosedPty),
            std::sync::Arc::new(super::super::execution::ExecutionCapture::default()),
            writer.clone(),
        )
        .unwrap();
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
        let end = reader.publication_ticket().unwrap();
        assert!(writer.wait_committed(end).unwrap().eof);
        assert_eq!(writer.progress().unwrap().admitted.sequence, 1);
        raw_thread.join().unwrap();
        owner.take().join().unwrap();
    }

    #[tokio::test]
    async fn cancelled_capture_owner_retains_blocked_thread_until_release() {
        let (release, blocked) = std::sync::mpsc::channel();
        let (done, finished) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            blocked.recv().unwrap();
            let _ = done.send(());
        });
        let owner = super::CaptureOwner::new(thread);
        drop(owner);
        tokio::task::yield_now().await;
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), finished)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn capture_cleanup_preserves_prior_failure_after_real_join() {
        let thread = std::thread::spawn(|| {});
        let (cleanup, mut acknowledged) = tokio::sync::watch::channel(None);
        let reason = super::acknowledge_cleanup(
            vec![thread],
            &cleanup,
            Some(Err("process cleanup failed".into())),
            "startup rejected".into(),
        )
        .await;
        acknowledged.changed().await.unwrap();
        assert_eq!(
            acknowledged
                .borrow()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err(),
            "process cleanup failed"
        );
        assert!(reason.contains("process cleanup failed"));
    }

    #[test]
    fn interrupted_read_retries_without_losing_bytes_or_inventing_eof() {
        struct InterruptedOnce {
            interrupted: bool,
            bytes: std::io::Cursor<Vec<u8>>,
        }
        impl Read for InterruptedOnce {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.bytes.read(output)
            }
        }
        let expected = vec![42; 40 * 8192];
        let (mut reader, capture) = super::start(Box::new(InterruptedOnce {
            interrupted: false,
            bytes: std::io::Cursor::new(expected.clone()),
        }))
        .unwrap();
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).unwrap();
        capture.join().unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn reader_error_is_preserved_after_captured_prefix() {
        struct FailingReader(bool);
        impl Read for FailingReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    return Err(std::io::Error::other("capture fixture failure"));
                }
                self.0 = true;
                output[..4].copy_from_slice(b"tail");
                Ok(4)
            }
        }
        let (mut reader, capture) = super::start(Box::new(FailingReader(false))).unwrap();
        let mut actual = Vec::new();
        let error = reader.read_to_end(&mut actual).unwrap_err();
        capture.join().unwrap();
        assert_eq!(actual, b"tail");
        assert_eq!(error.to_string(), "capture fixture failure");
    }

    #[test]
    fn immediate_bytes_survive_delayed_publication() {
        let bytes = b"literal;$(no-shell)".to_vec();
        let (mut reader, capture) =
            super::start(Box::new(std::io::Cursor::new(bytes.clone()))).unwrap();
        capture.join().unwrap();
        let mut captured = Vec::new();
        reader.read_to_end(&mut captured).unwrap();
        assert_eq!(captured, bytes);
    }
}
