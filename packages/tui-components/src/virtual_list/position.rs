//! Keyed scroll positions, overscroll geometry, and pure resolution functions.
//!
//! A position is stored as a key plus a row (or opaque caller source position)
//! inside that item. Collection rows are derived from the current
//! [`MeasuredListIndex`] each time they are needed, so a position survives
//! insertion, removal, reorder, and width reflow without correction.

use std::ops::Range;

use bmux_tui::measured_list::MeasuredListIndex;

use crate::resident_window::EdgeRows;

/// Offset of a viewport top inside its anchored item.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AnchorOffset<S = ()> {
    /// Row within the anchored item, clamped to the item's extent (its height
    /// plus the trailing gap) on resolution.
    Row(u64),
    /// Caller source position inside the item plus rows below the row where
    /// it begins. Resolved through a [`SourceMap`]; an unmapped position falls
    /// back to `Row(row_delta)`.
    Source {
        /// Opaque caller source position. BMUX never interprets it.
        position: S,
        /// Rows below the first row of `position`.
        row_delta: u64,
    },
}

/// A stable keyed anchor: an item and an offset inside it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListAnchor<K, S = ()> {
    /// Stable item key.
    pub key: K,
    /// Offset inside the item.
    pub offset: AnchorOffset<S>,
}

impl<K, S> ListAnchor<K, S> {
    /// Create an anchor from a key and an offset.
    #[must_use]
    pub const fn new(key: K, offset: AnchorOffset<S>) -> Self {
        Self { key, offset }
    }

    /// Anchor the first row of `key`.
    #[must_use]
    pub const fn top(key: K) -> Self {
        Self::row(key, 0)
    }

    /// Anchor row `row` of `key`.
    #[must_use]
    pub const fn row(key: K, row: u64) -> Self {
        Self {
            key,
            offset: AnchorOffset::Row(row),
        }
    }
}

/// Keyed scroll position of a virtualized list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum ListPosition<K, S = ()> {
    /// First collection row on the viewport top row.
    #[default]
    Top,
    /// Last content row on the viewport bottom row (top row 0 when the content
    /// fits). Survives growth; released only by an explicit caller or user
    /// action, never by a content change.
    FollowBottom,
    /// Anchor row on the viewport top row, clamped to `0..=max_content_top`
    /// (no blank rows below content).
    Anchored(ListAnchor<K, S>),
    /// Anchor row on the viewport top row, clamped to `0..=max_overscroll_top`
    /// (blank tail rows below content permitted).
    Overscrolled(ListAnchor<K, S>),
}

impl<K, S> ListPosition<K, S> {
    /// The keyed anchor of an `Anchored` or `Overscrolled` position.
    #[must_use]
    pub const fn anchor(&self) -> Option<&ListAnchor<K, S>> {
        match self {
            Self::Anchored(anchor) | Self::Overscrolled(anchor) => Some(anchor),
            Self::Top | Self::FollowBottom => None,
        }
    }

    /// Whether this position follows the content bottom.
    #[must_use]
    pub const fn is_follow_bottom(&self) -> bool {
        matches!(self, Self::FollowBottom)
    }
}

/// One end of a list or viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge {
    /// The first row.
    Top,
    /// The last row.
    Bottom,
}

/// Where a keyed item is placed relative to the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AnchorPolicy {
    /// Item top on the viewport top.
    ItemTopAtTop,
    /// Item bottom on the viewport bottom; an item taller than the viewport is
    /// placed top-at-top instead, so its top is never pushed off.
    ItemBottomAtBottom,
    /// Item centered (rounded toward the item top); taller items are placed
    /// top-at-top.
    Center,
    /// No movement when the item is fully visible, or when a taller item covers
    /// the whole viewport. Otherwise `Edge::Top` places it like
    /// [`AnchorPolicy::ItemTopAtTop`] and `Edge::Bottom` like
    /// [`AnchorPolicy::ItemBottomAtBottom`].
    EnsureVisibleFrom(Edge),
}

/// Blank tail extent permitted below the last content row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Overscroll {
    /// No blank rows below content.
    #[default]
    None,
    /// At most this many blank rows below the last content row.
    Rows(u64),
    /// At most `viewport_height - n` blank rows (`ViewportMinus(1)` lets the
    /// last content row reach the viewport top).
    ViewportMinus(u64),
}

impl Overscroll {
    /// Maximum blank rows below content for a viewport of `viewport_height`.
    #[must_use]
    pub const fn max_below(self, viewport_height: u64) -> u64 {
        match self {
            Self::None => 0,
            Self::Rows(rows) => rows,
            Self::ViewportMinus(rows) => viewport_height.saturating_sub(rows),
        }
    }
}

/// Viewport inputs to position resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListGeometry {
    /// Viewport height in logical rows.
    pub viewport_height: u64,
    /// Blank tail policy.
    pub overscroll: Overscroll,
}

impl ListGeometry {
    /// Geometry for a viewport without overscroll.
    #[must_use]
    pub const fn new(viewport_height: u64) -> Self {
        Self {
            viewport_height,
            overscroll: Overscroll::None,
        }
    }

    /// Replace the overscroll policy.
    #[must_use]
    pub const fn overscroll(mut self, overscroll: Overscroll) -> Self {
        self.overscroll = overscroll;
        self
    }

    /// Largest top offset without blank rows below content:
    /// `total_height - viewport_height`, saturating.
    #[must_use]
    pub const fn max_content_top(self, total_height: u64) -> u64 {
        total_height.saturating_sub(self.viewport_height)
    }

    /// Largest top offset including overscroll:
    /// `max(max_content_top, min(total + max_below - viewport_height, total - 1))`,
    /// saturating, so at least one content row stays visible.
    #[must_use]
    pub const fn max_overscroll_top(self, total_height: u64) -> u64 {
        let content = self.max_content_top(total_height);
        let below = self.overscroll.max_below(self.viewport_height);
        let extended = total_height
            .saturating_add(below)
            .saturating_sub(self.viewport_height);
        let last_row = total_height.saturating_sub(1);
        let overscrolled = if extended < last_row {
            extended
        } else {
            last_row
        };
        if overscrolled > content {
            overscrolled
        } else {
            content
        }
    }
}

/// Collection rows resolved from a keyed position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResolvedPosition {
    /// Collection row on the viewport top row.
    pub top_offset: u64,
    /// Viewport height used for resolution.
    pub viewport_height: u64,
    /// Exact total collection height.
    pub total_height: u64,
    /// Largest top offset without blank rows below content.
    pub max_content_top: u64,
    /// Largest top offset including overscroll.
    pub max_overscroll_top: u64,
}

impl ResolvedPosition {
    /// Collection rows covered by the viewport (`top..top + viewport_height`).
    #[must_use]
    pub const fn rows(self) -> Range<u64> {
        self.top_offset..self.top_offset.saturating_add(self.viewport_height)
    }

    /// Blank viewport rows below the last content row.
    #[must_use]
    pub const fn blank_rows_below(self) -> u64 {
        let blank = self
            .top_offset
            .saturating_add(self.viewport_height)
            .saturating_sub(self.total_height);
        if blank < self.viewport_height {
            blank
        } else {
            self.viewport_height
        }
    }

    /// Whether blank overscroll rows are shown below content.
    #[must_use]
    pub const fn is_overscrolled(self) -> bool {
        self.top_offset > self.max_content_top
    }

    /// Whether the last content row is at or above the viewport bottom.
    #[must_use]
    pub const fn is_at_content_bottom(self) -> bool {
        self.top_offset >= self.max_content_top
    }

    /// Exact measured rows beyond each viewport edge, for resident-window
    /// prefetch. Overscroll rows count as zero rows after.
    #[must_use]
    pub const fn edge_rows(self) -> EdgeRows {
        EdgeRows {
            before: self.top_offset,
            after: self
                .total_height
                .saturating_sub(self.top_offset.saturating_add(self.viewport_height)),
        }
    }
}

/// Caller hook that maps opaque source positions to rows inside an item at the
/// current width.
pub trait SourceMap<K, S> {
    /// Row inside item `key`, at the current width, where `position` begins.
    fn row_for_source(&self, key: &K, position: &S) -> Option<u64>;

    /// Source position covering `row` of item `key`, with the row where that
    /// position begins (at most `row`).
    fn source_for_row(&self, key: &K, row: u64) -> Option<(S, u64)>;
}

/// A [`SourceMap`] that maps nothing, so every anchor uses row offsets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct NoSourceMap;

impl<K, S> SourceMap<K, S> for NoSourceMap {
    fn row_for_source(&self, _key: &K, _position: &S) -> Option<u64> {
        None
    }

    fn source_for_row(&self, _key: &K, _row: u64) -> Option<(S, u64)> {
        None
    }
}

/// Extent of the item at `index`: its height plus the gap, except for the
/// final item, whose trailing gap is excluded.
pub(super) fn item_extent<K: Clone + Ord>(index: &MeasuredListIndex<K>, position: usize) -> u64 {
    let Some(item) = index.item(position) else {
        return 0;
    };
    if position + 1 < index.len() {
        item.height.saturating_add(index.gap())
    } else {
        item.height
    }
}

/// Clamp a row inside an item of `extent` rows.
const fn clamp_row(row: u64, extent: u64) -> u64 {
    if extent == 0 {
        0
    } else if row < extent {
        row
    } else {
        extent - 1
    }
}

/// Requested row inside the anchored item, before clamping.
fn requested_row<K, S>(anchor: &ListAnchor<K, S>, sources: &dyn SourceMap<K, S>) -> u64 {
    match &anchor.offset {
        AnchorOffset::Row(row) => *row,
        AnchorOffset::Source {
            position,
            row_delta,
        } => sources
            .row_for_source(&anchor.key, position)
            .map_or(*row_delta, |row| row.saturating_add(*row_delta)),
    }
}

/// Collection row of an anchor, or `None` when its key is not indexed.
pub(super) fn anchor_row<K: Clone + Ord, S>(
    index: &MeasuredListIndex<K>,
    anchor: &ListAnchor<K, S>,
    sources: &dyn SourceMap<K, S>,
) -> Option<u64> {
    let position = index.index_of(&anchor.key)?;
    let start = index.item_offset(position)?;
    let row = clamp_row(requested_row(anchor, sources), item_extent(index, position));
    Some(start.saturating_add(row))
}

/// Resolve a keyed position to collection rows.
///
/// `Top` is row 0; `FollowBottom` is `max_content_top`; `Anchored` and
/// `Overscrolled` resolve their anchor row and clamp it to `max_content_top`
/// and `max_overscroll_top` respectively. Returns `None` only when an anchor
/// key is not indexed.
pub fn resolve_position<K: Clone + Ord, S>(
    index: &MeasuredListIndex<K>,
    position: &ListPosition<K, S>,
    geometry: ListGeometry,
    sources: &dyn SourceMap<K, S>,
) -> Option<ResolvedPosition> {
    let total_height = index.total_height();
    let max_content_top = geometry.max_content_top(total_height);
    let max_overscroll_top = geometry.max_overscroll_top(total_height);
    let top_offset = match position {
        ListPosition::Top => 0,
        ListPosition::FollowBottom => max_content_top,
        ListPosition::Anchored(anchor) => anchor_row(index, anchor, sources)?.min(max_content_top),
        ListPosition::Overscrolled(anchor) => {
            anchor_row(index, anchor, sources)?.min(max_overscroll_top)
        }
    };
    Some(ResolvedPosition {
        top_offset,
        viewport_height: geometry.viewport_height,
        total_height,
        max_content_top,
        max_overscroll_top,
    })
}

/// Keyed anchor for the collection row `top_offset`.
///
/// Prefers a `Source` offset when the [`SourceMap`] maps the row, otherwise
/// produces `Row`. Returns `None` when `top_offset >= total_height`.
pub fn anchor_at_offset<K: Clone + Ord, S>(
    index: &MeasuredListIndex<K>,
    top_offset: u64,
    sources: &dyn SourceMap<K, S>,
) -> Option<ListAnchor<K, S>> {
    let position = index.item_at_offset(top_offset)?;
    let start = index.item_offset(position)?;
    let key = index.item(position)?.key.clone();
    let row = top_offset.saturating_sub(start);
    let offset = match sources.source_for_row(&key, row) {
        Some((source, begins)) if begins <= row => AnchorOffset::Source {
            position: source,
            row_delta: row - begins,
        },
        _ => AnchorOffset::Row(row),
    };
    Some(ListAnchor { key, offset })
}

/// Keyed position whose resolved top is `top_offset` under `geometry`.
///
/// Returns `Top` for an empty index, `Overscrolled` past `max_content_top`,
/// and `Anchored` otherwise. Never returns `FollowBottom`. Offsets past the
/// last content row anchor the last row.
pub fn position_at_offset<K: Clone + Ord, S>(
    index: &MeasuredListIndex<K>,
    top_offset: u64,
    geometry: ListGeometry,
    sources: &dyn SourceMap<K, S>,
) -> ListPosition<K, S> {
    let total = index.total_height();
    if total == 0 {
        // Only zero-height items (or none): every position resolves to row 0.
        return index.item(0).map_or(ListPosition::Top, |item| {
            ListPosition::Anchored(ListAnchor::top(item.key.clone()))
        });
    }
    let row = top_offset.min(total - 1);
    let Some(anchor) = anchor_at_offset(index, row, sources) else {
        return ListPosition::Top;
    };
    if top_offset > geometry.max_content_top(total) {
        ListPosition::Overscrolled(anchor)
    } else {
        ListPosition::Anchored(anchor)
    }
}

/// Top offset that places `key` according to `policy`, clamped to
/// `0..=max_overscroll_top`. Returns `None` when `key` is not indexed.
///
/// `current_top` is the viewport's current top offset; only
/// [`AnchorPolicy::EnsureVisibleFrom`] depends on it.
pub fn target_top_offset<K: Clone + Ord>(
    index: &MeasuredListIndex<K>,
    key: &K,
    policy: AnchorPolicy,
    current_top: u64,
    geometry: ListGeometry,
) -> Option<u64> {
    let position = index.index_of(key)?;
    let start = index.item_offset(position)?;
    let height = index.item(position)?.height;
    let viewport = geometry.viewport_height;
    let bottom_at_bottom = || {
        if height > viewport {
            start
        } else {
            start.saturating_add(height).saturating_sub(viewport)
        }
    };
    let target = match policy {
        AnchorPolicy::ItemTopAtTop => start,
        AnchorPolicy::ItemBottomAtBottom => bottom_at_bottom(),
        AnchorPolicy::Center => {
            if height > viewport {
                start
            } else {
                start.saturating_sub((viewport - height) / 2)
            }
        }
        AnchorPolicy::EnsureVisibleFrom(edge) => {
            let end = start.saturating_add(height);
            let view_end = current_top.saturating_add(viewport);
            let fully_visible = start >= current_top && end <= view_end;
            let covers = height > viewport && start <= current_top && end >= view_end;
            if fully_visible || covers {
                current_top
            } else {
                match edge {
                    Edge::Top => start,
                    Edge::Bottom => bottom_at_bottom(),
                }
            }
        }
    };
    Some(target.min(geometry.max_overscroll_top(index.total_height())))
}

/// Keyed position that places `key` according to `policy`:
/// `position_at_offset(target_top_offset(..))`.
pub fn anchor_for<K: Clone + Ord, S>(
    index: &MeasuredListIndex<K>,
    key: &K,
    policy: AnchorPolicy,
    current_top: u64,
    geometry: ListGeometry,
    sources: &dyn SourceMap<K, S>,
) -> Option<ListPosition<K, S>> {
    let top = target_top_offset(index, key, policy, current_top, geometry)?;
    Some(position_at_offset(index, top, geometry, sources))
}

#[cfg(test)]
mod tests;
