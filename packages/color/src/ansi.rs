//! Mapping colors onto the standard xterm 256- and 16-color palettes.
//!
//! The 16 base colors are terminal-configurable, so their RGB values here are
//! the xterm defaults. Callers that know the terminal's actual palette (for
//! example, from an OSC 4 query) should use [`nearest_in`] with that palette.

use std::sync::OnceLock;

use crate::{Oklab, Rgb};

/// Default xterm RGB values for ANSI indices 0–15.
pub const XTERM_16: [Rgb; 16] = [
    Rgb::new(0x00, 0x00, 0x00),
    Rgb::new(0xcd, 0x00, 0x00),
    Rgb::new(0x00, 0xcd, 0x00),
    Rgb::new(0xcd, 0xcd, 0x00),
    Rgb::new(0x00, 0x00, 0xee),
    Rgb::new(0xcd, 0x00, 0xcd),
    Rgb::new(0x00, 0xcd, 0xcd),
    Rgb::new(0xe5, 0xe5, 0xe5),
    Rgb::new(0x7f, 0x7f, 0x7f),
    Rgb::new(0xff, 0x00, 0x00),
    Rgb::new(0x00, 0xff, 0x00),
    Rgb::new(0xff, 0xff, 0x00),
    Rgb::new(0x5c, 0x5c, 0xff),
    Rgb::new(0xff, 0x00, 0xff),
    Rgb::new(0x00, 0xff, 0xff),
    Rgb::new(0xff, 0xff, 0xff),
];

/// Channel levels of the 6×6×6 color cube (indices 16–231).
const CUBE_LEVELS: [u8; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];

/// Return the default xterm RGB value of a 256-color palette index.
#[must_use]
pub const fn xterm_256(index: u8) -> Rgb {
    match index {
        0..=15 => XTERM_16[index as usize],
        16..=231 => {
            let offset = index - 16;
            Rgb::new(
                CUBE_LEVELS[(offset / 36) as usize],
                CUBE_LEVELS[((offset / 6) % 6) as usize],
                CUBE_LEVELS[(offset % 6) as usize],
            )
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            Rgb::new(level, level, level)
        }
    }
}

/// Return the perceptually nearest 256-color index, considering only the
/// fixed color cube and grayscale ramp (16–255).
///
/// Indices 0–15 are excluded because terminals commonly redefine them, so
/// matching against their defaults would produce unpredictable output.
#[must_use]
pub fn nearest_256(color: Rgb) -> u8 {
    static EXTENDED: OnceLock<[Oklab; 240]> = OnceLock::new();
    let palette = EXTENDED.get_or_init(|| {
        std::array::from_fn(|offset| {
            #[allow(clippy::cast_possible_truncation)] // offset < 240, so 16 + offset <= 255.
            let index = 16 + offset as u8;
            Oklab::from_rgb(xterm_256(index))
        })
    });
    let target = Oklab::from_rgb(color);
    let offset = palette
        .iter()
        .enumerate()
        .min_by(|a, b| {
            a.1.distance_squared(target)
                .total_cmp(&b.1.distance_squared(target))
        })
        .map_or(0, |(offset, _)| offset);
    #[allow(clippy::cast_possible_truncation)] // offset < 240.
    let index = 16 + offset as u8;
    index
}

/// Return the perceptually nearest ANSI 16-color index using xterm defaults.
#[must_use]
pub fn nearest_16(color: Rgb) -> u8 {
    #[allow(clippy::cast_possible_truncation)] // The palette has exactly 16 entries.
    let index = nearest_in(color, &XTERM_16).unwrap_or(0) as u8;
    index
}

/// Return the index of the perceptually nearest color in `palette`, or `None`
/// when `palette` is empty. Ties resolve to the lowest index.
#[must_use]
pub fn nearest_in(color: Rgb, palette: &[Rgb]) -> Option<usize> {
    let target = Oklab::from_rgb(color);
    palette
        .iter()
        .enumerate()
        .map(|(index, candidate)| (index, Oklab::from_rgb(*candidate).distance_squared(target)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_boundaries_match_xterm() {
        assert_eq!(xterm_256(16), Rgb::new(0, 0, 0));
        assert_eq!(xterm_256(231), Rgb::new(0xff, 0xff, 0xff));
        assert_eq!(xterm_256(196), Rgb::new(0xff, 0, 0));
        assert_eq!(xterm_256(232), Rgb::new(8, 8, 8));
        assert_eq!(xterm_256(255), Rgb::new(0xee, 0xee, 0xee));
        assert_eq!(xterm_256(9), XTERM_16[9]);
    }

    #[test]
    fn exact_palette_colors_map_to_themselves() {
        for index in 16..=255_u8 {
            let mapped = nearest_256(xterm_256(index));
            assert_eq!(xterm_256(mapped), xterm_256(index), "index {index}");
        }
    }

    #[test]
    fn nearest_256_never_selects_configurable_base_colors() {
        for color in [Rgb::new(0xcd, 0, 0), Rgb::new(0x7f, 0x7f, 0x7f), Rgb::BLACK] {
            assert!(nearest_256(color) >= 16);
        }
    }

    #[test]
    fn nearby_colors_pick_the_visually_closest_entry() {
        assert_eq!(nearest_256(Rgb::new(0xfe, 0x01, 0x02)), 196);
        assert_eq!(nearest_16(Rgb::new(0xf0, 0x10, 0x10)), 9);
        assert_eq!(nearest_16(Rgb::new(0x10, 0x10, 0x10)), 0);
    }

    #[test]
    fn nearest_in_handles_empty_palettes_and_ties() {
        assert_eq!(nearest_in(Rgb::WHITE, &[]), None);
        assert_eq!(nearest_in(Rgb::WHITE, &[Rgb::WHITE, Rgb::WHITE]), Some(0));
    }
}
