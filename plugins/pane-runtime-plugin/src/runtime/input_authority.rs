//! Retained request authority for queued terminal input.
use std::io::{self, Write};
use std::sync::Arc;

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
