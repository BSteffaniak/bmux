#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin_sdk::PluginEventKind;
use serde::{Deserialize, Serialize};

pub const RUNTIME_APPEARANCE_STATE_KIND: PluginEventKind =
    PluginEventKind::from_static("bmux.runtime/appearance");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeAppearance {
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    pub selection_background: String,
    pub border: RuntimeBorderAppearance,
    pub status: RuntimeStatusAppearance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeBorderAppearance {
    pub active: String,
    pub inactive: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeStatusAppearance {
    pub background: String,
    pub foreground: String,
    pub active_window: String,
    pub mode_indicator: String,
}

impl Default for RuntimeAppearance {
    fn default() -> Self {
        Self {
            foreground: "#ffffff".to_string(),
            background: "#000000".to_string(),
            cursor: "#ffffff".to_string(),
            selection_background: "#404040".to_string(),
            border: RuntimeBorderAppearance::default(),
            status: RuntimeStatusAppearance::default(),
        }
    }
}

impl Default for RuntimeBorderAppearance {
    fn default() -> Self {
        Self {
            active: "#00ff00".to_string(),
            inactive: "#808080".to_string(),
        }
    }
}

impl Default for RuntimeStatusAppearance {
    fn default() -> Self {
        Self {
            background: "#1e1e1e".to_string(),
            foreground: "#ffffff".to_string(),
            active_window: "#00ff00".to_string(),
            mode_indicator: "#ffff00".to_string(),
        }
    }
}
