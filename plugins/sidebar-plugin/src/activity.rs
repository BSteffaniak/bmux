//! What each tab is doing, as this client shows it.
//!
//! The tabs plugin publishes each tab's status (`tabs-status`): its running
//! command, directory, git status, last exit status, its most pressing coding
//! agent (from the agents plugin), and the one state every view shows for it,
//! decided on the server:
//!
//! - **needs input**: its agent needs an answer or approval, or a pane rang
//!   the bell or sent a notification since anyone looked at the tab;
//! - **failed**: a command exited non-zero (until seen, by the tabs plugin's
//!   `failed_clears` setting);
//! - **done**: its agent finished since anyone looked at the tab;
//! - **working**: its agent works, or a command runs and prints;
//! - **stopped**: its agent finished or waits at its prompt, and that was seen;
//! - **idle**: otherwise.
//!
//! This turns that into what a card says: its summary, detail and ages.
//! Server times are compared with the server's own clock (`now-ms` at
//! publication, advanced by the time since it arrived), never this client's.

use std::fmt::Write as _;
use std::time::Instant;

use bmux_plugin::{RenderColor, RenderNamedColor};
use bmux_tabs_plugin_api::tabs_status::{self, GitStatus, TabStatus, TabStatusSnapshot};
use uuid::Uuid;

/// Ordered most pressing first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TabState {
    Blocked,
    Failed,
    Done,
    Working,
    Stopped,
    Idle,
}

impl From<tabs_status::TabState> for TabState {
    fn from(state: tabs_status::TabState) -> Self {
        match state {
            tabs_status::TabState::NeedsInput => Self::Blocked,
            tabs_status::TabState::Failed => Self::Failed,
            tabs_status::TabState::Done => Self::Done,
            tabs_status::TabState::Working => Self::Working,
            tabs_status::TabState::Stopped => Self::Stopped,
            tabs_status::TabState::Idle => Self::Idle,
        }
    }
}

impl TabState {
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::Blocked => '◆',
            Self::Failed => '▲',
            Self::Done => '✓',
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
            Self::Done => RenderNamedColor::Cyan,
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
            Self::Done => "done",
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
    /// The focused pane's git status, when it is in a repository.
    pub git: Option<GitStatus>,
    /// What needs the user, when the tab is blocked and says.
    pub message: Option<String>,
    /// How long the tab has been in its state, when that is known: since its
    /// agent started working or stopped.
    pub since: Option<String>,
}

impl TabActivity {
    /// This activity with its detail fitted to `width` cells by shortening the
    /// directory from the left (its end says more than its start).
    #[must_use]
    pub fn fitted(mut self, width: usize) -> Self {
        let length = bmux_cell_geometry::display_width(&self.detail);
        if length > width
            && let Some(cwd) = &self.cwd
            && let Some(rest) = self.detail.strip_prefix(cwd.as_str())
        {
            let room = width
                .saturating_sub(bmux_cell_geometry::display_width(rest))
                .max(4);
            self.detail = format!("{}{rest}", shorten_left(cwd, room));
        }
        self
    }
}

#[derive(Debug)]
pub struct ActivityView {
    snapshot: Option<(TabStatusSnapshot, Instant)>,
    positions: std::collections::HashMap<Uuid, usize>,
    home: Option<String>,
}

impl Default for ActivityView {
    fn default() -> Self {
        Self {
            snapshot: None,
            positions: std::collections::HashMap::new(),
            home: std::env::var("HOME").ok().filter(|home| !home.is_empty()),
        }
    }
}

/// Shell integration reports the full command, including leading environment assignments.
fn command_program(command: &str) -> Option<String> {
    fn is_assignment(word: &str) -> bool {
        let Some((name, _)) = word.split_once('=') else {
            return false;
        };
        let mut bytes = name.bytes();
        bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }

    let first = command.split_whitespace().next()?;
    let program = if is_assignment(first) {
        shlex::Shlex::new(command).find(|word| !is_assignment(word))?
    } else {
        first.to_string()
    };
    Some(program.rsplit('/').next().unwrap_or(&program).to_string())
}

impl ActivityView {
    /// Take a new status snapshot.
    pub fn replace(&mut self, snapshot: TabStatusSnapshot) {
        self.positions.clear();
        for (index, status) in snapshot.tabs.iter().enumerate() {
            self.positions.entry(status.tab_id).or_insert(index);
        }
        self.snapshot = Some((snapshot, Instant::now()));
    }

    /// The latest status snapshot.
    #[cfg(test)]
    #[must_use]
    pub fn snapshot(&self) -> Option<&TabStatusSnapshot> {
        self.snapshot.as_ref().map(|(snapshot, _)| snapshot)
    }

    /// The server clock now.
    fn now_ms(&self) -> Option<u64> {
        self.snapshot.as_ref().map(|(snapshot, received)| {
            snapshot
                .now_ms
                .saturating_add(u64::try_from(received.elapsed().as_millis()).unwrap_or(u64::MAX))
        })
    }

    fn status(&self, tab: Uuid) -> Option<&TabStatus> {
        self.snapshot
            .as_ref()
            .and_then(|(snapshot, _)| snapshot.tabs.get(*self.positions.get(&tab)?))
    }

    pub fn state(&self, tab: Uuid) -> Option<TabState> {
        self.status(tab).map(|status| TabState::from(status.state))
    }

    /// `tab`'s activity: its published state, and what its card says.
    #[must_use]
    pub fn activity(&self, tab: Uuid) -> Option<TabActivity> {
        let status = self.status(tab)?;
        let now = self.now_ms()?;
        let state = TabState::from(status.state);
        let program = status.running_command.as_deref().and_then(command_program);
        let agent = status.agent.as_ref();
        let summary = match state {
            TabState::Blocked => Some("needs input".to_string()),
            TabState::Failed => status.last_exit.map(|code| format!("exit {code}")),
            TabState::Done => Some("done".to_string()),
            TabState::Working if agent.is_some() => Some("working".to_string()),
            TabState::Stopped => Some("stopped".to_string()),
            TabState::Working | TabState::Idle => program.clone(),
        };
        let message = (state == TabState::Blocked)
            .then(|| {
                agent
                    .and_then(|agent| agent.message.clone())
                    .or_else(|| status.attention.clone())
            })
            .flatten();
        let since = status
            .state_since_ms
            .filter(|_| {
                matches!(
                    state,
                    TabState::Working | TabState::Stopped | TabState::Done
                )
            })
            .map(|at| format_age(now.saturating_sub(at)));
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
        let detail = message.clone().unwrap_or_else(|| {
            let mut parts = Vec::new();
            if let Some(cwd) = &cwd {
                parts.push(cwd.clone());
            }
            if let Some(branch) = &branch {
                parts.push(branch.clone());
            }
            // How long an agent has waited since it finished.
            if matches!(state, TabState::Stopped | TabState::Done)
                && let Some(since) = &since
            {
                parts.push(since.clone());
            }
            parts.join(" · ")
        });
        Some(TabActivity {
            state,
            summary,
            detail,
            command: program,
            cwd,
            branch,
            agent: agent.map(|agent| agent.name.clone()),
            git: status.git.clone(),
            message,
            since,
        })
    }

    /// Whether any tab is working, so a working spinner may be on screen.
    #[must_use]
    pub fn any_working(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|(snapshot, _)| {
            snapshot
                .tabs
                .iter()
                .any(|tab| tab.state == tabs_status::TabState::Working)
        })
    }

    /// Whether any tab's shown ages can change with time alone, so the view
    /// should be refreshed.
    #[must_use]
    pub fn time_sensitive(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|(snapshot, _)| {
            snapshot.tabs.iter().any(|tab| tab.state_since_ms.is_some())
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

/// Shorten a path from the left to `width` terminal cells, keeping its end
/// whole graphemes at a time.
#[must_use]
pub fn shorten_left(text: &str, width: usize) -> String {
    if bmux_cell_geometry::display_width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 1; // The ellipsis.
    for grapheme in bmux_unicode_segmentation::graphemes(text).rev() {
        let cells = bmux_cell_geometry::display_width(grapheme);
        if used + cells > width {
            break;
        }
        used += cells;
        kept.push(grapheme);
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tabs_plugin_api::tabs_status::{AgentActivity, TabAgent};

    fn status(tab: u128, state: tabs_status::TabState) -> TabStatus {
        TabStatus {
            tab_id: Uuid::from_u128(tab),
            state,
            state_since_ms: None,
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

    fn agent(activity: AgentActivity) -> TabAgent {
        TabAgent {
            name: "claude".into(),
            activity,
            message: (activity == AgentActivity::Blocked).then(|| "approve edit?".into()),
            changed_at_ms: 1_000,
            finished_at_ms: None,
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

    /// A card shows the state the tabs plugin published: what it says, and
    /// for how long.
    #[test]
    fn cards_say_what_the_published_state_is() {
        let tab = Uuid::from_u128(1);
        let mut working = status(1, tabs_status::TabState::Working);
        working.running_command = Some("claude --resume".into());
        working.cwd = Some("/home/me/repo".into());
        working.git = Some(GitStatus {
            branch: "main".into(),
            dirty: true,
            ahead: 2,
            behind: 0,
        });
        working.agent = Some(agent(AgentActivity::Working));
        working.state_since_ms = Some(4_000);
        let shown = view(vec![working.clone()], 10_000).activity(tab).unwrap();
        assert_eq!(shown.state, TabState::Working);
        assert_eq!(shown.summary.as_deref(), Some("working"));
        assert_eq!(shown.since.as_deref(), Some("6s"));
        assert_eq!(shown.detail, "~/repo · main* ↑2");
        assert_eq!(shown.agent.as_deref(), Some("claude"));

        // Asking: the question is the detail.
        let mut asking = working.clone();
        asking.state = tabs_status::TabState::NeedsInput;
        asking.agent = Some(agent(AgentActivity::Blocked));
        let shown = view(vec![asking], 10_000).activity(tab).unwrap();
        assert_eq!(shown.state, TabState::Blocked);
        assert_eq!(shown.summary.as_deref(), Some("needs input"));
        assert_eq!(shown.detail, "approve edit?");

        // Finished, unseen: done, with how long ago.
        let mut done = working;
        done.state = tabs_status::TabState::Done;
        done.agent = Some(agent(AgentActivity::Idle));
        done.state_since_ms = Some(10_000);
        let shown = view(vec![done.clone()], 70_000).activity(tab).unwrap();
        assert_eq!(shown.state, TabState::Done);
        assert_eq!(shown.summary.as_deref(), Some("done"));
        assert_eq!(shown.detail, "~/repo · main* ↑2 · 1m");

        // Seen: stopped.
        let mut stopped = done;
        stopped.state = tabs_status::TabState::Stopped;
        let shown = view(vec![stopped], 70_000).activity(tab).unwrap();
        assert_eq!(shown.state, TabState::Stopped);
        assert_eq!(shown.summary.as_deref(), Some("stopped"));

        // Each state looks different at a glance: glyph, color and label.
        let looks = [
            TabState::Blocked,
            TabState::Failed,
            TabState::Done,
            TabState::Working,
            TabState::Stopped,
        ];
        for (index, state) in looks.iter().enumerate() {
            for other in &looks[index + 1..] {
                assert_ne!(state.glyph(), other.glyph());
                assert_ne!(state.color(), other.color());
                assert_ne!(state.label(), other.label());
            }
            assert_ne!(state.glyph(), TabState::Idle.glyph());
            assert_ne!(state.color(), TabState::Idle.color());
        }
        // Done ranks after failed and before working.
        assert!(TabState::Failed < TabState::Done && TabState::Done < TabState::Working);
    }

    #[test]
    fn other_programs_show_attention_failures_and_their_name() {
        let tab = Uuid::from_u128(2);
        let mut rang = status(2, tabs_status::TabState::NeedsInput);
        rang.running_command = Some("make watch".into());
        rang.attention = Some("tests failed".into());
        let shown = view(vec![rang], 10_000).activity(tab).unwrap();
        assert_eq!(shown.state, TabState::Blocked);
        assert_eq!(shown.detail, "tests failed");

        let mut printing = status(2, tabs_status::TabState::Working);
        printing.running_command = Some("make watch".into());
        let shown = view(vec![printing], 10_000).activity(tab).unwrap();
        assert_eq!(shown.summary.as_deref(), Some("make"));

        for command in [
            "BCODE_CONFIG=/home/me/.config/bcode/providers/astra.toml /home/me/GitHub/bcode/target/release/bcode",
            "BCODE_CONFIG='/home/me/provider configs/astra.toml' BCODE_PROFILE=dev '/home/me/local builds/bcode'",
        ] {
            let mut tab_status = status(2, tabs_status::TabState::Working);
            tab_status.running_command = Some(command.into());
            let shown = view(vec![tab_status], 10_000).activity(tab).unwrap();
            assert_eq!(shown.summary.as_deref(), Some("bcode"));
        }

        let mut failed = status(2, tabs_status::TabState::Failed);
        failed.last_exit = Some(101);
        let shown = view(vec![failed], 10_000).activity(tab).unwrap();
        assert_eq!(shown.state, TabState::Failed);
        assert_eq!(shown.summary.as_deref(), Some("exit 101"));

        let mut plain = status(2, tabs_status::TabState::Idle);
        plain.running_command = Some("/usr/bin/claude".into());
        let shown = view(vec![plain], 10_000).activity(tab).unwrap();
        assert_eq!(shown.state, TabState::Idle);
        assert_eq!(shown.summary.as_deref(), Some("claude"));
        assert!(shown.agent.is_none());
    }

    #[test]
    fn ages_and_paths_shorten() {
        assert_eq!(format_age(1_000), "now");
        assert_eq!(format_age(12_000), "12s");
        assert_eq!(format_age(180_000), "3m");
        assert_eq!(format_age(7_200_000), "2h");
        assert_eq!(shorten_left("~/a/long/path", 8), "…ng/path");
        assert_eq!(shorten_left("short", 8), "short");
        // Wide characters take two cells; none is split.
        assert_eq!(shorten_left("~/日本語/path", 9), "…語/path");
        assert_eq!(shorten_left("~/日本語", 4), "…語");
    }
}
