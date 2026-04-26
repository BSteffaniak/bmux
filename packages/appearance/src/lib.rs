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
    pub content_effects: BTreeMap<String, RuntimeContentEffect>,
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
    pub content_effects: BTreeMap<String, RuntimeContentEffectPatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeContentEffect {
    pub enabled: bool,
    pub scope: RuntimeContentEffectScope,
    pub when_bg: RuntimeContentEffectBgPredicate,
    pub background_blend: Option<RuntimeContentBlend>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeContentEffectPatch {
    pub enabled: Option<bool>,
    pub scope: Option<RuntimeContentEffectScope>,
    pub when_bg: Option<RuntimeContentEffectBgPredicate>,
    pub background_blend: RuntimeContentBlendPatch,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeContentEffectScope {
    #[default]
    Cells,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeContentEffectBgPredicate {
    #[default]
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeContentBlend {
    pub color: String,
    pub amount_permille: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeContentBlendPatch {
    pub color: Option<String>,
    pub amount_permille: Option<u16>,
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
            content_effects: BTreeMap::new(),
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
        for (name, effect_patch) in &patch.content_effects {
            self.content_effects
                .entry(name.clone())
                .or_default()
                .apply_patch(effect_patch);
        }
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
        for (name, effect) in &other.content_effects {
            self.content_effects
                .entry(name.clone())
                .or_default()
                .merge(effect);
        }
    }
}

impl Default for RuntimeContentEffect {
    fn default() -> Self {
        Self {
            enabled: true,
            scope: RuntimeContentEffectScope::default(),
            when_bg: RuntimeContentEffectBgPredicate::default(),
            background_blend: None,
        }
    }
}

impl RuntimeContentEffect {
    pub fn apply_patch(&mut self, patch: &RuntimeContentEffectPatch) {
        if let Some(value) = patch.enabled {
            self.enabled = value;
        }
        if let Some(value) = patch.scope {
            self.scope = value;
        }
        if let Some(value) = patch.when_bg {
            self.when_bg = value;
        }
        if patch.background_blend.has_values() {
            self.background_blend
                .get_or_insert_with(RuntimeContentBlend::default)
                .apply_patch(&patch.background_blend);
        }
    }
}

impl RuntimeContentEffectPatch {
    pub fn merge(&mut self, other: &Self) {
        if other.enabled.is_some() {
            self.enabled = other.enabled;
        }
        if other.scope.is_some() {
            self.scope = other.scope;
        }
        if other.when_bg.is_some() {
            self.when_bg = other.when_bg;
        }
        self.background_blend.merge(&other.background_blend);
    }
}

impl Default for RuntimeContentBlend {
    fn default() -> Self {
        Self {
            color: "#000000".to_string(),
            amount_permille: 0,
        }
    }
}

impl RuntimeContentBlend {
    fn apply_patch(&mut self, patch: &RuntimeContentBlendPatch) {
        if let Some(value) = &patch.color {
            self.color.clone_from(value);
        }
        if let Some(value) = patch.amount_permille {
            self.amount_permille = value.min(1000);
        }
    }
}

impl RuntimeContentBlendPatch {
    const fn has_values(&self) -> bool {
        self.color.is_some() || self.amount_permille.is_some()
    }

    fn merge(&mut self, other: &Self) {
        if other.color.is_some() {
            self.color.clone_from(&other.color);
        }
        if other.amount_permille.is_some() {
            self.amount_permille = other.amount_permille;
        }
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
