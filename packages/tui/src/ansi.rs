//! ANSI terminal backend helpers.

use std::io::{self, Write};

use crate::buffer::Buffer;
use crate::frame::Cursor;
use crate::geometry::Point;
use crate::style::{Color, Modifier, Style};

/// Write a full buffer frame using ANSI escape sequences.
///
/// This renderer is intentionally simple: it paints every row in the buffer and
/// leaves damage-aware flushing for a later backend layer.
///
/// # Errors
///
/// Returns any I/O error reported by `writer`.
pub fn write_ansi_frame(
    writer: &mut impl Write,
    buffer: &Buffer,
    cursor: Option<Cursor>,
) -> io::Result<()> {
    let area = buffer.area();
    let mut active_style = Style::new();
    write!(writer, "\x1b[?25l")?;
    for y in area.y..area.bottom() {
        write_ansi_move_to(writer, Point::new(area.x, y))?;
        for x in area.x..area.right() {
            let Some(cell) = buffer.get(Point::new(x, y)) else {
                continue;
            };
            if cell.style != active_style {
                write_ansi_style(writer, cell.style)?;
                active_style = cell.style;
            }
            if cell.symbol.is_empty() {
                continue;
            }
            writer.write_all(cell.symbol.as_bytes())?;
        }
    }
    write_ansi_style(writer, Style::new())?;
    if let Some(cursor) = cursor {
        write_ansi_move_to(writer, cursor.position)?;
        if cursor.visible {
            write!(writer, "\x1b[?25h")?;
        } else {
            write!(writer, "\x1b[?25l")?;
        }
    }
    Ok(())
}

fn write_ansi_move_to(writer: &mut impl Write, point: Point) -> io::Result<()> {
    write!(
        writer,
        "\x1b[{};{}H",
        u32::from(point.y).saturating_add(1),
        u32::from(point.x).saturating_add(1)
    )
}

fn write_ansi_style(writer: &mut impl Write, style: Style) -> io::Result<()> {
    writer.write_all(b"\x1b[0")?;
    write_ansi_modifier(writer, style.modifiers)?;
    if let Some(fg) = style.fg {
        write_ansi_color(writer, fg, ColorRole::Foreground)?;
    }
    if let Some(bg) = style.bg {
        write_ansi_color(writer, bg, ColorRole::Background)?;
    }
    writer.write_all(b"m")
}

fn write_ansi_modifier(writer: &mut impl Write, modifiers: Modifier) -> io::Result<()> {
    if modifiers.contains(Modifier::BOLD) {
        writer.write_all(b";1")?;
    }
    if modifiers.contains(Modifier::DIM) {
        writer.write_all(b";2")?;
    }
    if modifiers.contains(Modifier::ITALIC) {
        writer.write_all(b";3")?;
    }
    if modifiers.contains(Modifier::UNDERLINE) {
        writer.write_all(b";4")?;
    }
    if modifiers.contains(Modifier::SLOW_BLINK) {
        writer.write_all(b";5")?;
    }
    if modifiers.contains(Modifier::REVERSED) {
        writer.write_all(b";7")?;
    }
    if modifiers.contains(Modifier::HIDDEN) {
        writer.write_all(b";8")?;
    }
    if modifiers.contains(Modifier::CROSSED_OUT) {
        writer.write_all(b";9")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorRole {
    Foreground,
    Background,
}

fn write_ansi_color(writer: &mut impl Write, color: Color, role: ColorRole) -> io::Result<()> {
    match color {
        Color::Default => Ok(()),
        Color::Black => write!(writer, ";{}", color_base(role)),
        Color::Red => write!(writer, ";{}", color_base(role) + 1),
        Color::Green => write!(writer, ";{}", color_base(role) + 2),
        Color::Yellow => write!(writer, ";{}", color_base(role) + 3),
        Color::Blue => write!(writer, ";{}", color_base(role) + 4),
        Color::Magenta => write!(writer, ";{}", color_base(role) + 5),
        Color::Cyan => write!(writer, ";{}", color_base(role) + 6),
        Color::White => write!(writer, ";{}", color_base(role) + 7),
        Color::BrightBlack => write!(writer, ";{}", bright_color_base(role)),
        Color::BrightRed => write!(writer, ";{}", bright_color_base(role) + 1),
        Color::BrightGreen => write!(writer, ";{}", bright_color_base(role) + 2),
        Color::BrightYellow => write!(writer, ";{}", bright_color_base(role) + 3),
        Color::BrightBlue => write!(writer, ";{}", bright_color_base(role) + 4),
        Color::BrightMagenta => write!(writer, ";{}", bright_color_base(role) + 5),
        Color::BrightCyan => write!(writer, ";{}", bright_color_base(role) + 6),
        Color::BrightWhite => write!(writer, ";{}", bright_color_base(role) + 7),
        Color::Indexed(index) => match role {
            ColorRole::Foreground => write!(writer, ";38;5;{index}"),
            ColorRole::Background => write!(writer, ";48;5;{index}"),
        },
        Color::Rgb(red, green, blue) => match role {
            ColorRole::Foreground => write!(writer, ";38;2;{red};{green};{blue}"),
            ColorRole::Background => write!(writer, ";48;2;{red};{green};{blue}"),
        },
    }
}

const fn color_base(role: ColorRole) -> u8 {
    match role {
        ColorRole::Foreground => 30,
        ColorRole::Background => 40,
    }
}

const fn bright_color_base(role: ColorRole) -> u8 {
    match role {
        ColorRole::Foreground => 90,
        ColorRole::Background => 100,
    }
}

#[cfg(test)]
mod tests {
    use super::write_ansi_frame;
    use crate::buffer::Buffer;
    use crate::frame::Cursor;
    use crate::geometry::{Point, Rect};
    use crate::style::{Color, Modifier, Style};

    #[test]
    fn ansi_frame_writes_rows_and_cursor() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer.set_cell(Point::new(0, 0), "A", Style::new());
        buffer.set_cell(Point::new(1, 0), "B", Style::new());
        let mut out = Vec::new();

        write_ansi_frame(&mut out, &buffer, Some(Cursor::visible(Point::new(1, 0)))).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[?25l\x1b[1;1HAB\x1b[0m\x1b[1;2H\x1b[?25h"
        );
    }

    #[test]
    fn ansi_frame_writes_style_changes() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer.set_cell(
            Point::new(0, 0),
            "A",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        );
        buffer.set_cell(Point::new(1, 0), "B", Style::new().bg(Color::Rgb(1, 2, 3)));
        let mut out = Vec::new();

        write_ansi_frame(&mut out, &buffer, None).unwrap();
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("\x1b[0;1;31mA"));
        assert!(output.contains("\x1b[0;48;2;1;2;3mB"));
        assert!(output.ends_with("\x1b[0m"));
    }
}
