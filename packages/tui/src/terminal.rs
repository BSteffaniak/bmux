//! Terminal runtime abstraction.

use std::io::{self, Write};

use crate::ansi::{AnsiFrameDiffStats, write_ansi_frame, write_ansi_frame_diff};
use crate::buffer::Buffer;
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::hit::HitMap;
use crate::image::ImageContribution;
use crate::image_scene::{ImageScene, ImageSceneDelta};

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
    images: Vec<ImageContribution>,
    image_scene: ImageScene,
    image_delta: ImageSceneDelta,
}

impl<W: Write> Terminal<W> {
    /// Create a terminal runtime for `area`.
    #[must_use]
    pub fn new(writer: W, area: Rect) -> Self {
        Self {
            writer,
            area,
            previous: None,
            hits: HitMap::new(),
            images: Vec::new(),
            image_scene: ImageScene::default(),
            image_delta: ImageSceneDelta::default(),
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

    /// Return image lifecycle contributions registered by the last draw.
    #[must_use]
    pub fn images(&self) -> &[ImageContribution] {
        &self.images
    }

    /// Return the active image scene after the last draw.
    #[must_use]
    pub const fn image_scene(&self) -> &ImageScene {
        &self.image_scene
    }

    /// Return image additions, updates, and removals produced by the last draw.
    #[must_use]
    pub const fn image_delta(&self) -> &ImageSceneDelta {
        &self.image_delta
    }

    /// Reset retained terminal presentation state so the next draw repaints fully.
    pub fn reset(&mut self) {
        self.previous = None;
    }

    /// Resize the terminal area and force the next draw to repaint fully.
    pub fn resize(&mut self, area: Rect) {
        if self.area != area {
            self.area = area;
            self.reset();
        }
    }

    /// Draw one frame.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<DrawStats> {
        let mut buffer = Buffer::empty(self.area);
        let (cursor, hits, images) = {
            let mut frame = Frame::new(&mut buffer);
            render(&mut frame);
            (
                frame.cursor(),
                frame.hits().clone(),
                frame.images().to_vec(),
            )
        };

        let stats = if let Some(previous) = &self.previous {
            write_ansi_frame_diff(&mut self.writer, previous, &buffer, cursor)?.into()
        } else {
            write_ansi_frame(&mut self.writer, &buffer, cursor)?;
            DrawStats::full(buffer.cells().len())
        };
        self.writer.flush()?;
        self.hits = hits;
        self.image_delta = self.image_scene.reconcile(&images);
        self.images = images;
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
    use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement};
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

    #[test]
    fn terminal_exposes_only_the_latest_frame_image_contributions() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 1, 1));
        terminal
            .draw(|frame| {
                frame.push_image(ImageContribution::Remove(ImageKey::new("old")));
            })
            .unwrap();
        assert_eq!(terminal.images().len(), 1);

        terminal.draw(|_| {}).unwrap();

        assert!(terminal.images().is_empty());
    }

    #[test]
    fn terminal_reconciles_frame_images_after_rendering() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let placement = ImagePlacement {
            key: ImageKey::new("diagram"),
            payload: ImagePayload::Png {
                bytes: vec![1, 2, 3],
                width: 1,
                height: 1,
            },
            destination: Rect::new(1, 0, 2, 1),
            clip: Rect::new(0, 0, 4, 2),
            lifecycle: ImageLifecycle::Frame,
        };

        terminal
            .draw(|frame| frame.push_image(ImageContribution::Present(placement.clone())))
            .unwrap();
        assert_eq!(terminal.image_delta().upserted, [placement]);
        assert!(terminal.image_delta().removed.is_empty());
        assert_eq!(terminal.image_scene().placements().len(), 1);

        terminal.draw(|_| {}).unwrap();
        assert!(terminal.image_delta().upserted.is_empty());
        assert_eq!(terminal.image_delta().removed, [ImageKey::new("diagram")]);
        assert_eq!(terminal.image_scene().placements().len(), 0);
    }
}
