//! Optional pixel-precise semantic-border planning.
//!
//! This module intentionally does not emit terminal escape sequences. It maps
//! semantic border paint commands into generic terminal graphics primitives; the
//! attach renderer owns protocol selection, placement, caching, and cleanup.

use std::hash::{Hash, Hasher};

use bmux_plugin::{
    ExtensionRect, RenderLayerItem, TerminalGraphicFill, TerminalGraphicOverlay,
    TerminalRenderCapabilities, TerminalRgba,
};
use bmux_scene_protocol::scene_protocol::{
    Color as SceneColor, NamedColor, PaintCommand, Rect as SceneRect, Style as SceneStyle,
};
use bmux_scene_protocol_render::capabilities::{SceneRenderCapabilities, capability_query_matches};
use uuid::Uuid;

const SEMANTIC_BORDER_GRAPHIC_Z: i16 = -1;
const SEGMENT_KEY_STRIDE: u64 = 1_024;
const TOP_SEGMENT_KEY_BASE: u64 = 0;
const BOTTOM_SEGMENT_KEY_BASE: u64 = SEGMENT_KEY_STRIDE;
const LEFT_SEGMENT_KEY_BASE: u64 = SEGMENT_KEY_STRIDE * 2;
const RIGHT_SEGMENT_KEY_BASE: u64 = SEGMENT_KEY_STRIDE * 3;
const SEGMENT_COORD_SHIFT: u32 = 16;

#[derive(Clone, Copy, Debug)]
struct RasterSegment {
    key_slot: u64,
    col: u16,
    row: u16,
    cell_width: u16,
    cell_height: u16,
    pixel_width: u32,
    pixel_height: u32,
    fill: TerminalGraphicFill,
}

#[must_use]
pub fn semantic_border_terminal_graphic_z(_scene_z: i16) -> i16 {
    // Kitty z=-1 keeps semantic borders below terminal-cell text while still
    // above the terminal background. This avoids both header overwrites and
    // header-row placement churn/deletes.
    SEMANTIC_BORDER_GRAPHIC_Z
}

#[must_use]
pub fn semantic_border_graphic_items(
    surface_id: Uuid,
    semantic_key: u64,
    command: &PaintCommand,
    capabilities: TerminalRenderCapabilities,
    scene_capabilities: SceneRenderCapabilities,
) -> Option<Vec<RenderLayerItem>> {
    semantic_border_graphic_items_with_occlusion(
        surface_id,
        semantic_key,
        command,
        capabilities,
        scene_capabilities,
        &[],
    )
}

#[must_use]
pub fn semantic_border_graphic_items_with_occlusion(
    surface_id: Uuid,
    semantic_key: u64,
    command: &PaintCommand,
    capabilities: TerminalRenderCapabilities,
    scene_capabilities: SceneRenderCapabilities,
    occluders: &[ExtensionRect],
) -> Option<Vec<RenderLayerItem>> {
    let PaintCommand::SemanticBorder {
        rect,
        style,
        thickness_px,
        radius_px,
        z,
        when,
        ..
    } = command
    else {
        return None;
    };
    if !is_graphics_capable_semantic_border(rect, *thickness_px, capabilities)
        || !capability_query_matches(when.as_ref(), scene_capabilities)
    {
        return None;
    }

    let color = raster_color_from_style(style);
    let segments = raster_border_segments(rect, *thickness_px, *radius_px, capabilities, occluders);
    if segments.is_empty() {
        return Some(Vec::new());
    }
    Some(
        segments
            .into_iter()
            .map(|segment| {
                RenderLayerItem::Graphic(TerminalGraphicOverlay {
                    key: raster_image_key(surface_id, semantic_key, segment.key_slot),
                    cell_rect: bmux_plugin::ExtensionRect::new(
                        segment.col,
                        segment.row,
                        segment.cell_width,
                        segment.cell_height,
                    ),
                    pixel_width: segment.pixel_width,
                    pixel_height: segment.pixel_height,
                    color,
                    fill: segment.fill,
                    z_index: semantic_border_terminal_graphic_z(*z),
                })
            })
            .collect(),
    )
}

fn is_graphics_capable_semantic_border(
    rect: &SceneRect,
    thickness_px: u16,
    capabilities: TerminalRenderCapabilities,
) -> bool {
    // Kitty is the only currently enabled semantic-border graphics backend.
    // Sixel and iTerm2 inline images are not treated as transparent overlay
    // protocols because their placement/alpha semantics are not safe for pane
    // chrome.
    rect.w >= 2
        && rect.h >= 2
        && thickness_px > 0
        && capabilities.kitty_graphics
        && capabilities.graphics_alpha
        && capabilities.has_cell_pixels()
}

fn raster_border_segments(
    rect: &SceneRect,
    thickness_px: u16,
    _radius_px: u16,
    capabilities: TerminalRenderCapabilities,
    occluders: &[ExtensionRect],
) -> Vec<RasterSegment> {
    let cell_w = capabilities.cell_pixel_width;
    let cell_h = capabilities.cell_pixel_height;
    if rect.w < 2 || rect.h < 2 || cell_w == 0 || cell_h == 0 {
        return Vec::new();
    }
    let thickness = thickness_px.clamp(1, cell_w.min(cell_h));
    let pixel_width = u32::from(cell_w);
    let pixel_height = u32::from(cell_h);
    let base_segments = [
        RasterSegment {
            key_slot: TOP_SEGMENT_KEY_BASE,
            col: rect.x,
            row: rect.y,
            cell_width: rect.w,
            cell_height: 1,
            pixel_width,
            pixel_height,
            fill: TerminalGraphicFill::Top {
                thickness_px: thickness,
            },
        },
        RasterSegment {
            key_slot: BOTTOM_SEGMENT_KEY_BASE,
            col: rect.x,
            row: rect.y.saturating_add(rect.h.saturating_sub(1)),
            cell_width: rect.w,
            cell_height: 1,
            pixel_width,
            pixel_height,
            fill: TerminalGraphicFill::Bottom {
                thickness_px: thickness,
            },
        },
        RasterSegment {
            key_slot: LEFT_SEGMENT_KEY_BASE,
            col: rect.x,
            row: rect.y,
            cell_width: 1,
            cell_height: rect.h,
            pixel_width,
            pixel_height,
            fill: TerminalGraphicFill::Left {
                thickness_px: thickness,
            },
        },
        RasterSegment {
            key_slot: RIGHT_SEGMENT_KEY_BASE,
            col: rect.x.saturating_add(rect.w.saturating_sub(1)),
            row: rect.y,
            cell_width: 1,
            cell_height: rect.h,
            pixel_width,
            pixel_height,
            fill: TerminalGraphicFill::Right {
                thickness_px: thickness,
            },
        },
    ];
    base_segments
        .into_iter()
        .flat_map(|segment| subtract_occluders(segment, occluders))
        .collect()
}

fn subtract_occluders(segment: RasterSegment, occluders: &[ExtensionRect]) -> Vec<RasterSegment> {
    let horizontal = segment.cell_height == 1;
    let fixed = if horizontal { segment.row } else { segment.col };
    let start = if horizontal { segment.col } else { segment.row };
    let len = if horizontal {
        segment.cell_width
    } else {
        segment.cell_height
    };
    let end = start.saturating_add(len);
    let mut visible = vec![(start, end)];
    for occluder in occluders {
        let crosses_fixed = if horizontal {
            fixed >= occluder.y && fixed < occluder.bottom()
        } else {
            fixed >= occluder.x && fixed < occluder.right()
        };
        if !crosses_fixed {
            continue;
        }
        let cut_start = if horizontal { occluder.x } else { occluder.y };
        let cut_end = if horizontal {
            occluder.right()
        } else {
            occluder.bottom()
        };
        visible = visible
            .into_iter()
            .flat_map(|(visible_start, visible_end)| {
                if cut_end <= visible_start || cut_start >= visible_end {
                    return vec![(visible_start, visible_end)];
                }
                let mut pieces = Vec::with_capacity(2);
                if cut_start > visible_start {
                    pieces.push((visible_start, cut_start.min(visible_end)));
                }
                if cut_end < visible_end {
                    pieces.push((cut_end.max(visible_start), visible_end));
                }
                pieces
            })
            .collect();
    }
    visible
        .into_iter()
        .filter(|(visible_start, visible_end)| visible_start < visible_end)
        .map(|(visible_start, visible_end)| {
            let mut visible_segment = segment;
            if horizontal {
                visible_segment.col = visible_start;
                visible_segment.cell_width = visible_end.saturating_sub(visible_start);
            } else {
                visible_segment.row = visible_start;
                visible_segment.cell_height = visible_end.saturating_sub(visible_start);
            }
            visible_segment.key_slot = segment.key_slot
                | (u64::from(visible_start) << SEGMENT_COORD_SHIFT)
                | u64::from(visible_end);
            visible_segment
        })
        .collect()
}

fn raster_color_from_style(style: &SceneStyle) -> TerminalRgba {
    let (r, g, b, a) = match style.fg.as_ref().or(style.bg.as_ref()) {
        Some(SceneColor::Rgb { r, g, b }) => (*r, *g, *b, 255),
        Some(SceneColor::Named { name }) => named_color_rgb(*name),
        Some(SceneColor::Indexed { index }) => indexed_color_rgb(*index),
        _ => (255, 255, 255, 255),
    };
    TerminalRgba { r, g, b, a }
}

const fn named_color_rgb(color: NamedColor) -> (u8, u8, u8, u8) {
    match color {
        NamedColor::Black => (0, 0, 0, 255),
        NamedColor::Red => (170, 0, 0, 255),
        NamedColor::Green => (0, 170, 0, 255),
        NamedColor::Yellow => (170, 85, 0, 255),
        NamedColor::Blue => (0, 0, 170, 255),
        NamedColor::Magenta => (170, 0, 170, 255),
        NamedColor::Cyan => (0, 170, 170, 255),
        NamedColor::White => (170, 170, 170, 255),
        NamedColor::BrightBlack => (85, 85, 85, 255),
        NamedColor::BrightRed => (255, 85, 85, 255),
        NamedColor::BrightGreen => (85, 255, 85, 255),
        NamedColor::BrightYellow => (255, 255, 85, 255),
        NamedColor::BrightBlue => (85, 85, 255, 255),
        NamedColor::BrightMagenta => (255, 85, 255, 255),
        NamedColor::BrightCyan => (85, 255, 255, 255),
        NamedColor::BrightWhite => (255, 255, 255, 255),
    }
}

fn indexed_color_rgb(index: u8) -> (u8, u8, u8, u8) {
    if index < 16 {
        return named_color_rgb(match index {
            0 => NamedColor::Black,
            1 => NamedColor::Red,
            2 => NamedColor::Green,
            3 => NamedColor::Yellow,
            4 => NamedColor::Blue,
            5 => NamedColor::Magenta,
            6 => NamedColor::Cyan,
            7 => NamedColor::White,
            8 => NamedColor::BrightBlack,
            9 => NamedColor::BrightRed,
            10 => NamedColor::BrightGreen,
            11 => NamedColor::BrightYellow,
            12 => NamedColor::BrightBlue,
            13 => NamedColor::BrightMagenta,
            14 => NamedColor::BrightCyan,
            _ => NamedColor::BrightWhite,
        });
    }
    if index >= 232 {
        let value = 8_u8.saturating_add((index - 232).saturating_mul(10));
        return (value, value, value, 255);
    }
    let cube = index - 16;
    let r = cube / 36;
    let g = (cube / 6) % 6;
    let b = cube % 6;
    (
        color_cube_channel(r),
        color_cube_channel(g),
        color_cube_channel(b),
        255,
    )
}

const fn color_cube_channel(value: u8) -> u8 {
    if value == 0 { 0 } else { 55 + value * 40 }
}

fn raster_image_key(surface_id: Uuid, semantic_key: u64, segment_index: u64) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    surface_id.hash(&mut hash);
    semantic_key.hash(&mut hash);
    segment_index.hash(&mut hash);
    hash.finish().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_border_graphics_require_kitty_alpha_and_cell_pixels() {
        let command = PaintCommand::SemanticBorder {
            rect: SceneRect {
                x: 1,
                y: 2,
                w: 4,
                h: 3,
            },
            z: 7,
            style: SceneStyle {
                fg: None,
                bg: None,
                bold: false,
                underline: false,
                italic: false,
                reverse: false,
                dim: false,
                blink: false,
                strikethrough: false,
            },
            fallback_glyphs: bmux_scene_protocol::scene_protocol::BorderGlyphs::Rounded,
            thickness_px: 3,
            radius_px: 0,
            when: None,
        };
        assert!(
            semantic_border_graphic_items(
                Uuid::from_u128(1),
                0,
                &command,
                TerminalRenderCapabilities {
                    kitty_graphics: true,
                    graphics_alpha: true,
                    cell_pixel_width: 8,
                    cell_pixel_height: 16,
                    ..TerminalRenderCapabilities::default()
                },
                SceneRenderCapabilities::default(),
            )
            .is_some()
        );
        assert!(
            semantic_border_graphic_items(
                Uuid::from_u128(1),
                0,
                &command,
                TerminalRenderCapabilities {
                    sixel: true,
                    cell_pixel_width: 8,
                    cell_pixel_height: 16,
                    ..TerminalRenderCapabilities::default()
                },
                SceneRenderCapabilities::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn semantic_border_graphics_use_stable_under_text_z() {
        assert_eq!(semantic_border_terminal_graphic_z(0), -1);
        assert_eq!(semantic_border_terminal_graphic_z(500), -1);
        assert_eq!(semantic_border_terminal_graphic_z(-500), -1);
    }

    #[test]
    fn semantic_border_graphics_use_cell_sized_sources_and_large_placements() {
        let command = PaintCommand::SemanticBorder {
            rect: SceneRect {
                x: 10,
                y: 20,
                w: 30,
                h: 12,
            },
            z: 7,
            style: SceneStyle {
                fg: None,
                bg: None,
                bold: false,
                underline: false,
                italic: false,
                reverse: false,
                dim: false,
                blink: false,
                strikethrough: false,
            },
            fallback_glyphs: bmux_scene_protocol::scene_protocol::BorderGlyphs::Rounded,
            thickness_px: 3,
            radius_px: 0,
            when: None,
        };
        let items = semantic_border_graphic_items(
            Uuid::from_u128(1),
            0,
            &command,
            TerminalRenderCapabilities {
                kitty_graphics: true,
                graphics_alpha: true,
                cell_pixel_width: 8,
                cell_pixel_height: 16,
                ..TerminalRenderCapabilities::default()
            },
            SceneRenderCapabilities::default(),
        )
        .expect("semantic border should lower to graphics");

        assert_eq!(items.len(), 4);
        let RenderLayerItem::Graphic(top) = &items[0] else {
            panic!("expected graphic item");
        };
        assert_eq!(top.cell_rect.w, 30);
        assert_eq!(top.cell_rect.h, 1);
        assert_eq!(top.pixel_width, 8);
        assert_eq!(top.pixel_height, 16);

        let RenderLayerItem::Graphic(left) = &items[2] else {
            panic!("expected graphic item");
        };
        assert_eq!(left.cell_rect.w, 1);
        assert_eq!(left.cell_rect.h, 12);
        assert_eq!(left.pixel_width, 8);
        assert_eq!(left.pixel_height, 16);
        assert_eq!(left.z_index, -1);
    }

    #[test]
    fn semantic_border_graphics_split_around_global_occluders() {
        let command = PaintCommand::SemanticBorder {
            rect: SceneRect {
                x: 10,
                y: 20,
                w: 10,
                h: 6,
            },
            z: 7,
            style: SceneStyle {
                fg: None,
                bg: None,
                bold: false,
                underline: false,
                italic: false,
                reverse: false,
                dim: false,
                blink: false,
                strikethrough: false,
            },
            fallback_glyphs: bmux_scene_protocol::scene_protocol::BorderGlyphs::Rounded,
            thickness_px: 3,
            radius_px: 0,
            when: None,
        };
        let capabilities = TerminalRenderCapabilities {
            kitty_graphics: true,
            graphics_alpha: true,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            ..TerminalRenderCapabilities::default()
        };
        let top_split = semantic_border_graphic_items_with_occlusion(
            Uuid::from_u128(1),
            0,
            &command,
            capabilities,
            SceneRenderCapabilities::default(),
            &[ExtensionRect::new(12, 20, 4, 1)],
        )
        .expect("semantic border should lower to graphics");
        let left_split = semantic_border_graphic_items_with_occlusion(
            Uuid::from_u128(1),
            0,
            &command,
            capabilities,
            SceneRenderCapabilities::default(),
            &[ExtensionRect::new(10, 22, 1, 2)],
        )
        .expect("semantic border should lower to graphics");

        let graphic_signature = |items: Vec<RenderLayerItem>| {
            items
                .into_iter()
                .map(|item| match item {
                    RenderLayerItem::Graphic(graphic) => {
                        assert_eq!(graphic.z_index, -1);
                        (graphic.key, graphic.cell_rect)
                    }
                    RenderLayerItem::Op(_) => panic!("semantic border should emit graphics only"),
                })
                .collect::<Vec<_>>()
        };

        let top_signature = graphic_signature(top_split);
        let left_signature = graphic_signature(left_split);
        assert!(
            top_signature
                .iter()
                .any(|(_, rect)| *rect == ExtensionRect::new(10, 20, 2, 1))
        );
        assert!(
            top_signature
                .iter()
                .any(|(_, rect)| *rect == ExtensionRect::new(16, 20, 4, 1))
        );
        assert!(
            !top_signature
                .iter()
                .any(|(_, rect)| rect.intersects(ExtensionRect::new(12, 20, 4, 1)))
        );
        assert!(
            left_signature
                .iter()
                .any(|(_, rect)| *rect == ExtensionRect::new(10, 20, 1, 2))
        );
        assert!(
            left_signature
                .iter()
                .any(|(_, rect)| *rect == ExtensionRect::new(10, 24, 1, 2))
        );
        assert!(
            !left_signature
                .iter()
                .any(|(_, rect)| rect.intersects(ExtensionRect::new(10, 22, 1, 2)))
        );
    }
}
