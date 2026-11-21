//! Retained request authority for queued terminal input.
use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use switchy_thread::sync::Arc;

type WriteReceipt = switchy_thread::sync::mpsc::Receiver<Result<(), String>>;
const MAX_PENDING_WRITES: usize = 64;
const MAX_INPUT_BYTES: usize = 65_536;
const WRITE_SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(10);
// Shared capacity only: caller authority is captured separately in each command.
// Process-wide because it bounds OS PTY writer work across every activation.
static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

/// Process-wide PTY write queue capacity. It bounds OS writer threads and
/// memory across every activation, so it is a true process resource bound,
/// not per-instance state.
fn write_slots() -> Arc<tokio::sync::Semaphore> {
    Arc::clone(SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_WRITES))))
}

/// The input invocation one thread is synchronously executing for this
/// activation: the service context that authorizes local input and the PTY
/// write receipts it must settle before the invocation closes.
#[derive(Default)]
struct InvocationScope {
    context: Option<bmux_plugin_sdk::NativeServiceContext>,
    receipts: Option<Vec<Result<WriteReceipt, String>>>,
}

/// One activation's synchronous input invocations, keyed by the thread
/// executing each. Owned by the activation's host handle (never a
/// thread-local or process static), so an invocation of one instance never
/// authorizes or collects writes for another instance on the same thread.
#[derive(Default)]
pub(crate) struct InputScopes(
    switchy_thread::sync::Mutex<
        std::collections::HashMap<switchy_thread::thread::ThreadId, InvocationScope>,
    >,
);

impl InputScopes {
    fn with<R>(&self, f: impl FnOnce(&mut InvocationScope) -> R) -> R {
        let mut scopes = self
            .0
            .lock()
            .unwrap_or_else(switchy_thread::sync::PoisonError::into_inner);
        let id = switchy_thread::thread::current().id();
        let scope = scopes.entry(id).or_default();
        let result = f(scope);
        if scope.context.is_none() && scope.receipts.is_none() {
            scopes.remove(&id);
        }
        drop(scopes);
        result
    }

    fn replace_context(
        &self,
        context: Option<bmux_plugin_sdk::NativeServiceContext>,
    ) -> Option<bmux_plugin_sdk::NativeServiceContext> {
        self.with(|scope| std::mem::replace(&mut scope.context, context))
    }

    fn replace_receipts(
        &self,
        receipts: Option<Vec<Result<WriteReceipt, String>>>,
    ) -> Option<Vec<Result<WriteReceipt, String>>> {
        self.with(|scope| std::mem::replace(&mut scope.receipts, receipts))
    }

    fn context(&self) -> Option<bmux_plugin_sdk::NativeServiceContext> {
        self.with(|scope| scope.context.clone())
    }
}

pub(crate) fn with_local_settled<T>(
    host: &crate::runtime::PaneRuntimeHost,
    context: bmux_plugin_sdk::NativeServiceContext,
    effect: impl FnOnce() -> T,
) -> Result<T, String> {
    struct Restore<'a>(&'a InputScopes, Option<Vec<Result<WriteReceipt, String>>>);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.0.replace_receipts(self.1.take());
        }
    }
    let scopes = host.input_scopes();
    let restore = Restore(scopes, scopes.replace_receipts(Some(Vec::new())));
    let output = with_local(host, context.clone(), effect);
    let receipts = scopes.replace_receipts(None).unwrap_or_default();
    drop(restore);
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

/// Register one queued PTY write with the invocation's settlement collector.
/// The returned sender must receive the write's actual completion; the
/// invocation that authorizes the write stays retained until it does.
///
/// # Errors
/// Fails outside a settled invocation or beyond the admission bound: an
/// unsettled write would be re-authorized after its invocation closed.
pub(crate) fn settlement_receipt(
    host: &crate::runtime::PaneRuntimeHost,
) -> Result<switchy_thread::sync::mpsc::SyncSender<Result<(), String>>, String> {
    host.input_scopes().with(|scope| {
        let receipts = scope
            .receipts
            .as_mut()
            .ok_or_else(|| "PTY input settlement scope missing".to_string())?;
        if receipts.len() >= MAX_PENDING_WRITES - 1 {
            return Err("PTY input admission bound exceeded".to_string());
        }
        let (sender, receiver) = switchy_thread::sync::mpsc::sync_channel(1);
        receipts.push(Ok(receiver));
        Ok(sender)
    })
}

pub(crate) fn settled_local_command(
    host: &crate::runtime::PaneRuntimeHost,
    bytes: Vec<u8>,
    session: bmux_session_models::SessionId,
) -> Result<super::PaneRuntimeCommand, super::SessionRuntimeError> {
    let scopes = host.input_scopes();
    let (settling, pending, context) = scopes.with(|scope| {
        (
            scope.receipts.is_some(),
            scope.receipts.as_ref().map_or(0, Vec::len),
            scope.context.clone(),
        )
    });
    if !settling {
        return Ok(local_command(host, bytes, session));
    }
    let prepare = || {
        if pending >= MAX_PENDING_WRITES - 1 || bytes.len() > MAX_INPUT_BYTES {
            return Err("PTY input admission bound exceeded".to_string());
        }
        let permit = write_slots()
            .try_acquire_owned()
            .map_err(|_| "PTY input queue full".to_string())?;
        // The outer clone keeps the service context alive through receipt
        // settlement, not merely until command construction.
        let authority = context
            .as_ref()
            .map(|context| {
                crate::handlers::execution::local_input_authority(host, context, session.0)
            })
            .ok_or_else(|| "PTY input invocation missing".to_string())?;
        let (sender, receiver) = switchy_thread::sync::mpsc::sync_channel(1);
        Ok((
            super::PaneRuntimeCommand::AdmittedInput(bytes, sender, permit, authority),
            receiver,
        ))
    };
    let prepared = prepare();
    scopes.with(|scope| {
        let Some(receipts) = scope.receipts.as_mut() else {
            return Err(super::SessionRuntimeError::Closed);
        };
        match prepared {
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

pub(crate) fn with_local<T>(
    host: &crate::runtime::PaneRuntimeHost,
    context: bmux_plugin_sdk::NativeServiceContext,
    effect: impl FnOnce() -> T,
) -> T {
    struct Restore<'a>(
        &'a InputScopes,
        Option<bmux_plugin_sdk::NativeServiceContext>,
    );
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.0.replace_context(self.1.take());
        }
    }
    let scopes = host.input_scopes();
    let _restore = Restore(scopes, scopes.replace_context(Some(context)));
    effect()
}

pub(crate) fn local_command(
    host: &crate::runtime::PaneRuntimeHost,
    bytes: Vec<u8>,
    session: bmux_session_models::SessionId,
) -> super::PaneRuntimeCommand {
    match host.input_scopes().context() {
        Some(context) => super::PaneRuntimeCommand::AuthorizedInput(
            bytes,
            crate::handlers::execution::local_input_authority(host, &context, session.0),
        ),
        // In-process local input has no remote service caller. Remote legacy
        // routes are rejected at ingress, independently of permissions plugins.
        None => super::PaneRuntimeCommand::Input(bytes),
    }
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
    use switchy_thread::sync::atomic::{AtomicBool, Ordering};

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
            let (tx, rx) = switchy_thread::sync::mpsc::channel();
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
        let (sender, receiver) = switchy_thread::sync::mpsc::sync_channel(1);
        sender.send(Ok(())).unwrap();
        assert!(settle(vec![Ok(receiver)]).is_ok());
        let (sender, receiver) = switchy_thread::sync::mpsc::sync_channel(1);
        drop(sender);
        let error = settle(vec![Ok(receiver)]).unwrap_err();
        assert!(error.contains("completion unconfirmed"));
        assert!(error.contains("do not replay"));
    }

    #[test]
    fn settlement_drains_other_chunks_after_known_failure() {
        let (sender, receiver) = switchy_thread::sync::mpsc::sync_channel(0);
        let worker = switchy_thread::thread::spawn(move || sender.send(Ok(())).unwrap());
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
    fn execution_write_receipts_require_and_join_invocation_settlement() {
        // Outside a settled invocation the write would be re-authorized after
        // its invocation closed, so it is refused rather than queued unsettled.
        let host = crate::runtime::PaneRuntimeHost::detached();
        assert!(settlement_receipt(&host).is_err());
        host.input_scopes().replace_receipts(Some(Vec::new()));
        let completion = settlement_receipt(&host).unwrap();
        let receipts = host.input_scopes().replace_receipts(None).unwrap();
        assert_eq!(receipts.len(), 1);
        completion
            .send(Err("authority denied at write".into()))
            .unwrap();
        // The write's actual failure reaches the invocation's response.
        assert_eq!(settle(receipts).unwrap_err(), "authority denied at write");
    }

    #[test]
    fn admitted_input_writes_literal_bytes() {
        let authority = InputAuthority::new(|| Ok(()));
        let mut output = Vec::new();
        authority.write(&mut output, b"\0\xff\n").unwrap();
        assert_eq!(output, b"\0\xff\n");
    }
}
