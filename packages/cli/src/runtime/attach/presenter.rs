//! Frames presented on the host through a `bmux_tui` terminal.
//!
//! By default the attach renderer captures what it draws instead of encoding it (the same capture the observer paints), and a `bmux_tui` [`Terminal`] paints it
//! over the frame it last presented and encodes the difference: styles, cursor, synchronized
//! output and row moves come from the terminal, not the renderer.
//!
//! Graphics protocols keep their own encoders: a frame's pane image and extension graphic overlay
//! bytes are written through the terminal's output hooks, removals before its cells and
//! placements after them. A frame holding what a terminal frame cannot show (an imperative
//! extension's bytes, a rejected projection) is rendered again as the renderer's own bytes.

use bmux_attach_pipeline::AttachCursorState;
use bmux_attach_pipeline::projection::{PaintOperation, ProjectionFailure};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::pointer::PointerShapeCapability;
use bmux_tui::terminal::{ScrollRegionAcceleration, Terminal};
use std::io::Write;

use super::state::AttachViewState;

/// The presenter to use: a terminal, unless `BMUX_ATTACH_PRESENTER=bytes` asks for the
/// renderer's own bytes.
pub(super) fn from_env() -> Option<Terminal<Vec<u8>>> {
    (std::env::var("BMUX_ATTACH_PRESENTER").as_deref() != Ok("bytes")).then(terminal)
}

/// A terminal to present frames through.
pub(super) fn terminal() -> Terminal<Vec<u8>> {
    let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 0, 0));
    // The attach runtime presents pointer shapes itself.
    terminal.set_pointer_capability(PointerShapeCapability::Unsupported);
    terminal
}

/// Whether the next frame can be presented through the terminal: one is configured, the frame
/// goes to the host (not to an observer or a headless view), and frames are not paused on the
/// renderer's own bytes.
pub(super) const fn usable(view_state: &AttachViewState) -> bool {
    view_state.terminal_presenter.is_some()
        && !view_state.presentation_only
        && view_state.observer.is_none()
        && !view_state.presenter_paused
}

/// Decide whether the next frame is presented through the terminal. Switching to it from the
/// renderer's own bytes, which changed the host behind the terminal's back, repaints in full.
pub(super) fn begin_frame(view_state: &mut AttachViewState) {
    let was_presenting = view_state.presenting;
    view_state.presenting = usable(view_state);
    if view_state.presenting && !was_presenting {
        forget(view_state);
        view_state
            .dirty
            .mark_full_frame(super::state::AttachDirtySource::ManualRedraw);
    }
}

/// Graphics protocol bytes of a frame: written before its cells (clearing placements, erasing
/// raster images) and after them (placing images), inside the same synchronized frame.
#[derive(Clone, Copy)]
pub(super) struct Graphics<'bytes> {
    pub(super) before: &'bytes [u8],
    pub(super) after: &'bytes [u8],
}

/// After a frame went through the renderer's own bytes: resume presenting once a full frame
/// held nothing a terminal frame cannot show. A partial frame says nothing about what it did not repaint, so presenting
/// does not resume on one: otherwise frames would alternate between a rejected terminal frame
/// and a byte frame while unsupported content stays on screen.
pub(super) fn rendered_as_bytes(
    view_state: &mut AttachViewState,
    full: bool,
    operations: &[PaintOperation],
) {
    if full && unsupported(operations).is_none() {
        view_state.presenter_paused = false;
    }
}

/// A frame held what a terminal frame cannot show: nothing reached the host, and the frame must
/// be rendered again as the renderer's own bytes.
#[derive(Debug)]
pub(super) struct PresenterRejected;

impl std::fmt::Display for PresenterRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("frame needs the renderer's own bytes")
    }
}

impl std::error::Error for PresenterRejected {}

/// What presenting a captured frame produced.
pub(super) enum Presented {
    /// Bytes to write to the host, the whole frame.
    Frame(Vec<u8>),
    /// Nothing changed on the host.
    Unchanged,
    /// The frame holds what a terminal frame cannot show yet; render it as bytes instead.
    Unsupported(ProjectionFailure),
}

/// Paint `operations` and the cursor over the terminal's last frame, as the host must show
/// them, and return the bytes that change the host to match. `full` starts from a blank
/// screen, as the renderer's full frames do.
pub(super) fn present(
    view_state: &mut AttachViewState,
    operations: &[PaintOperation],
    cursor: Option<AttachCursorState>,
    full: bool,
    (cols, rows): (u16, u16),
    margins: bool,
    graphics: Graphics<'_>,
) -> Presented {
    if let Some(failure) = unsupported(operations) {
        return Presented::Unsupported(failure);
    }
    let cursor_changed = cursor != view_state.last_cursor_state;
    let Some(terminal) = view_state.terminal_presenter.as_mut() else {
        return Presented::Unchanged;
    };
    terminal.resize(Rect::new(0, 0, cols, rows));
    if full {
        terminal.reset();
    }
    let has_graphics = !graphics.before.is_empty() || !graphics.after.is_empty();
    if operations.is_empty() && !cursor_changed && !full && !has_graphics {
        return Presented::Unchanged;
    }
    terminal.set_scroll_region_acceleration(if margins {
        ScrollRegionAcceleration::AutoWithMargins
    } else {
        ScrollRegionAcceleration::Auto
    });
    terminal.writer_mut().clear();
    let paint = |cx: &mut bmux_tui::paint::PaintCx<'_, '_>| {
        // Checked above: every operation paints.
        let _ = bmux_attach_pipeline::projection::paint_over(cx, operations);
        if let Some(cursor) = cursor {
            cx.set_cursor(Point::new(cursor.x, cursor.y), cursor.visible);
        }
    };
    let drawn = if has_graphics {
        terminal.draw_over_with_output_hooks(
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
        terminal.draw_over(paint)
    };
    if drawn.is_ok() {
        Presented::Frame(std::mem::take(terminal.writer_mut()))
    } else {
        terminal.reset();
        Presented::Unchanged
    }
}

/// Forget what the terminal presented, so its next frame repaints in full: after its bytes
/// failed to reach the host, or after frames went through the renderer's own bytes.
pub(super) fn forget(view_state: &mut AttachViewState) {
    if let Some(terminal) = view_state.terminal_presenter.as_mut() {
        terminal.reset();
    }
}

/// The first operation a terminal frame cannot show, if any.
fn unsupported(operations: &[PaintOperation]) -> Option<ProjectionFailure> {
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
