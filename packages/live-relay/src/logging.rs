//! Best-effort security-bounded relay logs. A blocked sink never blocks Tokio.
use std::{
    io::Write,
    sync::atomic::{AtomicU64, Ordering},
    sync::{
        Mutex, OnceLock,
        mpsc::{self, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

const CAPACITY: usize = 64;
const PER_SECOND: u32 = 8;
const MAX_RECORD_BYTES: usize = 1024;
struct Budget {
    since: Instant,
    used: u32,
}
pub(crate) struct Log {
    sender: SyncSender<String>,
    budget: Mutex<Budget>,
    dropped: AtomicU64,
}
impl Log {
    fn new(mut sink: impl Write + Send + 'static) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<String>(CAPACITY);
        std::thread::Builder::new()
            .name("relay-log".into())
            .spawn(move || {
                for record in receiver {
                    if writeln!(sink, "{record}").is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender,
            budget: Mutex::new(Budget {
                since: Instant::now(),
                used: 0,
            }),
            dropped: AtomicU64::new(0),
        })
    }
    fn emit(&self, record: String) {
        if record.len() > MAX_RECORD_BYTES {
            self.drop_record();
            return;
        }
        // Never wait on either a logger lock or its IO thread.
        let Ok(mut budget) = self.budget.try_lock() else {
            self.drop_record();
            return;
        };
        if budget.since.elapsed() >= Duration::from_secs(1) {
            budget.since = Instant::now();
            budget.used = 0;
        }
        if budget.used >= PER_SECOND {
            self.drop_record();
            return;
        }
        budget.used += 1;
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.sender.try_send(record)
        {
            self.drop_record();
        }
    }
    fn drop_record(&self) {
        let _ = self
            .dropped
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
    }
}
static LOGGER: OnceLock<Option<Log>> = OnceLock::new();
// Initialize before serving; failure disables logging rather than failing publication.
pub(crate) fn initialize() {
    LOGGER.get_or_init(|| Log::new(std::io::stderr()).ok());
}
pub(crate) fn emit(record: serde_json::Value) {
    if let Some(Some(log)) = LOGGER.get() {
        log.emit(record.to_string());
    }
}
pub(crate) fn dropped() -> u64 {
    LOGGER
        .get()
        .and_then(Option::as_ref)
        .map_or(0, |log| log.dropped.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flood_and_contended_budget_never_wait_for_sink() {
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        struct Blocked {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Write for Blocked {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.entered.send(()).ok();
                self.release.recv().ok();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log = Log::new(Blocked {
            entered,
            release: blocked,
        })
        .unwrap();
        log.emit("first".into());
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..10000 {
            log.emit("bounded".into());
        }
        assert!(log.dropped.load(Ordering::Relaxed) >= 9993);
        let guard = log.budget.lock().unwrap();
        log.emit("contended".into());
        drop(guard);
        log.emit("x".repeat(MAX_RECORD_BYTES + 1));
        assert!(log.dropped.load(Ordering::Relaxed) >= 9995);
        drop(release); // unblock owned writer, including remaining newline writes
    }
    #[test]
    fn closed_sink_and_full_queue_are_nonblocking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let log = Log {
            sender,
            budget: Mutex::new(Budget {
                since: Instant::now(),
                used: 0,
            }),
            dropped: AtomicU64::new(0),
        };
        log.emit("one".into());
        log.emit("two".into());
        assert_eq!(log.dropped.load(Ordering::Relaxed), 1);
        drop(receiver);
        log.emit("three".into());
        assert_eq!(log.dropped.load(Ordering::Relaxed), 2);
    }
}
