//! Unified key encoding entry point.
//!
//! Dispatches to CSI u or legacy encoding based on whether the terminal
//! supports keyboard enhancement and whether the key event requires it.

use crate::csi_u;
use crate::legacy;
use crate::types::{KeyCode, KeyStroke};

/// Encode a key stroke to raw bytes for writing to a PTY.
///
/// When `enhanced` is true and the key event has modifiers that cannot be
/// represented in legacy encoding, CSI u format is used. Otherwise, legacy
/// VT/xterm encoding is used for maximum compatibility.
///
/// Returns `None` if the key cannot be encoded.
#[must_use]
pub fn encode_key(stroke: &KeyStroke, enhanced: bool) -> Option<Vec<u8>> {
    if enhanced && needs_enhanced_encoding(stroke) {
        if let Some(encoded) = csi_u::encode(stroke) {
            return Some(encoded);
        }
    }

    legacy::encode(stroke)
}

/// Determine whether a key stroke requires enhanced (CSI u) encoding.
///
/// Returns true when the key has modifiers that legacy encoding would silently
/// lose. Specifically:
///
/// - Special keys (Enter, Tab, Backspace, Escape) with Ctrl, Shift, or Super
///   (legacy only supports Alt as ESC prefix for these)
/// - Arrow keys with Ctrl, Alt, or Super (legacy only supports Shift)
/// - Navigation keys (Home, End, PageUp, PageDown, Insert, Delete) with any
///   modifier (legacy has no modifier encoding for these)
/// - F-keys with any modifier
fn needs_enhanced_encoding(stroke: &KeyStroke) -> bool {
    let mods = stroke.modifiers;
    if mods.is_empty() {
        return false;
    }

    match stroke.key {
        // Chars: legacy handles Ctrl+alpha and Alt+char, but not Shift+char
        // combos that differ from just the shifted character, or Super+char.
        // For safety, use CSI u when super is involved.
        KeyCode::Char(_) => mods.super_key,

        // Enter, Tab, Backspace, Escape: legacy only handles Alt prefix.
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Escape | KeyCode::Space => {
            mods.needs_csi_u()
        }

        // Arrows: legacy only handles Shift (param 2).
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
            mods.ctrl || mods.alt || mods.super_key
        }

        // Navigation: legacy has no modifier encoding at all.
        KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::Insert
        | KeyCode::Delete => true,

        // F-keys: legacy has no modifier encoding.
        KeyCode::F(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{KeyCode, KeyStroke, Modifiers};

    #[test]
    fn plain_enter_uses_legacy() {
        let stroke = KeyStroke::simple(KeyCode::Enter);
        assert_eq!(encode_key(&stroke, true).unwrap(), b"\r");
    }

    #[test]
    fn ctrl_enter_enhanced_uses_csi_u() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Enter,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode_key(&stroke, true).unwrap(), b"\x1b[13;5u");
    }

    #[test]
    fn ctrl_enter_not_enhanced_uses_legacy() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Enter,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        // Legacy encoding silently drops Ctrl modifier for Enter.
        assert_eq!(encode_key(&stroke, false).unwrap(), b"\r");
    }

    #[test]
    fn alt_enter_uses_legacy() {
        // Alt+Enter can be represented in legacy (ESC prefix).
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Enter,
            Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode_key(&stroke, true).unwrap(), b"\x1b\r");
    }

    #[test]
    fn ctrl_c_uses_legacy_even_enhanced() {
        // Ctrl+C has a well-known legacy encoding (0x03), so it should stay legacy.
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Char('c'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode_key(&stroke, true).unwrap(), vec![0x03]);
    }

    #[test]
    fn shift_up_uses_legacy() {
        // Shift+Up has legacy encoding (modifier param 2).
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Up,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode_key(&stroke, true).unwrap(), b"\x1b[1;2A");
    }

    #[test]
    fn ctrl_up_enhanced_uses_csi_u() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Up,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode_key(&stroke, true).unwrap(), b"\x1b[1;5A");
    }

    #[test]
    fn ctrl_up_not_enhanced_drops_modifier() {
        // Legacy encoding for arrows only handles Shift, so Ctrl+Up becomes plain Up.
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Up,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode_key(&stroke, false).unwrap(), b"\x1b[A");
    }

    #[test]
    fn ctrl_page_up_enhanced() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::PageUp,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode_key(&stroke, true).unwrap(), b"\x1b[5;5~");
    }
}
