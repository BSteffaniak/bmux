//! Crossterm event adapter.
//!
//! Converts crossterm `KeyEvent` types into the bmux-internal `InputEvent`
//! representation, including both the logical `KeyStroke` (for keybind matching)
//! and the raw byte encoding (for PTY forwarding).

use bmux_keyboard::encode::{KeyEncodingModes, encode_key_with_modes};
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use crossterm::event::{
    Event, KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent, KeyEventKind, KeyModifiers,
};

use super::{DecodedStroke, InputEvent};

/// Convert a crossterm [`Event`] into an [`InputEvent`], if applicable.
pub(super) fn crossterm_event_to_input_event(
    event: &Event,
    enhanced: bool,
    modes: KeyEncodingModes,
) -> Option<InputEvent> {
    match event {
        Event::Key(key) => key_event_to_input_event(key, enhanced, modes),
        _ => None,
    }
}

/// Convert a crossterm [`KeyEvent`] into an [`InputEvent`].
///
/// Filters out `Release` events. Produces both the logical `KeyStroke`
/// (for keybind matching) and the raw byte encoding (for PTY forwarding).
fn key_event_to_input_event(
    key: &CrosstermKeyEvent,
    enhanced: bool,
    modes: KeyEncodingModes,
) -> Option<InputEvent> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let stroke = key_event_to_stroke(key)?;
    let raw = key_event_to_bytes(key, enhanced, modes)?;
    Some(InputEvent::Key(DecodedStroke { stroke, raw }))
}

/// Convert a crossterm `KeyEvent` into a `bmux_keyboard::KeyStroke`.
pub const fn key_event_to_stroke(key: &CrosstermKeyEvent) -> Option<KeyStroke> {
    let modifiers = key.modifiers;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let mut shift = modifiers.contains(KeyModifiers::SHIFT);
    let super_key = modifiers.contains(KeyModifiers::SUPER);

    let key_code = match key.code {
        CrosstermKeyCode::Char(c) => {
            let normalized = if c.is_ascii_alphabetic() {
                if c.is_ascii_uppercase() {
                    shift = true;
                }
                c.to_ascii_lowercase()
            } else {
                // Symbol keys often arrive from crossterm with SHIFT set (e.g. '%' and '"').
                // Bindings for literal symbols are stored without SHIFT, so we normalize them.
                shift = false;
                c
            };
            KeyCode::Char(normalized)
        }
        CrosstermKeyCode::Enter => KeyCode::Enter,
        CrosstermKeyCode::Tab => KeyCode::Tab,
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
        },
    ))
}

/// Encode a crossterm `KeyEvent` to raw bytes for PTY forwarding.
///
/// Delegates to `bmux_keyboard::encode::encode_key_with_modes()` which uses CSI u
/// encoding when `enhanced` is true and needed, or legacy VT encoding
/// otherwise.
fn key_event_to_bytes(
    key: &CrosstermKeyEvent,
    enhanced: bool,
    modes: KeyEncodingModes,
) -> Option<Vec<u8>> {
    let stroke = key_event_to_stroke(key)?;
    encode_key_with_modes(&stroke, enhanced, modes)
}
