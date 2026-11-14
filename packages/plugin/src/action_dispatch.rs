//! Action dispatch channel — allows async plugin code to trigger runtime
//! actions in the attach loop.
//!
//! The data types are defined in [`bmux_plugin_sdk::action_dispatch`].
//! This module adds the process-global host channel that connects dispatch
//! callers (plugins, async tasks) to the attach loop.

#![allow(dead_code)]

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc;

pub use bmux_plugin_sdk::action_dispatch::ActionDispatchRequest;

// ── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionDispatchError {
    HostUnavailable,
    HostDisconnected,
    Full,
    TooLarge,
    OwnerClosed,
}

impl fmt::Display for ActionDispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable => f.write_str("action dispatch host unavailable"),
            Self::HostDisconnected => f.write_str("action dispatch host disconnected"),
            Self::Full => f.write_str("action dispatch queue full"),
            Self::TooLarge => f.write_str("action dispatch exceeds size bound"),
            Self::OwnerClosed => f.write_str("action dispatch owner closed"),
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
/// # Errors
///
/// Returns [`ActionDispatchError::HostUnavailable`] if no host is registered,
/// or [`ActionDispatchError::HostDisconnected`] if the channel is closed.
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

/// Explicit activation-owned action channel. No singleton is consulted.
#[derive(Clone)]
pub struct ActionDispatchPort {
    owner: bmux_plugin_runtime::ActivationScope,
    sender: mpsc::Sender<ActionDispatchRequest>,
}

pub struct ActionDispatchInbox {
    owner: bmux_plugin_runtime::ActivationScope,
    receiver: mpsc::Receiver<ActionDispatchRequest>,
}

impl ActionDispatchPort {
    /// The receiver belongs to exactly one frontend activation. Closing that
    /// activation invalidates queued work as well as retained sender clones.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn channel(
        owner: bmux_plugin_runtime::ActivationScope,
        capacity: usize,
    ) -> Result<(Self, ActionDispatchInbox), ActionDispatchError> {
        if !(1..=1024).contains(&capacity) {
            return Err(ActionDispatchError::Full);
        }
        let _admission = owner
            .try_admit()
            .map_err(|_| ActionDispatchError::OwnerClosed)?;
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self {
                owner: owner.clone(),
                sender,
            },
            ActionDispatchInbox { owner, receiver },
        ))
    }

    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn dispatch(&self, action: impl Into<String>) -> Result<(), ActionDispatchError> {
        let request = ActionDispatchRequest::new(action);
        if request.action.len() > 64 * 1024 {
            return Err(ActionDispatchError::TooLarge);
        }
        let _admission = self
            .owner
            .try_admit()
            .map_err(|_| ActionDispatchError::OwnerClosed)?;
        self.sender.try_send(request).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ActionDispatchError::Full,
            mpsc::error::TrySendError::Closed(_) => ActionDispatchError::HostDisconnected,
        })
    }
}

impl ActionDispatchInbox {
    /// Drain at most one queued action without suspending the frame loop.
    ///
    /// # Errors
    /// Rejects closed owners and disconnected senders; empty is `Ok(None)`.
    pub fn try_recv(&mut self) -> Result<Option<AdmittedAction>, ActionDispatchError> {
        let admission = self
            .owner
            .try_admit()
            .map_err(|_| ActionDispatchError::OwnerClosed)?;
        match self.receiver.try_recv() {
            Ok(request) => Ok(Some(AdmittedAction {
                request,
                _admission: admission,
            })),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                Err(ActionDispatchError::HostDisconnected)
            }
        }
    }

    /// Runtime must dispatch through this admitted envelope, not detach queued
    /// work from its owner. Closing admission stops subsequent receives.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub async fn recv(&mut self) -> Result<AdmittedAction, ActionDispatchError> {
        let request = tokio::select! {
            result = self.owner.cancelled() => {
                let _ = result;
                self.receiver.close();
                return Err(ActionDispatchError::OwnerClosed);
            }
            request = self.receiver.recv() => request.ok_or(ActionDispatchError::HostDisconnected)?,
        };
        let admission = self
            .owner
            .try_admit()
            .map_err(|_| ActionDispatchError::OwnerClosed)?;
        Ok(AdmittedAction {
            request,
            _admission: admission,
        })
    }
}

pub struct AdmittedAction {
    request: ActionDispatchRequest,
    _admission: bmux_plugin_runtime::AdmissionGuard,
}

impl std::ops::Deref for AdmittedAction {
    type Target = ActionDispatchRequest;
    fn deref(&self) -> &Self::Target {
        &self.request
    }
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

        dispatch("plugin:bmux.tabs:goto-tab 3").expect("dispatch should succeed");

        let request = rx.recv().await.expect("host should receive request");
        assert_eq!(request.action, "plugin:bmux.tabs:goto-tab 3");
    }

    #[tokio::test]
    async fn scoped_actions_reject_overflow_and_closed_owners() {
        use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
        let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()));
        let owner = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        owner.mark_active().unwrap();
        let (port, mut inbox) = ActionDispatchPort::channel(owner.clone(), 1).unwrap();
        port.dispatch("first").unwrap();
        assert_eq!(port.dispatch("second"), Err(ActionDispatchError::Full));
        let action = inbox.recv().await.unwrap();
        assert_eq!(action.action, "first");
        drop(action);
        port.dispatch("queued").unwrap();
        owner.close().unwrap();
        assert_eq!(
            port.dispatch("closed"),
            Err(ActionDispatchError::OwnerClosed)
        );
        assert!(matches!(
            inbox.recv().await,
            Err(ActionDispatchError::OwnerClosed)
        ));
        owner.drain().await.unwrap();
        owner.finish().unwrap();
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
