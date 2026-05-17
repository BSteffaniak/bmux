//! Crossterm event adapter.
//!
//! Converts crossterm `KeyEvent` types into the bmux-internal `InputEvent`
//! representation, including both the logical `KeyStroke` (for keybind matching)
//! and the raw byte encoding (for PTY forwarding).

use bmux_keyboard::crossterm::crossterm_key_event_to_stroke;
use bmux_keyboard::encode::{KeyEncodingModes, encode_key_with_modes};
use crossterm::event::{Event, KeyEvent as CrosstermKeyEvent, KeyEventKind};

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

    let stroke = crossterm_key_event_to_stroke(key)?;
    let raw = key_event_to_bytes(key, enhanced, modes)?;
    Some(InputEvent::Key(DecodedStroke { stroke, raw }))
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
    let stroke = crossterm_key_event_to_stroke(key)?;
    encode_key_with_modes(&stroke, enhanced, modes)
}
