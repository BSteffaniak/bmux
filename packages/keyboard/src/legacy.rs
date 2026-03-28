//! Legacy VT/xterm encoding and decoding.
//!
//! Implements the traditional terminal escape sequence encoding used before the
//! kitty keyboard protocol. This is the fallback when keyboard enhancement is
//! not available.

use crate::types::{KeyCode, KeyStroke, Modifiers};

/// Encode a key stroke as legacy VT/xterm bytes.
///
/// Returns `None` if the key cannot be represented in legacy encoding.
///
/// Legacy encoding has limited modifier support:
/// - Ctrl+alpha maps to control codes (0x01-0x1a)
/// - Alt prepends ESC (0x1b) for chars, Enter, Tab, Backspace
/// - Shift on alphabetic chars maps to uppercase (A-Z)
/// - Shift encodes for arrow keys (modifier param 2)
/// - Other modifier combinations for special keys are silently lost
#[must_use]
pub fn encode(stroke: &KeyStroke) -> Option<Vec<u8>> {
    let Modifiers {
        ctrl,
        alt,
        shift,
        super_key: _,
    } = stroke.modifiers;

    let mut out = Vec::new();
    let push_alt = |out: &mut Vec<u8>| {
        if alt {
            out.push(0x1b);
        }
    };

    match stroke.key {
        KeyCode::Char(c) => {
            if ctrl {
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    push_alt(&mut out);
                    out.push((lower as u8 - b'a') + 1);
                    return Some(out);
                }
            }

            push_alt(&mut out);
            let c = if shift && c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c
            };
            if c.is_ascii() {
                out.push(c as u8);
            } else {
                let mut buf = [0_u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
        KeyCode::Enter => {
            push_alt(&mut out);
            out.push(b'\r');
            Some(out)
        }
        KeyCode::Tab => {
            push_alt(&mut out);
            out.push(b'\t');
            Some(out)
        }
        KeyCode::Backspace => {
            push_alt(&mut out);
            out.push(0x7f);
            Some(out)
        }
        KeyCode::Escape => Some(vec![0x1b]),
        KeyCode::Space => {
            push_alt(&mut out);
            out.push(b' ');
            Some(out)
        }
        KeyCode::Up => arrow_encoding(shift, b'A'),
        KeyCode::Down => arrow_encoding(shift, b'B'),
        KeyCode::Right => arrow_encoding(shift, b'C'),
        KeyCode::Left => arrow_encoding(shift, b'D'),
        KeyCode::Home => Some(vec![0x1b, b'[', b'H']),
        KeyCode::End => Some(vec![0x1b, b'[', b'F']),
        KeyCode::PageUp => Some(vec![0x1b, b'[', b'5', b'~']),
        KeyCode::PageDown => Some(vec![0x1b, b'[', b'6', b'~']),
        KeyCode::Insert => Some(vec![0x1b, b'[', b'2', b'~']),
        KeyCode::Delete => Some(vec![0x1b, b'[', b'3', b'~']),
        KeyCode::F(n) => match n {
            1 => Some(vec![0x1b, b'O', b'P']),
            2 => Some(vec![0x1b, b'O', b'Q']),
            3 => Some(vec![0x1b, b'O', b'R']),
            4 => Some(vec![0x1b, b'O', b'S']),
            _ => None,
        },
    }
}

fn arrow_encoding(shift: bool, letter: u8) -> Option<Vec<u8>> {
    Some(if shift {
        vec![0x1b, b'[', b'1', b';', b'2', letter]
    } else {
        vec![0x1b, b'[', letter]
    })
}

/// Decode a single non-escape byte into a [`KeyStroke`].
///
/// Also returns the raw byte in a `Vec` for forwarding purposes.
#[must_use]
pub fn decode_single(byte: u8) -> (KeyStroke, Vec<u8>) {
    let stroke = match byte {
        b'\r' | b'\n' => KeyStroke::simple(KeyCode::Enter),
        b'\t' => KeyStroke::simple(KeyCode::Tab),
        0x7f => KeyStroke::simple(KeyCode::Backspace),
        b' ' => KeyStroke::simple(KeyCode::Space),
        0x01..=0x1a => {
            let character = char::from((byte - 1) + b'a');
            KeyStroke::with_modifiers(
                KeyCode::Char(character),
                Modifiers {
                    ctrl: true,
                    ..Modifiers::NONE
                },
            )
        }
        b'A'..=b'Z' => KeyStroke::with_modifiers(
            KeyCode::Char(char::from(byte).to_ascii_lowercase()),
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        ),
        _ => KeyStroke::simple(KeyCode::Char(char::from(byte))),
    };

    (stroke, vec![byte])
}

/// Decode result from [`decode_escape`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDecodeResult {
    pub stroke: KeyStroke,
    pub raw: Vec<u8>,
    pub consumed: usize,
}

/// Attempt to decode a legacy escape sequence from the given bytes.
///
/// The bytes should start with `ESC` (0x1b). Returns:
/// - `Some(result)` if a complete sequence was matched (or fallback to bare ESC)
/// - `None` if the bytes are an incomplete prefix of a known sequence
///   (the caller should buffer more data and retry)
#[must_use]
pub fn decode_escape(bytes: &[u8]) -> Option<LegacyDecodeResult> {
    #[allow(clippy::type_complexity)]
    let sequences: &[(&[u8], KeyStroke)] = &[
        (b"\x1b[A", KeyStroke::simple(KeyCode::Up)),
        (b"\x1b[B", KeyStroke::simple(KeyCode::Down)),
        (b"\x1b[C", KeyStroke::simple(KeyCode::Right)),
        (b"\x1b[D", KeyStroke::simple(KeyCode::Left)),
        (
            b"\x1b[1;2A",
            KeyStroke::with_modifiers(
                KeyCode::Up,
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            ),
        ),
        (
            b"\x1b[1;2B",
            KeyStroke::with_modifiers(
                KeyCode::Down,
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            ),
        ),
        (
            b"\x1b[1;2C",
            KeyStroke::with_modifiers(
                KeyCode::Right,
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            ),
        ),
        (
            b"\x1b[1;2D",
            KeyStroke::with_modifiers(
                KeyCode::Left,
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            ),
        ),
        (b"\x1b[H", KeyStroke::simple(KeyCode::Home)),
        (b"\x1b[F", KeyStroke::simple(KeyCode::End)),
        (b"\x1b[2~", KeyStroke::simple(KeyCode::Insert)),
        (b"\x1b[3~", KeyStroke::simple(KeyCode::Delete)),
        (b"\x1b[5~", KeyStroke::simple(KeyCode::PageUp)),
        (b"\x1b[6~", KeyStroke::simple(KeyCode::PageDown)),
        (
            b"\x1b[Z",
            KeyStroke::with_modifiers(
                KeyCode::Tab,
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            ),
        ),
        (b"\x1bOP", KeyStroke::simple(KeyCode::F(1))),
        (b"\x1bOQ", KeyStroke::simple(KeyCode::F(2))),
        (b"\x1bOR", KeyStroke::simple(KeyCode::F(3))),
        (b"\x1bOS", KeyStroke::simple(KeyCode::F(4))),
    ];

    for (pattern, stroke) in sequences {
        if bytes.starts_with(pattern) {
            return Some(LegacyDecodeResult {
                stroke: *stroke,
                raw: pattern.to_vec(),
                consumed: pattern.len(),
            });
        }

        if pattern.starts_with(bytes) {
            // Incomplete prefix of a known pattern -- wait for more data.
            return None;
        }
    }

    // No known pattern matched and none is a prefix. Return bare ESC.
    Some(LegacyDecodeResult {
        stroke: KeyStroke::simple(KeyCode::Escape),
        raw: vec![0x1b],
        consumed: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{KeyCode, KeyStroke, Modifiers};

    #[test]
    fn encode_plain_enter() {
        let stroke = KeyStroke::simple(KeyCode::Enter);
        assert_eq!(encode(&stroke).unwrap(), b"\r");
    }

    #[test]
    fn encode_alt_enter() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Enter,
            Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b\r");
    }

    #[test]
    fn encode_ctrl_c() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Char('c'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), vec![0x03]);
    }

    #[test]
    fn encode_plain_char() {
        let stroke = KeyStroke::simple(KeyCode::Char('x'));
        assert_eq!(encode(&stroke).unwrap(), b"x");
    }

    #[test]
    fn encode_shift_alpha_produces_uppercase() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Char('a'),
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"A");
    }

    #[test]
    fn encode_shift_various_letters() {
        for (lower, upper) in [('a', b'A'), ('z', b'Z'), ('m', b'M')] {
            let stroke = KeyStroke::with_modifiers(
                KeyCode::Char(lower),
                Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                },
            );
            assert_eq!(
                encode(&stroke).unwrap(),
                vec![upper],
                "shift+{lower} should encode as uppercase"
            );
        }
    }

    #[test]
    fn encode_shift_nonalpha_unchanged() {
        // Shift on non-alphabetic chars should not alter the character.
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Char('1'),
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"1");
    }

    #[test]
    fn encode_alt_shift_char() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Char('a'),
            Modifiers {
                alt: true,
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1bA");
    }

    #[test]
    fn encode_alt_char() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Char('x'),
            Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1bx");
    }

    #[test]
    fn encode_up_arrow() {
        let stroke = KeyStroke::simple(KeyCode::Up);
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[A");
    }

    #[test]
    fn encode_shift_left() {
        let stroke = KeyStroke::with_modifiers(
            KeyCode::Left,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(encode(&stroke).unwrap(), b"\x1b[1;2D");
    }

    #[test]
    fn encode_escape() {
        let stroke = KeyStroke::simple(KeyCode::Escape);
        assert_eq!(encode(&stroke).unwrap(), b"\x1b");
    }

    #[test]
    fn encode_f1() {
        let stroke = KeyStroke::simple(KeyCode::F(1));
        assert_eq!(encode(&stroke).unwrap(), b"\x1bOP");
    }

    #[test]
    fn encode_f5_returns_none() {
        let stroke = KeyStroke::simple(KeyCode::F(5));
        assert!(encode(&stroke).is_none());
    }

    #[test]
    fn decode_single_cr() {
        let (stroke, raw) = decode_single(b'\r');
        assert_eq!(stroke, KeyStroke::simple(KeyCode::Enter));
        assert_eq!(raw, vec![b'\r']);
    }

    #[test]
    fn decode_single_ctrl_a() {
        let (stroke, _) = decode_single(0x01);
        assert_eq!(stroke.key, KeyCode::Char('a'));
        assert!(stroke.modifiers.ctrl);
    }

    #[test]
    fn decode_single_uppercase() {
        let (stroke, _) = decode_single(b'A');
        assert_eq!(stroke.key, KeyCode::Char('a'));
        assert!(stroke.modifiers.shift);
    }

    #[test]
    fn decode_escape_up_arrow() {
        let result = decode_escape(b"\x1b[A").unwrap();
        assert_eq!(result.stroke, KeyStroke::simple(KeyCode::Up));
        assert_eq!(result.consumed, 3);
    }

    #[test]
    fn decode_escape_shift_down() {
        let result = decode_escape(b"\x1b[1;2B").unwrap();
        assert_eq!(result.stroke.key, KeyCode::Down);
        assert!(result.stroke.modifiers.shift);
        assert_eq!(result.consumed, 6);
    }

    #[test]
    fn decode_escape_f1() {
        let result = decode_escape(b"\x1bOP").unwrap();
        assert_eq!(result.stroke, KeyStroke::simple(KeyCode::F(1)));
        assert_eq!(result.consumed, 3);
    }

    #[test]
    fn decode_escape_incomplete() {
        // Incomplete CSI sequence should return None.
        assert!(decode_escape(b"\x1b[").is_none());
        assert!(decode_escape(b"\x1b[1;").is_none());
    }

    #[test]
    fn decode_escape_bare_esc() {
        // Unknown sequence fallback to bare ESC.
        let result = decode_escape(b"\x1bX").unwrap();
        assert_eq!(result.stroke, KeyStroke::simple(KeyCode::Escape));
        assert_eq!(result.consumed, 1);
    }

    #[test]
    fn decode_escape_shift_tab() {
        let result = decode_escape(b"\x1b[Z").unwrap();
        assert_eq!(result.stroke.key, KeyCode::Tab);
        assert!(result.stroke.modifiers.shift);
    }

    #[test]
    fn roundtrip_uppercase_decode_encode() {
        // Decoding uppercase byte then re-encoding should produce the same byte.
        for byte in b'A'..=b'Z' {
            let (stroke, raw) = decode_single(byte);
            let encoded = encode(&stroke).unwrap();
            assert_eq!(
                encoded, raw,
                "roundtrip failed for byte 0x{byte:02x} ('{}')",
                byte as char,
            );
        }
    }
}
