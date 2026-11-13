//! Conversion between terminal key events and canonical bmux key strokes.

use bmux_keyboard::{KeyCode as StrokeKey, KeyStroke, Modifiers};

use crate::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// The canonical [`KeyStroke`] for a key event.
///
/// Alphabetic characters are lowercased with Shift recorded as a modifier;
/// for other characters Shift is dropped because it is already reflected in
/// the character. Shift+Tab becomes Tab with Shift. Keys without a canonical
/// representation (lock, media, and lone modifier keys) yield `None`.
#[must_use]
pub const fn key_event_to_stroke(key: &KeyEvent) -> Option<KeyStroke> {
    let modifiers = key.modifiers;
    let mut shift = modifiers.contains(KeyModifiers::SHIFT);
    let code = match key.code {
        KeyCode::Char(character) => {
            let normalized = if character.is_ascii_alphabetic() {
                if character.is_ascii_uppercase() {
                    shift = true;
                }
                character.to_ascii_lowercase()
            } else {
                shift = false;
                character
            };
            StrokeKey::Char(normalized)
        }
        KeyCode::Enter => StrokeKey::Enter,
        KeyCode::Tab => StrokeKey::Tab,
        KeyCode::BackTab => {
            shift = true;
            StrokeKey::Tab
        }
        KeyCode::Backspace => StrokeKey::Backspace,
        KeyCode::Esc => StrokeKey::Escape,
        KeyCode::Up => StrokeKey::Up,
        KeyCode::Down => StrokeKey::Down,
        KeyCode::Left => StrokeKey::Left,
        KeyCode::Right => StrokeKey::Right,
        KeyCode::Home => StrokeKey::Home,
        KeyCode::End => StrokeKey::End,
        KeyCode::PageUp => StrokeKey::PageUp,
        KeyCode::PageDown => StrokeKey::PageDown,
        KeyCode::Insert => StrokeKey::Insert,
        KeyCode::Delete => StrokeKey::Delete,
        KeyCode::F(number) => StrokeKey::F(number),
        _ => return None,
    };
    Some(KeyStroke::with_modifiers(
        code,
        Modifiers {
            ctrl: modifiers.contains(KeyModifiers::CONTROL),
            alt: modifiers.contains(KeyModifiers::ALT),
            shift,
            super_key: modifiers.contains(KeyModifiers::SUPER),
            hyper: modifiers.contains(KeyModifiers::HYPER),
            meta: modifiers.contains(KeyModifiers::META),
        },
    ))
}

/// Whether a key event is a release, which carries no typing action.
#[must_use]
pub const fn key_event_is_release(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Release)
}

/// The terminal key code for a canonical key; Space becomes `Char(' ')`.
#[must_use]
pub const fn stroke_key_to_code(key: StrokeKey) -> KeyCode {
    match key {
        StrokeKey::Char(character) => KeyCode::Char(character),
        StrokeKey::Space => KeyCode::Char(' '),
        StrokeKey::Enter => KeyCode::Enter,
        StrokeKey::Tab => KeyCode::Tab,
        StrokeKey::Backspace => KeyCode::Backspace,
        StrokeKey::Delete => KeyCode::Delete,
        StrokeKey::Escape => KeyCode::Esc,
        StrokeKey::Up => KeyCode::Up,
        StrokeKey::Down => KeyCode::Down,
        StrokeKey::Left => KeyCode::Left,
        StrokeKey::Right => KeyCode::Right,
        StrokeKey::Home => KeyCode::Home,
        StrokeKey::End => KeyCode::End,
        StrokeKey::PageUp => KeyCode::PageUp,
        StrokeKey::PageDown => KeyCode::PageDown,
        StrokeKey::Insert => KeyCode::Insert,
        StrokeKey::F(number) => KeyCode::F(number),
    }
}

/// The terminal modifiers for canonical modifiers.
#[must_use]
pub const fn modifiers_to_key_modifiers(modifiers: Modifiers) -> KeyModifiers {
    let mut result = KeyModifiers::NONE;
    if modifiers.ctrl {
        result.insert(KeyModifiers::CONTROL);
    }
    if modifiers.alt {
        result.insert(KeyModifiers::ALT);
    }
    if modifiers.shift {
        result.insert(KeyModifiers::SHIFT);
    }
    if modifiers.super_key {
        result.insert(KeyModifiers::SUPER);
    }
    if modifiers.hyper {
        result.insert(KeyModifiers::HYPER);
    }
    if modifiers.meta {
        result.insert(KeyModifiers::META);
    }
    result
}

/// A key press event for a canonical stroke.
#[must_use]
pub const fn stroke_to_key_event(stroke: KeyStroke) -> KeyEvent {
    KeyEvent::new(
        stroke_key_to_code(stroke.key),
        modifiers_to_key_modifiers(stroke.modifiers),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_uppercase_char_to_shift_lowercase() {
        let event = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE);
        let stroke = key_event_to_stroke(&event).expect("stroke");
        assert_eq!(stroke.key, StrokeKey::Char('a'));
        assert!(stroke.modifiers.shift);
    }

    #[test]
    fn drops_shift_from_non_alphabetic_characters() {
        let event = KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT);
        let stroke = key_event_to_stroke(&event).expect("stroke");
        assert_eq!(stroke.key, StrokeKey::Char('!'));
        assert!(!stroke.modifiers.shift);
    }

    #[test]
    fn maps_space_to_char_space() {
        let event = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        let stroke = key_event_to_stroke(&event).expect("stroke");
        assert_eq!(stroke.key, StrokeKey::Char(' '));
    }

    #[test]
    fn keeps_alt_arrow_modifier_and_back_tab_shift() {
        let stroke =
            key_event_to_stroke(&KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)).expect("stroke");
        assert_eq!(stroke.key, StrokeKey::Left);
        assert!(stroke.modifiers.alt);
        let stroke = key_event_to_stroke(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE))
            .expect("stroke");
        assert_eq!(stroke.key, StrokeKey::Tab);
        assert!(stroke.modifiers.shift);
    }

    #[test]
    fn keys_without_strokes_are_skipped() {
        assert!(key_event_to_stroke(&KeyEvent::new(KeyCode::CapsLock, KeyModifiers::NONE)).is_none());
        assert!(key_event_to_stroke(&KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)).is_none());
    }

    #[test]
    fn strokes_round_trip_through_key_events() {
        let stroke = KeyStroke::with_modifiers(
            StrokeKey::Char('x'),
            Modifiers {
                ctrl: true,
                super_key: true,
                meta: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(key_event_to_stroke(&stroke_to_key_event(stroke)), Some(stroke));
        assert_eq!(
            stroke_to_key_event(KeyStroke::simple(StrokeKey::Space)).code,
            KeyCode::Char(' ')
        );
    }

    #[test]
    fn release_detection() {
        let release =
            KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Release);
        assert!(key_event_is_release(&release));
        assert!(!key_event_is_release(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    }
}
