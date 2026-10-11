//! Bounded raw capture, independent of publication and terminal projection.
use std::io::{self, Read};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread::JoinHandle;

pub(super) struct CaptureOwner {
    thread: Option<JoinHandle<()>>,
    runtime: tokio::runtime::Handle,
}
impl CaptureOwner {
    pub(super) fn new(thread: JoinHandle<()>) -> Self {
        Self {
            thread: Some(thread),
            runtime: tokio::runtime::Handle::current(),
        }
    }
    pub(super) fn take(&mut self) -> JoinHandle<()> {
        self.thread.take().expect("capture thread owner")
    }
}
impl Drop for CaptureOwner {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // Cancellation transfers ownership to the persistent runtime rather
            // than detaching the blocking thread or claiming completed cleanup.
            self.runtime.spawn(async move {
                let mut threads = [Some(thread)];
                while threads.iter().flatten().any(|thread| !thread.is_finished()) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if let Err(error) =
                    super::join_supervision_threads(&mut threads, std::time::Duration::ZERO).await
                {
                    tracing::error!(%error, "cancelled startup capture cleanup failed");
                }
            });
        }
    }
}

pub(super) struct StartupReader {
    receiver: Receiver<io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    offset: usize,
}

impl Read for StartupReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.offset == self.pending.len() {
            self.pending = match self.receiver.recv() {
                Ok(chunk) => chunk?,
                Err(_) => {
                    return Err(io::Error::other(
                        "raw capture ended without an explicit EOF boundary",
                    ));
                }
            };
            self.offset = 0;
        }
        let count = output.len().min(self.pending.len() - self.offset);
        output[..count].copy_from_slice(&self.pending[self.offset..self.offset + count]);
        self.offset += count;
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
                if sender.send(chunk).is_err() || done {
                    break;
                }
            }
        })?;
    Ok((
        StartupReader {
            receiver,
            pending: Vec::new(),
            offset: 0,
        },
        thread,
    ))
}

pub(super) async fn acknowledge_cleanup(
    thread: JoinHandle<()>,
    cleanup: &tokio::sync::watch::Sender<Option<Result<(), String>>>,
    previous: Option<Result<(), String>>,
    reason: String,
) -> String {
    let mut threads = [Some(thread)];
    let capture_result =
        super::join_supervision_threads(&mut threads, std::time::Duration::from_secs(4)).await;
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
    // Retain thread ownership after an explicit timeout until it really settles.
    while threads.iter().flatten().any(|thread| !thread.is_finished()) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = super::join_supervision_threads(&mut threads, std::time::Duration::ZERO).await;
    reason
}

#[cfg(test)]
mod tests {
    use std::io::Read;

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
