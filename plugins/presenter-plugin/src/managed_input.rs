//! Bounded, versioned linked-control input carried by the existing surface envelope.
//! Geometry/revision/authorization are checked by the owner before decoding this payload.
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind, MouseModifiers};
use bmux_tui::geometry::Point;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "presenter-input-v1:";
pub(crate) const MAX_PASTE_BYTES: usize = 256;
const MAX_WIRE_BYTES: usize = 2048;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Input {
    Key {
        code: u16,
        character: Option<char>,
        modifiers: u8,
    },
    FocusKey {
        nonce: u64,
        code: u16,
        character: Option<char>,
        modifiers: u8,
    },
    Paste(String),
    Mouse {
        kind: u8,
        button: u16,
        modifiers: u8,
    },
}
fn key_code(key: KeyCode) -> (u16, Option<char>) {
    match key {
        KeyCode::Char(c) => (0, Some(c)),
        KeyCode::Enter => (1, None),
        KeyCode::Tab => (2, None),
        KeyCode::Backspace => (3, None),
        KeyCode::Delete => (4, None),
        KeyCode::Escape => (5, None),
        KeyCode::Space => (6, None),
        KeyCode::Up => (7, None),
        KeyCode::Down => (8, None),
        KeyCode::Left => (9, None),
        KeyCode::Right => (10, None),
        KeyCode::Home => (11, None),
        KeyCode::End => (12, None),
        KeyCode::PageUp => (13, None),
        KeyCode::PageDown => (14, None),
        KeyCode::Insert => (15, None),
        KeyCode::F(n) => (256 + u16::from(n), None),
    }
}
fn decode_key(code: u16, character: Option<char>) -> Option<KeyCode> {
    if code == 0 {
        return character.map(KeyCode::Char);
    }
    if character.is_some() {
        return None;
    }
    Some(match code {
        1 => KeyCode::Enter,
        2 => KeyCode::Tab,
        3 => KeyCode::Backspace,
        4 => KeyCode::Delete,
        5 => KeyCode::Escape,
        6 => KeyCode::Space,
        7 => KeyCode::Up,
        8 => KeyCode::Down,
        9 => KeyCode::Left,
        10 => KeyCode::Right,
        11 => KeyCode::Home,
        12 => KeyCode::End,
        13 => KeyCode::PageUp,
        14 => KeyCode::PageDown,
        15 => KeyCode::Insert,
        256..=511 => KeyCode::F(u8::try_from(code - 256).ok()?),
        _ => return None,
    })
}
fn button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Other(n) => 256 + u16::from(n),
    }
}
fn decode_button(code: u16) -> Option<MouseButton> {
    Some(match code {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        256..=511 => MouseButton::Other(u8::try_from(code - 256).ok()?),
        _ => return None,
    })
}
pub(crate) fn encode(event: &Event) -> Option<String> {
    let input = match event {
        Event::Key(key) => {
            let (code, character) = key_code(key.key);
            let m = key.modifiers;
            Input::Key {
                code,
                character,
                modifiers: u8::from(m.ctrl)
                    | (u8::from(m.alt) << 1)
                    | (u8::from(m.shift) << 2)
                    | (u8::from(m.super_key) << 3)
                    | (u8::from(m.hyper) << 4)
                    | (u8::from(m.meta) << 5),
            }
        }
        Event::Paste(text) if text.len() <= MAX_PASTE_BYTES => Input::Paste(text.clone()),
        Event::Mouse(mouse) => {
            let (kind, button) = match mouse.kind {
                MouseEventKind::Down(b) => (0, button_code(b)),
                MouseEventKind::Up(b) => (1, button_code(b)),
                MouseEventKind::Drag(b) => (2, button_code(b)),
                MouseEventKind::Move => (3, 0),
                MouseEventKind::ScrollUp => (4, 0),
                MouseEventKind::ScrollDown => (5, 0),
                MouseEventKind::ScrollLeft => (6, 0),
                MouseEventKind::ScrollRight => (7, 0),
            };
            let m = mouse.modifiers;
            Input::Mouse {
                kind,
                button,
                modifiers: u8::from(m.ctrl) | (u8::from(m.alt) << 1) | (u8::from(m.shift) << 2),
            }
        }
        _ => return None,
    };
    let wire = format!("{PREFIX}{}", serde_json::to_string(&input).ok()?);
    (wire.len() <= MAX_WIRE_BYTES).then_some(wire)
}
pub(crate) fn decode(wire: &str, x: u16, y: u16) -> Option<Event> {
    if wire.len() > MAX_WIRE_BYTES {
        return None;
    }
    // Preserve the original single-character and click clients without guessing
    // multi-character key names or interpreting arbitrary strings as paste.
    let Some(json) = wire.strip_prefix(PREFIX) else {
        if wire == "click" {
            return Some(Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(x, y),
            )));
        }
        let mut chars = wire.chars();
        let c = chars.next()?;
        return chars
            .next()
            .is_none()
            .then(|| Event::Key(KeyStroke::simple(KeyCode::Char(c))));
    };
    match serde_json::from_str::<Input>(json).ok()? {
        Input::Key {
            code,
            character,
            modifiers: m,
        }
        | Input::FocusKey {
            code,
            character,
            modifiers: m,
            ..
        } if m < 64 => Some(Event::Key(KeyStroke {
            key: decode_key(code, character)?,
            modifiers: Modifiers {
                ctrl: m & 1 != 0,
                alt: m & 2 != 0,
                shift: m & 4 != 0,
                super_key: m & 8 != 0,
                hyper: m & 16 != 0,
                meta: m & 32 != 0,
            },
        })),
        Input::Paste(text) if text.len() <= MAX_PASTE_BYTES => Some(Event::Paste(text)),
        Input::Mouse {
            kind,
            button,
            modifiers: m,
        } if m < 8 => {
            let kind = match kind {
                0 => MouseEventKind::Down(decode_button(button)?),
                1 => MouseEventKind::Up(decode_button(button)?),
                2 => MouseEventKind::Drag(decode_button(button)?),
                3..=7 if button == 0 => match kind {
                    3 => MouseEventKind::Move,
                    4 => MouseEventKind::ScrollUp,
                    5 => MouseEventKind::ScrollDown,
                    6 => MouseEventKind::ScrollLeft,
                    _ => MouseEventKind::ScrollRight,
                },
                _ => return None,
            };
            let mut mouse = MouseEvent::new(kind, Point::new(x, y));
            mouse.modifiers = MouseModifiers {
                ctrl: m & 1 != 0,
                alt: m & 2 != 0,
                shift: m & 4 != 0,
            };
            Some(Event::Mouse(mouse))
        }
        _ => None,
    }
}

const ACK_PREFIX: &str = "presenter-focus-ack-v1:";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FocusAck {
    pub surface_id: String,
    pub revision: u64,
    pub request: String,
    pub handled: bool,
}
pub(crate) fn encode_focus(key: KeyStroke, nonce: u64) -> Option<String> {
    if !matches!(key.key, KeyCode::Tab | KeyCode::Escape) {
        return None;
    }
    let wire = encode(&Event::Key(key))?;
    let Input::Key {
        code,
        character,
        modifiers,
    } = serde_json::from_str(wire.strip_prefix(PREFIX)?).ok()?
    else {
        return None;
    };
    Some(format!(
        "{PREFIX}{}",
        serde_json::to_string(&Input::FocusKey {
            nonce,
            code,
            character,
            modifiers
        })
        .ok()?
    ))
}
pub(crate) fn focus_ack(
    surface_id: &str,
    revision: u64,
    wire: &str,
    handled: bool,
) -> Option<FocusAck> {
    if wire.len() > MAX_WIRE_BYTES || surface_id.len() > 256 {
        return None;
    }
    let Input::FocusKey { .. } = serde_json::from_str(wire.strip_prefix(PREFIX)?).ok()? else {
        return None;
    };
    if !matches!(decode(wire, 0, 0), Some(Event::Key(key)) if matches!(key.key, KeyCode::Tab | KeyCode::Escape))
    {
        return None;
    }
    Some(FocusAck {
        surface_id: surface_id.into(),
        revision,
        request: wire.into(),
        handled,
    })
}
pub(crate) fn encode_ack(ack: &FocusAck) -> Option<String> {
    let wire = format!("{ACK_PREFIX}{}", serde_json::to_string(ack).ok()?);
    (wire.len() <= MAX_WIRE_BYTES).then_some(wire)
}
pub(crate) fn decode_ack(wire: &str) -> Option<FocusAck> {
    if wire.len() > MAX_WIRE_BYTES {
        return None;
    }
    serde_json::from_str(wire.strip_prefix(ACK_PREFIX)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_every_key_and_modifier() {
        let keys = [
            KeyCode::Char('é'),
            KeyCode::Char('\0'),
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Escape,
            KeyCode::Space,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Insert,
            KeyCode::F(0),
            KeyCode::F(255),
        ];
        for key in keys {
            for bits in 0..64 {
                let key = KeyStroke {
                    key,
                    modifiers: Modifiers {
                        ctrl: bits & 1 != 0,
                        alt: bits & 2 != 0,
                        shift: bits & 4 != 0,
                        super_key: bits & 8 != 0,
                        hyper: bits & 16 != 0,
                        meta: bits & 32 != 0,
                    },
                };
                let Event::Key(decoded) = decode(&encode(&Event::Key(key)).unwrap(), 0, 0).unwrap()
                else {
                    panic!("key");
                };
                assert_eq!(decoded, key);
            }
        }
    }
    #[test]
    fn bounded_paste_and_invalid_payloads() {
        let text = "é".repeat(MAX_PASTE_BYTES / 2);
        let Event::Paste(decoded) =
            decode(&encode(&Event::Paste(text.clone())).unwrap(), 0, 0).unwrap()
        else {
            panic!("paste");
        };
        assert_eq!(decoded, text);
        assert!(encode(&Event::Paste("x".repeat(MAX_PASTE_BYTES + 1))).is_none());
        for invalid in [
            "enter",
            "presenter-input-v2:{}",
            "presenter-input-v1:{}",
            "presenter-input-v1:{\"Paste\":\"x\",\"extra\":1}",
        ] {
            assert!(decode(invalid, 0, 0).is_none());
        }
    }
    #[test]
    fn mouse_round_trip_preserves_kind_modifiers_and_owner_coordinates() {
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Middle),
            MouseEventKind::Drag(MouseButton::Other(255)),
            MouseEventKind::Move,
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            let mut mouse = MouseEvent::new(kind, Point::new(90, 90));
            mouse.modifiers = MouseModifiers {
                ctrl: true,
                alt: true,
                shift: true,
            };
            let Event::Mouse(decoded) =
                decode(&encode(&Event::Mouse(mouse)).unwrap(), 4, 5).unwrap()
            else {
                panic!("mouse");
            };
            assert_eq!(decoded.kind, kind);
            assert_eq!(decoded.modifiers, mouse.modifiers);
            assert_eq!(decoded.position, Point::new(4, 5));
        }
    }
}
