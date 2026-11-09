//! Scroll and activation requests produced by virtual-list input.

/// Rows moved by one page request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PageStep {
    /// One full viewport.
    #[default]
    Viewport,
    /// One viewport minus this many rows of overlap.
    ViewportMinus(u64),
    /// A fixed number of rows.
    Rows(u64),
}

impl PageStep {
    /// Rows moved by one page for a viewport of `viewport_height` rows; always
    /// at least one.
    #[must_use]
    pub const fn rows(self, viewport_height: u64) -> u64 {
        let rows = match self {
            Self::Viewport => viewport_height,
            Self::ViewportMinus(overlap) => viewport_height.saturating_sub(overlap),
            Self::Rows(rows) => rows,
        };
        if rows == 0 { 1 } else { rows }
    }
}

/// Input device that requested a row scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollInputSource {
    /// Mouse wheel.
    Wheel,
    /// Keyboard arrow keys.
    Key,
}

/// A request produced by list input. Controlled lists queue these for the
/// caller; uncontrolled lists apply them to their own position.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum VirtualListAction<K> {
    /// Move by signed rows; negative moves toward row 0.
    ScrollRequested {
        /// Signed row delta.
        rows: i64,
        /// Input device.
        source: ScrollInputSource,
    },
    /// Move by signed pages; negative moves toward row 0.
    PageRequested(i32),
    /// Jump to one end (Home / End).
    EdgeRequested(crate::virtual_list::Edge),
    /// Scrollbar drag to a fraction in `0.0..=1.0` of the (estimated)
    /// scrollable range.
    ScrollbarDrag(f64),
    /// Signed rows from a vertical selection edge-autoscroll request.
    SelectionAutoScroll(i64),
    /// Click on an item that did not handle it.
    Activated(K),
}

/// What happened when an action was dispatched through the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionDispatch {
    /// The action was queued for the caller (controlled lists, and
    /// `Activated` in every mode).
    Queued,
    /// The list applied the action and its position changed.
    Applied,
    /// The list applied the action without changing its position.
    Unchanged,
}
