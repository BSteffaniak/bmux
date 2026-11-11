//! Keyboard focus targets inside virtualized list items.
//!
//! An item may expose any number of focus targets, each a row range of the
//! item's retained layout (a section header, a disclosure, a whole item).
//! [`FocusTraversal`] orders the targets of indexed items in list order and
//! walks them; [`reveal_for_focus`] and [`VirtualListState::reveal_for_focus`]
//! compute the position that brings a target into view from the edge focus
//! approaches. Targets carry caller-owned identities; the list never owns
//! focus state and never changes its own position to follow focus.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ops::Range;

use bmux_tui::measured_list::MeasuredListIndex;

use super::VirtualListState;
use super::position::{
    AnchorPolicy, Edge, ListGeometry, ListPosition, SourceMap, place_rows_below, position_at_offset,
};

/// One focus target inside a list item.
///
/// `rows` are rows of the item's own layout (row 0 is the item top). An
/// empty range denotes a one-row target at `rows.start`. Rows past the item
/// height are clamped to the item when the target is placed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemFocusTarget<K, I> {
    /// Key of the item containing the target.
    pub key: K,
    /// Caller-owned target identity, stable across reflow.
    pub id: I,
    /// Item rows the target occupies.
    pub rows: Range<u64>,
    /// Enclosing target, if any.
    pub parent: Option<I>,
}

impl<K, I> ItemFocusTarget<K, I> {
    /// Create a top-level target occupying `rows` of item `key`.
    #[must_use]
    pub const fn new(key: K, id: I, rows: Range<u64>) -> Self {
        Self {
            key,
            id,
            rows,
            parent: None,
        }
    }

    /// Set the enclosing target.
    #[must_use]
    pub fn parent(mut self, parent: I) -> Self {
        self.parent = Some(parent);
        self
    }
}

impl<K: Clone + Ord, I> ItemFocusTarget<K, I> {
    /// Collection rows of this target in `index`, clamped to its item, or
    /// `None` when the item is not indexed. Never empty unless the item has
    /// zero height.
    #[must_use]
    pub fn list_rows(&self, index: &MeasuredListIndex<K>) -> Option<Range<u64>> {
        let position = index.index_of(&self.key)?;
        let start = index.item_offset(position)?;
        let height = index.item(position)?.height;
        if height == 0 {
            return Some(start..start);
        }
        let first = self.rows.start.min(height - 1);
        let end = self.rows.end.clamp(first + 1, height);
        Some(start + first..start + end)
    }
}

/// Focus targets of indexed items in list order.
///
/// Targets are ordered by item position, then by first row, with an
/// enclosing target (same first row, more rows) before the targets it
/// contains; equal targets keep their supplied order. Construction drops
/// targets whose item is not indexed and every repeat of an identity after
/// its first occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusTraversal<K, I> {
    targets: Vec<ItemFocusTarget<K, I>>,
    positions: BTreeMap<I, usize>,
}

impl<K, I> Default for FocusTraversal<K, I> {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            positions: BTreeMap::new(),
        }
    }
}

impl<K: Clone + Ord, I: Clone + Ord> FocusTraversal<K, I> {
    /// Order `targets` against `index` (`O(t log t + t log n)` for `t`
    /// targets and `n` items).
    #[must_use]
    pub fn new(
        targets: impl IntoIterator<Item = ItemFocusTarget<K, I>>,
        index: &MeasuredListIndex<K>,
    ) -> Self {
        let mut ordered: Vec<(usize, ItemFocusTarget<K, I>)> = targets
            .into_iter()
            .filter_map(|target| Some((index.index_of(&target.key)?, target)))
            .collect();
        ordered.sort_by_key(|(position, target)| {
            (*position, target.rows.start, Reverse(target.rows.end))
        });
        let mut traversal = Self::default();
        for (_, target) in ordered {
            if traversal.positions.contains_key(&target.id) {
                continue;
            }
            traversal
                .positions
                .insert(target.id.clone(), traversal.targets.len());
            traversal.targets.push(target);
        }
        traversal
    }

    /// Number of targets.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.targets.len()
    }

    /// Whether there are no targets.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Targets in traversal order.
    #[must_use]
    pub fn targets(&self) -> &[ItemFocusTarget<K, I>] {
        &self.targets
    }

    /// Traversal position of `id`.
    #[must_use]
    pub fn position(&self, id: &I) -> Option<usize> {
        self.positions.get(id).copied()
    }

    /// The target identified by `id`.
    #[must_use]
    pub fn get(&self, id: &I) -> Option<&ItemFocusTarget<K, I>> {
        self.targets.get(self.position(id)?)
    }

    /// The first target.
    #[must_use]
    pub fn first(&self) -> Option<&ItemFocusTarget<K, I>> {
        self.targets.first()
    }

    /// The last target.
    #[must_use]
    pub fn last(&self) -> Option<&ItemFocusTarget<K, I>> {
        self.targets.last()
    }

    /// The target after `id`; `None` at the end or for an unknown `id`.
    #[must_use]
    pub fn next(&self, id: &I) -> Option<&ItemFocusTarget<K, I>> {
        self.targets.get(self.position(id)? + 1)
    }

    /// The target before `id`; `None` at the start or for an unknown `id`.
    #[must_use]
    pub fn previous(&self, id: &I) -> Option<&ItemFocusTarget<K, I>> {
        self.targets.get(self.position(id)?.checked_sub(1)?)
    }

    /// The enclosing target of `id`, when it is part of this traversal.
    #[must_use]
    pub fn parent(&self, id: &I) -> Option<&ItemFocusTarget<K, I>> {
        self.get(self.get(id)?.parent.as_ref()?)
    }

    /// Targets whose collection rows intersect `rows`, in traversal order.
    pub fn visible<'a>(
        &'a self,
        index: &'a MeasuredListIndex<K>,
        rows: Range<u64>,
    ) -> impl Iterator<Item = &'a ItemFocusTarget<K, I>> + 'a {
        self.targets.iter().filter(move |target| {
            target
                .list_rows(index)
                .is_some_and(|own| own.start < rows.end && rows.start < own.end)
        })
    }

    /// The last target starting at or before collection row `row`, for
    /// choosing the nearest target above a viewport position. Binary search,
    /// `O(log t · log n)`.
    #[must_use]
    pub fn last_starting_at_or_before(
        &self,
        index: &MeasuredListIndex<K>,
        row: u64,
    ) -> Option<&ItemFocusTarget<K, I>> {
        let after = self.targets.partition_point(|target| {
            target
                .list_rows(index)
                .is_some_and(|rows| rows.start <= row)
        });
        self.targets.get(after.checked_sub(1)?)
    }

    /// The target to keep focused after this traversal replaced `previous`.
    ///
    /// Returns `id` itself when it survived. Otherwise returns the surviving
    /// target nearest to `id` in `previous` order, preferring the earlier one
    /// on a tie, so focus moves to a neighbour when its target leaves the
    /// list (for example, by eviction). Returns `None` when `id` was not in
    /// `previous` or no target of `previous` survived.
    #[must_use]
    pub fn surviving(&self, previous: &Self, id: &I) -> Option<&ItemFocusTarget<K, I>> {
        if let Some(target) = self.get(id) {
            return Some(target);
        }
        let origin = previous.position(id)?;
        let mut before = previous.targets[..origin].iter().rev();
        let mut after = previous.targets[origin + 1..].iter();
        loop {
            let earlier = before.next();
            let later = after.next();
            if earlier.is_none() && later.is_none() {
                return None;
            }
            for candidate in [earlier, later].into_iter().flatten() {
                if let Some(target) = self.get(&candidate.id) {
                    return Some(target);
                }
            }
        }
    }
}

/// Top offset that reveals `target` approaching from `edge`, clamped to
/// `0..=max_overscroll_top`, or `None` when its item is not indexed.
///
/// Uses [`AnchorPolicy::EnsureVisibleFrom`] on the target's rows: no
/// movement when the target is fully visible (or covers the whole viewport);
/// otherwise `Edge::Top` puts the target's first row on the viewport top and
/// `Edge::Bottom` puts its last row on the viewport bottom, except that a
/// target taller than the viewport keeps its first row on the viewport top.
#[must_use]
pub fn reveal_top_offset<K: Clone + Ord, I>(
    index: &MeasuredListIndex<K>,
    target: &ItemFocusTarget<K, I>,
    edge: Edge,
    current_top: u64,
    geometry: ListGeometry,
) -> Option<u64> {
    reveal_top_offset_below(index, target, edge, current_top, geometry, 0)
}

/// [`reveal_top_offset`] below `inset` covered viewport rows.
///
/// The covered rows (for example, pinned section headers) do not count as
/// visible: a target under them is revealed, and `Edge::Top` (or a target
/// taller than the uncovered rows) puts its first row directly below them.
/// `inset` is limited to `viewport_height - 1`. Returns `None` when the
/// target's item is not indexed.
#[must_use]
pub fn reveal_top_offset_below<K: Clone + Ord, I>(
    index: &MeasuredListIndex<K>,
    target: &ItemFocusTarget<K, I>,
    edge: Edge,
    current_top: u64,
    geometry: ListGeometry,
    inset: u64,
) -> Option<u64> {
    let rows = target.list_rows(index)?;
    let top = place_rows_below(
        rows.start,
        rows.end - rows.start,
        AnchorPolicy::EnsureVisibleFrom(edge),
        current_top,
        geometry.viewport_height,
        inset,
    );
    Some(top.min(geometry.max_overscroll_top(index.total_height())))
}

/// Keyed position revealing `target` from `edge` (see [`reveal_top_offset`]).
///
/// Returns `None` when the target's item is not indexed. The result is
/// anchored at the item on the revealed top row and never
/// [`ListPosition::FollowBottom`]; prefer
/// [`VirtualListState::reveal_for_focus`], which keeps the current position
/// when no movement is needed.
#[must_use]
pub fn reveal_for_focus<K: Clone + Ord, S, I>(
    index: &MeasuredListIndex<K>,
    target: &ItemFocusTarget<K, I>,
    edge: Edge,
    current_top: u64,
    geometry: ListGeometry,
    sources: &dyn SourceMap<K, S>,
) -> Option<ListPosition<K, S>> {
    let top = reveal_top_offset(index, target, edge, current_top, geometry)?;
    Some(position_at_offset(index, top, geometry, sources))
}

impl<K: Clone + Ord, S: Clone> VirtualListState<K, S> {
    /// Position that reveals `target` approaching from `edge` (focus moving
    /// toward the end of the list approaches from [`Edge::Bottom`]), relative
    /// to the current position at `viewport_height`.
    ///
    /// Returns the current position unchanged (so `FollowBottom` is kept)
    /// when the target is already visible, and `None` when its item is not
    /// indexed. The list does not move; callers apply the result with
    /// [`Self::set_position`] or their own controlled navigation.
    #[must_use]
    pub fn reveal_for_focus<I>(
        &self,
        target: &ItemFocusTarget<K, I>,
        edge: Edge,
        viewport_height: u64,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<ListPosition<K, S>> {
        let geometry = self.geometry(viewport_height);
        let current = self
            .resolve(viewport_height, sources)
            .map_or(0, |resolved| resolved.top_offset);
        let top = reveal_top_offset(&self.index, target, edge, current, geometry)?;
        Some(if top == current {
            self.position.clone()
        } else {
            position_at_offset(&self.index, top, geometry, sources)
        })
    }
}

#[cfg(test)]
mod tests;
