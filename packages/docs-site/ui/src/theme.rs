//! Dark theme remapping for hyperchad markdown containers.
//!
//! `hyperchad_markdown` hardcodes GitHub light-theme colors. This module provides
//! a post-processor that remaps those to dark terminal-aesthetic equivalents.

use hyperchad_color::Color;
use hyperchad_template::Container;

/// Light-theme colors hardcoded in hyperchad_markdown and their dark replacements.
const COLOR_REMAPS: &[([u8; 3], [u8; 3])] = &[
    // #f6f8fa (code block / inline code / thead background) -> dark surface
    ([0xf6, 0xf8, 0xfa], [0x16, 0x1b, 0x22]),
    // #d0d7de (borders, HR background) -> subtle dark border
    ([0xd0, 0xd7, 0xde], [0x30, 0x36, 0x3d]),
    // #656d76 (blockquote text) -> lighter muted for dark bg
    ([0x65, 0x6d, 0x76], [0x8b, 0x94, 0x9e]),
    // #0969da (link blue) -> brighter blue for dark bg
    ([0x09, 0x69, 0xda], [0x58, 0xa6, 0xff]),
];

fn matches_color(color: &Color, rgb: [u8; 3]) -> bool {
    color.r == rgb[0] && color.g == rgb[1] && color.b == rgb[2]
}

fn remap_color(color: &Color) -> Option<Color> {
    for (from, to) in COLOR_REMAPS {
        if matches_color(color, *from) {
            return Some(Color {
                r: to[0],
                g: to[1],
                b: to[2],
                a: color.a,
            });
        }
    }
    None
}

/// Recursively remap hardcoded light-theme colors in a markdown container tree
/// to dark-theme equivalents.
pub fn apply_dark_theme(container: &mut Container) {
    // Remap background
    if let Some(ref bg) = container.background
        && let Some(replacement) = remap_color(bg)
    {
        container.background = Some(replacement);
    }

    // Remap text color
    if let Some(ref c) = container.color
        && let Some(replacement) = remap_color(c)
    {
        container.color = Some(replacement);
    }

    // Recurse into children
    for child in &mut container.children {
        apply_dark_theme(child);
    }
}
