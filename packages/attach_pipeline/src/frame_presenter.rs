//! Frames presented through a `bmux_tui` terminal from what the attach renderer captures.
//!
//! The renderer captures what it draws (the same paint operations an observer paints) instead of
//! encoding it, and a [`FramePresenter`] paints them over the frame it last presented and encodes
//! the difference: styles, cursor, synchronized output and row moves come from the terminal, not
//! the renderer. Graphics protocols keep their own encoders: their bytes are written through the
//! terminal's output hooks, removals before the frame's cells and placements after them.

use std::io::Write;

use bmux_tui::geometry::{Point, Rect};
use bmux_tui::pointer::PointerShapeCapability;
use bmux_tui::terminal::{ScrollRegionAcceleration, Terminal};

use crate::AttachCursorState;
use crate::projection::{GraphicsOutput, PaintOperation, ProjectionFailure};

/// Presents captured frames on a host through a `bmux_tui` terminal.
pub struct FramePresenter {
    terminal: Terminal<Vec<u8>>,
    /// The cursor the last presented frame left on the host.
    cursor: Option<AttachCursorState>,
    /// A frame was presented since the presenter was made or last forgot: the next frame can be
    /// painted over it rather than over a blank screen.
    shown: bool,
}

impl Default for FramePresenter {
    fn default() -> Self {
        Self::new()
    }
}

/// Graphics protocol bytes of a frame: written before its cells (clearing placements, erasing
/// raster images) and after them (placing images), inside the same synchronized frame.
#[derive(Clone, Copy, Default)]
pub struct Graphics<'bytes> {
    pub before: &'bytes [u8],
    pub after: &'bytes [u8],
}

impl<'bytes> From<&'bytes GraphicsOutput> for Graphics<'bytes> {
    fn from(output: &'bytes GraphicsOutput) -> Self {
        Self {
            before: &output.before,
            after: &output.after,
        }
    }
}

/// What presenting a captured frame produced.
#[derive(Debug)]
pub enum Presented {
    /// Bytes to write to the host, the whole frame.
    Frame(Vec<u8>),
    /// Nothing changed on the host.
    Unchanged,
    /// The frame holds what a terminal frame cannot show; nothing was presented.
    Unsupported(ProjectionFailure),
}

/// A captured frame to present.
pub struct CapturedFrame<'frame> {
    pub operations: &'frame [PaintOperation],
    pub cursor: Option<AttachCursorState>,
    /// The frame starts from a blank screen, as the renderer's full frames do.
    pub full: bool,
    pub size: (u16, u16),
    /// The host has left and right margins (DECSLRM), so a narrower pane scrolls inside them.
    pub margins: bool,
    pub graphics: Graphics<'frame>,
}

impl FramePresenter {
    /// A presenter that has shown nothing yet: its first frame repaints in full.
    #[must_use]
    pub fn new() -> Self {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 0, 0));
        // Attach clients present pointer shapes themselves.
        terminal.set_pointer_capability(PointerShapeCapability::Unsupported);
        Self {
            terminal,
            cursor: None,
            shown: false,
        }
    }

    /// Select whether frames are wrapped in a synchronized update (DEC mode 2026).
    pub const fn set_synchronized_output(&mut self, enabled: bool) {
        self.terminal.set_synchronized_output(enabled);
    }

    /// Paint `frame` over the last frame presented, as the host must show it, and return the
    /// bytes that change the host to match.
    ///
    /// The frame counts as shown once its bytes are returned; when they do not reach the host,
    /// [`Self::forget`] makes the next frame repaint in full.
    pub fn present(&mut self, frame: &CapturedFrame<'_>) -> Presented {
        if let Some(failure) = unsupported(frame.operations) {
            return Presented::Unsupported(failure);
        }
        let (cols, rows) = frame.size;
        self.terminal.resize(Rect::new(0, 0, cols, rows));
        if frame.full {
            self.terminal.reset();
        }
        let graphics = frame.graphics;
        let has_graphics = !graphics.before.is_empty() || !graphics.after.is_empty();
        if frame.operations.is_empty()
            && frame.cursor == self.cursor
            && !frame.full
            && !has_graphics
        {
            return Presented::Unchanged;
        }
        self.terminal
            .set_scroll_region_acceleration(if frame.margins {
                ScrollRegionAcceleration::AutoWithMargins
            } else {
                ScrollRegionAcceleration::Auto
            });
        self.terminal.writer_mut().clear();
        let operations = frame.operations;
        let cursor = frame.cursor;
        let paint = |cx: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            // Checked above: every operation paints.
            let _ = crate::projection::paint_over(cx, operations);
            if let Some(cursor) = cursor {
                cx.set_cursor(Point::new(cursor.x, cursor.y), cursor.visible);
            }
        };
        let drawn = if has_graphics {
            self.terminal.draw_over_with_output_hooks(
                paint,
                |writer| writer.write_all(graphics.before).map(|()| false),
                |writer, _, _| {
                    if graphics.after.is_empty() {
                        return Ok(());
                    }
                    // Placing images moves the cursor: keep the frame's.
                    writer.write_all(b"\x1b7")?;
                    writer.write_all(graphics.after)?;
                    writer.write_all(b"\x1b8")
                },
            )
        } else {
            self.terminal.draw_over(paint)
        };
        if drawn.is_ok() {
            self.cursor = cursor;
            self.shown = true;
            Presented::Frame(std::mem::take(self.terminal.writer_mut()))
        } else {
            self.forget();
            Presented::Unchanged
        }
    }

    /// Forget what was presented, so the next frame repaints in full: after its bytes failed to
    /// reach the host, or after the host was changed some other way.
    pub fn forget(&mut self) {
        self.terminal.reset();
        self.cursor = None;
        self.shown = false;
    }

    /// Whether a frame was presented since the presenter was made or last forgot. Until one is,
    /// frames must be full: they are painted over a blank screen.
    #[must_use]
    pub const fn shown(&self) -> bool {
        self.shown
    }
}

/// The first operation a terminal frame cannot show, if any.
#[must_use]
pub fn unsupported(operations: &[PaintOperation]) -> Option<ProjectionFailure> {
    operations.iter().find_map(|operation| match operation {
        PaintOperation::Rejected(ProjectionFailure::Deferred { .. })
        | PaintOperation::Text { .. }
        | PaintOperation::Fill { .. }
        | PaintOperation::Scroll { .. }
        | PaintOperation::Cells { .. } => None,
        PaintOperation::Rejected(reason) => Some(reason.clone()),
        PaintOperation::Unsupported => Some(ProjectionFailure::Unsupported {
            reason: "legacy imperative capture",
        }),
        PaintOperation::Grid(_) => Some(ProjectionFailure::Unsupported {
            reason: "local grid",
        }),
        PaintOperation::Image(_) => Some(ProjectionFailure::Unsupported {
            reason: "image placement",
        }),
    })
}

/// Run `render` with its cells captured and its graphics protocol output collected, as a
/// [`FramePresenter`] presents them.
///
/// # Panics
/// Panics if called inside another capture.
pub fn capture_frame<T>(render: impl FnOnce() -> T) -> (T, Vec<PaintOperation>, GraphicsOutput) {
    let ((result, graphics), operations) = crate::projection::capture_neutral(|| {
        crate::projection::with_graphics_output(|| {
            let result = render();
            (result, crate::projection::take_graphics_output())
        })
    });
    (result, operations, graphics)
}

#[cfg(test)]
mod tests {
    use super::{CapturedFrame, FramePresenter, Graphics, Presented};
    use crate::AttachCursorState;
    use crate::projection::{PaintOperation, ProjectionFailure};
    use bmux_tui::style::Style;

    fn frame(
        operations: &[PaintOperation],
        cursor: Option<AttachCursorState>,
        full: bool,
    ) -> CapturedFrame<'_> {
        CapturedFrame {
            operations,
            cursor,
            full,
            size: (10, 2),
            margins: false,
            graphics: Graphics::default(),
        }
    }

    fn text(x: u16, y: u16, text: &str) -> PaintOperation {
        PaintOperation::Text {
            x,
            y,
            text: text.to_owned(),
            style: Style::new(),
        }
    }

    #[test]
    fn frames_write_what_changed_and_nothing_when_nothing_did() {
        let mut presenter = FramePresenter::new();
        assert!(!presenter.shown());
        let cursor = Some(AttachCursorState {
            x: 1,
            y: 1,
            visible: true,
        });
        let Presented::Frame(first) =
            presenter.present(&frame(&[text(0, 0, "hello")], cursor, true))
        else {
            panic!("the first frame is presented");
        };
        assert!(String::from_utf8_lossy(&first).contains("hello"));
        assert!(presenter.shown());
        assert!(matches!(
            presenter.present(&frame(&[], cursor, false)),
            Presented::Unchanged
        ));
        let Presented::Frame(edit) =
            presenter.present(&frame(&[text(0, 1, "world")], cursor, false))
        else {
            panic!("an edit is presented");
        };
        let edit = String::from_utf8_lossy(&edit);
        assert!(
            edit.contains("world") && !edit.contains("hello"),
            "{edit:?}"
        );
        let moved = Some(AttachCursorState {
            x: 2,
            y: 0,
            visible: true,
        });
        assert!(matches!(
            presenter.present(&frame(&[], moved, false)),
            Presented::Frame(_)
        ));
    }

    #[test]
    fn rejected_frames_present_nothing_and_forgetting_repaints() {
        let mut presenter = FramePresenter::new();
        assert!(matches!(
            presenter.present(&frame(&[text(0, 0, "kept")], None, true)),
            Presented::Frame(_)
        ));
        let rejected = [PaintOperation::Rejected(ProjectionFailure::Budget {
            resource: "operations",
            used: 2,
            limit: 1,
        })];
        assert!(matches!(
            presenter.present(&frame(&rejected, None, false)),
            Presented::Unsupported(ProjectionFailure::Budget { .. })
        ));
        assert!(
            presenter.shown(),
            "a rejected frame leaves the last one shown"
        );
        presenter.forget();
        assert!(!presenter.shown());
        let Presented::Frame(repaint) =
            presenter.present(&frame(&[text(0, 0, "kept")], None, true))
        else {
            panic!("a full frame after forgetting is presented");
        };
        assert!(String::from_utf8_lossy(&repaint).contains("kept"));
    }
}
