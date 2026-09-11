//! Shared lowering from component cells to retained plugin render operations.
use crate::{RenderColor, RenderNamedColor, RenderOp, RenderStyle};
use bmux_tui::{
    buffer::Buffer,
    geometry::Point,
    style::{Color, Modifier, Style},
};

/// Convert a TUI buffer into coalesced retained render operations.
///
/// Runs never cross rows or style boundaries. Empty continuation cells are
/// preserved so the terminal cursor advances exactly as the TUI buffer does.
#[must_use]
pub fn buffer_render_ops(buffer: &Buffer) -> Vec<RenderOp> {
    let area = buffer.area();
    let mut ops = Vec::new();
    for y in area.y..area.bottom() {
        let mut x = area.x;
        while x < area.right() {
            let Some(first) = buffer.get(Point::new(x, y)) else {
                break;
            };
            let style = first.style;
            let start = x;
            let mut text = String::new();
            while x < area.right() {
                let Some(cell) = buffer.get(Point::new(x, y)) else {
                    break;
                };
                if cell.style != style {
                    break;
                }
                text.push_str(&cell.symbol);
                x = x.saturating_add(1);
            }
            ops.push(RenderOp::text_run(start, y, text, render_style(style)));
        }
    }
    ops
}

#[must_use]
pub const fn render_style(style: Style) -> RenderStyle {
    RenderStyle {
        fg: convert_optional_color(style.fg),
        bg: convert_optional_color(style.bg),
        bold: style.modifiers.contains(Modifier::BOLD),
        underline: style.modifiers.contains(Modifier::UNDERLINE),
        italic: style.modifiers.contains(Modifier::ITALIC),
        reverse: style.modifiers.contains(Modifier::REVERSED),
        dim: style.modifiers.contains(Modifier::DIM),
        blink: style.modifiers.contains(Modifier::SLOW_BLINK),
        strikethrough: style.modifiers.contains(Modifier::CROSSED_OUT),
    }
}

const fn convert_optional_color(color: Option<Color>) -> Option<RenderColor> {
    match color {
        Some(color) => Some(render_color(color)),
        None => None,
    }
}

const fn render_color(color: Color) -> RenderColor {
    match color {
        Color::Default => RenderColor::Default,
        Color::Black => RenderColor::Named(RenderNamedColor::Black),
        Color::Red => RenderColor::Named(RenderNamedColor::Red),
        Color::Green => RenderColor::Named(RenderNamedColor::Green),
        Color::Yellow => RenderColor::Named(RenderNamedColor::Yellow),
        Color::Blue => RenderColor::Named(RenderNamedColor::Blue),
        Color::Magenta => RenderColor::Named(RenderNamedColor::Magenta),
        Color::Cyan => RenderColor::Named(RenderNamedColor::Cyan),
        Color::White => RenderColor::Named(RenderNamedColor::White),
        Color::BrightBlack => RenderColor::Named(RenderNamedColor::BrightBlack),
        Color::BrightRed => RenderColor::Named(RenderNamedColor::BrightRed),
        Color::BrightGreen => RenderColor::Named(RenderNamedColor::BrightGreen),
        Color::BrightYellow => RenderColor::Named(RenderNamedColor::BrightYellow),
        Color::BrightBlue => RenderColor::Named(RenderNamedColor::BrightBlue),
        Color::BrightMagenta => RenderColor::Named(RenderNamedColor::BrightMagenta),
        Color::BrightCyan => RenderColor::Named(RenderNamedColor::BrightCyan),
        Color::BrightWhite => RenderColor::Named(RenderNamedColor::BrightWhite),
        Color::Indexed(index) => RenderColor::Indexed(index),
        Color::Rgb(r, g, b) => RenderColor::Rgb { r, g, b },
    }
}
