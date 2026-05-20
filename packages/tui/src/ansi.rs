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

/// Statistics from a damage-aware ANSI frame write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnsiFrameDiffStats {
    /// Number of changed cells written.
    pub changed_cells: usize,
    /// Whether the renderer fell back to a full frame repaint.
    pub full_repaint: bool,
}

/// Write only changed cells between two buffers using ANSI escape sequences.
///
/// If buffer areas differ, this falls back to [`write_ansi_frame`] and reports
/// `full_repaint = true`.
///
/// # Errors
///
/// Returns any I/O error reported by `writer`.
pub fn write_ansi_frame_diff(
    writer: &mut impl Write,
    previous: &Buffer,
    current: &Buffer,
    cursor: Option<Cursor>,
) -> io::Result<AnsiFrameDiffStats> {
    if previous.area() != current.area() {
        write_ansi_frame(writer, current, cursor)?;
        return Ok(AnsiFrameDiffStats {
            changed_cells: current.cells().len(),
            full_repaint: true,
        });
    }

    let area = current.area();
    let mut active_style = Style::new();
    let mut changed_cells = 0usize;
    write!(writer, "\x1b[?25l")?;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let point = Point::new(x, y);
            let Some(previous_cell) = previous.get(point) else {
                continue;
            };
            let Some(current_cell) = current.get(point) else {
                continue;
            };
            if previous_cell == current_cell {
                continue;
            }
            write_ansi_move_to(writer, point)?;
            if current_cell.style != active_style {
                write_ansi_style(writer, current_cell.style)?;
                active_style = current_cell.style;
            }
            if current_cell.symbol.is_empty() {
                writer.write_all(b" ")?;
            } else {
                writer.write_all(current_cell.symbol.as_bytes())?;
            }
            changed_cells = changed_cells.saturating_add(1);
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
    Ok(AnsiFrameDiffStats {
        changed_cells,
        full_repaint: false,
    })
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
    use super::{write_ansi_frame, write_ansi_frame_diff};
    use crate::buffer::Buffer;
    use crate::frame::Cursor;
    use crate::geometry::{Point, Rect};
    use crate::style::{Color, Modifier, Style};

    #[test]
    fn ansi_frame_diff_writes_only_changed_cells() {
        let mut previous = Buffer::empty(Rect::new(0, 0, 3, 1));
        previous.set_cell(Point::new(0, 0), "A", Style::new());
        previous.set_cell(Point::new(1, 0), "B", Style::new());
        previous.set_cell(Point::new(2, 0), "C", Style::new());
        let mut current = previous.clone();
        current.set_cell(Point::new(1, 0), "X", Style::new().fg(Color::Red));
        let mut out = Vec::new();

        let stats = write_ansi_frame_diff(&mut out, &previous, &current, None).unwrap();
        let output = String::from_utf8(out).unwrap();

        assert_eq!(stats.changed_cells, 1);
        assert!(!stats.full_repaint);
        assert!(output.contains("\x1b[1;2H\x1b[0;31mX"));
        assert!(!output.contains('A'));
        assert!(!output.contains('C'));
    }

    #[test]
    fn ansi_frame_diff_falls_back_when_areas_differ() {
        let previous = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut current = Buffer::empty(Rect::new(0, 0, 2, 1));
        current.set_cell(Point::new(0, 0), "A", Style::new());
        current.set_cell(Point::new(1, 0), "B", Style::new());
        let mut out = Vec::new();

        let stats = write_ansi_frame_diff(&mut out, &previous, &current, None).unwrap();

        assert_eq!(stats.changed_cells, 2);
        assert!(stats.full_repaint);
        assert!(String::from_utf8(out).unwrap().contains("AB"));
    }

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
