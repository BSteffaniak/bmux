#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin_sdk::PluginEventKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    pub modes: BTreeMap<String, RuntimeAppearancePatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeAppearancePatch {
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub cursor: Option<String>,
    pub selection_background: Option<String>,
    pub border: RuntimeBorderAppearancePatch,
    pub status: RuntimeStatusAppearancePatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeBorderAppearance {
    pub active: String,
    pub inactive: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeBorderAppearancePatch {
    pub active: Option<String>,
    pub inactive: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeStatusAppearance {
    pub background: String,
    pub foreground: String,
    pub active_window: String,
    pub mode_indicator: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeStatusAppearancePatch {
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub active_window: Option<String>,
    pub mode_indicator: Option<String>,
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
            modes: BTreeMap::new(),
        }
    }
}

impl RuntimeAppearance {
    #[must_use]
    pub fn with_patch(mut self, patch: &RuntimeAppearancePatch) -> Self {
        self.apply_patch(patch);
        self
    }

    pub fn apply_patch(&mut self, patch: &RuntimeAppearancePatch) {
        if let Some(value) = &patch.foreground {
            self.foreground.clone_from(value);
        }
        if let Some(value) = &patch.background {
            self.background.clone_from(value);
        }
        if let Some(value) = &patch.cursor {
            self.cursor.clone_from(value);
        }
        if let Some(value) = &patch.selection_background {
            self.selection_background.clone_from(value);
        }
        self.border.apply_patch(&patch.border);
        self.status.apply_patch(&patch.status);
    }

    #[must_use]
    pub fn for_mode(&self, mode_id: &str) -> Self {
        self.modes
            .get(mode_id)
            .map_or_else(|| self.clone(), |patch| self.clone().with_patch(patch))
    }
}

impl RuntimeAppearancePatch {
    pub fn merge(&mut self, other: &Self) {
        if other.foreground.is_some() {
            self.foreground.clone_from(&other.foreground);
        }
        if other.background.is_some() {
            self.background.clone_from(&other.background);
        }
        if other.cursor.is_some() {
            self.cursor.clone_from(&other.cursor);
        }
        if other.selection_background.is_some() {
            self.selection_background
                .clone_from(&other.selection_background);
        }
        self.border.merge(&other.border);
        self.status.merge(&other.status);
    }
}

impl RuntimeBorderAppearance {
    fn apply_patch(&mut self, patch: &RuntimeBorderAppearancePatch) {
        if let Some(value) = &patch.active {
            self.active.clone_from(value);
        }
        if let Some(value) = &patch.inactive {
            self.inactive.clone_from(value);
        }
    }
}

impl RuntimeBorderAppearancePatch {
    fn merge(&mut self, other: &Self) {
        if other.active.is_some() {
            self.active.clone_from(&other.active);
        }
        if other.inactive.is_some() {
            self.inactive.clone_from(&other.inactive);
        }
    }
}

impl RuntimeStatusAppearance {
    fn apply_patch(&mut self, patch: &RuntimeStatusAppearancePatch) {
        if let Some(value) = &patch.background {
            self.background.clone_from(value);
        }
        if let Some(value) = &patch.foreground {
            self.foreground.clone_from(value);
        }
        if let Some(value) = &patch.active_window {
            self.active_window.clone_from(value);
        }
        if let Some(value) = &patch.mode_indicator {
            self.mode_indicator.clone_from(value);
        }
    }
}

impl RuntimeStatusAppearancePatch {
    fn merge(&mut self, other: &Self) {
        if other.background.is_some() {
            self.background.clone_from(&other.background);
        }
        if other.foreground.is_some() {
            self.foreground.clone_from(&other.foreground);
        }
        if other.active_window.is_some() {
            self.active_window.clone_from(&other.active_window);
        }
        if other.mode_indicator.is_some() {
            self.mode_indicator.clone_from(&other.mode_indicator);
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
