//! Key binding configuration for bmux
//!
//! This module provides key binding configuration management with support
//! for modal keybindings (Normal, Insert, Visual, Command modes).

use bmux_config_doc_derive::ConfigDoc;
use bmux_event_models::{KeyCode, KeyEvent};
use bmux_keybind::{RuntimeAction, action_to_name, parse_action};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MIN_TIMEOUT_MS: u64 = 50;
pub const MAX_TIMEOUT_MS: u64 = 5_000;

const PROFILE_FAST: &str = "fast";
const PROFILE_TRADITIONAL: &str = "traditional";
const PROFILE_SLOW: &str = "slow";

/// Keyboard shortcuts organized by scope.
///
/// Modal bindings are first-class and defined in `modes`.
/// Legacy prefix/runtime/global bindings remain available for compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "keybindings")]
#[serde(default)]
pub struct KeyBindingConfig {
    /// Initial interaction mode id. Mode ids are matched case-insensitively.
    pub initial_mode: String,
    /// First-class modal keymap definitions keyed by mode id.
    pub modes: BTreeMap<String, ModeBindingConfig>,
    /// Prefix key for runtime key chords (e.g. "ctrl+a"). All runtime
    /// bindings require pressing this key first. Legacy path only.
    pub prefix: String,
    /// Exact timeout in milliseconds for multi-stroke chord resolution.
    /// Takes precedence over `timeout_profile`. Valid range: 50-5000.
    pub timeout_ms: Option<u64>,
    /// Named timeout profile for multi-stroke chord resolution. Built-in
    /// profiles: fast (200ms), traditional (400ms), slow (800ms). Ignored
    /// when `timeout_ms` is set.
    pub timeout_profile: Option<String>,
    /// Override values for built-in timeout profiles or define custom ones.
    /// Keys are profile names, values are timeout in milliseconds.
    pub timeout_profiles: BTreeMap<String, u64>,
    /// Key bindings triggered after pressing the prefix key. Maps key
    /// chords to runtime action names.
    pub runtime: BTreeMap<String, String>,
    /// Key bindings that trigger without the prefix key. Use sparingly to
    /// avoid conflicts with pane input.
    pub global: BTreeMap<String, String>,
    /// Key bindings active in scrollback/copy mode. No prefix required
    /// unless the chord explicitly includes it.
    pub scroll: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[serde(default)]
pub struct ModeBindingConfig {
    /// Human-friendly label shown in attach status and hints.
    pub label: String,
    /// If true, unmatched keys are forwarded to the focused pane.
    pub passthrough: bool,
    /// Bindings active while this mode is selected.
    pub bindings: BTreeMap<String, String>,
}

impl Default for ModeBindingConfig {
    fn default() -> Self {
        Self {
            label: "Mode".to_string(),
            passthrough: false,
            bindings: BTreeMap::new(),
        }
    }
}

impl Default for KeyBindingConfig {
    fn default() -> Self {
        Self {
            initial_mode: "normal".to_string(),
            modes: default_modal_modes(),
            prefix: "ctrl+a".to_string(),
            timeout_ms: None,
            timeout_profile: None,
            timeout_profiles: BTreeMap::new(),
            runtime: default_runtime_bindings(),
            global: default_global_runtime_bindings(),
            scroll: default_scroll_bindings(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTimeout {
    Indefinite,
    Exact(u64),
    Profile { name: String, ms: u64 },
}

impl ResolvedTimeout {
    #[must_use]
    pub const fn timeout_ms(&self) -> Option<u64> {
        match self {
            Self::Indefinite => None,
            Self::Exact(ms) | Self::Profile { ms, .. } => Some(*ms),
        }
    }
}

fn default_global_runtime_bindings() -> BTreeMap<String, String> {
    let mut map = action_bindings(&[
        // Session navigation
        ("ctrl+o", RuntimeAction::SessionPrev),
        // Pane focus navigation
        ("alt+h", RuntimeAction::FocusLeft),
        ("alt+left", RuntimeAction::FocusLeft),
        ("alt+j", RuntimeAction::FocusDown),
        ("alt+down", RuntimeAction::FocusDown),
        ("alt+k", RuntimeAction::FocusUp),
        ("alt+up", RuntimeAction::FocusUp),
        ("alt+right", RuntimeAction::FocusRight),
        // Pane cycling
        ("ctrl+k", RuntimeAction::FocusPrev),
        ("ctrl+t", RuntimeAction::FocusPrev),
        ("alt+t", RuntimeAction::FocusNext),
        // Resize
        ("alt+plus", RuntimeAction::IncreaseSplit),
        ("alt+=", RuntimeAction::IncreaseSplit),
        ("alt+minus", RuntimeAction::DecreaseSplit),
        // New pane
        ("alt+n", RuntimeAction::SplitFocusedHorizontal),
    ]);
    // Window navigation via plugin (no-op if plugin not loaded)
    for i in 1..=9 {
        map.insert(
            format!("alt+{i}"),
            format!("plugin:bmux.windows:goto-window {i}"),
        );
    }
    map.insert(
        "alt+0".to_string(),
        "plugin:bmux.windows:goto-window 10".to_string(),
    );
    map.insert(
        "ctrl+h".to_string(),
        "plugin:bmux.windows:prev-window".to_string(),
    );
    map.insert(
        "ctrl+j".to_string(),
        "plugin:bmux.windows:prev-window".to_string(),
    );
    map.insert(
        "ctrl+left".to_string(),
        "plugin:bmux.windows:prev-window".to_string(),
    );
    map.insert(
        "ctrl+s".to_string(),
        "plugin:bmux.windows:next-window".to_string(),
    );
    map.insert(
        "ctrl+right".to_string(),
        "plugin:bmux.windows:next-window".to_string(),
    );
    map.insert(
        "ctrl+l".to_string(),
        "plugin:bmux.windows:last-window".to_string(),
    );
    map
}

fn default_runtime_bindings() -> BTreeMap<String, String> {
    let mut map = action_bindings(&[
        ("shift+c", RuntimeAction::NewSession),
        ("o", RuntimeAction::FocusNext),
        ("h", RuntimeAction::FocusLeft),
        ("l", RuntimeAction::FocusRight),
        ("k", RuntimeAction::FocusUp),
        ("j", RuntimeAction::FocusDown),
        ("arrow_left", RuntimeAction::FocusLeft),
        ("arrow_right", RuntimeAction::FocusRight),
        ("arrow_up", RuntimeAction::FocusUp),
        ("arrow_down", RuntimeAction::FocusDown),
        ("t", RuntimeAction::ToggleSplitDirection),
        ("%", RuntimeAction::SplitFocusedVertical),
        ("\"", RuntimeAction::SplitFocusedHorizontal),
        ("plus", RuntimeAction::IncreaseSplit),
        ("minus", RuntimeAction::DecreaseSplit),
        ("shift+h", RuntimeAction::ResizeLeft),
        ("shift+l", RuntimeAction::ResizeRight),
        ("shift+k", RuntimeAction::ResizeUp),
        ("shift+j", RuntimeAction::ResizeDown),
        ("shift+arrow_left", RuntimeAction::ResizeLeft),
        ("shift+arrow_right", RuntimeAction::ResizeRight),
        ("shift+arrow_up", RuntimeAction::ResizeUp),
        ("shift+arrow_down", RuntimeAction::ResizeDown),
        ("r", RuntimeAction::RestartFocusedPane),
        ("x", RuntimeAction::CloseFocusedPane),
        ("z", RuntimeAction::ZoomPane),
        ("?", RuntimeAction::ShowHelp),
        ("[", RuntimeAction::EnterScrollMode),
        ("]", RuntimeAction::ExitScrollMode),
        ("ctrl+y", RuntimeAction::ScrollUpLine),
        ("ctrl+e", RuntimeAction::ScrollDownLine),
        ("page_up", RuntimeAction::ScrollUpPage),
        ("page_down", RuntimeAction::ScrollDownPage),
        ("g", RuntimeAction::ScrollTop),
        ("shift+g", RuntimeAction::ScrollBottom),
        ("v", RuntimeAction::BeginSelection),
        ("d", RuntimeAction::Detach),
        ("q", RuntimeAction::Quit),
        // Session prev/next
        ("(", RuntimeAction::SessionPrev),
        (")", RuntimeAction::SessionNext),
    ]);
    // Plugin command: toggle last window (no-op if plugin not loaded)
    map.insert(
        "^".to_string(),
        "plugin:bmux.windows:last-window".to_string(),
    );
    map
}

fn default_scroll_bindings() -> BTreeMap<String, String> {
    action_bindings(&[
        ("escape", RuntimeAction::ExitScrollMode),
        ("ctrl+c", RuntimeAction::ExitScrollMode),
        ("ctrl+a ]", RuntimeAction::ExitScrollMode),
        ("enter", RuntimeAction::ConfirmScrollback),
        ("arrow_left", RuntimeAction::MoveCursorLeft),
        ("arrow_right", RuntimeAction::MoveCursorRight),
        ("arrow_up", RuntimeAction::MoveCursorUp),
        ("arrow_down", RuntimeAction::MoveCursorDown),
        ("h", RuntimeAction::MoveCursorLeft),
        ("l", RuntimeAction::MoveCursorRight),
        ("k", RuntimeAction::MoveCursorUp),
        ("j", RuntimeAction::MoveCursorDown),
        ("ctrl+y", RuntimeAction::ScrollUpLine),
        ("ctrl+e", RuntimeAction::ScrollDownLine),
        ("ctrl+b", RuntimeAction::ScrollUpPage),
        ("page_up", RuntimeAction::ScrollUpPage),
        ("page_down", RuntimeAction::ScrollDownPage),
        ("g", RuntimeAction::ScrollTop),
        ("shift+g", RuntimeAction::ScrollBottom),
        ("v", RuntimeAction::BeginSelection),
    ])
}

fn action_bindings(pairs: &[(&str, RuntimeAction)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, action)| ((*key).to_string(), action_to_name(action).to_string()))
        .collect()
}

impl KeyBindingConfig {
    #[must_use]
    pub fn canonical_mode_id(mode_id: &str) -> String {
        mode_id.trim().to_ascii_lowercase()
    }

    #[must_use]
    pub fn mode_id_eq(left: &str, right: &str) -> bool {
        Self::canonical_mode_id(left) == Self::canonical_mode_id(right)
    }

    /// Validate modal keybinding configuration.
    ///
    /// # Errors
    ///
    /// Returns an error string when modal configuration is invalid.
    pub fn validate_modes(&self) -> Result<(), String> {
        if self.modes.is_empty() {
            return Err("keybindings.modes must define at least one mode".to_string());
        }

        let mut canonical_modes = BTreeMap::new();
        for mode_id in self.modes.keys() {
            let canonical = Self::canonical_mode_id(mode_id);
            if canonical.is_empty() {
                return Err("keybindings.modes contains an empty mode id".to_string());
            }
            if let Some(existing) = canonical_modes.insert(canonical.clone(), mode_id.clone()) {
                return Err(format!(
                    "keybindings.modes has case-insensitive duplicate mode ids '{existing}' and '{mode_id}'"
                ));
            }
        }

        let initial_mode = Self::canonical_mode_id(&self.initial_mode);
        if initial_mode.is_empty() {
            return Err("keybindings.initial_mode must not be empty".to_string());
        }
        if !canonical_modes.contains_key(&initial_mode) {
            return Err(format!(
                "keybindings.initial_mode '{}' is not defined in keybindings.modes",
                self.initial_mode
            ));
        }

        for (mode_id, mode_config) in &self.modes {
            for action_name in mode_config.bindings.values() {
                let action = parse_action(action_name).map_err(|error| {
                    format!(
                        "keybindings.modes.{mode_id}.bindings contains invalid action '{action_name}' ({error})"
                    )
                })?;
                if let RuntimeAction::EnterMode(target_mode) = action
                    && !canonical_modes.contains_key(&target_mode)
                {
                    return Err(format!(
                        "keybindings.modes.{mode_id}.bindings enter_mode target '{target_mode}' is not defined"
                    ));
                }
            }
        }

        Ok(())
    }

    #[must_use]
    pub fn built_in_timeout_profiles() -> BTreeMap<String, u64> {
        BTreeMap::from([
            (PROFILE_FAST.to_string(), 200),
            (PROFILE_TRADITIONAL.to_string(), 400),
            (PROFILE_SLOW.to_string(), 800),
        ])
    }

    #[must_use]
    pub fn merged_timeout_profiles(&self) -> BTreeMap<String, u64> {
        let mut profiles = Self::built_in_timeout_profiles();
        for (name, value) in &self.timeout_profiles {
            profiles.insert(name.clone(), *value);
        }
        profiles
    }

    /// Resolve the effective timeout for multi-stroke chord resolution.
    ///
    /// # Errors
    ///
    /// Returns an error when `timeout_ms` is out of range, `timeout_profile`
    /// references an unknown or empty profile, or a custom profile value is
    /// out of range.
    pub fn resolve_timeout(&self) -> Result<ResolvedTimeout, String> {
        if let Some(timeout_ms) = self.timeout_ms {
            validate_timeout_value(timeout_ms, "keybindings.timeout_ms")?;
            return Ok(ResolvedTimeout::Exact(timeout_ms));
        }

        let profiles = self.merged_timeout_profiles();
        for (name, value) in &self.timeout_profiles {
            validate_timeout_value(*value, &format!("keybindings.timeout_profiles.{name}"))?;
        }

        let Some(profile_name) = self.timeout_profile.as_deref() else {
            return Ok(ResolvedTimeout::Indefinite);
        };

        if profile_name.trim().is_empty() {
            return Err("keybindings.timeout_profile must not be empty".to_string());
        }

        let Some(timeout_ms) = profiles.get(profile_name) else {
            return Err(format!(
                "keybindings.timeout_profile references unknown profile '{profile_name}'"
            ));
        };
        validate_timeout_value(
            *timeout_ms,
            &format!("keybindings.timeout_profiles.{profile_name}"),
        )?;
        Ok(ResolvedTimeout::Profile {
            name: profile_name.to_string(),
            ms: *timeout_ms,
        })
    }

    #[must_use]
    pub fn mode_config(&self, mode_id: &str) -> Option<&ModeBindingConfig> {
        let canonical = Self::canonical_mode_id(mode_id);
        self.modes
            .iter()
            .find(|(configured_mode_id, _)| {
                Self::canonical_mode_id(configured_mode_id) == canonical
            })
            .map(|(_, config)| config)
    }
}

fn validate_timeout_value(value: u64, field: &str) -> Result<(), String> {
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&value) {
        return Err(format!(
            "{field} must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS}"
        ));
    }
    Ok(())
}

/// Convert a key code to a string representation for binding lookup
#[must_use]
pub fn keycode_to_string(key: &KeyCode) -> String {
    match key {
        KeyCode::Char(c) => {
            if c.is_ascii_control() {
                format!("C-{}", (*c as u8 + b'@') as char)
            } else {
                c.to_string()
            }
        }
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Escape => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Space => "Space".to_string(),
    }
}

/// Convert a key event to a string representation for binding lookup
#[must_use]
pub fn key_event_to_string(event: &KeyEvent) -> String {
    let mut result = String::new();

    if event.modifiers.ctrl {
        result.push_str("C-");
    }
    if event.modifiers.alt {
        result.push_str("M-");
    }
    if event.modifiers.shift {
        result.push_str("S-");
    }
    if event.modifiers.super_key {
        result.push_str("Super-");
    }

    result.push_str(&keycode_to_string(&event.code));
    result
}

fn default_normal_bindings() -> BTreeMap<String, String> {
    let mut bindings = default_runtime_bindings();
    bindings.insert("i".to_string(), "enter_mode insert".to_string());
    bindings.insert(
        "c".to_string(),
        "plugin:bmux.windows:new-window".to_string(),
    );
    bindings
}

fn default_insert_bindings() -> BTreeMap<String, String> {
    let mut bindings = BTreeMap::new();
    bindings.insert("escape".to_string(), "enter_mode normal".to_string());
    bindings
}

fn default_modal_modes() -> BTreeMap<String, ModeBindingConfig> {
    BTreeMap::from([
        (
            "normal".to_string(),
            ModeBindingConfig {
                label: "NORMAL".to_string(),
                passthrough: false,
                bindings: default_normal_bindings(),
            },
        ),
        (
            "insert".to_string(),
            ModeBindingConfig {
                label: "INSERT".to_string(),
                passthrough: true,
                bindings: default_insert_bindings(),
            },
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::{KeyBindingConfig, ModeBindingConfig};
    use std::collections::BTreeMap;

    #[test]
    fn validate_modes_rejects_case_insensitive_duplicate_mode_ids() {
        let mut config = KeyBindingConfig {
            initial_mode: "normal".to_string(),
            ..KeyBindingConfig::default()
        };
        config.modes = BTreeMap::from([
            ("normal".to_string(), ModeBindingConfig::default()),
            ("NORMAL".to_string(), ModeBindingConfig::default()),
        ]);

        let error = config
            .validate_modes()
            .expect_err("duplicate mode ids should fail");
        assert!(error.contains("duplicate mode ids"));
    }

    #[test]
    fn validate_modes_rejects_unknown_enter_mode_target() {
        let config = KeyBindingConfig {
            modes: BTreeMap::from([(
                "normal".to_string(),
                ModeBindingConfig {
                    label: "NORMAL".to_string(),
                    passthrough: false,
                    bindings: BTreeMap::from([("i".to_string(), "enter_mode typing".to_string())]),
                },
            )]),
            ..KeyBindingConfig::default()
        };

        let error = config
            .validate_modes()
            .expect_err("unknown mode target should fail");
        assert!(error.contains("enter_mode target 'typing'"));
    }

    #[test]
    fn validate_modes_accepts_case_insensitive_initial_mode_lookup() {
        let config = KeyBindingConfig {
            initial_mode: "INSERT".to_string(),
            ..KeyBindingConfig::default()
        };
        assert!(config.validate_modes().is_ok());
    }
}
