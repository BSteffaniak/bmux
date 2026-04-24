//! ANSI SGR (Select Graphic Rendition) helpers for
//! [`bmux_scene_protocol::scene_protocol::Style`] values.
//!
//! Given a `Style`, [`scene_style_sgr_prelude`] returns the escape
//! sequence that sets the terminal to that style (empty string when
//! every attribute is at its default). Consumers pair this with a
//! trailing `\x1b[0m` when the styled run ends, so style flags don't
//! bleed into subsequent text.

use bmux_scene_protocol::scene_protocol::{Color, NamedColor, Style};

/// Translate a [`Style`] to its ANSI SGR prelude (e.g. `"\x1b[1;33m"`).
///
/// Returns an empty string when every attribute is at its default,
/// so diff-sensitive callers can skip the write entirely.
#[must_use]
pub fn scene_style_sgr_prelude(style: &Style) -> String {
    let mut params: Vec<String> = Vec::new();
    if style.bold {
        params.push("1".to_string());
    }
    if style.dim {
        params.push("2".to_string());
    }
    if style.italic {
        params.push("3".to_string());
    }
    if style.underline {
        params.push("4".to_string());
    }
    if style.blink {
        params.push("5".to_string());
    }
    if style.reverse {
        params.push("7".to_string());
    }
    if style.strikethrough {
        params.push("9".to_string());
    }
    if let Some(fg) = style.fg.as_ref() {
        params.push(scene_color_to_sgr(fg, false));
    }
    if let Some(bg) = style.bg.as_ref() {
        params.push(scene_color_to_sgr(bg, true));
    }
    if params.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", params.join(";"))
    }
}

/// Map a [`Color`] value to its SGR parameter string for either
/// foreground (`background = false`) or background (`background = true`).
///
/// `Default` and `Reset` both map to the "reset" parameter on that
/// channel. `Indexed` emits `38;5;n` / `48;5;n`; `Rgb` emits
/// `38;2;r;g;b` / `48;2;r;g;b`.
fn scene_color_to_sgr(color: &Color, background: bool) -> String {
    match color {
        Color::Default | Color::Reset => if background { "49" } else { "39" }.to_string(),
        Color::Named { name } => named_color_to_sgr(*name, background).to_string(),
        Color::Indexed { index } => {
            if background {
                format!("48;5;{index}")
            } else {
                format!("38;5;{index}")
            }
        }
        Color::Rgb { r, g, b } => {
            if background {
                format!("48;2;{r};{g};{b}")
            } else {
                format!("38;2;{r};{g};{b}")
            }
        }
    }
}

/// Map a 16-colour palette entry to its foreground SGR code (or, when
/// `background == true`, its background equivalent by adding 10).
const fn named_color_to_sgr(name: NamedColor, background: bool) -> u16 {
    let fg = match name {
        NamedColor::Black => 30,
        NamedColor::Red => 31,
        NamedColor::Green => 32,
        NamedColor::Yellow => 33,
        NamedColor::Blue => 34,
        NamedColor::Magenta => 35,
        NamedColor::Cyan => 36,
        NamedColor::White => 37,
        NamedColor::BrightBlack => 90,
        NamedColor::BrightRed => 91,
        NamedColor::BrightGreen => 92,
        NamedColor::BrightYellow => 93,
        NamedColor::BrightBlue => 94,
        NamedColor::BrightMagenta => 95,
        NamedColor::BrightCyan => 96,
        NamedColor::BrightWhite => 97,
    };
    if background { fg + 10 } else { fg }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_style() -> Style {
        Style {
            fg: None,
            bg: None,
            bold: false,
            underline: false,
            italic: false,
            reverse: false,
            dim: false,
            blink: false,
            strikethrough: false,
        }
    }

    #[test]
    fn empty_style_produces_empty_prelude() {
        assert_eq!(scene_style_sgr_prelude(&default_style()), "");
    }

    #[test]
    fn bold_yellow_emits_expected_sgr() {
        let mut style = default_style();
        style.bold = true;
        style.fg = Some(Color::Named {
            name: NamedColor::BrightYellow,
        });
        assert_eq!(scene_style_sgr_prelude(&style), "\x1b[1;93m");
    }

    #[test]
    fn rgb_foreground_emits_truecolor_sgr() {
        let mut style = default_style();
        style.fg = Some(Color::Rgb {
            r: 57,
            g: 255,
            b: 20,
        });
        assert_eq!(scene_style_sgr_prelude(&style), "\x1b[38;2;57;255;20m");
    }
}
