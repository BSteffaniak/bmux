//! What each pane's agent is doing, from everything that says so.
//!
//! A pane runs an agent while its foreground job is one (see
//! [`crate::identify`]) or while a process in it reports being one. Its state
//! is decided, most authoritative first, by:
//!
//! 1. a screen rule showing a blocker (a permission dialog);
//! 2. a screen rule plainly showing work (a spinner with "esc to interrupt");
//! 3. the agent's own report (its hooks, or `bmux agents report`), except
//!    that a stale "working" yields to a visible idle prompt once output stops;
//! 4. any other matching screen rule;
//! 5. for an agent with no rules that never reported, its output: working
//!    while it keeps printing.
//!
//! A report binds to the job that runs when it arrives and ends with it.
//! Going idle waits briefly for the screen to settle, unless the agent said
//! so itself.

use std::collections::{BTreeMap, HashMap};

use bmux_agents_plugin_api::agent_states::{AgentSource, AgentState, PaneAgent};
use bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent;
use bmux_pane_runtime_plugin_api::pane_runtime_terminal_events::TerminalEvent;
use uuid::Uuid;

use crate::identify::Catalog;
use crate::rules::{self, RuleState};

/// Output this recent means an agent without rules is working, and keeps a
/// reported "working" alive against an idle-looking screen.
pub const ACTIVE_WINDOW_MS: u64 = 3_000;
/// How long the screen must look idle before a working agent counts as idle.
pub const IDLE_SETTLE_MS: u64 = 1_200;

/// What a pane's program signalled, kept whether or not it runs an agent.
#[derive(Debug, Clone, Default)]
struct Signals {
    title: String,
    progress: String,
    output_at_ms: Option<u64>,
    notification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Report {
    state: AgentState,
    message: Option<String>,
}

#[derive(Debug, Clone)]
struct Track {
    session_id: Uuid,
    agent: String,
    /// The foreground process group the agent runs in; `None` until the
    /// pane's jobs are next listed (a report can arrive first).
    job: Option<i32>,
    /// Identified from the job (rather than only reported).
    identified: bool,
    /// Listings of the pane's jobs a reported agent has gone unseen in.
    unseen: u8,
    report: Option<Report>,
    screen: Option<rules::Verdict>,
    /// The screen changed since rules last read it.
    screen_dirty: bool,
    state: Option<(AgentState, AgentSource, Option<String>)>,
    changed_at_ms: u64,
    finished_at_ms: Option<u64>,
    idle_since_ms: Option<u64>,
}

/// One pane's foreground job, as the pane runtime lists it.
#[derive(Debug, Clone)]
pub struct Job {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub process_group_id: i32,
    pub commands: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Tracker {
    signals: HashMap<Uuid, Signals>,
    tracks: BTreeMap<Uuid, Track>,
    published: Option<Vec<PaneAgent>>,
}

impl Tracker {
    /// Record a terminal signal.
    pub fn apply_terminal(&mut self, event: &TerminalEvent) {
        match event {
            TerminalEvent::Title { pane_id, title, .. } => {
                self.signals
                    .entry(*pane_id)
                    .or_default()
                    .title
                    .clone_from(title);
                self.mark_dirty(*pane_id);
            }
            TerminalEvent::Progress { pane_id, value, .. } => {
                self.signals
                    .entry(*pane_id)
                    .or_default()
                    .progress
                    .clone_from(value);
                self.mark_dirty(*pane_id);
            }
            TerminalEvent::Notification {
                pane_id,
                title,
                body,
                ..
            } => {
                self.signals.entry(*pane_id).or_default().notification = Some(match title {
                    Some(title) if !title.is_empty() => format!("{title}: {body}"),
                    _ => body.clone(),
                });
            }
            TerminalEvent::AgentReport {
                session_id,
                pane_id,
                state,
                agent,
                message,
            } => self.report(
                *session_id,
                *pane_id,
                state,
                agent.as_deref(),
                message.clone(),
            ),
            TerminalEvent::WorkingDirectory { .. }
            | TerminalEvent::Bell { .. }
            | TerminalEvent::PromptMark { .. } => {}
        }
    }

    /// Record a pane lifecycle or output event at `now_ms`.
    pub fn apply_pane(&mut self, event: &PaneEvent, now_ms: u64) {
        match event {
            PaneEvent::OutputAvailable { pane_id, .. } => {
                self.signals.entry(*pane_id).or_default().output_at_ms = Some(now_ms);
                self.mark_dirty(*pane_id);
            }
            PaneEvent::Exited { pane_id, .. } | PaneEvent::Restarted { pane_id, .. } => {
                self.signals.remove(pane_id);
                self.tracks.remove(pane_id);
            }
            _ => {}
        }
    }

    fn mark_dirty(&mut self, pane: Uuid) {
        if let Some(track) = self.tracks.get_mut(&pane) {
            track.screen_dirty = true;
        }
    }

    fn report(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        state: &str,
        agent: Option<&str>,
        message: Option<String>,
    ) {
        let state = match state {
            "working" => AgentState::Working,
            "blocked" => AgentState::Blocked,
            "idle" => AgentState::Idle,
            // The agent is leaving: what it reported no longer holds.
            "release" => {
                if let Some(track) = self.tracks.get_mut(&pane_id) {
                    track.report = None;
                    if !track.identified {
                        self.tracks.remove(&pane_id);
                    }
                }
                return;
            }
            _ => return,
        };
        let track = self.tracks.entry(pane_id).or_insert_with(|| Track {
            session_id,
            agent: agent.unwrap_or("agent").to_string(),
            job: None,
            identified: false,
            unseen: 0,
            report: None,
            screen: None,
            screen_dirty: true,
            state: None,
            changed_at_ms: 0,
            finished_at_ms: None,
            idle_since_ms: None,
        });
        if let Some(agent) = agent
            && !track.identified
        {
            track.agent = agent.to_string();
        }
        track.report = Some(Report { state, message });
    }

    /// Follow the panes' foreground jobs: identify agents, and end tracks
    /// whose job ended or changed.
    pub fn set_jobs(&mut self, jobs: &[Job], catalog: &Catalog) {
        let jobs = jobs
            .iter()
            .map(|job| (job.pane_id, job))
            .collect::<HashMap<_, _>>();
        self.tracks
            .retain(|pane, track| match (jobs.get(pane), track.job) {
                // A report that arrived before its job was seen binds to it.
                (Some(job), None) => {
                    track.job = Some(job.process_group_id);
                    true
                }
                (Some(job), Some(bound)) => job.process_group_id == bound,
                // A report from no job (the shell at its prompt) describes
                // nothing running; it may race the listing once.
                (None, None) => {
                    track.unseen += 1;
                    !track.identified && track.unseen < 2
                }
                (None, Some(_)) => false,
            });
        for (pane, job) in jobs {
            let Some(agent) = catalog.identify(&job.commands) else {
                continue;
            };
            let track = self.tracks.entry(pane).or_insert_with(|| Track {
                session_id: job.session_id,
                agent: agent.clone(),
                job: Some(job.process_group_id),
                identified: true,
                unseen: 0,
                report: None,
                screen: None,
                screen_dirty: true,
                state: None,
                changed_at_ms: 0,
                finished_at_ms: None,
                idle_since_ms: None,
            });
            track.identified = true;
            track.agent = agent;
        }
    }

    /// Panes whose agent has screen rules and whose screen changed since
    /// they were last read.
    pub fn screens_to_read(&self, has_rules: impl Fn(&str) -> bool) -> Vec<(Uuid, Uuid)> {
        self.tracks
            .iter()
            .filter(|(_, track)| track.screen_dirty && has_rules(&track.agent))
            .map(|(pane, track)| (track.session_id, *pane))
            .collect()
    }

    /// The agent running in `pane`, if any.
    #[must_use]
    pub fn agent(&self, pane: Uuid) -> Option<&str> {
        self.tracks.get(&pane).map(|track| track.agent.as_str())
    }

    /// Read `pane`'s screen with its agent's rules.
    pub fn apply_screen(&mut self, pane: Uuid, screen: &str, rules: &rules::Agent) {
        let signals = self.signals.get(&pane).cloned().unwrap_or_default();
        let Some(track) = self.tracks.get_mut(&pane) else {
            return;
        };
        track.screen_dirty = false;
        let verdict = rules.evaluate(&rules::Input {
            screen,
            title: &signals.title,
            progress: &signals.progress,
        });
        match verdict {
            // A viewer or menu says nothing: keep what the screen last said.
            Some(verdict) if verdict.skip => {}
            verdict => track.screen = verdict,
        }
    }

    /// Decide every agent's state at `now_ms`.
    pub fn settle(&mut self, now_ms: u64, has_rules: impl Fn(&str) -> bool) {
        for (pane, track) in &mut self.tracks {
            let signals = self.signals.get(pane);
            let active = signals
                .and_then(|signals| signals.output_at_ms)
                .is_some_and(|at| now_ms.saturating_sub(at) < ACTIVE_WINDOW_MS);
            let screen = track.screen.as_ref();
            let screen_says = |state| screen.filter(|verdict| verdict.state == state);
            let report = track.report.as_ref().map(|report| report.state);
            let (state, source, rule) = if let Some(verdict) = screen_says(RuleState::Blocked) {
                (
                    AgentState::Blocked,
                    AgentSource::Screen,
                    Some(verdict.rule.clone()),
                )
            } else if let Some(verdict) =
                screen_says(RuleState::Working).filter(|verdict| verdict.visible)
            {
                (
                    AgentState::Working,
                    AgentSource::Screen,
                    Some(verdict.rule.clone()),
                )
            } else if report == Some(AgentState::Working)
                && screen_says(RuleState::Idle).is_some_and(|verdict| verdict.visible)
                && !active
            {
                let verdict = screen_says(RuleState::Idle).map(|verdict| verdict.rule.clone());
                (AgentState::Idle, AgentSource::Screen, verdict)
            } else if let Some(report) = report {
                (report, AgentSource::Report, None)
            } else if let Some(verdict) = screen {
                let state = match verdict.state {
                    RuleState::Working => AgentState::Working,
                    RuleState::Blocked => AgentState::Blocked,
                    RuleState::Idle | RuleState::Unknown => AgentState::Idle,
                };
                (state, AgentSource::Screen, Some(verdict.rule.clone()))
            } else if has_rules(&track.agent) {
                // A known agent no rule matches is waiting (as herdr has it).
                (AgentState::Idle, AgentSource::Screen, None)
            } else if active {
                (AgentState::Working, AgentSource::Activity, None)
            } else {
                (AgentState::Idle, AgentSource::Activity, None)
            };
            let previous = track.state.as_ref().map(|(state, ..)| *state);
            // Settle before leaving work, unless the agent said it is done.
            if previous == Some(AgentState::Working)
                && state == AgentState::Idle
                && source != AgentSource::Report
            {
                let since = *track.idle_since_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) < IDLE_SETTLE_MS {
                    continue;
                }
            }
            track.idle_since_ms = None;
            if previous != Some(state) {
                track.changed_at_ms = now_ms;
                if previous == Some(AgentState::Working) && state == AgentState::Idle {
                    track.finished_at_ms = Some(now_ms);
                }
            }
            track.state = Some((state, source, rule));
        }
    }

    /// Every pane's agent, by pane.
    #[must_use]
    pub fn snapshot(&self) -> Vec<PaneAgent> {
        self.tracks
            .iter()
            .filter_map(|(pane, track)| {
                let (state, source, rule) = track.state.clone()?;
                let message = track
                    .report
                    .as_ref()
                    .and_then(|report| report.message.clone())
                    .or_else(|| {
                        (state == AgentState::Blocked)
                            .then(|| self.signals.get(pane)?.notification.clone())
                            .flatten()
                    });
                Some(PaneAgent {
                    session_id: track.session_id,
                    pane_id: *pane,
                    agent: track.agent.clone(),
                    state,
                    source,
                    rule,
                    message,
                    changed_at_ms: track.changed_at_ms,
                    finished_at_ms: track.finished_at_ms,
                })
            })
            .collect()
    }

    /// Whether `panes` differs from what was last published; records it when so.
    pub fn changed(&mut self, panes: &[PaneAgent]) -> bool {
        if self.published.as_deref() == Some(panes) {
            return false;
        }
        self.published = Some(panes.to_vec());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANE: Uuid = Uuid::from_u128(1);
    const SESSION: Uuid = Uuid::from_u128(9);

    fn claude() -> rules::Agent {
        rules::Agent::parse(
            rules::BUNDLED
                .iter()
                .find(|(name, _)| *name == "claude")
                .unwrap()
                .1,
        )
        .unwrap()
    }

    fn catalog() -> Catalog {
        let claude = claude();
        Catalog::new(
            [(claude.id.as_str(), claude.aliases.as_slice())],
            &["aider".to_string()],
        )
    }

    fn job(group: i32, command: &str) -> Job {
        Job {
            session_id: SESSION,
            pane_id: PANE,
            process_group_id: group,
            commands: vec![command.to_string()],
        }
    }

    fn report(tracker: &mut Tracker, state: &str, message: Option<&str>) {
        tracker.apply_terminal(&TerminalEvent::AgentReport {
            session_id: SESSION,
            pane_id: PANE,
            state: state.into(),
            agent: Some("claude".into()),
            message: message.map(str::to_string),
        });
    }

    fn output(tracker: &mut Tracker, now: u64) {
        tracker.apply_pane(
            &PaneEvent::OutputAvailable {
                session_id: SESSION,
                pane_id: PANE,
            },
            now,
        );
    }

    fn state(tracker: &Tracker) -> Option<(AgentState, AgentSource)> {
        tracker
            .snapshot()
            .first()
            .map(|pane| (pane.state, pane.source))
    }

    const WORKING: &str = "✻ Pondering… (3s · esc to interrupt)\n─────\n❯ \n─────";
    const BLOCKED: &str = " Bash command\n rm -rf build\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n Esc to cancel · Tab to amend";
    const IDLE: &str = "● Done.\n─────\n❯ \n─────\n  ? for shortcuts";

    #[test]
    fn screens_decide_a_claude_panes_state_and_idle_settles() {
        let rules = claude();
        let has_rules = |agent: &str| agent == "claude";
        let mut tracker = Tracker::default();
        tracker.set_jobs(
            &[job(40, "node /x/@anthropic-ai/claude-code/cli.js")],
            &catalog(),
        );
        assert_eq!(tracker.screens_to_read(has_rules), [(SESSION, PANE)]);
        tracker.apply_screen(PANE, WORKING, &rules);
        assert_eq!(tracker.screens_to_read(has_rules).len(), 0);
        tracker.settle(1_000, has_rules);
        assert_eq!(
            state(&tracker),
            Some((AgentState::Working, AgentSource::Screen))
        );

        output(&mut tracker, 2_000);
        tracker.apply_screen(PANE, BLOCKED, &rules);
        tracker.settle(2_000, has_rules);
        assert_eq!(
            state(&tracker),
            Some((AgentState::Blocked, AgentSource::Screen))
        );

        tracker.apply_screen(PANE, WORKING, &rules);
        tracker.settle(3_000, has_rules);
        tracker.apply_screen(PANE, IDLE, &rules);
        // Idle must hold before it counts.
        tracker.settle(3_100, has_rules);
        assert_eq!(
            state(&tracker).map(|(state, _)| state),
            Some(AgentState::Working)
        );
        tracker.settle(3_100 + IDLE_SETTLE_MS, has_rules);
        let pane = tracker.snapshot().remove(0);
        assert_eq!(pane.state, AgentState::Idle);
        assert_eq!(pane.finished_at_ms, Some(3_100 + IDLE_SETTLE_MS));
        assert_eq!(pane.rule.as_deref(), Some("live_prompt_box"));

        // The job ending ends the agent.
        tracker.set_jobs(&[], &catalog());
        assert_eq!(tracker.snapshot().len(), 0);
    }

    #[test]
    fn reports_bind_to_their_job_and_yield_to_what_the_screen_shows() {
        let rules = claude();
        let has_rules = |agent: &str| agent == "claude";
        let mut tracker = Tracker::default();
        // A report before the job was listed binds to it when it is.
        report(&mut tracker, "working", None);
        tracker.settle(0, has_rules);
        assert_eq!(
            state(&tracker),
            Some((AgentState::Working, AgentSource::Report))
        );
        tracker.set_jobs(&[job(40, "claude")], &catalog());

        // A report outranks an ambiguous screen; a visible blocker outranks it.
        report(&mut tracker, "blocked", Some("approve edit?"));
        tracker.settle(100, has_rules);
        let pane = tracker.snapshot().remove(0);
        assert_eq!(
            (pane.state, pane.source),
            (AgentState::Blocked, AgentSource::Report)
        );
        assert_eq!(pane.message.as_deref(), Some("approve edit?"));
        report(&mut tracker, "working", None);
        tracker.apply_screen(PANE, BLOCKED, &rules);
        tracker.settle(200, has_rules);
        assert_eq!(
            state(&tracker),
            Some((AgentState::Blocked, AgentSource::Screen))
        );

        // A stale "working" yields to an idle prompt once output stops.
        tracker.apply_screen(PANE, IDLE, &rules);
        output(&mut tracker, 300);
        tracker.settle(300, has_rules);
        assert_eq!(
            state(&tracker),
            Some((AgentState::Working, AgentSource::Report))
        );
        tracker.settle(300 + ACTIVE_WINDOW_MS, has_rules);
        tracker.settle(300 + ACTIVE_WINDOW_MS + IDLE_SETTLE_MS, has_rules);
        assert_eq!(
            state(&tracker),
            Some((AgentState::Idle, AgentSource::Screen))
        );

        // Saying it is done needs no settling.
        report(&mut tracker, "working", None);
        tracker.apply_screen(PANE, "", &rules);
        tracker.settle(10_000, has_rules);
        report(&mut tracker, "idle", None);
        tracker.settle(10_001, has_rules);
        assert_eq!(
            state(&tracker),
            Some((AgentState::Idle, AgentSource::Report))
        );

        // Another job in the pane ends the report with the agent.
        tracker.set_jobs(&[job(41, "vim")], &catalog());
        assert_eq!(tracker.snapshot().len(), 0);
    }

    #[test]
    fn agents_without_rules_work_while_they_print() {
        let has_rules = |_: &str| false;
        let mut tracker = Tracker::default();
        tracker.set_jobs(&[job(50, "aider")], &catalog());
        assert_eq!(tracker.screens_to_read(has_rules).len(), 0);
        output(&mut tracker, 1_000);
        tracker.settle(1_000, has_rules);
        assert_eq!(
            state(&tracker),
            Some((AgentState::Working, AgentSource::Activity))
        );
        tracker.settle(1_000 + ACTIVE_WINDOW_MS + IDLE_SETTLE_MS * 2, has_rules);
        tracker.settle(1_000 + ACTIVE_WINDOW_MS + IDLE_SETTLE_MS * 3, has_rules);
        assert_eq!(
            state(&tracker),
            Some((AgentState::Idle, AgentSource::Activity))
        );
        // Unknown programs are not agents.
        tracker.set_jobs(&[job(51, "make test")], &catalog());
        assert_eq!(tracker.snapshot().len(), 0);
    }

    #[test]
    fn a_released_report_leaves_with_its_agent() {
        let mut tracker = Tracker::default();
        report(&mut tracker, "working", None);
        report(&mut tracker, "release", None);
        tracker.settle(0, |_| false);
        assert_eq!(tracker.snapshot().len(), 0);
        // Unknown states are ignored.
        report(&mut tracker, "dancing", None);
        assert_eq!(tracker.snapshot().len(), 0);
        // A report from no job lasts one listing of the pane's jobs.
        report(&mut tracker, "working", None);
        tracker.set_jobs(&[], &catalog());
        tracker.settle(0, |_| false);
        assert_eq!(tracker.snapshot().len(), 1);
        tracker.set_jobs(&[], &catalog());
        assert_eq!(tracker.snapshot().len(), 0);
        // Unchanged snapshots are published once.
        assert!(tracker.changed(&[]));
        assert!(!tracker.changed(&[]));
    }
}
