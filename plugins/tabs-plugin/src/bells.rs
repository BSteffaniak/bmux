//! Pane bells on this client's terminal, by `behavior.bell_action`.
//!
//! Runs inside each attach client. It follows how many bells each tab's panes
//! have rung (`tabs-bells`) and which tab this client shows
//! (`tabs-local-view`), and rings the terminal once for each new bell its
//! runtime's bell policy admits: from any tab, only the one shown, or only the
//! others. Counts it saw before it started are history.

use std::collections::HashMap;
use std::sync::Arc;

use bmux_plugin::{AttachAlert, AttachBellPolicy, AttachPresentationResources};
use bmux_tabs_plugin_api::{tabs_bells, tabs_local_view};
use uuid::Uuid;

/// What bells have been seen, and which are new.
#[derive(Debug, Default)]
pub struct BellWatch {
    /// Each tab's count when last seen; `None` until the first snapshot.
    seen: Option<HashMap<Uuid, u64>>,
}

impl BellWatch {
    /// Take `snapshot` while this client shows `viewing`; returns how many
    /// new bells `policy` admits.
    pub fn observe(
        &mut self,
        snapshot: &tabs_bells::TabBellsSnapshot,
        viewing: Option<Uuid>,
        policy: AttachBellPolicy,
    ) -> u64 {
        let current = snapshot
            .tabs
            .iter()
            .map(|tab| (tab.tab_id, tab.bells))
            .collect::<HashMap<_, _>>();
        let Some(seen) = self.seen.replace(current.clone()) else {
            return 0;
        };
        current
            .iter()
            .filter(|(tab, _)| policy.admits(viewing == Some(**tab)))
            .map(|(tab, bells)| {
                // A tab first seen now started counting after we did; a count
                // that went down belongs to a restarted program.
                let before = seen.get(tab).copied().unwrap_or(0);
                bells.saturating_sub(before)
            })
            .sum()
    }
}

/// The running bell watch of one attach runtime; dropping it stops it.
pub struct BellAlerts {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for BellAlerts {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl BellAlerts {
    /// Start watching bells for the runtime `resources` belong to.
    ///
    /// # Errors
    /// Returns when there is no async runtime to run in or a channel cannot be
    /// followed.
    pub fn install(resources: &AttachPresentationResources) -> Result<Self, String> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|error| format!("tab bells need an async runtime: {error}"))?;
        let events = Arc::clone(&resources.events);
        let (_, mut bells) = subscribe_bells(&events)?;
        let mut selection = events
            .subscribe_state::<tabs_local_view::TabSelection>(&tabs_local_view::STATE_KIND)
            .ok()
            .map(|(_, receiver)| receiver);
        let alerts = bmux_plugin::attach_alerts(&resources.states);
        let task = handle.spawn(async move {
            let mut watch = BellWatch::default();
            let mut viewing = selection
                .as_ref()
                .and_then(|receiver| receiver.borrow().context_id);
            let snapshot = bells.borrow_and_update().clone();
            let policy = |alerts: &std::sync::RwLock<bmux_plugin::AttachAlerts>| {
                alerts
                    .read()
                    .map_or(AttachBellPolicy::None, |alerts| alerts.bell_policy())
            };
            watch.observe(&snapshot, viewing, policy(&alerts));
            loop {
                tokio::select! {
                    changed = bells.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let snapshot = bells.borrow_and_update().clone();
                        let rings = watch.observe(&snapshot, viewing, policy(&alerts));
                        if rings > 0
                            && let Ok(mut alerts) = alerts.write()
                        {
                            alerts.post(AttachAlert::bell());
                        }
                    }
                    changed = async {
                        match selection.as_mut() {
                            Some(receiver) => receiver.changed().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        match changed {
                            Ok(()) => {
                                viewing = selection
                                    .as_ref()
                                    .and_then(|receiver| receiver.borrow().context_id);
                            }
                            Err(_) => selection = None,
                        }
                    }
                }
            }
        });
        Ok(Self { task })
    }
}

type BellsSubscription = (
    Arc<tabs_bells::TabBellsSnapshot>,
    tokio::sync::watch::Receiver<Arc<tabs_bells::TabBellsSnapshot>>,
);

/// Follow the forwarded `tabs-bells` state, registering its decoder when
/// nothing has yet.
fn subscribe_bells(events: &Arc<bmux_plugin::EventBus>) -> Result<BellsSubscription, String> {
    if let Ok(subscription) =
        events.subscribe_state::<tabs_bells::TabBellsSnapshot>(&tabs_bells::STATE_KIND)
    {
        return Ok(subscription);
    }
    events.register_state_channel_with_decoder(
        tabs_bells::STATE_KIND,
        tabs_bells::TabBellsSnapshot {
            tabs: Vec::new(),
            revision: 0,
        },
    );
    events
        .subscribe_state::<tabs_bells::TabBellsSnapshot>(&tabs_bells::STATE_KIND)
        .map_err(|error| format!("following tab bells: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(tabs: &[(u128, u64)]) -> tabs_bells::TabBellsSnapshot {
        tabs_bells::TabBellsSnapshot {
            tabs: tabs
                .iter()
                .map(|(tab, bells)| tabs_bells::TabBells {
                    tab_id: Uuid::from_u128(*tab),
                    bells: *bells,
                })
                .collect(),
            revision: 0,
        }
    }

    /// Each policy rings for new bells from the tabs it covers, once per bell,
    /// never for what was counted before the watch started.
    #[test]
    fn bells_ring_by_policy_and_whether_their_tab_is_shown() {
        let shown = Some(Uuid::from_u128(1));
        for (policy, from_shown, from_other) in [
            (AttachBellPolicy::None, 0, 0),
            (AttachBellPolicy::Any, 1, 2),
            (AttachBellPolicy::Current, 1, 0),
            (AttachBellPolicy::Other, 0, 2),
        ] {
            let mut watch = BellWatch::default();
            assert_eq!(
                watch.observe(&snapshot(&[(1, 5), (2, 9)]), shown, policy),
                0
            );
            assert_eq!(
                watch.observe(&snapshot(&[(1, 6), (2, 9)]), shown, policy),
                from_shown,
                "{policy:?} shown"
            );
            assert_eq!(
                watch.observe(&snapshot(&[(1, 6), (2, 11)]), shown, policy),
                from_other,
                "{policy:?} other"
            );
            // Nothing new: nothing rings.
            assert_eq!(
                watch.observe(&snapshot(&[(1, 6), (2, 11)]), shown, policy),
                0
            );
        }
    }

    #[test]
    fn restarted_counts_and_new_tabs_ring_only_for_new_bells() {
        let mut watch = BellWatch::default();
        let policy = AttachBellPolicy::Any;
        watch.observe(&snapshot(&[(1, 5)]), None, policy);
        // The program restarted: its count went down, which is no bell.
        assert_eq!(watch.observe(&snapshot(&[(1, 0)]), None, policy), 0);
        assert_eq!(watch.observe(&snapshot(&[(1, 1)]), None, policy), 1);
        // A tab that appears with a bell rang it after the watch started.
        assert_eq!(watch.observe(&snapshot(&[(1, 1), (2, 1)]), None, policy), 1);
    }
}
