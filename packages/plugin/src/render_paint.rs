//! Paint declarative render operations into a `bmux_tui` frame.
//!
//! One lowering serves every host that draws plugin output: the attach client paints
//! extension and surface operations through it, and an embedding host replays a
//! published [`crate::surface::PluginSurface`] through it. Painting uses terminal
//! semantics: a text run replaces the cells it covers with exactly its style, and
//! colors it leaves unset are the terminal's defaults.

use crate::{
    BorderGlyphs, ExtensionRect, RenderCell, RenderColor, RenderDamage, RenderNamedColor, RenderOp,
    RenderStyle, RenderTextSpan, clip_render_text_run_to_rect, render_text_width_u16,
};
use bmux_presentation_scene::scene::CellBlock;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx, SizedCell};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{Line, Span};

/// Preserve all declarative terminal style attributes without terminal encoding.
#[must_use]
pub fn tui_style(value: RenderStyle) -> Style {
    let mut result = Style::new();
    if let Some(color) = value.fg {
        result = result.fg(tui_color(color));
    }
    if let Some(color) = value.bg {
        result = result.bg(tui_color(color));
    }
    for (enabled, flag) in [
        (value.bold, Modifier::BOLD),
        (value.dim, Modifier::DIM),
        (value.italic, Modifier::ITALIC),
        (value.underline, Modifier::UNDERLINE),
        (value.reverse, Modifier::REVERSED),
        (value.blink, Modifier::SLOW_BLINK),
        (value.strikethrough, Modifier::CROSSED_OUT),
        (value.hidden, Modifier::HIDDEN),
    ] {
        if enabled {
            result = result.add_modifier(flag);
        }
    }
    result
}

/// The `bmux_tui` color for a declarative render color.
#[must_use]
pub const fn tui_color(value: RenderColor) -> Color {
    match value {
        RenderColor::Default => Color::Default,
        RenderColor::Indexed(index) => Color::Indexed(index),
        RenderColor::Rgb { r, g, b } => Color::Rgb(r, g, b),
        RenderColor::Named(named) => match named {
            RenderNamedColor::Black => Color::Black,
            RenderNamedColor::Red => Color::Red,
            RenderNamedColor::Green => Color::Green,
            RenderNamedColor::Yellow => Color::Yellow,
            RenderNamedColor::Blue => Color::Blue,
            RenderNamedColor::Magenta => Color::Magenta,
            RenderNamedColor::Cyan => Color::Cyan,
            RenderNamedColor::White => Color::White,
            RenderNamedColor::BrightBlack => Color::BrightBlack,
            RenderNamedColor::BrightRed => Color::BrightRed,
            RenderNamedColor::BrightGreen => Color::BrightGreen,
            RenderNamedColor::BrightYellow => Color::BrightYellow,
            RenderNamedColor::BrightBlue => Color::BrightBlue,
            RenderNamedColor::BrightMagenta => Color::BrightMagenta,
            RenderNamedColor::BrightCyan => Color::BrightCyan,
            RenderNamedColor::BrightWhite => Color::BrightWhite,
        },
    }
}

/// Paint `text` from column `x` of row `y`, replacing each cell it covers with exactly
/// `style`: colors it leaves unset are the terminal's defaults, as the reset before every
/// run of terminal output makes them.
pub fn paint_terminal_text(cx: &mut PaintCx<'_, '_>, x: u16, y: u16, text: &str, style: Style) {
    let explicit = |color: Option<Color>| Some(color.unwrap_or(Color::Default));
    let style = Style {
        fg: explicit(style.fg),
        bg: explicit(style.bg),
        ..style
    };
    if text.is_ascii() {
        // Each ASCII byte is one narrow cell: no grapheme segmentation needed.
        cx.replace_cells(
            i32::from(x),
            i64::from(y),
            (0..text.len()).map(|index| SizedCell {
                symbol: &text[index..=index],
                width: 1,
                style,
            }),
        );
        return;
    }
    if text.chars().all(standalone_char) {
        // Every character is a grapheme of its own (box drawing, most symbols).
        cx.replace_cells(
            i32::from(x),
            i64::from(y),
            text.char_indices().map(|(start, ch)| SizedCell {
                symbol: &text[start..start + ch.len_utf8()],
                width: u8::try_from(bmux_cell_geometry::scalar_width(ch)).unwrap_or(1),
                style,
            }),
        );
        return;
    }
    let width = render_text_width_u16(text);
    cx.write_line(
        LocalRect::new(i32::from(x), i64::from(y), width, 1),
        &Line::from_spans(vec![Span::styled(text, style)]),
    );
}

/// Fill `rect` with blanks in `style`.
pub fn paint_blanks(cx: &mut PaintCx<'_, '_>, rect: Rect, style: Style) {
    cx.fill(LocalRect::terminal(rect), " ", style);
}

/// Whether `ch` is always a grapheme of its own, one or two columns wide: not zero-width (a
/// joiner, a combining mark, a variation selector) and not one of the characters that combine
/// with a neighbour into one grapheme (regional indicators, emoji skin-tone modifiers).
fn standalone_char(ch: char) -> bool {
    !matches!(ch, '\u{1f1e6}'..='\u{1f1ff}' | '\u{1f3fb}'..='\u{1f3ff}')
        && matches!(bmux_cell_geometry::scalar_width(ch), 1 | 2)
}

/// The cells an operation can touch, before clipping.
#[must_use]
pub fn render_op_bounds(op: &RenderOp) -> ExtensionRect {
    match op {
        RenderOp::TextRun { x, y, text, .. } => ExtensionRect {
            x: *x,
            y: *y,
            w: render_text_width_u16(text),
            h: 1,
        },
        RenderOp::StyledText { x, y, spans } => ExtensionRect {
            x: *x,
            y: *y,
            w: spans.iter().fold(0_u16, |width, span| {
                width.saturating_add(render_text_width_u16(&span.text))
            }),
            h: 1,
        },
        RenderOp::ClearRect { rect, .. }
        | RenderOp::FillRect { rect, .. }
        | RenderOp::Border { rect, .. } => *rect,
        RenderOp::EraseRowSegment { x, y, width, .. } => ExtensionRect {
            x: *x,
            y: *y,
            w: *width,
            h: 1,
        },
        RenderOp::CellGrid { x, y, rows } => ExtensionRect {
            x: *x,
            y: *y,
            w: rows
                .iter()
                .map(Vec::len)
                .max()
                .and_then(|width| u16::try_from(width).ok())
                .unwrap_or(u16::MAX),
            h: u16::try_from(rows.len()).unwrap_or(u16::MAX),
        },
        RenderOp::Cells(block) => block.rect(),
    }
}

/// Whether `op` touches any of `damage`.
#[must_use]
pub fn render_op_intersects_damage(op: &RenderOp, damage: &RenderDamage) -> bool {
    match damage {
        RenderDamage::None => false,
        RenderDamage::FullSurface => true,
        RenderDamage::Regions(regions) => regions
            .iter()
            .copied()
            .any(|region| render_op_bounds(op).intersects(region)),
    }
}

/// The part of `rect` inside `bounds`, if any.
#[must_use]
pub fn clip_extension_rect(rect: ExtensionRect, bounds: ExtensionRect) -> Option<ExtensionRect> {
    let x1 = rect.x.max(bounds.x);
    let y1 = rect.y.max(bounds.y);
    let x2 = rect.right().min(bounds.right());
    let y2 = rect.bottom().min(bounds.bottom());
    if x1 >= x2 || y1 >= y2 {
        None
    } else {
        Some(ExtensionRect {
            x: x1,
            y: y1,
            w: x2.saturating_sub(x1),
            h: y2.saturating_sub(y1),
        })
    }
}

/// How painted cells combine with the cells beneath them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellComposition {
    /// Terminal semantics: a painted cell takes exactly its style, and colors the style leaves
    /// unset are the terminal's defaults. The attach client paints this way.
    Replace,
    /// Overlay semantics: what a style leaves unset keeps what is beneath, as a component
    /// painting into its host's frame does. Embedding hosts replay surfaces this way.
    Overlay,
}

/// Paint `ops` with terminal semantics; see [`paint_render_ops_composed`].
pub fn paint_render_ops(
    cx: &mut PaintCx<'_, '_>,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
) -> bool {
    paint_render_ops_composed(cx, surface_rect, damage, ops, CellComposition::Replace)
}

/// Paint `ops` into `cx`, combining cells as `composition` says.
///
/// Operations are clipped to `surface_rect` and to `damage`, in `cx`'s coordinates.
/// Adjacent text runs of one style on one row are painted as one run, so a grapheme
/// split across them still forms one cluster. Returns whether anything was painted.
pub fn paint_render_ops_composed(
    cx: &mut PaintCx<'_, '_>,
    surface_rect: ExtensionRect,
    damage: &RenderDamage,
    ops: &[RenderOp],
    composition: CellComposition,
) -> bool {
    let target = &mut Target { cx, composition };
    let mut wrote = false;
    let mut pending_text_run = None;
    for op in ops {
        if !render_op_intersects_damage(op, damage) {
            wrote |= flush_pending_text_run(target, surface_rect, &mut pending_text_run);
            continue;
        }
        if let RenderOp::TextRun { x, y, text, style } = op {
            if !merge_pending_text_run(&mut pending_text_run, *x, *y, text, *style) {
                wrote |= flush_pending_text_run(target, surface_rect, &mut pending_text_run);
                pending_text_run = Some(PendingTextRun {
                    x: *x,
                    y: *y,
                    text: text.clone(),
                    style: *style,
                });
            }
            continue;
        }
        wrote |= flush_pending_text_run(target, surface_rect, &mut pending_text_run);
        wrote |= match op {
            RenderOp::TextRun { .. } => false,
            RenderOp::StyledText { x, y, spans } => {
                paint_styled_text(target, surface_rect, *x, *y, spans)
            }
            RenderOp::ClearRect { rect, style } => {
                paint_fill_rect(target, surface_rect, *rect, ' ', *style)
            }
            RenderOp::EraseRowSegment { x, y, width, style } => paint_fill_rect(
                target,
                surface_rect,
                ExtensionRect {
                    x: *x,
                    y: *y,
                    w: *width,
                    h: 1,
                },
                ' ',
                *style,
            ),
            RenderOp::FillRect { rect, ch, style } => {
                paint_fill_rect(target, surface_rect, *rect, *ch, *style)
            }
            RenderOp::Border {
                rect,
                glyphs,
                style,
            } => paint_border(target, surface_rect, *rect, *glyphs, *style),
            RenderOp::CellGrid { x, y, rows } => {
                paint_cell_grid(target, surface_rect, *x, *y, rows)
            }
            RenderOp::Cells(block) => paint_cell_block(target, surface_rect, block),
        };
    }
    wrote |= flush_pending_text_run(target, surface_rect, &mut pending_text_run);
    wrote
}

struct Target<'target, 'frame, 'buffer> {
    cx: &'target mut PaintCx<'frame, 'buffer>,
    composition: CellComposition,
}

impl Target<'_, '_, '_> {
    /// Paint text that begins at `(x, y)`.
    fn text(&mut self, x: u16, y: u16, text: &str, style: RenderStyle) {
        let style = tui_style(style);
        match self.composition {
            CellComposition::Replace => paint_terminal_text(self.cx, x, y, text, style),
            CellComposition::Overlay => {
                let width = render_text_width_u16(text);
                self.cx.write_line(
                    LocalRect::new(i32::from(x), i64::from(y), width, 1),
                    &Line::from_spans(vec![Span::styled(text, style)]),
                );
            }
        }
    }

    /// Paint one grapheme at `(x, y)`.
    fn symbol(&mut self, x: u16, y: u16, symbol: &str, style: RenderStyle) {
        match self.composition {
            CellComposition::Replace => self.text(x, y, symbol, style),
            CellComposition::Overlay => {
                self.cx
                    .set_cell(i32::from(x), i64::from(y), symbol, tui_style(style));
            }
        }
    }

    /// Paint `width` blanks from `(x, y)`.
    fn blanks(&mut self, x: u16, y: u16, width: u16, style: RenderStyle) {
        paint_blanks(self.cx, Rect::new(x, y, width, 1), tui_style(style));
    }
}

#[derive(Clone, Debug)]
struct PendingTextRun {
    x: u16,
    y: u16,
    text: String,
    style: RenderStyle,
}

fn merge_pending_text_run(
    pending: &mut Option<PendingTextRun>,
    x: u16,
    y: u16,
    text: &str,
    style: RenderStyle,
) -> bool {
    let Some(pending) = pending.as_mut() else {
        return false;
    };
    if pending.y != y || pending.style != style {
        return false;
    }
    let pending_width = render_text_width_u16(pending.text.as_str());
    if pending.x.saturating_add(pending_width) != x {
        return false;
    }
    pending.text.push_str(text);
    true
}

fn flush_pending_text_run(
    target: &mut Target<'_, '_, '_>,
    surface_rect: ExtensionRect,
    pending: &mut Option<PendingTextRun>,
) -> bool {
    let Some(pending) = pending.take() else {
        return false;
    };
    paint_text_run(
        target,
        surface_rect,
        pending.x,
        pending.y,
        &pending.text,
        pending.style,
    )
}

fn paint_text_run(
    target: &mut Target<'_, '_, '_>,
    surface_rect: ExtensionRect,
    x: u16,
    y: u16,
    text: &str,
    style: RenderStyle,
) -> bool {
    if y < surface_rect.y || y >= surface_rect.bottom() || x >= surface_rect.right() {
        return false;
    }
    let Some((clipped_x, clipped)) = clip_render_text_run_to_rect(x, text, surface_rect) else {
        return false;
    };
    target.text(clipped_x, y, &clipped, style);
    true
}

fn paint_styled_text(
    target: &mut Target<'_, '_, '_>,
    surface_rect: ExtensionRect,
    x: u16,
    y: u16,
    spans: &[RenderTextSpan],
) -> bool {
    let mut wrote = false;
    let mut column = x;
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        wrote |= paint_text_run(target, surface_rect, column, y, &span.text, span.style);
        column = column.saturating_add(render_text_width_u16(&span.text));
    }
    wrote
}

fn paint_fill_rect(
    target: &mut Target<'_, '_, '_>,
    surface_rect: ExtensionRect,
    rect: ExtensionRect,
    ch: char,
    style: RenderStyle,
) -> bool {
    let Some(rect) = clip_extension_rect(rect, surface_rect) else {
        return false;
    };
    if rect.is_empty() {
        return false;
    }
    if ch == ' ' {
        for y in rect.y..rect.bottom() {
            target.blanks(rect.x, y, rect.w, style);
        }
        return true;
    }
    let row = ch.to_string().repeat(usize::from(rect.w));
    for y in rect.y..rect.bottom() {
        target.text(rect.x, y, &row, style);
    }
    true
}

fn paint_border(
    target: &mut Target<'_, '_, '_>,
    surface_rect: ExtensionRect,
    rect: ExtensionRect,
    glyphs: BorderGlyphs,
    style: RenderStyle,
) -> bool {
    let Some(rect) = clip_extension_rect(rect, surface_rect) else {
        return false;
    };
    if rect.w == 0 || rect.h == 0 {
        return false;
    }
    let vertical = glyphs.vertical.to_string();
    if rect.h == 1 {
        let row = glyphs.horizontal.to_string().repeat(usize::from(rect.w));
        target.text(rect.x, rect.y, &row, style);
        return true;
    }
    if rect.w == 1 {
        for y in rect.y..rect.bottom() {
            target.text(rect.x, y, &vertical, style);
        }
        return true;
    }
    let inner_width = usize::from(rect.w.saturating_sub(2));
    let horizontal = glyphs.horizontal.to_string().repeat(inner_width);
    let top = format!("{}{horizontal}{}", glyphs.top_left, glyphs.top_right);
    let bottom = format!("{}{horizontal}{}", glyphs.bottom_left, glyphs.bottom_right);
    target.text(rect.x, rect.y, &top, style);
    for y in rect.y.saturating_add(1)..rect.bottom().saturating_sub(1) {
        target.text(rect.x, y, &vertical, style);
        target.text(rect.right().saturating_sub(1), y, &vertical, style);
    }
    target.text(rect.x, rect.bottom().saturating_sub(1), &bottom, style);
    true
}

fn paint_cell_grid(
    target: &mut Target<'_, '_, '_>,
    surface_rect: ExtensionRect,
    x: u16,
    y: u16,
    rows: &[Vec<RenderCell>],
) -> bool {
    let mut wrote = false;
    for (row_offset, row) in rows.iter().enumerate() {
        let Ok(row_offset) = u16::try_from(row_offset) else {
            break;
        };
        let cell_y = y.saturating_add(row_offset);
        if cell_y < surface_rect.y || cell_y >= surface_rect.bottom() {
            continue;
        }
        for (col_offset, cell) in row.iter().enumerate() {
            let Ok(col_offset) = u16::try_from(col_offset) else {
                break;
            };
            let cell_x = x.saturating_add(col_offset);
            if cell_x < surface_rect.x || cell_x >= surface_rect.right() {
                continue;
            }
            let Some(ch) = cell.ch else {
                continue;
            };
            target.symbol(cell_x, cell_y, ch.encode_utf8(&mut [0; 4]), cell.style);
            wrote = true;
        }
    }
    wrote
}

/// Paint a block's visible cells; transparent cells keep what is beneath.
/// A wide symbol is painted only when all of its cells are inside the surface.
fn paint_cell_block(
    target: &mut Target<'_, '_, '_>,
    surface_rect: ExtensionRect,
    block: &CellBlock,
) -> bool {
    let mut wrote = false;
    for row in 0..block.height {
        let cell_y = block.y.saturating_add(row);
        if cell_y < surface_rect.y || cell_y >= surface_rect.bottom() {
            continue;
        }
        for column in 0..block.width {
            let cell_x = block.x.saturating_add(column);
            let Some(cell) = block.get(column, row) else {
                continue;
            };
            let Some(symbol) = cell.symbol.as_ref() else {
                continue;
            };
            let width = symbol.width().max(1);
            if cell_x < surface_rect.x || cell_x.saturating_add(width) > surface_rect.right() {
                continue;
            }
            let mut buffer = [0; 4];
            target.symbol(cell_x, cell_y, symbol.as_str(&mut buffer), cell.style);
            wrote = true;
        }
    }
    wrote
}
