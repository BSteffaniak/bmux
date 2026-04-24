//! Shared helpers for turning a [`crate::scene_protocol::BorderGlyphs`]
//! choice into the six concrete runes required to paint a 1-cell
//! border.
//!
//! Both the attach renderer and the decoration plugin consume this
//! table. Keeping it in the scene-protocol crate (adjacent to the
//! generated [`crate::scene_protocol::BorderGlyphs`] variant) ensures
//! they stay in lock-step when new preset variants are added.

use crate::scene_protocol::BorderGlyphs;

/// Six-rune border glyph set used when painting a 1-cell border.
///
/// String slices point at either compile-time static literals (for
/// the built-in presets) or at the owned strings carried by a
/// [`BorderGlyphs::Custom`] variant. The lifetime parameter captures
/// whichever is appropriate for the caller.
#[derive(Clone, Copy, Debug)]
pub struct BorderGlyphSet<'a> {
    pub top_left: &'a str,
    pub top_right: &'a str,
    pub bottom_left: &'a str,
    pub bottom_right: &'a str,
    pub horizontal: &'a str,
    pub vertical: &'a str,
}

/// Expand a [`BorderGlyphs`] preset into its six runes.
///
/// Returns `None` for [`BorderGlyphs::None`] (explicit no-border) and
/// [`BorderGlyphs::Custom`] (callers must look at the variant's owned
/// strings directly; use [`border_glyphs_corners_or_custom`] when you
/// want both cases handled with one call).
#[must_use]
#[allow(clippy::ref_option)] // Match arms borrow the `&BorderGlyphs` input directly.
pub const fn border_glyphs_corners(choice: &BorderGlyphs) -> Option<BorderGlyphSet<'static>> {
    let set = match choice {
        BorderGlyphs::None | BorderGlyphs::Custom { .. } => return None,
        BorderGlyphs::Ascii => BorderGlyphSet {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            horizontal: "-",
            vertical: "|",
        },
        BorderGlyphs::AsciiFocused => BorderGlyphSet {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            horizontal: "=",
            vertical: "|",
        },
        BorderGlyphs::AsciiZoomed => BorderGlyphSet {
            top_left: "#",
            top_right: "#",
            bottom_left: "#",
            bottom_right: "#",
            horizontal: "=",
            vertical: "\u{2551}",
        },
        BorderGlyphs::SingleLine => BorderGlyphSet {
            top_left: "\u{250c}",
            top_right: "\u{2510}",
            bottom_left: "\u{2514}",
            bottom_right: "\u{2518}",
            horizontal: "\u{2500}",
            vertical: "\u{2502}",
        },
        BorderGlyphs::DoubleLine => BorderGlyphSet {
            top_left: "\u{2554}",
            top_right: "\u{2557}",
            bottom_left: "\u{255a}",
            bottom_right: "\u{255d}",
            horizontal: "\u{2550}",
            vertical: "\u{2551}",
        },
        BorderGlyphs::Rounded => BorderGlyphSet {
            top_left: "\u{256d}",
            top_right: "\u{256e}",
            bottom_left: "\u{2570}",
            bottom_right: "\u{256f}",
            horizontal: "\u{2500}",
            vertical: "\u{2502}",
        },
        BorderGlyphs::Thick => BorderGlyphSet {
            top_left: "\u{250f}",
            top_right: "\u{2513}",
            bottom_left: "\u{2517}",
            bottom_right: "\u{251b}",
            horizontal: "\u{2501}",
            vertical: "\u{2503}",
        },
        BorderGlyphs::HeavyDouble => BorderGlyphSet {
            top_left: "\u{250f}",
            top_right: "\u{2513}",
            bottom_left: "\u{2517}",
            bottom_right: "\u{251b}",
            horizontal: "\u{2550}",
            vertical: "\u{2551}",
        },
        BorderGlyphs::Dashed => BorderGlyphSet {
            top_left: "\u{250c}",
            top_right: "\u{2510}",
            bottom_left: "\u{2514}",
            bottom_right: "\u{2518}",
            horizontal: "\u{254c}",
            vertical: "\u{254e}",
        },
        BorderGlyphs::Dotted => BorderGlyphSet {
            top_left: "\u{250c}",
            top_right: "\u{2510}",
            bottom_left: "\u{2514}",
            bottom_right: "\u{2518}",
            horizontal: "\u{2508}",
            vertical: "\u{250a}",
        },
        BorderGlyphs::NerdPowerline => BorderGlyphSet {
            top_left: "\u{e0b6}",
            top_right: "\u{e0b4}",
            bottom_left: "\u{e0b6}",
            bottom_right: "\u{e0b4}",
            horizontal: " ",
            vertical: " ",
        },
    };
    Some(set)
}

/// Expand a [`BorderGlyphs`] into its six runes, handling
/// [`BorderGlyphs::Custom`] by borrowing the variant's owned strings.
/// Returns `None` only for [`BorderGlyphs::None`].
#[must_use]
pub fn border_glyphs_corners_or_custom(choice: &BorderGlyphs) -> Option<BorderGlyphSet<'_>> {
    match choice {
        BorderGlyphs::None => None,
        BorderGlyphs::Custom {
            top_left,
            top_right,
            bottom_left,
            bottom_right,
            horizontal,
            vertical,
        } => Some(BorderGlyphSet {
            top_left,
            top_right,
            bottom_left,
            bottom_right,
            horizontal,
            vertical,
        }),
        other => border_glyphs_corners(other),
    }
}
