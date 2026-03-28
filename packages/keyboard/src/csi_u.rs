//! Kitty keyboard protocol (CSI u) encoding and decoding.
//!
//! Implements the encoding and decoding of key events using the CSI u format
//! as specified by the kitty keyboard protocol.
//!
//! CSI u format: `ESC [ <codepoint> ; <modifier> u`
//!
//! For keys that have legacy CSI representations (arrows, nav keys), the
//! xterm-style modifier parameter format is used instead:
//! - Arrows: `ESC [ 1 ; <modifier> <A-D>`
//! - Nav keys: `ESC [ <number> ; <modifier> ~`

use crate::types::{KeyCode, KeyStroke, Modifiers};

/// Compute the CSI u modifier parameter value from modifier flags.
///
/// Per the kitty keyboard protocol specification:
/// `modifier_param = 1 + (shift ? 1 : 0) + (alt ? 2 : 0) + (ctrl ? 4 : 0) + (super ? 8 : 0)`
#[must_use]
pub const fn modifier_param(mods: Modifiers) -> u8 {
    let mut param: u8 = 1;
    if mods.shift {
        param += 1;
    }
    if mods.alt {
        param += 2;
    }
    if mods.ctrl {
        param += 4;
    }
    if mods.super_key {
        param += 8;
    }
    param
}

/// Decode a CSI u modifier parameter value back into modifier flags.
///
/// Returns `None` if `param` is 0 (invalid per spec).
#[must_use]
pub const fn modifiers_from_param(param: u8) -> Option<Modifiers> {
    if param == 0 {
        return None;
    }
    let val = param - 1;
    Some(Modifiers {
        shift: val & 1 != 0,
        alt: val & 2 != 0,
        ctrl: val & 4 != 0,
        super_key: val & 8 != 0,
    })
}

/// Map a [`KeyCode`] to its CSI u Unicode codepoint value.
///
/// Returns `None` for keys that should use legacy CSI encoding with modifier
/// parameters (arrows, Home, End, etc.) or that are not representable.
#[must_use]
pub const fn keycode_to_codepoint(key: KeyCode) -> Option<u32> {
    match key {
        KeyCode::Char(c) => Some(c as u32),
        KeyCode::Enter => Some(13),
        KeyCode::Tab => Some(9),
        KeyCode::Backspace => Some(127),
        KeyCode::Escape => Some(27),
        KeyCode::Space => Some(32),
        // Delete, Insert use legacy tilde encoding with modifier parameters.
        KeyCode::Delete | KeyCode::Insert => None,
        KeyCode::F(n) => {
            // F1-F4 don't have stable CSI u codepoints in all implementations.
            // F5+ use legacy encoding. For consistency, map F-keys to their
            // unicode codepoints from the kitty spec.
            match n {
                1..=4 => None,  // Use legacy SS3 encoding
                5..=12 => None, // Use legacy CSI ~ encoding
                _ => None,
            }
        }
        // These use legacy CSI encoding with modifier parameters instead.
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown => None,
    }
}

/// Map a CSI u codepoint back to a [`KeyCode`].
#[must_use]
pub const fn codepoint_to_keycode(cp: u32) -> Option<KeyCode> {
    match cp {
        13 => Some(KeyCode::Enter),
        9 => Some(KeyCode::Tab),
        127 => Some(KeyCode::Backspace),
        27 => Some(KeyCode::Escape),
        32 => Some(KeyCode::Space),
        cp if cp > 0 && cp < 0x110000 => {
            // Safety: we check the range is valid for char
            match char::from_u32(cp) {
                Some(c) => Some(KeyCode::Char(c)),
                None => None,
            }
        }
        _ => None,
    }
}

/// Encode a key stroke as a CSI u byte sequence.
///
/// This handles two cases:
/// 1. Keys with CSI u codepoints: `ESC [ <codepoint> ; <modifier> u`
///    (or `ESC [ <codepoint> u` with no modifiers)
/// 2. Keys with legacy CSI representations but needing modifier parameters:
///    - Arrows: `ESC [ 1 ; <modifier> <final>`
///    - Nav keys with tilde: `ESC [ <number> ; <modifier> ~`
///    - Home/End: `ESC [ 1 ; <modifier> <H|F>`
///
/// Returns `None` if the key cannot be encoded in CSI u format (e.g., F5+).
#[must_use]
pub fn encode(stroke: &KeyStroke) -> Option<Vec<u8>> {
    // First try CSI u codepoint encoding.
    if let Some(cp) = keycode_to_codepoint(stroke.key) {
        let modifier = modifier_param(stroke.modifiers);
        return if modifier == 1 {
            Some(format!("\x1b[{cp}u").into_bytes())
        } else {
            Some(format!("\x1b[{cp};{modifier}u").into_bytes())
        };
    }

    // Fall back to legacy CSI format with modifier parameters.
    encode_modified_legacy(stroke)
}

/// Encode navigation/arrow keys with xterm-style modifier parameters.
fn encode_modified_legacy(stroke: &KeyStroke) -> Option<Vec<u8>> {
    let modifier = modifier_param(stroke.modifiers);

    match stroke.key {
        // Arrows: ESC [ 1 ; <mod> <letter>
        KeyCode::Up => Some(format!("\x1b[1;{modifier}A").into_bytes()),
        KeyCode::Down => Some(format!("\x1b[1;{modifier}B").into_bytes()),
        KeyCode::Right => Some(format!("\x1b[1;{modifier}C").into_bytes()),
        KeyCode::Left => Some(format!("\x1b[1;{modifier}D").into_bytes()),
        // Home/End: ESC [ 1 ; <mod> <H|F>
        KeyCode::Home => Some(format!("\x1b[1;{modifier}H").into_bytes()),
        KeyCode::End => Some(format!("\x1b[1;{modifier}F").into_bytes()),
        // Tilde keys: ESC [ <number> ; <mod> ~
        KeyCode::Insert => Some(format!("\x1b[2;{modifier}~").into_bytes()),
        KeyCode::Delete => Some(format!("\x1b[3;{modifier}~").into_bytes()),
        KeyCode::PageUp => Some(format!("\x1b[5;{modifier}~").into_bytes()),
        KeyCode::PageDown => Some(format!("\x1b[6;{modifier}~").into_bytes()),
        // F-keys with modifier params
        KeyCode::F(n) => encode_modified_fkey(n, modifier),
        _ => None,
    }
}

fn encode_modified_fkey(n: u8, modifier: u8) -> Option<Vec<u8>> {
    // F1-F4 use SS3 encoding without modifiers, but with modifiers switch to CSI format.
    let (number, final_byte) = match n {
        1 => (1, Some(b'P')),
        2 => (1, Some(b'Q')),
        3 => (1, Some(b'R')),
        4 => (1, Some(b'S')),
        5 => (15, None),
        6 => (17, None),
        7 => (18, None),
        8 => (19, None),
        9 => (20, None),
        10 => (21, None),
        11 => (23, None),
        12 => (24, None),
        _ => return None,
    };

    if let Some(fb) = final_byte {
        // F1-F4 with modifiers: ESC [ 1 ; <mod> <P|Q|R|S>
        Some(format!("\x1b[1;{modifier}{}", fb as char).into_bytes())
    } else {
        // F5-F12: ESC [ <number> ; <mod> ~
        Some(format!("\x1b[{number};{modifier}~").into_bytes())
    }
}

/// Decode result from [`decode`].
///
/// Contains the decoded key stroke and the number of bytes consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeResult {
    pub stroke: KeyStroke,
    pub consumed: usize,
}

/// Attempt to decode a CSI u or modified xterm sequence from the given bytes.
///
/// Returns `Some(DecodeResult)` on success, or `None` if the bytes do not
/// start with a recognized sequence. Returns `None` for incomplete sequences
/// (the caller should buffer more data and retry).
///
/// Recognized formats:
/// - `ESC [ <codepoint> u` (CSI u, no modifiers)
/// - `ESC [ <codepoint> ; <modifier> u` (CSI u with modifiers)
/// - `ESC [ 1 ; <modifier> <A-D|H|F|P-S>` (modified arrows/Home/End/F1-F4)
/// - `ESC [ <number> ; <modifier> ~` (modified tilde keys)
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<DecodeResult> {
    // Must start with ESC [
    if bytes.len() < 2 || bytes[0] != 0x1b || bytes[1] != b'[' {
        return None;
    }

    let seq = &bytes[2..];
    if seq.is_empty() {
        // Incomplete, need more bytes.
        return None;
    }

    // Find the final byte (alphabetic or tilde).
    let mut final_idx = None;
    for (i, &byte) in seq.iter().enumerate() {
        if byte.is_ascii_alphabetic() || byte == b'~' {
            final_idx = Some(i);
            break;
        }
        // Only digits and semicolons are valid in the parameter area.
        if !byte.is_ascii_digit() && byte != b';' {
            return None;
        }
    }

    let final_idx = final_idx?; // Incomplete if no final byte found.
    let final_byte = seq[final_idx];
    let params_bytes = &seq[..final_idx];
    let consumed = 2 + final_idx + 1; // ESC + [ + params + final

    // Parse parameters (semicolon-separated numbers).
    let params = parse_params(params_bytes);

    if final_byte == b'u' {
        // CSI u format: codepoint [; modifier] u
        let codepoint = *params.first()?;
        let modifier_param = params.get(1).copied().unwrap_or(1);
        let modifiers = modifiers_from_param(u8::try_from(modifier_param).ok()?)?;
        let key = codepoint_to_keycode(codepoint)?;
        return Some(DecodeResult {
            stroke: KeyStroke::with_modifiers(key, modifiers),
            consumed,
        });
    }

    if final_byte == b'~' {
        // Modified tilde key: <number> [; modifier] ~
        let number = *params.first()?;
        let modifier_param = params.get(1).copied().unwrap_or(1);
        let modifiers = modifiers_from_param(u8::try_from(modifier_param).ok()?)?;
        let key = tilde_number_to_keycode(number)?;
        return Some(DecodeResult {
            stroke: KeyStroke::with_modifiers(key, modifiers),
            consumed,
        });
    }

    if final_byte.is_ascii_alphabetic() {
        // Modified arrow/Home/End or F1-F4: [1] ; <modifier> <final>
        // Format: ESC [ <params> <final>
        let modifier_param = params.get(1).copied().unwrap_or(1);
        let modifiers = modifiers_from_param(u8::try_from(modifier_param).ok()?)?;

        // Only decode if there's a modifier parameter (otherwise it's a plain
        // legacy sequence that the legacy decoder should handle).
        if params.len() < 2 {
            return None;
        }

        let key = match final_byte {
            b'A' => Some(KeyCode::Up),
            b'B' => Some(KeyCode::Down),
            b'C' => Some(KeyCode::Right),
            b'D' => Some(KeyCode::Left),
            b'H' => Some(KeyCode::Home),
            b'F' => Some(KeyCode::End),
            b'P' => Some(KeyCode::F(1)),
            b'Q' => Some(KeyCode::F(2)),
            b'R' => Some(KeyCode::F(3)),
            b'S' => Some(KeyCode::F(4)),
            _ => None,
        }?;

        return Some(DecodeResult {
            stroke: KeyStroke::with_modifiers(key, modifiers),
            consumed,
        });
    }

    None
}

fn tilde_number_to_keycode(number: u32) -> Option<KeyCode> {
    match number {
        2 => Some(KeyCode::Insert),
        3 => Some(KeyCode::Delete),
        5 => Some(KeyCode::PageUp),
        6 => Some(KeyCode::PageDown),
        15 => Some(KeyCode::F(5)),
        17 => Some(KeyCode::F(6)),
        18 => Some(KeyCode::F(7)),
        19 => Some(KeyCode::F(8)),
        20 => Some(KeyCode::F(9)),
        21 => Some(KeyCode::F(10)),
        23 => Some(KeyCode::F(11)),
        24 => Some(KeyCode::F(12)),
        _ => None,
    }
}

fn parse_params(bytes: &[u8]) -> Vec<u32> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    s.split(';')
        .filter_map(|part| {
            if part.is_empty() {
                Some(0) // Empty param defaults to 0 per ECMA-48
            } else {
                part.parse::<u32>().ok()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{KeyCode, KeyStroke, Modifiers};

    #[test]
    fn modifier_param_none() {
        assert_eq!(modifier_param(Modifiers::NONE), 1);
    }

    #[test]
    fn modifier_param_ctrl() {
        assert_eq!(
            modifier_param(Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            }),
            5
        );
    }

    #[test]
    fn modifier_param_roundtrip() {
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
            let param = modifier_param(*m);
            let decoded = modifiers_from_param(param).expect("valid param");
            assert_eq!(*m, decoded, "roundtrip failed for param {param}");
        }
    }

    #[test]
    fn modifiers_from_param_zero_is_none() {
        assert!(modifiers_from_param(0).is_none());
    }

    #[test]
    fn encode_ctrl_enter() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Enter,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[13;5u");
    }

    #[test]
    fn encode_shift_enter() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Enter,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[13;2u");
    }

    #[test]
    fn encode_ctrl_shift_enter() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Enter,
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[13;6u");
    }

    #[test]
    fn encode_bare_enter() {
        let stroke = KeyStroke::simple(KeyCode::Enter);
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[13u");
    }

    #[test]
    fn encode_ctrl_tab() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Tab,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[9;5u");
    }

    #[test]
    fn encode_ctrl_backspace() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Backspace,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[127;5u");
    }

    #[test]
    fn encode_ctrl_up() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Up,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[1;5A");
    }

    #[test]
    fn encode_alt_left() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Left,
            Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[1;3D");
    }

    #[test]
    fn encode_ctrl_page_up() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::PageUp,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[5;5~");
    }

    #[test]
    fn encode_ctrl_home() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Home,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[1;5H");
    }

    #[test]
    fn decode_ctrl_enter() {
        let result = decode(b"\x1b[13;5u").unwrap();
        assert_eq!(result.consumed, 7);
        assert_eq!(result.stroke.key, KeyCode::Enter);
        assert!(result.stroke.modifiers.ctrl);
        assert!(!result.stroke.modifiers.shift);
    }

    #[test]
    fn decode_shift_enter() {
        let result = decode(b"\x1b[13;2u").unwrap();
        assert_eq!(result.stroke.key, KeyCode::Enter);
        assert!(result.stroke.modifiers.shift);
    }

    #[test]
    fn decode_bare_csi_u_enter() {
        let result = decode(b"\x1b[13u").unwrap();
        assert_eq!(result.stroke.key, KeyCode::Enter);
        assert!(result.stroke.modifiers.is_empty());
    }

    #[test]
    fn decode_ctrl_tab() {
        let result = decode(b"\x1b[9;5u").unwrap();
        assert_eq!(result.stroke.key, KeyCode::Tab);
        assert!(result.stroke.modifiers.ctrl);
    }

    #[test]
    fn decode_ctrl_up() {
        let result = decode(b"\x1b[1;5A").unwrap();
        assert_eq!(result.stroke.key, KeyCode::Up);
        assert!(result.stroke.modifiers.ctrl);
    }

    #[test]
    fn decode_ctrl_page_up() {
        let result = decode(b"\x1b[5;5~").unwrap();
        assert_eq!(result.stroke.key, KeyCode::PageUp);
        assert!(result.stroke.modifiers.ctrl);
    }

    #[test]
    fn decode_shift_arrow_left() {
        let result = decode(b"\x1b[1;2D").unwrap();
        assert_eq!(result.stroke.key, KeyCode::Left);
        assert!(result.stroke.modifiers.shift);
    }

    #[test]
    fn decode_incomplete_returns_none() {
        // Incomplete CSI sequence.
        assert!(decode(b"\x1b[").is_none());
        assert!(decode(b"\x1b[13").is_none());
        assert!(decode(b"\x1b[13;").is_none());
        assert!(decode(b"\x1b[13;5").is_none());
    }

    #[test]
    fn decode_not_csi_returns_none() {
        assert!(decode(b"\x1b").is_none());
        assert!(decode(b"hello").is_none());
    }

    #[test]
    fn decode_plain_arrow_returns_none() {
        // Plain arrow without modifier parameter should return None so legacy
        // decoder can handle it.
        assert!(decode(b"\x1b[A").is_none());
    }

    #[test]
    fn roundtrip_ctrl_enter() {
        let original = KeyStroke::with_modifiers(
            KeyCode::Enter,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        let encoded = encode(&original).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.stroke, original);
        assert_eq!(decoded.consumed, encoded.len());
    }

    #[test]
    fn roundtrip_ctrl_shift_tab() {
        let original = KeyStroke::with_modifiers(
            KeyCode::Tab,
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::NONE
            },
        );
        let encoded = encode(&original).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.stroke, original);
    }

    #[test]
    fn roundtrip_ctrl_up() {
        let original = KeyStroke::with_modifiers(
            KeyCode::Up,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        let encoded = encode(&original).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.stroke, original);
    }

    #[test]
    fn roundtrip_ctrl_page_down() {
        let original = KeyStroke::with_modifiers(
            KeyCode::PageDown,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        let encoded = encode(&original).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.stroke, original);
    }

    #[test]
    fn roundtrip_ctrl_delete() {
        let original = KeyStroke::with_modifiers(
            KeyCode::Delete,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        let encoded = encode(&original).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.stroke, original);
    }

    #[test]
    fn decode_char_csi_u() {
        // ESC [ 97 ; 5 u = Ctrl+a
        let result = decode(b"\x1b[97;5u").unwrap();
        assert_eq!(result.stroke.key, KeyCode::Char('a'));
        assert!(result.stroke.modifiers.ctrl);
    }

    #[test]
    fn decode_extra_bytes_after_sequence() {
        // Only consume the sequence, leave the rest.
        let result = decode(b"\x1b[13;5ufoo").unwrap();
        assert_eq!(result.consumed, 7);
        assert_eq!(result.stroke.key, KeyCode::Enter);
    }
}
