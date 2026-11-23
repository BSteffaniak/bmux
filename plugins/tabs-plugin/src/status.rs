//! What each tab is doing, published on the `tabs-status` state channel.
//!
//! Pane signals (title, OSC 7 directory, bell, notifications, OSC 133 exit
//! status, output) arrive as pane-runtime events and are kept per pane. Once a
//! second the board turns them, each tab's panes and running commands, and the
//! git status of the focused pane's directory into one snapshot, published
//! only when it changed.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent;
use bmux_pane_runtime_plugin_api::pane_runtime_terminal_events::{PromptMarkKind, TerminalEvent};
use bmux_tabs_plugin_api::tabs_status::{GitStatus, TabStatus};
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

/// Signals one pane's program sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PaneSignals {
    title: Option<String>,
    cwd: Option<String>,
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
    /// The directory shell integration last reported.
    pub cwd: Option<String>,
}

/// One tab and its panes.
#[derive(Debug, Clone)]
pub struct TabPanes {
    pub tab_id: Uuid,
    pub panes: Vec<PaneInfo>,
}

#[derive(Debug, Default)]
pub struct StatusBoard {
    panes: HashMap<Uuid, PaneSignals>,
    git: HashMap<String, (Instant, Option<GitStatus>)>,
    published: Option<Vec<TabStatus>>,
}

impl StatusBoard {
    /// Record a terminal signal.
    pub fn apply_terminal(&mut self, event: &TerminalEvent, now_ms: u64) {
        match event {
            TerminalEvent::Title { pane_id, title, .. } => {
                self.panes.entry(*pane_id).or_default().title =
                    (!title.trim().is_empty()).then(|| title.clone());
            }
            TerminalEvent::WorkingDirectory {
                pane_id, location, ..
            } => {
                self.panes.entry(*pane_id).or_default().cwd = directory_from_location(location);
            }
            TerminalEvent::Notification {
                pane_id,
                title,
                body,
                ..
            } => {
                let signals = self.panes.entry(*pane_id).or_default();
                signals.attention_at_ms = Some(now_ms);
                signals.attention = Some(match title {
                    Some(title) if !title.is_empty() => format!("{title}: {body}"),
                    _ => body.clone(),
                });
            }
            TerminalEvent::Bell { pane_id, .. } => {
                self.panes.entry(*pane_id).or_default().attention_at_ms = Some(now_ms);
            }
            TerminalEvent::PromptMark {
                pane_id,
                kind,
                exit_status,
                ..
            } => {
                let signals = self.panes.entry(*pane_id).or_default();
                match kind {
                    PromptMarkKind::CommandFinished => {
                        if let Some(status) = exit_status {
                            signals.last_exit = Some((now_ms, *status));
                        }
                    }
                    // A new command clears the previous one's exit status.
                    PromptMarkKind::CommandExecuted => signals.last_exit = None,
                    PromptMarkKind::PromptStart | PromptMarkKind::CommandStart => {}
                }
            }
        }
    }

    /// Record a pane lifecycle or output event.
    pub fn apply_pane(&mut self, event: &PaneEvent, now_ms: u64) {
        match event {
            PaneEvent::OutputAvailable { pane_id, .. } => {
                self.panes.entry(*pane_id).or_default().output_at_ms = Some(now_ms);
            }
            PaneEvent::Restarted { pane_id, .. } => {
                self.panes.remove(pane_id);
            }
            _ => {}
        }
    }

    /// Build every tab's status. A pane's directory is what its program last
    /// reported with OSC 7, else what shell integration last reported; `git`
    /// reads a directory's status.
    pub fn build(
        &mut self,
        tabs: &[TabPanes],
        git: &mut dyn FnMut(&str) -> Option<GitStatus>,
    ) -> Vec<TabStatus> {
        let live = tabs
            .iter()
            .flat_map(|tab| tab.panes.iter().map(|pane| pane.id))
            .collect::<std::collections::HashSet<_>>();
        self.panes.retain(|pane, _| live.contains(pane));
        let now = Instant::now();
        tabs.iter()
            .map(|tab| {
                let focused = tab
                    .panes
                    .iter()
                    .find(|pane| pane.focused)
                    .or_else(|| tab.panes.first());
                let signals = focused.and_then(|pane| self.panes.get(&pane.id)).cloned();
                let cwd = focused.and_then(|pane| {
                    signals
                        .as_ref()
                        .and_then(|signals| signals.cwd.clone())
                        .or_else(|| pane.cwd.clone())
                });
                let git = cwd.as_deref().and_then(|cwd| match self.git.get(cwd) {
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
                TabStatus {
                    tab_id: tab.tab_id,
                    panes: u32::try_from(tab.panes.len()).unwrap_or(u32::MAX),
                    running_command: focused.and_then(|pane| pane.running_command.clone()),
                    cwd,
                    title: signals.and_then(|signals| signals.title),
                    git,
                    last_exit: tab_signals
                        .iter()
                        .filter_map(|signals| signals.last_exit)
                        .max_by_key(|(at, _)| *at)
                        .map(|(_, status)| status),
                    output_at_ms: tab_signals
                        .iter()
                        .filter_map(|signals| signals.output_at_ms)
                        .max(),
                    attention_at_ms: attention.as_ref().map(|(at, _)| *at),
                    attention: attention.and_then(|(_, text)| text),
                }
            })
            .collect()
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

/// The directory an OSC 7 location names: `file://host/path` (percent
/// decoded) or a bare absolute path.
#[must_use]
pub fn directory_from_location(location: &str) -> Option<String> {
    let path = location.strip_prefix("file://").map_or(location, |rest| {
        rest.find('/').map_or("", |slash| &rest[slash..])
    });
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The git status of `directory`, or `None` outside a repository, when git is
/// unavailable, or when it takes longer than [`GIT_TIMEOUT`].
#[must_use]
pub fn git_status(directory: &str) -> Option<GitStatus> {
    if !Path::new(directory).is_dir() {
        return None;
    }
    let mut child = Command::new("git")
        .args([
            "-C",
            directory,
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
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
    let mut output = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut()?, &mut output).ok()?;
    parse_porcelain(&output)
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
                    cwd: (*id == 13).then(|| "/elsewhere".to_string()),
                })
                .collect(),
        }
    }

    #[test]
    fn locations_decode_to_directories() {
        assert_eq!(
            directory_from_location("file://host/Users/me/my%20repo").as_deref(),
            Some("/Users/me/my repo")
        );
        assert_eq!(directory_from_location("/tmp").as_deref(), Some("/tmp"));
        assert_eq!(directory_from_location("relative"), None);
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
        let session = Uuid::from_u128(9);
        let focused = Uuid::from_u128(11);
        let other = Uuid::from_u128(12);
        board.apply_terminal(
            &TerminalEvent::WorkingDirectory {
                session_id: session,
                pane_id: focused,
                location: "file://h/repo".into(),
            },
            10,
        );
        board.apply_terminal(
            &TerminalEvent::Title {
                session_id: session,
                pane_id: focused,
                title: "vim".into(),
            },
            11,
        );
        board.apply_pane(
            &PaneEvent::OutputAvailable {
                session_id: session,
                pane_id: other,
            },
            20,
        );
        board.apply_terminal(
            &TerminalEvent::Notification {
                session_id: session,
                pane_id: other,
                title: Some("claude".into()),
                body: "needs input".into(),
            },
            30,
        );
        board.apply_terminal(
            &TerminalEvent::PromptMark {
                session_id: session,
                pane_id: other,
                kind: PromptMarkKind::CommandFinished,
                line: 0,
                exit_status: Some(1),
            },
            40,
        );
        let mut git_reads = 0;
        let tabs = [tab(1, &[(11, true, Some("nvim")), (12, false, None)])];
        let built = board.build(&tabs, &mut |cwd| {
            git_reads += 1;
            (cwd == "/repo").then(|| GitStatus {
                branch: "main".into(),
                dirty: true,
                ahead: 0,
                behind: 0,
            })
        });
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
        assert_eq!(status.output_at_ms, Some(20));
        assert_eq!(status.attention_at_ms, Some(30));
        assert_eq!(status.attention.as_deref(), Some("claude: needs input"));
        // Git status is cached.
        board.build(&tabs, &mut |_| unreachable!("cached"));
        assert_eq!(git_reads, 1);
        assert!(board.changed(&built));
        assert!(!board.changed(&built));
        // A pane without OSC 7 uses the shell's directory, and closed panes are forgotten.
        let tabs = [tab(1, &[(13, true, None)])];
        let built = board.build(&tabs, &mut |_| None);
        assert_eq!(built[0].cwd.as_deref(), Some("/elsewhere"));
        assert_eq!(board.git_cache_len(), 2);
        assert!(board.panes.is_empty());
    }
}
