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

/// Presentation colors for terminal output: what the sixteen ANSI colors and, optionally, the
/// default foreground and background look like.
///
/// Terminal output maps ANSI indexed colors 0-15 to the named colors, which the host terminal
/// paints from its own palette. A palette replaces them when output is painted, so an
/// application can show terminal output in its own theme's colors. The terminal grid is not
/// changed: indexed colors 16-255 and true colors are painted as the program wrote them, and
/// text, selection, and hit regions are the same with or without a palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalPalette {
    /// Colors for ANSI indices 0-15 (black, red, green, yellow, blue, magenta, cyan, white,
    /// then their bright variants), typically [`Color::Rgb`].
    pub ansi: [Color; 16],
    /// Foreground for text the program left in the default color; `None` keeps the host
    /// default.
    pub foreground: Option<Color>,
    /// Background for cells the program left in the default color; `None` keeps the host
    /// default. Only cells the terminal holds are painted.
    pub background: Option<Color>,
}

impl TerminalPalette {
    /// A palette with `ansi` colors for indices 0-15 that keeps the host default foreground
    /// and background.
    #[must_use]
    pub const fn new(ansi: [Color; 16]) -> Self {
        Self {
            ansi,
            foreground: None,
            background: None,
        }
    }

    /// Paint default-colored text with `color`.
    #[must_use]
    pub const fn with_foreground(mut self, color: Color) -> Self {
        self.foreground = Some(color);
        self
    }

    /// Paint default-colored cells with background `color`.
    #[must_use]
    pub const fn with_background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    /// The color `color` is painted with: ANSI colors 0-15 (named or indexed) come from the
    /// palette, every other color is unchanged.
    #[must_use]
    pub const fn color(&self, color: Color) -> Color {
        match ansi_index(color) {
            Some(index) => self.ansi[index as usize],
            None => color,
        }
    }

    /// `style` with its colors painted from this palette; default colors become the
    /// palette's foreground and background when it sets them. Modifiers are unchanged.
    #[must_use]
    pub const fn style(&self, mut style: Style) -> Style {
        style.fg = match style.fg {
            Some(color) => Some(self.color(color)),
            None => self.foreground,
        };
        style.bg = match style.bg {
            Some(color) => Some(self.color(color)),
            None => self.background,
        };
        style
    }

    /// `line` with every span's style painted from this palette. Text is unchanged.
    #[must_use]
    pub fn line(&self, mut line: Line) -> Line {
        for span in &mut line.spans {
            span.style = self.style(span.style);
        }
        line
    }
}

/// The ANSI index 0-15 of `color`, if it is one of the sixteen ANSI colors.
const fn ansi_index(color: Color) -> Option<u8> {
    Some(match color {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::White => 7,
        Color::BrightBlack => 8,
        Color::BrightRed => 9,
        Color::BrightGreen => 10,
        Color::BrightYellow => 11,
        Color::BrightBlue => 12,
        Color::BrightMagenta => 13,
        Color::BrightCyan => 14,
        Color::BrightWhite => 15,
        Color::Indexed(index) if index < 16 => index,
        Color::Indexed(_) | Color::Rgb(..) | Color::Default => return None,
    })
}
