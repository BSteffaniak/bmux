//! Last-chance cleanup of pane sessions when the server dies abnormally.
//!
//! Each live pane registers its session ID and PTY master descriptor in a
//! fixed-size, lock-free table. Two process-wide hooks walk that table:
//!
//! - a `SIGABRT` handler, which covers `abort()`, `panic = "abort"` profiles and
//!   Rust's stack-overflow handler (which aborts);
//! - an `atexit` hook, which covers `exit()` while panes are still registered.
//!
//! Both only make async-signal-safe calls: `kill` on the session's own process
//! group and on the PTY's current foreground group. Background groups that
//! ignore `SIGHUP` are left to the startup sweep of the next server, which reads
//! the crash records written by `session_control`.
//!
//! `SIGSEGV`, `SIGBUS`, `SIGILL` and `SIGFPE` are deliberately not hooked:
//! wasmtime handles traps through those signals on Linux, and a handler placed in
//! front of it would kill every pane on an ordinary guest trap.
use std::sync::OnceLock;
use switchy_thread::sync::atomic::{AtomicI32, Ordering};

const SLOTS: usize = 4096;

static SESSIONS: [AtomicI32; SLOTS] = [const { AtomicI32::new(0) }; SLOTS];
static MASTERS: [AtomicI32; SLOTS] = [const { AtomicI32::new(-1) }; SLOTS];
static PREVIOUS_ABORT: OnceLock<libc::sigaction> = OnceLock::new();
static INSTALLED: OnceLock<()> = OnceLock::new();

/// One registered pane session; dropping it removes the registration.
#[derive(Debug)]
pub(crate) struct Registration(usize);

impl Drop for Registration {
    fn drop(&mut self) {
        MASTERS[self.0].store(-1, Ordering::SeqCst);
        SESSIONS[self.0].store(0, Ordering::SeqCst);
    }
}

/// Register a live pane session. `None` when the table is full; the startup
/// sweep still covers such a pane after a crash.
pub(crate) fn register(sid: i32, master_fd: Option<i32>) -> Option<Registration> {
    if sid <= 1 {
        return None;
    }
    install();
    for slot in 0..SLOTS {
        if SESSIONS[slot]
            .compare_exchange(0, sid, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            MASTERS[slot].store(master_fd.unwrap_or(-1), Ordering::SeqCst);
            return Some(Registration(slot));
        }
    }
    tracing::warn!(sid, "pane crash-guard table is full");
    None
}

fn install() {
    INSTALLED.get_or_init(|| {
        // SAFETY: zeroed sigaction is valid output storage.
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: querying the current disposition into valid storage.
        if unsafe { libc::sigaction(libc::SIGABRT, std::ptr::null(), &raw mut previous) } == 0 {
            let _ = PREVIOUS_ABORT.set(previous);
            // SAFETY: zeroed sigaction is a valid starting value.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = on_abort as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO;
            // SAFETY: valid storage; an empty mask.
            unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
            // SAFETY: installing a handler that only makes async-signal-safe calls.
            if unsafe { libc::sigaction(libc::SIGABRT, &raw const action, std::ptr::null_mut()) }
                != 0
            {
                tracing::warn!(
                    error = %std::io::Error::last_os_error(),
                    "pane crash guard could not hook SIGABRT"
                );
            }
        }
        // SAFETY: registering a plain function pointer.
        if unsafe { libc::atexit(on_exit) } != 0 {
            tracing::warn!("pane crash guard could not register an exit hook");
        }
    });
}

/// Kill every registered session's own group and its terminal's foreground
/// group. Only async-signal-safe calls.
fn kill_registered() {
    for slot in 0..SLOTS {
        let sid = SESSIONS[slot].load(Ordering::SeqCst);
        if sid <= 1 {
            continue;
        }
        // SAFETY: kill is async-signal-safe; the group is the live session's.
        unsafe { libc::kill(-sid, libc::SIGKILL) };
        let master = MASTERS[slot].load(Ordering::SeqCst);
        if master >= 0 {
            // SAFETY: tcgetpgrp is async-signal-safe; a stale descriptor fails.
            let group = unsafe { libc::tcgetpgrp(master) };
            if group > 1 {
                // SAFETY: kill is async-signal-safe.
                unsafe { libc::kill(-group, libc::SIGKILL) };
            }
        }
    }
}

extern "C" fn on_abort(
    signal: libc::c_int,
    _info: *mut libc::siginfo_t,
    _context: *mut libc::c_void,
) {
    kill_registered();
    // Hand the abort back to whatever handled it before us (normally the
    // default action) and re-raise so the process still terminates.
    if let Some(previous) = PREVIOUS_ABORT.get() {
        // SAFETY: restoring a disposition captured from sigaction itself.
        unsafe { libc::sigaction(signal, previous, std::ptr::null_mut()) };
    }
    // SAFETY: raise is async-signal-safe.
    unsafe { libc::raise(signal) };
}

extern "C" fn on_exit() {
    kill_registered();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrations_are_released_on_drop() {
        let registration = register(424_242, Some(-1)).expect("free slot");
        let slot = registration.0;
        assert_eq!(SESSIONS[slot].load(Ordering::SeqCst), 424_242);
        drop(registration);
        assert_eq!(SESSIONS[slot].load(Ordering::SeqCst), 0);
        assert_eq!(MASTERS[slot].load(Ordering::SeqCst), -1);
    }

    #[test]
    fn invalid_sessions_are_not_registered() {
        assert!(register(0, None).is_none());
        assert!(register(1, None).is_none());
    }
}
