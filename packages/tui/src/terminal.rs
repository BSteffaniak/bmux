//! Terminal runtime abstraction.

use std::io::{self, Write};

use crate::ansi::{AnsiFrameDiffStats, write_ansi_frame, write_ansi_frame_diff};
use crate::buffer::Buffer;
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::hit::HitMap;

/// Statistics from one terminal draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawStats {
    /// Number of cells written by the backend.
    pub changed_cells: usize,
    /// Whether the draw used a full repaint.
    pub full_repaint: bool,
}

impl DrawStats {
    const fn full(cell_count: usize) -> Self {
        Self {
            changed_cells: cell_count,
            full_repaint: true,
        }
    }
}

impl From<AnsiFrameDiffStats> for DrawStats {
    fn from(value: AnsiFrameDiffStats) -> Self {
        Self {
            changed_cells: value.changed_cells,
            full_repaint: value.full_repaint,
        }
    }
}

/// A simple ANSI terminal runtime backed by an arbitrary writer.
///
/// `Terminal` owns the previous frame buffer so repeated draws can use
/// damage-aware flushing.
pub struct Terminal<W> {
    writer: W,
    area: Rect,
    previous: Option<Buffer>,
    hits: HitMap,
}

impl<W: Write> Terminal<W> {
    /// Create a terminal runtime for `area`.
    #[must_use]
    pub const fn new(writer: W, area: Rect) -> Self {
        Self {
            writer,
            area,
            previous: None,
            hits: HitMap::new(),
        }
    }

    /// Return the terminal area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.area
    }

    /// Return the hit map registered by the last draw.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        &self.hits
    }

    /// Resize the terminal area and force the next draw to repaint fully.
    pub fn resize(&mut self, area: Rect) {
        if self.area != area {
            self.area = area;
            self.previous = None;
        }
    }

    /// Draw one frame.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<DrawStats> {
        let mut buffer = Buffer::empty(self.area);
        let (cursor, hits) = {
            let mut frame = Frame::new(&mut buffer);
            render(&mut frame);
            (frame.cursor(), frame.hits().clone())
        };

        let stats = if let Some(previous) = &self.previous {
            write_ansi_frame_diff(&mut self.writer, previous, &buffer, cursor)?.into()
        } else {
            write_ansi_frame(&mut self.writer, &buffer, cursor)?;
            DrawStats::full(buffer.cells().len())
        };
        self.writer.flush()?;
        self.hits = hits;
        self.previous = Some(buffer);
        Ok(stats)
    }

    /// Return a reference to the backend writer.
    #[must_use]
    pub const fn writer(&self) -> &W {
        &self.writer
    }

    /// Return a mutable reference to the backend writer.
    pub const fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Consume the terminal and return the backend writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::Terminal;
    use crate::geometry::{Point, Rect};
    use crate::style::Style;

    #[test]
    fn terminal_first_draw_repaints_full_frame() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));

        let stats = terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_cell(Point::new(0, 0), "A", Style::new());
            })
            .unwrap();

        assert!(stats.full_repaint);
        assert_eq!(stats.changed_cells, 2);
        assert!(
            String::from_utf8(terminal.into_inner())
                .unwrap()
                .contains('A')
        );
    }

    #[test]
    fn terminal_second_draw_uses_diff_flush() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));
        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_cell(Point::new(0, 0), "A", Style::new());
            })
            .unwrap();

        let stats = terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_cell(Point::new(1, 0), "B", Style::new());
            })
            .unwrap();

        assert!(!stats.full_repaint);
        assert_eq!(stats.changed_cells, 2);
    }

    #[test]
    fn terminal_resize_forces_full_repaint() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 1, 1));
        terminal.draw(|_| {}).unwrap();

        terminal.resize(Rect::new(0, 0, 2, 1));
        let stats = terminal.draw(|_| {}).unwrap();

        assert!(stats.full_repaint);
        assert_eq!(stats.changed_cells, 2);
    }
}
