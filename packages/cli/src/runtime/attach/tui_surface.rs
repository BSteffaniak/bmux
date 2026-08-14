use bmux_appearance::RuntimeAppearance;
use bmux_plugin::{RenderColor, RenderNamedColor, RenderOp, RenderStyle};
use bmux_tui::buffer::Buffer;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::theme::{ComponentSurfaces, ComponentTheme};

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
pub fn component_theme(appearance: &RuntimeAppearance) -> ComponentTheme {
    let foreground = parse_tui_color(&appearance.foreground).unwrap_or(Color::BrightWhite);
    let background = parse_tui_color(&appearance.background).unwrap_or(Color::Black);
    let selection = parse_tui_color(&appearance.selection_background).unwrap_or(Color::Cyan);
    let cursor = parse_tui_color(&appearance.cursor).unwrap_or(Color::BrightCyan);
    ComponentTheme {
        canvas: Style::new().fg(foreground).bg(background),
        surfaces: ComponentSurfaces {
            normal: Style::new().bg(background),
            raised: Style::new().bg(background),
            overlay: Style::new().bg(background),
            scrim: None,
        },
        text: Style::new().fg(foreground),
        focused: Style::new().fg(cursor),
        selected: Style::new().fg(background).bg(selection),
        disabled: Style::new()
            .fg(Color::BrightBlack)
            .add_modifier(Modifier::DIM),
        muted: Style::new().fg(Color::BrightBlack),
        info: Style::new().fg(cursor),
        success: Style::new().fg(Color::Green),
        warning: Style::new().fg(Color::Yellow),
        error: Style::new().fg(Color::Red),
        border: Style::new().fg(cursor),
    }
}

#[must_use]
pub fn parse_tui_color(value: &str) -> Option<Color> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
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

/// Create a buffer whose coordinates match an absolute terminal rectangle.
#[must_use]
pub fn surface_buffer(rect: Rect) -> Buffer {
    Buffer::empty(rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_rows_by_style() {
        let area = Rect::new(4, 2, 4, 2);
        let mut buffer = Buffer::empty(area);
        let accent = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        buffer.set_cell(Point::new(4, 2), "a", accent);
        buffer.set_cell(Point::new(5, 2), "b", accent);
        buffer.set_cell(Point::new(6, 2), "c", Style::new());

        let ops = buffer_render_ops(&buffer);

        assert_eq!(ops.len(), 3);
        assert!(matches!(
            &ops[0],
            RenderOp::TextRun { x: 4, y: 2, text, style }
                if text == "ab" && style.bold
        ));
        assert!(matches!(
            &ops[1],
            RenderOp::TextRun { x: 6, y: 2, text, .. } if text == "c "
        ));
    }

    #[test]
    fn converts_all_style_fields() {
        let style = Style::new()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Indexed(7))
            .add_modifier(
                Modifier::BOLD
                    | Modifier::DIM
                    | Modifier::ITALIC
                    | Modifier::UNDERLINE
                    | Modifier::SLOW_BLINK
                    | Modifier::REVERSED
                    | Modifier::CROSSED_OUT,
            );

        let converted = render_style(style);

        assert_eq!(converted.fg, Some(RenderColor::Rgb { r: 1, g: 2, b: 3 }));
        assert_eq!(converted.bg, Some(RenderColor::Indexed(7)));
        assert!(converted.bold);
        assert!(converted.dim);
        assert!(converted.italic);
        assert!(converted.underline);
        assert!(converted.blink);
        assert!(converted.reverse);
        assert!(converted.strikethrough);
    }
}
