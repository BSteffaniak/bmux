//! Render buffer primitives.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::geometry::{Point, Rect, Size};
use crate::style::Style;
use crate::text::Line;

/// One render-buffer cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Cell symbol.
    pub symbol: String,
    /// Cell style.
    pub style: Style,
    width: u8,
    wide_continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            symbol: " ".to_owned(),
            style: Style::new(),
            width: 1,
            wide_continuation: false,
        }
    }
}

impl Cell {
    /// Create a cell from a single-cell symbol and style.
    #[must_use]
    pub fn new(symbol: impl Into<String>, style: Style) -> Self {
        let symbol = symbol.into();
        Self {
            width: u8::try_from(grapheme_width(&symbol).max(1)).unwrap_or(2),
            symbol,
            style,
            wide_continuation: false,
        }
    }

    /// Set this cell's symbol and style as a standalone logical cell.
    pub fn set(&mut self, symbol: impl Into<String>, style: Style) {
        let symbol = symbol.into();
        self.width = u8::try_from(grapheme_width(&symbol).max(1)).unwrap_or(2);
        self.symbol = symbol;
        self.style = style;
        self.wide_continuation = false;
    }

    /// Return this logical cell's terminal width.
    #[must_use]
    pub const fn width(&self) -> u8 {
        self.width
    }

    /// Return whether this physical cell continues the wide leader to its left.
    #[must_use]
    pub const fn is_wide_continuation(&self) -> bool {
        self.wide_continuation
    }

    /// Return whether this cell leads a width-two grapheme.
    #[must_use]
    pub const fn is_wide_leader(&self) -> bool {
        self.width == 2 && !self.wide_continuation
    }

    fn set_continuation(&mut self, style: Style) {
        self.symbol.clear();
        self.style = style;
        self.width = 0;
        self.wide_continuation = true;
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

    /// Expand rectangles to include every old/new wide span they touch.
    #[must_use]
    pub fn expand_regions_to_cell_spans(&self, other: &Self, regions: &[Rect]) -> Vec<Rect> {
        regions
            .iter()
            .map(|region| {
                let mut expanded = region.intersection(self.area);
                if expanded.is_empty() || self.area != other.area {
                    return expanded;
                }
                for y in expanded.y..expanded.bottom() {
                    let left = expanded.x;
                    if [self, other].iter().any(|buffer| {
                        buffer
                            .get(Point::new(left, y))
                            .is_some_and(Cell::is_wide_continuation)
                    }) {
                        expanded.x = expanded.x.saturating_sub(1).max(self.area.x);
                        expanded.width = expanded.right().saturating_sub(expanded.x);
                    }
                    let right = expanded.right().saturating_sub(1);
                    if [self, other].iter().any(|buffer| {
                        buffer
                            .get(Point::new(right, y))
                            .is_some_and(Cell::is_wide_leader)
                    }) {
                        expanded.width = expanded
                            .width
                            .saturating_add(1)
                            .min(self.area.right().saturating_sub(expanded.x));
                    }
                }
                expanded
            })
            .collect()
    }

    /// Assert wide-span topology in debug/test builds.
    pub fn debug_assert_valid_wide_spans(&self) {
        for y in self.area.y..self.area.bottom() {
            for x in self.area.x..self.area.right() {
                let point = Point::new(x, y);
                let Some(cell) = self.get(point) else {
                    continue;
                };
                if cell.is_wide_leader() {
                    debug_assert!(x.saturating_add(1) < self.area.right());
                    debug_assert!(
                        self.get(Point::new(x.saturating_add(1), y))
                            .is_some_and(Cell::is_wide_continuation)
                    );
                }
                if cell.is_wide_continuation() {
                    debug_assert!(x > self.area.x);
                    debug_assert!(
                        self.get(Point::new(x - 1, y))
                            .is_some_and(Cell::is_wide_leader)
                    );
                }
            }
        }
    }

    /// Restore cells outside `regions` from `retained`, leaving region cells as rendered.
    pub fn restore_outside(&mut self, retained: &Self, regions: &[Rect]) {
        if self.area != retained.area {
            return;
        }
        for y in self.area.y..self.area.bottom() {
            for x in self.area.x..self.area.right() {
                let point = Point::new(x, y);
                if regions.iter().any(|region| region.contains(point)) {
                    continue;
                }
                if let (Some(source), Some(destination)) =
                    (retained.get(point), self.get_mut(point))
                {
                    destination.clone_from(source);
                }
            }
        }
    }

    /// Set a standalone cell if the point is inside this buffer.
    ///
    /// Existing wide spans touching the destination are cleared first. Width-two graphemes are
    /// installed atomically with an explicit continuation cell when room remains.
    pub fn set_cell(&mut self, point: Point, symbol: impl Into<String>, style: Style) {
        if !self.area.contains(point) {
            return;
        }
        self.clear_span_at(point);
        let symbol = symbol.into();
        let width = grapheme_width(&symbol);
        if width == 2 && point.x.saturating_add(1) < self.area.right() {
            self.clear_span_at(Point::new(point.x.saturating_add(1), point.y));
            if let Some(cell) = self.get_mut(point) {
                cell.set(symbol, style);
                cell.width = 2;
            }
            if let Some(cell) = self.get_mut(Point::new(point.x.saturating_add(1), point.y)) {
                cell.set_continuation(style);
            }
        } else if let Some(cell) = self.get_mut(point) {
            cell.set(symbol, style);
            cell.width = 1;
        }
    }

    fn clear_span_at(&mut self, point: Point) {
        let Some(cell) = self.get(point) else {
            return;
        };
        let (start, width) = if cell.is_wide_continuation() && point.x > self.area.x {
            (Point::new(point.x - 1, point.y), 2)
        } else if cell.is_wide_leader() {
            (point, 2)
        } else {
            (point, 1)
        };
        for offset in 0..width {
            if let Some(cell) = self.get_mut(Point::new(start.x.saturating_add(offset), start.y)) {
                *cell = Cell::default();
            }
        }
    }

    /// Fill a rectangular area, clipped to this buffer.
    pub fn fill(&mut self, area: Rect, symbol: &str, style: Style) {
        let clip = self.area.intersection(area);
        if clip.is_empty() {
            return;
        }
        for y in clip.y..clip.bottom() {
            for x in clip.x..clip.right() {
                self.set_cell(Point::new(x, y), symbol, style);
            }
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
            for grapheme in span.content.graphemes(true) {
                let width = grapheme_width(grapheme);
                if width == 0 {
                    continue;
                }
                if x >= area.right() {
                    return;
                }
                if x >= clip.x && x < clip.right() {
                    self.set_cell(Point::new(x, area.y), grapheme.to_owned(), span.style);
                }
                x = x.saturating_add(width);
            }
        }
    }

    /// Fill `area` with `style`, then write `line` with that style applied as
    /// a fallback behind every span.
    pub fn write_line_with_fallback_style(&mut self, area: Rect, line: &Line, style: Style) {
        self.fill(area, " ", style);
        self.write_line(area, &line.with_fallback_style(style));
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

fn grapheme_width(grapheme: &str) -> u16 {
    match UnicodeWidthStr::width(grapheme) {
        0 => 0,
        1 => 1,
        _ => 2,
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
    fn fill_clips_to_area_and_buffer() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));

        buffer.fill(Rect::new(1, 0, 4, 1), "x", Style::new());

        assert_eq!(buffer.row_symbols(0).as_deref(), Some(" xxx"));
        assert_eq!(buffer.row_symbols(1).as_deref(), Some("    "));
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

    #[test]
    fn write_line_with_fallback_style_fills_row_and_patches_spans() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let fallback = Style::new().fg(Color::White).bg(Color::Black);
        let explicit = Style::new().fg(Color::Red);
        let line = Line::from_spans(vec![Span::styled("a", explicit)]);

        buffer.write_line_with_fallback_style(Rect::new(0, 0, 4, 1), &line, fallback);

        assert_eq!(buffer.row_symbols(0).as_deref(), Some("a   "));
        assert_eq!(
            buffer.get(Point::new(0, 0)).map(|cell| cell.style),
            Some(fallback.patch(explicit))
        );
        assert_eq!(
            buffer.get(Point::new(1, 0)).map(|cell| cell.style),
            Some(fallback)
        );
    }

    #[test]
    fn overwriting_either_half_of_wide_span_clears_complete_topology() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        buffer.write_line(Rect::new(0, 0, 4, 1), &Line::raw("A👩🏽‍💻B"));
        buffer.set_cell(Point::new(2, 0), "X", Style::new());
        buffer.debug_assert_valid_wide_spans();

        assert_eq!(buffer.row_symbols(0).as_deref(), Some("A XB"));
        assert!(!buffer.get(Point::new(1, 0)).unwrap().is_wide_leader());
        assert!(!buffer.get(Point::new(2, 0)).unwrap().is_wide_continuation());
    }

    #[test]
    fn damage_regions_expand_over_wide_leader_and_continuation() {
        let previous = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut current = previous.clone();
        current.write_line(Rect::new(0, 0, 4, 1), &Line::raw("A👩🏽‍💻B"));

        assert_eq!(
            current.expand_regions_to_cell_spans(&previous, &[Rect::new(2, 0, 1, 1)]),
            [Rect::new(1, 0, 2, 1)]
        );
        assert_eq!(
            current.expand_regions_to_cell_spans(&previous, &[Rect::new(1, 0, 1, 1)]),
            [Rect::new(1, 0, 2, 1)]
        );
    }

    #[test]
    fn write_line_preserves_combining_graphemes() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let line = Line::raw("e\u{301}x");

        buffer.write_line(Rect::new(0, 0, 2, 1), &line);

        assert_eq!(buffer.row_symbols(0).as_deref(), Some("e\u{301}x"));
    }

    #[test]
    fn write_line_preserves_emoji_zwj_sequence() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let line = Line::raw("👨‍👩‍👧‍👦x");

        buffer.write_line(Rect::new(0, 0, 3, 1), &line);

        assert_eq!(buffer.row_symbols(0).as_deref(), Some("👨‍👩‍👧‍👦x"));
        assert_eq!(
            buffer
                .get(Point::new(1, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("")
        );
    }
}
