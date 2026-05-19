//! Widget traits and helpers.

use crate::frame::Frame;
use crate::geometry::Rect;

/// A renderable terminal UI component.
pub trait Widget {
    /// Render this widget into `area`.
    fn render(&self, area: Rect, frame: &mut Frame<'_>);
}

/// A renderable terminal UI component with caller-owned state.
pub trait StatefulWidget {
    /// Caller-owned widget state.
    type State;

    /// Render this widget into `area` using `state`.
    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State);
}
