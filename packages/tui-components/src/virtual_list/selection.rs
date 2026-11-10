//! Content selection across virtualized items.
//!
//! [`bmux_tui::selection::SelectionController`] resolves endpoints against
//! the committed scene, which holds only visible fragments, so a selection
//! whose rows scroll out of view (or whose items leave a resident window)
//! no longer resolves there. [`LogicalSelection`] instead keeps endpoints as
//! an item key plus a source offset inside that item. They do not depend on
//! rows, so they survive scrolling, reflow, eviction and reloading; every
//! frame derives the visible highlight from the selection again.
//!
//! Item order comes from an [`ItemOrder`]: [`KeyOrder`] for keys whose `Ord`
//! is list order (so endpoints in evicted items still order correctly), or a
//! [`MeasuredListIndex`] for resident items only.

use std::cmp::Ordering;
use std::ops::Range;

use bmux_tui::geometry::Rect;
use bmux_tui::measured_list::MeasuredListIndex;
use bmux_tui::selection::{
    SelectionAutoScrollRequest, SelectionContentId, SelectionEndpoint, SelectionScene,
    SelectionScrollAxis, SelectionScrollDirection,
};

use super::action::VirtualListAction;

/// One selection endpoint: a source byte offset inside one list item.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogicalSelectionEndpoint<K> {
    /// Item (or caller-defined content unit) containing the endpoint.
    pub key: K,
    /// Caller-defined source offset inside the item.
    pub source_offset: usize,
}

impl<K> LogicalSelectionEndpoint<K> {
    /// Create an endpoint at `source_offset` in item `key`.
    #[must_use]
    pub const fn new(key: K, source_offset: usize) -> Self {
        Self { key, source_offset }
    }

    /// Convert an endpoint resolved against a selection scene, mapping its
    /// content identity to an item key. Returns `None` when `key_for`
    /// does not map the content.
    pub fn from_scene(
        endpoint: &SelectionEndpoint,
        key_for: impl FnOnce(&SelectionContentId) -> Option<K>,
    ) -> Option<Self> {
        Some(Self::new(key_for(&endpoint.content_id)?, endpoint.offset))
    }
}

/// Relative order of list items for selection.
pub trait ItemOrder<K> {
    /// Order of `left` relative to `right`, or `None` when either cannot be
    /// ordered (for example, a key that is no longer indexed).
    fn compare_items(&self, left: &K, right: &K) -> Option<Ordering>;
}

/// Orders items by their keys' `Ord`, for keys that sort in list order. It
/// orders every key, resident or not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct KeyOrder;

impl<K: Ord> ItemOrder<K> for KeyOrder {
    fn compare_items(&self, left: &K, right: &K) -> Option<Ordering> {
        Some(left.cmp(right))
    }
}

/// Orders items by their position in the index; only indexed keys order.
impl<K: Clone + Ord> ItemOrder<K> for MeasuredListIndex<K> {
    fn compare_items(&self, left: &K, right: &K) -> Option<Ordering> {
        Some(self.index_of(left)?.cmp(&self.index_of(right)?))
    }
}

/// Selected source range of one item.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogicalSelectionFragment<K> {
    /// Item key.
    pub key: K,
    /// Selected source range inside the item.
    pub source_range: Range<usize>,
}

/// A selection between two logical endpoints, independent of rows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogicalSelection<K> {
    anchor: LogicalSelectionEndpoint<K>,
    focus: LogicalSelectionEndpoint<K>,
}

impl<K: Clone + PartialEq> LogicalSelection<K> {
    /// Select from `anchor` (where the gesture began) to `focus`.
    #[must_use]
    pub const fn new(
        anchor: LogicalSelectionEndpoint<K>,
        focus: LogicalSelectionEndpoint<K>,
    ) -> Self {
        Self { anchor, focus }
    }

    /// An empty selection at `at`.
    #[must_use]
    pub fn collapsed(at: LogicalSelectionEndpoint<K>) -> Self {
        Self {
            focus: at.clone(),
            anchor: at,
        }
    }

    /// Where the selection began.
    #[must_use]
    pub const fn anchor(&self) -> &LogicalSelectionEndpoint<K> {
        &self.anchor
    }

    /// Where the selection currently ends.
    #[must_use]
    pub const fn focus(&self) -> &LogicalSelectionEndpoint<K> {
        &self.focus
    }

    /// Move the focus endpoint, keeping the anchor.
    pub fn set_focus(&mut self, focus: LogicalSelectionEndpoint<K>) {
        self.focus = focus;
    }

    /// Whether both endpoints are the same position.
    #[must_use]
    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.focus
    }

    /// The endpoints as `(start, end)` in list order, or `None` when `order`
    /// cannot order their items.
    #[must_use]
    pub fn ordered(
        &self,
        order: &dyn ItemOrder<K>,
    ) -> Option<(&LogicalSelectionEndpoint<K>, &LogicalSelectionEndpoint<K>)> {
        Some(if self.is_reversed(order)? {
            (&self.focus, &self.anchor)
        } else {
            (&self.anchor, &self.focus)
        })
    }

    /// Whether the focus precedes the anchor, or `None` when unordered.
    #[must_use]
    pub fn is_reversed(&self, order: &dyn ItemOrder<K>) -> Option<bool> {
        let items = if self.anchor.key == self.focus.key {
            Ordering::Equal
        } else {
            order.compare_items(&self.focus.key, &self.anchor.key)?
        };
        Some(
            items
                .then(self.focus.source_offset.cmp(&self.anchor.source_offset))
                .is_lt(),
        )
    }

    /// Whether item `key` lies within the selection (an endpoint item or an
    /// item between them); useful for keeping selected items resident.
    #[must_use]
    pub fn spans(&self, key: &K, order: &dyn ItemOrder<K>) -> bool {
        self.range_in(key, usize::MAX, order).is_some() || self.endpoint_item(key)
    }

    /// Selected source range of item `key` whose source is `len` bytes long,
    /// or `None` when the item is not selected (or the selection is empty in
    /// it). Offsets past `len` are clamped.
    #[must_use]
    pub fn range_in(&self, key: &K, len: usize, order: &dyn ItemOrder<K>) -> Option<Range<usize>> {
        let (start, end) = self.ordered(order)?;
        let begin = if &start.key == key {
            start.source_offset.min(len)
        } else if order.compare_items(key, &start.key)?.is_gt() {
            0
        } else {
            return None;
        };
        let finish = if &end.key == key {
            end.source_offset.min(len)
        } else if order.compare_items(key, &end.key)?.is_lt() {
            len
        } else {
            return None;
        };
        (begin < finish).then_some(begin..finish)
    }

    /// Selected ranges of `items` (key and source length, in any order),
    /// skipping items the selection does not cover.
    pub fn fragments<'k>(
        &self,
        items: impl IntoIterator<Item = (&'k K, usize)>,
        order: &dyn ItemOrder<K>,
    ) -> Vec<LogicalSelectionFragment<K>>
    where
        K: 'k,
    {
        items
            .into_iter()
            .filter_map(|(key, len)| {
                Some(LogicalSelectionFragment {
                    key: key.clone(),
                    source_range: self.range_in(key, len, order)?,
                })
            })
            .collect()
    }

    /// Terminal rectangles of the fragments of `scene` that this selection
    /// covers, for [`bmux_tui::paint::PaintCx::paint_selection_highlights`].
    ///
    /// `key_for` maps a fragment's content identity to its item key. A
    /// fragment is covered when its source range overlaps the item's selected
    /// range; a zero-width fragment (such as an empty line) is covered when
    /// its offset lies inside that range. Pass the scene being painted (from
    /// [`bmux_tui::paint::PaintCx::selection`]) so highlights match the
    /// frame's own fragments.
    pub fn highlights(
        &self,
        scene: &SelectionScene,
        mut key_for: impl FnMut(&SelectionContentId) -> Option<K>,
        order: &dyn ItemOrder<K>,
    ) -> Vec<Rect> {
        if self.is_collapsed() {
            return Vec::new();
        }
        scene
            .fragments()
            .iter()
            .filter(|fragment| {
                key_for(&fragment.content_id)
                    .and_then(|key| self.range_in(&key, usize::MAX, order))
                    .is_some_and(|selected| {
                        let source = &fragment.source_range;
                        if source.is_empty() {
                            selected.contains(&source.start)
                        } else {
                            source.start < selected.end && selected.start < source.end
                        }
                    })
            })
            .map(|fragment| fragment.area)
            .collect()
    }

    fn endpoint_item(&self, key: &K) -> bool {
        &self.anchor.key == key || &self.focus.key == key
    }
}

impl<K> VirtualListAction<K> {
    /// The list action for a selection edge-autoscroll request: a vertical
    /// request becomes [`VirtualListAction::SelectionAutoScroll`] of its
    /// signed intensity (negative toward row 0); horizontal requests return
    /// `None`.
    #[must_use]
    pub fn selection_auto_scroll(request: &SelectionAutoScrollRequest) -> Option<Self> {
        if request.axis != SelectionScrollAxis::Vertical {
            return None;
        }
        let rows = i64::from(request.intensity.max(1));
        Some(Self::SelectionAutoScroll(match request.direction {
            SelectionScrollDirection::Backward => -rows,
            SelectionScrollDirection::Forward => rows,
        }))
    }
}

#[cfg(test)]
mod tests;
