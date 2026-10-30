//! Scoped process signal ownership. Handlers only touch one lock-free atomic.
#[cfg(unix)]
use std::sync::atomic::Ordering;
use std::sync::{Arc, atomic::AtomicBool};

#[cfg(unix)]
mod unix {
    use super::{Arc, AtomicBool, Ordering};
    use std::sync::{Mutex, MutexGuard, TryLockError};
    use std::thread::JoinHandle;
    use std::time::Duration;

    static OWNER: Mutex<()> = Mutex::new(());
    static CANCELLED: AtomicBool = AtomicBool::new(false);
    // If the OS refuses restoration, no subsequent owner may overwrite evidence.
    static BROKEN: AtomicBool = AtomicBool::new(false);
    const SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGTSTP];

    extern "C" fn cancel(_: libc::c_int) {
        // AtomicBool is lock-free on supported Rust Unix targets. No allocation,
        // locks, runtime calls, cleanup, unwinding, or nonlocal control transfer.
        CANCELLED.store(true, Ordering::Relaxed);
    }

    pub(super) fn refresh(target: &AtomicBool) {
        if CANCELLED.load(Ordering::Relaxed) {
            target.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) struct Guard {
        _owner: MutexGuard<'static, ()>,
        saved: Vec<(libc::c_int, libc::sigaction)>,
        stop: Arc<AtomicBool>,
        bridge: Option<JoinHandle<()>>,
        target: Arc<AtomicBool>,
    }

    impl Guard {
        pub(crate) fn install(target: Arc<AtomicBool>) -> Result<Self, String> {
            let owner = match OWNER.try_lock() {
                Ok(owner) => owner,
                Err(TryLockError::WouldBlock) => return Err("benchmark signal ownership is already active; use run_from_with_cancel with caller-owned policy".into()),
                Err(TryLockError::Poisoned(_)) => return Err("benchmark signal ownership is poisoned".into()),
            };
            if BROKEN.load(Ordering::Relaxed) {
                return Err(
                    "previous benchmark signal restoration failed; refusing new signal ownership"
                        .into(),
                );
            }
            CANCELLED.store(false, Ordering::Relaxed);
            let mut guard = Self {
                _owner: owner,
                saved: Vec::with_capacity(SIGNALS.len()),
                stop: Arc::new(AtomicBool::new(false)),
                bridge: None,
                target,
            };
            for signal in SIGNALS {
                // SAFETY: zeroed sigaction/sigset_t are initialized below before
                // installation; the output action is initialized by sigaction.
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
                action.sa_sigaction = cancel as *const () as libc::sighandler_t;
                action.sa_flags = 0;
                // Block our handled signals during the tiny handler itself.
                unsafe {
                    libc::sigemptyset(&raw mut action.sa_mask);
                    for blocked in SIGNALS {
                        libc::sigaddset(&raw mut action.sa_mask, blocked);
                    }
                }
                // SAFETY: pointers reference initialized action and writable old
                // storage; ownership is serialized for the entire install/run/restore.
                if unsafe { libc::sigaction(signal, &raw const action, &raw mut previous) } != 0 {
                    let error = format!(
                        "install signal {signal}: {}",
                        std::io::Error::last_os_error()
                    );
                    return match guard.restore() {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(format!("{error}; {cleanup}")),
                    };
                }
                guard.saved.push((signal, previous));
            }
            let target = Arc::clone(&guard.target);
            let stop = Arc::clone(&guard.stop);
            // The runner contract accepts a caller-owned AtomicBool; bridge the
            // static handler flag without sharing heap pointers with a handler.
            guard.bridge = match std::thread::Builder::new()
                .name("bench-cancel".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        refresh(&target);
                        std::thread::park_timeout(Duration::from_millis(2));
                    }
                    refresh(&target);
                }) {
                Ok(thread) => Some(thread),
                Err(error) => {
                    return match guard.restore() {
                        Ok(()) => Err(format!("start cancellation bridge: {error}")),
                        Err(cleanup) => {
                            Err(format!("start cancellation bridge: {error}; {cleanup}"))
                        }
                    };
                }
            };
            Ok(guard)
        }

        pub(crate) fn restore(&mut self) -> Result<(), String> {
            let mut errors = Vec::new();
            // Restore the full original sigactions, not just handler pointers:
            // masks, flags, SA_SIGINFO and platform-specific fields remain intact.
            self.saved.retain(|(signal, previous)| {
                // SAFETY: previous is the exact action returned by sigaction;
                // no output storage is requested and the owner remains locked.
                if unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) } != 0 {
                    errors.push(format!(
                        "restore signal {signal}: {}",
                        std::io::Error::last_os_error()
                    ));
                    true
                } else {
                    false
                }
            });
            self.stop.store(true, Ordering::Relaxed);
            if let Some(bridge) = self.bridge.take() {
                bridge.thread().unpark();
                if bridge.join().is_err() {
                    errors.push("cancellation bridge panicked".into());
                }
            }
            refresh(&self.target);
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            // Explicit restore reports errors; Drop retries on unwinding/early exit.
            if self.restore().is_err() {
                BROKEN.store(true, Ordering::Relaxed);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        extern "C" fn previous_handler(_: libc::c_int) {}

        fn read_action(signal: libc::c_int) -> libc::sigaction {
            // SAFETY: sigaction initializes writable storage on success.
            let mut action = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::sigaction(signal, std::ptr::null(), &raw mut action) },
                0
            );
            action
        }

        fn assert_same(signal: libc::c_int, expected: &libc::sigaction) {
            let actual = read_action(signal);
            assert_eq!(actual.sa_sigaction, expected.sa_sigaction);
            assert_eq!(actual.sa_flags, expected.sa_flags);
            // Compare semantic mask membership, not uninitialized ABI padding.
            for member in 1..32 {
                assert_eq!(
                    unsafe { libc::sigismember(&raw const actual.sa_mask, member) },
                    unsafe { libc::sigismember(&raw const expected.sa_mask, member) }
                );
            }
        }

        #[test]
        fn dispositions_and_cancellation_subprocess() {
            const CHILD: &str = "BMUX_BENCH_SIGNAL_TEST_CHILD";
            if std::env::var_os(CHILD).is_none() {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "signals::unix::tests::dispositions_and_cancellation_subprocess",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .spawn()
                    .unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(15);
                loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        assert!(status.success(), "signal subprocess failed: {status}");
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        child.kill().unwrap();
                        let _ = child.wait();
                        panic!("signal subprocess timed out");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                return;
            }
            // All disposition mutation stays in this explicitly isolated child.
            let mut originals = Vec::new();
            for signal in SIGNALS {
                let original = read_action(signal);
                let mut action = original;
                action.sa_sigaction = previous_handler as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                unsafe {
                    libc::sigemptyset(&raw mut action.sa_mask);
                    libc::sigaddset(&raw mut action.sa_mask, libc::SIGUSR1);
                    assert_eq!(
                        libc::sigaction(signal, &raw const action, std::ptr::null_mut()),
                        0
                    );
                }
                originals.push((signal, original, read_action(signal)));
            }
            for signal in SIGNALS {
                let target = Arc::new(AtomicBool::new(false));
                let mut guard = Guard::install(Arc::clone(&target)).unwrap();
                assert!(Guard::install(Arc::new(AtomicBool::new(false))).is_err());
                // Caller-owned entry never competes for our signal owner.
                let error = crate::run_from_with_cancel(
                    ["bmux-bench", "run", "--samples", "0"],
                    Arc::clone(&target),
                )
                .unwrap_err();
                assert!(error.contains("samples"));
                // SAFETY: our scoped handler is installed for this signal.
                assert_eq!(unsafe { libc::raise(signal) }, 0);
                let deadline = std::time::Instant::now() + Duration::from_secs(1);
                while !target.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(target.load(Ordering::Relaxed));
                guard.restore().unwrap();
                drop(guard);
                for (signal, _, expected) in &originals {
                    assert_same(*signal, expected);
                }
            }
            // Successful reusable entry also restores dispositions before returning.
            #[cfg(feature = "internal")]
            {
                let code = crate::run_from([
                    "bmux-bench",
                    "run",
                    "--internal",
                    "--plain",
                    "--samples",
                    "1",
                    "--no-artifacts",
                ])
                .unwrap();
                assert!(code == 0 || code == 1);
                for (signal, _, expected) in &originals {
                    assert_same(*signal, expected);
                }
            }
            // Error and unwind paths restore too, without running terminal I/O.
            assert!(crate::run_from(["bmux-bench", "run", "--samples", "0"]).is_err());
            for (signal, _, expected) in &originals {
                assert_same(*signal, expected);
            }
            let _ = std::panic::catch_unwind(|| {
                let _guard = Guard::install(Arc::new(AtomicBool::new(false))).unwrap();
                panic!("exercise unwind restoration");
            });
            for (signal, original, expected) in &originals {
                assert_same(*signal, expected);
                unsafe {
                    assert_eq!(libc::sigaction(*signal, original, std::ptr::null_mut()), 0);
                }
            }
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::Guard;

pub(crate) fn refresh(target: &AtomicBool) {
    #[cfg(unix)]
    unix::refresh(target);
    #[cfg(not(unix))]
    let _ = target;
}

#[cfg(not(unix))]
pub(crate) struct Guard;
#[cfg(not(unix))]
impl Guard {
    pub(crate) fn install(_: Arc<AtomicBool>) -> Result<Self, String> {
        Err("scoped benchmark signal handling is unavailable on this platform; use run_from_with_cancel with caller-owned cancellation".into())
    }
    pub(crate) fn restore(&mut self) -> Result<(), String> {
        Ok(())
    }
}
