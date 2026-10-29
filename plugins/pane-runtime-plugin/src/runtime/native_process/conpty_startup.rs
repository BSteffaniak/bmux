//! `ConPTY`'s initial cursor negotiation, before its hosted process can start.
//!
//! Only the exact leading `CSI 6 n` is transport bootstrap, on a newly allocated
//! empty terminal (origin 1;1). Application output ends this recognizer forever.
//! Raw bytes are still captured verbatim; projection drops only this transport
//! query so the ordinary terminal parser cannot answer it a second time later.
use std::borrow::Cow;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

pub(in super::super) type Writer = Arc<Mutex<Box<dyn Write + Send>>>;
const QUERY: &[u8] = b"\x1b[6n";
const REPLY: &[u8] = b"\x1b[1;1R";

#[derive(Default)]
pub(in super::super) struct PrefixQuery {
    prefix: Vec<u8>,
    finished: bool,
}

impl PrefixQuery {
    /// Retains at most three bytes between reads. A mismatch releases every
    /// byte unchanged and disables recognition for the rest of this execution.
    pub(in super::super) fn project<'a>(&mut self, bytes: &'a [u8]) -> (Cow<'a, [u8]>, bool) {
        if self.finished {
            return (Cow::Borrowed(bytes), false);
        }
        for (index, byte) in bytes.iter().copied().enumerate() {
            self.prefix.push(byte);
            if byte != QUERY[self.prefix.len() - 1] {
                self.finished = true;
                let mut output = std::mem::take(&mut self.prefix);
                output.extend_from_slice(&bytes[index + 1..]);
                return (Cow::Owned(output), false);
            }
            if self.prefix.len() == QUERY.len() {
                self.finished = true;
                self.prefix.clear();
                return (Cow::Borrowed(&bytes[index + 1..]), true);
            }
        }
        (Cow::Borrowed(&[]), false)
    }
}

// At most the first sixteen byte *classes* are exposed, never printable output.
// This distinguishes cursor-prefix transport from absent/different output while
// keeping credentials, commands and terminal contents out of diagnostics.
fn diagnostic(owner: uuid::Uuid, stage: &'static str, total: u64, bytes: &[u8]) {
    let classes: String = bytes
        .iter()
        .take(16)
        .map(|byte| match byte {
            0x1b => 'E',
            b'[' => '[',
            b'0'..=b'9' => 'D',
            b'a'..=b'z' | b'A'..=b'Z' => 'L',
            0..=31 | 127 => 'C',
            _ => '?',
        })
        .collect();
    tracing::debug!(%owner, stage, total, %classes, "ConPTY bootstrap progress");
    #[cfg(test)]
    eprintln!("ConPTY bootstrap owner={owner} stage={stage} total={total} classes={classes}");
}

/// Observe only the native spawn boundary, without logging command arguments,
/// bootstrap paths, authentication, or environment. This distinguishes a stuck
/// `CreateProcess` from subsequent supervisor pipe/lifetime negotiation.
#[cfg(windows)]
pub(in super::super) struct ObservedSlave<'a> {
    pub(in super::super) slave: &'a dyn portable_pty::SlavePty,
    pub(in super::super) owner: uuid::Uuid,
}

#[cfg(windows)]
impl portable_pty::SlavePty for ObservedSlave<'_> {
    fn spawn_command(
        &self,
        command: portable_pty::CommandBuilder,
    ) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
        diagnostic(self.owner, "native-spawn-enter", 0, &[]);
        let result = self.slave.spawn_command(command);
        diagnostic(
            self.owner,
            if result.is_ok() {
                "native-spawn-ok"
            } else {
                "native-spawn-error"
            },
            0,
            &[],
        );
        result
    }
}

pub(in super::super) struct BootstrapReader {
    owner: uuid::Uuid,
    raw: Box<dyn Read + Send>,
    writer: Writer,
    query: PrefixQuery,
    first_read: bool,
    read_bytes: u64,
}

impl BootstrapReader {
    pub(in super::super) fn new(
        raw: Box<dyn Read + Send>,
        writer: Writer,
        owner: uuid::Uuid,
    ) -> Self {
        Self {
            owner,
            raw,
            writer,
            query: PrefixQuery::default(),
            first_read: true,
            read_bytes: 0,
        }
    }
}

impl Read for BootstrapReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let first = std::mem::take(&mut self.first_read);
        if first {
            diagnostic(self.owner, "read-enter", 0, &[]);
        }
        let count = self.raw.read(buffer)?;
        self.read_bytes = self
            .read_bytes
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        if first || count == 0 {
            diagnostic(
                self.owner,
                if count == 0 { "read-eof" } else { "first-read" },
                self.read_bytes,
                &buffer[..count],
            );
        }
        let was_finished = self.query.finished;
        let (_, reply) = self.query.project(&buffer[..count]);
        if !was_finished && self.query.finished && !reply {
            diagnostic(
                self.owner,
                "prefix-mismatch",
                self.read_bytes,
                &buffer[..count],
            );
        }
        if reply {
            diagnostic(self.owner, "reply-lock-enter", self.read_bytes, &[]);
            // This is terminal protocol traffic on this exact PTY, never an
            // input command, input event, or another execution's writer.
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| io::Error::other("ConPTY bootstrap writer poisoned"))?;
            diagnostic(self.owner, "reply-write-enter", self.read_bytes, &[]);
            writer.write_all(REPLY)?;
            diagnostic(self.owner, "reply-written", self.read_bytes, &[]);
            writer.flush()?;
            diagnostic(self.owner, "reply-flushed", self.read_bytes, &[]);
        }
        Ok(count)
    }
}

/// Closing a retired console can synchronously wait for its output drain.
/// Keep it off the executor; the independent reader remains the sole EOF owner.
#[cfg(any(windows, test))]
pub(in super::super) async fn close_retired_console<T: Send + 'static>(
    console: T,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || drop(console)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct ReplyWriter(mpsc::SyncSender<Vec<u8>>);
    impl Write for ReplyWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.send(bytes.to_vec()).map_err(io::Error::other)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    struct WaitingReader {
        phase: usize,
        replies: mpsc::Receiver<Vec<u8>>,
    }
    impl Read for WaitingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let bytes: &[u8] = match self.phase {
                0 => b"\x1b[",
                1 => b"6n",
                2 => {
                    let reply = self
                        .replies
                        .recv_timeout(Duration::from_secs(1))
                        .map_err(io::Error::other)?;
                    assert_eq!(reply, REPLY);
                    b"program-started\r\n\x1b[6n"
                }
                _ => {
                    assert!(matches!(
                        self.replies.try_recv(),
                        Err(mpsc::TryRecvError::Empty)
                    ));
                    return Ok(0);
                }
            };
            self.phase += 1;
            buffer[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    #[test]
    fn bootstrap_unblocks_startup_preserves_capture_and_leaves_application_queries_alone() {
        let (sent, replies) = mpsc::sync_channel(1);
        let writer: Writer = Arc::new(Mutex::new(Box::new(ReplyWriter(sent))));
        let raw = Box::new(WaitingReader { phase: 0, replies });
        let mut reader = BootstrapReader::new(raw, writer, uuid::Uuid::new_v4());
        let mut captured = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let count = reader.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            captured.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(captured, b"\x1b[6nprogram-started\r\n\x1b[6n");
        let mut projection = PrefixQuery::default();
        let mut projected = Vec::new();
        for byte in &captured {
            projected.extend_from_slice(&projection.project(&[*byte]).0);
        }
        assert_eq!(projected, b"program-started\r\n\x1b[6n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retired_console_closes_while_independent_output_reader_drains() {
        struct Console {
            close_started: mpsc::SyncSender<()>,
            drain_finished: mpsc::Receiver<()>,
        }
        impl Drop for Console {
            fn drop(&mut self) {
                self.close_started.send(()).unwrap();
                self.drain_finished
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap();
            }
        }
        let (started, read_close) = mpsc::sync_channel(1);
        let (drained, read_drain) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            read_close.recv_timeout(Duration::from_secs(1)).unwrap();
            // The real read owner, not the close operation, reports drain.
            drained.send(()).unwrap();
        });
        let console = Console {
            close_started: started,
            drain_finished: read_drain,
        };
        close_retired_console(console).await.unwrap();
        reader.join().unwrap();
    }

    #[test]
    fn prefix_state_is_bounded_and_never_shared_between_executions() {
        let mut first = PrefixQuery::default();
        let mut other = PrefixQuery::default();
        assert!(first.project(b"\x1b[").0.is_empty());
        assert_eq!(other.project(b"6n").0.as_ref(), b"6n");
        assert!(first.project(b"6n").1);
        assert_eq!(first.project(QUERY).0.as_ref(), QUERY);
        assert!(!other.project(QUERY).1);
        for split in 0..QUERY.len() {
            let mut probe = PrefixQuery::default();
            assert!(!probe.project(&QUERY[..split]).1);
            assert!(probe.prefix.len() < QUERY.len());
            assert!(probe.project(&QUERY[split..]).1);
        }
        let mut ordinary = PrefixQuery::default();
        assert_eq!(ordinary.project(b"\x1b[31mred").0.as_ref(), b"\x1b[31mred");
    }
}
