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
    AssertLayout { pane_count: Option<u32> },
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
#[derive(Debug, Clone, Serialize)]
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
}

/// Result of a single step execution.
#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub index: usize,
    pub action: String,
    pub status: StepStatus,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Step execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pass,
    Fail,
    Skip,
}

/// A named snapshot of screen state captured mid-playbook.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotCapture {
    pub id: String,
    pub panes: Vec<PaneCapture>,
}

/// Captured state of a single pane.
#[derive(Debug, Clone, Serialize)]
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
}
