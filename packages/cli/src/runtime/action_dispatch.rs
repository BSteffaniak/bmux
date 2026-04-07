#![allow(dead_code)]

//! Action dispatch channel — allows async plugin code to trigger runtime
//! actions in the attach loop.
//!
//! This follows the same global-host-registration pattern as the prompt
//! system ([`super::prompt`]).  Any code running in the same process (such
//! as a plugin task spawned from [`RustPlugin::run_command`]) can call
//! [`dispatch`] to send an action string to the attach loop, which will
//! parse it as a `RuntimeAction` and execute it through the normal dispatch
//! path.
//!
//! # Usage
//!
//! The attach loop registers itself as the dispatch host on startup and
//! drops the guard on exit:
//!
//! ```ignore
//! let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
//! let _guard = action_dispatch::register_host(tx);
//! ```
//!
//! Plugin code dispatches actions:
//!
//! ```ignore
//! action_dispatch::dispatch("plugin:bmux.plugin_cli:recording-cut --last-seconds 30")?;
//! ```

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc;

pub use bmux_plugin_sdk::action_dispatch::ActionDispatchRequest;

// ── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionDispatchError {
    /// No attach loop has registered as the dispatch host.
    HostUnavailable,
    /// The dispatch host channel has been dropped.
    HostDisconnected,
}

impl fmt::Display for ActionDispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable => f.write_str("action dispatch host unavailable"),
            Self::HostDisconnected => f.write_str("action dispatch host disconnected"),
        }
    }
}

impl Error for ActionDispatchError {}

// ── Host registration ────────────────────────────────────────────────────────

#[derive(Clone)]
struct HostRegistration {
    id: u64,
    sender: mpsc::UnboundedSender<ActionDispatchRequest>,
}

static HOST_REGISTRY: OnceLock<Mutex<Option<HostRegistration>>> = OnceLock::new();
static HOST_REGISTRATION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn host_registry() -> &'static Mutex<Option<HostRegistration>> {
    HOST_REGISTRY.get_or_init(|| Mutex::new(None))
}

/// RAII guard that unregisters the dispatch host on drop.
#[derive(Debug)]
pub struct ActionDispatchHostGuard {
    id: u64,
}

impl Drop for ActionDispatchHostGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = host_registry().lock()
            && slot.as_ref().is_some_and(|reg| reg.id == self.id)
        {
            *slot = None;
        }
    }
}

/// Register the attach loop as the action dispatch host.
///
/// Only one host can be registered at a time.  Dropping the returned guard
/// unregisters the host.
pub fn register_host(
    sender: mpsc::UnboundedSender<ActionDispatchRequest>,
) -> ActionDispatchHostGuard {
    let id = HOST_REGISTRATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut slot) = host_registry().lock() {
        *slot = Some(HostRegistration { id, sender });
    }
    ActionDispatchHostGuard { id }
}

/// Dispatch an action string to the attach loop.
///
/// The string uses the same format as keybinding action values
/// (e.g. `"focus_next_pane"`, `"plugin:bmux.windows:goto-window 1"`).
///
/// Returns immediately — the action is processed asynchronously by the
/// attach loop on its next event iteration.
pub fn dispatch(action: impl Into<String>) -> Result<(), ActionDispatchError> {
    let request = ActionDispatchRequest::new(action);

    let guard = host_registry()
        .lock()
        .map_err(|_| ActionDispatchError::HostDisconnected)?;
    let sender = guard
        .as_ref()
        .map(|reg| reg.sender.clone())
        .ok_or(ActionDispatchError::HostUnavailable)?;
    drop(guard);

    sender
        .send(request)
        .map_err(|_| ActionDispatchError::HostDisconnected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn dispatch_fails_when_no_host_is_registered() {
        let result = dispatch("focus_next_pane");
        assert_eq!(result, Err(ActionDispatchError::HostUnavailable));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn dispatch_sends_to_registered_host() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _guard = register_host(tx);

        dispatch("plugin:bmux.windows:goto-window 3").expect("dispatch should succeed");

        let request = rx.recv().await.expect("host should receive request");
        assert_eq!(request.action, "plugin:bmux.windows:goto-window 3");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn dropping_guard_unregisters_host() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let guard = register_host(tx);
        drop(guard);

        let result = dispatch("focus_next_pane");
        assert!(matches!(result, Err(ActionDispatchError::HostUnavailable)));
    }
}
