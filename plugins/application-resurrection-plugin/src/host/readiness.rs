//! Startup coordination is not authorization. Callers re-probe process ownership
//! after waking and again at the state-changing boundary.
use super::{CommandPhase, Entry};
use crate::process::{self, Identity};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) struct Rejection {
    pub code: &'static str,
    pub detail: String,
}

impl Rejection {
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

pub(super) fn wait(entry: &Entry, shell: Identity, deadline: Instant) -> Result<(), Rejection> {
    wait_observed(entry, shell, deadline, || {})
}

fn wait_observed(
    entry: &Entry,
    shell: Identity,
    deadline: Instant,
    mut on_pending: impl FnMut(),
) -> Result<(), Rejection> {
    let mut state = entry
        .state
        .lock()
        .map_err(|error| Rejection::new("state_unavailable", error.to_string()))?;
    loop {
        if entry.stopped.load(Ordering::SeqCst) {
            return Err(Rejection::new(
                "execution_stopped",
                "launch stopped while awaiting readiness",
            ));
        }
        if !process::current(shell) {
            return Err(Rejection::new(
                "shell_identity_changed",
                "requesting shell is no longer current",
            ));
        }
        let pending = if let Some(owner) = state.identity {
            if !process::current(owner) {
                return Err(Rejection::new(
                    "owner_identity_changed",
                    "owner process is no longer current",
                ));
            }
            if owner == shell {
                // Root hooks acknowledge startup, not an active command. Waiting
                // for Active here would prevent the first prompt from completing.
                return Ok(());
            }
            if !state.shell {
                return Err(Rejection::new(
                    "owner_not_shell",
                    "launch does not own shell boundaries",
                ));
            }
            if state.command_phase == CommandPhase::Active {
                return Ok(());
            }
            "command_boundary_not_ready"
        } else {
            "launch_not_bound"
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Rejection::new(pending, "shell readiness deadline expired"));
        }
        // wait_timeout releases the state lock; bind/OSC handling never waits
        // for this callback. A short bounded wake also detects dead processes
        // and failure paths that only set the existing stopped atomic.
        on_pending();
        (state, _) = entry
            .changed
            .wait_timeout(state, remaining.min(Duration::from_millis(50)))
            .map_err(|error| Rejection::new("state_unavailable", error.to_string()))?;
    }
}
