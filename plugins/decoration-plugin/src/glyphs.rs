//! Shared border-glyph name parsing used by both the theme loader
//! (TOML strings like `"single-line"` or `"rounded"`) and the Lua
//! scripting layer (script-supplied `"glyphs" = "single_line"`).
//!
//! Accepts both `kebab-case` and `snake_case` variants for
//! compatibility with existing theme files and script conventions.
//! Unknown values fall back to [`BorderGlyphs::SingleLine`] so typos
//! don't produce invisible borders.

use bmux_scene_protocol::scene_protocol::BorderGlyphs;

/// Parse a border-glyph preset name into the scene-protocol variant.
///
/// Accepts `kebab-case`, `snake_case`, and mixed case; normalises to
/// lowercase + `_` before matching. The `"double"` shorthand is
/// accepted as an alias for `"double_line"` to match the preset name
/// convention used in theme files (see `pulse-demo.toml`).
#[must_use]
pub fn parse_border_glyphs(name: &str) -> BorderGlyphs {
    let normalized = name.trim().to_ascii_lowercase().replace('-', "_");
    match normalized.as_str() {
        "none" => BorderGlyphs::None,
        "ascii" => BorderGlyphs::Ascii,
        "ascii_focused" => BorderGlyphs::AsciiFocused,
        "ascii_zoomed" => BorderGlyphs::AsciiZoomed,
        "double_line" | "double" => BorderGlyphs::DoubleLine,
        "rounded" => BorderGlyphs::Rounded,
        "thick" => BorderGlyphs::Thick,
        "heavy_double" => BorderGlyphs::HeavyDouble,
        "dashed" => BorderGlyphs::Dashed,
        "dotted" => BorderGlyphs::Dotted,
        "nerd_powerline" => BorderGlyphs::NerdPowerline,
        // Every other input (including empty and unknown) falls back
        // to single-line so typos in theme files produce visible
        // borders instead of silently vanishing.
        _ => BorderGlyphs::SingleLine,
    }
}

/// Expand a theme's `style == "custom"` configuration into the
/// [`BorderGlyphs::Custom`] variant. Expects exactly six entries:
/// top-left, top-right, bottom-left, bottom-right, horizontal,
/// vertical. Falls back to [`BorderGlyphs::SingleLine`] on malformed
/// input so an incomplete custom entry still produces visible output.
#[must_use]
pub fn parse_custom_glyphs(runes: &[String]) -> BorderGlyphs {
    if runes.len() != 6 {
        return BorderGlyphs::SingleLine;
    }
    BorderGlyphs::Custom {
        top_left: runes[0].clone(),
        top_right: runes[1].clone(),
        bottom_left: runes[2].clone(),
        bottom_right: runes[3].clone(),
        horizontal: runes[4].clone(),
        vertical: runes[5].clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_border_glyphs_handles_kebab_and_snake() {
        assert_eq!(parse_border_glyphs("single-line"), BorderGlyphs::SingleLine);
        assert_eq!(parse_border_glyphs("single_line"), BorderGlyphs::SingleLine);
        assert_eq!(parse_border_glyphs("Double-Line"), BorderGlyphs::DoubleLine);
        assert_eq!(parse_border_glyphs("double"), BorderGlyphs::DoubleLine);
        assert_eq!(parse_border_glyphs("thick"), BorderGlyphs::Thick);
        assert_eq!(parse_border_glyphs("rounded"), BorderGlyphs::Rounded);
        assert_eq!(
            parse_border_glyphs("nerd-powerline"),
            BorderGlyphs::NerdPowerline
        );
    }

    #[test]
    fn parse_border_glyphs_falls_back_on_unknown() {
        assert_eq!(
            parse_border_glyphs("not-a-preset"),
            BorderGlyphs::SingleLine
        );
        assert_eq!(parse_border_glyphs(""), BorderGlyphs::SingleLine);
    }

    #[test]
    fn parse_custom_glyphs_requires_exactly_six_entries() {
        let runes: Vec<String> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        match parse_custom_glyphs(&runes) {
            BorderGlyphs::Custom {
                top_left,
                top_right,
                bottom_left,
                bottom_right,
                horizontal,
                vertical,
            } => {
                assert_eq!(top_left, "a");
                assert_eq!(top_right, "b");
                assert_eq!(bottom_left, "c");
                assert_eq!(bottom_right, "d");
                assert_eq!(horizontal, "e");
                assert_eq!(vertical, "f");
            }
            _ => panic!("expected Custom variant"),
        }
    }

    #[test]
    fn parse_custom_glyphs_falls_back_on_wrong_length() {
        let runes = vec!["a".to_string(), "b".to_string()];
        assert_eq!(parse_custom_glyphs(&runes), BorderGlyphs::SingleLine);
        let runes: Vec<String> = Vec::new();
        assert_eq!(parse_custom_glyphs(&runes), BorderGlyphs::SingleLine);
    }
}
