//! Bursts of alerts reduced to what a person can take in.
//!
//! Bells within half a second ring once, a notification repeated within a few
//! seconds shows once, and only a few notifications show in any ten seconds.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bmux_plugin::{AttachAlert, AttachNotification};

/// Bells closer together than this ring once.
const BELL_INTERVAL: Duration = Duration::from_millis(500);
/// The same notification within this long shows once.
const REPEAT_WINDOW: Duration = Duration::from_secs(3);
/// At most [`BURST_LIMIT`] notifications show within this long.
const BURST_WINDOW: Duration = Duration::from_secs(10);
const BURST_LIMIT: usize = 4;

/// What an alert gate lets through at once.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AdmittedAlerts {
    pub bell: bool,
    pub notifications: Vec<AttachNotification>,
}

impl AdmittedAlerts {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.bell && self.notifications.is_empty()
    }
}

/// Limits the alerts one terminal shows over time.
#[derive(Debug, Default)]
pub struct AlertGate {
    last_bell: Option<Instant>,
    shown: VecDeque<(Instant, AttachNotification)>,
}

impl AlertGate {
    /// What of `alerts`, posted by `now`, to show.
    pub fn admit(&mut self, alerts: Vec<AttachAlert>, now: Instant) -> AdmittedAlerts {
        while self
            .shown
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= BURST_WINDOW)
        {
            self.shown.pop_front();
        }
        let mut admitted = AdmittedAlerts::default();
        for alert in alerts {
            if alert.bell
                && !admitted.bell
                && self
                    .last_bell
                    .is_none_or(|at| now.duration_since(at) >= BELL_INTERVAL)
            {
                admitted.bell = true;
                self.last_bell = Some(now);
            }
            let Some(notification) = alert.notification else {
                continue;
            };
            let repeated = self.shown.iter().any(|(at, shown)| {
                *shown == notification && now.duration_since(*at) < REPEAT_WINDOW
            }) || admitted.notifications.contains(&notification);
            if repeated || self.shown.len() >= BURST_LIMIT {
                continue;
            }
            self.shown.push_back((now, notification.clone()));
            admitted.notifications.push(notification);
        }
        admitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursts_ring_once_and_repeats_show_once() {
        let mut gate = AlertGate::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let shown = gate.admit(
            vec![
                AttachAlert::bell(),
                AttachAlert::bell(),
                AttachAlert::notification("bmux", "claude finished"),
                AttachAlert::notification("bmux", "claude finished"),
            ],
            at(0),
        );
        assert!(shown.bell);
        assert_eq!(shown.notifications.len(), 1);
        // Within half a second and three seconds: nothing new.
        let shown = gate.admit(
            vec![
                AttachAlert::bell(),
                AttachAlert::notification("bmux", "claude finished"),
            ],
            at(300),
        );
        assert!(shown.is_empty());
        // Later, both again.
        let shown = gate.admit(
            vec![
                AttachAlert::bell(),
                AttachAlert::notification("bmux", "claude finished"),
            ],
            at(4_000),
        );
        assert!(shown.bell);
        assert_eq!(shown.notifications.len(), 1);
    }

    #[test]
    fn only_a_few_notifications_show_in_ten_seconds() {
        let mut gate = AlertGate::default();
        let start = Instant::now();
        let flood = (0..10)
            .map(|index| AttachAlert::notification("bmux", format!("tab {index}")))
            .collect();
        assert_eq!(gate.admit(flood, start).notifications.len(), BURST_LIMIT);
        assert!(
            gate.admit(
                vec![AttachAlert::notification("bmux", "more")],
                start + Duration::from_secs(5)
            )
            .is_empty()
        );
        assert_eq!(
            gate.admit(
                vec![AttachAlert::notification("bmux", "more")],
                start + Duration::from_secs(10)
            )
            .notifications
            .len(),
            1
        );
    }
}
