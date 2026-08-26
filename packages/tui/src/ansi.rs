//! ANSI terminal backend helpers.

use std::io::{self, Write};

use crate::buffer::Buffer;
use crate::frame::Cursor;
use crate::geometry::Point;
use crate::style::{Color, Modifier, Style};
use crate::text::{Line, Span};
use crate::text_width::display_width;

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
        emit_row_span(writer, buffer, y, area.x, area.right(), &mut active_style)?;
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
        if let Some((start, end)) = changed_row_suffix(previous, current, y) {
            emit_row_span(writer, current, y, start, end, &mut active_style)?;
            changed_cells = changed_cells.saturating_add(usize::from(end.saturating_sub(start)));
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

fn changed_row_suffix(previous: &Buffer, current: &Buffer, y: u16) -> Option<(u16, u16)> {
    let area = current.area();
    let mut changed = (area.x..area.right()).filter(|x| {
        let point = Point::new(*x, y);
        previous.get(point) != current.get(point)
    });
    let first_changed = changed.next()?;
    let last_changed = changed.next_back().unwrap_or(first_changed);
    let mut start = first_changed;
    let mut end = last_changed.saturating_add(1).min(area.right());
    for buffer in [previous, current] {
        let first = Point::new(first_changed, y);
        if buffer
            .get(first)
            .is_some_and(crate::buffer::Cell::is_wide_continuation)
            && first_changed > area.x
        {
            start = first_changed - 1;
        }
        if first_changed > area.x
            && buffer
                .get(Point::new(first_changed - 1, y))
                .is_some_and(crate::buffer::Cell::is_wide_leader)
        {
            start = first_changed - 1;
        }
        let last = Point::new(last_changed, y);
        if buffer
            .get(last)
            .is_some_and(crate::buffer::Cell::is_wide_leader)
        {
            end = last_changed.saturating_add(2).min(area.right());
        }
    }
    Some((start, end))
}

fn emit_row_span(
    writer: &mut impl Write,
    buffer: &Buffer,
    y: u16,
    start: u16,
    end: u16,
    active_style: &mut Style,
) -> io::Result<()> {
    if start >= end {
        return Ok(());
    }
    write_ansi_move_to(writer, Point::new(start, y))?;
    let mut x = start;
    while x < end {
        let Some(cell) = buffer.get(Point::new(x, y)) else {
            break;
        };
        if cell.is_wide_continuation() {
            // A valid span never starts on a continuation. If malformed input reaches here, clear
            // the physical column rather than emitting an empty symbol without cursor progress.
            writer.write_all(b" ")?;
            x = x.saturating_add(1);
            continue;
        }
        if cell.style != *active_style {
            write_ansi_style(writer, cell.style)?;
            *active_style = cell.style;
        }
        if cell.symbol.is_empty() {
            writer.write_all(b" ")?;
            x = x.saturating_add(1);
            continue;
        }
        writer.write_all(cell.symbol.as_bytes())?;
        let width = u16::from(cell.width()).max(
            u16::try_from(display_width(&cell.symbol))
                .unwrap_or(u16::MAX)
                .min(2),
        );
        x = x.saturating_add(width.max(1));
    }
    Ok(())
}

/// Convert ANSI-styled terminal output into BMUX styled text lines.
///
/// Unsupported control sequences are stripped. SGR styling is preserved for the common ANSI
/// color/modifier forms, including 8-color, bright-color, 256-color, and RGB colors.
#[must_use]
pub fn ansi_to_lines(input: &str) -> Vec<Line> {
    let mut parser = AnsiTextParser::default();
    parser.push_str(input);
    parser.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AnsiTextParser {
    lines: Vec<Line>,
    current: Line,
    style: Style,
}

impl Default for AnsiTextParser {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            current: Line::new(),
            style: Style::new(),
        }
    }
}

impl AnsiTextParser {
    fn push_str(&mut self, input: &str) {
        let mut chars = input.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\x1b' => self.consume_escape(&mut chars),
                '\n' => self.push_newline(),
                '\r' => {}
                '\t' => self.push_text("    "),
                ch if ch.is_control() => {}
                ch => self.push_text(&ch.to_string()),
            }
        }
    }

    fn finish(mut self) -> Vec<Line> {
        self.lines.push(self.current);
        self.lines
    }

    fn push_newline(&mut self) {
        self.lines.push(std::mem::take(&mut self.current));
    }

    fn push_text(&mut self, text: &str) {
        if let Some(last) = self.current.spans.last_mut()
            && last.style == self.style
        {
            last.content.push_str(text);
            return;
        }
        self.current
            .push_span(Span::styled(text.to_owned(), self.style));
    }

    fn consume_escape(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        match chars.next() {
            Some('[') => self.consume_csi(chars),
            Some(']') => consume_until_bel_or_st(chars),
            Some(_) | None => {}
        }
    }

    fn consume_csi(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        let mut sequence = String::new();
        for ch in chars.by_ref() {
            if ('@'..='~').contains(&ch) {
                if ch == 'm' {
                    apply_sgr(&mut self.style, &sequence);
                }
                return;
            }
            sequence.push(ch);
        }
    }
}

fn consume_until_bel_or_st(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut previous_was_escape = false;
    for ch in chars.by_ref() {
        if ch == '\x07' || previous_was_escape && ch == '\\' {
            return;
        }
        previous_was_escape = ch == '\x1b';
    }
}

fn apply_sgr(style: &mut Style, sequence: &str) {
    let params = sgr_params(sequence);
    if params.is_empty() {
        *style = Style::new();
        return;
    }
    let mut index = 0usize;
    while index < params.len() {
        match params[index] {
            0 => *style = Style::new(),
            1 => style.modifiers |= Modifier::BOLD,
            2 => style.modifiers |= Modifier::DIM,
            3 => style.modifiers |= Modifier::ITALIC,
            4 => style.modifiers |= Modifier::UNDERLINE,
            5 => style.modifiers |= Modifier::SLOW_BLINK,
            7 => style.modifiers |= Modifier::REVERSED,
            8 => style.modifiers |= Modifier::HIDDEN,
            9 => style.modifiers |= Modifier::CROSSED_OUT,
            22 => style.modifiers = style.modifiers.difference(Modifier::BOLD | Modifier::DIM),
            23 => style.modifiers = style.modifiers.difference(Modifier::ITALIC),
            24 => style.modifiers = style.modifiers.difference(Modifier::UNDERLINE),
            25 => style.modifiers = style.modifiers.difference(Modifier::SLOW_BLINK),
            27 => style.modifiers = style.modifiers.difference(Modifier::REVERSED),
            28 => style.modifiers = style.modifiers.difference(Modifier::HIDDEN),
            29 => style.modifiers = style.modifiers.difference(Modifier::CROSSED_OUT),
            30..=37 => style.fg = Some(standard_color(params[index] - 30, false)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(standard_color(params[index] - 40, false)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(standard_color(params[index] - 90, true)),
            100..=107 => style.bg = Some(standard_color(params[index] - 100, true)),
            38 | 48 => {
                index = apply_extended_color(style, &params, index);
            }
            _ => {}
        }
        index = index.saturating_add(1);
    }
}

fn sgr_params(sequence: &str) -> Vec<u16> {
    if sequence.is_empty() {
        return Vec::new();
    }
    sequence
        .split([';', ':'])
        .map(|part| part.parse::<u16>().unwrap_or(0))
        .collect()
}

fn apply_extended_color(style: &mut Style, params: &[u16], index: usize) -> usize {
    let Some(mode) = params.get(index.saturating_add(1)).copied() else {
        return index;
    };
    let color_role = if params[index] == 38 {
        ColorRole::Foreground
    } else {
        ColorRole::Background
    };
    match mode {
        5 => {
            if let Some(color) = params.get(index.saturating_add(2)).copied() {
                set_style_color(
                    style,
                    color_role,
                    Color::Indexed(u8::try_from(color).unwrap_or(u8::MAX)),
                );
                return index.saturating_add(2);
            }
        }
        2 => {
            if let (Some(red), Some(green), Some(blue)) = (
                params.get(index.saturating_add(2)).copied(),
                params.get(index.saturating_add(3)).copied(),
                params.get(index.saturating_add(4)).copied(),
            ) {
                set_style_color(
                    style,
                    color_role,
                    Color::Rgb(
                        u8::try_from(red).unwrap_or(u8::MAX),
                        u8::try_from(green).unwrap_or(u8::MAX),
                        u8::try_from(blue).unwrap_or(u8::MAX),
                    ),
                );
                return index.saturating_add(4);
            }
        }
        _ => {}
    }
    index
}

const fn set_style_color(style: &mut Style, role: ColorRole, color: Color) {
    match role {
        ColorRole::Foreground => style.fg = Some(color),
        ColorRole::Background => style.bg = Some(color),
    }
}

const fn standard_color(offset: u16, bright: bool) -> Color {
    match (offset, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::White,
        (0, true) => Color::BrightBlack,
        (1, true) => Color::BrightRed,
        (2, true) => Color::BrightGreen,
        (3, true) => Color::BrightYellow,
        (4, true) => Color::BrightBlue,
        (5, true) => Color::BrightMagenta,
        (6, true) => Color::BrightCyan,
        (7, true) => Color::BrightWhite,
        (_, _) => Color::Default,
    }
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
    use super::{ansi_to_lines, write_ansi_frame, write_ansi_frame_diff};
    use crate::buffer::Buffer;
    use crate::frame::Cursor;
    use crate::geometry::{Point, Rect};
    use crate::style::{Color, Modifier, Style};
    use crate::text::Line;

    #[test]
    fn ansi_to_lines_preserves_sgr_styles() {
        let lines = ansi_to_lines("normal \x1b[31;1mred\x1b[0m\n\x1b[38;5;42midx\x1b[0m");

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].plain_text(), "normal red");
        assert_eq!(lines[1].plain_text(), "idx");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Red));
        assert!(lines[0].spans[1].style.modifiers.contains(Modifier::BOLD));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Indexed(42)));
    }

    #[test]
    fn ansi_to_lines_strips_unsupported_control_sequences() {
        let lines = ansi_to_lines("before\x1b]0;title\x07after\x1b[2K");

        assert_eq!(lines[0].plain_text(), "beforeafter");
    }

    fn wide_buffer(text: &str, width: u16) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
        buffer.write_line(Rect::new(0, 0, width, 1), &Line::raw(text));
        buffer
    }

    #[test]
    fn ansi_diff_emits_wide_graphemes_atomically_without_targeting_continuations() {
        let previous = wide_buffer(" A👩🏽‍💻B ", 7);
        let current = wide_buffer(" 👩🏽‍💻AB ", 7);
        let mut out = Vec::new();

        let stats = write_ansi_frame_diff(&mut out, &previous, &current, None).unwrap();
        let output = String::from_utf8(out).unwrap();

        assert!(!stats.full_repaint);
        assert!(output.contains("👩🏽‍💻"));
        assert!(!output.contains("\x1b[1;3H "));
    }

    #[test]
    fn ansi_diff_clears_complete_old_wide_span_before_border() {
        let previous = wide_buffer("A👩🏽‍💻│", 4);
        let current = wide_buffer("AB │", 4);
        let mut out = Vec::new();

        let stats = write_ansi_frame_diff(&mut out, &previous, &current, None).unwrap();
        let output = String::from_utf8(out).unwrap();

        assert_eq!(stats.changed_cells, 2);
        assert!(output.contains("B "), "{output:?}");
    }

    #[test]
    fn ansi_diff_repaints_row_suffix_after_first_changed_cell() {
        let previous = wide_buffer("prefix old stale", 20);
        let current = wide_buffer("prefix new", 20);
        let mut out = Vec::new();

        let stats = write_ansi_frame_diff(&mut out, &previous, &current, None).unwrap();
        let output = String::from_utf8(out).unwrap();

        assert!(!stats.full_repaint);
        assert_eq!(stats.changed_cells, 9);
        assert!(output.contains("\x1b[1;8Hnew"), "{output:?}");
        assert!(output.ends_with("      \x1b[0m"), "{output:?}");
    }

    #[test]
    fn ansi_diff_row_suffix_avoids_disjoint_cursor_drift_after_emoji() {
        let previous = wide_buffer("👩🏽‍💻 A stale tail", 18);
        let current = wide_buffer("👩🏽‍💻 B", 18);
        let mut out = Vec::new();

        write_ansi_frame_diff(&mut out, &previous, &current, None).unwrap();
        let output = String::from_utf8(out).unwrap();

        // One absolute move begins the changed suffix; no later cursor move can
        // accumulate a terminal/unicode-width disagreement.
        assert_eq!(output.matches('H').count(), 1, "{output:?}");
        assert!(output.contains('B'));
        assert!(output.contains("          "), "{output:?}");
    }

    #[test]
    fn ansi_frame_diff_writes_changed_row_suffix() {
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
