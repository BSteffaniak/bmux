//! What each tab is doing, as this client shows it.
//!
//! The tabs plugin publishes each tab's status (`tabs-status`): its running
//! command, directory, git status, last exit status, when it last produced
//! output or asked for attention, and its most pressing coding agent (from the
//! agents plugin). This turns that, plus when this client last viewed each
//! tab, into one state per tab. A tab running an agent shows the agent's:
//!
//! - **blocked**: it needs the user to answer or approve something;
//! - **working**: it is working;
//! - **stopped**: otherwise: it finished, or waits at its prompt.
//!
//! An agent's state is what the agent is doing, whichever tab this client
//! shows: viewing a tab never changes it.
//!
//! Any other tab is **blocked** when it rang the bell or sent a notification
//! since it was viewed, **failed** when its last command exited non-zero,
//! **working** while a command runs and prints, and **idle** otherwise.
//!
//! Server times are compared with the server's own clock (`now-ms` at
//! publication, advanced by the time since it arrived), never this client's.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use bmux_plugin::{RenderColor, RenderNamedColor};
use bmux_tabs_plugin_api::tabs_status::{AgentActivity, TabAgent, TabStatus, TabStatusSnapshot};
use uuid::Uuid;

/// Output within this long counts as working.
const WORKING_WINDOW: Duration = Duration::from_secs(3);

/// Ordered most pressing first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TabState {
    Blocked,
    Failed,
    Working,
    Stopped,
    Idle,
}

impl TabState {
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::Blocked => '◆',
            Self::Failed => '▲',
            Self::Working => '◉',
            Self::Stopped => '■',
            Self::Idle => '○',
        }
    }

    #[must_use]
    pub const fn color(self) -> RenderColor {
        RenderColor::Named(match self {
            Self::Blocked => RenderNamedColor::Yellow,
            Self::Failed => RenderNamedColor::Red,
            Self::Working => RenderNamedColor::Green,
            Self::Stopped => RenderNamedColor::Blue,
            Self::Idle => RenderNamedColor::BrightBlack,
        })
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Working => "working",
            Self::Stopped => "stopped",
            Self::Idle => "idle",
        }
    }
}

/// One tab's derived activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabActivity {
    pub state: TabState,
    /// Shown at the right of the tab's first line.
    pub summary: Option<String>,
    /// The tab's second line: directory, git status, age or the attention text.
    pub detail: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// The agent running in the tab, if any.
    pub agent: Option<String>,
}

impl TabActivity {
    /// This activity with its detail fitted to `width` cells by shortening the
    /// directory from the left (its end says more than its start).
    #[must_use]
    pub fn fitted(mut self, width: usize) -> Self {
        let length = self.detail.chars().count();
        if length > width
            && let Some(cwd) = &self.cwd
            && let Some(rest) = self.detail.strip_prefix(cwd.as_str())
        {
            let room = width.saturating_sub(rest.chars().count()).max(4);
            self.detail = format!("{}{rest}", shorten_left(cwd, room));
        }
        self
    }
}

#[derive(Debug)]
pub struct ActivityView {
    snapshot: Option<(TabStatusSnapshot, Instant)>,
    /// When this client last viewed each tab, on the server clock.
    viewed: HashMap<Uuid, u64>,
    home: Option<String>,
}

impl Default for ActivityView {
    fn default() -> Self {
        Self {
            snapshot: None,
            viewed: HashMap::new(),
            home: std::env::var("HOME").ok().filter(|home| !home.is_empty()),
        }
    }
}

impl ActivityView {
    /// Take a new status snapshot.
    pub fn replace(&mut self, snapshot: TabStatusSnapshot) {
        self.snapshot = Some((snapshot, Instant::now()));
    }

    /// The server clock now.
    fn now_ms(&self) -> Option<u64> {
        self.snapshot.as_ref().map(|(snapshot, received)| {
            snapshot
                .now_ms
                .saturating_add(u64::try_from(received.elapsed().as_millis()).unwrap_or(u64::MAX))
        })
    }

    /// Record that this client is looking at `tab` now.
    pub fn mark_viewed(&mut self, tab: Uuid) {
        if let Some(now) = self.now_ms() {
            self.viewed.insert(tab, now);
        }
    }

    fn status(&self, tab: Uuid) -> Option<&TabStatus> {
        self.snapshot
            .as_ref()
            .and_then(|(snapshot, _)| snapshot.tabs.iter().find(|status| status.tab_id == tab))
    }

    /// `tab`'s activity; `active` when this client is showing it.
    #[must_use]
    pub fn activity(&self, tab: Uuid, active: bool) -> Option<TabActivity> {
        let status = self.status(tab)?;
        let now = self.now_ms()?;
        let viewed = if active {
            now
        } else {
            self.viewed.get(&tab).copied().unwrap_or(0)
        };
        let command = status.running_command.clone();
        let program = command
            .as_deref()
            .and_then(|command| command.split_whitespace().next())
            .map(|program| program.rsplit('/').next().unwrap_or(program).to_string());
        let (state, summary, message) = status.agent.as_ref().map_or_else(
            || Self::program_state(status, now, viewed, program.as_deref()),
            Self::agent_state,
        );
        let cwd = status.cwd.as_deref().map(|cwd| self.shorten_home(cwd));
        let branch = status.git.as_ref().map(|git| {
            let mut branch = git.branch.clone();
            if git.dirty {
                branch.push('*');
            }
            if git.ahead > 0 {
                let _ = write!(branch, " ↑{}", git.ahead);
            }
            if git.behind > 0 {
                let _ = write!(branch, " ↓{}", git.behind);
            }
            branch
        });
        let detail = if state == TabState::Blocked
            && let Some(message) = message.or_else(|| status.attention.clone())
        {
            message
        } else {
            let mut parts = Vec::new();
            if let Some(cwd) = &cwd {
                parts.push(cwd.clone());
            }
            if let Some(branch) = &branch {
                parts.push(branch.clone());
            }
            // How long an agent has waited since it finished.
            if matches!(state, TabState::Stopped | TabState::Idle)
                && let Some(at) = status
                    .agent
                    .as_ref()
                    .map(|agent| agent.finished_at_ms.unwrap_or(agent.changed_at_ms))
            {
                parts.push(format_age(now.saturating_sub(at)));
            }
            parts.join(" · ")
        };
        Some(TabActivity {
            state,
            summary,
            detail,
            command: program,
            cwd,
            branch,
            agent: status.agent.as_ref().map(|agent| agent.name.clone()),
        })
    }

    /// An agent's state, summary and message, from what it is doing alone.
    fn agent_state(agent: &TabAgent) -> (TabState, Option<String>, Option<String>) {
        match agent.activity {
            AgentActivity::Blocked => (
                TabState::Blocked,
                Some("needs input".to_string()),
                agent.message.clone(),
            ),
            AgentActivity::Working => (TabState::Working, Some("working".to_string()), None),
            AgentActivity::Idle => (TabState::Stopped, Some("stopped".to_string()), None),
        }
    }

    /// Any other program's state and summary, from its signals.
    fn program_state(
        status: &TabStatus,
        now: u64,
        viewed: u64,
        program: Option<&str>,
    ) -> (TabState, Option<String>, Option<String>) {
        let recent_output = status.output_at_ms.is_some_and(|at| {
            now.saturating_sub(at) < u64::try_from(WORKING_WINDOW.as_millis()).unwrap_or(u64::MAX)
        });
        let running = status.running_command.is_some();
        if status.attention_at_ms.is_some_and(|at| at > viewed) {
            (TabState::Blocked, Some("needs input".to_string()), None)
        } else if status.last_exit.is_some_and(|code| code != 0) && !running {
            (
                TabState::Failed,
                status.last_exit.map(|code| format!("exit {code}")),
                None,
            )
        } else if recent_output && running {
            (TabState::Working, program.map(str::to_string), None)
        } else {
            (TabState::Idle, program.map(str::to_string), None)
        }
    }

    /// Whether any tab's shown state can change with time alone (output
    /// stopping, ages ticking), so the view should be refreshed.
    #[must_use]
    pub fn time_sensitive(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|(snapshot, _)| {
            snapshot
                .tabs
                .iter()
                .any(|tab| tab.output_at_ms.is_some() || tab.agent.is_some())
        })
    }

    fn shorten_home(&self, cwd: &str) -> String {
        match &self.home {
            Some(home) if cwd == home => "~".to_string(),
            Some(home) => cwd
                .strip_prefix(home.as_str())
                .filter(|rest| rest.starts_with('/'))
                .map_or_else(|| cwd.to_string(), |rest| format!("~{rest}")),
            None => cwd.to_string(),
        }
    }
}

/// A short age: `now`, `12s`, `3m`, `2h`, `4d`.
#[must_use]
pub fn format_age(ms: u64) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..=4 => "now".to_string(),
        5..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Shorten a path from the left to `width` cells, keeping its end.
#[must_use]
pub fn shorten_left(text: &str, width: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let keep = chars.len() - (width - 1);
    std::iter::once('…')
        .chain(chars[keep..].iter().copied())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tabs_plugin_api::tabs_status::GitStatus;

    fn status(tab: u128) -> TabStatus {
        TabStatus {
            tab_id: Uuid::from_u128(tab),
            panes: 1,
            running_command: None,
            cwd: None,
            title: None,
            git: None,
            last_exit: None,
            output_at_ms: None,
            attention_at_ms: None,
            attention: None,
            agent: None,
        }
    }

    fn agent(activity: AgentActivity, finished_at_ms: Option<u64>) -> TabAgent {
        TabAgent {
            name: "claude".into(),
            activity,
            message: (activity == AgentActivity::Blocked).then(|| "approve edit?".into()),
            changed_at_ms: 1_000,
            finished_at_ms,
        }
    }

    fn view(tabs: Vec<TabStatus>, now_ms: u64) -> ActivityView {
        let mut view = ActivityView {
            home: Some("/home/me".into()),
            ..ActivityView::default()
        };
        view.replace(TabStatusSnapshot {
            tabs,
            now_ms,
            revision: 1,
        });
        view
    }

    #[test]
    fn agents_show_working_blocked_and_stopped_distinctly() {
        let tab = Uuid::from_u128(1);
        let mut status = status(1);
        status.running_command = Some("claude --resume".into());
        status.cwd = Some("/home/me/repo".into());
        status.git = Some(GitStatus {
            branch: "main".into(),
            dirty: true,
            ahead: 2,
            behind: 0,
        });
        status.agent = Some(agent(AgentActivity::Working, None));
        let working = view(vec![status.clone()], 10_000)
            .activity(tab, false)
            .unwrap();
        assert_eq!(working.state, TabState::Working);
        assert_eq!(working.summary.as_deref(), Some("working"));
        assert_eq!(working.detail, "~/repo · main* ↑2");
        assert_eq!(working.agent.as_deref(), Some("claude"));

        // Waiting for the user's answer shows the question.
        status.agent = Some(agent(AgentActivity::Blocked, None));
        let blocked = view(vec![status.clone()], 10_000)
            .activity(tab, false)
            .unwrap();
        assert_eq!(blocked.state, TabState::Blocked);
        assert_eq!(blocked.summary.as_deref(), Some("needs input"));
        assert_eq!(blocked.detail, "approve edit?");

        // No longer working: stopped, with how long ago it finished.
        status.agent = Some(agent(AgentActivity::Idle, Some(9_000)));
        let stopped = view(vec![status], 70_000).activity(tab, false).unwrap();
        assert_eq!(stopped.state, TabState::Stopped);
        assert_eq!(stopped.summary.as_deref(), Some("stopped"));
        assert_eq!(stopped.detail, "~/repo · main* ↑2 · 1m");

        // Each state looks different at a glance: glyph, color and label.
        let looks = [working.state, blocked.state, stopped.state];
        for (index, state) in looks.iter().enumerate() {
            for other in &looks[index + 1..] {
                assert_ne!(state.glyph(), other.glyph());
                assert_ne!(state.color(), other.color());
                assert_ne!(state.label(), other.label());
            }
            // None looks like a tab doing nothing.
            assert_ne!(state.glyph(), TabState::Idle.glyph());
            assert_ne!(state.color(), TabState::Idle.color());
        }
    }

    #[test]
    fn switching_tabs_does_not_change_an_agents_state() {
        for (activity, finished, expected) in [
            (AgentActivity::Working, None, TabState::Working),
            (AgentActivity::Blocked, None, TabState::Blocked),
            (AgentActivity::Idle, Some(9_000), TabState::Stopped),
            (AgentActivity::Idle, None, TabState::Stopped),
        ] {
            let tab = Uuid::from_u128(1);
            let other = Uuid::from_u128(2);
            let mut status = status(1);
            status.running_command = Some("claude".into());
            status.agent = Some(agent(activity, finished));
            let mut view = view(vec![status, self::status(2)], 70_000);
            let mut seen = Vec::new();
            // Never viewed, then shown, then left for another tab, then back.
            seen.push(view.activity(tab, false).unwrap());
            view.mark_viewed(tab);
            seen.push(view.activity(tab, true).unwrap());
            view.mark_viewed(other);
            seen.push(view.activity(tab, false).unwrap());
            seen.push(view.activity(tab, true).unwrap());
            for shown in &seen {
                assert_eq!(shown.state, expected, "{activity:?}");
                assert_eq!(shown, &seen[0], "{activity:?}");
            }
        }
    }

    #[test]
    fn other_programs_show_attention_failures_and_output() {
        let tab = Uuid::from_u128(2);
        let mut blocked = status(2);
        blocked.running_command = Some("make watch".into());
        blocked.output_at_ms = Some(9_900);
        blocked.attention_at_ms = Some(9_950);
        blocked.attention = Some("tests failed".into());
        let mut view_blocked = view(vec![blocked], 10_000);
        let activity = view_blocked.activity(tab, false).unwrap();
        assert_eq!(activity.state, TabState::Blocked);
        assert_eq!(activity.detail, "tests failed");
        view_blocked.mark_viewed(tab);
        let activity = view_blocked.activity(tab, false).unwrap();
        assert_eq!(activity.state, TabState::Working);
        assert_eq!(activity.summary.as_deref(), Some("make"));

        let mut failed = status(2);
        failed.last_exit = Some(101);
        let activity = view(vec![failed], 10_000).activity(tab, false).unwrap();
        assert_eq!(activity.state, TabState::Failed);
        assert_eq!(activity.summary.as_deref(), Some("exit 101"));

        // A program's name alone does not make it an agent any more.
        let mut plain = status(2);
        plain.running_command = Some("/usr/bin/claude".into());
        let activity = view(vec![plain], 10_000).activity(tab, false).unwrap();
        assert_eq!(activity.state, TabState::Idle);
        assert_eq!(activity.summary.as_deref(), Some("claude"));
        assert!(activity.agent.is_none());
    }

    #[test]
    fn ages_and_paths_shorten() {
        assert_eq!(format_age(1_000), "now");
        assert_eq!(format_age(12_000), "12s");
        assert_eq!(format_age(180_000), "3m");
        assert_eq!(format_age(7_200_000), "2h");
        assert_eq!(shorten_left("~/a/long/path", 8), "…ng/path");
        assert_eq!(shorten_left("short", 8), "short");
    }
}
