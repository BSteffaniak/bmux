//! Terminal runtime abstraction.

use std::io::{self, Write};

use crate::ansi::{AnsiFrameDiffStats, write_ansi_frame, write_ansi_frame_diff};
use crate::buffer::Buffer;
use crate::damage::Damage;
use crate::focus::{FocusId, FocusTrap};
use crate::geometry::Rect;
use crate::hit::HitMap;
use crate::image::ImageContribution;
use crate::image_scene::{ImageScene, ImageSceneDelta};
use crate::paint::PaintCx;
use crate::presentation::{PreparedPresentation, PresentationState};
use crate::selection::SelectionScene;

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
    state: PresentationState,
}

impl<W: Write> Terminal<W> {
    /// Create a terminal runtime for `area`.
    #[must_use]
    pub fn new(writer: W, area: Rect) -> Self {
        Self {
            writer,
            state: PresentationState::new(area),
        }
    }

    /// Return the terminal area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.state.area()
    }

    /// Return the last successfully committed cell buffer, if one exists.
    #[must_use]
    pub const fn retained_buffer(&self) -> Option<&Buffer> {
        self.state.retained_buffer()
    }

    /// Return the cursor state from the last successfully committed draw.
    #[must_use]
    pub const fn cursor(&self) -> Option<crate::frame::Cursor> {
        self.state.cursor()
    }

    /// Return the interaction scene registered by the last draw.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        self.state.hits()
    }

    /// Return the selection scene from the last successfully committed draw.
    #[must_use]
    pub const fn selection(&self) -> &SelectionScene {
        self.state.selection()
    }

    /// Return semantic regions from the last successfully committed draw.
    #[must_use]
    pub const fn semantics(&self) -> &crate::semantic::SemanticScene {
        self.state.semantics()
    }

    /// Return ordered focus state derived from the last committed scene.
    #[must_use]
    pub const fn focus(&self) -> &FocusTrap {
        self.state.focus()
    }

    /// Return the active focus target after the last committed draw.
    #[must_use]
    pub fn focused(&self) -> Option<&FocusId> {
        self.state.focus().active()
    }

    /// Set the active focus target when it exists in the committed scene.
    pub fn set_focused(&mut self, id: &FocusId) -> bool {
        self.state.set_focused(id)
    }

    /// Return image lifecycle contributions registered by the last draw.
    #[must_use]
    pub fn images(&self) -> &[ImageContribution] {
        self.state.images()
    }

    /// Return the active image scene after the last draw.
    #[must_use]
    pub const fn image_scene(&self) -> &ImageScene {
        self.state.image_scene()
    }

    /// Return image additions, updates, and removals produced by the last draw.
    #[must_use]
    pub const fn image_delta(&self) -> &ImageSceneDelta {
        self.state.image_delta()
    }

    /// Reset retained terminal presentation state so the next draw repaints fully.
    ///
    /// Committed interaction and selection scenes remain authoritative until a
    /// later successful draw replaces them. This lets consumers continue to
    /// route input against what is still visibly committed after an output
    /// failure or backend reset.
    pub fn reset(&mut self) {
        self.state.reset();
    }

    /// Resize the terminal area and force the next draw to repaint fully.
    pub fn resize(&mut self, area: Rect) {
        self.state.resize(area);
    }

    /// Draw one complete frame.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer.
    pub fn draw(&mut self, render: impl FnOnce(&mut PaintCx<'_, '_>)) -> io::Result<DrawStats> {
        self.draw_damage(Damage::Full, render)
    }

    /// Draw one complete frame and emit protocol overlays before committing it.
    ///
    /// The overlay callback receives the staged image scene and its delta after
    /// cell output has been written but before the shared output flush. The
    /// frame becomes authoritative only when both cell and overlay output
    /// succeed. This allows image-capable presenters to preserve the same
    /// commit boundary as text, focus, hit, cursor, and selection metadata.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer or overlay callback.
    pub fn draw_with_overlay(
        &mut self,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        self.draw_damage_with_overlay(Damage::Full, render, overlay)
    }

    /// Draw a frame using process-local retained presentation state outside `damage`.
    ///
    /// Region damage is valid only after a successful complete presentation. If no retained
    /// frame exists, the terminal safely promotes the draw to a complete presentation. Rendering
    /// and metadata are staged and become authoritative only after terminal output flushes. A
    /// regional render that does not emit cursor or focus-scope metadata retains the previously
    /// committed values; emitting either value replaces it.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer. Failed output does not advance the
    /// retained buffer, hit map, selection scene, cursor, or image scene.
    pub fn draw_damage(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> io::Result<DrawStats> {
        self.draw_damage_with_overlay(damage, render, |_, _, _| Ok(()))
    }

    /// Draw a frame and emit protocol overlays before committing retained state.
    ///
    /// This has the same regional-retention semantics as [`Self::draw_damage`].
    /// The overlay callback observes the complete staged image scene and delta,
    /// and writes through the terminal's writer before its single flush.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer or overlay callback.
    pub fn draw_damage_with_overlay(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        self.draw_damage_with_output_hooks(damage, render, |_| Ok(false), overlay)
    }

    /// Draw with a pre-paint output hook and a post-paint overlay in one transaction.
    /// A pre-paint hook returning `true` invalidates the backend's retained cells
    /// and forces complete cell output without changing committed scene authority.
    ///
    /// # Errors
    /// Returns output or hook errors; retained state commits only after flush.
    pub fn draw_damage_with_output_hooks(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        before: impl FnOnce(&mut W) -> io::Result<bool>,
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        self.draw_damage_with_prepared_output_hooks(damage, render, |_| Ok(()), before, overlay)
    }

    /// Configure a prepared snapshot before any output in the same transaction.
    /// Callers stage scopes and validate their admission budget here. Rejection
    /// writes nothing; output failure preserves all committed scene metadata.
    ///
    /// # Errors
    /// Returns configuration, output, or hook errors without committing staging.
    pub fn draw_damage_with_prepared_output_hooks(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        configure: impl FnOnce(&mut PreparedPresentation) -> io::Result<()>,
        before: impl FnOnce(&mut W) -> io::Result<bool>,
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        let mut prepared = self.state.prepare(damage, render);
        if let Some(prepared) = &mut prepared {
            configure(prepared)?;
        }
        let writer = &mut self.writer;
        self.state
            .present_prepared(prepared, |prepared, previous| {
                writer.write_all(b"\x1b[?2026h")?;
                let output: io::Result<DrawStats> = (|| {
                    let repaint = before(writer)?;
                    let stats = if let Some(previous) = previous.filter(|_| !repaint) {
                        write_ansi_frame_diff(
                            writer,
                            previous,
                            prepared.buffer(),
                            prepared.cursor(),
                        )?
                        .into()
                    } else {
                        write_ansi_frame(writer, prepared.buffer(), prepared.cursor())?;
                        DrawStats::full(prepared.buffer().cells().len())
                    };
                    overlay(writer, prepared.image_scene(), prepared.image_delta())?;
                    Ok(stats)
                })();
                let end_result = writer.write_all(b"\x1b[?2026l");
                let stats = output?;
                end_result?;
                writer.flush()?;
                Ok(stats)
            })
            .map(|stats| {
                stats.unwrap_or(DrawStats {
                    changed_cells: 0,
                    full_repaint: false,
                })
            })
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
    use crate::paint::{LocalRect, PaintCx};
    use crate::selection::{SelectionFragment, SelectionScope};
    use crate::semantic::SemanticRegion;
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
    fn flush_failure_preserves_scene_after_successful_cell_and_overlay_writes() {
        struct FlushFailure {
            bytes: Vec<u8>,
            fail_flush: bool,
        }
        impl Write for FlushFailure {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                if self.fail_flush {
                    Err(io::Error::other("flush failed"))
                } else {
                    Ok(())
                }
            }
        }
        let mut terminal = Terminal::new(
            FlushFailure {
                bytes: Vec::new(),
                fail_flush: false,
            },
            Rect::new(0, 0, 2, 1),
        );
        terminal
            .draw(|cx| {
                cx.fill(LocalRect::new(0, 0, 2, 1), "A", Style::new());
                cx.push_hit(HitRegion::new("committed", cx.clip()).focusable(true));
            })
            .unwrap();
        terminal.writer_mut().fail_flush = true;
        let mut overlay_ran = false;
        let failed = terminal.draw_with_overlay(
            |cx| {
                cx.fill(LocalRect::new(0, 0, 2, 1), "B", Style::new());
                cx.push_hit(HitRegion::new("speculative", cx.clip()).focusable(true));
            },
            |writer, _, _| {
                overlay_ran = true;
                writer.write_all(b"overlay")
            },
        );
        assert!(failed.is_err());
        assert!(overlay_ran);
        assert_eq!(terminal.hits().regions()[0].id.as_str(), "committed");
        assert_eq!(terminal.focused().unwrap().as_str(), "committed");
        assert!(terminal.retained_buffer().is_none());
        terminal.writer_mut().fail_flush = false;
        assert!(terminal.draw(|_| {}).unwrap().full_repaint);
    }

    #[test]
    fn sparse_output_short_writes_and_failures_repair_transactionally() {
        struct ShortWriter {
            bytes: Vec<u8>,
            remaining: Option<usize>,
            fail_flush: bool,
        }
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.remaining == Some(0) {
                    self.remaining = None;
                    return Err(io::Error::other("injected prefix failure"));
                }
                let count = bytes.len().min(3).min(self.remaining.unwrap_or(usize::MAX));
                self.bytes.extend_from_slice(&bytes[..count]);
                if let Some(remaining) = &mut self.remaining {
                    *remaining -= count;
                }
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                if self.fail_flush {
                    Err(io::Error::other("injected flush failure"))
                } else {
                    Ok(())
                }
            }
        }
        let paint = |cx: &mut PaintCx<'_, '_>| {
            cx.fill(LocalRect::new(0, 0, 1, 1), "A", Style::new());
            cx.fill(LocalRect::new(79, 0, 1, 1), "Z", Style::new());
            cx.push_hit(HitRegion::new("new", cx.clip()).focusable(true));
            cx.set_cursor(Point::new(79, 0), true);
        };
        for (budget, fail_flush) in [
            (Some(0), false),
            (Some(1), false),
            (Some(6), false),
            (Some(12), false),
            (Some(20), false),
            (Some(30), false),
            (None, true),
            (None, false),
        ] {
            let mut terminal = Terminal::new(
                ShortWriter {
                    bytes: Vec::new(),
                    remaining: None,
                    fail_flush: false,
                },
                Rect::new(0, 0, 80, 1),
            );
            terminal
                .draw(|cx| {
                    cx.push_hit(HitRegion::new("old", cx.clip()).focusable(true));
                    cx.set_cursor(Point::new(0, 0), true);
                })
                .unwrap();
            terminal.writer_mut().bytes.clear();
            terminal.writer_mut().remaining = budget;
            terminal.writer_mut().fail_flush = fail_flush;
            let result = terminal.draw(paint);
            if budget.is_some() || fail_flush {
                assert!(result.is_err());
                assert!(terminal.retained_buffer().is_none());
                assert_eq!(terminal.hits().regions()[0].id.as_str(), "old");
                assert_eq!(terminal.focused().unwrap().as_str(), "old");
                assert_eq!(terminal.cursor(), Some(Cursor::visible(Point::new(0, 0))));
                terminal.writer_mut().remaining = None;
                terminal.writer_mut().fail_flush = false;
                terminal.writer_mut().bytes.clear();
                assert!(terminal.draw(paint).unwrap().full_repaint);
            } else {
                assert_eq!(result.unwrap().changed_cells, 2);
                assert!(terminal.writer().bytes.len() < 80);
            }
            assert_eq!(terminal.hits().regions()[0].id.as_str(), "new");
            assert_eq!(terminal.cursor(), Some(Cursor::visible(Point::new(79, 0))));
            let buffer = terminal.retained_buffer().unwrap();
            assert_eq!(buffer.get(Point::new(0, 0)).unwrap().symbol, "A");
            assert_eq!(buffer.get(Point::new(79, 0)).unwrap().symbol, "Z");
            terminal.resize(Rect::new(0, 0, 81, 1));
            assert!(terminal.draw(paint).unwrap().full_repaint);
        }
    }

    #[test]
    fn failed_overlay_does_not_commit_image_or_interaction_scene() {
        let mut terminal = Terminal::new(
            FailingWriter {
                bytes: Vec::new(),
                fail: false,
            },
            Rect::new(0, 0, 4, 1),
        );
        terminal
            .draw(|frame| {
                frame.push_hit(HitRegion::new("committed", frame.clip()));
                frame.push_image(ImageContribution::Present(ImagePlacement {
                    fit: crate::image::ImageFit::Stretch,
                    allocation: None,
                    key: ImageKey::new("committed"),
                    payload: ImagePayload::Pixels {
                        bytes: vec![1, 2, 3, 4],
                        width: 1,
                        height: 1,
                        format: crate::image::ImagePixelFormat::Rgba8,
                    },
                    destination: frame.clip(),
                    clip: frame.clip(),
                    lifecycle: ImageLifecycle::Frame,
                }));
            })
            .expect("initial frame commits");

        let error = terminal.draw_with_overlay(
            |frame| {
                frame.push_hit(HitRegion::new("speculative", frame.clip()));
                frame.push_image(ImageContribution::Present(ImagePlacement {
                    fit: crate::image::ImageFit::Stretch,
                    allocation: None,
                    key: ImageKey::new("speculative"),
                    payload: ImagePayload::Pixels {
                        bytes: vec![5, 6, 7, 8],
                        width: 1,
                        height: 1,
                        format: crate::image::ImagePixelFormat::Rgba8,
                    },
                    destination: frame.clip(),
                    clip: frame.clip(),
                    lifecycle: ImageLifecycle::Frame,
                }));
            },
            |_, _, _| Err(io::Error::other("injected overlay failure")),
        );

        assert!(error.is_err());
        assert_eq!(terminal.hits().regions()[0].id.as_str(), "committed");
        assert!(
            terminal
                .image_scene()
                .get(&ImageKey::new("committed"))
                .is_some()
        );
        assert!(
            terminal
                .image_scene()
                .get(&ImageKey::new("speculative"))
                .is_none()
        );
        assert!(terminal.retained_buffer().is_none());
        let output = String::from_utf8(terminal.writer().bytes.clone()).expect("ANSI output");
        assert!(output.ends_with("\u{1b}[?2026l"));
    }

    #[test]
    fn failed_draw_does_not_commit_speculative_selection_scene() {
        let mut terminal = Terminal::new(
            FailingWriter {
                bytes: Vec::new(),
                fail: false,
            },
            Rect::new(0, 0, 4, 1),
        );
        terminal
            .draw(|frame| {
                frame.push_selection_scope(SelectionScope::new("committed", frame.clip()));
                frame.push_selection_fragment(SelectionFragment::new(
                    "committed",
                    "content",
                    Rect::new(0, 0, 1, 1),
                    0,
                    0..1,
                ));
            })
            .expect("initial selection scene commits");

        terminal.writer_mut().fail = true;
        assert!(
            terminal
                .draw(|frame| {
                    frame.push_selection_scope(SelectionScope::new("speculative", frame.clip()));
                })
                .is_err()
        );

        assert_eq!(terminal.selection().scopes()[0].id.as_str(), "committed");
    }

    #[test]
    fn terminal_diff_scrolls_zwj_text_without_independent_continuation_writes() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 8, 2));
        terminal
            .draw(|frame| {
                frame.write_line(LocalRect::new(0, 0, 8, 1), &crate::text::Line::raw("A👩🏽‍💻│"));
                frame.write_line(
                    LocalRect::new(0, 1, 8, 1),
                    &crate::text::Line::raw("bottom│"),
                );
            })
            .unwrap();
        let first_len = terminal.writer().len();

        terminal
            .draw(|frame| {
                frame.write_line(
                    LocalRect::new(0, 0, 8, 1),
                    &crate::text::Line::raw("bottom│"),
                );
                frame.write_line(LocalRect::new(0, 1, 8, 1), &crate::text::Line::raw("B👩🏽‍💻│"));
            })
            .unwrap();
        let output = String::from_utf8(terminal.writer()[first_len..].to_vec()).unwrap();

        assert!(output.contains("👩🏽‍💻"));
        assert!(!output.contains("\x1b[2;3H "));
    }

    #[test]
    fn terminal_first_draw_repaints_full_frame() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));

        let stats = terminal
            .draw(|frame| {
                frame.write_line(LocalRect::new(0, 0, 1, 1), &crate::text::Line::raw("A"));
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
                frame.write_line(LocalRect::new(0, 0, 1, 1), &crate::text::Line::raw("A"));
            })
            .unwrap();

        let stats = terminal
            .draw(|frame| {
                frame.write_line(LocalRect::new(1, 0, 1, 1), &crate::text::Line::raw("B"));
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
        let scope = crate::hit::HitId::new("modal");
        terminal
            .draw(|frame| {
                frame.fill(LocalRect::new(0, 0, 4, 2), "A", Style::new());
                frame.push_hit(HitRegion::new("left", Rect::new(0, 0, 2, 2)));
                frame.push_hit(HitRegion::new("right", Rect::new(2, 0, 2, 2)));
                frame.set_focus_scope(Some(scope.clone()));
                frame.set_cursor(Point::new(3, 1), true);
            })
            .unwrap();

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 2, 2)]), |frame| {
                frame.fill(LocalRect::new(0, 0, 2, 2), "B", Style::new());
                frame.push_hit(HitRegion::new("new-left", Rect::new(0, 0, 2, 2)));
            })
            .unwrap();

        assert_eq!(stats.changed_cells, 4);
        assert!(!stats.full_repaint);
        assert_eq!(terminal.cursor(), Some(Cursor::visible(Point::new(3, 1))));
        assert_eq!(terminal.focus().active_scope(), Some(&scope));
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
                frame.push_hit(HitRegion::new("committed", frame.clip()));
                frame.push_selection_scope(SelectionScope::new("committed", frame.clip()));
                frame.push_selection_fragment(SelectionFragment::new(
                    "committed",
                    "committed-content",
                    frame.clip(),
                    0,
                    0..1,
                ));
                frame.push_semantic(SemanticRegion::new("committed", frame.clip(), "content"));
                frame.set_cursor(Point::new(1, 0), true);
            })
            .unwrap();
        terminal.writer_mut().fail = true;

        let result = terminal.draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |frame| {
            frame.fill(frame.area(), "B", Style::new());
            frame.push_hit(HitRegion::new("uncommitted", frame.clip()));
            frame.push_selection_scope(SelectionScope::new("uncommitted", frame.clip()));
            frame.push_selection_fragment(SelectionFragment::new(
                "uncommitted",
                "uncommitted-content",
                frame.clip(),
                0,
                0..1,
            ));
            frame.push_semantic(SemanticRegion::new("uncommitted", frame.clip(), "content"));
            frame.set_cursor(Point::new(0, 0), false);
        });

        assert!(result.is_err());
        assert_eq!(terminal.hits().regions()[0].id.as_str(), "committed");
        assert_eq!(terminal.selection().scopes()[0].id.as_str(), "committed");
        assert_eq!(
            terminal.selection().fragments()[0].content_id.as_str(),
            "committed-content"
        );
        assert_eq!(terminal.semantics().regions()[0].id.as_str(), "committed");
        assert_eq!(terminal.cursor(), Some(Cursor::visible(Point::new(1, 0))));
        terminal.writer_mut().fail = false;
        let recovered = terminal.draw(|frame| frame.fill(frame.area(), "C", Style::new()));
        assert!(recovered.expect("full recovery draw succeeds").full_repaint);
    }

    #[test]
    fn region_draw_matches_complete_frame_for_declared_change() {
        fn first(frame: &mut PaintCx<'_, '_>) {
            frame.fill(frame.area(), "A", Style::new());
            frame.push_hit(HitRegion::new("left", Rect::new(0, 0, 2, 1)));
            frame.push_hit(HitRegion::new("right", Rect::new(2, 0, 2, 1)));
            frame.push_selection_scope(SelectionScope::new("left-scope", Rect::new(0, 0, 2, 1)));
            frame.push_selection_scope(SelectionScope::new("right-scope", Rect::new(2, 0, 2, 1)));
            frame.push_semantic(SemanticRegion::new(
                "left-semantic",
                Rect::new(0, 0, 2, 1),
                "content",
            ));
            frame.push_semantic(SemanticRegion::new(
                "right-semantic",
                Rect::new(2, 0, 2, 1),
                "content",
            ));
            frame.set_cursor(Point::new(3, 0), true);
        }
        fn second(frame: &mut PaintCx<'_, '_>) {
            frame.fill(LocalRect::new(0, 0, 2, 1), "B", Style::new());
            frame.fill(LocalRect::new(2, 0, 2, 1), "A", Style::new());
            frame.push_hit(HitRegion::new("new-left", Rect::new(0, 0, 2, 1)));
            frame.push_hit(HitRegion::new("right", Rect::new(2, 0, 2, 1)));
            frame.push_selection_scope(
                SelectionScope::new("left-scope", Rect::new(0, 0, 2, 1)).revision(1),
            );
            frame.push_selection_scope(SelectionScope::new("right-scope", Rect::new(2, 0, 2, 1)));
            frame.push_semantic(SemanticRegion::new(
                "left-semantic",
                Rect::new(0, 0, 2, 1),
                "new-content",
            ));
            frame.push_semantic(SemanticRegion::new(
                "right-semantic",
                Rect::new(2, 0, 2, 1),
                "content",
            ));
            frame.set_cursor(Point::new(3, 0), true);
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
        assert_eq!(partial.selection(), complete.selection());
        assert_eq!(
            partial.semantics().regions().len(),
            complete.semantics().regions().len()
        );
        for expected in complete.semantics().regions() {
            assert_eq!(
                partial
                    .semantics()
                    .regions()
                    .iter()
                    .find(|region| region.id == expected.id),
                Some(expected)
            );
        }
        assert_eq!(partial.image_scene(), complete.image_scene());
    }

    #[test]
    fn region_draw_without_retained_frame_promotes_to_full() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |frame| {
                frame.fill(LocalRect::new(0, 0, 1, 1), "X", Style::new());
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
            fit: crate::image::ImageFit::Stretch,
            allocation: None,
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
