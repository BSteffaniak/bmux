use bmux_appearance::RuntimeAppearance;
#[cfg(test)]
use bmux_plugin::{RenderColor, RenderOp};
use bmux_tui::buffer::Buffer;
#[cfg(test)]
use bmux_tui::geometry::Point;
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::theme::{ComponentSurfaces, ComponentTheme};

pub use bmux_plugin::component_render::buffer_render_ops;
#[cfg(test)]
use bmux_plugin::component_render::render_style;

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
        // Cursor and foreground colors may be identical (including in the
        // default appearance). Focus must not depend on color contrast alone.
        focused: Style::new()
            .fg(cursor)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINE),
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
    fn preserves_wide_and_combining_text_cells() {
        let area = Rect::new(0, 0, 5, 1);
        let mut buffer = Buffer::empty(area);
        let style = Style::new().fg(Color::Cyan);
        buffer.set_cell(Point::new(0, 0), "界", style);
        buffer.set_cell(Point::new(2, 0), "e\u{301}", style);

        let ops = buffer_render_ops(&buffer);

        assert!(matches!(
            &ops[0],
            RenderOp::TextRun { x: 0, y: 0, text, .. } if text.starts_with("界e\u{301}")
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
