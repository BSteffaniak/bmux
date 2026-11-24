//! What each tab is doing, published on the `tabs-status` state channel.
//!
//! Pane signals (title, OSC 7 directory, bell, notifications, OSC 133 exit
//! status, output) arrive as pane-runtime events and are kept per pane. Once a
//! second the board turns them, each tab's panes and running commands, and the
//! git status of the focused pane's directory into one snapshot, published
//! only when it changed.
//!
//! A pane's directory comes from two sources. The pane runtime's listing
//! carries the directory the shell reported through bmux's shell integration
//! (or the operating system reported for its process): trusted. OSC 7 is
//! whatever any output claimed (a program, a remote shell over ssh, or a file
//! someone printed), so it is shown only while the command that was running
//! when it arrived still runs, a remote host is named as such, and git only
//! ever runs in a trusted directory.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bmux_agents_plugin_api::agent_states::{AgentSnapshot, AgentState, PaneAgent};
use bmux_tabs_plugin_api::tabs_bells::TabBells;
use bmux_tabs_plugin_api::tabs_status::{AgentActivity, GitStatus, TabAgent, TabStatus};

use crate::attention::{AgentSignal, AttentionBook, FailedClears, Observation};
use uuid::Uuid;

/// How long a git status stays fresh.
const GIT_TTL: Duration = Duration::from_secs(4);
/// How long `git status` may take before it is abandoned.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// A directory named by OSC 7.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A path on this machine.
    Local(String),
    /// A path on another machine (for example through ssh).
    Remote { host: String, path: String },
}

impl Location {
    fn display(&self) -> String {
        match self {
            Self::Local(path) => path.clone(),
            Self::Remote { host, path } => format!("{host}:{path}"),
        }
    }
}

/// Which execution an OSC 7 directory describes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    /// Arrived since the board last looked at the pane.
    Pending,
    /// The command that was running when it arrived; dropped when that ends.
    Command(String),
    /// The shell itself, which reports its directory no other way.
    Shell,
}

/// A directory a pane's output reported with OSC 7.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reported {
    location: Location,
    binding: Binding,
}

/// Signals one pane's program sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PaneSignals {
    title: Option<String>,
    reported: Option<Reported>,
    output_at_ms: Option<u64>,
    attention_at_ms: Option<u64>,
    attention: Option<String>,
    last_exit: Option<(u64, u32)>,
}

/// One pane of a tab, as the pane runtime lists it.
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub id: Uuid,
    pub focused: bool,
    pub running_command: Option<String>,
    /// The directory shell integration (or the operating system) last
    /// reported for the pane's shell: trusted, unlike OSC 7.
    pub cwd: Option<String>,
}

/// One tab and its panes.
#[derive(Debug, Clone)]
pub struct TabPanes {
    pub tab_id: Uuid,
    pub panes: Vec<PaneInfo>,
}

/// What a pane's program last signalled, as the pane runtime retains it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetainedSignals {
    pub pane_id: Uuid,
    pub title: Option<String>,
    pub working_directory: Option<String>,
    pub notifications: u64,
    pub last_notification: Option<String>,
    pub bells: u64,
    pub exit_status: Option<u32>,
    pub screen_revision: u64,
}

#[derive(Debug, Default)]
pub struct StatusBoard {
    panes: HashMap<Uuid, PaneSignals>,
    /// Each pane's retained signals as last taken.
    seen: HashMap<Uuid, RetainedSignals>,
    /// Each pane's agent, from the agents plugin.
    agents: HashMap<Uuid, PaneAgent>,
    git: HashMap<String, (Instant, Option<GitStatus>)>,
    published: Option<Vec<TabStatus>>,
    published_bells: Option<Vec<TabBells>>,
    /// What deserves a look in each tab, and whether anyone has looked.
    attention: AttentionBook,
    /// The attention book changed since it was last taken to be saved.
    attention_changed: bool,
    failed_clears: FailedClears,
}

impl StatusBoard {
    /// A board starting from a saved attention book, ending failed states by
    /// `failed_clears`.
    #[must_use]
    pub fn new(attention: AttentionBook, failed_clears: FailedClears) -> Self {
        Self {
            attention,
            failed_clears,
            ..Self::default()
        }
    }

    /// The attention book, when it changed since this was last called.
    pub fn take_changed_attention(&mut self) -> Option<AttentionBook> {
        std::mem::take(&mut self.attention_changed).then(|| self.attention.clone())
    }

    /// Take the agents plugin's latest snapshot.
    pub fn set_agents(&mut self, snapshot: &AgentSnapshot) {
        self.agents = snapshot
            .panes
            .iter()
            .map(|agent| (agent.pane_id, agent.clone()))
            .collect();
    }

    /// The tab's most pressing agent: blocked, then working, then the
    /// focused pane's, then any.
    fn tab_agent(&self, tab: &TabPanes) -> Option<TabAgent> {
        let urgency = |state: AgentState| match state {
            AgentState::Blocked => 2,
            AgentState::Working => 1,
            AgentState::Idle => 0,
        };
        tab.panes
            .iter()
            .filter_map(|pane| Some((pane, self.agents.get(&pane.id)?)))
            .max_by_key(|(pane, agent)| (urgency(agent.state), pane.focused))
            .map(|(_, agent)| TabAgent {
                name: agent.agent.clone(),
                activity: match agent.state {
                    AgentState::Idle => AgentActivity::Idle,
                    AgentState::Working => AgentActivity::Working,
                    AgentState::Blocked => AgentActivity::Blocked,
                },
                message: agent.message.clone(),
                changed_at_ms: agent.changed_at_ms,
                finished_at_ms: agent.finished_at_ms,
            })
    }

    /// Take every pane's retained signals at `now_ms`. What changed since the
    /// board last looked is new: a reported directory, a notification or
    /// bell (attention), a finished command's status, or the screen's
    /// content changing (output). Reading state rather than events, nothing
    /// is missed when events are.
    pub fn apply_signals(&mut self, panes: &[RetainedSignals], now_ms: u64) {
        for retained in panes {
            let seen = self.seen.insert(retained.pane_id, retained.clone());
            // Counters that went back belong to a restarted program.
            let seen = seen.filter(|seen| {
                seen.notifications <= retained.notifications && seen.bells <= retained.bells
            });
            let signals = self.panes.entry(retained.pane_id).or_default();
            signals.title = retained
                .title
                .clone()
                .filter(|title| !title.trim().is_empty());
            if retained.working_directory.is_some()
                && seen
                    .as_ref()
                    .is_none_or(|seen| seen.working_directory != retained.working_directory)
            {
                signals.reported = retained
                    .working_directory
                    .as_deref()
                    .and_then(|location| parse_location(location, local_hostname()))
                    .map(|location| Reported {
                        location,
                        binding: Binding::Pending,
                    });
            }
            if let Some(seen) = &seen {
                if retained.notifications > seen.notifications {
                    signals.attention_at_ms = Some(now_ms);
                    signals.attention.clone_from(&retained.last_notification);
                } else if retained.bells > seen.bells {
                    signals.attention_at_ms = Some(now_ms);
                }
                if retained.screen_revision != seen.screen_revision {
                    signals.output_at_ms = Some(now_ms);
                }
            }
            if seen
                .as_ref()
                .is_none_or(|seen| seen.exit_status != retained.exit_status)
            {
                signals.last_exit = retained.exit_status.map(|status| (now_ms, status));
            }
        }
    }

    /// Build every tab's status at `now_ms`, with its state: tabs in
    /// `looked_at` are shown by a client whose terminal has focus. A pane's
    /// directory is what the running command reported with OSC 7, else what
    /// its shell reported; `git` reads a directory's status and only ever
    /// sees a trusted directory.
    pub fn build(
        &mut self,
        tabs: &[TabPanes],
        looked_at: &std::collections::HashSet<Uuid>,
        now_ms: u64,
        git: &mut dyn FnMut(&str) -> Option<GitStatus>,
    ) -> Vec<TabStatus> {
        let live = tabs
            .iter()
            .flat_map(|tab| tab.panes.iter().map(|pane| pane.id))
            .collect::<std::collections::HashSet<_>>();
        self.panes.retain(|pane, _| live.contains(pane));
        self.seen.retain(|pane, _| live.contains(pane));
        // A tab list without tabs is a catalog not yet read: keep the book.
        if !tabs.is_empty() {
            let live_tabs = tabs.iter().map(|tab| tab.tab_id).collect();
            self.attention_changed |= self.attention.retain(&live_tabs);
        }
        for pane in tabs.iter().flat_map(|tab| &tab.panes) {
            if let Some(signals) = self.panes.get_mut(&pane.id) {
                settle(signals, pane);
            }
        }
        let now = Instant::now();
        tabs.iter()
            .map(|tab| {
                let focused = tab
                    .panes
                    .iter()
                    .find(|pane| pane.focused)
                    .or_else(|| tab.panes.first());
                let signals = focused.and_then(|pane| self.panes.get(&pane.id)).cloned();
                let reported = signals
                    .as_ref()
                    .and_then(|signals| signals.reported.as_ref());
                let trusted = focused.and_then(|pane| pane.cwd.clone());
                let (cwd, git_directory) = match reported.map(|reported| &reported.location) {
                    Some(Location::Local(path)) => (
                        Some(path.clone()),
                        trusted.filter(|trusted| trusted == path),
                    ),
                    Some(remote @ Location::Remote { .. }) => (Some(remote.display()), None),
                    None => (trusted.clone(), trusted),
                };
                let git = git_directory
                    .as_deref()
                    .and_then(|cwd| match self.git.get(cwd) {
                        Some((at, status)) if now.duration_since(*at) < GIT_TTL => status.clone(),
                        _ => {
                            let status = git(cwd);
                            self.git.insert(cwd.to_string(), (now, status.clone()));
                            status
                        }
                    });
                let tab_signals = tab
                    .panes
                    .iter()
                    .filter_map(|pane| self.panes.get(&pane.id))
                    .collect::<Vec<_>>();
                let attention = tab_signals
                    .iter()
                    .filter_map(|signals| {
                        signals
                            .attention_at_ms
                            .map(|at| (at, signals.attention.clone()))
                    })
                    .max_by_key(|(at, _)| *at);
                let agent = self.tab_agent(tab);
                let last_exit = tab_signals
                    .iter()
                    .filter_map(|signals| signals.last_exit)
                    .max_by_key(|(at, _)| *at);
                let output_at_ms = tab_signals
                    .iter()
                    .filter_map(|signals| signals.output_at_ms)
                    .max();
                let running_command = focused.and_then(|pane| pane.running_command.clone());
                let observation = Observation {
                    agent: agent.as_ref().map(|agent| AgentSignal {
                        activity: agent.activity,
                        changed_at_ms: agent.changed_at_ms,
                        finished_at_ms: agent.finished_at_ms,
                    }),
                    running: running_command.is_some(),
                    output_at_ms,
                    attention_at_ms: attention.as_ref().map(|(at, _)| *at),
                    last_exit,
                };
                self.attention_changed |= self.attention.observe(
                    tab.tab_id,
                    &observation,
                    looked_at.contains(&tab.tab_id),
                    now_ms,
                );
                let (state, state_since_ms) =
                    self.attention
                        .state(tab.tab_id, &observation, self.failed_clears, now_ms);
                TabStatus {
                    tab_id: tab.tab_id,
                    panes: u32::try_from(tab.panes.len()).unwrap_or(u32::MAX),
                    running_command,
                    cwd,
                    title: signals.and_then(|signals| signals.title),
                    git,
                    last_exit: last_exit.map(|(_, status)| status),
                    output_at_ms,
                    attention_at_ms: attention.as_ref().map(|(at, _)| *at),
                    attention: attention.and_then(|(_, text)| text),
                    agent,
                    state,
                    state_since_ms,
                }
            })
            .collect()
    }

    /// Each tab's bells, summed over its panes, when they differ from what was
    /// last published; records them when so.
    pub fn changed_bells(&mut self, tabs: &[TabPanes]) -> Option<Vec<TabBells>> {
        let bells = tabs
            .iter()
            .map(|tab| TabBells {
                tab_id: tab.tab_id,
                bells: tab
                    .panes
                    .iter()
                    .filter_map(|pane| self.seen.get(&pane.id))
                    .map(|signals| signals.bells)
                    .sum(),
            })
            .collect::<Vec<_>>();
        if self.published_bells.as_ref() == Some(&bells) {
            return None;
        }
        self.published_bells = Some(bells.clone());
        Some(bells)
    }

    /// Whether `tabs` differs from what was last published; records it when so.
    pub fn changed(&mut self, tabs: &[TabStatus]) -> bool {
        if self.published.as_deref() == Some(tabs) {
            return false;
        }
        self.published = Some(tabs.to_vec());
        true
    }

    #[cfg(test)]
    fn git_cache_len(&self) -> usize {
        self.git.len()
    }
}

/// Keep an OSC 7 directory only while it still describes the pane: bound to
/// the command running when the board first sees it, dropped when that
/// command ends (the shell's own report is authoritative again), and kept for
/// a shell with no other way to report its directory.
fn settle(signals: &mut PaneSignals, pane: &PaneInfo) {
    let Some(reported) = signals.reported.as_mut() else {
        return;
    };
    let keep = match (&reported.binding, &pane.running_command) {
        (Binding::Pending, Some(command)) => {
            reported.binding = Binding::Command(command.clone());
            true
        }
        (Binding::Pending, None) if pane.cwd.is_none() => {
            reported.binding = Binding::Shell;
            true
        }
        (Binding::Command(bound), Some(command)) => bound == command,
        (Binding::Shell, _) => pane.cwd.is_none(),
        (Binding::Pending | Binding::Command(_), None) => false,
    };
    if !keep {
        signals.reported = None;
    }
}

/// The directory an OSC 7 location names: `file://host/path` (percent
/// decoded), kitty's `kitty-shell-cwd://host/path` (verbatim) or a bare
/// absolute path. A host other than this machine (`local_host`, compared by
/// its first label, case-insensitively) makes it remote.
#[must_use]
pub fn parse_location(location: &str, local_host: Option<&str>) -> Option<Location> {
    let (host, path) = if let Some(rest) = location.strip_prefix("file://") {
        let (host, path) = split_authority(rest)?;
        (host, percent_decode(path)?)
    } else if let Some(rest) = location.strip_prefix("kitty-shell-cwd://") {
        let (host, path) = split_authority(rest)?;
        (host, path.to_string())
    } else {
        ("", location.to_string())
    };
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return None;
    }
    if is_local_host(host, local_host) {
        Some(Location::Local(path))
    } else {
        Some(Location::Remote {
            host: host.to_string(),
            path,
        })
    }
}

fn split_authority(rest: &str) -> Option<(&str, &str)> {
    let slash = rest.find('/')?;
    Some((&rest[..slash], &rest[slash..]))
}

fn percent_decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = bytes
            .get(index + 1..index + 3)
            .filter(|hex| bytes[index] == b'%' && hex.iter().all(u8::is_ascii_hexdigit));
        if let Some(hex) = escape {
            let hex = std::str::from_utf8(hex).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn is_local_host(host: &str, local_host: Option<&str>) -> bool {
    let short = |name: &str| {
        name.split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    match host {
        "" | "localhost" | "127.0.0.1" | "::1" | "[::1]" => true,
        // Without a known name, a host cannot be told apart; git is gated on
        // the trusted directory regardless.
        host => local_host.is_none_or(|local| short(local) == short(host)),
    }
}

/// This machine's name, read once.
fn local_hostname() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| {
        let output = Command::new("hostname")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        let name = String::from_utf8(output.stdout).ok()?.trim().to_string();
        (!name.is_empty()).then_some(name)
    })
    .as_deref()
}

/// The git status of `directory`, or `None` outside a repository, when git is
/// unavailable, or when it takes longer than [`GIT_TIMEOUT`].
#[must_use]
pub fn git_status(directory: &str) -> Option<GitStatus> {
    if !Path::new(directory).is_dir() {
        return None;
    }
    // Repository configuration can name commands git runs during `status`
    // (`core.fsmonitor`); the board's polling must never start one.
    let mut child = Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-C",
            directory,
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read while git runs: a status larger than the pipe buffer would
    // otherwise block it until the timeout.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut output = String::new();
        stdout.read_to_string(&mut output).ok().map(|_| output)
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if started.elapsed() > GIT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    parse_porcelain(&reader.join().ok()??)
}

/// Parse `git status --porcelain=v2 --branch`.
#[must_use]
pub fn parse_porcelain(output: &str) -> Option<GitStatus> {
    let mut branch = None;
    let mut status = GitStatus {
        branch: String::new(),
        dirty: false,
        ahead: 0,
        behind: 0,
    };
    let mut oid = None;
    for line in output.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            branch = Some(head.to_string());
        } else if let Some(commit) = line.strip_prefix("# branch.oid ") {
            oid = Some(commit.chars().take(7).collect::<String>());
        } else if let Some(counts) = line.strip_prefix("# branch.ab ") {
            for count in counts.split_whitespace() {
                if let Some(ahead) = count.strip_prefix('+') {
                    status.ahead = ahead.parse().unwrap_or(0);
                } else if let Some(behind) = count.strip_prefix('-') {
                    status.behind = behind.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            status.dirty = true;
        }
    }
    status.branch = match branch? {
        detached if detached == "(detached)" => oid.unwrap_or(detached),
        branch => branch,
    };
    Some(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(tab: u128, panes: &[(u128, bool, Option<&str>)]) -> TabPanes {
        TabPanes {
            tab_id: Uuid::from_u128(tab),
            panes: panes
                .iter()
                .map(|(id, focused, command)| PaneInfo {
                    id: Uuid::from_u128(*id),
                    focused: *focused,
                    running_command: command.map(str::to_string),
                    cwd: match id {
                        11 => Some("/repo".to_string()),
                        13 => Some("/elsewhere".to_string()),
                        _ => None,
                    },
                })
                .collect(),
        }
    }

    /// Each tab's bells are its panes' retained counts summed, published
    /// only when they change.
    #[test]
    fn tabs_count_their_panes_bells() {
        let mut board = StatusBoard::default();
        let tabs = [
            tab(1, &[(11, true, None), (12, false, None)]),
            tab(2, &[(13, true, None)]),
        ];
        let bells = |pane: u128, bells: u64| RetainedSignals {
            pane_id: Uuid::from_u128(pane),
            bells,
            ..RetainedSignals::default()
        };
        board.apply_signals(&[bells(11, 2), bells(12, 3), bells(13, 0)], 0);
        let counts = |published: Option<Vec<TabBells>>| {
            published.map(|tabs| tabs.iter().map(|tab| tab.bells).collect::<Vec<_>>())
        };
        assert_eq!(counts(board.changed_bells(&tabs)), Some(vec![5, 0]));
        assert_eq!(counts(board.changed_bells(&tabs)), None);
        board.apply_signals(&[bells(11, 2), bells(12, 3), bells(13, 1)], 1);
        assert_eq!(counts(board.changed_bells(&tabs)), Some(vec![5, 1]));
    }

    #[test]
    fn locations_name_local_and_remote_directories() {
        let local = |path: &str| Some(Location::Local(path.into()));
        let here = Some("Mac.local");
        assert_eq!(
            parse_location("file://mac/Users/me/my%20repo", here),
            local("/Users/me/my repo")
        );
        assert_eq!(parse_location("file:///tmp", here), local("/tmp"));
        assert_eq!(parse_location("file://localhost/tmp", here), local("/tmp"));
        assert_eq!(parse_location("file://MAC.lan/tmp", here), local("/tmp"));
        assert_eq!(
            parse_location("file://build-box/home/ci", here),
            Some(Location::Remote {
                host: "build-box".into(),
                path: "/home/ci".into(),
            })
        );
        assert_eq!(
            parse_location("kitty-shell-cwd://mac/a b%20", here),
            local("/a b%20")
        );
        assert_eq!(parse_location("/tmp", here), local("/tmp"));
        // Malformed escapes stay literal; a host cannot be judged without a name.
        assert_eq!(parse_location("file:///a%+1%4", here), local("/a%+1%4"));
        assert_eq!(parse_location("file://any/tmp", None), local("/tmp"));
        assert_eq!(parse_location("relative", here), None);
        assert_eq!(parse_location("file://host", here), None);
        assert_eq!(parse_location("file:///a%0Ab", here), None);
    }

    #[test]
    fn tabs_carry_their_most_pressing_agent() {
        let mut board = StatusBoard::default();
        let session = Uuid::from_u128(9);
        let tabs = [tab(1, &[(11, true, Some("claude")), (12, false, None)])];
        // The tab's most pressing agent is folded in.
        let agent = |pane: u128, state| PaneAgent {
            session_id: session,
            pane_id: Uuid::from_u128(pane),
            agent: format!("agent{pane}"),
            state,
            source: bmux_agents_plugin_api::agent_states::AgentSource::Report,
            rule: None,
            message: Some("approve?".into()),
            changed_at_ms: 5,
            finished_at_ms: None,
        };
        board.set_agents(&AgentSnapshot {
            panes: vec![agent(11, AgentState::Idle), agent(12, AgentState::Blocked)],
            revision: 1,
        });
        let with_agents = board.build(
            &tabs,
            &std::collections::HashSet::new(),
            now_ms(),
            &mut |_| None,
        );
        let tab_agent = with_agents[0].agent.as_ref().unwrap();
        assert_eq!(
            (tab_agent.name.as_str(), tab_agent.activity),
            ("agent12", AgentActivity::Blocked)
        );
        assert_eq!(tab_agent.message.as_deref(), Some("approve?"));
        board.set_agents(&AgentSnapshot {
            panes: vec![agent(11, AgentState::Idle), agent(12, AgentState::Idle)],
            revision: 2,
        });
        let focused_agent = board.build(
            &tabs,
            &std::collections::HashSet::new(),
            now_ms(),
            &mut |_| None,
        );
        assert_eq!(focused_agent[0].agent.as_ref().unwrap().name, "agent11");
    }

    #[test]
    fn reported_directories_last_as_long_as_the_command_that_reported_them() {
        let pane = Uuid::from_u128(1);
        let tabs = |command: Option<&str>, cwd: Option<&str>| {
            [TabPanes {
                tab_id: Uuid::from_u128(2),
                panes: vec![PaneInfo {
                    id: pane,
                    focused: true,
                    running_command: command.map(str::to_string),
                    cwd: cwd.map(str::to_string),
                }],
            }]
        };
        let report = |board: &mut StatusBoard, location: &str| {
            board.apply_signals(
                &[RetainedSignals {
                    pane_id: pane,
                    working_directory: Some(location.into()),
                    ..RetainedSignals::default()
                }],
                0,
            );
        };
        let git_reads = std::cell::RefCell::new(Vec::new());
        let build = |board: &mut StatusBoard, command, cwd| {
            let built = board.build(
                &tabs(command, cwd),
                &std::collections::HashSet::new(),
                now_ms(),
                &mut |dir| {
                    git_reads.borrow_mut().push(dir.to_string());
                    None
                },
            );
            built[0].cwd.clone()
        };
        let mut board = StatusBoard::default();
        let repo = Some("/repo");

        // Whatever a program reports is shown while it runs, but git only
        // ever reads the shell's own directory.
        report(&mut board, "file:///tmp/planted");
        assert_eq!(
            build(&mut board, Some("cat notes"), repo).as_deref(),
            Some("/tmp/planted")
        );
        assert_eq!(
            build(&mut board, Some("cat notes"), repo).as_deref(),
            Some("/tmp/planted")
        );
        // When it ends, the shell's directory is authoritative again.
        assert_eq!(build(&mut board, None, repo).as_deref(), Some("/repo"));
        assert_eq!(*git_reads.borrow(), ["/repo"]);

        // A remote shell's directory names its host and is never read.
        report(&mut board, "file://build-box.example/home/ci");
        assert_eq!(
            build(&mut board, Some("ssh build-box"), repo).as_deref(),
            Some("build-box.example:/home/ci")
        );
        // Another command, even before the board saw the shell's prompt, drops it.
        assert_eq!(
            build(&mut board, Some("make"), repo).as_deref(),
            Some("/repo")
        );
        // A report that arrives while the shell sits at its prompt is
        // superseded by the shell's own report.
        report(&mut board, "file:///else");
        assert_eq!(build(&mut board, None, repo).as_deref(), Some("/repo"));
        // A report agreeing with the shell's directory is read.
        report(&mut board, "file:///repo");
        assert_eq!(
            build(&mut board, Some("nvim"), repo).as_deref(),
            Some("/repo")
        );

        // A shell with no other way to report keeps its OSC 7 directory, unread.
        let mut board = StatusBoard::default();
        git_reads.borrow_mut().clear();
        report(&mut board, "file:///srv");
        assert_eq!(build(&mut board, None, None).as_deref(), Some("/srv"));
        assert_eq!(
            build(&mut board, Some("make"), None).as_deref(),
            Some("/srv")
        );
        assert!(git_reads.borrow().is_empty());
    }

    #[test]
    fn porcelain_reports_branch_counts_and_changes() {
        let clean = "# branch.oid 0123456789\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n";
        assert_eq!(
            parse_porcelain(clean),
            Some(GitStatus {
                branch: "main".into(),
                dirty: false,
                ahead: 2,
                behind: 1,
            })
        );
        let dirty = "# branch.oid 0123456789abc\n# branch.head (detached)\n? new-file\n";
        assert_eq!(
            parse_porcelain(dirty),
            Some(GitStatus {
                branch: "0123456".into(),
                dirty: true,
                ahead: 0,
                behind: 0,
            })
        );
        assert_eq!(parse_porcelain(""), None);
    }

    #[test]
    fn tabs_summarize_their_panes_signals() {
        let mut board = StatusBoard::default();
        let focused = Uuid::from_u128(11);
        let other = Uuid::from_u128(12);
        // First look: the focused pane's directory and title; the other pane
        // as it was.
        let focused_signals = RetainedSignals {
            pane_id: focused,
            title: Some("vim".into()),
            working_directory: Some("file:///repo".into()),
            ..RetainedSignals::default()
        };
        let mut other_signals = RetainedSignals {
            pane_id: other,
            ..RetainedSignals::default()
        };
        board.apply_signals(&[focused_signals.clone(), other_signals.clone()], 10);
        // Then the other pane's screen changed, it asked for attention, and
        // its command failed: seen however many events about it were missed.
        other_signals.screen_revision = 7;
        other_signals.notifications = 1;
        other_signals.last_notification = Some("claude: needs input".into());
        other_signals.exit_status = Some(1);
        board.apply_signals(&[focused_signals, other_signals.clone()], 30);
        // Redraws that change nothing are not output.
        board.apply_signals(&[other_signals], 40);
        let mut git_reads = 0;
        let tabs = [tab(1, &[(11, true, Some("nvim")), (12, false, None)])];
        let built = board.build(
            &tabs,
            &std::collections::HashSet::new(),
            now_ms(),
            &mut |cwd| {
                git_reads += 1;
                (cwd == "/repo").then(|| GitStatus {
                    branch: "main".into(),
                    dirty: true,
                    ahead: 0,
                    behind: 0,
                })
            },
        );
        assert_eq!(built.len(), 1);
        let status = &built[0];
        assert_eq!(status.panes, 2);
        assert_eq!(status.running_command.as_deref(), Some("nvim"));
        assert_eq!(status.cwd.as_deref(), Some("/repo"));
        assert_eq!(status.title.as_deref(), Some("vim"));
        assert_eq!(
            status.git.as_ref().map(|git| git.branch.as_str()),
            Some("main")
        );
        assert_eq!(status.last_exit, Some(1));
        assert_eq!(status.output_at_ms, Some(30));
        assert_eq!(status.attention_at_ms, Some(30));
        assert_eq!(status.attention.as_deref(), Some("claude: needs input"));
        assert_eq!(status.agent, None);
        // Git status is cached.
        board.build(
            &tabs,
            &std::collections::HashSet::new(),
            now_ms(),
            &mut |_| unreachable!("cached"),
        );
        assert_eq!(git_reads, 1);
        assert!(board.changed(&built));
        assert!(!board.changed(&built));
        // A pane without OSC 7 uses the shell's directory, and closed panes are forgotten.
        let tabs = [tab(1, &[(13, true, None)])];
        let built = board.build(
            &tabs,
            &std::collections::HashSet::new(),
            now_ms(),
            &mut |_| None,
        );
        assert_eq!(built[0].cwd.as_deref(), Some("/elsewhere"));
        assert_eq!(board.git_cache_len(), 2);
        assert!(board.panes.is_empty());
    }
}
