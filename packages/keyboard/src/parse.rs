//! Human-readable key binding parsing.

use crate::{KeyCode, KeyStroke, Modifiers};
use std::error::Error;
use std::fmt;

/// Error returned when parsing a key stroke or chord fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseKeyError {
    message: String,
}

impl ParseKeyError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ParseKeyError {}

/// Parse a space-separated key chord string into a sequence of key strokes.
///
/// # Errors
///
/// Returns an error when the chord is empty or any stroke contains an unknown
/// modifier or key token.
pub fn parse_key_chord(value: &str) -> Result<Vec<KeyStroke>, ParseKeyError> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.is_empty() {
        return Err(ParseKeyError::new("empty key chord"));
    }

    parts.into_iter().map(parse_key_stroke).collect()
}

/// Parse a single key stroke string like `ctrl+a` or `shift+arrow_up`.
///
/// # Errors
///
/// Returns an error when the stroke is empty or contains an unknown modifier or
/// key token.
pub fn parse_key_stroke(value: &str) -> Result<KeyStroke, ParseKeyError> {
    let lowered = value.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        return Err(ParseKeyError::new("empty key stroke"));
    }

    if lowered == "+" || lowered == "-" {
        return Ok(KeyStroke::with_modifiers(
            parse_key_token(&lowered)?,
            Modifiers::NONE,
        ));
    }

    let tokens: Vec<&str> = lowered.split('+').collect();
    if tokens.is_empty() {
        return Err(ParseKeyError::new(format!("invalid stroke: {value}")));
    }

    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut super_key = false;
    let mut hyper = false;
    let mut meta = false;

    for modifier in &tokens[..tokens.len() - 1] {
        match *modifier {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "super" | "cmd" | "command" | "win" | "windows" => super_key = true,
            "hyper" => hyper = true,
            "meta" => meta = true,
            unknown => {
                return Err(ParseKeyError::new(format!(
                    "unknown modifier '{unknown}' in '{value}'"
                )));
            }
        }
    }

    Ok(KeyStroke::with_modifiers(
        parse_key_token(tokens[tokens.len() - 1])?,
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

fn parse_key_token(value: &str) -> Result<KeyCode, ParseKeyError> {
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
        token if token.starts_with('f') => token[1..]
            .parse::<u8>()
            .map(KeyCode::F)
            .map_err(|_| ParseKeyError::new(format!("invalid function key '{token}'"))),
        token if token.len() == 1 => Ok(KeyCode::Char(token.chars().next().unwrap_or_default())),
        _ => Err(ParseKeyError::new(format!("unknown key '{value}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifier_stroke() {
        let stroke = parse_key_stroke("alt+left").expect("parse alt-left");
        assert_eq!(stroke.key, KeyCode::Left);
        assert!(stroke.modifiers.alt);
    }

    #[test]
    fn parses_chord() {
        let chord = parse_key_chord("ctrl+a d").expect("parse chord");
        assert_eq!(chord.len(), 2);
        assert!(chord[0].modifiers.ctrl);
        assert_eq!(chord[1].key, KeyCode::Char('d'));
    }
}
