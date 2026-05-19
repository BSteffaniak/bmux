//! Render buffer primitives.

use unicode_width::UnicodeWidthChar;

use crate::geometry::{Point, Rect, Size};
use crate::style::Style;
use crate::text::Line;

/// One render-buffer cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Cell symbol. Wide continuation cells are represented as an empty string.
    pub symbol: String,
    /// Cell style.
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            symbol: " ".to_owned(),
            style: Style::new(),
        }
    }
}

impl Cell {
    /// Create a cell from a single-cell symbol and style.
    #[must_use]
    pub fn new(symbol: impl Into<String>, style: Style) -> Self {
        Self {
            symbol: symbol.into(),
            style,
        }
    }

    /// Set this cell's symbol and style.
    pub fn set(&mut self, symbol: impl Into<String>, style: Style) {
        self.symbol = symbol.into();
        self.style = style;
    }
}

/// A two-dimensional terminal render buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
    area: Rect,
    cells: Vec<Cell>,
}

impl Buffer {
    /// Create a new buffer for `area`.
    #[must_use]
    pub fn empty(area: Rect) -> Self {
        let len = usize::from(area.width) * usize::from(area.height);
        Self {
            area,
            cells: vec![Cell::default(); len],
        }
    }

    /// Return the buffer area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.area
    }

    /// Return the buffer size.
    #[must_use]
    pub const fn size(&self) -> Size {
        self.area.size()
    }

    /// Return all cells in row-major order.
    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// Return an immutable cell at an absolute terminal point.
    #[must_use]
    pub fn get(&self, point: Point) -> Option<&Cell> {
        self.index(point).and_then(|index| self.cells.get(index))
    }

    /// Return a mutable cell at an absolute terminal point.
    pub fn get_mut(&mut self, point: Point) -> Option<&mut Cell> {
        self.index(point)
            .and_then(|index| self.cells.get_mut(index))
    }

    /// Set a cell if the point is inside this buffer.
    pub fn set_cell(&mut self, point: Point, symbol: impl Into<String>, style: Style) {
        if let Some(cell) = self.get_mut(point) {
            cell.set(symbol, style);
        }
    }

    /// Write styled text clipped to the supplied absolute area.
    pub fn write_line(&mut self, area: Rect, line: &Line) {
        let clip = self.area.intersection(area);
        if clip.is_empty() {
            return;
        }

        let mut x = area.x;
        for span in &line.spans {
            for ch in span.content.chars() {
                let width = char_width(ch);
                if width == 0 {
                    continue;
                }
                if x >= area.right() {
                    return;
                }
                if x >= clip.x && x < clip.right() {
                    self.set_cell(Point::new(x, area.y), ch.to_string(), span.style);
                    if width == 2 && x.saturating_add(1) < clip.right() {
                        self.set_cell(Point::new(x.saturating_add(1), area.y), "", span.style);
                    }
                }
                x = x.saturating_add(width);
            }
        }
    }

    /// Return plain symbols for one absolute row in this buffer.
    #[must_use]
    pub fn row_symbols(&self, y: u16) -> Option<String> {
        if y < self.area.y || y >= self.area.bottom() {
            return None;
        }
        let mut row = String::new();
        for x in self.area.x..self.area.right() {
            if let Some(cell) = self.get(Point::new(x, y)) {
                row.push_str(&cell.symbol);
            }
        }
        Some(row)
    }

    fn index(&self, point: Point) -> Option<usize> {
        if !self.area.contains(point) {
            return None;
        }
        let x = usize::from(point.x.saturating_sub(self.area.x));
        let y = usize::from(point.y.saturating_sub(self.area.y));
        Some(y * usize::from(self.area.width) + x)
    }
}

fn char_width(ch: char) -> u16 {
    match UnicodeWidthChar::width(ch) {
        Some(0) | None => 0,
        Some(1) => 1,
        Some(_) => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::Buffer;
    use crate::geometry::{Point, Rect};
    use crate::style::{Color, Style};
    use crate::text::{Line, Span};

    #[test]
    fn set_cell_ignores_points_outside_buffer() {
        let mut buffer = Buffer::empty(Rect::new(2, 3, 4, 2));

        buffer.set_cell(Point::new(1, 3), "x", Style::new());
        buffer.set_cell(Point::new(2, 3), "y", Style::new());

        assert_eq!(buffer.row_symbols(3).as_deref(), Some("y   "));
    }

    #[test]
    fn write_line_clips_to_area_and_buffer() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let line = Line::raw("abcdef");

        buffer.write_line(Rect::new(2, 0, 2, 1), &line);

        assert_eq!(buffer.row_symbols(0).as_deref(), Some("  ab "));
    }

    #[test]
    fn write_line_preserves_span_styles() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let style = Style::new().fg(Color::Red);
        let line = Line::from_spans(vec![Span::styled("a", style), Span::raw("b")]);

        buffer.write_line(Rect::new(0, 0, 2, 1), &line);

        assert_eq!(
            buffer.get(Point::new(0, 0)).map(|cell| cell.style),
            Some(style)
        );
        assert_eq!(
            buffer.get(Point::new(1, 0)).map(|cell| cell.style),
            Some(Style::new())
        );
    }
}
