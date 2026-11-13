//! Terminal key event adapter.
//!
//! Converts terminal `KeyEvent` types into the bmux-internal `InputEvent`
//! representation, including both the logical `KeyStroke` (for keybind matching)
//! and the raw byte encoding (for PTY forwarding).

use bmux_terminal::keyboard::key_event_to_stroke;
use bmux_keyboard::encode::{KeyEncodingModes, encode_key_with_modes};
use bmux_terminal::event::{Event, KeyEvent as TermKeyEvent, KeyEventKind};

use super::{DecodedStroke, InputEvent};

/// Convert a terminal [`Event`] into an [`InputEvent`], if applicable.
pub(super) fn terminal_event_to_input_event(
    event: &Event,
    enhanced: bool,
    modes: KeyEncodingModes,
) -> Option<InputEvent> {
    match event {
        Event::Key(key) => key_event_to_input_event(key, enhanced, modes),
        _ => None,
    }
}

/// Convert a terminal [`KeyEvent`] into an [`InputEvent`].
///
/// Filters out `Release` events. Produces both the logical `KeyStroke`
/// (for keybind matching) and the raw byte encoding (for PTY forwarding).
fn key_event_to_input_event(
    key: &TermKeyEvent,
    enhanced: bool,
    modes: KeyEncodingModes,
) -> Option<InputEvent> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let stroke = key_event_to_stroke(key)?;
    let raw = key_event_to_bytes(key, enhanced, modes)?;
    Some(InputEvent::Key(DecodedStroke {
        kind: key.kind,
        stroke,
        raw,
    }))
}

/// Encode a terminal `KeyEvent` to raw bytes for PTY forwarding.
///
/// Delegates to `bmux_keyboard::encode::encode_key_with_modes()` which uses CSI u
/// encoding when `enhanced` is true and needed, or legacy VT encoding
/// otherwise.
fn key_event_to_bytes(
    key: &TermKeyEvent,
    enhanced: bool,
    modes: KeyEncodingModes,
) -> Option<Vec<u8>> {
    let stroke = key_event_to_stroke(key)?;
    encode_key_with_modes(&stroke, enhanced, modes)
}
