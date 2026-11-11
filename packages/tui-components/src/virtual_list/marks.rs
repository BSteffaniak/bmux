//! Caller-supplied marks drawn on a virtual list's scrollbar track, scaled
//! to the whole collection.

use std::ops::Range;

use bmux_tui::geometry::Rect;
use bmux_tui::style::Style;
use bmux_tui::text_width::display_width;

use super::{MeasuredListIndex, PositionEstimate};

/// Where a [`ScrollbarMark`] sits in the collection.
///
/// Rows are logical collection rows of the measured (resident) content, the
/// same rows as [`super::ResolvedPosition::top_offset`]. Keyed positions
/// follow their items through insertion, removal, reorder, and reflow; a
/// key that is not indexed draws nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScrollbarMarkPosition<K> {
    /// One collection row.
    Row(u64),
    /// Collection rows `start..end`; an empty range marks `start`.
    Rows(Range<u64>),
    /// Every row of one item (its first row when it has zero height).
    Item(K),
    /// From the first row of `first` through the last row of `last`, in
    /// collection order regardless of argument order.
    Items {
        /// One end of the marked item range.
        first: K,
        /// The other end of the marked item range.
        last: K,
    },
}

/// A styled mark on the scrollbar track of a `VirtualListComponent`.
///
/// A mark covers the track cells its rows map to when the whole collection
/// is scaled onto the track (at least one cell). Marks paint after the track
/// in slice order, so a later mark wins a shared cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScrollbarMark<K> {
    /// Marked collection rows.
    pub position: ScrollbarMarkPosition<K>,
    /// Style of marked cells.
    pub style: Style,
    /// Single-cell symbol for marked cells, or `None` to keep the scrollbar's
    /// own symbol for each cell. A symbol that is not exactly one terminal
    /// cell wide is ignored in favor of the scrollbar's symbol.
    pub symbol: Option<&'static str>,
    /// Whether the mark also draws over thumb cells. By default the thumb
    /// hides marks under it.
    pub over_thumb: bool,
}

impl<K> ScrollbarMark<K> {
    /// Create a mark at `position` with `style`, keeping the scrollbar's
    /// symbols and staying under the thumb.
    #[must_use]
    pub const fn new(position: ScrollbarMarkPosition<K>, style: Style) -> Self {
        Self {
            position,
            style,
            symbol: None,
            over_thumb: false,
        }
    }

    /// Create a mark on one collection row.
    #[must_use]
    pub const fn row(row: u64, style: Style) -> Self {
        Self::new(ScrollbarMarkPosition::Row(row), style)
    }

    /// Create a mark covering one item.
    #[must_use]
    pub const fn item(key: K, style: Style) -> Self {
        Self::new(ScrollbarMarkPosition::Item(key), style)
    }

    /// Return this mark drawn with a single-cell `symbol`.
    #[must_use]
    pub const fn symbol(mut self, symbol: &'static str) -> Self {
        self.symbol = Some(symbol);
        self
    }

    /// Return this mark drawn over (`true`) or under (`false`) the thumb.
    #[must_use]
    pub const fn over_thumb(mut self, over_thumb: bool) -> Self {
        self.over_thumb = over_thumb;
        self
    }

    /// The mark's symbol when it is exactly one terminal cell wide.
    pub(super) fn cell_symbol(&self) -> Option<&'static str> {
        self.symbol.filter(|symbol| display_width(symbol) == 1)
    }
}

/// Track cells covered by one scrollbar mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarMarkCells {
    /// Index of the mark in the slice supplied to the component.
    pub mark: usize,
    /// Covered track cells in the list's root-local coordinates: one column,
    /// one or more rows.
    pub area: Rect,
}

/// Exact collection rows (`start..end`, never empty) covered by `position`,
/// or `None` when a key is not indexed or the collection is empty.
fn mark_rows<K: Clone + Ord>(
    index: &MeasuredListIndex<K>,
    position: &ScrollbarMarkPosition<K>,
) -> Option<Range<u64>> {
    let total = index.total_height();
    if total == 0 {
        return None;
    }
    let item_rows = |key: &K| {
        let start = index.key_offset(key)?;
        let height = index.item_by_key(key)?.height;
        Some(start..start.saturating_add(height.max(1)))
    };
    let rows = match position {
        ScrollbarMarkPosition::Row(row) => *row..row.saturating_add(1),
        ScrollbarMarkPosition::Rows(rows) => rows.start..rows.end.max(rows.start.saturating_add(1)),
        ScrollbarMarkPosition::Item(key) => item_rows(key)?,
        ScrollbarMarkPosition::Items { first, last } => {
            let (first, last) = (item_rows(first)?, item_rows(last)?);
            first.start.min(last.start)..first.end.max(last.end)
        }
    };
    let start = rows.start.min(total - 1);
    Some(start..rows.end.clamp(start + 1, total))
}

/// Track cells for each mark on a vertical `track`, scaling the scrollbar
/// model's whole collection onto it: exact rows, offset by the estimate's
/// rows before the resident content when an active estimate is drawn.
pub(super) fn mark_cells<K: Clone + Ord>(
    index: &MeasuredListIndex<K>,
    estimate: Option<PositionEstimate>,
    marks: &[ScrollbarMark<K>],
    track: Rect,
) -> Vec<ScrollbarMarkCells> {
    let resident = index.total_height();
    let (before, total) = match estimate {
        Some(estimate) if estimate.resident == resident => (estimate.before, estimate.total()),
        _ => (0, resident),
    };
    if track.height == 0 || track.width == 0 || total == 0 {
        return Vec::new();
    }
    let cells = u128::from(track.height);
    let cell = |row: u64| {
        let scaled = u128::from(before.saturating_add(row)) * cells / u128::from(total);
        u16::try_from(scaled.min(cells - 1)).unwrap_or(track.height - 1)
    };
    marks
        .iter()
        .enumerate()
        .filter_map(|(mark, entry)| {
            let rows = mark_rows(index, &entry.position)?;
            let (first, last) = (cell(rows.start), cell(rows.end - 1));
            Some(ScrollbarMarkCells {
                mark,
                area: Rect::new(track.x, track.y + first, 1, last - first + 1),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
