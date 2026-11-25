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

use crate::attention::{AgentSignal, AttentionBook, Exit, FailedClears, Loaded, Observation};
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
    /// The saved book has been read (or there was none).
    attention_loaded: bool,
    /// The saved book may be overwritten: not one in a newer format.
    attention_writable: bool,
    /// The book as last written, and when: a write happens only when the
    /// book's bytes differ, at most once per [`SAVE_INTERVAL_MS`].
    saved_bytes: Option<Vec<u8>>,
    saved_at_ms: Option<u64>,
    /// When each recorded tab was first found missing from the tab list.
    missing_since: HashMap<Uuid, u64>,
}

/// How long a tab may be missing from the tab list before what it recorded is
/// forgotten: tabs come back one by one after a restart.
const FORGET_MISSING_TAB_MS: u64 = 60_000;
/// How often the attention book is written at most: a pane ringing its bell
/// over and over changes it every refresh.
pub const SAVE_INTERVAL_MS: u64 = 5_000;

impl StatusBoard {
    /// Record what is new in a tab, mark it seen when looked at, and decide
    /// its state.
    fn tab_state(
        &mut self,
        tab: Uuid,
        observation: &Observation,
        looked_at: bool,
        now_ms: u64,
    ) -> (bmux_tabs_plugin_api::tabs_status::TabState, Option<u64>) {
        self.attention_changed |= self.attention.observe(tab, observation, looked_at, now_ms);
        self.attention
            .state(tab, observation, self.failed_clears, now_ms)
    }

    /// Forget what tabs recorded once they have been missing from the tab
    /// list for [`FORGET_MISSING_TAB_MS`].
    fn forget_missing_tabs(&mut self, tabs: &[TabPanes], now_ms: u64) {
        let live = |tab: &Uuid| tabs.iter().any(|live| live.tab_id == *tab);
        let missing_since = &mut self.missing_since;
        missing_since.retain(|tab, _| !live(tab));
        for tab in self.attention.tabs.keys().filter(|tab| !live(tab)) {
            missing_since.entry(*tab).or_insert(now_ms);
        }
        if self.attention.retain(|tab| {
            missing_since
                .get(tab)
                .is_none_or(|since| now_ms.saturating_sub(*since) < FORGET_MISSING_TAB_MS)
        }) {
            self.attention_changed = true;
            let recorded = &self.attention.tabs;
            missing_since.retain(|tab, _| recorded.contains_key(tab));
        }
    }

    /// Whether the saved attention book still needs reading.
    #[must_use]
    pub const fn attention_unloaded(&self) -> bool {
        !self.attention_loaded
    }

    /// End failed states by `failed_clears` (the plugin's setting).
    pub const fn set_failed_clears(&mut self, failed_clears: FailedClears) {
        self.failed_clears = failed_clears;
    }

    /// Start from what was saved. A book in a newer format is left alone; a
    /// damaged one is replaced by a fresh one.
    pub fn load_attention(&mut self, saved: Loaded) {
        self.attention_loaded = true;
        match saved {
            Loaded::Book(mut book) => {
                book.mark_restored();
                self.attention = book;
                self.attention_writable = true;
            }
            Loaded::Nothing | Loaded::Corrupt(_) => {
                self.attention_writable = true;
                // Replace a damaged save even before anything changes.
                self.attention_changed = true;
            }
            Loaded::Unsupported => self.attention_writable = false,
        }
    }

    /// The attention book's bytes to write at `now_ms`: when they differ
    /// from what was last written, the book may be written, and the last
    /// write was [`SAVE_INTERVAL_MS`] ago or more, or `flush` asks for it
    /// now. Report the outcome with [`StatusBoard::attention_written`].
    pub fn attention_to_save(&mut self, now_ms: u64, flush: bool) -> Option<Vec<u8>> {
        if !self.attention_changed || !self.attention_writable {
            return None;
        }
        if !flush
            && self
                .saved_at_ms
                .is_some_and(|at| now_ms.saturating_sub(at) < SAVE_INTERVAL_MS)
        {
            return None;
        }
        self.attention_changed = false;
        let bytes = match self.attention.to_bytes() {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, "tabs attention not encoded");
                return None;
            }
        };
        if self.saved_bytes.as_deref() == Some(bytes.as_slice()) {
            return None;
        }
        self.saved_at_ms = Some(now_ms);
        Some(bytes)
    }

    /// Record whether `bytes`, from [`StatusBoard::attention_to_save`], were
    /// written; a failed write is retried.
    pub fn attention_written(&mut self, bytes: Vec<u8>, written: bool) {
        if written {
            self.saved_bytes = Some(bytes);
        } else {
            self.attention_changed = true;
        }
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

    /// The tab's latest exit, judged by its own pane: a command running in
    /// another pane does not end a failure.
    fn latest_exit(&self, tab: &TabPanes) -> Option<Exit> {
        tab.panes
            .iter()
            .filter_map(|pane| {
                let (at_ms, status) = self.panes.get(&pane.id)?.last_exit?;
                Some(Exit {
                    at_ms,
                    status,
                    rerun: pane.running_command.is_some(),
                })
            })
            .max_by_key(|exit| exit.at_ms)
    }

    fn shell_command(&self, tab: &TabPanes) -> Option<String> {
        let mut first = None;
        for pane in &tab.panes {
            if let Some(command) = pane.running_command.as_deref()
                && !command.trim().is_empty()
                && !self.agents.contains_key(&pane.id)
            {
                if pane.focused {
                    return Some(command.to_owned());
                }
                first.get_or_insert(command);
            }
        }
        first.map(str::to_owned)
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
        self.forget_missing_tabs(tabs, now_ms);
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
                let last_exit = self.latest_exit(tab);
                let output_at_ms = tab_signals
                    .iter()
                    .filter_map(|signals| signals.output_at_ms)
                    .max();
                let running_command = focused.and_then(|pane| pane.running_command.clone());
                let (state, state_since_ms) = self.tab_state(
                    tab.tab_id,
                    &Observation {
                        agent: agent.as_ref().map(|agent| AgentSignal {
                            activity: agent.activity,
                            changed_at_ms: agent.changed_at_ms,
                            finished_at_ms: agent.finished_at_ms,
                        }),
                        running: running_command.is_some(),
                        output_at_ms,
                        attention_at_ms: attention.as_ref().map(|(at, _)| *at),
                        last_exit,
                    },
                    looked_at.contains(&tab.tab_id),
                    now_ms,
                );
                TabStatus {
                    tab_id: tab.tab_id,
                    panes: u32::try_from(tab.panes.len()).unwrap_or(u32::MAX),
                    running_command,
                    shell_command: self.shell_command(tab),
                    cwd,
                    title: signals.and_then(|signals| signals.title),
                    git,
                    last_exit: last_exit.map(|exit| exit.status),
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

    /// A tab whose agent finished while nobody looked is published done
    /// until a focused client shows it. What it recorded is saved and
    /// survives a restart, where the agent comes back with no finish time
    /// (the agents plugin keeps it only in memory), and the tab briefly
    /// missing from the list.
    #[test]
    fn a_finished_agent_is_done_until_looked_at_and_that_is_saved() {
        use bmux_tabs_plugin_api::tabs_status::TabState;
        let tabs = [tab(1, &[(11, true, Some("claude"))])];
        let tab_id = Uuid::from_u128(1);
        let agent = |changed_at_ms: u64, finished_at_ms: Option<u64>| AgentSnapshot {
            panes: vec![PaneAgent {
                session_id: Uuid::from_u128(9),
                pane_id: Uuid::from_u128(11),
                agent: "claude".into(),
                state: AgentState::Idle,
                source: bmux_agents_plugin_api::agent_states::AgentSource::Report,
                rule: None,
                message: None,
                changed_at_ms,
                finished_at_ms,
            }],
            revision: 1,
        };
        let nobody = std::collections::HashSet::new();
        let looking = std::collections::HashSet::from([tab_id]);
        let mut board = StatusBoard::default();
        board.load_attention(Loaded::Nothing);
        board.set_agents(&agent(10_000, None));
        let state = |built: Vec<TabStatus>| (built[0].state, built[0].state_since_ms);
        assert_eq!(
            state(board.build(&tabs, &nobody, 10_000, &mut |_| None)),
            (TabState::Idle, None),
            "an idle agent with no observed work"
        );
        board.set_agents(&agent(10_000, Some(10_000)));
        assert_eq!(
            state(board.build(&tabs, &nobody, 12_000, &mut |_| None)),
            (TabState::Done, Some(10_000))
        );
        let saved = board
            .attention_to_save(12_000, false)
            .expect("the finish is saved");
        board.attention_written(saved.clone(), true);
        assert_eq!(board.attention_to_save(30_000, false), None, "once");

        // The server restarts: the agents plugin knows the agent again, but
        // not when it finished. The saved finish is still done.
        let restart = || {
            let mut restarted = StatusBoard::default();
            restarted.load_attention(Loaded::from_bytes(Some(&saved)));
            restarted.set_agents(&agent(20_000, None));
            restarted
        };
        let mut restarted = restart();
        // Missing from the list for a while keeps it.
        restarted.build(&[], &nobody, 20_000, &mut |_| None);
        restarted.build(&[tab(2, &[(12, true, None)])], &nobody, 40_000, &mut |_| {
            None
        });
        assert_eq!(
            state(restarted.build(&tabs, &nobody, 41_000, &mut |_| None)),
            (TabState::Done, Some(10_000))
        );
        assert_eq!(
            state(restarted.build(&tabs, &looking, 42_000, &mut |_| None)),
            (TabState::Idle, None),
            "looked at"
        );
        let seen = restarted.attention_to_save(42_000, false);
        assert!(seen.is_some(), "the seen mark is saved");

        // Working again after the restart supersedes the saved finish.
        let mut relaunched = restart();
        let mut working = agent(43_000, None);
        working.panes[0].state = AgentState::Working;
        relaunched.set_agents(&working);
        assert_eq!(
            state(relaunched.build(&tabs, &nobody, 43_000, &mut |_| None)).0,
            TabState::Working
        );
        relaunched.set_agents(&agent(44_000, Some(44_000)));
        assert_eq!(
            state(relaunched.build(&tabs, &nobody, 44_000, &mut |_| None)),
            (TabState::Stopped, Some(44_000)),
            "the old finish is gone; the new one settles first"
        );

        // Missing for long enough, it is forgotten.
        let other = [tab(2, &[(12, true, None)])];
        restarted.build(&other, &nobody, 50_000, &mut |_| None);
        restarted.build(&other, &nobody, 50_000 + FORGET_MISSING_TAB_MS, &mut |_| {
            None
        });
        assert!(!restarted.attention.tabs.contains_key(&tab_id));

        // A book in a newer format is neither used nor overwritten.
        let mut future = StatusBoard::default();
        future.load_attention(Loaded::Unsupported);
        future.set_agents(&agent(10_000, Some(10_000)));
        future.build(&tabs, &nobody, 12_000, &mut |_| None);
        assert_eq!(future.attention_to_save(12_000, true), None);

        // A damaged one is replaced, even before anything changes.
        let mut damaged = StatusBoard::default();
        damaged.load_attention(Loaded::from_bytes(Some(&saved[..saved.len() / 2])));
        assert!(damaged.attention_to_save(0, false).is_some());
    }

    /// A command that fails in a split is failed while the focused pane runs
    /// an editor, until a command runs in the split again.
    #[test]
    fn a_failure_in_a_split_shows_while_the_focused_pane_runs() {
        use bmux_tabs_plugin_api::tabs_status::TabState;
        let nobody = std::collections::HashSet::new();
        let mut board = StatusBoard::default();
        let exit = |status: Option<u32>| RetainedSignals {
            pane_id: Uuid::from_u128(12),
            exit_status: status,
            ..RetainedSignals::default()
        };
        board.apply_signals(&[exit(None)], 1_000);
        board.apply_signals(&[exit(Some(101))], 2_000);
        let split = |command: Option<&'static str>| {
            [tab(1, &[(11, true, Some("nvim")), (12, false, command)])]
        };
        assert_eq!(
            board.build(&split(None), &nobody, 2_000, &mut |_| None)[0].state,
            TabState::Failed
        );
        assert_eq!(
            board.build(&split(Some("cargo test")), &nobody, 3_000, &mut |_| None)[0].state,
            TabState::Idle
        );
    }

    /// Changes are written at most once per [`SAVE_INTERVAL_MS`], only when
    /// the saved bytes differ, and a flush or failed write loses nothing.
    #[test]
    fn the_attention_book_is_written_sparingly() {
        let tabs = [tab(1, &[(11, true, None)])];
        let nobody = std::collections::HashSet::new();
        let mut board = StatusBoard::default();
        board.load_attention(Loaded::Nothing);
        let bell = |board: &mut StatusBoard, bells: u64, now: u64| {
            board.apply_signals(
                &[RetainedSignals {
                    pane_id: Uuid::from_u128(11),
                    bells,
                    ..RetainedSignals::default()
                }],
                now,
            );
            board.build(&tabs, &nobody, now, &mut |_| None);
        };
        bell(&mut board, 0, 0);
        let first = board.attention_to_save(0, false).expect("written");
        board.attention_written(first, true);
        bell(&mut board, 1, 1_000);
        bell(&mut board, 2, 2_000);
        assert_eq!(board.attention_to_save(2_000, false), None, "too soon");
        let flushed = board.attention_to_save(2_000, true).expect("flushed");
        board.attention_written(flushed, false);
        assert_eq!(board.attention_to_save(3_000, false), None, "too soon");
        let retried = board
            .attention_to_save(2_000 + SAVE_INTERVAL_MS, false)
            .expect("a failed write is retried");
        board.attention_written(retried, true);
        // Nothing new: nothing written, however long it has been.
        bell(&mut board, 2, 20_000);
        assert_eq!(board.attention_to_save(20_000, false), None);
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
    fn quiet_foreground_jobs_are_reported_separately_from_agents() {
        use bmux_agents_plugin_api::agent_states::AgentSource;

        let mut board = StatusBoard::default();
        board.set_agents(&AgentSnapshot {
            panes: vec![PaneAgent {
                session_id: Uuid::from_u128(9),
                pane_id: Uuid::from_u128(11),
                agent: "claude".into(),
                state: AgentState::Working,
                source: AgentSource::Report,
                rule: None,
                message: None,
                changed_at_ms: 1,
                finished_at_ms: None,
            }],
            revision: 1,
        });
        let tabs = [
            tab(
                1,
                &[(11, true, Some("claude")), (12, false, Some("sleep 30"))],
            ),
            tab(
                2,
                &[(21, false, Some("nvim")), (22, true, Some("cargo build"))],
            ),
            tab(3, &[(31, true, None)]),
        ];
        let looked_at = std::collections::HashSet::new();
        let status = board.build(&tabs, &looked_at, 30_000, &mut |_| None);
        assert_eq!(status[0].running_command.as_deref(), Some("claude"));
        assert_eq!(status[0].shell_command.as_deref(), Some("sleep 30"));
        assert_eq!(
            status[0].agent.as_ref().unwrap().activity,
            AgentActivity::Working
        );
        assert_eq!(status[1].shell_command.as_deref(), Some("cargo build"));
        assert_eq!(status[2].shell_command, None);

        let tabs = [tab(1, &[(11, true, Some("claude")), (12, false, None)])];
        let status = board.build(&tabs, &looked_at, 31_000, &mut |_| None);
        assert_eq!(status[0].shell_command, None);
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
