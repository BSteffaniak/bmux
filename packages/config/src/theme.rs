//! Theme configuration for bmux
//!
//! This module provides theme configuration management for colors and styling.

use serde::{Deserialize, Serialize};

/// Theme configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Theme name
    pub name: String,
    /// Foreground color
    pub foreground: String,
    /// Background color
    pub background: String,
    /// Cursor color
    pub cursor: String,
    /// Selection background color
    pub selection_background: String,
    /// Border colors
    pub border: BorderColors,
    /// Status bar colors
    pub status: StatusColors,
}

/// Border color configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BorderColors {
    /// Active pane border color
    pub active: String,
    /// Inactive pane border color
    pub inactive: String,
}

/// Status bar color configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusColors {
    /// Status bar background
    pub background: String,
    /// Status bar foreground
    pub foreground: String,
    /// Active window indicator
    pub active_window: String,
    /// Current mode indicator
    pub mode_indicator: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            foreground: "#ffffff".to_string(),
            background: "#000000".to_string(),
            cursor: "#ffffff".to_string(),
            selection_background: "#404040".to_string(),
            border: BorderColors::default(),
            status: StatusColors::default(),
        }
    }
}

impl Default for BorderColors {
    fn default() -> Self {
        Self {
            active: "#00ff00".to_string(),
            inactive: "#808080".to_string(),
        }
    }
}

impl Default for StatusColors {
    fn default() -> Self {
        Self {
            background: "#1e1e1e".to_string(),
            foreground: "#ffffff".to_string(),
            active_window: "#00ff00".to_string(),
            mode_indicator: "#ffff00".to_string(),
        }
    }
}
