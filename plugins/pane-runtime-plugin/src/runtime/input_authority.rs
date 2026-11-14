//! Retained request authority for queued terminal input.
use std::io::{self, Write};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

type WriteReceipt = std::sync::mpsc::Receiver<Result<(), String>>;
const MAX_PENDING_WRITES: usize = 64;
const MAX_INPUT_BYTES: usize = 65_536;
const WRITE_SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(10);
// Shared capacity only: caller authority is captured separately in each command.
static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

thread_local! {
    // Receipts are collected only during synchronous route execution. Waiting
    // happens after the handler releases its manager locks, but before the host
    // closes the invocation that authorizes the queued PTY writes.
    static WRITE_RECEIPTS: std::cell::RefCell<Option<Vec<Result<WriteReceipt, String>>>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn with_local_settled<T>(
    context: bmux_plugin_sdk::NativeServiceContext,
    effect: impl FnOnce() -> T,
) -> Result<T, String> {
    struct Restore(Option<Vec<Result<WriteReceipt, String>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            WRITE_RECEIPTS.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(WRITE_RECEIPTS.with(|slot| slot.replace(Some(Vec::new()))));
    let output = with_local(context.clone(), effect);
    let receipts = WRITE_RECEIPTS.with(|slot| slot.borrow_mut().take().unwrap_or_default());
    let settlement = settle(receipts);
    drop(context);
    settlement?;
    Ok(output)
}

fn settle(receipts: Vec<Result<WriteReceipt, String>>) -> Result<(), String> {
    let deadline = Instant::now() + WRITE_SETTLEMENT_TIMEOUT;
    let mut failure = None;
    for receipt in receipts {
        let result = receipt.and_then(|receiver| {
            receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|error| {
                    format!("PTY input completion unconfirmed ({error}); do not replay")
                })?
        });
        if let Err(error) = result {
            // Settle the remaining admitted chunks even after a known failure.
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

pub(crate) fn settled_local_command(
    bytes: Vec<u8>,
    session: bmux_session_models::SessionId,
) -> Result<super::PaneRuntimeCommand, super::SessionRuntimeError> {
    WRITE_RECEIPTS.with(|slot| {
        let mut receipts = slot.borrow_mut();
        let Some(receipts) = receipts.as_mut() else {
            return Ok(local_command(bytes, session));
        };
        let prepare = || {
            if receipts.len() >= MAX_PENDING_WRITES - 1 || bytes.len() > MAX_INPUT_BYTES {
                return Err("PTY input admission bound exceeded".to_string());
            }
            let permit = SLOTS
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_WRITES)))
                .clone()
                .try_acquire_owned()
                .map_err(|_| "PTY input queue full".to_string())?;
            // The outer clone keeps the service context alive through receipt
            // settlement, not merely until command construction.
            let authority = LOCAL_INPUT
                .with(|slot| {
                    slot.borrow().as_ref().map(|context| {
                        crate::handlers::execution::local_input_authority(context, session.0)
                    })
                })
                .ok_or_else(|| "PTY input invocation missing".to_string())?;
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            Ok((
                super::PaneRuntimeCommand::AdmittedInput(bytes, sender, permit, authority),
                receiver,
            ))
        };
        match prepare() {
            Ok((command, receiver)) => {
                receipts.push(Ok(receiver));
                Ok(command)
            }
            Err(error) => {
                // Keep the collector bounded even if a handler ignores errors.
                if receipts.len() < MAX_PENDING_WRITES {
                    receipts.push(Err(error));
                }
                Err(super::SessionRuntimeError::Closed)
            }
        }
    })
}

thread_local! {
    // Synchronous ingress only; copied into commands, never read by PTY workers.
    static LOCAL_INPUT: std::cell::RefCell<Option<bmux_plugin_sdk::NativeServiceContext>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn with_local<T>(
    context: bmux_plugin_sdk::NativeServiceContext,
    effect: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<bmux_plugin_sdk::NativeServiceContext>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LOCAL_INPUT.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(LOCAL_INPUT.with(|slot| slot.replace(Some(context))));
    effect()
}

pub(crate) fn local_command(
    bytes: Vec<u8>,
    session: bmux_session_models::SessionId,
) -> super::PaneRuntimeCommand {
    LOCAL_INPUT.with(|slot| match slot.borrow().as_ref() {
        Some(context) => super::PaneRuntimeCommand::AuthorizedInput(
            bytes,
            crate::handlers::execution::local_input_authority(context, session.0),
        ),
        // In-process local input has no remote service caller. Remote legacy
        // routes are rejected at ingress, independently of permissions plugins.
        None => super::PaneRuntimeCommand::Input(bytes),
    })
}

/// A bound host context (including cancellation and admission ownership), not a
/// caller-supplied identity or a cached ingress decision.
#[derive(Clone)]
pub(crate) struct InputAuthority(Arc<dyn Fn() -> Result<(), String> + Send + Sync>);

impl InputAuthority {
    pub(crate) fn new(check: impl Fn() -> Result<(), String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(check))
    }

    pub(crate) fn check(&self) -> io::Result<()> {
        (self.0)().map_err(io::Error::other)
    }

    /// Recheck after acquiring the writer, and before every partial-write
    /// continuation. Never retry Interrupted or any other uncertain failure.
    pub(crate) fn write(&self, writer: &mut impl Write, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            self.check()?;
            let written = writer.write(bytes)?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[written..];
        }
        self.check()?;
        writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn queued_revocation_and_cancellation_prevent_writes() {
        for reason in ["admission revoked", "request cancelled"] {
            let allowed = Arc::new(AtomicBool::new(true));
            let current = allowed.clone();
            let authority = InputAuthority::new(move || {
                current
                    .load(Ordering::SeqCst)
                    .then_some(())
                    .ok_or(reason.into())
            });
            authority.check().unwrap(); // ingress, before enqueue
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send((authority, b"queued".to_vec())).unwrap();
            allowed.store(false, Ordering::SeqCst);
            let (authority, bytes) = rx.recv().unwrap();
            let mut output = Vec::new();
            assert!(authority.write(&mut output, &bytes).is_err());
            assert_eq!(output, [] as [u8; 0]);
        }
    }

    #[test]
    fn settlement_preserves_success_and_reports_unconfirmed_completion() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.send(Ok(())).unwrap();
        assert!(settle(vec![Ok(receiver)]).is_ok());
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        drop(sender);
        let error = settle(vec![Ok(receiver)]).unwrap_err();
        assert!(error.contains("completion unconfirmed"));
        assert!(error.contains("do not replay"));
    }

    #[test]
    fn settlement_drains_other_chunks_after_known_failure() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(0);
        let worker = std::thread::spawn(move || sender.send(Ok(())).unwrap());
        assert_eq!(
            settle(vec![Err("denied".into()), Ok(receiver)]).unwrap_err(),
            "denied"
        );
        worker.join().unwrap();
    }

    #[test]
    fn partial_write_revocation_stops_remaining_bytes_without_retry() {
        struct Partial {
            allowed: Arc<AtomicBool>,
            output: Vec<u8>,
        }
        impl Write for Partial {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.output.push(bytes[0]);
                self.allowed.store(false, Ordering::SeqCst);
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                panic!("revoked input must not flush")
            }
        }
        let allowed = Arc::new(AtomicBool::new(true));
        let current = allowed.clone();
        let authority = InputAuthority::new(move || {
            current
                .load(Ordering::SeqCst)
                .then_some(())
                .ok_or("revoked".into())
        });
        let mut writer = Partial {
            allowed,
            output: Vec::new(),
        };
        assert!(authority.write(&mut writer, b"abc").is_err());
        assert_eq!(writer.output, b"a");
    }

    #[test]
    fn admitted_input_writes_literal_bytes() {
        let authority = InputAuthority::new(|| Ok(()));
        let mut output = Vec::new();
        authority.write(&mut output, b"\0\xff\n").unwrap();
        assert_eq!(output, b"\0\xff\n");
    }
}
