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
