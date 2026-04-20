#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

// ============================================================================
// Modal System
// ============================================================================

/// Modes that bmux can operate in
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "SCREAMING_SNAKE_CASE"))]
pub enum Mode {
    #[default]
    Normal, // Default mode - no prefix keys needed
    Insert,  // Terminal interaction mode
    Visual,  // Text selection mode
    Command, // Command entry mode
}

impl Mode {
    /// Check if transition from current mode to target mode is valid
    #[must_use]
    pub const fn can_transition_to(&self, target: Self) -> bool {
        match (self, target) {
            // All other modes can only go to Normal (except Visual->Command)
            (Self::Insert | Self::Command, Self::Normal)
            | (Self::Visual, Self::Normal | Self::Command)
            | (Self::Normal, _) => true,
            _ => false,
        }
    }

    /// Get the default next mode when exiting current mode
    #[must_use]
    pub const fn default_exit_mode(&self) -> Self {
        // All modes default to Normal when exiting
        Self::Normal
    }
}

// ============================================================================
// Input Events
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum KeyCode {
    // Letter keys
    Char(char),

    // Special keys
    Enter,
    Tab,
    Backspace,
    Delete,
    Escape,
    Space,

    // Arrow keys
    Up,
    Down,
    Left,
    Right,

    // Function keys
    F(u8), // F1-F12

    // Other keys
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[allow(clippy::struct_excessive_bools)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool, // Windows/Cmd key
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyEvent {
    /// Create a new key event with the specified key code
    #[must_use]
    pub fn new(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::default(),
        }
    }

    /// Add modifiers to this key event
    #[must_use]
    pub const fn with_modifiers(mut self, modifiers: KeyModifiers) -> Self {
        self.modifiers = modifiers;
        self
    }

    /// Add Ctrl modifier to this key event
    #[must_use]
    pub const fn ctrl(mut self) -> Self {
        self.modifiers.ctrl = true;
        self
    }

    /// Add Alt modifier to this key event
    #[must_use]
    pub const fn alt(mut self) -> Self {
        self.modifiers.alt = true;
        self
    }

    /// Add Shift modifier to this key event
    #[must_use]
    pub const fn shift(mut self) -> Self {
        self.modifiers.shift = true;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum MouseEventType {
    Down,
    Up,
    Click,
    DoubleClick,
    Drag,
    Move,
    Scroll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MouseEvent {
    pub event_type: MouseEventType,
    pub button: Option<MouseButton>,
    pub x: u16,
    pub y: u16,
    pub modifiers: KeyModifiers,
}
