//! Playbook data types: actions, configuration, and result structures.
//!
//! These types are the internal representation shared between the DSL parser,
//! TOML parser, and execution engine. See `docs/playbooks.md` for the full
//! user-facing reference.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;
use uuid::Uuid;

/// Top-level playbook definition parsed from either TOML or line DSL.
#[derive(Debug, Clone)]
pub struct Playbook {
    pub config: PlaybookConfig,
    pub steps: Vec<Step>,
}

/// Playbook-wide configuration.
#[derive(Debug, Clone)]
pub struct PlaybookConfig {
    pub name: Option<String>,
    pub description: Option<String>,
    pub viewport: Viewport,
    pub shell: Option<String>,
    pub timeout: Duration,
    pub record: bool,
    pub plugins: PluginConfig,
    /// User-defined variables for substitution.
    pub vars: BTreeMap<String, String>,
    /// Environment variables to set in the sandbox server process.
    /// In `Inherit` mode these are overlaid on the parent environment.
    /// In `Clean` mode the sandbox starts empty and only has these plus
    /// deterministic defaults.
    pub env: BTreeMap<String, String>,
    /// Controls how the sandbox inherits the parent process environment.
    /// `None` means the playbook did not explicitly specify a mode.
    pub env_mode: Option<SandboxEnvMode>,
    /// Path to the bmux binary for spawning sandbox servers.
    /// `None` falls back to `std::env::current_exe()`.
    pub binary: Option<std::path::PathBuf>,
    /// Pre-computed bundled plugin IDs for sandbox plugin configuration.
    /// Populated by the CLI runtime; empty when not available.
    pub bundled_plugin_ids: Vec<String>,
    /// Print step-by-step progress to stderr during execution.
    pub verbose: bool,
}

/// Controls how the sandbox server inherits environment variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxEnvMode {
    /// Inherit the full parent environment, then overlay deterministic defaults
    /// (`TERM`, `LANG`, `LC_ALL`, `HOME`) and any explicit `@env` overrides.
    /// This is backward-compatible and the default.
    Inherit,
    /// Start from an empty environment. Only `PATH`, `USER`, and `SHELL` are
    /// inherited from the parent; everything else uses deterministic defaults
    /// or explicit `@env` overrides. Maximally deterministic.
    Clean,
}

impl PlaybookConfig {
    /// Resolve the effective sandbox environment mode.
    ///
    /// Priority: playbook config (if explicitly set) → `BMUX_PLAYBOOK_ENV_MODE`
    /// env var (if set) → `Inherit`.
    pub fn effective_env_mode(&self) -> SandboxEnvMode {
        if let Some(mode) = self.env_mode {
            return mode;
        }
        match std::env::var("BMUX_PLAYBOOK_ENV_MODE").ok().as_deref() {
            Some("clean") => SandboxEnvMode::Clean,
            Some("inherit") => SandboxEnvMode::Inherit,
            _ => SandboxEnvMode::Inherit,
        }
    }
}

impl Default for PlaybookConfig {
    fn default() -> Self {
        Self {
            name: None,
            description: None,
            viewport: Viewport::default(),
            shell: None,
            timeout: Duration::from_secs(30),
            record: false,
            plugins: PluginConfig::default(),
            vars: BTreeMap::new(),
            env: BTreeMap::new(),
            env_mode: None,
            binary: None,
            bundled_plugin_ids: Vec::new(),
            verbose: false,
        }
    }
}

/// Terminal viewport dimensions.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub cols: u16,
    pub rows: u16,
}

impl Default for Viewport {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// Plugin loading configuration for the ephemeral server.
#[derive(Debug, Clone, Default)]
pub struct PluginConfig {
    pub enable: Vec<String>,
    pub disable: Vec<String>,
}

/// A single step in the playbook.
#[derive(Debug, Clone)]
pub struct Step {
    pub index: usize,
    pub action: Action,
    /// If true, the playbook continues executing even if this step fails.
    pub continue_on_error: bool,
}

impl Step {
    /// Serialize the step to a DSL line, including the `!continue` suffix if set.
    pub fn to_dsl(&self) -> String {
        let line = self.action.to_dsl();
        if self.continue_on_error {
            format!("{line} !continue")
        } else {
            line
        }
    }
}

/// All supported playbook actions.
#[derive(Debug, Clone)]
pub enum Action {
    /// Create a new session.
    NewSession { name: Option<String> },
    /// Kill a session by name.
    KillSession { name: String },
    /// Split the focused (or target) pane.
    SplitPane {
        direction: SplitDirection,
        #[allow(dead_code)]
        ratio: Option<f64>,
    },
    /// Focus a pane by index.
    FocusPane { target: u32 },
    /// Close a pane (focused if no target given).
    ClosePane { target: Option<u32> },
    /// Send keystrokes to the focused pane (C-style escapes supported).
    SendKeys { keys: Vec<u8>, pane: Option<u32> },
    /// Send raw bytes (hex-encoded) to the focused pane.
    SendBytes { hex: Vec<u8> },
    /// Wait until pane screen content matches a regex pattern.
    WaitFor {
        pattern: String,
        pane: Option<u32>,
        timeout: Duration,
        /// Number of retry attempts (default 1 = no retry). Each attempt
        /// re-drains output and re-polls from scratch.
        retry: u32,
    },
    /// Hard pause.
    Sleep { duration: Duration },
    /// Capture and label full screen state at this point.
    Snapshot { id: String },
    /// Assert pane screen text.
    AssertScreen {
        pane: Option<u32>,
        contains: Option<String>,
        not_contains: Option<String>,
        matches: Option<String>,
    },
    /// Assert layout structure.
    AssertLayout { pane_count: u32 },
    /// Assert cursor position in a pane.
    AssertCursor {
        pane: Option<u32>,
        row: u16,
        col: u16,
    },
    /// Change the terminal viewport size mid-playbook.
    ResizeViewport { cols: u16, rows: u16 },
    /// Send the prefix key combo + a key character.
    PrefixKey { key: char },
    /// Wait for a server event.
    WaitForEvent { event: String, timeout: Duration },
    /// Invoke a plugin service.
    InvokeService {
        capability: String,
        kind: ServiceKind,
        interface_id: String,
        operation: String,
        payload: String,
    },
    /// Capture the current screen state of all panes. In batch mode, the result
    /// is included in the `PlaybookResult`'s step details as serialized JSON.
    /// This is useful for LLM debugging: inspect screen state without asserting.
    Screen,
    /// Query the current session status (session ID, pane count, focused pane).
    /// In batch mode, the result is included in the step details.
    Status,
}

/// Plugin service invocation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    Query,
    Command,
}

/// Pane split direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Vertical,
    Horizontal,
}

/// Result of a full playbook run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookResult {
    pub playbook_name: Option<String>,
    pub pass: bool,
    pub steps: Vec<StepResult>,
    pub snapshots: Vec<SnapshotCapture>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_path: Option<String>,
    pub total_elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Sandbox temp directory path. Retained on failure for inspection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_root: Option<String>,
}

/// Result of a single step execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub index: usize,
    pub action: String,
    pub status: StepStatus,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The expected value/pattern for assertion failures (assert-screen, wait-for, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// The actual value/screen text found at the time of failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    /// Screen capture of all panes at the time of failure.
    /// Only populated when `status == Fail` and the session was attached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_captures: Option<Vec<PaneCapture>>,
    /// Whether this step had `continue_on_error` set. If true and the step
    /// failed, execution continued past this step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub continue_on_error: bool,
}

/// Step execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pass,
    Fail,
    Skip,
}

/// Structured step failure with optional expected/actual values.
///
/// Used internally by `execute_step` to return rich failure context that gets
/// propagated into `StepResult` fields.
#[derive(Debug)]
pub struct StepFailure {
    /// Human-readable error message.
    pub message: String,
    /// The expected value/pattern (for assertion failures).
    pub expected: Option<String>,
    /// The actual value/screen text found.
    pub actual: Option<String>,
}

impl StepFailure {
    /// Create a failure with only a message (no expected/actual).
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            expected: None,
            actual: None,
        }
    }

    /// Create an assertion failure with expected and actual values.
    pub fn assertion(
        message: impl Into<String>,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            expected: Some(expected.into()),
            actual: Some(actual.into()),
        }
    }
}

impl std::fmt::Display for StepFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for StepFailure {}

/// A named snapshot of screen state captured mid-playbook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotCapture {
    pub id: String,
    pub panes: Vec<PaneCapture>,
}

/// Captured state of a single pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneCapture {
    pub index: u32,
    pub focused: bool,
    pub screen_text: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

impl Action {
    /// Return the action name for display / reporting.
    pub fn name(&self) -> &'static str {
        match self {
            Self::NewSession { .. } => "new-session",
            Self::KillSession { .. } => "kill-session",
            Self::SplitPane { .. } => "split-pane",
            Self::FocusPane { .. } => "focus-pane",
            Self::ClosePane { .. } => "close-pane",
            Self::SendKeys { .. } => "send-keys",
            Self::SendBytes { .. } => "send-bytes",
            Self::WaitFor { .. } => "wait-for",
            Self::Sleep { .. } => "sleep",
            Self::Snapshot { .. } => "snapshot",
            Self::AssertScreen { .. } => "assert-screen",
            Self::AssertLayout { .. } => "assert-layout",
            Self::AssertCursor { .. } => "assert-cursor",
            Self::ResizeViewport { .. } => "resize-viewport",
            Self::PrefixKey { .. } => "prefix-key",
            Self::WaitForEvent { .. } => "wait-for-event",
            Self::InvokeService { .. } => "invoke-service",
            Self::Screen => "screen",
            Self::Status => "status",
        }
    }

    /// Serialize the action back to a DSL line for round-trip display.
    pub fn to_dsl(&self) -> String {
        use super::from_recording::{bytes_to_c_escaped, escape_single_quote};

        match self {
            Self::NewSession { name } => match name {
                Some(n) => format!("new-session name='{}'", escape_single_quote(n)),
                None => "new-session".to_string(),
            },
            Self::KillSession { name } => {
                format!("kill-session name='{}'", escape_single_quote(name))
            }
            Self::SplitPane { direction, ratio } => {
                let dir = match direction {
                    SplitDirection::Vertical => "vertical",
                    SplitDirection::Horizontal => "horizontal",
                };
                match ratio {
                    Some(r) => format!("split-pane direction={dir} ratio={r}"),
                    None => format!("split-pane direction={dir}"),
                }
            }
            Self::FocusPane { target } => format!("focus-pane target={target}"),
            Self::ClosePane { target } => match target {
                Some(t) => format!("close-pane target={t}"),
                None => "close-pane".to_string(),
            },
            Self::SendKeys { keys, pane } => {
                let escaped = bytes_to_c_escaped(keys);
                match pane {
                    Some(p) => format!("send-keys keys='{escaped}' pane={p}"),
                    None => format!("send-keys keys='{escaped}'"),
                }
            }
            Self::SendBytes { hex } => {
                let hex_str: String = hex.iter().map(|b| format!("{b:02x}")).collect();
                format!("send-bytes hex={hex_str}")
            }
            Self::WaitFor {
                pattern,
                pane,
                timeout,
                retry,
            } => {
                let escaped = escape_single_quote(pattern);
                let mut line = format!("wait-for pattern='{escaped}'");
                if let Some(p) = pane {
                    line.push_str(&format!(" pane={p}"));
                }
                let ms = timeout.as_millis();
                if ms != 5000 {
                    line.push_str(&format!(" timeout={ms}"));
                }
                if *retry > 1 {
                    line.push_str(&format!(" retry={retry}"));
                }
                line
            }
            Self::Sleep { duration } => format!("sleep ms={}", duration.as_millis()),
            Self::Snapshot { id } => {
                format!("snapshot id='{}'", escape_single_quote(id))
            }
            Self::AssertScreen {
                pane,
                contains,
                not_contains,
                matches,
            } => {
                let mut line = "assert-screen".to_string();
                if let Some(p) = pane {
                    line.push_str(&format!(" pane={p}"));
                }
                if let Some(c) = contains {
                    line.push_str(&format!(" contains='{}'", escape_single_quote(c)));
                }
                if let Some(nc) = not_contains {
                    line.push_str(&format!(" not_contains='{}'", escape_single_quote(nc)));
                }
                if let Some(m) = matches {
                    line.push_str(&format!(" matches='{}'", escape_single_quote(m)));
                }
                line
            }
            Self::AssertLayout { pane_count } => {
                format!("assert-layout pane_count={pane_count}")
            }
            Self::AssertCursor { pane, row, col } => {
                let mut line = format!("assert-cursor row={row} col={col}");
                if let Some(p) = pane {
                    line.push_str(&format!(" pane={p}"));
                }
                line
            }
            Self::ResizeViewport { cols, rows } => {
                format!("resize-viewport cols={cols} rows={rows}")
            }
            Self::PrefixKey { key } => format!("prefix-key key={key}"),
            Self::WaitForEvent { event, timeout } => {
                let escaped = escape_single_quote(event);
                let ms = timeout.as_millis();
                if ms != 5000 {
                    format!("wait-for-event event='{escaped}' timeout={ms}")
                } else {
                    format!("wait-for-event event='{escaped}'")
                }
            }
            Self::InvokeService {
                capability,
                kind,
                interface_id,
                operation,
                payload,
            } => {
                let kind_str = match kind {
                    ServiceKind::Query => "query",
                    ServiceKind::Command => "command",
                };
                let mut line = format!(
                    "invoke-service capability='{}' interface='{}' operation='{}' kind={kind_str}",
                    escape_single_quote(capability),
                    escape_single_quote(interface_id),
                    escape_single_quote(operation),
                );
                if !payload.is_empty() {
                    line.push_str(&format!(" payload='{}'", escape_single_quote(payload)));
                }
                line
            }
            Self::Screen => "screen".to_string(),
            Self::Status => "status".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn config_with_env_mode(mode: Option<SandboxEnvMode>) -> PlaybookConfig {
        PlaybookConfig {
            env_mode: mode,
            ..Default::default()
        }
    }

    fn set_env(key: &str, val: &str) {
        // SAFETY: Tests using this helper are marked #[serial] to prevent
        // concurrent env var mutation.
        unsafe { std::env::set_var(key, val) };
    }

    fn remove_env(key: &str) {
        // SAFETY: Tests using this helper are marked #[serial].
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    #[serial]
    fn explicit_clean_wins_over_env_var() {
        set_env("BMUX_PLAYBOOK_ENV_MODE", "inherit");
        let config = config_with_env_mode(Some(SandboxEnvMode::Clean));
        assert_eq!(config.effective_env_mode(), SandboxEnvMode::Clean);
        remove_env("BMUX_PLAYBOOK_ENV_MODE");
    }

    #[test]
    #[serial]
    fn explicit_inherit_wins_over_env_var() {
        set_env("BMUX_PLAYBOOK_ENV_MODE", "clean");
        let config = config_with_env_mode(Some(SandboxEnvMode::Inherit));
        assert_eq!(config.effective_env_mode(), SandboxEnvMode::Inherit);
        remove_env("BMUX_PLAYBOOK_ENV_MODE");
    }

    #[test]
    #[serial]
    fn none_falls_through_to_env_var_clean() {
        set_env("BMUX_PLAYBOOK_ENV_MODE", "clean");
        let config = config_with_env_mode(None);
        assert_eq!(config.effective_env_mode(), SandboxEnvMode::Clean);
        remove_env("BMUX_PLAYBOOK_ENV_MODE");
    }

    #[test]
    #[serial]
    fn none_falls_through_to_env_var_inherit() {
        set_env("BMUX_PLAYBOOK_ENV_MODE", "inherit");
        let config = config_with_env_mode(None);
        assert_eq!(config.effective_env_mode(), SandboxEnvMode::Inherit);
        remove_env("BMUX_PLAYBOOK_ENV_MODE");
    }

    #[test]
    #[serial]
    fn none_no_env_var_defaults_to_inherit() {
        remove_env("BMUX_PLAYBOOK_ENV_MODE");
        let config = config_with_env_mode(None);
        assert_eq!(config.effective_env_mode(), SandboxEnvMode::Inherit);
    }

    #[test]
    #[serial]
    fn none_invalid_env_var_defaults_to_inherit() {
        set_env("BMUX_PLAYBOOK_ENV_MODE", "garbage");
        let config = config_with_env_mode(None);
        assert_eq!(config.effective_env_mode(), SandboxEnvMode::Inherit);
        remove_env("BMUX_PLAYBOOK_ENV_MODE");
    }

    // ── to_dsl() round-trip tests ──────────────────────────────────────

    /// Parse a single DSL action line (prepended with `new-session` so the
    /// playbook is valid) and return the parsed action at the given step index.
    fn parse_action_dsl(dsl_line: &str, step_index: usize) -> Action {
        let input = format!("new-session\n{dsl_line}\n");
        let (playbook, _) =
            crate::playbook::parse_dsl::parse_dsl(&input).expect("DSL should parse");
        playbook.steps[step_index].action.clone()
    }

    /// Round-trip helper: serialize an action to DSL, parse it back, return both.
    fn round_trip(action: &Action) -> (String, Action) {
        let dsl = action.to_dsl();
        let parsed = parse_action_dsl(&dsl, 1); // index 1 because new-session is index 0
        (dsl, parsed)
    }

    #[test]
    fn to_dsl_round_trip_new_session_no_name() {
        let action = Action::NewSession { name: None };
        // new-session is the first step, so parse at index 0
        let dsl = action.to_dsl();
        let (playbook, _) = crate::playbook::parse_dsl::parse_dsl(&format!("{dsl}\n")).unwrap();
        assert!(matches!(
            playbook.steps[0].action,
            Action::NewSession { name: None }
        ));
    }

    #[test]
    fn to_dsl_round_trip_new_session_with_name() {
        let action = Action::NewSession {
            name: Some("my-session".to_string()),
        };
        let dsl = action.to_dsl();
        let (playbook, _) = crate::playbook::parse_dsl::parse_dsl(&format!("{dsl}\n")).unwrap();
        match &playbook.steps[0].action {
            Action::NewSession { name } => assert_eq!(name.as_deref(), Some("my-session")),
            other => panic!("expected NewSession, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_kill_session() {
        let (_, parsed) = round_trip(&Action::KillSession {
            name: "test".to_string(),
        });
        match parsed {
            Action::KillSession { name } => assert_eq!(name, "test"),
            other => panic!("expected KillSession, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_split_pane() {
        let (_, parsed) = round_trip(&Action::SplitPane {
            direction: SplitDirection::Horizontal,
            ratio: None,
        });
        match parsed {
            Action::SplitPane { direction, .. } => {
                assert_eq!(direction, SplitDirection::Horizontal)
            }
            other => panic!("expected SplitPane, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_focus_pane() {
        let (_, parsed) = round_trip(&Action::FocusPane { target: 3 });
        match parsed {
            Action::FocusPane { target } => assert_eq!(target, 3),
            other => panic!("expected FocusPane, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_close_pane() {
        let (_, parsed) = round_trip(&Action::ClosePane { target: Some(2) });
        match parsed {
            Action::ClosePane { target } => assert_eq!(target, Some(2)),
            other => panic!("expected ClosePane, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_close_pane_no_target() {
        let (_, parsed) = round_trip(&Action::ClosePane { target: None });
        match parsed {
            Action::ClosePane { target } => assert_eq!(target, None),
            other => panic!("expected ClosePane, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_send_keys() {
        let action = Action::SendKeys {
            keys: b"echo hello\r".to_vec(),
            pane: None,
        };
        let (_, parsed) = round_trip(&action);
        match parsed {
            Action::SendKeys { keys, pane } => {
                assert_eq!(keys, b"echo hello\r");
                assert_eq!(pane, None);
            }
            other => panic!("expected SendKeys, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_send_keys_with_pane() {
        let action = Action::SendKeys {
            keys: b"\x1b[A".to_vec(), // ESC [ A (up arrow)
            pane: Some(2),
        };
        let (_, parsed) = round_trip(&action);
        match parsed {
            Action::SendKeys { keys, pane } => {
                assert_eq!(keys, b"\x1b[A");
                assert_eq!(pane, Some(2));
            }
            other => panic!("expected SendKeys, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_send_bytes() {
        let action = Action::SendBytes {
            hex: vec![0x1b, 0x5b, 0x41],
        };
        let (_, parsed) = round_trip(&action);
        match parsed {
            Action::SendBytes { hex } => assert_eq!(hex, vec![0x1b, 0x5b, 0x41]),
            other => panic!("expected SendBytes, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_wait_for_default_timeout() {
        let action = Action::WaitFor {
            pattern: "hello".to_string(),
            pane: None,
            timeout: Duration::from_millis(5000),
            retry: 1,
        };
        let (dsl, parsed) = round_trip(&action);
        // Default timeout should be omitted from DSL
        assert!(
            !dsl.contains("timeout="),
            "default timeout should be omitted: {dsl}"
        );
        match parsed {
            Action::WaitFor {
                pattern, timeout, ..
            } => {
                assert_eq!(pattern, "hello");
                assert_eq!(timeout, Duration::from_millis(5000));
            }
            other => panic!("expected WaitFor, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_wait_for_custom_timeout() {
        let action = Action::WaitFor {
            pattern: "prompt\\$".to_string(),
            pane: Some(1),
            timeout: Duration::from_millis(10000),
            retry: 1,
        };
        let (dsl, parsed) = round_trip(&action);
        assert!(
            dsl.contains("timeout=10000"),
            "custom timeout in DSL: {dsl}"
        );
        match parsed {
            Action::WaitFor {
                pattern,
                pane,
                timeout,
                ..
            } => {
                assert_eq!(pattern, "prompt\\$");
                assert_eq!(pane, Some(1));
                assert_eq!(timeout, Duration::from_millis(10000));
            }
            other => panic!("expected WaitFor, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_sleep() {
        let (_, parsed) = round_trip(&Action::Sleep {
            duration: Duration::from_millis(500),
        });
        match parsed {
            Action::Sleep { duration } => assert_eq!(duration, Duration::from_millis(500)),
            other => panic!("expected Sleep, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_snapshot() {
        let (_, parsed) = round_trip(&Action::Snapshot {
            id: "after_echo".to_string(),
        });
        match parsed {
            Action::Snapshot { id } => assert_eq!(id, "after_echo"),
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_assert_screen() {
        let action = Action::AssertScreen {
            pane: Some(1),
            contains: Some("hello".to_string()),
            not_contains: Some("error".to_string()),
            matches: Some("\\d+".to_string()),
        };
        let (_, parsed) = round_trip(&action);
        match parsed {
            Action::AssertScreen {
                pane,
                contains,
                not_contains,
                matches,
            } => {
                assert_eq!(pane, Some(1));
                assert_eq!(contains.as_deref(), Some("hello"));
                assert_eq!(not_contains.as_deref(), Some("error"));
                assert_eq!(matches.as_deref(), Some("\\d+"));
            }
            other => panic!("expected AssertScreen, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_assert_layout() {
        let (_, parsed) = round_trip(&Action::AssertLayout { pane_count: 3 });
        match parsed {
            Action::AssertLayout { pane_count } => assert_eq!(pane_count, 3),
            other => panic!("expected AssertLayout, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_assert_cursor() {
        let action = Action::AssertCursor {
            pane: Some(1),
            row: 5,
            col: 10,
        };
        let (_, parsed) = round_trip(&action);
        match parsed {
            Action::AssertCursor { pane, row, col } => {
                assert_eq!(pane, Some(1));
                assert_eq!(row, 5);
                assert_eq!(col, 10);
            }
            other => panic!("expected AssertCursor, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_resize_viewport() {
        let (_, parsed) = round_trip(&Action::ResizeViewport {
            cols: 132,
            rows: 50,
        });
        match parsed {
            Action::ResizeViewport { cols, rows } => {
                assert_eq!(cols, 132);
                assert_eq!(rows, 50);
            }
            other => panic!("expected ResizeViewport, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_prefix_key() {
        let (_, parsed) = round_trip(&Action::PrefixKey { key: 'c' });
        match parsed {
            Action::PrefixKey { key } => assert_eq!(key, 'c'),
            other => panic!("expected PrefixKey, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_wait_for_event() {
        let action = Action::WaitForEvent {
            event: "session_created".to_string(),
            timeout: Duration::from_millis(5000),
        };
        let (dsl, parsed) = round_trip(&action);
        assert!(!dsl.contains("timeout="), "default timeout omitted: {dsl}");
        match parsed {
            Action::WaitForEvent { event, timeout } => {
                assert_eq!(event, "session_created");
                assert_eq!(timeout, Duration::from_millis(5000));
            }
            other => panic!("expected WaitForEvent, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_invoke_service() {
        let action = Action::InvokeService {
            capability: "my.cap".to_string(),
            kind: ServiceKind::Query,
            interface_id: "iface.1".to_string(),
            operation: "do_thing".to_string(),
            payload: r#"{"key":"val"}"#.to_string(),
        };
        let (_, parsed) = round_trip(&action);
        match parsed {
            Action::InvokeService {
                capability,
                kind,
                interface_id,
                operation,
                payload,
            } => {
                assert_eq!(capability, "my.cap");
                assert_eq!(kind, ServiceKind::Query);
                assert_eq!(interface_id, "iface.1");
                assert_eq!(operation, "do_thing");
                assert_eq!(payload, r#"{"key":"val"}"#);
            }
            other => panic!("expected InvokeService, got {other:?}"),
        }
    }

    #[test]
    fn to_dsl_round_trip_screen_status() {
        let (_, parsed_screen) = round_trip(&Action::Screen);
        assert!(matches!(parsed_screen, Action::Screen));

        let (_, parsed_status) = round_trip(&Action::Status);
        assert!(matches!(parsed_status, Action::Status));
    }

    #[test]
    fn to_dsl_round_trip_wait_for_with_retry() {
        let action = Action::WaitFor {
            pattern: "hello".to_string(),
            pane: None,
            timeout: Duration::from_millis(5000),
            retry: 3,
        };
        let (dsl, parsed) = round_trip(&action);
        assert!(dsl.contains("retry=3"), "DSL should contain retry=3: {dsl}");
        match parsed {
            Action::WaitFor { retry, .. } => assert_eq!(retry, 3),
            other => panic!("expected WaitFor, got {other:?}"),
        }
    }
}
