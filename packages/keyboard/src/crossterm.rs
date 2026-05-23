//! Crossterm event conversion helpers.

use crate::{KeyCode, KeyStroke, Modifiers};
use crossterm::event::{
    KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent, KeyEventKind, KeyModifiers,
};

/// Convert a crossterm key event into a canonical [`KeyStroke`].
///
/// Returns `None` for crossterm key codes that do not have a canonical BMUX key
/// representation.
#[must_use]
pub const fn crossterm_key_event_to_stroke(key: &CrosstermKeyEvent) -> Option<KeyStroke> {
    let modifiers = key.modifiers;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let mut shift = modifiers.contains(KeyModifiers::SHIFT);
    let super_key = modifiers.contains(KeyModifiers::SUPER);
    let hyper = modifiers.contains(KeyModifiers::HYPER);
    let meta = modifiers.contains(KeyModifiers::META);

    let key_code = match key.code {
        CrosstermKeyCode::Char(c) => {
            let normalized = if c.is_ascii_alphabetic() {
                if c.is_ascii_uppercase() {
                    shift = true;
                }
                c.to_ascii_lowercase()
            } else {
                shift = false;
                c
            };
            KeyCode::Char(normalized)
        }
        CrosstermKeyCode::Enter => KeyCode::Enter,
        CrosstermKeyCode::Tab => KeyCode::Tab,
        CrosstermKeyCode::BackTab => {
            shift = true;
            KeyCode::Tab
        }
        CrosstermKeyCode::Backspace => KeyCode::Backspace,
        CrosstermKeyCode::Esc => KeyCode::Escape,
        CrosstermKeyCode::Up => KeyCode::Up,
        CrosstermKeyCode::Down => KeyCode::Down,
        CrosstermKeyCode::Left => KeyCode::Left,
        CrosstermKeyCode::Right => KeyCode::Right,
        CrosstermKeyCode::Home => KeyCode::Home,
        CrosstermKeyCode::End => KeyCode::End,
        CrosstermKeyCode::PageUp => KeyCode::PageUp,
        CrosstermKeyCode::PageDown => KeyCode::PageDown,
        CrosstermKeyCode::Insert => KeyCode::Insert,
        CrosstermKeyCode::Delete => KeyCode::Delete,
        CrosstermKeyCode::F(number) => KeyCode::F(number),
        _ => return None,
    };

    Some(KeyStroke::with_modifiers(
        key_code,
        Modifiers {
            ctrl,
            alt,
            shift,
            super_key,
            hyper,
            meta,
        },
    ))
}

/// Return whether a crossterm key event should be ignored for text/keybind
/// processing because it is a release event.
#[must_use]
pub const fn crossterm_key_event_is_release(key: &CrosstermKeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Release)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_uppercase_char_to_shift_lowercase() {
        let event = CrosstermKeyEvent::new(CrosstermKeyCode::Char('A'), KeyModifiers::NONE);
        let stroke = crossterm_key_event_to_stroke(&event).expect("stroke");
        assert_eq!(stroke.key, KeyCode::Char('a'));
        assert!(stroke.modifiers.shift);
    }

    #[test]
    fn maps_space_to_char_space() {
        let event = CrosstermKeyEvent::new(CrosstermKeyCode::Char(' '), KeyModifiers::NONE);
        let stroke = crossterm_key_event_to_stroke(&event).expect("stroke");
        assert_eq!(stroke.key, KeyCode::Char(' '));
    }

    #[test]
    fn keeps_alt_arrow_modifier() {
        let event = CrosstermKeyEvent::new(CrosstermKeyCode::Left, KeyModifiers::ALT);
        let stroke = crossterm_key_event_to_stroke(&event).expect("stroke");
        assert_eq!(stroke.key, KeyCode::Left);
        assert!(stroke.modifiers.alt);
    }
}
