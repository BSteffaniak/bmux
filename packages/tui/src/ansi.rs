//! ANSI terminal backend helpers.

mod log;
pub use log::AnsiLog;

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

/// Paint a buffer at the current cursor row without absolute positioning.
///
/// Each row is cleared before painting and ends at column zero of the next row.
/// The caller owns row reservation and must leave a spare terminal column to
/// avoid automatic wrapping. Uses the same style and wide-cell encoder as frames.
///
/// # Errors
/// Returns any writer error.
pub fn write_ansi_inline_frame(writer: &mut impl Write, buffer: &Buffer) -> io::Result<()> {
    let area = buffer.area();
    let mut style = Style::new();
    write_ansi_style(writer, style)?;
    for y in area.y..area.bottom() {
        writer.write_all(b"\r\x1b[2K")?;
        emit_row_cells(writer, buffer, y, area.x, area.right(), &mut style)?;
        writer.write_all(b"\r\n")?;
    }
    write_ansi_style(writer, Style::new())
}

/// Update a same-sized inline buffer from the cursor row immediately below it.
///
/// Only changed cell spans are emitted; unchanged frames emit no bytes. The
/// cursor returns below the buffer without newlines or clearing visible rows.
///
/// # Errors
/// Returns a writer error or invalid input for differently sized buffers.
pub fn write_ansi_inline_frame_diff(
    writer: &mut impl Write,
    previous: &Buffer,
    current: &Buffer,
) -> io::Result<()> {
    if previous.area() != current.area() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inline diff requires matching frame areas",
        ));
    }
    let area = current.area();
    let mut style = Style::new();
    let mut started = false;
    for y in area.y..area.bottom() {
        if let Some((start, end)) = changed_row_suffix(previous, current, y) {
            if !started {
                write_ansi_style(writer, style)?;
                started = true;
            }
            let distance = area.bottom() - y;
            write!(writer, "\r\x1b[{distance}A")?;
            let column = start - area.x;
            if column > 0 {
                write!(writer, "\x1b[{column}C")?;
            }
            emit_row_cells(writer, current, y, start, end, &mut style)?;
            write!(writer, "\r\x1b[{distance}B")?;
        }
    }
    if started {
        write_ansi_style(writer, Style::new())?;
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
            changed_cells = changed_cells.saturating_add(emit_sparse_row_span(
                writer,
                previous,
                current,
                y,
                start..end,
                &mut active_style,
            )?);
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

// Only skip gaps whose omitted output is exactly one byte per cell and cannot
// affect the active style. A CUP is at most 14 bytes for u16 coordinates (plus
// one), so 16 cells conservatively pays for it without buffering or costing SGR.
fn emit_sparse_row_span(
    writer: &mut impl Write,
    previous: &Buffer,
    current: &Buffer,
    y: u16,
    span: std::ops::Range<u16>,
    active_style: &mut Style,
) -> io::Result<usize> {
    let mut start = span.start;
    let mut x = start;
    let mut written = 0;
    while x < span.end {
        let gap_start = x;
        let style = current.get(Point::new(x, y)).map(|cell| cell.style);
        while x < span.end {
            let point = Point::new(x, y);
            let Some(cell) = current.get(point) else {
                break;
            };
            if previous.get(point) != Some(cell)
                || Some(cell.style) != style
                || cell.width() != 1
                || cell.is_wide_continuation()
                || cell.symbol.len() != 1
                || !cell.symbol.as_bytes()[0].is_ascii_graphic() && cell.symbol != " "
            {
                break;
            }
            x += 1;
        }
        if x - gap_start >= 16
            && gap_start > start
            && x < span.end
            && current
                .get(Point::new(gap_start - 1, y))
                .is_some_and(|cell| Some(cell.style) == style)
            && current
                .get(Point::new(x, y))
                .is_some_and(|cell| Some(cell.style) == style && !cell.is_wide_continuation())
        {
            emit_row_span(writer, current, y, start, gap_start, active_style)?;
            written += usize::from(gap_start - start);
            start = x;
        }
        // Advance past a changed/ineligible cell; an eligible run was already scanned.
        if x < span.end {
            x += 1;
        }
    }
    emit_row_span(writer, current, y, start, span.end, active_style)?;
    Ok(written + usize::from(span.end - start))
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
    emit_row_cells(writer, buffer, y, start, end, active_style)
}

fn emit_row_cells(
    writer: &mut impl Write,
    buffer: &Buffer,
    y: u16,
    start: u16,
    end: u16,
    active_style: &mut Style,
) -> io::Result<()> {
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
        // `Cell::symbol` is public, so defend even against callers bypassing buffer
        // setters. Never let cell text inject terminal protocol or shift the cursor.
        if cell.symbol.chars().any(char::is_control) {
            let width = u16::from(cell.width()).max(1).min(end - x);
            for _ in 0..width {
                writer.write_all(b" ")?;
            }
            x = x.saturating_add(width);
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
        // Retain only bounded, ordinary SGR parameters. Private prefixes and
        // intermediates (for example modifyOtherKeys) are not text styling.
        const MAX_SGR_BYTES: usize = 4096;
        let mut sequence = String::new();
        let mut valid = true;
        for ch in chars.by_ref() {
            if ('@'..='~').contains(&ch) {
                if ch == 'm' && valid {
                    apply_sgr(&mut self.style, &sequence);
                }
                return;
            }
            if valid
                && (ch.is_ascii_digit() || matches!(ch, ';' | ':'))
                && sequence.len() < MAX_SGR_BYTES
            {
                sequence.push(ch);
            } else {
                valid = false;
            }
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
    // Semicolons separate commands; colons group subparameters belonging to
    // one command. Never reinterpret malformed operands as independent SGRs.
    let mut params = sequence.split(';');
    while let Some(parameter) = params.next() {
        if parameter.contains(':') {
            apply_sgr_subparameters(style, parameter);
            continue;
        }
        let Some(code) = sgr_number(parameter, true) else {
            continue;
        };
        match code {
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
            30..=37 => style.fg = Some(standard_color(code - 30, false)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(standard_color(code - 40, false)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(standard_color(code - 90, true)),
            100..=107 => style.bg = Some(standard_color(code - 100, true)),
            38 | 48 | 58 => {
                let color = parse_sgr_color(&mut params, false);
                apply_sgr_color(style, code, color);
            }
            _ => {}
        }
    }
}

fn sgr_number(parameter: &str, empty_is_zero: bool) -> Option<u16> {
    if parameter.is_empty() && empty_is_zero {
        Some(0)
    } else if parameter.bytes().all(|byte| byte.is_ascii_digit()) {
        parameter.parse().ok()
    } else {
        None
    }
}

fn apply_sgr_subparameters(style: &mut Style, parameter: &str) {
    let mut parts = parameter.split(':');
    match parts.next().and_then(|part| sgr_number(part, false)) {
        Some(4) => {
            let kind = parts.next().and_then(|part| sgr_number(part, false));
            if parts.next().is_none() {
                match kind {
                    Some(0) => {
                        style.modifiers = style.modifiers.difference(Modifier::UNDERLINE);
                    }
                    // The text model has only basic underline; preserve that
                    // fallback for double, curly, dotted and dashed underline.
                    Some(1..=5) => style.modifiers |= Modifier::UNDERLINE,
                    _ => {}
                }
            }
        }
        Some(code @ (38 | 48 | 58)) => {
            let color = parse_sgr_color(&mut parts, true);
            if parts.next().is_none() {
                apply_sgr_color(style, code, color);
            }
        }
        _ => {}
    }
}

fn parse_sgr_color(params: &mut std::str::Split<'_, char>, colon: bool) -> Option<Color> {
    match params.next().and_then(|part| sgr_number(part, false)) {
        Some(5) => params.next()?.parse::<u8>().ok().map(Color::Indexed),
        Some(2) => {
            // ISO colon RGB may include an omitted/default color-space slot.
            // Semicolon legacy RGB has exactly three operands, even if invalid.
            let valid_space = if colon && params.clone().count() == 4 {
                matches!(params.next(), Some("" | "0"))
            } else {
                true
            };
            let red = params.next().and_then(|part| part.parse::<u8>().ok());
            let green = params.next().and_then(|part| part.parse::<u8>().ok());
            let blue = params.next().and_then(|part| part.parse::<u8>().ok());
            if valid_space {
                Some(Color::Rgb(red?, green?, blue?))
            } else {
                None
            }
        }
        _ => {
            // Unknown color modes have unknown arity. Do not turn their
            // remaining operands into resets or unrelated style changes.
            for _ in params.by_ref() {}
            None
        }
    }
}

const fn apply_sgr_color(style: &mut Style, code: u16, color: Option<Color>) {
    if let Some(color) = color {
        match code {
            38 => set_style_color(style, ColorRole::Foreground, color),
            48 => set_style_color(style, ColorRole::Background, color),
            // Underline color is unsupported, but its operands are consumed.
            _ => {}
        }
    }
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
    fn distant_changes_bound_gap_bytes() {
        for width in [80, 800, 8_000] {
            let previous = Buffer::empty(Rect::new(0, 0, width, 1));
            let mut current = previous.clone();
            current.set_cell(Point::new(0, 0), "A", Style::new());
            current.set_cell(Point::new(width - 1, 0), "Z", Style::new());
            let mut reference = b"\x1b[?25l".to_vec();
            reference.extend(reference_row_diff(&previous, &current));
            let mut output = Vec::new();
            let stats = write_ansi_frame_diff(&mut output, &previous, &current, None).unwrap();
            eprintln!(
                "sparse width={width}: reference_bytes={} new_bytes={}",
                reference.len(),
                output.len()
            );
            assert!(reference.len() > 40, "reference must fail the same budget");
            assert_eq!(replay_frame(previous.clone(), &reference), current);
            assert_eq!(replay_frame(previous, &output), current);
            assert!(output.len() <= 40, "width {width}: {} bytes", output.len());
            assert_eq!(stats.changed_cells, 2);
            assert!(!output.windows(16).any(|bytes| bytes == b"                "));
        }
    }

    // Replay just the frame encoder's wire vocabulary, independently of its span selection.
    fn replay_frame(mut buffer: Buffer, wire: &[u8]) -> Buffer {
        use unicode_segmentation::UnicodeSegmentation;

        let wire = std::str::from_utf8(wire).unwrap();
        let mut remaining = wire;
        let mut point = Point::new(0, 0);
        let mut style = Style::new();
        while !remaining.is_empty() {
            if let Some(csi) = remaining.strip_prefix("\x1b[") {
                let end = csi.find(|ch: char| ch.is_ascii_alphabetic()).unwrap();
                let parameters = &csi[..end];
                match csi.as_bytes()[end] {
                    b'H' => {
                        let (row, col) = parameters.split_once(';').unwrap();
                        point = Point::new(
                            col.parse::<u16>().unwrap() - 1,
                            row.parse::<u16>().unwrap() - 1,
                        );
                    }
                    b'm' => super::apply_sgr(&mut style, parameters),
                    b'h' | b'l' => {}
                    command => panic!("unexpected CSI {command}"),
                }
                remaining = &csi[end + 1..];
            } else {
                let end = remaining.find('\x1b').unwrap_or(remaining.len());
                for grapheme in remaining[..end].graphemes(true) {
                    buffer.set_cell(point, grapheme, style);
                    point.x += u16::try_from(super::display_width(grapheme)).unwrap();
                }
                remaining = &remaining[end..];
            }
        }
        buffer
    }

    fn reference_row_diff(previous: &Buffer, current: &Buffer) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut style = Style::new();
        for y in current.area().y..current.area().bottom() {
            if let Some((start, end)) = super::changed_row_suffix(previous, current, y) {
                super::emit_row_span(&mut wire, current, y, start, end, &mut style).unwrap();
            }
        }
        super::write_ansi_style(&mut wire, Style::new()).unwrap();
        wire
    }

    #[test]
    fn sparse_diff_matches_reference_wire_semantics_without_byte_growth() {
        let red = Style::new().fg(Color::Red);
        for gap in [0, 1, 8, 15, 16, 17, 40, 80] {
            for symbol in ["x", "界", "👩‍💻", "e\u{301}"] {
                for gap_style in [Style::new(), red] {
                    let mut previous = Buffer::empty(Rect::new(0, 0, 100, 2));
                    previous.fill(Rect::new(0, 0, 100, 2), ".", gap_style);
                    previous.set_cell(Point::new(0, 0), "界", red);
                    let mut current = previous.clone();
                    current.set_cell(Point::new(0, 0), symbol, Style::new());
                    current.set_cell(Point::new(3 + gap, 0), symbol, Style::new());
                    current.set_cell(Point::new(1, 1), "👩‍💻", red);
                    current.set_cell(Point::new(90, 1), "e\u{301}", red);
                    let reference = reference_row_diff(&previous, &current);
                    let mut wire = Vec::new();
                    write_ansi_frame_diff(&mut wire, &previous, &current, None).unwrap();
                    // The reference omits the common cursor-hide prefix.
                    assert!(wire.len() <= reference.len() + b"\x1b[?25l".len());
                    assert_eq!(replay_frame(previous.clone(), &reference), current);
                    assert_eq!(replay_frame(previous, &wire), current);
                }
            }
        }
    }

    #[test]
    fn raw_cell_controls_cannot_escape_full_or_diff_output() {
        for payload in ["\x1b[2J", "\x1b]0;injected\x07", "\r\n", "\t", "\u{9b}2J"] {
            let previous = Buffer::empty(Rect::new(0, 0, 8, 1));
            let mut current = previous.clone();
            current.get_mut(Point::new(0, 0)).unwrap().symbol = payload.to_owned();
            current.set_cell(Point::new(1, 0), "X", Style::new());
            let mut output = Vec::new();
            let mut style = Style::new();
            super::emit_row_cells(&mut output, &current, 0, 0, 8, &mut style).unwrap();
            assert_eq!(output, b" X      ");
            let mut expected = current.clone();
            expected.set_cell(Point::new(0, 0), " ", Style::new());
            let mut actual = Vec::new();
            let mut safe = Vec::new();
            write_ansi_frame(&mut actual, &current, None).unwrap();
            write_ansi_frame(&mut safe, &expected, None).unwrap();
            assert_eq!(actual, safe);
            // A changed first cell may widen the diff span, but never emits its payload.
            actual.clear();
            write_ansi_frame_diff(&mut actual, &previous, &current, None).unwrap();
            assert!(
                !actual
                    .windows(payload.len())
                    .any(|bytes| bytes == payload.as_bytes())
            );
        }
    }

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
    fn ansi_sgr_underline_subparameters_preserve_other_styles() {
        for kind in 1..=5 {
            let lines = ansi_to_lines(&format!("\x1b[1;31;44m\x1b[4:{kind}mX"));
            assert_eq!(
                lines[0].spans[0].style,
                Style::new()
                    .fg(Color::Red)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINE)
            );
        }
        for off in ["4:0", "24"] {
            let lines = ansi_to_lines(&format!("\x1b[1;3;4;31;44m\x1b[{off}mX"));
            assert_eq!(
                lines[0].spans[0].style,
                Style::new()
                    .fg(Color::Red)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC)
            );
        }
    }

    #[test]
    fn ansi_sgr_extended_colors_accept_supported_encodings() {
        for role in [38, 48] {
            for operands in ["2::12:34:56", "2:12:34:56", "2:0:12:34:56", "2;12;34;56"] {
                let separator = if operands.contains(':') { ':' } else { ';' };
                let lines = ansi_to_lines(&format!("\x1b[{role}{separator}{operands};1mX"));
                let style = lines[0].spans[0].style;
                assert_eq!(
                    if role == 38 { style.fg } else { style.bg },
                    Some(Color::Rgb(12, 34, 56))
                );
                assert_eq!(style.modifiers, Modifier::BOLD);
            }
            for operands in ["5:42", "5;42"] {
                let separator = if operands.contains(':') { ':' } else { ';' };
                let lines = ansi_to_lines(&format!("\x1b[{role}{separator}{operands}mX"));
                let style = lines[0].spans[0].style;
                assert_eq!(
                    if role == 38 { style.fg } else { style.bg },
                    Some(Color::Indexed(42))
                );
            }
        }
    }

    #[test]
    fn ansi_sgr_unsupported_underline_color_consumes_operands() {
        for color in [
            "58;2;0;3;4",
            "58:2::0:3:4",
            "58:2:0:3:4",
            "58;5;0",
            "58:5:3",
        ] {
            let lines = ansi_to_lines(&format!("\x1b[1;31;44m\x1b[{color};4mX"));
            assert_eq!(
                lines[0].spans[0].style,
                Style::new()
                    .fg(Color::Red)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINE),
                "{color}"
            );
        }
    }

    #[test]
    fn ansi_sgr_invalid_color_operands_never_become_styles() {
        for color in [
            "38;2;999;0;3",
            "48;2;;0;4",
            "58;2;999;0;3",
            "38;2;0;3",
            "48;2;0",
            "58;2;0;4",
            "38;5;999",
            "38;2;999999999999999999999999;0;3",
            "38;9;0;3;4",
            "38:2::999:0:3",
            "48:2:0:3",
            "58:5:999",
            "38:2:1:12:34:56",
            "38:2::12:34:56:0",
            "38:5:42:0",
            "38:2::12::56",
            "48:2:12:34:999",
            "38:2:12:34:56:0:0",
        ] {
            let lines = ansi_to_lines(&format!("\x1b[1;31;44m\x1b[{color}mX"));
            assert_eq!(
                lines[0].spans[0].style,
                Style::new()
                    .fg(Color::Red)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
                "{color}"
            );
        }
        // A malformed but fully consumed operand group does not swallow the
        // next independent command.
        let lines = ansi_to_lines("\x1b[1;31m\x1b[38;2;999;0;3;4mX");
        assert_eq!(
            lines[0].spans[0].style.modifiers,
            Modifier::BOLD | Modifier::UNDERLINE
        );
    }

    #[test]
    fn ansi_sgr_rejects_private_intermediate_and_malformed_sequences() {
        for parameters in [
            ">4;2",
            "?0",
            "=0",
            "0 ",
            "4:99",
            "4:",
            "4:0:3",
            "1:0",
            "999999999999999999999",
        ] {
            let lines = ansi_to_lines(&format!("\x1b[1;31m\x1b[{parameters}mX"));
            assert_eq!(lines[0].plain_text(), "X");
            assert_eq!(
                lines[0].spans[0].style,
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                "{parameters}"
            );
        }
        let oversized = format!("\x1b[1;31m\x1b[{}0mX\x1b[mY", "0;".repeat(4096));
        let lines = ansi_to_lines(&oversized);
        assert_eq!(lines[0].plain_text(), "XY");
        assert_eq!(
            lines[0].spans[0].style,
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[1].style, Style::new());
    }

    #[test]
    fn ansi_sgr_color_resets_preserve_other_state() {
        for (code, fg, bg) in [(39, None, Some(Color::Blue)), (49, Some(Color::Red), None)] {
            let lines = ansi_to_lines(&format!("\x1b[1;3;31;44m\x1b[{code}mX"));
            let style = lines[0].spans[0].style;
            assert_eq!(style.fg, fg);
            assert_eq!(style.bg, bg);
            assert_eq!(style.modifiers, Modifier::BOLD | Modifier::ITALIC);
        }
        let lines = ansi_to_lines("\x1b[1;3m\x1b[38:2::0:3:4;48;2;4;0;3mX");
        assert_eq!(
            lines[0].spans[0].style,
            Style::new()
                .fg(Color::Rgb(0, 3, 4))
                .bg(Color::Rgb(4, 0, 3))
                .add_modifier(Modifier::BOLD | Modifier::ITALIC)
        );
    }

    #[test]
    fn ansi_sgr_style_off_preserves_unrelated_state() {
        let initial = Modifier::BOLD
            | Modifier::DIM
            | Modifier::ITALIC
            | Modifier::UNDERLINE
            | Modifier::SLOW_BLINK
            | Modifier::REVERSED
            | Modifier::HIDDEN
            | Modifier::CROSSED_OUT;
        for (code, removed) in [
            (22, Modifier::BOLD | Modifier::DIM),
            (23, Modifier::ITALIC),
            (24, Modifier::UNDERLINE),
            (25, Modifier::SLOW_BLINK),
            (27, Modifier::REVERSED),
            (28, Modifier::HIDDEN),
            (29, Modifier::CROSSED_OUT),
        ] {
            let lines = ansi_to_lines(&format!("\x1b[1;2;3;4;5;7;8;9;31;44m\x1b[{code}mX"));
            assert_eq!(
                lines[0].spans[0].style,
                Style::new()
                    .fg(Color::Red)
                    .bg(Color::Blue)
                    .add_modifier(initial.difference(removed))
            );
        }
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
