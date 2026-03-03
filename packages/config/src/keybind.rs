//! Key binding configuration for bmux
//!
//! This module provides key binding configuration management with support
//! for modal keybindings (Normal, Insert, Visual, Command modes).

use bmux_event::{KeyCode, KeyEvent, Mode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Key binding configuration for all modes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyBindingConfig {
    /// Prefix key used for runtime key chords (e.g. "ctrl+a")
    pub prefix: String,
    /// Timeout for multi-stroke chord resolution
    pub timeout_ms: u64,
    /// Runtime action bindings after prefix
    pub runtime: BTreeMap<String, String>,
    /// Global runtime action bindings (no prefix required)
    pub global: BTreeMap<String, String>,
    /// Normal mode key bindings
    pub normal: BTreeMap<String, String>,
    /// Insert mode key bindings (usually just Escape)
    pub insert: BTreeMap<String, String>,
    /// Visual mode key bindings
    pub visual: BTreeMap<String, String>,
    /// Command mode key bindings
    pub command: BTreeMap<String, String>,
}

impl Default for KeyBindingConfig {
    fn default() -> Self {
        Self {
            prefix: "ctrl+a".to_string(),
            timeout_ms: 400,
            runtime: default_runtime_bindings(),
            global: default_global_runtime_bindings(),
            normal: default_normal_bindings(),
            insert: default_insert_bindings(),
            visual: default_visual_bindings(),
            command: default_command_bindings(),
        }
    }
}

fn default_global_runtime_bindings() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn default_runtime_bindings() -> BTreeMap<String, String> {
    let mut bindings = BTreeMap::new();
    bindings.insert("o".to_string(), "focus_next_pane".to_string());
    bindings.insert("h".to_string(), "focus_left_pane".to_string());
    bindings.insert("l".to_string(), "focus_right_pane".to_string());
    bindings.insert("k".to_string(), "focus_up_pane".to_string());
    bindings.insert("j".to_string(), "focus_down_pane".to_string());
    bindings.insert("arrow_left".to_string(), "focus_left_pane".to_string());
    bindings.insert("arrow_right".to_string(), "focus_right_pane".to_string());
    bindings.insert("arrow_up".to_string(), "focus_up_pane".to_string());
    bindings.insert("arrow_down".to_string(), "focus_down_pane".to_string());
    bindings.insert("t".to_string(), "toggle_split_direction".to_string());
    bindings.insert("%".to_string(), "split_focused_vertical".to_string());
    bindings.insert("\"".to_string(), "split_focused_horizontal".to_string());
    bindings.insert("plus".to_string(), "increase_split".to_string());
    bindings.insert("minus".to_string(), "decrease_split".to_string());
    bindings.insert("r".to_string(), "restart_focused_pane".to_string());
    bindings.insert("x".to_string(), "close_focused_pane".to_string());
    bindings.insert("?".to_string(), "show_help".to_string());
    bindings.insert("q".to_string(), "quit".to_string());
    bindings
}

impl KeyBindingConfig {
    /// Get key bindings for a specific mode
    #[must_use]
    pub const fn get_bindings_for_mode(&self, mode: Mode) -> &BTreeMap<String, String> {
        match mode {
            Mode::Normal => &self.normal,
            Mode::Insert => &self.insert,
            Mode::Visual => &self.visual,
            Mode::Command => &self.command,
        }
    }

    /// Get a command for a key in a specific mode
    #[must_use]
    pub fn get_command(&self, mode: Mode, key: &str) -> Option<&str> {
        self.get_bindings_for_mode(mode)
            .get(key)
            .map(String::as_str)
    }

    /// Add or update a key binding for a specific mode
    pub fn set_binding(&mut self, mode: Mode, key: String, command: String) {
        let bindings = match mode {
            Mode::Normal => &mut self.normal,
            Mode::Insert => &mut self.insert,
            Mode::Visual => &mut self.visual,
            Mode::Command => &mut self.command,
        };
        bindings.insert(key, command);
    }

    /// Remove a key binding for a specific mode
    pub fn remove_binding(&mut self, mode: Mode, key: &str) -> Option<String> {
        let bindings = match mode {
            Mode::Normal => &mut self.normal,
            Mode::Insert => &mut self.insert,
            Mode::Visual => &mut self.visual,
            Mode::Command => &mut self.command,
        };
        bindings.remove(key)
    }
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

/// Default key bindings for Normal mode
fn default_normal_bindings() -> BTreeMap<String, String> {
    let mut bindings = BTreeMap::new();

    // Navigation
    bindings.insert("h".to_string(), "move-pane-left".to_string());
    bindings.insert("j".to_string(), "move-pane-down".to_string());
    bindings.insert("k".to_string(), "move-pane-up".to_string());
    bindings.insert("l".to_string(), "move-pane-right".to_string());

    // Window management
    bindings.insert("c".to_string(), "new-window".to_string());
    bindings.insert("n".to_string(), "next-window".to_string());
    bindings.insert("p".to_string(), "prev-window".to_string());
    bindings.insert("&".to_string(), "kill-window".to_string());

    // Pane management
    bindings.insert("\"".to_string(), "split-horizontal".to_string());
    bindings.insert("%".to_string(), "split-vertical".to_string());
    bindings.insert("x".to_string(), "kill-pane".to_string());
    bindings.insert("z".to_string(), "zoom-pane".to_string());

    // Mode switching
    bindings.insert("i".to_string(), "enter-insert-mode".to_string());
    bindings.insert("v".to_string(), "enter-visual-mode".to_string());
    bindings.insert(":".to_string(), "enter-command-mode".to_string());

    // Other commands
    bindings.insert("f".to_string(), "fuzzy-find".to_string());
    bindings.insert("/".to_string(), "search".to_string());
    bindings.insert("d".to_string(), "detach".to_string());
    bindings.insert("r".to_string(), "refresh".to_string());
    bindings.insert("?".to_string(), "show-help".to_string());

    // Resize
    bindings.insert("H".to_string(), "resize-pane-left".to_string());
    bindings.insert("J".to_string(), "resize-pane-down".to_string());
    bindings.insert("K".to_string(), "resize-pane-up".to_string());
    bindings.insert("L".to_string(), "resize-pane-right".to_string());

    bindings
}

/// Default key bindings for Insert mode
fn default_insert_bindings() -> BTreeMap<String, String> {
    let mut bindings = BTreeMap::new();

    // Only Escape to return to Normal mode
    bindings.insert("Esc".to_string(), "enter-normal-mode".to_string());

    bindings
}

/// Default key bindings for Visual mode
fn default_visual_bindings() -> BTreeMap<String, String> {
    let mut bindings = BTreeMap::new();

    // Basic movement (same as Normal mode)
    bindings.insert("h".to_string(), "move-left".to_string());
    bindings.insert("j".to_string(), "move-down".to_string());
    bindings.insert("k".to_string(), "move-up".to_string());
    bindings.insert("l".to_string(), "move-right".to_string());

    // Text selection commands
    bindings.insert("y".to_string(), "copy-selection".to_string());
    bindings.insert("d".to_string(), "cut-selection".to_string());

    // Mode switching
    bindings.insert("Esc".to_string(), "enter-normal-mode".to_string());
    bindings.insert(":".to_string(), "enter-command-mode".to_string());

    bindings
}

/// Default key bindings for Command mode
fn default_command_bindings() -> BTreeMap<String, String> {
    let mut bindings = BTreeMap::new();

    // Return to Normal mode
    bindings.insert("Esc".to_string(), "enter-normal-mode".to_string());
    bindings.insert("Enter".to_string(), "execute-command".to_string());

    // Basic editing
    bindings.insert("Backspace".to_string(), "backspace".to_string());
    bindings.insert("C-w".to_string(), "delete-word".to_string());
    bindings.insert("C-u".to_string(), "clear-line".to_string());

    bindings
}
