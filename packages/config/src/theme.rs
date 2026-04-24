//! Theme configuration for bmux
//!
//! This module provides theme configuration management for colors and styling.

use bmux_config_doc_derive::ConfigDoc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Theme configuration
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "theme_file")]
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
    #[config_doc(nested)]
    pub border: BorderColors,
    /// Status bar colors
    #[config_doc(nested)]
    pub status: StatusColors,
    /// Opaque per-plugin theme extensions.
    ///
    /// Keys are plugin IDs (e.g. `"bmux.my-plugin"`); values are
    /// plugin-owned TOML records that core stores but does not
    /// interpret. Each plugin's `ConfigExtensionValidator` typed
    /// service parses its slice against a BPDL-declared schema at
    /// load time and surfaces errors through `bmux config validate`.
    ///
    /// Matches the existing `plugins.settings` idiom for non-theme
    /// plugin configuration; the same round-trip-preserving
    /// convention applies here.
    #[serde(
        default,
        rename = "plugins",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub plugins: BTreeMap<String, toml::Value>,
}

/// Border color configuration
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[serde(default)]
pub struct BorderColors {
    /// Active pane border color
    pub active: String,
    /// Inactive pane border color
    pub inactive: String,
}

/// Status bar color configuration
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
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
            plugins: BTreeMap::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_plugins_extension_round_trips_opaque_toml() {
        let theme_text = r##"
name = "hacker"
foreground = "#39ff14"
background = "#000000"
cursor = "#39ff14"
selection_background = "#004400"

[border]
active = "#39ff14"
inactive = "#1a4d1a"

[status]
background = "#000000"
foreground = "#39ff14"
active_window = "#00ff00"
mode_indicator = "#ffff00"

[plugins."bmux.example"]
whatever = "is fine"

[plugins."bmux.example".nested]
key = "value"
"##;
        let parsed: ThemeConfig = toml::from_str(theme_text).expect("parse");
        let example_ext = parsed
            .plugins
            .get("bmux.example")
            .expect("example extension present");
        // Core did not interpret the payload — it's round-tripped as
        // an opaque TOML value keyed by plugin id.
        let table = example_ext.as_table().expect("table");
        assert_eq!(
            table.get("whatever").and_then(|v| v.as_str()),
            Some("is fine"),
        );
        assert!(
            table.get("nested").and_then(|v| v.as_table()).is_some(),
            "nested tables should survive round-trip",
        );
    }

    #[test]
    fn theme_without_plugins_extension_parses_cleanly() {
        let theme_text = r#"
name = "spare"
"#;
        let parsed: ThemeConfig = toml::from_str(theme_text).expect("parse");
        assert!(parsed.plugins.is_empty());
    }
}
