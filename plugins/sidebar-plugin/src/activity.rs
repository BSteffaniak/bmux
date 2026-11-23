//! What each tab is doing, as this client shows it.
//!
//! The tabs plugin publishes each tab's status (`tabs-status`): its running
//! command, directory, git status, last exit status, and when it last produced
//! output or asked for attention. This turns that, plus when this client last
//! viewed each tab, into one state per tab:
//!
//! - **blocked**: it rang the bell or sent a notification since it was viewed
//!   (an agent waiting for input, usually);
//! - **failed**: its last command exited non-zero;
//! - **working**: a command or agent is running and produced output recently;
//! - **done**: an agent's output stopped since the tab was last viewed;
//! - **idle**: otherwise.
//!
//! Server times are compared with the server's own clock (`now-ms` at
//! publication, advanced by the time since it arrived), never this client's.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use bmux_plugin::{RenderColor, RenderNamedColor};
use bmux_tabs_plugin_api::tabs_status::{TabStatus, TabStatusSnapshot};
use uuid::Uuid;

/// Output within this long counts as working.
const WORKING_WINDOW: Duration = Duration::from_secs(3);

/// Programs treated as coding agents (by the running command's first word).
pub const DEFAULT_AGENTS: &[&str] = &[
    "claude",
    "codex",
    "aider",
    "gemini",
    "opencode",
    "amp",
    "goose",
    "crush",
    "cursor-agent",
    "qwen",
    "copilot",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabState {
    Blocked,
    Failed,
    Working,
    Done,
    Idle,
}

impl TabState {
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::Blocked => '◆',
            Self::Failed => '▲',
            Self::Working => '◉',
            Self::Done => '●',
            Self::Idle => '○',
        }
    }

    #[must_use]
    pub const fn color(self) -> RenderColor {
        RenderColor::Named(match self {
            Self::Blocked => RenderNamedColor::Yellow,
            Self::Failed => RenderNamedColor::Red,
            Self::Working => RenderNamedColor::Green,
            Self::Done => RenderNamedColor::Blue,
            Self::Idle => RenderNamedColor::BrightBlack,
        })
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Working => "working",
            Self::Done => "done",
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
    pub agent: bool,
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
    agents: Vec<String>,
    home: Option<String>,
}

impl Default for ActivityView {
    fn default() -> Self {
        Self {
            snapshot: None,
            viewed: HashMap::new(),
            agents: DEFAULT_AGENTS
                .iter()
                .map(|agent| (*agent).to_string())
                .collect(),
            home: std::env::var("HOME").ok().filter(|home| !home.is_empty()),
        }
    }
}

impl ActivityView {
    /// Replace the agent programs (from settings).
    pub fn set_agents(&mut self, agents: Vec<String>) {
        self.agents = agents;
    }

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

    fn is_agent(&self, command: Option<&str>) -> bool {
        let Some(program) = command
            .and_then(|command| command.split_whitespace().next())
            .map(|program| program.rsplit('/').next().unwrap_or(program))
        else {
            return false;
        };
        self.agents.iter().any(|agent| agent == program)
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
        let agent = self.is_agent(command.as_deref());
        let recent_output = status.output_at_ms.is_some_and(|at| {
            now.saturating_sub(at) < u64::try_from(WORKING_WINDOW.as_millis()).unwrap_or(u64::MAX)
        });
        let unseen_attention = status.attention_at_ms.is_some_and(|at| at > viewed);
        let state = if unseen_attention {
            TabState::Blocked
        } else if status.last_exit.is_some_and(|code| code != 0) && command.is_none() {
            TabState::Failed
        } else if recent_output && (agent || command.is_some()) {
            TabState::Working
        } else if agent && status.output_at_ms.is_some_and(|at| at > viewed) {
            TabState::Done
        } else {
            TabState::Idle
        };
        let program = command
            .as_deref()
            .and_then(|command| command.split_whitespace().next())
            .map(|program| program.rsplit('/').next().unwrap_or(program).to_string());
        let summary = match state {
            TabState::Blocked => Some("needs input".to_string()),
            TabState::Failed => status.last_exit.map(|code| format!("exit {code}")),
            TabState::Working if agent => Some("working".to_string()),
            TabState::Done => Some("done".to_string()),
            _ => program.clone(),
        };
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
            && let Some(attention) = status.attention.as_deref()
        {
            attention.to_string()
        } else {
            let mut parts = Vec::new();
            if let Some(cwd) = &cwd {
                parts.push(cwd.clone());
            }
            if let Some(branch) = &branch {
                parts.push(branch.clone());
            }
            if matches!(state, TabState::Done | TabState::Idle)
                && agent
                && let Some(at) = status.output_at_ms
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
            agent,
        })
    }

    /// Whether any tab's shown state can change with time alone (output
    /// stopping, ages ticking), so the view should be refreshed.
    #[must_use]
    pub fn time_sensitive(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|(snapshot, _)| snapshot.tabs.iter().any(|tab| tab.output_at_ms.is_some()))
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
    fn agents_move_through_working_done_and_idle() {
        let tab = Uuid::from_u128(1);
        let mut agent = status(1);
        agent.running_command = Some("claude --resume".into());
        agent.output_at_ms = Some(9_000);
        agent.cwd = Some("/home/me/repo".into());
        agent.git = Some(GitStatus {
            branch: "main".into(),
            dirty: true,
            ahead: 2,
            behind: 0,
        });
        let working = view(vec![agent.clone()], 10_000);
        let activity = working.activity(tab, false).unwrap();
        assert_eq!(activity.state, TabState::Working);
        assert_eq!(activity.summary.as_deref(), Some("working"));
        assert_eq!(activity.detail, "~/repo · main* ↑2");
        assert!(activity.agent);
        // Output stopped a minute ago and nobody looked: done.
        let mut done = view(vec![agent], 70_000);
        assert_eq!(done.activity(tab, false).unwrap().state, TabState::Done);
        assert_eq!(
            done.activity(tab, false).unwrap().detail,
            "~/repo · main* ↑2 · 1m"
        );
        // Viewing it makes it idle.
        done.mark_viewed(tab);
        assert_eq!(done.activity(tab, false).unwrap().state, TabState::Idle);
        assert_eq!(done.activity(tab, true).unwrap().state, TabState::Idle);
    }

    #[test]
    fn attention_and_failures_outrank_activity() {
        let tab = Uuid::from_u128(2);
        let mut blocked = status(2);
        blocked.running_command = Some("codex".into());
        blocked.output_at_ms = Some(9_900);
        blocked.attention_at_ms = Some(9_950);
        blocked.attention = Some("approve edit?".into());
        let mut view_blocked = view(vec![blocked], 10_000);
        let activity = view_blocked.activity(tab, false).unwrap();
        assert_eq!(activity.state, TabState::Blocked);
        assert_eq!(activity.detail, "approve edit?");
        view_blocked.mark_viewed(tab);
        assert_eq!(
            view_blocked.activity(tab, false).unwrap().state,
            TabState::Working
        );

        let mut failed = status(2);
        failed.last_exit = Some(101);
        let activity = view(vec![failed], 10_000).activity(tab, false).unwrap();
        assert_eq!(activity.state, TabState::Failed);
        assert_eq!(activity.summary.as_deref(), Some("exit 101"));

        let mut plain = status(2);
        plain.running_command = Some("/usr/bin/nvim notes.md".into());
        let activity = view(vec![plain], 10_000).activity(tab, false).unwrap();
        assert_eq!(activity.state, TabState::Idle);
        assert_eq!(activity.summary.as_deref(), Some("nvim"));
        assert!(!activity.agent);
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
