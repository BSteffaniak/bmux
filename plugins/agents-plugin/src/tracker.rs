//! What each pane's agent is doing, decided from what its pane shows now.
//!
//! A pane runs an agent while its foreground job is one (see
//! [`crate::identify`]) or while a program in it reports being one. Its state
//! answers two questions, each from the pane's current state (its retained
//! signals and its screen), never from which events happened to arrive:
//!
//! 1. Is it working? The first channel that says either way decides: the
//!    agent's title (OSC 0/2) or progress report (OSC 9;4) by its rules, its
//!    own report (its hooks, or `bmux agents report`), then its screen by its
//!    rules. A "working" report that arrived after the title or progress
//!    last changed outranks their idle or asking verdict, which is stale.
//!    An agent with no rules is working while its visible screen keeps
//!    changing.
//! 2. When it is not working, is it waiting for an answer? Its title or
//!    progress, a blocker on its screen (a permission or question dialog) or
//!    its own report says so. Otherwise, background work its screen shows
//!    (by a rule marked `background_working`) keeps it working.
//!
//! Asking only when not working keeps text that merely looks like a dialog
//! (in an agent's transcript) from turning a working agent into a waiting
//! one. A title or progress the pane had before the agent started is not
//! the agent's, so it counts only once it changes.

use std::collections::{BTreeMap, HashMap};

use bmux_agents_plugin_api::agent_states::{AgentSource, AgentState, PaneAgent};
use uuid::Uuid;

use crate::identify::Catalog;
use crate::rules::{self, Channel, RuleState};

/// An agent with no rules is working while its screen changed this recently.
pub const ACTIVE_WINDOW_MS: u64 = 3_000;

/// A program's report of its own state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub state: String,
    pub agent: Option<String>,
    pub message: Option<String>,
}

/// What a pane signals beyond its screen, as the pane runtime retains it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneView {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub title: Option<String>,
    pub progress: Option<String>,
    pub last_notification: Option<String>,
    pub report: Option<Report>,
    /// How many reports the pane has had: a new one is a report to apply.
    pub reports: u64,
    /// Changes whenever the screen may have.
    pub screen_revision: u64,
}

/// One pane's foreground job, as the pane runtime lists it.
#[derive(Debug, Clone)]
pub struct Job {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub process_group_id: i32,
    pub commands: Vec<String>,
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
    report: Option<(AgentState, Option<String>)>,
    /// The [`Tracker::epoch`] its report last arrived in, and that its title
    /// or progress last changed in: the newer of the two is the current one.
    report_epoch: u64,
    signal_epoch: u64,
    /// The title and progress the pane had when the agent appeared: not the
    /// agent's until they change.
    inherited: Option<(Option<String>, Option<String>)>,
    title_live: bool,
    progress_live: bool,
    /// The screen revision last read, the text then, and when it last
    /// visibly changed.
    screen_revision: Option<u64>,
    screen: Option<String>,
    screen_changed_at_ms: Option<u64>,
    screen_verdict: Option<rules::Verdict>,
    state: Option<(AgentState, AgentSource, Option<String>)>,
    changed_at_ms: u64,
    finished_at_ms: Option<u64>,
}

impl Track {
    const fn new(session_id: Uuid, agent: String, job: Option<i32>, identified: bool) -> Self {
        Self {
            session_id,
            agent,
            job,
            identified,
            unseen: 0,
            report: None,
            report_epoch: 0,
            signal_epoch: 0,
            inherited: None,
            title_live: false,
            progress_live: false,
            screen_revision: None,
            screen: None,
            screen_changed_at_ms: None,
            screen_verdict: None,
            state: None,
            changed_at_ms: 0,
            finished_at_ms: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Tracker {
    /// Every pane's retained signals, as last read.
    views: HashMap<Uuid, PaneView>,
    /// Reports already applied, by pane.
    reports_seen: HashMap<Uuid, u64>,
    tracks: BTreeMap<Uuid, Track>,
    published: Option<Vec<PaneAgent>>,
    /// How many times views were taken: orders reports against title and
    /// progress changes.
    epoch: u64,
}

impl Tracker {
    /// Take every pane's current signals. Panes absent from `views` are gone,
    /// and so are their agents. A report the pane runtime has not shown
    /// before is applied; the reports a pane had when first seen are history.
    pub fn set_views(&mut self, views: Vec<PaneView>) {
        let live = views
            .iter()
            .map(|view| view.pane_id)
            .collect::<std::collections::HashSet<_>>();
        self.tracks.retain(|pane, _| live.contains(pane));
        self.reports_seen.retain(|pane, _| live.contains(pane));
        self.epoch += 1;
        for view in views {
            let seen = self.reports_seen.insert(view.pane_id, view.reports);
            if let Some(report) = view.report.clone()
                && seen.is_some_and(|seen| view.reports > seen)
            {
                self.report(&view, &report);
            }
            if let Some(track) = self.tracks.get_mut(&view.pane_id) {
                let inherited = track
                    .inherited
                    .get_or_insert_with(|| (view.title.clone(), view.progress.clone()));
                track.title_live |= view.title != inherited.0;
                track.progress_live |= view.progress != inherited.1;
                let previous = self.views.get(&view.pane_id);
                if previous.is_none_or(|previous| {
                    previous.title != view.title || previous.progress != view.progress
                }) {
                    track.signal_epoch = self.epoch;
                }
            }
            self.views.insert(view.pane_id, view);
        }
        self.views.retain(|pane, _| live.contains(pane));
    }

    fn report(&mut self, view: &PaneView, report: &Report) {
        let state = match report.state.as_str() {
            "working" => AgentState::Working,
            "blocked" => AgentState::Blocked,
            "idle" => AgentState::Idle,
            // The agent is leaving: what it reported no longer holds.
            "release" => {
                if let Some(track) = self.tracks.get_mut(&view.pane_id) {
                    track.report = None;
                    if !track.identified {
                        self.tracks.remove(&view.pane_id);
                    }
                }
                return;
            }
            _ => return,
        };
        let track = self.tracks.entry(view.pane_id).or_insert_with(|| {
            let mut track = Track::new(
                view.session_id,
                report.agent.clone().unwrap_or_else(|| "agent".to_string()),
                None,
                false,
            );
            // A reporting agent's title is its own from the start.
            track.inherited = Some((None, None));
            track
        });
        if let Some(agent) = &report.agent
            && !track.identified
        {
            track.agent.clone_from(agent);
        }
        track.report = Some((state, report.message.clone()));
        track.report_epoch = self.epoch;
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
            let view = self.views.get(&pane);
            let track = self.tracks.entry(pane).or_insert_with(|| {
                let mut track = Track::new(
                    job.session_id,
                    agent.clone(),
                    Some(job.process_group_id),
                    true,
                );
                track.inherited = view.map(|view| (view.title.clone(), view.progress.clone()));
                track
            });
            track.identified = true;
            track.agent = agent;
        }
    }

    /// Agent panes whose screen changed since it was last read.
    pub fn screens_to_read(&self) -> Vec<(Uuid, Uuid)> {
        self.tracks
            .iter()
            .filter(|(pane, track)| {
                self.views
                    .get(pane)
                    .is_some_and(|view| track.screen_revision != Some(view.screen_revision))
            })
            .map(|(pane, track)| (track.session_id, *pane))
            .collect()
    }

    /// The agent running in `pane`, if any.
    #[must_use]
    pub fn agent(&self, pane: Uuid) -> Option<&str> {
        self.tracks.get(&pane).map(|track| track.agent.as_str())
    }

    /// Take `pane`'s screen as read at `revision` and `now_ms`, with its
    /// agent's rules when it has them.
    pub fn apply_screen(
        &mut self,
        pane: Uuid,
        revision: u64,
        screen: &str,
        rules: Option<&rules::Agent>,
        now_ms: u64,
    ) {
        let Some(track) = self.tracks.get_mut(&pane) else {
            return;
        };
        track.screen_revision = Some(revision);
        // A redraw that leaves the screen as it was is not activity.
        if track.screen.as_deref() != Some(screen) {
            if track.screen.is_some() {
                track.screen_changed_at_ms = Some(now_ms);
            }
            track.screen = Some(screen.to_string());
        }
        if let Some(rules) = rules {
            let verdict = rules.evaluate_channel(
                &rules::Input {
                    screen,
                    ..rules::Input::default()
                },
                Channel::Screen,
            );
            match verdict {
                // A viewer or menu says nothing: keep what the screen last said.
                Some(verdict) if verdict.skip => {}
                verdict => track.screen_verdict = verdict,
            }
        }
    }

    /// Decide every agent's state at `now_ms`.
    pub fn settle<'a>(
        &mut self,
        now_ms: u64,
        rules_for: impl Fn(&str) -> Option<&'a rules::Agent>,
    ) {
        for (pane, track) in &mut self.tracks {
            let view = self.views.get(pane);
            let (state, source, rule) = classify(track, view, rules_for(&track.agent), now_ms);
            let previous = track.state.as_ref().map(|(state, ..)| *state);
            if previous != Some(state) {
                track.changed_at_ms = now_ms;
                if previous == Some(AgentState::Working) {
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
                    .and_then(|(_, message)| message.clone())
                    .or_else(|| {
                        (state == AgentState::Blocked)
                            .then(|| self.views.get(pane)?.last_notification.clone())
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

/// One agent's state, source and deciding rule (see the module docs).
fn classify(
    track: &Track,
    view: Option<&PaneView>,
    rules: Option<&rules::Agent>,
    now_ms: u64,
) -> (AgentState, AgentSource, Option<String>) {
    let title = view
        .and_then(|view| view.title.as_deref())
        .filter(|_| track.title_live)
        .unwrap_or("");
    let progress = view
        .and_then(|view| view.progress.as_deref())
        .filter(|_| track.progress_live)
        .unwrap_or("");
    let signals = rules::Input {
        title,
        progress,
        ..rules::Input::default()
    };
    let says = |verdict: &rules::Verdict| match verdict.state {
        RuleState::Working => Some(true),
        RuleState::Idle | RuleState::Blocked => Some(false),
        RuleState::Unknown => None,
    };
    // 1. Is it working? A working report newer than an idle or blocked
    // title or progress outranks it: that title is stale.
    let report = track.report.as_ref().map(|(state, _)| *state);
    let signalled = rules
        .and_then(|rules| {
            [Channel::Title, Channel::Progress]
                .into_iter()
                .filter_map(|channel| rules.evaluate_channel(&signals, channel))
                .filter(|verdict| !verdict.skip)
                .find_map(|verdict| {
                    says(&verdict).map(|working| (working, verdict.state, verdict.rule))
                })
        })
        .filter(|(working, ..)| {
            *working
                || report != Some(AgentState::Working)
                || track.report_epoch <= track.signal_epoch
        });
    // A title or progress that says the agent is asking, whichever path
    // decides it is not working.
    let channel_blocked = signalled
        .as_ref()
        .filter(|(_, state, _)| *state == RuleState::Blocked)
        .map(|(.., rule)| rule.clone());
    let working = signalled
        .map(|(working, _, rule)| (working, AgentSource::Screen, Some(rule)))
        .or_else(|| report.map(|state| (state == AgentState::Working, AgentSource::Report, None)))
        .or_else(|| {
            let verdict = track.screen_verdict.as_ref()?;
            let working = says(verdict)?;
            Some((working, AgentSource::Screen, Some(verdict.rule.clone())))
        })
        .unwrap_or_else(|| {
            if rules.is_some() {
                // A known agent no rule matches is waiting (as herdr has it).
                (false, AgentSource::Screen, None)
            } else {
                let active = track
                    .screen_changed_at_ms
                    .is_some_and(|at| now_ms.saturating_sub(at) < ACTIVE_WINDOW_MS);
                (active, AgentSource::Activity, None)
            }
        });
    if working.0 {
        return (AgentState::Working, working.1, working.2);
    }
    // 2. Not working: is it waiting for an answer? Its title or progress,
    // its screen, or its report can say so.
    if let Some(rule) = channel_blocked {
        return (AgentState::Blocked, AgentSource::Screen, Some(rule));
    }
    if let Some(verdict) = track
        .screen_verdict
        .as_ref()
        .filter(|verdict| verdict.state == RuleState::Blocked)
    {
        return (
            AgentState::Blocked,
            AgentSource::Screen,
            Some(verdict.rule.clone()),
        );
    }
    if report == Some(AgentState::Blocked) {
        return (AgentState::Blocked, AgentSource::Report, None);
    }
    // An idle title, progress signal or report can describe the foreground
    // turn while a live screen indicator still shows background work.
    if let Some(verdict) = track
        .screen_verdict
        .as_ref()
        .filter(|verdict| verdict.background_working)
    {
        return (
            AgentState::Working,
            AgentSource::Screen,
            Some(verdict.rule.clone()),
        );
    }
    (AgentState::Idle, working.1, working.2)
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

    fn view(title: Option<&str>, screen_revision: u64) -> PaneView {
        PaneView {
            session_id: SESSION,
            pane_id: PANE,
            title: title.map(str::to_string),
            progress: None,
            last_notification: None,
            report: None,
            reports: 0,
            screen_revision,
        }
    }

    /// A pane: its retained signals, then its screen read at that revision,
    /// then a settled decision at `now`.
    struct Pane {
        tracker: Tracker,
        rules: rules::Agent,
        revision: u64,
    }

    impl Pane {
        fn claude() -> Self {
            let mut tracker = Tracker::default();
            tracker.set_views(vec![view(None, 0)]);
            tracker.set_jobs(
                &[job(
                    40,
                    "/nix/store/x-claude-code/bin/.claude-wrapped_ --resume",
                )],
                &catalog(),
            );
            Self {
                tracker,
                rules: claude(),
                revision: 0,
            }
        }

        fn show(
            &mut self,
            title: Option<&str>,
            screen: &str,
            now: u64,
        ) -> (AgentState, AgentSource) {
            self.revision += 1;
            self.tracker.set_views(vec![view(title, self.revision)]);
            for (_, pane) in self.tracker.screens_to_read() {
                self.tracker
                    .apply_screen(pane, self.revision, screen, Some(&self.rules), now);
            }
            let rules = &self.rules;
            self.tracker
                .settle(now, |agent| (agent == "claude").then_some(rules));
            let pane = self.tracker.snapshot().remove(0);
            (pane.state, pane.source)
        }
    }

    // Claude Code 2.1.318's screens, as captured: working (the prompt box
    // stays on screen), asking for permission, and idle.
    const WORKING: &str = "❯ touch marker\n✻ Pondering… (3s · esc to interrupt)\n─────\n❯ \n─────\n  ⏸ manual mode on";
    const ASKING: &str = " Bash command\n touch marker\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n Esc to cancel · Tab to amend";
    const IDLE: &str =
        "⏺ done\n✻ Crunched for 2s · done 7:05 PM\n─────\n❯ \n─────\n  ⏸ manual mode on";

    /// Working, asking for permission and idle follow Claude's title and
    /// screen exactly as captured, the moment they change: no settling.
    #[test]
    fn claude_works_asks_and_stops_as_its_title_and_screen_say() {
        let mut pane = Pane::claude();
        assert_eq!(
            pane.show(Some("✳ Claude Code"), IDLE, 1_000).0,
            AgentState::Idle
        );
        // The spinner in the title: working, though the prompt box shows.
        assert_eq!(
            pane.show(Some("◐ Touch probe marker file"), WORKING, 2_000)
                .0,
            AgentState::Working
        );
        // Waiting for permission: the title stops spinning, a dialog shows.
        let (state, source) = pane.show(Some("✳ Touch probe marker file"), ASKING, 3_000);
        assert_eq!((state, source), (AgentState::Blocked, AgentSource::Screen));
        assert_eq!(
            pane.show(Some("◑ Touch probe marker file"), WORKING, 4_000)
                .0,
            AgentState::Working
        );
        // Done: idle at once, and finished then.
        assert_eq!(
            pane.show(Some("✳ Touch probe marker file"), IDLE, 4_001).0,
            AgentState::Idle
        );
        let pane_agent = pane.tracker.snapshot().remove(0);
        assert_eq!(pane_agent.finished_at_ms, Some(4_001));
        assert_eq!(pane_agent.rule.as_deref(), Some("osc_title_idle"));
    }

    /// What one channel shows cannot hide what another says: a working title
    /// is working even when old transcript text looks like a dialog.
    #[test]
    fn a_dialog_in_the_transcript_does_not_stop_a_working_agent() {
        let mut pane = Pane::claude();
        pane.show(Some("✳ Claude Code"), IDLE, 1_000);
        let transcript = format!("{ASKING}\n⏺ ran it\n{WORKING}");
        assert_eq!(
            pane.show(Some("◐ Next step"), &transcript, 2_000).0,
            AgentState::Working
        );
    }

    /// The state follows what the pane shows now, however many changes were
    /// missed in between.
    #[test]
    fn missed_changes_do_not_leave_a_stale_state() {
        let mut pane = Pane::claude();
        pane.show(Some("◐ Long task"), WORKING, 1_000);
        // Many spinner frames and a permission round-trip later: idle.
        pane.revision += 50;
        assert_eq!(
            pane.show(Some("✳ Long task"), IDLE, 60_000).0,
            AgentState::Idle
        );
    }

    /// A title the pane had before the agent started is not the agent's.
    #[test]
    fn an_inherited_title_counts_only_once_it_changes() {
        let mut tracker = Tracker::default();
        let rules = claude();
        tracker.set_views(vec![view(Some("◐ old run"), 1)]);
        tracker.set_jobs(&[job(40, "claude")], &catalog());
        tracker.set_views(vec![view(Some("◐ old run"), 2)]);
        tracker.apply_screen(PANE, 2, IDLE, Some(&rules), 1_000);
        tracker.settle(1_000, |_| Some(&rules));
        assert_eq!(tracker.snapshot()[0].state, AgentState::Idle);
        tracker.set_views(vec![view(Some("◑ new run"), 3)]);
        tracker.settle(2_000, |_| Some(&rules));
        assert_eq!(tracker.snapshot()[0].state, AgentState::Working);
    }

    /// An agent's own report decides when its title says nothing, binds to
    /// its job, and a waiting report or screen blocker shows it asking.
    #[test]
    fn reports_bind_to_their_job_and_rank_below_the_title() {
        let rules = claude();
        let mut tracker = Tracker::default();
        let reported = |reports: u64, state: &str, title: Option<&str>| PaneView {
            report: Some(Report {
                state: state.into(),
                agent: Some("claude".into()),
                message: Some("approve edit?".into()),
            }),
            reports,
            ..view(title, reports)
        };
        tracker.set_views(vec![view(None, 0)]);
        // A report arriving before the job is listed binds to it.
        tracker.set_views(vec![reported(1, "working", None)]);
        tracker.settle(0, |_| Some(&rules));
        assert_eq!(
            tracker.snapshot()[0].source,
            AgentSource::Report,
            "{:?}",
            tracker.snapshot()
        );
        assert_eq!(tracker.snapshot()[0].state, AgentState::Working);
        tracker.set_jobs(&[job(40, "claude")], &catalog());

        tracker.set_views(vec![reported(2, "blocked", None)]);
        tracker.settle(100, |_| Some(&rules));
        let agent = tracker.snapshot().remove(0);
        assert_eq!(
            (agent.state, agent.source),
            (AgentState::Blocked, AgentSource::Report)
        );
        assert_eq!(agent.message.as_deref(), Some("approve edit?"));

        // A stale "working" yields to an idle title.
        tracker.set_views(vec![reported(3, "working", Some("✳ done"))]);
        tracker.settle(200, |_| Some(&rules));
        assert_eq!(tracker.snapshot()[0].state, AgentState::Idle);

        // Another job in the pane ends the agent and its report.
        tracker.set_jobs(&[job(41, "vim")], &catalog());
        assert_eq!(tracker.snapshot().len(), 0);
    }

    /// A working report that arrives after the title last changed is newer
    /// than the idle title, which is stale; a title change after the report
    /// is newer again.
    #[test]
    fn a_working_report_outranks_an_older_idle_title() {
        let rules = claude();
        let mut tracker = Tracker::default();
        let reported = |reports: u64, state: &str, title: &str| PaneView {
            report: Some(Report {
                state: state.into(),
                agent: Some("claude".into()),
                message: None,
            }),
            reports,
            ..view(Some(title), reports)
        };
        tracker.set_views(vec![view(None, 0)]);
        tracker.set_views(vec![reported(1, "idle", "✳ ready")]);
        tracker.set_jobs(&[job(40, "claude")], &catalog());
        tracker.settle(10, |_| Some(&rules));
        assert_eq!(tracker.snapshot()[0].state, AgentState::Idle);

        // The title stays idle; the agent reports working.
        tracker.set_views(vec![reported(2, "working", "✳ ready")]);
        tracker.settle(20, |_| Some(&rules));
        let agent = tracker.snapshot().remove(0);
        assert_eq!(
            (agent.state, agent.source),
            (AgentState::Working, AgentSource::Report)
        );

        // Unchanged views keep it so.
        tracker.set_views(vec![reported(2, "working", "✳ ready")]);
        tracker.settle(30, |_| Some(&rules));
        assert_eq!(tracker.snapshot()[0].state, AgentState::Working);

        // The title changes after the report: the report is the stale one.
        tracker.set_views(vec![reported(2, "working", "✳ all done")]);
        tracker.settle(40, |_| Some(&rules));
        assert_eq!(tracker.snapshot()[0].state, AgentState::Idle);
    }

    /// A title that says the agent asks makes it blocked, with no
    /// background work on its screen too.
    #[test]
    fn a_blocked_title_asks_without_background_work() {
        let codex = rules::Agent::parse(
            rules::BUNDLED
                .iter()
                .find(|(name, _)| *name == "codex")
                .unwrap()
                .1,
        )
        .unwrap();
        let catalog = Catalog::new(
            [(codex.id.as_str(), codex.aliases.as_slice())],
            &[] as &[String],
        );
        let mut tracker = Tracker::default();
        tracker.set_views(vec![view(None, 0)]);
        tracker.set_jobs(&[job(40, "codex")], &catalog);
        tracker.set_views(vec![view(Some("Action Required: approve"), 1)]);
        tracker.settle(10, |_| Some(&codex));
        let agent = tracker.snapshot().remove(0);
        assert_eq!(
            (agent.state, agent.rule.as_deref()),
            (AgentState::Blocked, Some("osc_title_blocked"))
        );
    }

    #[test]
    fn live_background_work_outlasts_an_idle_title_and_report() {
        let rules = claude();
        let mut tracker = Tracker::default();
        tracker.set_views(vec![view(None, 0)]);
        tracker.set_jobs(&[job(40, "claude")], &catalog());
        let footer = "✶ Waiting for 2 background agents to finish\n─────\n❯ \n─────";
        let update = |revision, state: &str| PaneView {
            report: Some(Report {
                state: state.into(),
                agent: Some("claude".into()),
                message: None,
            }),
            reports: revision,
            ..view(Some("✳ ready"), revision)
        };
        tracker.set_views(vec![update(1, "idle")]);
        tracker.apply_screen(PANE, 1, footer, Some(&rules), 10);
        tracker.settle(10, |_| Some(&rules));
        assert_eq!(tracker.snapshot()[0].state, AgentState::Working);

        tracker.set_views(vec![update(2, "blocked")]);
        tracker.settle(20, |_| Some(&rules));
        assert_eq!(tracker.snapshot()[0].state, AgentState::Blocked);

        tracker.set_views(vec![update(3, "idle")]);
        tracker.apply_screen(PANE, 3, "─────\n❯ \n─────", Some(&rules), 30);
        tracker.settle(30, |_| Some(&rules));
        assert_eq!(tracker.snapshot()[0].state, AgentState::Idle);
    }

    /// A report from before the agents plugin saw the pane is history, and a
    /// released report leaves with an agent that only reported.
    #[test]
    fn old_and_released_reports_do_not_make_agents() {
        let mut tracker = Tracker::default();
        let report = |reports: u64, state: &str| PaneView {
            report: Some(Report {
                state: state.into(),
                agent: Some("bot".into()),
                message: None,
            }),
            reports,
            ..view(None, 0)
        };
        tracker.set_views(vec![report(5, "working")]);
        tracker.settle(0, |_| None);
        assert_eq!(tracker.snapshot().len(), 0);
        tracker.set_views(vec![report(6, "working")]);
        tracker.settle(0, |_| None);
        assert_eq!(tracker.snapshot().len(), 1);
        tracker.set_views(vec![report(7, "release")]);
        tracker.settle(0, |_| None);
        assert_eq!(tracker.snapshot().len(), 0);
        // Unknown states are ignored.
        tracker.set_views(vec![report(8, "dancing")]);
        assert_eq!(tracker.snapshot().len(), 0);
    }

    /// An agent without rules works while its screen changes; redraws that
    /// leave it as it was are not work.
    #[test]
    fn agents_without_rules_work_while_their_screen_changes() {
        let mut tracker = Tracker::default();
        tracker.set_views(vec![view(None, 0)]);
        tracker.set_jobs(&[job(50, "aider")], &catalog());
        let show = |tracker: &mut Tracker, revision: u64, screen: &str, now: u64| {
            tracker.set_views(vec![view(None, revision)]);
            for (_, pane) in tracker.screens_to_read() {
                tracker.apply_screen(pane, revision, screen, None, now);
            }
            tracker.settle(now, |_| None);
            let agent = tracker.snapshot().remove(0);
            (agent.state, agent.source)
        };
        assert_eq!(show(&mut tracker, 1, "> ", 1_000).0, AgentState::Idle);
        assert_eq!(
            show(&mut tracker, 2, "> thinking.", 2_000),
            (AgentState::Working, AgentSource::Activity)
        );
        // Empty synchronized redraws move the revision, not the screen.
        for (revision, now) in [(3, 3_000), (4, 4_000), (5, 5_500)] {
            show(&mut tracker, revision, "> thinking.", now);
        }
        assert_eq!(
            show(&mut tracker, 6, "> thinking.", 5_500).0,
            AgentState::Idle
        );
        // Unknown programs are not agents.
        tracker.set_jobs(&[job(51, "make test")], &catalog());
        assert_eq!(tracker.snapshot().len(), 0);
    }

    /// A pane that is gone takes its agent with it, missed exit event or not.
    #[test]
    fn a_vanished_pane_ends_its_agent() {
        let mut pane = Pane::claude();
        pane.show(Some("◐ x"), WORKING, 1_000);
        pane.tracker.set_views(Vec::new());
        assert_eq!(pane.tracker.snapshot().len(), 0);
        assert!(pane.tracker.changed(&[]));
        assert!(!pane.tracker.changed(&[]));
    }
}
