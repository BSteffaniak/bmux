//! Frames presented on the host through a `bmux_tui` terminal.
//!
//! By default the attach renderer captures what it draws instead of encoding it (the same
//! capture the observer paints), and a [`FramePresenter`] paints it over the frame it last
//! presented and encodes the difference.
//!
//! Graphics protocols keep their own encoders: a frame's pane image and extension graphic overlay
//! bytes are written through the terminal's output hooks, removals before its cells and
//! placements after them. A frame holding what a terminal frame cannot show (an imperative
//! extension's bytes, a rejected projection) is rendered again as the renderer's own bytes.

use bmux_attach_pipeline::frame_presenter::{FramePresenter, unsupported};
use bmux_attach_pipeline::projection::PaintOperation;

use super::state::AttachViewState;

pub(super) use bmux_attach_pipeline::frame_presenter::{CapturedFrame, Graphics, Presented};

/// The presenter to use: a terminal, unless `BMUX_ATTACH_PRESENTER=bytes` asks for the
/// renderer's own bytes.
pub(super) fn from_env() -> Option<FramePresenter> {
    (std::env::var("BMUX_ATTACH_PRESENTER").as_deref() != Ok("bytes")).then(terminal)
}

/// A presenter to present frames through.
pub(super) fn terminal() -> FramePresenter {
    FramePresenter::new()
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

/// After a frame went through the renderer's own bytes: resume presenting once a full frame
/// held nothing a terminal frame cannot show. A partial frame says nothing about what it did not
/// repaint, so presenting does not resume on one: otherwise frames would alternate between a
/// rejected terminal frame and a byte frame while unsupported content stays on screen.
pub(super) fn rendered_as_bytes(
    view_state: &mut AttachViewState,
    full: bool,
    operations: &[PaintOperation],
) {
    if full && unsupported(operations).is_none() {
        view_state.presenter_paused = false;
    }
}

/// Paint a captured frame over the last one presented and return the bytes that change the
/// host to match.
pub(super) fn present(view_state: &mut AttachViewState, frame: &CapturedFrame<'_>) -> Presented {
    view_state
        .terminal_presenter
        .as_mut()
        .map_or(Presented::Unchanged, |presenter| presenter.present(frame))
}

/// Forget what the terminal presented, so its next frame repaints in full: after its bytes
/// failed to reach the host, or after frames went through the renderer's own bytes.
pub(super) fn forget(view_state: &mut AttachViewState) {
    if let Some(presenter) = view_state.terminal_presenter.as_mut() {
        presenter.forget();
    }
}
