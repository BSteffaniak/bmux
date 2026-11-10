//! Render buffer primitives.
//!
//! Optional serde decoding is for output transport only. Decoders must bound
//! allocations and validate grid length, geometry, symbols, widths, and complete
//! wide-cell topology (including continuation styles) before consuming a buffer.
//! Private width/continuation fields are untrusted after decoding. A decoded
//! buffer cannot be installed into authoritative `PresentationState`.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::geometry::{Point, Rect, Size};
use crate::style::Style;
use crate::text::Line;

/// One render-buffer cell.
///
/// Setters reuse the symbol's allocation, so repainting a buffer in place does
/// not allocate per cell.
#[derive(Debug, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Cell {
    /// Cell symbol.
    pub symbol: String,
    /// Cell style.
    pub style: Style,
    width: u8,
    wide_continuation: bool,
}

// Frame diffs compare every cell: check the cheap fields first, and compare
// single-byte symbols without a `memcmp` call.
impl PartialEq for Cell {
    fn eq(&self, other: &Self) -> bool {
        self.style == other.style
            && self.width == other.width
            && self.wide_continuation == other.wide_continuation
            && match (self.symbol.as_bytes(), other.symbol.as_bytes()) {
                ([left], [right]) => left == right,
                (left, right) => left == right,
            }
    }
}

impl Clone for Cell {
    fn clone(&self) -> Self {
        Self {
            symbol: self.symbol.clone(),
            style: self.style,
            width: self.width,
            wide_continuation: self.wide_continuation,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.symbol.clone_from(&source.symbol);
        self.style = source.style;
        self.width = source.width;
        self.wide_continuation = source.wide_continuation;
    }
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
    ///
    /// Control-containing symbols become a visible replacement character. Parse ANSI
    /// into styled text before constructing cells when terminal styling is intended.
    #[must_use]
    pub fn new(symbol: impl Into<String>, style: Style) -> Self {
        let symbol = symbol.into();
        let symbol = if is_safe_symbol(&symbol) {
            symbol
        } else {
            REPLACEMENT.to_owned()
        };
        Self {
            width: u8::try_from(grapheme_width(&symbol).max(1)).unwrap_or(2),
            symbol,
            style,
            wide_continuation: false,
        }
    }

    /// Set this cell's symbol and style as a standalone logical cell.
    pub fn set(&mut self, symbol: impl AsRef<str>, style: Style) {
        let symbol = safe_symbol(symbol.as_ref());
        self.assign(
            symbol,
            u8::try_from(grapheme_width(symbol).max(1)).unwrap_or(2),
            style,
        );
    }

    /// Install an already sanitized symbol of known width, reusing the
    /// symbol's allocation.
    fn assign(&mut self, symbol: &str, width: u8, style: Style) {
        if self.symbol != symbol {
            self.symbol.clear();
            self.symbol.push_str(symbol);
        }
        self.style = style;
        self.width = width;
        self.wide_continuation = false;
    }

    fn reset(&mut self) {
        self.assign(" ", 1, Style::new());
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
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Buffer {
    area: Rect,
    cells: Vec<Cell>,
}

impl Clone for Buffer {
    fn clone(&self) -> Self {
        Self {
            area: self.area,
            cells: self.cells.clone(),
        }
    }

    /// Copy `source` reusing this buffer's cell storage.
    fn clone_from(&mut self, source: &Self) {
        self.area = source.area;
        self.cells.clone_from(&source.cells);
    }
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

    /// Blank this buffer and give it `area`, reusing its cell storage: a
    /// recycled buffer repaints without allocating.
    pub fn reset(&mut self, area: Rect) {
        let len = usize::from(area.width) * usize::from(area.height);
        self.area = area;
        self.cells.truncate(len);
        for cell in &mut self.cells {
            cell.reset();
        }
        self.cells.resize_with(len, Cell::default);
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
                loop {
                    let previous = expanded;
                    for y in expanded.y..expanded.bottom() {
                        let left = expanded.x;
                        if [self, other].iter().any(|buffer| {
                            buffer
                                .get(Point::new(left, y))
                                .is_some_and(Cell::is_wide_continuation)
                        }) {
                            let right = expanded.right();
                            expanded.x = expanded.x.saturating_sub(1).max(self.area.x);
                            expanded.width = right.saturating_sub(expanded.x);
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
                    if expanded == previous {
                        break;
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
    pub fn set_cell(&mut self, point: Point, symbol: impl AsRef<str>, style: Style) {
        let symbol = safe_symbol(symbol.as_ref());
        self.put_symbol(point, symbol, grapheme_width(symbol), style);
    }

    /// Set a cell to an already sanitized symbol of known grapheme width.
    fn put_symbol(&mut self, point: Point, symbol: &str, width: u16, style: Style) {
        let Some(index) = self.index(point) else {
            return;
        };
        // Common case: a narrow symbol over a narrow cell touches nothing else.
        let cell = &mut self.cells[index];
        if width < 2 && cell.width == 1 && !cell.wide_continuation {
            cell.assign(symbol, 1, style);
            return;
        }
        self.clear_span_at(point);
        if width == 2 && point.x.saturating_add(1) < self.area.right() {
            let next = Point::new(point.x.saturating_add(1), point.y);
            self.clear_span_at(next);
            self.cells[index].assign(symbol, 2, style);
            if let Some(cell) = self.get_mut(next) {
                cell.set_continuation(style);
            }
        } else {
            self.cells[index].assign(if width == 2 { " " } else { symbol }, 1, style);
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
                cell.reset();
            }
        }
    }

    /// Fill a rectangular area, clipped to this buffer.
    pub fn fill(&mut self, area: Rect, symbol: &str, style: Style) {
        let clip = self.area.intersection(area);
        if clip.is_empty() {
            return;
        }
        let symbol = safe_symbol(symbol);
        let symbol_width = grapheme_width(symbol);
        let width = symbol_width.max(1);
        for y in clip.y..clip.bottom() {
            let mut x = clip.x;
            let clipped_columns = clip.x.saturating_sub(area.x) % width;
            if clipped_columns != 0 {
                self.put_symbol(Point::new(x, y), " ", 1, style);
                x = x.saturating_add(width - clipped_columns);
            }
            while x < clip.right() {
                if width > clip.right().saturating_sub(x) {
                    self.put_symbol(Point::new(x, y), " ", 1, style);
                } else {
                    self.put_symbol(Point::new(x, y), symbol, symbol_width, style);
                }
                x = x.saturating_add(width);
            }
        }
    }

    /// Set a cell, layering `style` over the cell's current style: attributes
    /// `style` leaves unset (`None` colors) keep the existing cell's values,
    /// so text drawn over a filled background keeps that background. An
    /// explicit color (including [`Color::Default`](crate::style::Color))
    /// still replaces it. [`Self::set_cell`] replaces the whole style.
    pub fn overlay_cell(&mut self, point: Point, symbol: impl AsRef<str>, style: Style) {
        let symbol = safe_symbol(symbol.as_ref());
        self.put_cell(point, symbol, grapheme_width(symbol), style, true);
    }

    /// Write styled text clipped to the supplied absolute area, replacing the
    /// style of every written cell (see [`Self::overlay_line`]).
    pub fn write_line(&mut self, area: Rect, line: &Line) {
        self.write_line_cells(area, line, false, Style::new());
    }

    /// Write styled text clipped to the supplied absolute area, layering each
    /// span's style over the cells beneath (see [`Self::overlay_cell`]).
    /// Cells outside the written text are untouched.
    pub fn overlay_line(&mut self, area: Rect, line: &Line) {
        self.write_line_cells(area, line, true, Style::new());
    }

    /// [`Self::overlay_line`] with `fallback` behind every span's style, as
    /// if written with [`Line::with_fallback_style`] but without copying it.
    pub(crate) fn overlay_line_with_fallback(&mut self, area: Rect, line: &Line, fallback: Style) {
        self.write_line_cells(area, line, true, fallback);
    }

    fn write_line_cells(&mut self, area: Rect, line: &Line, overlay: bool, fallback: Style) {
        let clip = self.area.intersection(area);
        if clip.is_empty() {
            return;
        }

        let mut x = area.x;
        for span in &line.spans {
            let style = fallback.patch(span.style);
            let content = span.content.as_str();
            // Printable ASCII is one narrow grapheme per byte: skip segmentation.
            let ascii = content
                .bytes()
                .all(|byte| byte.is_ascii() && !byte.is_ascii_control());
            let mut graphemes = content.graphemes(true);
            let mut offset = 0;
            loop {
                let grapheme = if ascii {
                    let Some(grapheme) = content.get(offset..offset + 1) else {
                        break;
                    };
                    offset += 1;
                    grapheme
                } else {
                    let Some(grapheme) = graphemes.next() else {
                        break;
                    };
                    grapheme
                };
                let width = grapheme_width(grapheme);
                if width == 0 {
                    continue;
                }
                if x >= area.right() {
                    return;
                }
                if x >= clip.x && x < clip.right() {
                    let point = Point::new(x, area.y);
                    if width > clip.right().saturating_sub(x) {
                        self.put_cell(point, " ", 1, style, overlay);
                    } else {
                        // Control-containing graphemes measure 1, like their replacement.
                        self.put_cell(point, safe_symbol(grapheme), width, style, overlay);
                    }
                } else if x < clip.x && x.saturating_add(width) > clip.x {
                    self.put_cell(Point::new(clip.x, area.y), " ", 1, style, overlay);
                }
                x = x.saturating_add(width);
            }
        }
    }

    fn put_cell(&mut self, point: Point, symbol: &str, width: u16, style: Style, overlay: bool) {
        let style = if overlay {
            let Some(existing) = self.get(point) else {
                return;
            };
            existing.style.patch(style)
        } else {
            style
        };
        self.put_symbol(point, symbol, width, style);
    }

    /// Fill `area` with `style`, then write `line` with that style applied as
    /// a fallback behind every span.
    pub fn write_line_with_fallback_style(&mut self, area: Rect, line: &Line, style: Style) {
        self.fill(area, " ", style);
        self.write_line_cells(area, line, false, style);
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

    /// Model a terminal scroll region: move the full-width rows of `rows`
    /// (`rows.y..rows.bottom()`) by `shift` rows toward row 0 (negative moves
    /// them away from row 0) and blank the rows exposed at the other end.
    ///
    /// Rows outside the band are unchanged. A shift of at least the band
    /// height blanks the whole band. Columns of `rows` are ignored: terminal
    /// scroll regions always move complete rows.
    pub(crate) fn shift_rows(&mut self, rows: Rect, shift: i64) {
        let top = rows.y.max(self.area.y);
        let bottom = rows.bottom().min(self.area.bottom());
        if top >= bottom || shift == 0 {
            return;
        }
        let width = usize::from(self.area.width);
        let start = usize::from(top - self.area.y) * width;
        let end = usize::from(bottom - self.area.y) * width;
        let band = &mut self.cells[start..end];
        let moved = usize::try_from(shift.unsigned_abs())
            .unwrap_or(usize::MAX)
            .saturating_mul(width)
            .min(band.len());
        if shift > 0 {
            band.rotate_left(moved);
            let blank = band.len() - moved;
            band[blank..].fill(Cell::default());
        } else {
            band.rotate_right(moved);
            band[..moved].fill(Cell::default());
        }
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

// Ordinary cells are text, not terminal protocol. Replace a control-containing
// symbol with one visible cell; callers wanting ANSI styles must parse first.
const REPLACEMENT: &str = "\u{fffd}";

fn is_safe_symbol(symbol: &str) -> bool {
    match symbol.as_bytes() {
        [byte] => !byte.is_ascii_control(),
        _ => !symbol.chars().any(char::is_control),
    }
}

fn safe_symbol(symbol: &str) -> &str {
    if is_safe_symbol(symbol) {
        symbol
    } else {
        REPLACEMENT
    }
}

/// Cells [`Buffer::write_line`] advances for one grapheme: 0 for zero-width
/// graphemes (which paint nothing), 1 for narrow and control-containing ones
/// (painted as their replacement), 2 for wide ones.
pub(crate) fn grapheme_width(grapheme: &str) -> u16 {
    // One byte is one ASCII character: printable ones are narrow, and
    // controls measure like their replacement character.
    if grapheme.len() == 1 || grapheme.chars().any(char::is_control) {
        return 1;
    }
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
    fn reset_recycles_a_painted_buffer_into_a_blank_one() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        buffer.fill(buffer.area(), "界", Style::new().bg(Color::Red));
        buffer.set_cell(Point::new(5, 1), "x", Style::new());
        for area in [
            Rect::new(0, 0, 6, 2),
            Rect::new(2, 1, 3, 4),
            Rect::new(0, 0, 1, 1),
        ] {
            let mut recycled = buffer.clone();
            recycled.reset(area);
            assert_eq!(recycled, Buffer::empty(area), "{area:?}");
        }
    }

    #[test]
    fn clone_from_copies_into_existing_storage() {
        let mut source = Buffer::empty(Rect::new(0, 0, 4, 1));
        source.write_line(source.area(), &Line::raw("a界b"));
        let mut target = Buffer::empty(Rect::new(1, 1, 8, 3));
        target.fill(target.area(), "z", Style::new());
        target.clone_from(&source);
        assert_eq!(target, source);
    }

    #[test]
    fn repainting_in_place_matches_a_fresh_buffer() {
        let style = Style::new().fg(Color::Blue);
        let paint = |buffer: &mut Buffer, text: &str| {
            buffer.fill(buffer.area(), " ", Style::new());
            buffer.write_line(
                buffer.area(),
                &Line::from_spans([Span::styled(text, style)]),
            );
        };
        let mut reused = Buffer::empty(Rect::new(0, 0, 8, 1));
        for text in ["界界界界", "plain", "e\u{301}x界", "\x1b!", ""] {
            paint(&mut reused, text);
            let mut fresh = Buffer::empty(reused.area());
            paint(&mut fresh, text);
            assert_eq!(reused, fresh, "{text:?}");
            reused.debug_assert_valid_wide_spans();
        }
    }

    #[test]
    fn cell_controls_are_replaced_before_width_accounting() {
        for text in ["\x1b", "\r\n", "\t", "\x07", "\u{9b}", "\x1b[2J"] {
            let mut cell = super::Cell::new(text, Style::new());
            assert_eq!(cell.symbol, "�");
            assert_eq!(cell.width(), 1);
            cell.set(text, Style::new());
            assert_eq!(cell.symbol, "�");
            let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
            buffer.set_cell(Point::new(0, 0), text, Style::new());
            assert_eq!(buffer.row_symbols(0).unwrap(), "�       ");
        }
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 1));
        buffer.write_line(buffer.area(), &Line::raw("\x1b[31mred\x1b[0m"));
        assert_eq!(buffer.row_symbols(0).unwrap(), "�[31mred�[0m        ");
    }

    #[test]
    fn clipped_fill_matches_repeated_text_rendering() {
        let style = Style::new().bg(Color::Red);
        for symbol in ["a", "界", "e\u{301}"] {
            let line = Line::from_spans([Span::styled(symbol.repeat(12), style)]);
            for buffer_left in 0..5 {
                for buffer_width in 0..7 {
                    let viewport = Rect::new(buffer_left, 2, buffer_width, 1);
                    for left in 0..5 {
                        for width in 0..7 {
                            let requested = Rect::new(left, 2, width, 1);
                            let mut filled = Buffer::empty(viewport);
                            filled.fill(viewport, "x", Style::new());
                            let mut written = filled.clone();
                            filled.fill(requested, symbol, style);
                            written.write_line(requested, &line);
                            assert_eq!(
                                filled.cells(),
                                written.cells(),
                                "symbol={symbol:?}, viewport={viewport:?}, requested={requested:?}"
                            );
                            filled.debug_assert_valid_wide_spans();
                            written.debug_assert_valid_wide_spans();
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fill_preserves_tile_alignment_when_left_clipped() {
        let style = Style::new().bg(Color::Red);
        for left in 1..5 {
            let area = Rect::new(left, 2, 4, 1);
            let mut buffer = Buffer::empty(area);
            buffer.fill(Rect::new(0, 2, 8, 1), "界", style);
            let expected = if left % 2 == 0 { "界界" } else { " 界 " };
            assert_eq!(buffer.row_symbols(2).as_deref(), Some(expected));
            assert!(buffer.cells().iter().all(|cell| cell.style == style));
            buffer.debug_assert_valid_wide_spans();
        }
    }

    #[test]
    fn fill_tiles_wide_glyphs_without_overwriting_continuations() {
        let area = Rect::new(2, 3, 7, 2);
        let mut buffer = Buffer::empty(area);
        buffer.fill(area, "x", Style::new());
        let style = Style::new().bg(Color::Red);
        buffer.fill(Rect::new(3, 3, 5, 2), "界", style);
        for y in 3..5 {
            assert_eq!(buffer.row_symbols(y).as_deref(), Some("x界界 x"));
            for x in 3..8 {
                assert_eq!(buffer.get(Point::new(x, y)).unwrap().style, style);
            }
            assert_eq!(buffer.get(Point::new(8, y)).unwrap().style, Style::new());
        }
        buffer.debug_assert_valid_wide_spans();
    }

    #[test]
    fn set_cell_blanks_wide_glyph_without_room_for_continuation() {
        let mut buffer = Buffer::empty(Rect::new(4, 2, 2, 1));
        let style = Style::new().bg(Color::Red);
        buffer.set_cell(Point::new(4, 2), "界", Style::new());
        buffer.set_cell(Point::new(5, 2), "界", style);
        assert_eq!(buffer.row_symbols(2).as_deref(), Some("  "));
        assert_eq!(buffer.get(Point::new(5, 2)).unwrap().style, style);
        assert!(!buffer.get(Point::new(5, 2)).unwrap().is_wide_leader());
        buffer.debug_assert_valid_wide_spans();
    }

    #[test]
    fn write_line_blanks_visible_half_of_left_clipped_wide_glyph() {
        let area = Rect::new(1, 0, 3, 1);
        let mut buffer = Buffer::empty(area);
        buffer.fill(area, "x", Style::new());
        let style = Style::new().bg(Color::Red);
        buffer.write_line(
            Rect::new(0, 0, 4, 1),
            &Line::from_spans([Span::styled("界a", style)]),
        );
        assert_eq!(buffer.row_symbols(0).as_deref(), Some(" ax"));
        assert_eq!(buffer.get(Point::new(1, 0)).unwrap().style, style);
        assert_eq!(buffer.get(Point::new(2, 0)).unwrap().style, style);
        assert_eq!(buffer.get(Point::new(3, 0)).unwrap().style, Style::new());
        buffer.debug_assert_valid_wide_spans();
    }

    #[test]
    fn write_line_does_not_extend_wide_glyph_past_requested_area() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        buffer.fill(area, "x", Style::new());
        let style = Style::new().bg(Color::Red);
        buffer.write_line(
            Rect::new(1, 0, 1, 1),
            &Line::from_spans([Span::styled("界", style)]),
        );
        assert_eq!(buffer.row_symbols(0).as_deref(), Some("x xx"));
        assert_eq!(buffer.get(Point::new(1, 0)).unwrap().style, style);
        assert_eq!(buffer.get(Point::new(2, 0)).unwrap().style, Style::new());
        buffer.debug_assert_valid_wide_spans();
    }

    #[test]
    fn wide_damage_expansion_closes_all_small_row_topologies() {
        let area = Rect::new(2, 3, 6, 1);
        let rows = (0_u16..32)
            .filter(|mask| mask & (mask << 1) == 0)
            .map(|mask| {
                let mut buffer = Buffer::empty(area);
                for x in 0..5 {
                    if mask & (1 << x) != 0 {
                        buffer.set_cell(Point::new(area.x + x, area.y), "界", Style::new());
                    }
                }
                buffer
            })
            .collect::<Vec<_>>();
        for current in &rows {
            for previous in &rows {
                for left in area.x..area.right() {
                    for right in left + 1..=area.right() {
                        let requested = Rect::new(left, area.y, right - left, 1);
                        let expanded = current.expand_regions_to_cell_spans(previous, &[requested]);
                        let rect = expanded[0];
                        assert!(rect.x <= left && rect.right() >= right);
                        assert_eq!(rect.intersection(area), rect);
                        for buffer in [current, previous] {
                            assert!(
                                !buffer
                                    .get(Point::new(rect.x, area.y))
                                    .unwrap()
                                    .is_wide_continuation()
                            );
                            assert!(
                                !buffer
                                    .get(Point::new(rect.right() - 1, area.y))
                                    .unwrap()
                                    .is_wide_leader()
                            );
                        }
                        assert_eq!(
                            current.expand_regions_to_cell_spans(previous, &expanded),
                            expanded
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn wide_damage_expansion_revisits_rows_and_both_buffers_until_stable() {
        let area = Rect::new(0, 0, 8, 2);
        let mut current = Buffer::empty(area);
        let mut previous = Buffer::empty(area);
        current.write_line(Rect::new(0, 0, 8, 1), &Line::raw("界界界界"));
        previous.write_line(Rect::new(0, 1, 8, 1), &Line::raw("a界界界b"));
        let expanded = current.expand_regions_to_cell_spans(&previous, &[Rect::new(3, 0, 2, 2)]);
        assert_eq!(expanded, vec![area]);
        assert_eq!(
            current.expand_regions_to_cell_spans(&previous, &expanded),
            expanded
        );
    }

    #[test]
    fn wide_damage_expansion_preserves_original_right_edge() {
        let area = Rect::new(2, 3, 8, 1);
        let mut buffer = Buffer::empty(area);
        buffer.write_line(area, &Line::raw("界ab界cd"));
        let other = Buffer::empty(area);
        for (current, previous) in [(&buffer, &other), (&other, &buffer)] {
            assert_eq!(
                current.expand_regions_to_cell_spans(previous, &[Rect::new(3, 3, 3, 1)]),
                vec![Rect::new(2, 3, 4, 1)]
            );
            assert_eq!(
                current.expand_regions_to_cell_spans(previous, &[Rect::new(3, 3, 4, 1)]),
                vec![Rect::new(2, 3, 6, 1)]
            );
        }
    }

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
