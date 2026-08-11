//! Terminal runtime abstraction.

use std::io::{self, Write};

use crate::ansi::{AnsiFrameDiffStats, write_ansi_frame, write_ansi_frame_diff};
use crate::buffer::Buffer;
use crate::damage::Damage;
use crate::focus::{FocusId, FocusScopeId, FocusTrap};
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
    focus: FocusTrap,
    focus_scope: Option<FocusScopeId>,
    images: Vec<ImageContribution>,
    image_scene: ImageScene,
    image_delta: ImageSceneDelta,
    cursor: Option<crate::frame::Cursor>,
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
            focus: FocusTrap::new(),
            focus_scope: None,
            images: Vec::new(),
            image_scene: ImageScene::default(),
            image_delta: ImageSceneDelta::default(),
            cursor: None,
        }
    }

    /// Return the terminal area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.area
    }

    /// Return the last successfully committed cell buffer, if one exists.
    #[must_use]
    pub const fn retained_buffer(&self) -> Option<&Buffer> {
        self.previous.as_ref()
    }

    /// Return the cursor state from the last successfully committed draw.
    #[must_use]
    pub const fn cursor(&self) -> Option<crate::frame::Cursor> {
        self.cursor
    }

    /// Return the interaction scene registered by the last draw.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        &self.hits
    }

    /// Return ordered focus state derived from the last committed scene.
    #[must_use]
    pub const fn focus(&self) -> &FocusTrap {
        &self.focus
    }

    /// Return the active focus target after the last committed draw.
    #[must_use]
    pub fn focused(&self) -> Option<&FocusId> {
        self.focus.active()
    }

    /// Set the active focus target when it exists in the committed scene.
    pub fn set_focused(&mut self, id: &FocusId) -> bool {
        self.focus.set_active(id)
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

    /// Draw one complete frame.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<DrawStats> {
        self.draw_damage(Damage::Full, render)
    }

    /// Draw a frame using process-local retained presentation state outside `damage`.
    ///
    /// Region damage is valid only after a successful complete presentation. If no retained
    /// frame exists, the terminal safely promotes the draw to a complete presentation. Rendering
    /// and metadata are staged and become authoritative only after terminal output flushes.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer. Failed output does not advance the
    /// retained buffer, hit map, cursor, or image scene.
    pub fn draw_damage(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut Frame<'_>),
    ) -> io::Result<DrawStats> {
        let damage = if self.previous.is_none() {
            Damage::Full
        } else {
            damage
        };
        if damage.is_none() {
            return Ok(DrawStats {
                changed_cells: 0,
                full_repaint: false,
            });
        }
        let regions = damage.retained_regions();
        let mut buffer = Buffer::empty(self.area);
        let (cursor, hits, focus_scope, images) = {
            let mut frame = Frame::new(&mut buffer);
            render(&mut frame);
            let mut hits = if matches!(damage, Damage::Regions(_)) {
                let mut retained = self.hits.clone();
                for region in regions {
                    retained.retain_outside(*region);
                }
                retained
            } else {
                HitMap::new()
            };
            for hit in frame.hits().regions() {
                if !matches!(damage, Damage::Regions(_))
                    || regions
                        .iter()
                        .any(|region| !hit.area.intersection(*region).is_empty())
                {
                    hits.push(hit.clone());
                }
            }
            let mut images = if matches!(damage, Damage::Regions(_)) {
                self.image_scene.contributions_outside(regions)
            } else {
                Vec::new()
            };
            images.extend_from_slice(frame.images());
            let cursor = frame.cursor();
            let focus_scope = frame.focus_scope().cloned();
            (cursor, hits, focus_scope, images)
        };
        if let (Some(previous), Damage::Regions(_)) = (&self.previous, &damage) {
            buffer.restore_outside(previous, regions);
        }

        let output = (|| {
            let stats = if let Some(previous) = &self.previous {
                write_ansi_frame_diff(&mut self.writer, previous, &buffer, cursor)?.into()
            } else {
                write_ansi_frame(&mut self.writer, &buffer, cursor)?;
                DrawStats::full(buffer.cells().len())
            };
            self.writer.flush()?;
            Ok(stats)
        })();
        let stats = match output {
            Ok(stats) => stats,
            Err(error) => {
                self.reset();
                return Err(error);
            }
        };
        let mut image_scene = self.image_scene.clone();
        let image_delta = image_scene.reconcile(&images);
        let previous_focus = self.focus.active().cloned();
        self.hits = hits;
        self.focus_scope = focus_scope;
        self.focus = FocusTrap::from_hits(
            &self.hits,
            self.focus_scope.as_ref(),
            previous_focus.as_ref(),
        );
        self.image_scene = image_scene;
        self.image_delta = image_delta;
        self.images = images;
        self.cursor = cursor;
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
    use crate::damage::Damage;
    use crate::frame::Cursor;
    use crate::geometry::{Point, Rect};
    use crate::hit::HitRegion;
    use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement};
    use crate::style::Style;
    use std::io::{self, Write};

    struct FailingWriter {
        bytes: Vec<u8>,
        fail: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail {
                Err(io::Error::other("injected write failure"))
            } else {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail {
                Err(io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }
    }

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
    fn region_draw_retains_undamaged_cells_and_metadata() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        terminal
            .draw(|frame| {
                frame.fill(Rect::new(0, 0, 4, 2), "A", Style::new());
                frame.push_hit(HitRegion::new("left", Rect::new(0, 0, 2, 2)));
                frame.push_hit(HitRegion::new("right", Rect::new(2, 0, 2, 2)));
                frame.set_cursor(Cursor::visible(Point::new(3, 1)));
            })
            .unwrap();

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 2, 2)]), |frame| {
                frame.fill(Rect::new(0, 0, 2, 2), "B", Style::new());
                frame.push_hit(HitRegion::new("new-left", Rect::new(0, 0, 2, 2)));
            })
            .unwrap();

        assert_eq!(stats.changed_cells, 4);
        assert!(!stats.full_repaint);
        assert_eq!(
            terminal
                .hits()
                .regions()
                .iter()
                .map(|region| region.id.as_str())
                .collect::<Vec<_>>(),
            ["right", "new-left"]
        );
    }

    #[test]
    fn region_draw_discards_writes_outside_declared_damage() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 4, 1));
        terminal
            .draw(|frame| frame.fill(frame.area(), "A", Style::new()))
            .unwrap();
        let before = terminal.writer().len();

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |frame| {
                frame.fill(frame.area(), "Z", Style::new());
            })
            .unwrap();

        assert_eq!(stats.changed_cells, 1);
        assert_eq!(
            String::from_utf8_lossy(&terminal.writer()[before..])
                .matches('Z')
                .count(),
            1
        );
    }

    #[test]
    fn region_draw_clears_removed_content_inside_damage() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));
        terminal
            .draw(|frame| frame.fill(frame.area(), "A", Style::new()))
            .unwrap();

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |_| {})
            .unwrap();

        assert_eq!(stats.changed_cells, 1);
    }

    #[test]
    fn failed_region_draw_does_not_commit_metadata() {
        let writer = FailingWriter {
            bytes: Vec::new(),
            fail: false,
        };
        let mut terminal = Terminal::new(writer, Rect::new(0, 0, 2, 1));
        terminal
            .draw(|frame| {
                frame.fill(frame.area(), "A", Style::new());
                frame.push_hit(HitRegion::new("committed", frame.area()));
                frame.set_cursor(Cursor::visible(Point::new(1, 0)));
            })
            .unwrap();
        terminal.writer_mut().fail = true;

        let result = terminal.draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |frame| {
            frame.fill(frame.area(), "B", Style::new());
            frame.push_hit(HitRegion::new("uncommitted", frame.area()));
            frame.set_cursor(Cursor::hidden(Point::new(0, 0)));
        });

        assert!(result.is_err());
        assert_eq!(terminal.hits().regions()[0].id.as_str(), "committed");
        terminal.writer_mut().fail = false;
        let recovered = terminal.draw(|frame| frame.fill(frame.area(), "C", Style::new()));
        assert!(recovered.expect("full recovery draw succeeds").full_repaint);
    }

    #[test]
    fn region_draw_matches_complete_frame_for_declared_change() {
        fn first(frame: &mut crate::frame::Frame<'_>) {
            frame.fill(frame.area(), "A", Style::new());
            frame.push_hit(HitRegion::new("left", Rect::new(0, 0, 2, 1)));
            frame.push_hit(HitRegion::new("right", Rect::new(2, 0, 2, 1)));
            frame.set_cursor(Cursor::visible(Point::new(3, 0)));
        }
        fn second(frame: &mut crate::frame::Frame<'_>) {
            frame.fill(Rect::new(0, 0, 2, 1), "B", Style::new());
            frame.fill(Rect::new(2, 0, 2, 1), "A", Style::new());
            frame.push_hit(HitRegion::new("new-left", Rect::new(0, 0, 2, 1)));
            frame.push_hit(HitRegion::new("right", Rect::new(2, 0, 2, 1)));
            frame.set_cursor(Cursor::visible(Point::new(3, 0)));
        }

        let area = Rect::new(0, 0, 4, 1);
        let mut complete = Terminal::new(Vec::new(), area);
        complete.draw(first).unwrap();
        complete.draw(second).unwrap();
        let mut partial = Terminal::new(Vec::new(), area);
        partial.draw(first).unwrap();
        partial
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 2, 1)]), second)
            .unwrap();

        assert_eq!(partial.retained_buffer(), complete.retained_buffer());
        for point in [Point::new(0, 0), Point::new(3, 0)] {
            assert_eq!(
                partial
                    .hits()
                    .hit_test(point)
                    .map(|hit| hit.id().as_str().to_owned()),
                complete
                    .hits()
                    .hit_test(point)
                    .map(|hit| hit.id().as_str().to_owned())
            );
        }
        assert_eq!(partial.cursor(), complete.cursor());
        assert_eq!(partial.image_scene(), complete.image_scene());
    }

    #[test]
    fn region_draw_without_retained_frame_promotes_to_full() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |frame| {
                frame.fill(Rect::new(0, 0, 1, 1), "X", Style::new());
            })
            .unwrap();

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
