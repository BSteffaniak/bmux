#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_session_models::{ClientId, PaneId, SessionId};
use bmux_terminal_models::PaneSize;
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

// ============================================================================
// Events
// ============================================================================

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum SessionEvent {
    Created {
        session_id: SessionId,
        name: Option<String>,
    },
    Destroyed {
        session_id: SessionId,
    },
    ClientAttached {
        session_id: SessionId,
        client_id: ClientId,
    },
    ClientDetached {
        session_id: SessionId,
        client_id: ClientId,
    },
    Renamed {
        session_id: SessionId,
        old_name: Option<String>,
        new_name: Option<String>,
    },
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PaneEvent {
    Created {
        pane_id: PaneId,
        size: PaneSize,
    },
    Destroyed {
        pane_id: PaneId,
    },
    Resized {
        pane_id: PaneId,
        old_size: PaneSize,
        new_size: PaneSize,
    },
    TitleChanged {
        pane_id: PaneId,
        old_title: Option<String>,
        new_title: Option<String>,
    },
    WorkingDirectoryChanged {
        pane_id: PaneId,
        old_dir: Option<String>,
        new_dir: Option<String>,
    },
    ShellCommandChanged {
        pane_id: PaneId,
        old_cmd: Option<String>,
        new_cmd: Option<String>,
    },
    Activated {
        pane_id: PaneId,
    },
    Deactivated {
        pane_id: PaneId,
    },
    Output {
        pane_id: PaneId,
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ClientEvent {
    Connected {
        client_id: ClientId,
        independent_view: bool,
    },
    Disconnected {
        client_id: ClientId,
    },
    AttachedToSession {
        client_id: ClientId,
        session_id: SessionId,
    },
    DetachedFromSession {
        client_id: ClientId,
        session_id: SessionId,
    },
    ModeChanged {
        client_id: ClientId,
        old_mode: Mode,
        new_mode: Mode,
    },
    FollowingChanged {
        client_id: ClientId,
        following: Option<ClientId>,
    },
    ViewChanged {
        client_id: ClientId,
        session_id: SessionId,
        pane_id: PaneId,
    },
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum InputEvent {
    Key {
        client_id: ClientId,
        event: KeyEvent,
    },
    Mouse {
        client_id: ClientId,
        event: MouseEvent,
    },
    Paste {
        client_id: ClientId,
        content: String,
    },
    Resize {
        client_id: ClientId,
        size: PaneSize,
    },
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum SystemEvent {
    ServerStarted,
    ServerStopping,
    ConfigReloaded,
    PluginLoaded { name: String },
    PluginUnloaded { name: String },
    ModeChanged(Mode),
    Error { message: String },
}

// ============================================================================
// Top-level Event enum
// ============================================================================

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Event {
    Session(SessionEvent),
    Pane(PaneEvent),
    Client(ClientEvent),
    Input(InputEvent),
    System(SystemEvent),
}

impl Event {
    /// Create a session created event
    #[must_use]
    pub const fn session_created(session_id: SessionId, name: Option<String>) -> Self {
        Self::Session(SessionEvent::Created { session_id, name })
    }

    /// Create a pane created event
    #[must_use]
    pub const fn pane_created(pane_id: PaneId, size: PaneSize) -> Self {
        Self::Pane(PaneEvent::Created { pane_id, size })
    }

    /// Create a client connected event
    #[must_use]
    pub const fn client_connected(client_id: ClientId, independent_view: bool) -> Self {
        Self::Client(ClientEvent::Connected {
            client_id,
            independent_view,
        })
    }

    /// Create a key input event
    #[must_use]
    pub const fn key_input(client_id: ClientId, event: KeyEvent) -> Self {
        Self::Input(InputEvent::Key { client_id, event })
    }

    /// Create a mouse input event
    #[must_use]
    pub const fn mouse_input(client_id: ClientId, event: MouseEvent) -> Self {
        Self::Input(InputEvent::Mouse { client_id, event })
    }

    /// Create a system error event
    #[must_use]
    pub const fn system_error(message: String) -> Self {
        Self::System(SystemEvent::Error { message })
    }
}

// ============================================================================
// Conversions to/from bmux_keyboard types
// ============================================================================

#[cfg(feature = "keyboard")]
mod keyboard_conversions {
    use super::*;
    use bmux_keyboard::types as kb;

    impl From<kb::KeyCode> for KeyCode {
        fn from(k: kb::KeyCode) -> Self {
            match k {
                kb::KeyCode::Char(c) => Self::Char(c),
                kb::KeyCode::Enter => Self::Enter,
                kb::KeyCode::Tab => Self::Tab,
                kb::KeyCode::Backspace => Self::Backspace,
                kb::KeyCode::Delete => Self::Delete,
                kb::KeyCode::Escape => Self::Escape,
                kb::KeyCode::Space => Self::Space,
                kb::KeyCode::Up => Self::Up,
                kb::KeyCode::Down => Self::Down,
                kb::KeyCode::Left => Self::Left,
                kb::KeyCode::Right => Self::Right,
                kb::KeyCode::Home => Self::Home,
                kb::KeyCode::End => Self::End,
                kb::KeyCode::PageUp => Self::PageUp,
                kb::KeyCode::PageDown => Self::PageDown,
                kb::KeyCode::Insert => Self::Insert,
                kb::KeyCode::F(n) => Self::F(n),
            }
        }
    }

    impl From<KeyCode> for kb::KeyCode {
        fn from(k: KeyCode) -> Self {
            match k {
                KeyCode::Char(c) => Self::Char(c),
                KeyCode::Enter => Self::Enter,
                KeyCode::Tab => Self::Tab,
                KeyCode::Backspace => Self::Backspace,
                KeyCode::Delete => Self::Delete,
                KeyCode::Escape => Self::Escape,
                KeyCode::Space => Self::Space,
                KeyCode::Up => Self::Up,
                KeyCode::Down => Self::Down,
                KeyCode::Left => Self::Left,
                KeyCode::Right => Self::Right,
                KeyCode::Home => Self::Home,
                KeyCode::End => Self::End,
                KeyCode::PageUp => Self::PageUp,
                KeyCode::PageDown => Self::PageDown,
                KeyCode::Insert => Self::Insert,
                KeyCode::F(n) => Self::F(n),
            }
        }
    }

    impl From<kb::Modifiers> for KeyModifiers {
        fn from(m: kb::Modifiers) -> Self {
            Self {
                ctrl: m.ctrl,
                alt: m.alt,
                shift: m.shift,
                super_key: m.super_key,
            }
        }
    }

    impl From<KeyModifiers> for kb::Modifiers {
        fn from(m: KeyModifiers) -> Self {
            Self {
                ctrl: m.ctrl,
                alt: m.alt,
                shift: m.shift,
                super_key: m.super_key,
            }
        }
    }

    impl From<kb::KeyStroke> for KeyEvent {
        fn from(ks: kb::KeyStroke) -> Self {
            Self {
                code: ks.key.into(),
                modifiers: ks.modifiers.into(),
            }
        }
    }

    impl From<KeyEvent> for kb::KeyStroke {
        fn from(ke: KeyEvent) -> Self {
            Self {
                key: ke.code.into(),
                modifiers: ke.modifiers.into(),
            }
        }
    }
}
