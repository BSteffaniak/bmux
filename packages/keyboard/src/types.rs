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

    /// Compute the CSI u modifier parameter value.
    ///
    /// Per the kitty keyboard protocol specification:
    /// `modifier_param = 1 + (shift ? 1 : 0) + (alt ? 2 : 0) + (ctrl ? 4 : 0) + (super ? 8 : 0)`
    #[must_use]
    pub const fn csi_u_param(self) -> u8 {
        let mut param: u8 = 1;
        if self.shift {
            param += 1;
        }
        if self.alt {
            param += 2;
        }
        if self.ctrl {
            param += 4;
        }
        if self.super_key {
            param += 8;
        }
        param
    }

    /// Decode a CSI u modifier parameter value back into modifier flags.
    ///
    /// Returns `None` if `param` is 0 (invalid per spec).
    #[must_use]
    pub const fn from_csi_u_param(param: u8) -> Option<Self> {
        if param == 0 {
            return None;
        }
        let val = param - 1;
        Some(Self {
            shift: val & 1 != 0,
            alt: val & 2 != 0,
            ctrl: val & 4 != 0,
            super_key: val & 8 != 0,
        })
    }

    /// Returns true if no modifiers are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.ctrl && !self.alt && !self.shift && !self.super_key
    }

    /// Returns true if any modifier beyond Alt is set.
    ///
    /// Legacy encoding can only represent Alt (as ESC prefix) for certain keys.
    /// This returns true when CSI u encoding is needed.
    #[must_use]
    pub const fn needs_csi_u(self) -> bool {
        self.ctrl || self.shift || self.super_key
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
    fn csi_u_param_none() {
        assert_eq!(Modifiers::NONE.csi_u_param(), 1);
    }

    #[test]
    fn csi_u_param_shift() {
        let m = Modifiers {
            shift: true,
            ..Modifiers::NONE
        };
        assert_eq!(m.csi_u_param(), 2);
    }

    #[test]
    fn csi_u_param_alt() {
        let m = Modifiers {
            alt: true,
            ..Modifiers::NONE
        };
        assert_eq!(m.csi_u_param(), 3);
    }

    #[test]
    fn csi_u_param_ctrl() {
        let m = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        assert_eq!(m.csi_u_param(), 5);
    }

    #[test]
    fn csi_u_param_ctrl_shift() {
        let m = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        };
        assert_eq!(m.csi_u_param(), 6);
    }

    #[test]
    fn csi_u_param_all() {
        let m = Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            super_key: true,
        };
        assert_eq!(m.csi_u_param(), 16);
    }

    #[test]
    fn csi_u_param_roundtrip() {
        let cases = [
            Modifiers::NONE,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
            Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
            Modifiers {
                super_key: true,
                ..Modifiers::NONE
            },
            Modifiers {
                ctrl: true,
                shift: true,
                alt: true,
                super_key: true,
            },
        ];
        for m in &cases {
            let param = m.csi_u_param();
            let decoded = Modifiers::from_csi_u_param(param).expect("valid param");
            assert_eq!(*m, decoded, "roundtrip failed for param {param}");
        }
    }

    #[test]
    fn from_csi_u_param_zero_is_none() {
        assert!(Modifiers::from_csi_u_param(0).is_none());
    }

    #[test]
    fn needs_csi_u_only_alt_returns_false() {
        let m = Modifiers {
            alt: true,
            ..Modifiers::NONE
        };
        assert!(!m.needs_csi_u());
    }

    #[test]
    fn needs_csi_u_ctrl_returns_true() {
        let m = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        assert!(m.needs_csi_u());
    }

    #[test]
    fn simple_keystroke_has_no_modifiers() {
        let ks = KeyStroke::simple(KeyCode::Enter);
        assert!(ks.modifiers.is_empty());
    }
}
