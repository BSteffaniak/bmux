//! Backend-neutral native input. Text is not a key and composition is never PTY input.
//!
//! Consumers offer key events to the existing BMUX reducer first. Only an unconsumed
//! press/repeat may emit `KeyEvent::text`; an IME commit emits text exactly once.
//! Pointer coordinates remain physical pixels until the committed presentation maps
//! them. This module owns no hit map, focus tree, terminal encoder, or resource model.

/// Native input, independent of window-system types.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    Key(KeyEvent),
    Ime(ImeEvent),
    PointerMoved {
        position: PhysicalPoint,
    },
    PointerLeft,
    PointerButton {
        button: PointerButton,
        phase: KeyPhase,
        modifiers: Modifiers,
    },
    Scroll {
        delta: ScrollDelta,
        modifiers: Modifiers,
    },
    Focus(bool),
}

/// Physical framebuffer pixels, not logical window pixels or execution cells.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PhysicalPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyPhase {
    Press,
    Repeat,
    Release,
}

/// Modifier snapshot associated with this event, not a later global state.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub physical: PhysicalKey,
    pub logical: LogicalKey,
    pub phase: KeyPhase,
    pub modifiers: Modifiers,
    /// Committed keyboard text. Absent during composition and on release.
    /// Offer the key to bindings first; never also encode its character key.
    pub text: Option<String>,
}

/// Opaque physical position identity, independent of keyboard layout.
/// Codes are backend-local diagnostic identities, not persistent binding syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhysicalKey {
    Code(String),
    Unidentified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogicalKey {
    Character(String),
    Named(NamedKey),
    Dead(Option<char>),
    Unidentified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Tab,
    Backspace,
    Delete,
    Escape,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Function(u8),
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    Shift,
    Control,
    Alt,
    Super,
}

/// Preedit cursor offsets are UTF-8 byte offsets, validated by the adapter.
/// Empty preedit cancels composition; it must not become committed input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImeEvent {
    Enabled,
    Disabled,
    Preedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    Commit(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
    Other(u16),
}

/// Preserve high-resolution scrolling; converting pixels to cells belongs to
/// the committed presentation geometry, not the window backend.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollDelta {
    Lines { x: f64, y: f64 },
    Pixels { x: f64, y: f64 },
}

impl KeyEvent {
    /// Text fallback after bindings decline this key. Control/Alt/Super chords
    /// use BMUX's existing key encoder rather than platform text translation.
    #[must_use]
    pub fn unhandled_text(&self) -> Option<&str> {
        if self.phase == KeyPhase::Release
            || self.modifiers.control
            || self.modifiers.alt
            || self.modifiers.super_key
            || !matches!(
                self.logical,
                LogicalKey::Character(_) | LogicalKey::Named(NamedKey::Space)
            )
        {
            return None;
        }
        self.text.as_deref().filter(|text| !text.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_and_shortcuts_never_emit_text_fallback() {
        let mut event = KeyEvent {
            physical: PhysicalKey::Code("KeyA".into()),
            logical: LogicalKey::Character("a".into()),
            phase: KeyPhase::Press,
            modifiers: Modifiers::default(),
            text: Some("a".into()),
        };
        assert_eq!(event.unhandled_text(), Some("a"));
        event.phase = KeyPhase::Repeat;
        assert_eq!(event.unhandled_text(), Some("a"));
        event.phase = KeyPhase::Release;
        assert_eq!(event.unhandled_text(), None);
        event.phase = KeyPhase::Press;
        event.modifiers.control = true;
        assert_eq!(event.unhandled_text(), None);
    }
}
