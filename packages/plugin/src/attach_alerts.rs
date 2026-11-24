//! Alerts for the person at an attached terminal: its bell and desktop
//! notifications.
//!
//! Plugins post an [`AttachAlert`] to their runtime's [`AttachAlerts`] (found
//! with [`attach_alerts`] in the runtime's shared state). The attach runtime
//! takes what was posted, limits bursts, and has its terminal ring the bell or
//! show a notification in whatever way that terminal supports. Each attach
//! client alerts its own terminal: two terminals attached to one session decide
//! independently.

use std::sync::{Arc, RwLock};

/// A desktop notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachNotification {
    pub title: String,
    pub body: String,
}

/// One alert: ring the bell, show a notification, or both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachAlert {
    pub bell: bool,
    pub notification: Option<AttachNotification>,
}

impl AttachAlert {
    /// Ring the bell.
    #[must_use]
    pub const fn bell() -> Self {
        Self {
            bell: true,
            notification: None,
        }
    }

    /// Show a notification.
    #[must_use]
    pub fn notification(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            bell: false,
            notification: Some(AttachNotification {
                title: title.into(),
                body: body.into(),
            }),
        }
    }
}

/// Which panes' bells reach the terminal (`behavior.bell_action`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AttachBellPolicy {
    /// None.
    None,
    /// Every pane's.
    #[default]
    Any,
    /// Only the tab this client is looking at.
    Current,
    /// Only tabs this client is not looking at.
    Other,
}

impl AttachBellPolicy {
    /// Whether a bell from a tab this client is (or is not) `viewing` rings.
    #[must_use]
    pub const fn admits(self, viewing: bool) -> bool {
        match self {
            Self::None => false,
            Self::Any => true,
            Self::Current => viewing,
            Self::Other => !viewing,
        }
    }
}

/// The most alerts kept waiting; older ones are dropped first.
const MAX_PENDING: usize = 32;

/// One attach runtime's alerts.
#[derive(Debug, Default)]
pub struct AttachAlerts {
    pending: Vec<AttachAlert>,
    wake: Arc<tokio::sync::Notify>,
    bell_policy: AttachBellPolicy,
    /// The terminal reported losing focus (another window or app is in
    /// front of it), and has not reported regaining it.
    terminal_unfocused: bool,
}

impl AttachAlerts {
    /// Post `alert` for the terminal, waking the runtime.
    pub fn post(&mut self, alert: AttachAlert) {
        if !alert.bell && alert.notification.is_none() {
            return;
        }
        if self.pending.len() == MAX_PENDING {
            self.pending.remove(0);
        }
        self.pending.push(alert);
        self.wake.notify_one();
    }

    /// Take everything posted so far.
    pub fn take(&mut self) -> Vec<AttachAlert> {
        std::mem::take(&mut self.pending)
    }

    /// Notified whenever an alert is posted.
    #[must_use]
    pub fn wake(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.wake)
    }

    /// Which panes' bells should reach this runtime's terminal.
    #[must_use]
    pub const fn bell_policy(&self) -> AttachBellPolicy {
        self.bell_policy
    }

    /// Set from the runtime's configuration.
    pub const fn set_bell_policy(&mut self, policy: AttachBellPolicy) {
        self.bell_policy = policy;
    }

    /// Whether the terminal has focus: what this client shows is in front
    /// of the user. Assumed until the terminal reports otherwise.
    #[must_use]
    pub const fn terminal_focused(&self) -> bool {
        !self.terminal_unfocused
    }

    /// Set from the terminal's focus reports.
    pub const fn set_terminal_focused(&mut self, focused: bool) {
        self.terminal_unfocused = !focused;
    }
}

/// The runtime's alerts, from its shared state (see
/// [`crate::AttachPresentationResources::states`]).
#[must_use]
pub fn attach_alerts(states: &crate::PluginStateRegistry) -> Arc<RwLock<AttachAlerts>> {
    states.get_or_insert_with(AttachAlerts::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posting_queues_and_wakes_and_bounds_what_waits() {
        let states = crate::PluginStateRegistry::new();
        let alerts = attach_alerts(&states);
        let wake = alerts.read().unwrap().wake();
        alerts.write().unwrap().post(AttachAlert::bell());
        // A post wakes whoever waits next, even one that starts waiting later.
        let woken = futures_lite_poll(wake.notified());
        assert!(woken);
        // The same instance is shared through the runtime's state.
        assert_eq!(
            attach_alerts(&states).write().unwrap().take(),
            [AttachAlert::bell()]
        );
        // Empty alerts are ignored; a flood keeps the newest.
        let taken = {
            let mut alerts = alerts.write().unwrap();
            alerts.post(AttachAlert::default());
            for index in 0..40 {
                alerts.post(AttachAlert::notification("t", index.to_string()));
            }
            alerts.take()
        };
        assert_eq!(taken.len(), MAX_PENDING);
        assert_eq!(taken[0], AttachAlert::notification("t", "8"));
    }

    /// Whether `future` is ready on its first poll.
    fn futures_lite_poll(future: impl std::future::Future<Output = ()>) -> bool {
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        future.as_mut().poll(&mut context).is_ready()
    }

    #[test]
    fn terminal_focus_is_assumed_until_reported_lost() {
        let mut alerts = AttachAlerts::default();
        assert!(alerts.terminal_focused());
        alerts.set_terminal_focused(false);
        assert!(!alerts.terminal_focused());
        alerts.set_terminal_focused(true);
        assert!(alerts.terminal_focused());
    }

    #[test]
    fn bell_policies_admit_by_whether_the_tab_is_viewed() {
        for (policy, viewing, other) in [
            (AttachBellPolicy::None, false, false),
            (AttachBellPolicy::Any, true, true),
            (AttachBellPolicy::Current, true, false),
            (AttachBellPolicy::Other, false, true),
        ] {
            assert_eq!(policy.admits(true), viewing, "{policy:?}");
            assert_eq!(policy.admits(false), other, "{policy:?}");
        }
    }
}
