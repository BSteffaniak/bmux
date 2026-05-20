//! Frame render context.

use crate::buffer::Buffer;
use crate::geometry::{Point, Rect};
use crate::hit::{HitMap, HitRegion};
use crate::style::Style;
use crate::text::Line;

/// Cursor requested by a rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Cursor position.
    pub position: Point,
    /// Whether the cursor should be visible.
    pub visible: bool,
}

impl Cursor {
    /// Create a visible cursor at `position`.
    #[must_use]
    pub const fn visible(position: Point) -> Self {
        Self {
            position,
            visible: true,
        }
    }

    /// Create a hidden cursor at `position`.
    #[must_use]
    pub const fn hidden(position: Point) -> Self {
        Self {
            position,
            visible: false,
        }
    }
}

/// Mutable render context for a single frame.
pub struct Frame<'buffer> {
    buffer: &'buffer mut Buffer,
    cursor: Option<Cursor>,
    hits: HitMap,
}

impl<'buffer> Frame<'buffer> {
    /// Create a frame that renders into `buffer`.
    pub const fn new(buffer: &'buffer mut Buffer) -> Self {
        Self {
            buffer,
            cursor: None,
            hits: HitMap::new(),
        }
    }

    /// Return the frame area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.buffer.area()
    }

    /// Return the current cursor request.
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }

    /// Request a cursor state for this frame.
    pub const fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = Some(cursor);
    }

    /// Return an immutable view of the backing buffer.
    #[must_use]
    pub const fn buffer(&self) -> &Buffer {
        self.buffer
    }

    /// Return a mutable view of the backing buffer.
    pub const fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }

    /// Return registered hit regions.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        &self.hits
    }

    /// Add a hit-test region for this frame.
    pub fn push_hit(&mut self, region: HitRegion) {
        self.hits.push(region);
    }

    /// Fill a rectangular area with a symbol and style.
    pub fn fill(&mut self, area: Rect, symbol: &str, style: Style) {
        self.buffer.fill(area, symbol, style);
    }

    /// Write a styled line into a rectangular area.
    pub fn write_line(&mut self, area: Rect, line: &Line) {
        self.buffer.write_line(area, line);
    }
}

#[cfg(test)]
mod tests {
    use super::{Cursor, Frame};
    use crate::buffer::Buffer;
    use crate::geometry::{Point, Rect};

    #[test]
    fn frame_tracks_cursor_request() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);

        frame.set_cursor(Cursor::visible(Point::new(1, 1)));

        assert_eq!(frame.cursor(), Some(Cursor::visible(Point::new(1, 1))));
    }
}
