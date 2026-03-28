//! Canonical key types for bmux.

/// A key code identifying a specific key on the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum KeyCode {
    /// A character key (always lowercase for alphabetic).
    Char(char),
    /// Enter / Return.
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Escape.
    Escape,
    /// Space bar.
    Space,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Insert.
    Insert,
    /// Function key (F1 through F12+).
    F(u8),
}

/// Modifier flags for a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[allow(clippy::struct_excessive_bools)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        super_key: false,
    };

    /// Returns true if no modifiers are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.ctrl && !self.alt && !self.shift && !self.super_key
    }
}

/// A key stroke: a key code combined with modifier flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct KeyStroke {
    pub key: KeyCode,
    pub modifiers: Modifiers,
}

impl KeyStroke {
    /// Create a key stroke with no modifiers.
    #[must_use]
    pub const fn simple(key: KeyCode) -> Self {
        Self {
            key,
            modifiers: Modifiers::NONE,
        }
    }

    /// Create a key stroke with the given modifier flags.
    #[must_use]
    pub const fn with_modifiers(key: KeyCode, modifiers: Modifiers) -> Self {
        Self { key, modifiers }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_empty_returns_true_for_none() {
        assert!(Modifiers::NONE.is_empty());
    }

    #[test]
    fn is_empty_returns_false_when_ctrl_set() {
        let m = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        assert!(!m.is_empty());
    }

    #[test]
    fn simple_keystroke_has_no_modifiers() {
        let ks = KeyStroke::simple(KeyCode::Enter);
        assert!(ks.modifiers.is_empty());
    }
}
