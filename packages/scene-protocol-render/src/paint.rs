//! Paint-command executor: turn a
//! [`bmux_scene_protocol::scene_protocol::SurfaceDecoration`] (or its
//! individual [`PaintCommand`]s) into bytes on a
//! [`std::io::Write`] target.
//!
//! The executor orders commands by `z` (stable within a tier), emits
//! each command's text with the appropriate ANSI SGR prelude, and
//! closes with a single reset so attributes don't leak into
//! subsequent surfaces.
//!
//! This module is the runtime companion to the scene-protocol wire
//! schema; it knows nothing about any particular producer (decoration
//! plugin, overlay plugin, future scripted UIs). Renderer hosts call
//! [`apply_paint_commands`] once per surface; render-extension
//! implementors call it per surface they want to draw.

use anyhow::{Context, Result};
use bmux_scene_protocol::scene_protocol::{
    BorderGlyphs, Cell, GradientAxis, PaintCommand, Rect, Style, SurfaceDecoration,
};
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Print;
use std::io;
use unicode_width::UnicodeWidthStr;

use crate::glyphs::border_glyphs_corners_or_custom;
use crate::sgr::scene_style_sgr_prelude;

/// Truncate or pad `content` to exactly `width` columns. Used by
/// renderer call sites that paint fixed-width status bars or badges.
#[must_use]
pub fn opaque_row_text(content: &str, width: usize) -> String {
    let mut rendered = content.to_string();
    if rendered.len() > width {
        rendered.truncate(width);
    }
    if rendered.len() < width {
        rendered.push_str(&" ".repeat(width - rendered.len()));
    }
    rendered
}

/// Apply every paint command in a [`SurfaceDecoration`] to `stdout`.
///
/// Commands are sorted by `z` (stable within a tier) before emission,
/// so callers can layer decorations without caring about insertion
/// order. A single reset (`\x1b[0m`) is emitted after the last
/// command when any bytes were written, keeping attributes contained
/// to the surface.
///
/// # Errors
///
/// Returns any error from queueing cursor movement or text output.
pub fn apply_paint_commands<W: io::Write>(
    stdout: &mut W,
    surface: &SurfaceDecoration,
) -> Result<()> {
    let mut ordered: Vec<(usize, &PaintCommand)> =
        surface.paint_commands.iter().enumerate().collect();
    ordered.sort_by_key(|(i, cmd)| (paint_command_z(cmd), *i));

    let mut emitted_any = false;
    for (_, command) in ordered {
        emitted_any |= apply_paint_command(stdout, command)?;
    }
    if emitted_any {
        queue!(stdout, Print("\x1b[0m")).context("failed resetting paint-command surface style")?;
    }
    Ok(())
}

/// Extract the `z` ordering for a paint command. Every current
/// variant carries an explicit `z` field.
const fn paint_command_z(command: &PaintCommand) -> i16 {
    match command {
        PaintCommand::Text { z, .. }
        | PaintCommand::FilledRect { z, .. }
        | PaintCommand::GradientRun { z, .. }
        | PaintCommand::CellGrid { z, .. }
        | PaintCommand::BoxBorder { z, .. } => *z,
    }
}

/// Apply a single paint command variant. Returns `true` when any
/// bytes were queued (so the caller knows whether to emit a trailing
/// reset).
///
/// # Errors
///
/// Returns any error from queueing cursor movement or text output.
pub fn apply_paint_command<W: io::Write>(stdout: &mut W, command: &PaintCommand) -> Result<bool> {
    match command {
        PaintCommand::Text {
            col,
            row,
            text,
            style,
            ..
        } => {
            queue_styled_text(stdout, *col, *row, text, style)?;
            Ok(!text.is_empty())
        }
        PaintCommand::FilledRect {
            rect, glyph, style, ..
        } => {
            if rect.w == 0 || rect.h == 0 || glyph.is_empty() {
                return Ok(false);
            }
            let row_text = glyph.repeat(usize::from(rect.w));
            for dy in 0..rect.h {
                queue_styled_text(stdout, rect.x, rect.y.saturating_add(dy), &row_text, style)?;
            }
            Ok(true)
        }
        PaintCommand::GradientRun {
            col,
            row,
            text,
            axis,
            from_style,
            to_style,
            ..
        } => {
            queue_gradient_run(stdout, *col, *row, text, *axis, from_style, to_style)?;
            Ok(!text.is_empty())
        }
        PaintCommand::CellGrid {
            origin_col,
            origin_row,
            cols,
            cells,
            ..
        } => {
            queue_cell_grid(stdout, *origin_col, *origin_row, *cols, cells)?;
            Ok(!cells.is_empty())
        }
        PaintCommand::BoxBorder {
            rect,
            glyphs,
            style,
            ..
        } => {
            queue_box_border(stdout, rect, glyphs, style)?;
            Ok(rect.w >= 2 && rect.h >= 2)
        }
    }
}

/// Write `text` at `(col, row)` prefixed by `style`'s SGR prelude.
/// Emits no style when `style` is all-default so diff-sensitive
/// callers can skip no-op writes.
fn queue_styled_text<W: io::Write>(
    stdout: &mut W,
    col: u16,
    row: u16,
    text: &str,
    style: &Style,
) -> Result<()> {
    queue!(stdout, MoveTo(col, row)).context("failed positioning paint command")?;
    let prelude = scene_style_sgr_prelude(style);
    if !prelude.is_empty() {
        queue!(stdout, Print(&prelude)).context("failed emitting paint command style")?;
    }
    queue!(stdout, Print(text)).context("failed emitting paint command text")?;
    Ok(())
}

/// Emit a gradient-interpolated styled run. For each grapheme in
/// `text` the effective style is the linear interpolation between
/// `from_style` and `to_style`.
fn queue_gradient_run<W: io::Write>(
    stdout: &mut W,
    col: u16,
    row: u16,
    text: &str,
    axis: GradientAxis,
    from_style: &Style,
    to_style: &Style,
) -> Result<()> {
    let graphemes: Vec<&str> = grapheme_iter(text).collect();
    let n = graphemes.len();
    if n == 0 {
        return Ok(());
    }
    if n == 1 {
        queue_styled_text(stdout, col, row, text, from_style)?;
        return Ok(());
    }
    let mut offset: u16 = 0;
    #[allow(clippy::cast_precision_loss)] // n bounded by terminal width.
    let denom = (n - 1) as f32;
    for (i, grapheme) in graphemes.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f32 / denom;
        let style = interpolate_style(from_style, to_style, t);
        match axis {
            GradientAxis::Horizontal => {
                queue_styled_text(stdout, col.saturating_add(offset), row, grapheme, &style)?;
                offset =
                    offset.saturating_add(u16::try_from(grapheme_width(grapheme)).unwrap_or(1));
            }
            GradientAxis::Vertical => {
                queue_styled_text(stdout, col, row.saturating_add(offset), grapheme, &style)?;
                offset = offset.saturating_add(1);
            }
            GradientAxis::Diagonal => {
                queue_styled_text(
                    stdout,
                    col.saturating_add(offset),
                    row.saturating_add(offset),
                    grapheme,
                    &style,
                )?;
                offset = offset.saturating_add(1);
            }
        }
    }
    Ok(())
}

/// Paint an explicit per-cell grid.
fn queue_cell_grid<W: io::Write>(
    stdout: &mut W,
    origin_col: u16,
    origin_row: u16,
    cols: u16,
    cells: &[Cell],
) -> Result<()> {
    if cols == 0 || cells.is_empty() {
        return Ok(());
    }
    for (i, cell) in cells.iter().enumerate() {
        if cell.glyph.is_empty() {
            continue;
        }
        let grid_col = u16::try_from(i % usize::from(cols)).unwrap_or(0);
        let grid_row = u16::try_from(i / usize::from(cols)).unwrap_or(0);
        queue_styled_text(
            stdout,
            origin_col.saturating_add(grid_col),
            origin_row.saturating_add(grid_row),
            &cell.glyph,
            &cell.style,
        )?;
    }
    Ok(())
}

/// Paint a 1-cell border around `rect` using the given glyph set.
fn queue_box_border<W: io::Write>(
    stdout: &mut W,
    rect: &Rect,
    glyphs: &BorderGlyphs,
    style: &Style,
) -> Result<()> {
    if rect.w < 2 || rect.h < 2 {
        return Ok(());
    }
    let Some(corners) = border_glyphs_corners_or_custom(glyphs) else {
        return Ok(());
    };
    let top = assemble_border_row(
        rect.w,
        corners.top_left,
        corners.horizontal,
        corners.top_right,
    );
    let bottom = assemble_border_row(
        rect.w,
        corners.bottom_left,
        corners.horizontal,
        corners.bottom_right,
    );
    queue_styled_text(stdout, rect.x, rect.y, &top, style)?;
    queue_styled_text(
        stdout,
        rect.x,
        rect.y.saturating_add(rect.h.saturating_sub(1)),
        &bottom,
        style,
    )?;
    for dy in 1..rect.h.saturating_sub(1) {
        let y = rect.y.saturating_add(dy);
        queue_styled_text(stdout, rect.x, y, corners.vertical, style)?;
        queue_styled_text(
            stdout,
            rect.x.saturating_add(rect.w.saturating_sub(1)),
            y,
            corners.vertical,
            style,
        )?;
    }
    Ok(())
}

/// Interpolate between two scene styles. Boolean flags are taken from
/// whichever style is "closer" at `t` (`< 0.5` → `from`, `>= 0.5` → `to`).
/// Colour interpolation works for paired truecolor endpoints; every
/// other pair falls back to the nearest endpoint.
fn interpolate_style(from: &Style, to: &Style, t: f32) -> Style {
    let pick = |a, b| if t < 0.5 { a } else { b };
    Style {
        fg: interpolate_color(from.fg.as_ref(), to.fg.as_ref(), t),
        bg: interpolate_color(from.bg.as_ref(), to.bg.as_ref(), t),
        bold: pick(from.bold, to.bold),
        underline: pick(from.underline, to.underline),
        italic: pick(from.italic, to.italic),
        reverse: pick(from.reverse, to.reverse),
        dim: pick(from.dim, to.dim),
        blink: pick(from.blink, to.blink),
        strikethrough: pick(from.strikethrough, to.strikethrough),
    }
}

fn interpolate_color(
    from: Option<&bmux_scene_protocol::scene_protocol::Color>,
    to: Option<&bmux_scene_protocol::scene_protocol::Color>,
    t: f32,
) -> Option<bmux_scene_protocol::scene_protocol::Color> {
    use bmux_scene_protocol::scene_protocol::Color;
    match (from, to) {
        (
            Some(Color::Rgb {
                r: r0,
                g: g0,
                b: b0,
            }),
            Some(Color::Rgb {
                r: r1,
                g: g1,
                b: b1,
            }),
        ) => Some(Color::Rgb {
            r: lerp_u8(*r0, *r1, t),
            g: lerp_u8(*g0, *g1, t),
            b: lerp_u8(*b0, *b1, t),
        }),
        (a, b) => {
            if t < 0.5 {
                a.cloned()
            } else {
                b.cloned()
            }
        }
    }
}

#[allow(
    clippy::suboptimal_flops,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // Colour channel lerp; fma and cast warnings are noise for u8 output.
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let a = f32::from(a);
    let b = f32::from(b);
    let v = a + (b - a) * t.clamp(0.0, 1.0);
    v.round().clamp(0.0, 255.0) as u8
}

/// Grapheme iterator that walks `text` one Unicode scalar at a time.
/// Adequate for border glyphs and ASCII art; complex graphemes (emoji
/// clusters) will paint one scalar per position.
fn grapheme_iter(text: &str) -> impl Iterator<Item = &str> {
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || {
        let (start, _) = chars.next()?;
        let end = chars.peek().map_or(text.len(), |(i, _)| *i);
        Some(&text[start..end])
    })
}

fn grapheme_width(g: &str) -> usize {
    UnicodeWidthStr::width(g).max(1)
}

/// Compose a horizontal border row: `left` corner, `mid` repeated
/// `width - 2` times, `right` corner. Returns an empty string when
/// `width == 0`; a single `left` corner when `width == 1`.
#[allow(clippy::suspicious_operation_groupings)] // Byte-length arithmetic; clippy misfires.
fn assemble_border_row(width: u16, left: &str, mid: &str, right: &str) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return left.to_string();
    }
    let body_len = mid.len() * width.saturating_sub(2);
    let mut line = String::with_capacity(left.len() + body_len + right.len());
    line.push_str(left);
    if width > 2 {
        for _ in 0..(width - 2) {
            line.push_str(mid);
        }
    }
    line.push_str(right);
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_scene_protocol::scene_protocol::{BorderGlyphs, Color, NamedColor, Rect};
    use uuid::Uuid;

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
    fn opaque_row_text_truncates_and_pads() {
        assert_eq!(opaque_row_text("hello", 3), "hel");
        assert_eq!(opaque_row_text("hi", 5), "hi   ");
        assert_eq!(opaque_row_text("", 4), "    ");
    }

    #[test]
    fn apply_paint_command_text_emits_bytes() {
        let mut out = Vec::new();
        let cmd = PaintCommand::Text {
            col: 0,
            row: 0,
            z: 0,
            text: "hello".to_string(),
            style: default_style(),
        };
        assert!(apply_paint_command(&mut out, &cmd).expect("paints"));
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("hello"));
    }

    #[test]
    fn apply_paint_commands_sorts_by_z_and_closes_with_reset() {
        let mut surface = SurfaceDecoration {
            surface_id: Uuid::from_u128(1),
            rect: Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 1,
            },
            content_rect: Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 1,
            },
            paint_commands: vec![
                PaintCommand::Text {
                    col: 0,
                    row: 0,
                    z: 5,
                    text: "HIGH".to_string(),
                    style: default_style(),
                },
                PaintCommand::Text {
                    col: 0,
                    row: 0,
                    z: 0,
                    text: "LOW".to_string(),
                    style: default_style(),
                },
            ],
            interactive_regions: Vec::new(),
        };
        // Clone so we can also verify a second call produces identical output.
        let surface2 = surface.clone();
        let mut out = Vec::new();
        apply_paint_commands(&mut out, &surface).expect("paints");
        let rendered = String::from_utf8(out).expect("utf8");
        // LOW (z=0) should appear before HIGH (z=5) in the output stream.
        let low = rendered.find("LOW").expect("LOW present");
        let high = rendered.find("HIGH").expect("HIGH present");
        assert!(low < high, "z-ordering not respected: {rendered:?}");
        assert!(
            rendered.ends_with("\x1b[0m"),
            "surface must end with reset sequence: {rendered:?}"
        );
        // Make sure the no-op branch (empty paint commands) doesn't emit anything.
        surface.paint_commands.clear();
        let _ = surface2; // silence unused warning
        let mut empty_out = Vec::new();
        apply_paint_commands(&mut empty_out, &surface).expect("empty paints");
        assert!(empty_out.is_empty());
    }

    #[test]
    fn apply_paint_command_box_border_with_rounded_glyphs() {
        let mut out = Vec::new();
        let cmd = PaintCommand::BoxBorder {
            rect: Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 3,
            },
            z: 0,
            glyphs: BorderGlyphs::Rounded,
            style: Style {
                fg: Some(Color::Named {
                    name: NamedColor::BrightWhite,
                }),
                ..default_style()
            },
        };
        assert!(apply_paint_command(&mut out, &cmd).expect("paints"));
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains('\u{256d}'),
            "top-left rounded glyph missing: {rendered:?}"
        );
    }

    #[test]
    fn apply_paint_command_vertical_gradient_advances_rows() {
        let mut out = Vec::new();
        let cmd = PaintCommand::GradientRun {
            col: 2,
            row: 3,
            z: 0,
            text: "abc".to_string(),
            axis: GradientAxis::Vertical,
            from_style: default_style(),
            to_style: default_style(),
        };

        assert!(apply_paint_command(&mut out, &cmd).expect("paints"));
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("\x1b[4;3Ha\x1b[5;3Hb\x1b[6;3Hc"),
            "vertical gradient should keep the column and advance rows: {rendered:?}"
        );
    }
}
