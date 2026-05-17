//! Compatibility wrappers for key chord parsing.

use anyhow::Result;
use bmux_keyboard::{KeyStroke, parse_key_chord, parse_key_stroke};

/// Parse a space-separated key chord string into a sequence of [`KeyStroke`]s.
pub(super) fn parse_chord(value: &str) -> Result<Vec<KeyStroke>> {
    parse_key_chord(value).map_err(Into::into)
}

/// Parse a single key stroke string like `ctrl+a` or `shift+arrow_up`.
pub(super) fn parse_stroke(value: &str) -> Result<KeyStroke> {
    parse_key_stroke(value).map_err(Into::into)
}
