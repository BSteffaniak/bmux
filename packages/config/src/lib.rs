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

pub use keybind::KeyBindingConfig;
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
        }
    }
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
            // Create a default config file if it doesn't exist
            let default_config = Self::default();
            default_config.save_to_path(path)?;
            return Ok(default_config);
        }

        let contents = std::fs::read_to_string(path).map_err(|e| ConfigError::ReadError {
            error: e.to_string(),
        })?;

        let config: Self = toml::from_str(&contents).map_err(|e| ConfigError::ParseError {
            error: e.to_string(),
        })?;

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

        if self.keybindings.timeout_ms == 0 {
            return Err(ConfigError::InvalidValue {
                field: "keybindings.timeout_ms".to_string(),
                value: "0".to_string(),
            });
        }

        Ok(())
    }

    /// Merge another configuration into this one
    pub fn merge(&mut self, other: Self) {
        // This is a simple merge that overwrites values
        // In a real implementation, you might want more sophisticated merging
        *self = other;
    }
}
