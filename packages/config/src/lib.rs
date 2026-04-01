#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Configuration management for bmux terminal multiplexer
//!
//! This crate provides configuration loading, validation, and management
//! for the bmux terminal multiplexer system.

use bmux_config_doc_derive::{ConfigDoc, ConfigDocEnum};
use bmux_event::Mode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

pub mod keybind;
pub mod paths;
pub mod theme;

pub use bmux_config_doc::{ConfigDocSchema, FieldDoc};
pub use keybind::{KeyBindingConfig, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS, ResolvedTimeout};
pub use paths::ConfigPaths;
pub use theme::ThemeConfig;

/// Configuration error types
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Configuration file not found: {}", path.display())]
    FileNotFound { path: PathBuf },

    #[error("Failed to read configuration file: {error}")]
    ReadError { error: String },

    #[error("Failed to parse configuration: {error}")]
    ParseError { error: String },

    #[error("Invalid configuration value: {field} = {value}")]
    InvalidValue { field: String, value: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// Root configuration structure for bmux, deserialized from `bmux.toml`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BmuxConfig {
    /// Core session defaults: shell, scrollback depth, and server connection settings
    pub general: GeneralConfig,
    /// Visual styling: color theme, pane borders, status bar placement, and window titles
    pub appearance: AppearanceConfig,
    /// Runtime behavior toggles for terminal protocol handling, layout persistence, and build compatibility
    pub behavior: BehaviorConfig,
    /// Settings for multiple clients attached to the same session
    pub multi_client: MultiClientConfig,
    /// Keyboard shortcuts organized by scope and interaction mode
    pub keybindings: KeyBindingConfig,
    /// Plugin discovery, enablement, and per-plugin settings
    pub plugins: PluginConfig,
    /// Content and layout of the status bar displayed at the top or bottom of the terminal
    pub status_bar: StatusBarConfig,
    /// Session recording for terminal replay, debugging, and playbook generation
    pub recording: RecordingConfig,
}

/// Session recording for terminal replay, debugging, and playbook generation.
/// Records pane I/O and lifecycle events to disk.
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "recording")]
#[serde(default)]
pub struct RecordingConfig {
    /// Root directory for recording data.
    /// Relative paths are resolved against the directory containing `bmux.toml`.
    pub dir: Option<PathBuf>,
    /// Enable the recording subsystem. When false, no recording data is captured
    /// or written to disk regardless of other recording settings.
    pub enabled: bool,
    /// Capture pane input bytes (keystrokes sent to pane processes)
    pub capture_input: bool,
    /// Capture pane output bytes (terminal output from pane processes)
    pub capture_output: bool,
    /// Capture lifecycle and server events (pane creation, resize, close, etc.)
    pub capture_events: bool,
    /// Rotate recording segments at approximately this size in MB
    pub segment_mb: usize,
    /// Retention period for completed recordings in days. Set to 0 to disable
    /// automatic pruning and keep recordings indefinitely.
    pub retention_days: u64,
    /// Default settings for `recording export` rendering.
    pub export: RecordingExportConfig,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            dir: None,
            enabled: true,
            capture_input: true,
            capture_output: true,
            capture_events: true,
            segment_mb: 64,
            retention_days: 30,
            export: RecordingExportConfig::default(),
        }
    }
}

/// Defaults for `recording export` cursor rendering behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ConfigDoc)]
#[config_doc(section = "recording.export")]
#[serde(default)]
pub struct RecordingExportConfig {
    /// Cursor rendering default for `recording export`.
    pub cursor: RecordingExportCursorMode,
    /// Cursor shape default for `recording export`.
    pub cursor_shape: RecordingExportCursorShape,
    /// Cursor blink default for `recording export`.
    pub cursor_blink: RecordingExportCursorBlinkMode,
    /// Cursor blink period default for `recording export`.
    pub cursor_blink_period_ms: u32,
    /// Cursor color default for `recording export` (`auto` or #RRGGBB).
    pub cursor_color: String,
    /// Cursor behavior profile default for `recording export`.
    pub cursor_profile: RecordingExportCursorProfile,
    /// Keep cursor solid after activity for this duration in milliseconds.
    /// `None` lets terminal profiles choose an emulator-specific default.
    pub cursor_solid_after_activity_ms: Option<u32>,
    /// Keep cursor solid after input activity for this duration in milliseconds.
    pub cursor_solid_after_input_ms: Option<u32>,
    /// Keep cursor solid after output activity for this duration in milliseconds.
    pub cursor_solid_after_output_ms: Option<u32>,
    /// Keep cursor solid after cursor movement activity for this duration in milliseconds.
    pub cursor_solid_after_cursor_ms: Option<u32>,
    /// Cursor paint mode default for `recording export`.
    pub cursor_paint_mode: RecordingExportCursorPaintMode,
    /// Cursor text mode default for `recording export`.
    pub cursor_text_mode: RecordingExportCursorTextMode,
    /// Cursor bar width default as a percent of cell width.
    pub cursor_bar_width_pct: u8,
    /// Cursor underline height default as a percent of cell height.
    pub cursor_underline_height_pct: u8,
}

impl Default for RecordingExportConfig {
    fn default() -> Self {
        Self {
            cursor: RecordingExportCursorMode::Auto,
            cursor_shape: RecordingExportCursorShape::Auto,
            cursor_blink: RecordingExportCursorBlinkMode::Auto,
            cursor_blink_period_ms: 500,
            cursor_color: "auto".to_string(),
            cursor_profile: RecordingExportCursorProfile::Auto,
            cursor_solid_after_activity_ms: None,
            cursor_solid_after_input_ms: None,
            cursor_solid_after_output_ms: None,
            cursor_solid_after_cursor_ms: None,
            cursor_paint_mode: RecordingExportCursorPaintMode::Auto,
            cursor_text_mode: RecordingExportCursorTextMode::Auto,
            cursor_bar_width_pct: 16,
            cursor_underline_height_pct: 12,
        }
    }
}

impl RecordingExportConfig {
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorMode {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorShape {
    Auto,
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorBlinkMode {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorProfile {
    Auto,
    Ghostty,
    Generic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorPaintMode {
    Auto,
    Invert,
    Fill,
    Outline,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorTextMode {
    Auto,
    SwapFgBg,
    ForceContrast,
}

/// Core session defaults: shell, scrollback depth, and server connection settings
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "general")]
#[serde(default)]
pub struct GeneralConfig {
    /// Default interaction mode when starting bmux
    #[config_doc(values("normal", "insert", "visual", "command"))]
    pub default_mode: Mode,
    /// Enable mouse support for clicking to focus panes, scrolling through
    /// output, and dragging to resize pane borders
    pub mouse_support: bool,
    /// Default shell to launch in new panes. When unset, uses the SHELL
    /// environment variable or falls back to /bin/sh.
    pub default_shell: Option<String>,
    /// Maximum number of scrollback lines retained per pane. Must be at least 1.
    pub scrollback_limit: usize,
    /// Server socket timeout in milliseconds. Must be at least 1.
    pub server_timeout: u64,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_mode: Mode::Normal,
            mouse_support: true,
            default_shell: None,
            scrollback_limit: 10_000,
            server_timeout: 5_000,
        }
    }
}

/// Visual styling: color theme, pane borders, status bar placement, and window titles
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "appearance")]
#[serde(default)]
pub struct AppearanceConfig {
    /// Name of the color theme to apply. Empty string uses the default theme.
    pub theme: String,
    /// Where to render the status bar. TOP places it above panes, BOTTOM below
    /// panes, and OFF hides it entirely.
    pub status_position: StatusPosition,
    /// Drawing style for pane borders. NONE hides borders entirely, giving
    /// panes the full terminal width.
    pub pane_border_style: BorderStyle,
    /// Display a title label in each pane's border showing the running
    /// process name
    pub show_pane_titles: bool,
    /// Format string for the outer terminal's title bar. Empty string leaves
    /// the title unset.
    pub window_title_format: String,
}

/// Runtime behavior toggles for terminal protocol handling, layout persistence,
/// and build compatibility
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "behavior")]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct BehaviorConfig {
    /// Immediately resize panes to the largest remaining client when a client
    /// disconnects, rather than keeping the previous dimensions
    pub aggressive_resize: bool,
    /// Highlight panes in the status bar when they produce output while
    /// unfocused, so you can tell which panes have new activity
    pub visual_activity: bool,
    /// How to handle terminal bell signals from panes. NONE ignores bells
    /// entirely. ANY notifies on bells from any pane. CURRENT only notifies
    /// from the focused pane. OTHER only notifies from unfocused panes.
    pub bell_action: BellAction,
    /// Automatically rename windows based on the currently running command
    /// in the focused pane
    pub automatic_rename: bool,
    /// Exit bmux when no sessions remain
    pub exit_empty: bool,
    /// Restore and persist the last local CLI runtime layout across sessions,
    /// so reattaching resumes where you left off
    pub restore_last_layout: bool,
    /// Prompt for confirmation before a destructive quit that clears
    /// persisted local runtime state
    pub confirm_quit_destroy: bool,
    /// Terminal type exposed to pane processes via the TERM environment
    /// variable. Common values: bmux-256color, xterm-256color, screen-256color.
    pub pane_term: String,
    /// Enable protocol query/reply tracing in the runtime. Useful for
    /// debugging terminal protocol behavior with CSI/OSC/DCS sequences.
    pub protocol_trace_enabled: bool,
    /// Maximum number of in-memory protocol trace events to retain.
    /// Must be at least 1.
    pub protocol_trace_capacity: usize,
    /// What to do when the bmux terminfo entry is missing from the system.
    /// ask prompts before installing. always installs silently. never skips
    /// installation, which may degrade terminal rendering.
    pub terminfo_auto_install: TerminfoAutoInstall,
    /// Number of days to wait before prompting again after the user declines
    /// terminfo installation
    pub terminfo_prompt_cooldown_days: u64,
    /// What to do when the running server was built from a different version
    /// than the current CLI binary. error refuses to connect until the server
    /// is restarted. warn connects with a warning message.
    pub stale_build_action: StaleBuildAction,
    /// Enable the Kitty keyboard protocol for enhanced key reporting.
    /// When true, bmux negotiates enhanced keyboard mode with the outer
    /// terminal, allowing modified special keys like Ctrl+Enter to be
    /// correctly forwarded to pane programs.
    pub kitty_keyboard: bool,
    /// Mouse interaction settings for attach mode (focus/scroll gestures).
    pub mouse: MouseBehaviorConfig,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            aggressive_resize: false,
            visual_activity: false,
            bell_action: BellAction::Any,
            automatic_rename: false,
            exit_empty: false,
            restore_last_layout: true,
            confirm_quit_destroy: true,
            pane_term: "bmux-256color".to_string(),
            protocol_trace_enabled: false,
            protocol_trace_capacity: 200,
            terminfo_auto_install: TerminfoAutoInstall::Never,
            terminfo_prompt_cooldown_days: 7,
            stale_build_action: StaleBuildAction::Error,
            kitty_keyboard: true,
            mouse: MouseBehaviorConfig::default(),
        }
    }
}

/// Mouse interaction behavior for attach mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ConfigDoc)]
#[config_doc(section = "behavior.mouse")]
#[serde(default)]
pub struct MouseBehaviorConfig {
    /// Master toggle for mouse handling in attach mode.
    pub enabled: bool,
    /// Focus pane when clicking inside it.
    pub focus_on_click: bool,
    /// Focus pane when hovering over it.
    pub focus_on_hover: bool,
    /// Hover dwell time before focus is applied.
    pub hover_delay_ms: u64,
    /// Route wheel scrolling to focused pane scrollback.
    pub scroll_scrollback: bool,
    /// Number of scrollback lines per mouse wheel tick.
    pub scroll_lines_per_tick: u16,
    /// Exit scrollback mode automatically when wheel scrolling reaches bottom.
    pub exit_scrollback_on_bottom: bool,
    /// Optional mouse gesture overrides mapped to action names.
    ///
    /// Supported keys in the current core runtime are `click_left`,
    /// `hover_focus`, `scroll_up`, and `scroll_down`.
    /// Values use the same action naming format as keybindings, including
    /// built-in action names and `plugin:<id>:<command>`.
    pub gesture_actions: BTreeMap<String, String>,
}

impl Default for MouseBehaviorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            focus_on_click: true,
            focus_on_hover: false,
            hover_delay_ms: 175,
            scroll_scrollback: true,
            scroll_lines_per_tick: 3,
            exit_scrollback_on_bottom: true,
            gesture_actions: BTreeMap::new(),
        }
    }
}

impl MouseBehaviorConfig {
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StaleBuildAction {
    #[default]
    Error,
    Warn,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum TerminfoAutoInstall {
    Ask,
    Always,
    #[default]
    Never,
}

/// Settings for multiple clients attached to the same session, controlling
/// independent views and mode synchronization
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "multi_client")]
#[serde(default)]
pub struct MultiClientConfig {
    /// Allow clients to have independent views of the same session, with
    /// separate focused panes and scroll positions
    pub allow_independent_views: bool,
    /// When true, new clients automatically track the leader client's focused
    /// pane and scroll position. When false, clients start with an independent view.
    pub default_follow_mode: bool,
    /// Maximum number of clients that can attach to a single session.
    /// Set to 0 for unlimited.
    pub max_clients_per_session: usize,
    /// When true, switching interaction modes (e.g. Normal to Insert) on one
    /// client applies to all attached clients in the same session
    pub sync_client_modes: bool,
}

/// Plugin discovery, enablement, and per-plugin settings. Bundled plugins
/// (like bmux.windows and bmux.permissions) are enabled by default.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "plugins")]
#[serde(default)]
pub struct PluginConfig {
    /// Plugin IDs to enable in addition to the bundled defaults. Bundled
    /// plugins like bmux.windows and bmux.permissions are enabled automatically
    /// without being listed here.
    pub enabled: Vec<String>,
    /// Plugin IDs to explicitly disable, including bundled ones. Overrides
    /// both bundled defaults and the enabled list.
    pub disabled: Vec<String>,
    /// Additional directories to scan for plugin binaries beyond the default
    /// plugin search path
    pub search_paths: Vec<PathBuf>,
    /// Per-plugin settings keyed by plugin ID. Each plugin defines its own
    /// accepted keys and values.
    pub settings: BTreeMap<String, toml::Value>,
}

/// Content and layout of the status bar displayed at the top or bottom
/// of the terminal
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "status_bar")]
#[serde(default)]
pub struct StatusBarConfig {
    /// Maximum number of tabs shown in the tab strip before overflow is collapsed.
    pub max_tabs: usize,
    /// Maximum display width for each tab label.
    pub tab_label_max_width: usize,
    /// Prefix used for active tabs.
    pub active_tab_prefix: String,
    /// Suffix used for active tabs.
    pub active_tab_suffix: String,
    /// Prefix used for inactive tabs.
    pub inactive_tab_prefix: String,
    /// Suffix used for inactive tabs.
    pub inactive_tab_suffix: String,
    /// Separator inserted between tab entries.
    pub tab_separator: String,
    /// Marker appended when additional tabs are hidden.
    pub tab_overflow_marker: String,
    /// Display 1-based tab indexes before labels.
    pub show_tab_index: bool,
    /// Which context set to render as tabs.
    pub tab_scope: StatusTabScope,
    /// Display the active session name in the status bar.
    pub show_session_name: bool,
    /// Display the current context label in the status bar.
    pub show_context_name: bool,
    /// Display the current interaction mode (Normal, Scroll, Help, etc.).
    pub show_mode: bool,
    /// Display the current attach role (write/read-only).
    pub show_role: bool,
    /// Display follow target details when following another client.
    pub show_follow: bool,
    /// Display runtime hints on the right side.
    pub show_hint: bool,
    /// Separator used between non-tab status segments.
    pub segment_separator: String,
    /// Hint visibility policy.
    pub hint_policy: StatusHintPolicy,
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        Self {
            max_tabs: 12,
            tab_label_max_width: 20,
            active_tab_prefix: "[".to_string(),
            active_tab_suffix: "]".to_string(),
            inactive_tab_prefix: " ".to_string(),
            inactive_tab_suffix: " ".to_string(),
            tab_separator: " ".to_string(),
            tab_overflow_marker: "+".to_string(),
            show_tab_index: true,
            tab_scope: StatusTabScope::AllContexts,
            show_session_name: true,
            show_context_name: false,
            show_mode: true,
            show_role: true,
            show_follow: true,
            show_hint: true,
            segment_separator: " | ".to_string(),
            hint_policy: StatusHintPolicy::Always,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusTabScope {
    #[default]
    AllContexts,
    SessionContexts,
    Mru,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusHintPolicy {
    #[default]
    Always,
    ScrollOnly,
    Never,
}

/// Status bar position
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, ConfigDocEnum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StatusPosition {
    Top,
    #[default]
    Bottom,
    Off,
}

/// Border style for panes
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, ConfigDocEnum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BorderStyle {
    #[default]
    Single,
    Double,
    Rounded,
    Thick,
    None,
}

/// Bell action behavior
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, ConfigDocEnum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BellAction {
    None,
    #[default]
    Any,
    Current,
    Other,
}

impl BmuxConfig {
    #[must_use]
    pub fn attach_mouse_config(&self) -> MouseBehaviorConfig {
        let defaults = MouseBehaviorConfig::default();
        if self.behavior.mouse == defaults
            && self.general.mouse_support != GeneralConfig::default().mouse_support
        {
            let mut mapped = self.behavior.mouse.clone();
            mapped.enabled = self.general.mouse_support;
            return mapped;
        }
        self.behavior.mouse.clone()
    }

    /// Resolve the effective recordings directory.
    ///
    /// If `recording.dir` is set and relative, it is resolved relative to the
    /// directory that contains the active `bmux.toml`.
    #[must_use]
    pub fn recordings_dir(&self, paths: &ConfigPaths) -> PathBuf {
        match &self.recording.dir {
            Some(dir) if dir.is_absolute() => dir.clone(),
            Some(dir) => {
                let base = paths
                    .config_file()
                    .parent()
                    .map_or_else(|| paths.config_dir.clone(), std::path::Path::to_path_buf);
                base.join(dir)
            }
            None => paths.recordings_dir(),
        }
    }

    /// Load configuration from default location
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load() -> Result<Self> {
        let paths = ConfigPaths::default();
        Self::load_from_path(&paths.config_file())
    }

    /// Load configuration from a specific path
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_from_path(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(path).map_err(|e| ConfigError::ReadError {
            error: e.to_string(),
        })?;

        let mut config: Self = toml::from_str(&contents).map_err(|e| ConfigError::ParseError {
            error: e.to_string(),
        })?;

        let repaired_fields = config.sanitize_invalid_values();
        if !repaired_fields.is_empty() {
            for warning in &repaired_fields {
                eprintln!("bmux warning: repaired invalid config value {warning}");
            }
        }

        config.validate()?;
        Ok(config)
    }

    /// Save configuration to default location
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be written.
    pub fn save(&self) -> Result<()> {
        let paths = ConfigPaths::default();
        self.save_to_path(&paths.config_file())
    }

    /// Save configuration to a specific path
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be written.
    pub fn save_to_path(&self, path: &std::path::Path) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self).map_err(|e| ConfigError::ParseError {
            error: e.to_string(),
        })?;

        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Validate the configuration
    ///
    /// # Errors
    ///
    /// Returns an error if any configuration values are invalid.
    pub fn validate(&self) -> Result<()> {
        // Validate scrollback limit
        if self.general.scrollback_limit == 0 {
            return Err(ConfigError::InvalidValue {
                field: "general.scrollback_limit".to_string(),
                value: "0".to_string(),
            });
        }

        // Validate server timeout
        if self.general.server_timeout == 0 {
            return Err(ConfigError::InvalidValue {
                field: "general.server_timeout".to_string(),
                value: "0".to_string(),
            });
        }

        if self.keybindings.prefix.trim().is_empty() {
            return Err(ConfigError::InvalidValue {
                field: "keybindings.prefix".to_string(),
                value: self.keybindings.prefix.clone(),
            });
        }

        if let Err(error) = self.keybindings.resolve_timeout() {
            return Err(ConfigError::InvalidValue {
                field: "keybindings".to_string(),
                value: error,
            });
        }

        if self.behavior.mouse.hover_delay_ms == 0 {
            return Err(ConfigError::InvalidValue {
                field: "behavior.mouse.hover_delay_ms".to_string(),
                value: "0".to_string(),
            });
        }

        if self.behavior.mouse.scroll_lines_per_tick == 0 {
            return Err(ConfigError::InvalidValue {
                field: "behavior.mouse.scroll_lines_per_tick".to_string(),
                value: "0".to_string(),
            });
        }

        if self.recording.export.cursor_blink_period_ms == 0 {
            return Err(ConfigError::InvalidValue {
                field: "recording.export.cursor_blink_period_ms".to_string(),
                value: "0".to_string(),
            });
        }
        if self.recording.export.cursor_bar_width_pct == 0
            || self.recording.export.cursor_bar_width_pct > 100
        {
            return Err(ConfigError::InvalidValue {
                field: "recording.export.cursor_bar_width_pct".to_string(),
                value: self.recording.export.cursor_bar_width_pct.to_string(),
            });
        }
        if self.recording.export.cursor_underline_height_pct == 0
            || self.recording.export.cursor_underline_height_pct > 100
        {
            return Err(ConfigError::InvalidValue {
                field: "recording.export.cursor_underline_height_pct".to_string(),
                value: self
                    .recording
                    .export
                    .cursor_underline_height_pct
                    .to_string(),
            });
        }

        if self.status_bar.max_tabs == 0 {
            return Err(ConfigError::InvalidValue {
                field: "status_bar.max_tabs".to_string(),
                value: "0".to_string(),
            });
        }

        if self.status_bar.tab_label_max_width == 0 {
            return Err(ConfigError::InvalidValue {
                field: "status_bar.tab_label_max_width".to_string(),
                value: "0".to_string(),
            });
        }

        Ok(())
    }

    fn sanitize_invalid_values(&mut self) -> Vec<String> {
        let general_defaults = GeneralConfig::default();
        let keybind_defaults = KeyBindingConfig::default();
        let behavior_defaults = BehaviorConfig::default();
        let recording_defaults = RecordingConfig::default();
        let mut repaired_fields = Vec::new();

        if self.general.scrollback_limit == 0 {
            self.general.scrollback_limit = general_defaults.scrollback_limit;
            repaired_fields.push(format!(
                "general.scrollback_limit=0 -> {}",
                general_defaults.scrollback_limit
            ));
        }

        if self.general.server_timeout == 0 {
            self.general.server_timeout = general_defaults.server_timeout;
            repaired_fields.push(format!(
                "general.server_timeout=0 -> {}",
                general_defaults.server_timeout
            ));
        }

        if self.keybindings.prefix.trim().is_empty() {
            self.keybindings.prefix = keybind_defaults.prefix.clone();
            repaired_fields.push(format!(
                "keybindings.prefix=<empty> -> {}",
                keybind_defaults.prefix
            ));
        }

        if let Err(error) = self.keybindings.resolve_timeout() {
            self.keybindings.timeout_ms = keybind_defaults.timeout_ms;
            self.keybindings.timeout_profile = keybind_defaults.timeout_profile.clone();
            self.keybindings.timeout_profiles = keybind_defaults.timeout_profiles;
            repaired_fields.push(format!(
                "keybindings timeout settings -> indefinite ({error})"
            ));
        }

        if self.behavior.protocol_trace_capacity == 0 {
            self.behavior.protocol_trace_capacity = behavior_defaults.protocol_trace_capacity;
            repaired_fields.push(format!(
                "behavior.protocol_trace_capacity=0 -> {}",
                behavior_defaults.protocol_trace_capacity
            ));
        }

        if self.behavior.mouse == MouseBehaviorConfig::default()
            && self.general.mouse_support != general_defaults.mouse_support
        {
            self.behavior.mouse.enabled = self.general.mouse_support;
            repaired_fields.push(format!(
                "general.mouse_support={} -> behavior.mouse.enabled={}",
                self.general.mouse_support, self.general.mouse_support
            ));
        }

        if self.behavior.mouse.hover_delay_ms == 0 {
            self.behavior.mouse.hover_delay_ms = MouseBehaviorConfig::default().hover_delay_ms;
            repaired_fields.push(format!(
                "behavior.mouse.hover_delay_ms=0 -> {}",
                self.behavior.mouse.hover_delay_ms
            ));
        }

        if self.behavior.mouse.scroll_lines_per_tick == 0 {
            self.behavior.mouse.scroll_lines_per_tick =
                MouseBehaviorConfig::default().scroll_lines_per_tick;
            repaired_fields.push(format!(
                "behavior.mouse.scroll_lines_per_tick=0 -> {}",
                self.behavior.mouse.scroll_lines_per_tick
            ));
        }

        if self.recording.segment_mb == 0 {
            self.recording.segment_mb = recording_defaults.segment_mb;
            repaired_fields.push(format!(
                "recording.segment_mb=0 -> {}",
                recording_defaults.segment_mb
            ));
        }

        if self.recording.export.cursor_blink_period_ms == 0 {
            self.recording.export.cursor_blink_period_ms =
                recording_defaults.export.cursor_blink_period_ms;
            repaired_fields.push(format!(
                "recording.export.cursor_blink_period_ms=0 -> {}",
                self.recording.export.cursor_blink_period_ms
            ));
        }
        if self.recording.export.cursor_bar_width_pct == 0
            || self.recording.export.cursor_bar_width_pct > 100
        {
            self.recording.export.cursor_bar_width_pct =
                recording_defaults.export.cursor_bar_width_pct;
            repaired_fields.push(format!(
                "recording.export.cursor_bar_width_pct out of range -> {}",
                self.recording.export.cursor_bar_width_pct
            ));
        }
        if self.recording.export.cursor_underline_height_pct == 0
            || self.recording.export.cursor_underline_height_pct > 100
        {
            self.recording.export.cursor_underline_height_pct =
                recording_defaults.export.cursor_underline_height_pct;
            repaired_fields.push(format!(
                "recording.export.cursor_underline_height_pct out of range -> {}",
                self.recording.export.cursor_underline_height_pct
            ));
        }

        if self.status_bar.max_tabs == 0 {
            self.status_bar.max_tabs = StatusBarConfig::default().max_tabs;
            repaired_fields.push(format!(
                "status_bar.max_tabs=0 -> {}",
                self.status_bar.max_tabs
            ));
        }

        if self.status_bar.tab_label_max_width == 0 {
            self.status_bar.tab_label_max_width = StatusBarConfig::default().tab_label_max_width;
            repaired_fields.push(format!(
                "status_bar.tab_label_max_width=0 -> {}",
                self.status_bar.tab_label_max_width
            ));
        }

        repaired_fields
    }

    /// Merge another configuration into this one
    pub fn merge(&mut self, other: Self) {
        // This is a simple merge that overwrites values
        // In a real implementation, you might want more sophisticated merging
        *self = other;
    }
}

#[cfg(test)]
mod tests {
    use super::{BmuxConfig, ResolvedTimeout, StaleBuildAction};
    use crate::ConfigPaths;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_path() -> std::path::PathBuf {
        let unique = format!(
            "bmux-config-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("failed to create temp test directory");
        dir.join("bmux.toml")
    }

    #[test]
    fn default_config_is_valid() {
        let config = BmuxConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.behavior.stale_build_action, StaleBuildAction::Error);
        assert!(config.plugins.enabled.is_empty());
        assert!(config.plugins.disabled.is_empty());
        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("default timeout"),
            ResolvedTimeout::Indefinite
        );
    }

    #[test]
    fn load_missing_file_returns_defaults_without_creating_file() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();

        let config = BmuxConfig::load_from_path(&path).expect("failed loading missing config");
        let defaults = BmuxConfig::default();

        assert!(!path.exists());
        assert_eq!(
            config.general.scrollback_limit,
            defaults.general.scrollback_limit
        );
        assert_eq!(
            config.general.server_timeout,
            defaults.general.server_timeout
        );
        assert_eq!(config.keybindings.prefix, defaults.keybindings.prefix);
        assert_eq!(
            config.keybindings.timeout_ms,
            defaults.keybindings.timeout_ms
        );
        assert_eq!(
            config.keybindings.timeout_profile,
            defaults.keybindings.timeout_profile
        );
        assert!(config.plugins.disabled.is_empty());

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_parses_plugin_disabled_list() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[plugins]\nenabled = ['bmux.windows']\ndisabled = ['bmux.permissions']\n",
        )
        .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.plugins.enabled, vec!["bmux.windows".to_string()]);
        assert_eq!(
            config.plugins.disabled,
            vec!["bmux.permissions".to_string()]
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_repairs_zero_general_limits_without_persisting() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();

        std::fs::write(
            &path,
            "[general]\nscrollback_limit = 0\nserver_timeout = 0\n",
        )
        .expect("failed writing invalid config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.general.scrollback_limit, 10_000);
        assert_eq!(config.general.server_timeout, 5_000);

        let persisted = std::fs::read_to_string(&path).expect("failed reading config file");
        assert!(persisted.contains("scrollback_limit = 0"));
        assert!(persisted.contains("server_timeout = 0"));

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_repairs_invalid_fields_and_keeps_valid_behavior_settings_without_persisting() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();

        std::fs::write(
            &path,
            r#"[general]
scrollback_limit = 0
server_timeout = 5000

[behavior]
pane_term = "xterm-256color"
protocol_trace_enabled = true
protocol_trace_capacity = 0

[keybindings]
prefix = ""
timeout_profile = "warp"

[keybindings.timeout_profiles]
fast = 10
"#,
        )
        .expect("failed writing invalid config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.general.scrollback_limit, 10_000);
        assert_eq!(config.general.server_timeout, 5_000);
        assert_eq!(config.keybindings.prefix, "ctrl+a");
        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("resolved timeout"),
            ResolvedTimeout::Indefinite
        );
        assert_eq!(config.behavior.pane_term, "xterm-256color");
        assert!(config.behavior.protocol_trace_enabled);
        assert_eq!(config.behavior.protocol_trace_capacity, 200);

        let persisted = std::fs::read_to_string(&path).expect("failed reading config file");
        assert!(persisted.contains("scrollback_limit = 0"));
        assert!(persisted.contains("prefix = \"\""));
        assert!(persisted.contains("timeout_profile = \"warp\""));
        assert!(persisted.contains("fast = 10"));
        assert!(persisted.contains("protocol_trace_capacity = 0"));
        assert!(persisted.contains("pane_term = \"xterm-256color\""));

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn timeout_profile_resolves_with_override() {
        let mut config = BmuxConfig::default();
        config.keybindings.timeout_profile = Some("traditional".to_string());
        config
            .keybindings
            .timeout_profiles
            .insert("traditional".to_string(), 450);

        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("resolved timeout"),
            ResolvedTimeout::Profile {
                name: "traditional".to_string(),
                ms: 450,
            }
        );
    }

    #[test]
    fn exact_timeout_takes_precedence_over_profile() {
        let mut config = BmuxConfig::default();
        config.keybindings.timeout_ms = Some(275);
        config.keybindings.timeout_profile = Some("slow".to_string());

        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("resolved timeout"),
            ResolvedTimeout::Exact(275)
        );
    }

    #[test]
    fn invalid_profile_repairs_to_indefinite_without_persisting() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            r#"[keybindings]
timeout_profile = "missing"
"#,
        )
        .expect("failed writing invalid config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("resolved timeout"),
            ResolvedTimeout::Indefinite
        );
        assert_eq!(config.keybindings.timeout_profile, None);

        let persisted = std::fs::read_to_string(&path).expect("failed reading config file");
        assert!(persisted.contains("timeout_profile = \"missing\""));

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_parses_warn_stale_build_action() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[behavior]\nstale_build_action = \"warn\"\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.behavior.stale_build_action, StaleBuildAction::Warn);

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn attach_mouse_config_maps_legacy_general_mouse_support_when_mouse_block_uses_defaults() {
        let mut config = BmuxConfig::default();
        config.general.mouse_support = false;

        let mouse = config.attach_mouse_config();
        assert!(!mouse.enabled);
    }

    #[test]
    fn load_repairs_invalid_mouse_values_without_persisting() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[behavior.mouse]\nhover_delay_ms = 0\nscroll_lines_per_tick = 0\n",
        )
        .expect("failed writing invalid config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.behavior.mouse.hover_delay_ms, 175);
        assert_eq!(config.behavior.mouse.scroll_lines_per_tick, 3);

        let persisted = std::fs::read_to_string(&path).expect("failed reading config file");
        assert!(persisted.contains("hover_delay_ms = 0"));
        assert!(persisted.contains("scroll_lines_per_tick = 0"));

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn recordings_dir_uses_default_when_unset() {
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let config = BmuxConfig::default();

        assert_eq!(config.recordings_dir(&paths), paths.recordings_dir());
    }

    #[test]
    fn recordings_dir_uses_absolute_override() {
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let mut config = BmuxConfig::default();
        config.recording.dir = Some(std::path::PathBuf::from("/custom/recordings"));

        assert_eq!(
            config.recordings_dir(&paths),
            std::path::PathBuf::from("/custom/recordings")
        );
    }

    #[test]
    fn recordings_dir_resolves_relative_to_config_file_directory() {
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/cfg-root"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let mut config = BmuxConfig::default();
        config.recording.dir = Some(std::path::PathBuf::from("recordings/custom"));

        assert_eq!(
            config.recordings_dir(&paths),
            std::path::PathBuf::from("/cfg-root/recordings/custom")
        );
    }

    #[test]
    fn load_parses_recording_dir_override() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[recording]\ndir = 'recordings/custom'\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config.recording.dir,
            Some(std::path::PathBuf::from("recordings/custom"))
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn recording_export_defaults_include_cursor_settings() {
        let config = BmuxConfig::default();
        assert_eq!(
            config.recording.export.cursor,
            crate::RecordingExportCursorMode::Auto
        );
        assert_eq!(
            config.recording.export.cursor_shape,
            crate::RecordingExportCursorShape::Auto
        );
        assert_eq!(
            config.recording.export.cursor_blink,
            crate::RecordingExportCursorBlinkMode::Auto
        );
        assert_eq!(config.recording.export.cursor_blink_period_ms, 500);
        assert_eq!(config.recording.export.cursor_color, "auto");
        assert_eq!(
            config.recording.export.cursor_profile,
            crate::RecordingExportCursorProfile::Auto
        );
        assert_eq!(config.recording.export.cursor_solid_after_activity_ms, None);
        assert_eq!(config.recording.export.cursor_solid_after_input_ms, None);
        assert_eq!(config.recording.export.cursor_solid_after_output_ms, None);
        assert_eq!(config.recording.export.cursor_solid_after_cursor_ms, None);
        assert_eq!(
            config.recording.export.cursor_paint_mode,
            crate::RecordingExportCursorPaintMode::Auto
        );
        assert_eq!(
            config.recording.export.cursor_text_mode,
            crate::RecordingExportCursorTextMode::Auto
        );
        assert_eq!(config.recording.export.cursor_bar_width_pct, 16);
        assert_eq!(config.recording.export.cursor_underline_height_pct, 12);
    }

    #[test]
    fn load_parses_recording_export_cursor_defaults() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[recording.export]\ncursor = 'on'\ncursor_shape = 'underline'\ncursor_blink = 'off'\ncursor_blink_period_ms = 650\ncursor_color = '#44aaee'\ncursor_profile = 'ghostty'\ncursor_solid_after_activity_ms = 900\ncursor_solid_after_input_ms = 910\ncursor_solid_after_output_ms = 920\ncursor_solid_after_cursor_ms = 930\ncursor_paint_mode = 'fill'\ncursor_text_mode = 'swap_fg_bg'\ncursor_bar_width_pct = 14\ncursor_underline_height_pct = 11\n",
        )
        .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config.recording.export.cursor,
            crate::RecordingExportCursorMode::On
        );
        assert_eq!(
            config.recording.export.cursor_shape,
            crate::RecordingExportCursorShape::Underline
        );
        assert_eq!(
            config.recording.export.cursor_blink,
            crate::RecordingExportCursorBlinkMode::Off
        );
        assert_eq!(config.recording.export.cursor_blink_period_ms, 650);
        assert_eq!(config.recording.export.cursor_color, "#44aaee");
        assert_eq!(
            config.recording.export.cursor_profile,
            crate::RecordingExportCursorProfile::Ghostty
        );
        assert_eq!(
            config.recording.export.cursor_solid_after_activity_ms,
            Some(900)
        );
        assert_eq!(
            config.recording.export.cursor_solid_after_input_ms,
            Some(910)
        );
        assert_eq!(
            config.recording.export.cursor_solid_after_output_ms,
            Some(920)
        );
        assert_eq!(
            config.recording.export.cursor_solid_after_cursor_ms,
            Some(930)
        );
        assert_eq!(
            config.recording.export.cursor_paint_mode,
            crate::RecordingExportCursorPaintMode::Fill
        );
        assert_eq!(
            config.recording.export.cursor_text_mode,
            crate::RecordingExportCursorTextMode::SwapFgBg
        );
        assert_eq!(config.recording.export.cursor_bar_width_pct, 14);
        assert_eq!(config.recording.export.cursor_underline_height_pct, 11);

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }
}
