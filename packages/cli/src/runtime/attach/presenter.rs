//! Frames presented on the host through a `bmux_tui` terminal.
//!
//! The attach renderer captures what it draws instead of encoding it (the same capture an
//! observer paints), and a [`FramePresenter`] paints it over the frame it last presented and
//! encodes the difference.
//!
//! Graphics protocols keep their own encoders: a frame's pane image and extension graphic overlay
//! bytes are written through the terminal's output hooks, removals before its cells and
//! placements after them. A frame holding what a terminal frame cannot show (a capture over its
//! budget, a rejected local zoom) is not presented: the host keeps the last frame until the next
//! one, which repaints in full.

use bmux_attach_pipeline::projection::ProjectionFailure;

use super::state::AttachViewState;

pub(super) use bmux_attach_pipeline::frame_presenter::{CapturedFrame, Graphics, Presented};

/// Whether the next frame is presented on the host: it is not when the view is headless.
pub(super) const fn usable(view_state: &AttachViewState) -> bool {
    !view_state.presentation_only
}

/// Decide whether the next frame is presented on the host. A presenter that has shown nothing
/// (or forgot what it showed) paints over a blank screen, so that frame repaints in full.
pub(super) fn begin_frame(view_state: &mut AttachViewState) {
    view_state.presenting = usable(view_state);
    if view_state.presenting && !view_state.terminal_presenter.shown() {
        view_state
            .dirty
            .mark_full_frame(super::state::AttachDirtySource::ManualRedraw);
    }
}

/// A frame held what a terminal frame cannot show; nothing reached the host.
#[derive(Debug)]
pub(super) struct PresenterRejected(pub(super) ProjectionFailure);

impl std::fmt::Display for PresenterRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "attach frame cannot be presented: {}", self.0)
    }
}

impl std::error::Error for PresenterRejected {}

/// Paint a captured frame over the last one presented and return the bytes that change the
/// host to match.
pub(super) fn present(view_state: &mut AttachViewState, frame: &CapturedFrame<'_>) -> Presented {
    view_state.terminal_presenter.present(frame)
}

/// Forget what the terminal presented, so its next frame repaints in full: after its bytes
/// failed to reach the host, or after a frame was not presented.
pub(super) fn forget(view_state: &mut AttachViewState) {
    view_state.terminal_presenter.forget();
}
