#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Configuration management for bmux terminal multiplexer
//!
//! This crate provides configuration loading, validation, and management
//! for the bmux terminal multiplexer system.

use bmux_event::Mode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

pub mod keybind;
pub mod paths;
pub mod theme;

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

/// Main configuration structure for bmux
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BmuxConfig {
    /// General settings
    pub general: GeneralConfig,
    /// Appearance settings
    pub appearance: AppearanceConfig,
    /// Behavior settings
    pub behavior: BehaviorConfig,
    /// Multi-client settings
    pub multi_client: MultiClientConfig,
    /// Key bindings
    pub keybindings: KeyBindingConfig,
    /// Plugin settings
    pub plugins: PluginConfig,
    /// Status bar settings
    pub status_bar: StatusBarConfig,
}

/// General configuration options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Default mode when starting bmux
    pub default_mode: Mode,
    /// Enable mouse support
    pub mouse_support: bool,
    /// Default shell to use in new panes
    pub default_shell: Option<String>,
    /// Maximum number of scrollback lines
    pub scrollback_limit: usize,
    /// Server socket timeout in milliseconds
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

/// Appearance configuration options
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppearanceConfig {
    /// Theme name
    pub theme: String,
    /// Status bar position
    pub status_position: StatusPosition,
    /// Pane border style
    pub pane_border_style: BorderStyle,
    /// Show pane titles
    pub show_pane_titles: bool,
    /// Window title format
    pub window_title_format: String,
}

/// Behavior configuration options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct BehaviorConfig {
    /// Aggressively resize windows when clients disconnect
    pub aggressive_resize: bool,
    /// Show visual activity indicators
    pub visual_activity: bool,
    /// Bell action behavior
    pub bell_action: BellAction,
    /// Automatically rename windows based on running command
    pub automatic_rename: bool,
    /// Exit bmux when no sessions remain
    pub exit_empty: bool,
    /// Restore and persist last local CLI runtime layout
    pub restore_last_layout: bool,
    /// Confirm before destructive quit that clears persisted local runtime state
    pub confirm_quit_destroy: bool,
    /// Terminal type to expose to pane processes as TERM
    pub pane_term: String,
    /// Enable protocol query/reply tracing in runtime
    pub protocol_trace_enabled: bool,
    /// Maximum in-memory protocol trace events to retain
    pub protocol_trace_capacity: usize,
    /// Auto-install policy for bmux terminfo when missing
    pub terminfo_auto_install: TerminfoAutoInstall,
    /// Cooldown before prompting again after declining install
    pub terminfo_prompt_cooldown_days: u64,
    /// Behavior when the running server build differs from the current CLI build
    pub stale_build_action: StaleBuildAction,
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
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StaleBuildAction {
    #[default]
    Error,
    Warn,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminfoAutoInstall {
    #[default]
    Ask,
    Always,
    Never,
}

/// Multi-client specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MultiClientConfig {
    /// Allow clients to have independent views of the same session
    pub allow_independent_views: bool,
    /// Default follow mode for new clients
    pub default_follow_mode: bool,
    /// Maximum clients per session (0 = unlimited)
    pub max_clients_per_session: usize,
    /// Sync client modes by default
    pub sync_client_modes: bool,
}

/// Plugin configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PluginConfig {
    /// Enabled plugins
    pub enabled: Vec<String>,
    /// Plugin-specific settings
    pub settings: BTreeMap<String, toml::Value>,
}

/// Status bar configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StatusBarConfig {
    /// Left side format
    pub left: String,
    /// Right side format
    pub right: String,
    /// Update interval in seconds
    pub update_interval: u64,
    /// Show session name
    pub show_session_name: bool,
    /// Show window list
    pub show_window_list: bool,
    /// Show current mode
    pub show_mode: bool,
}

/// Status bar position
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StatusPosition {
    Top,
    #[default]
    Bottom,
    Off,
}

/// Border style for panes
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BellAction {
    None,
    #[default]
    Any,
    Current,
    Other,
}

impl BmuxConfig {
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

        Ok(())
    }

    fn sanitize_invalid_values(&mut self) -> Vec<String> {
        let general_defaults = GeneralConfig::default();
        let keybind_defaults = KeyBindingConfig::default();
        let behavior_defaults = BehaviorConfig::default();
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
            self.keybindings.timeout_profiles = keybind_defaults.timeout_profiles.clone();
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
}
