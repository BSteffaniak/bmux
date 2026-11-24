//! Alerts on this client's terminal when an agent needs an answer or
//! finishes its work.
//!
//! Runs inside each attach client. It follows every tab's agent
//! (`tabs-status`), the tabs' names (`tabs-list`) and which tab this client
//! shows (`tabs-local-view`), and posts an alert, once, when a tab's agent
//! starts waiting for an answer or finishes work: a bell, a desktop
//! notification, or both, by `[plugins.settings."bmux.agents".notify]`. By
//! default the tab this client shows does not alert: its screen already shows
//! what happened. States an agent was already in when the client attached
//! are history.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bmux_plugin::{AttachAlert, AttachPresentationResources};
use bmux_tabs_plugin_api::tabs_status::{AgentActivity, TabStatusSnapshot};
use bmux_tabs_plugin_api::{tabs_list, tabs_local_view, tabs_status};
use uuid::Uuid;

/// How an alert reaches the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Via {
    pub bell: bool,
    pub notification: bool,
}

impl Via {
    const NONE: Self = Self {
        bell: false,
        notification: false,
    };
}

/// When the tab this client shows alerts too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// Only tabs this client is not showing.
    NotViewing,
    Always,
}

/// `[plugins.settings."bmux.agents".notify]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifySettings {
    pub needs_input: Via,
    pub finished: Via,
    pub when: When,
}

impl Default for NotifySettings {
    fn default() -> Self {
        Self {
            needs_input: Via {
                bell: true,
                notification: true,
            },
            finished: Via {
                bell: false,
                notification: true,
            },
            when: When::NotViewing,
        }
    }
}

impl NotifySettings {
    /// Parse the `notify` table: `on` (which changes alert: `needs_input`,
    /// `finished`), `via` (how, for all of them: `bell`, `notification`;
    /// without it, a bell and a notification for `needs_input` and a
    /// notification for `finished`), and `when` (`not_viewing` or `always`).
    ///
    /// # Errors
    /// Returns what is wrong with the table.
    pub fn parse(value: Option<&toml::Value>) -> Result<Self, String> {
        let mut settings = Self::default();
        let Some(value) = value else {
            return Ok(settings);
        };
        let table = value
            .as_table()
            .ok_or("bmux.agents notify must be a table")?;
        let names = |key: &str, allowed: &[&str]| -> Result<Option<Vec<String>>, String> {
            let Some(value) = table.get(key) else {
                return Ok(None);
            };
            let shape = format!(
                "bmux.agents notify.{key} must be a list of {}",
                allowed.join(", ")
            );
            value
                .as_array()
                .ok_or_else(|| shape.clone())?
                .iter()
                .map(|name| {
                    name.as_str()
                        .filter(|name| allowed.contains(name))
                        .map(str::to_string)
                        .ok_or_else(|| shape.clone())
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
        };
        for key in table.keys() {
            if !matches!(key.as_str(), "on" | "via" | "when") {
                return Err(format!("unknown bmux.agents notify setting `{key}`"));
            }
        }
        if let Some(via) = names("via", &["bell", "notification"])? {
            let via = Via {
                bell: via.iter().any(|name| name == "bell"),
                notification: via.iter().any(|name| name == "notification"),
            };
            settings.needs_input = via;
            settings.finished = via;
        }
        if let Some(on) = names("on", &["needs_input", "finished"])? {
            if !on.iter().any(|name| name == "needs_input") {
                settings.needs_input = Via::NONE;
            }
            if !on.iter().any(|name| name == "finished") {
                settings.finished = Via::NONE;
            }
        }
        if let Some(when) = table.get("when") {
            settings.when = match when.as_str() {
                Some("not_viewing") => When::NotViewing,
                Some("always") => When::Always,
                _ => {
                    return Err(
                        "bmux.agents notify.when must be \"not_viewing\" or \"always\"".into(),
                    );
                }
            };
        }
        Ok(settings)
    }
}

/// A change in a tab's agent worth an alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentChange {
    NeedsInput {
        tab: Uuid,
        agent: String,
        message: Option<String>,
    },
    Finished {
        tab: Uuid,
        agent: String,
    },
}

#[derive(Debug, Clone, Copy)]
struct Seen {
    activity: AgentActivity,
    finished_at_ms: Option<u64>,
}

/// How long an agent must stay idle for its finishing to alert. An agent
/// that asks for permission stops working a moment before its dialog shows,
/// which is not finishing.
pub const FINISHED_GRACE: Duration = Duration::from_millis(1_500);

/// What each tab's agent was doing, and what changed.
#[derive(Debug, Default)]
pub struct AgentWatch {
    /// `None` until the first snapshot: what agents were doing before then
    /// is history.
    seen: Option<HashMap<Uuid, Seen>>,
    /// Finishing that alerts once its agent has stayed idle until then.
    finishing: HashMap<Uuid, (Instant, String)>,
}

impl AgentWatch {
    /// The changes `snapshot`, taken at `now`, shows since the last one, and
    /// any finishing now due (see [`FINISHED_GRACE`]).
    pub fn observe(&mut self, snapshot: &TabStatusSnapshot, now_at: Instant) -> Vec<AgentChange> {
        let current = snapshot
            .tabs
            .iter()
            .filter_map(|tab| {
                let agent = tab.agent.as_ref()?;
                Some((
                    tab.tab_id,
                    agent,
                    Seen {
                        activity: agent.activity,
                        finished_at_ms: agent.finished_at_ms,
                    },
                ))
            })
            .collect::<Vec<_>>();
        let previous = self
            .seen
            .replace(current.iter().map(|(tab, _, seen)| (*tab, *seen)).collect());
        let Some(previous) = previous else {
            return Vec::new();
        };
        // An agent that is busy again, asking or gone did not finish.
        self.finishing.retain(|tab, _| {
            current
                .iter()
                .any(|(id, _, seen)| id == tab && seen.activity == AgentActivity::Idle)
        });
        let mut changes = Vec::new();
        for (tab, agent, now) in current {
            let before = previous.get(&tab);
            if now.activity == AgentActivity::Blocked
                && before.is_none_or(|before| before.activity != AgentActivity::Blocked)
            {
                changes.push(AgentChange::NeedsInput {
                    tab,
                    agent: agent.name.clone(),
                    message: agent.message.clone(),
                });
            }
            // Finished: idle now, after work that ended since we last looked.
            if now.activity == AgentActivity::Idle
                && now.finished_at_ms.is_some()
                && before.is_some_and(|before| before.finished_at_ms != now.finished_at_ms)
            {
                self.finishing
                    .insert(tab, (now_at + FINISHED_GRACE, agent.name.clone()));
            }
        }
        changes.extend(self.due(now_at));
        changes
    }

    /// Finishing whose grace ended by `now`.
    pub fn due(&mut self, now: Instant) -> Vec<AgentChange> {
        self.finishing
            .extract_if(|_, (at, _)| *at <= now)
            .map(|(tab, (_, agent))| AgentChange::Finished { tab, agent })
            .collect()
    }

    /// When the next finishing is due.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.finishing.values().map(|(at, _)| *at).min()
    }
}

/// The alert `change` makes under `settings`, for a client showing
/// `viewing`; `names` gives each tab's workspace and name.
#[must_use]
pub fn alert_for(
    change: &AgentChange,
    settings: NotifySettings,
    viewing: Option<Uuid>,
    names: &HashMap<Uuid, (String, String)>,
) -> Option<AttachAlert> {
    let (tab, via) = match change {
        AgentChange::NeedsInput { tab, .. } => (*tab, settings.needs_input),
        AgentChange::Finished { tab, .. } => (*tab, settings.finished),
    };
    if settings.when == When::NotViewing && viewing == Some(tab) {
        return None;
    }
    let place = names.get(&tab).map_or_else(
        || tab.to_string(),
        |(workspace, name)| format!("{workspace}/{name}"),
    );
    let body = match change {
        AgentChange::NeedsInput {
            agent,
            message: Some(message),
            ..
        } => format!("{agent} in {place} needs input: {message}"),
        AgentChange::NeedsInput { agent, .. } => format!("{agent} in {place} needs input"),
        AgentChange::Finished { agent, .. } => format!("{agent} in {place} finished"),
    };
    let alert = AttachAlert {
        bell: via.bell,
        notification: via.notification.then(|| bmux_plugin::AttachNotification {
            title: "bmux".to_string(),
            body,
        }),
    };
    (alert.bell || alert.notification.is_some()).then_some(alert)
}

/// The running agent alerts of one attach runtime; dropping it stops them.
pub struct AgentAlerts {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AgentAlerts {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AgentAlerts {
    /// Start alerting for the runtime `resources` belong to, by `settings`
    /// (the `bmux.agents` plugin settings).
    ///
    /// # Errors
    /// Returns invalid settings, or when there is no async runtime to run in
    /// or a channel cannot be followed.
    pub fn install(
        settings: Option<&toml::Value>,
        resources: &AttachPresentationResources,
    ) -> Result<Self, String> {
        let settings = NotifySettings::parse(settings.and_then(|settings| settings.get("notify")))?;
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|error| format!("agent alerts need an async runtime: {error}"))?;
        let events = Arc::clone(&resources.events);
        let mut status = subscribe::<TabStatusSnapshot>(
            &events,
            &tabs_status::STATE_KIND,
            TabStatusSnapshot {
                tabs: Vec::new(),
                now_ms: 0,
                revision: 0,
            },
        )?;
        let mut list = subscribe::<tabs_list::TabListSnapshot>(
            &events,
            &tabs_list::STATE_KIND,
            tabs_list::TabListSnapshot {
                tabs: Vec::new(),
                revision: 0,
            },
        )?;
        let mut selection = events
            .subscribe_state::<tabs_local_view::TabSelection>(&tabs_local_view::STATE_KIND)
            .ok()
            .map(|(_, receiver)| receiver);
        let alerts = bmux_plugin::attach_alerts(&resources.states);
        let task = handle.spawn(async move {
            let mut watch = AgentWatch::default();
            watch.observe(&status.borrow_and_update(), Instant::now());
            loop {
                let next_due = watch.next_due();
                let found = tokio::select! {
                    changed = status.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let snapshot = status.borrow_and_update().clone();
                        watch.observe(&snapshot, Instant::now())
                    }
                    () = async {
                        match next_due {
                            Some(at) => tokio::time::sleep_until(at.into()).await,
                            None => std::future::pending().await,
                        }
                    } => watch.due(Instant::now()),
                    changed = list.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        Vec::new()
                    }
                    changed = async {
                        match selection.as_mut() {
                            Some(receiver) => receiver.changed().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if changed.is_err() {
                            selection = None;
                        }
                        Vec::new()
                    }
                };
                if found.is_empty() {
                    continue;
                }
                let names = tab_names(&list.borrow_and_update());
                let selected = selection
                    .as_ref()
                    .and_then(|receiver| receiver.borrow().context_id);
                let Ok(mut alerts) = alerts.write() else {
                    return;
                };
                let viewing = viewed_tab(selected, alerts.terminal_focused());
                for change in &found {
                    if let Some(alert) = alert_for(change, settings, viewing, &names) {
                        alerts.post(alert);
                    }
                }
            }
        });
        Ok(Self { task })
    }
}

/// The tab the user is looking at: the one this client shows, while its
/// terminal has focus. With another window or app in front, none is.
#[must_use]
pub const fn viewed_tab(selected: Option<Uuid>, terminal_focused: bool) -> Option<Uuid> {
    if terminal_focused { selected } else { None }
}

/// Each tab's workspace and name.
fn tab_names(list: &tabs_list::TabListSnapshot) -> HashMap<Uuid, (String, String)> {
    list.tabs
        .iter()
        .map(|tab| (tab.id, (tab.workspace.clone(), tab.name.clone())))
        .collect()
}

/// Follow a forwarded state channel, registering its decoder (starting from
/// `initial`) when nothing has yet.
fn subscribe<T>(
    events: &Arc<bmux_plugin::EventBus>,
    kind: &bmux_plugin::PluginEventKind,
    initial: T,
) -> Result<tokio::sync::watch::Receiver<Arc<T>>, String>
where
    T: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    if let Ok((_, receiver)) = events.subscribe_state::<T>(kind) {
        return Ok(receiver);
    }
    events.register_state_channel_with_decoder(kind.clone(), initial);
    events
        .subscribe_state::<T>(kind)
        .map(|(_, receiver)| receiver)
        .map_err(|error| format!("following {kind:?}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tabs_plugin_api::tabs_status::{TabAgent, TabStatus};

    fn snapshot(tabs: &[(u128, AgentActivity, Option<u64>)]) -> TabStatusSnapshot {
        TabStatusSnapshot {
            tabs: tabs
                .iter()
                .map(|(tab, activity, finished_at_ms)| TabStatus {
                    tab_id: Uuid::from_u128(*tab),
                    panes: 1,
                    running_command: None,
                    cwd: None,
                    title: None,
                    git: None,
                    last_exit: None,
                    output_at_ms: None,
                    attention_at_ms: None,
                    attention: None,
                    agent: Some(TabAgent {
                        name: "claude".into(),
                        activity: *activity,
                        message: (*activity == AgentActivity::Blocked)
                            .then(|| "Do you want to proceed?".into()),
                        changed_at_ms: 0,
                        finished_at_ms: *finished_at_ms,
                    }),
                })
                .collect(),
            now_ms: 0,
            revision: 0,
        }
    }

    use AgentActivity::{Blocked, Idle, Working};

    /// An agent that works, asks, works again and finishes alerts once for
    /// asking and once for finishing (when it stays idle); what it was doing
    /// when the client attached does not alert.
    #[test]
    fn each_real_change_alerts_once_and_attaching_alerts_nothing() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut watch = AgentWatch::default();
        assert_eq!(
            watch
                .observe(
                    &snapshot(&[(1, Blocked, Some(5)), (2, Idle, Some(9))]),
                    at(0)
                )
                .len(),
            0
        );
        let tab = Uuid::from_u128(1);
        // Answered and working, then asking again.
        assert_eq!(
            watch
                .observe(&snapshot(&[(1, Working, Some(5))]), at(10))
                .len(),
            0
        );
        assert_eq!(
            watch.observe(&snapshot(&[(1, Blocked, Some(20))]), at(20)),
            [AgentChange::NeedsInput {
                tab,
                agent: "claude".into(),
                message: Some("Do you want to proceed?".into()),
            }]
        );
        // Still asking: nothing new.
        assert_eq!(
            watch
                .observe(&snapshot(&[(1, Blocked, Some(20))]), at(30))
                .len(),
            0
        );
        assert_eq!(
            watch
                .observe(&snapshot(&[(1, Working, Some(20))]), at(40))
                .len(),
            0
        );
        // Finished: once it has stayed idle through the grace.
        assert_eq!(
            watch
                .observe(&snapshot(&[(1, Idle, Some(30))]), at(50))
                .len(),
            0
        );
        assert_eq!(watch.next_due(), Some(at(50) + FINISHED_GRACE));
        assert_eq!(
            watch.due(at(50) + FINISHED_GRACE),
            [AgentChange::Finished {
                tab,
                agent: "claude".into(),
            }]
        );
        assert_eq!(
            watch
                .observe(&snapshot(&[(1, Idle, Some(30))]), at(5_000))
                .len(),
            0
        );
        assert_eq!(watch.next_due(), None);
        // Declining a request is not finishing work.
        watch.observe(&snapshot(&[(1, Working, Some(30))]), at(6_000));
        watch.observe(&snapshot(&[(1, Blocked, Some(40))]), at(6_100));
        assert_eq!(
            watch
                .observe(&snapshot(&[(1, Idle, Some(40))]), at(6_200))
                .len(),
            0
        );
        assert_eq!(watch.due(at(60_000)).len(), 0);
    }

    /// An agent that stops working a moment before its permission dialog
    /// shows was asking, not finishing: only the asking alerts.
    #[test]
    fn stopping_to_ask_does_not_alert_as_finishing() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut watch = AgentWatch::default();
        watch.observe(&snapshot(&[(1, Working, None)]), at(0));
        // The title stopped spinning; the dialog is not drawn yet.
        assert_eq!(
            watch
                .observe(&snapshot(&[(1, Idle, Some(100))]), at(100))
                .len(),
            0
        );
        let changes = watch.observe(&snapshot(&[(1, Blocked, Some(100))]), at(300));
        assert!(matches!(
            changes.as_slice(),
            [AgentChange::NeedsInput { .. }]
        ));
        assert_eq!(watch.due(at(10_000)).len(), 0);
    }

    /// Settings choose which changes alert and how, and by default the tab
    /// this client shows stays quiet.
    #[test]
    fn settings_choose_what_alerts_how_and_where() {
        let tab = Uuid::from_u128(1);
        let names = HashMap::from([(tab, ("bcode".to_string(), "master".to_string()))]);
        let asking = AgentChange::NeedsInput {
            tab,
            agent: "claude".into(),
            message: Some("Do you want to proceed?".into()),
        };
        let finished = AgentChange::Finished {
            tab,
            agent: "claude".into(),
        };
        let defaults = NotifySettings::default();
        let alert = alert_for(&asking, defaults, None, &names).unwrap();
        assert!(alert.bell);
        assert_eq!(
            alert.notification.unwrap().body,
            "claude in bcode/master needs input: Do you want to proceed?"
        );
        let alert = alert_for(&finished, defaults, None, &names).unwrap();
        assert!(!alert.bell);
        assert_eq!(
            alert.notification.unwrap().body,
            "claude in bcode/master finished"
        );
        // The tab shown here stays quiet unless `when = "always"`.
        assert_eq!(alert_for(&asking, defaults, Some(tab), &names), None);
        // With another window in front, the tab shown here is not being
        // looked at, so it alerts.
        let viewing = viewed_tab(Some(tab), false);
        assert!(alert_for(&asking, defaults, viewing, &names).is_some());
        assert_eq!(viewed_tab(Some(tab), true), Some(tab));
        let parse = |source: &str| {
            NotifySettings::parse(Some(&toml::from_str::<toml::Value>(source).unwrap()))
        };
        let always = parse("when = \"always\"").unwrap();
        assert!(alert_for(&asking, always, Some(tab), &names).is_some());
        let bells_for_finishing = parse("on = [\"finished\"]\nvia = [\"bell\"]").unwrap();
        assert_eq!(alert_for(&asking, bells_for_finishing, None, &names), None);
        assert_eq!(
            alert_for(&finished, bells_for_finishing, None, &names),
            Some(AttachAlert::bell())
        );
        for (source, error) in [
            ("on = [\"done\"]", "notify.on"),
            ("via = \"bell\"", "notify.via"),
            ("when = \"never\"", "notify.when"),
            ("sound = true", "unknown"),
        ] {
            let message = parse(source).unwrap_err();
            assert!(message.contains(error), "{source}: {message}");
        }
    }
}
