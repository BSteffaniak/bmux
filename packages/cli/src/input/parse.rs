//! Config string parsing for key chords and strokes.
//!
//! Parses human-readable key binding strings like "ctrl+a", "`shift+arrow_up`",
//! or "ctrl+a d" into `bmux_keyboard` types.

use anyhow::{Result, anyhow, bail};
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

/// Parse a space-separated key chord string into a sequence of [`KeyStroke`]s.
///
/// Example: `"ctrl+a d"` -> `[Ctrl+A, D]`
pub(super) fn parse_chord(value: &str) -> Result<Vec<KeyStroke>> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.is_empty() {
        bail!("empty key chord");
    }

    parts.into_iter().map(parse_stroke).collect()
}

/// Parse a single key stroke string like "ctrl+a" or "`shift+arrow_up`".
pub(super) fn parse_stroke(value: &str) -> Result<KeyStroke> {
    let lowered = value.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        bail!("empty key stroke");
    }

    if lowered == "+" || lowered == "-" {
        return Ok(KeyStroke::with_modifiers(
            parse_key_token(&lowered)?,
            Modifiers::NONE,
        ));
    }

    let tokens: Vec<&str> = lowered.split('+').collect();
    if tokens.is_empty() {
        bail!("invalid stroke: {value}");
    }

    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut super_key = false;

    for modifier in &tokens[..tokens.len() - 1] {
        match *modifier {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "super" => super_key = true,
            unknown => bail!("unknown modifier '{unknown}' in '{value}'"),
        }
    }

    Ok(KeyStroke::with_modifiers(
        parse_key_token(tokens[tokens.len() - 1])?,
        Modifiers {
            ctrl,
            alt,
            shift,
            super_key,
        },
    ))
}

/// Parse a key token string (the part after modifiers) into a [`KeyCode`].
fn parse_key_token(value: &str) -> Result<KeyCode> {
    let normalized = match value {
        "esc" => "escape",
        "up" => "arrow_up",
        "down" => "arrow_down",
        "left" => "arrow_left",
        "right" => "arrow_right",
        "pgup" => "page_up",
        "pgdn" => "page_down",
        "+" => "plus",
        "-" => "minus",
        _ => value,
    };

    match normalized {
        "enter" => Ok(KeyCode::Enter),
        "escape" => Ok(KeyCode::Escape),
        "tab" => Ok(KeyCode::Tab),
        "backspace" => Ok(KeyCode::Backspace),
        "space" => Ok(KeyCode::Space),
        "arrow_up" => Ok(KeyCode::Up),
        "arrow_down" => Ok(KeyCode::Down),
        "arrow_left" => Ok(KeyCode::Left),
        "arrow_right" => Ok(KeyCode::Right),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "page_up" => Ok(KeyCode::PageUp),
        "page_down" => Ok(KeyCode::PageDown),
        "insert" => Ok(KeyCode::Insert),
        "delete" => Ok(KeyCode::Delete),
        "plus" => Ok(KeyCode::Char('+')),
        "minus" => Ok(KeyCode::Char('-')),
        "question" => Ok(KeyCode::Char('?')),
        token if token.starts_with('f') => {
            let number = token[1..]
                .parse::<u8>()
                .map_err(|_| anyhow!("invalid function key '{token}'"))?;
            Ok(KeyCode::F(number))
        }
        token if token.len() == 1 => Ok(KeyCode::Char(token.chars().next().unwrap_or_default())),
        _ => bail!("unknown key '{value}'"),
    }
}
