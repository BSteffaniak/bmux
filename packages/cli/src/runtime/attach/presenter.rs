//! Frames presented through a `bmux_tui` terminal.
//!
//! The attach renderer paints each frame straight into the terminal a
//! [`bmux_attach_pipeline::frame_presenter::FramePresenter`] owns, over the frame it last
//! presented, and the terminal encodes the difference. Every client presents this way: a headless
//! client's terminal encodes nothing and keeps the frame for its observer, which mirrors the
//! change the terminal made.
//!
//! Graphics protocols keep their own encoders: a frame's pane image and extension graphic overlay
//! bytes are written through the terminal's output hooks, removals before its cells and
//! placements after them. A frame holding what a terminal frame cannot show (a capture over its
//! budget, a rejected local zoom) is not presented: the host keeps the last frame until the next
//! one, which repaints in full.

use bmux_attach_pipeline::projection::ProjectionFailure;

use super::state::AttachViewState;

pub(super) use bmux_attach_pipeline::frame_presenter::Presented;

/// Whether the next frame is written to a host: it is not when the view is headless.
pub(super) const fn usable(view_state: &AttachViewState) -> bool {
    !view_state.presentation_only
}

/// Decide whether the next frame is written to a host; a headless client's presenter encodes
/// nothing. A presenter that has shown nothing (or forgot what it showed) paints over a blank
/// screen, so that frame repaints in full.
pub(super) fn begin_frame(view_state: &mut AttachViewState) {
    view_state.presenting = usable(view_state);
    view_state
        .terminal_presenter
        .set_encoding(view_state.presenting);
    if !view_state.terminal_presenter.shown() {
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

/// Forget what the terminal presented, so its next frame repaints in full: after its bytes
/// failed to reach the host, or after a frame was not presented.
pub(super) fn forget(view_state: &mut AttachViewState) {
    view_state.terminal_presenter.forget();
}
