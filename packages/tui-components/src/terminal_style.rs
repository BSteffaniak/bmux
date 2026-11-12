//! Conversion of terminal grid styles to TUI styles, shared by the terminal components.

use bmux_terminal_grid::{Color as GridColor, PhysicalRow, Style as GridStyle, TerminalGrid};
use bmux_tui::prelude::{Color, Line, Span, Style};
use bmux_tui::style::Modifier;

/// The TUI style of a terminal grid style. ANSI indexed colors 0-15 map to the named
/// colors, so they follow the host terminal's palette.
#[must_use]
pub const fn terminal_grid_style(style: GridStyle) -> Style {
    let mut output = Style::new();
    if let Some(fg) = style.fg {
        output = output.fg(terminal_grid_color(fg));
    }
    if let Some(bg) = style.bg {
        output = output.bg(terminal_grid_color(bg));
    }
    if style.bold {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        output = output.add_modifier(Modifier::UNDERLINE);
    }
    if style.dim {
        output = output.add_modifier(Modifier::DIM);
    }
    if style.inverse {
        output = output.add_modifier(Modifier::REVERSED);
    }
    if style.hidden {
        output = output.add_modifier(Modifier::HIDDEN);
    }
    if style.strike {
        output = output.add_modifier(Modifier::CROSSED_OUT);
    }
    output
}

/// A row of `grid` as a styled line, adjacent cells of one style joined into a span.
#[must_use]
pub fn terminal_grid_line(grid: &TerminalGrid, row: &PhysicalRow) -> Line {
    let palette = grid.palette();
    let mut spans = Vec::new();
    let mut current: Option<(Style, String)> = None;
    for cell in row.cells() {
        if cell.is_wide_continuation() {
            continue;
        }
        let style = terminal_grid_style(palette.get(cell.style()));
        match &mut current {
            Some((current_style, text)) if *current_style == style => text.push_str(cell.text()),
            _ => {
                if let Some((style, text)) = current.take() {
                    spans.push(Span::styled(text, style));
                }
                current = Some((style, cell.text().to_owned()));
            }
        }
    }
    if let Some((style, text)) = current {
        spans.push(Span::styled(text, style));
    }
    Line::from_spans(spans)
}

const fn terminal_grid_color(color: GridColor) -> Color {
    match color {
        GridColor::Indexed(index) => ansi_indexed_color(index),
        GridColor::Rgb { r, g, b } => Color::Rgb(r, g, b),
    }
}

const fn ansi_indexed_color(index: u8) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::White,
        8 => Color::BrightBlack,
        9 => Color::BrightRed,
        10 => Color::BrightGreen,
        11 => Color::BrightYellow,
        12 => Color::BrightBlue,
        13 => Color::BrightMagenta,
        14 => Color::BrightCyan,
        15 => Color::BrightWhite,
        other => Color::Indexed(other),
    }
}
