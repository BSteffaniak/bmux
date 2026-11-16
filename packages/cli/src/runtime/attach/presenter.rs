//! Frames presented on the host through a `bmux_tui` terminal.
//!
//! With `BMUX_ATTACH_PRESENTER=terminal`, the attach renderer captures what it draws instead of
//! encoding it (the same capture the observer paints), and a `bmux_tui` [`Terminal`] paints it
//! over the frame it last presented and encodes the difference: styles, cursor, synchronized
//! output and row moves come from the terminal, not the renderer.
//!
//! Graphics are still written by the renderer, so while any are on the host (or a frame
//! captures one) frames go through the renderer's own bytes, and the terminal repaints in full
//! when they are gone.

use bmux_attach_pipeline::AttachCursorState;
use bmux_attach_pipeline::projection::{PaintOperation, ProjectionFailure};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::pointer::PointerShapeCapability;
use bmux_tui::terminal::{ScrollRegionAcceleration, Terminal};

use super::state::AttachViewState;

/// The presenter `BMUX_ATTACH_PRESENTER` selects: a terminal for `terminal`, otherwise none.
pub(super) fn from_env() -> Option<Terminal<Vec<u8>>> {
    (std::env::var("BMUX_ATTACH_PRESENTER").as_deref() == Ok("terminal")).then(terminal)
}

/// A terminal to present frames through.
pub(super) fn terminal() -> Terminal<Vec<u8>> {
    let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 0, 0));
    // The attach runtime presents pointer shapes itself.
    terminal.set_pointer_capability(PointerShapeCapability::Unsupported);
    terminal
}

/// Whether the next frame can be presented through the terminal: one is configured, the frame
/// goes to the host (not to an observer or a headless view), and no graphics are involved.
pub(super) fn usable(view_state: &AttachViewState) -> bool {
    view_state.terminal_presenter.is_some()
        && !view_state.presentation_only
        && view_state.observer.is_none()
        && !view_state.graphics_on_host()
        && !view_state.presenter_paused
}

/// After a frame went through the renderer's own bytes: resume presenting once no graphics it
/// placed (extension overlays) remain on the host.
pub(super) fn rendered_as_bytes(view_state: &mut AttachViewState) {
    if view_state.terminal_graphics_cache.is_empty() {
        view_state.presenter_paused = false;
    }
}

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
    if operations.is_empty() && !cursor_changed && !full {
        return Presented::Unchanged;
    }
    terminal.set_scroll_region_acceleration(if margins {
        ScrollRegionAcceleration::AutoWithMargins
    } else {
        ScrollRegionAcceleration::Auto
    });
    terminal.writer_mut().clear();
    let drawn = terminal.draw_over(|cx| {
        // Checked above: every operation paints.
        let _ = bmux_attach_pipeline::projection::paint_over(cx, operations);
        if let Some(cursor) = cursor {
            cx.set_cursor(Point::new(cursor.x, cursor.y), cursor.visible);
        }
    });
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
