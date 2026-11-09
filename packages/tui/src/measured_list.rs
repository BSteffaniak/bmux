//! Keyed variable-height collection index.
//!
//! [`MeasuredListIndex`] owns exact logical item heights in collection order
//! and answers prefix-height, item-offset, offset-to-item, and key-to-index
//! queries in logarithmic time. Membership changes (insert, prepend, append,
//! remove, splice) cost `O(k + log n)` tree work for `k` changed items plus
//! `O(k log n)` key bookkeeping, so a long collection never needs a full
//! rebuild for a local change.
//!
//! Rendering policy, item component ownership, scroll positions, and paging
//! policy remain in higher-level collection components and their callers.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

const NIL: usize = usize::MAX;

#[cfg(test)]
thread_local! {
    static DESCENT_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One retained variable-height item measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredListItem<K> {
    /// Stable caller-owned item identity.
    pub key: K,
    /// Caller-owned layout revision.
    pub layout_revision: u64,
    /// Width at which `height` was measured.
    pub width: u64,
    /// Geometry-affecting terminal capability revision at measurement time.
    pub capability_revision: u64,
    /// Exact logical item height, excluding collection gap.
    pub height: u64,
}

impl<K> MeasuredListItem<K> {
    /// Create one exact item measurement.
    #[must_use]
    pub const fn new(
        key: K,
        layout_revision: u64,
        width: u64,
        capability_revision: u64,
        height: u64,
    ) -> Self {
        Self {
            key,
            layout_revision,
            width,
            capability_revision,
            height,
        }
    }

    /// Whether this measurement is valid for a layout revision, width, and
    /// capability revision.
    #[must_use]
    pub const fn is_valid_for(
        &self,
        layout_revision: u64,
        width: u64,
        capability_revision: u64,
    ) -> bool {
        self.layout_revision == layout_revision
            && self.width == width
            && self.capability_revision == capability_revision
    }
}

/// Visible item range and boundary offsets for one logical viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleItemRange {
    /// First intersecting item index, inclusive.
    pub start: usize,
    /// Last intersecting item index, exclusive.
    pub end: usize,
    /// Logical row clipped from the first item's top.
    pub first_item_offset: u64,
}

/// Rejected incremental [`MeasuredListIndex`] mutation. A rejected mutation
/// leaves the index unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeasuredListError {
    /// A supplied item key is repeated within the batch or already indexed
    /// outside the replaced range.
    DuplicateKey {
        /// Zero-based position of the offending item within the supplied batch.
        position: usize,
    },
    /// The requested index range is reversed or extends past the collection.
    OutOfBounds {
        /// Requested range start.
        start: usize,
        /// Requested range end (exclusive).
        end: usize,
        /// Collection length at the time of the request.
        len: usize,
    },
}

impl fmt::Display for MeasuredListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateKey { position } => write!(
                f,
                "measured-list item at batch position {position} has a duplicate key"
            ),
            Self::OutOfBounds { start, end, len } => write!(
                f,
                "measured-list range {start}..{end} is out of bounds for length {len}"
            ),
        }
    }
}

impl std::error::Error for MeasuredListError {}

#[derive(Debug, Clone)]
struct Node<K> {
    item: MeasuredListItem<K>,
    priority: u64,
    left: usize,
    right: usize,
    parent: usize,
    /// Number of items in this subtree.
    size: usize,
    /// Saturating sum of item heights in this subtree (gaps excluded).
    heights: u64,
}

/// Retained keyed index for exact variable-height item geometry.
///
/// Items are stored in an order-statistic treap ordered by position, with
/// parent links so a stable key resolves to its current index in logarithmic
/// time after any splice. Gaps are applied arithmetically, so changing the gap
/// is constant time. Arena slots are compacted on removal, so memory tracks
/// the current length.
///
/// Equality compares logical content (gap and items in order), not internal
/// tree shape.
#[derive(Clone)]
pub struct MeasuredListIndex<K> {
    nodes: Vec<Node<K>>,
    root: usize,
    key_to_node: BTreeMap<K, usize>,
    gap: u64,
    seed: u64,
}

impl<K> MeasuredListIndex<K>
where
    K: Clone + Ord,
{
    /// Create an empty measured collection with a logical inter-item gap.
    #[must_use]
    pub const fn new(gap: u64) -> Self {
        Self {
            nodes: Vec::new(),
            root: NIL,
            key_to_node: BTreeMap::new(),
            gap,
            seed: 0x9e37_79b9_7f4a_7c15,
        }
    }

    /// Create a collection from exact measurements in order.
    ///
    /// # Errors
    ///
    /// * [`MeasuredListError::DuplicateKey`] when two items share a key.
    pub fn from_items(
        gap: u64,
        items: impl IntoIterator<Item = MeasuredListItem<K>>,
    ) -> Result<Self, MeasuredListError> {
        let mut index = Self::new(gap);
        index.append(items)?;
        Ok(index)
    }

    /// Synchronize item order while retaining exact measurements for matching
    /// key, layout revision, width, and capability revision.
    ///
    /// The callback is invoked only for new or invalidated items, in collection
    /// order. The common unchanged prefix and suffix are retained in place and
    /// only the differing middle is spliced, so appends, prepends, and local
    /// edits do not rebuild the index. Comparing the supplied entries is still
    /// `O(n)`; use the incremental methods when the change is already known.
    ///
    /// # Panics
    ///
    /// Panics when the synchronized collection contains duplicate stable keys.
    pub fn sync(
        &mut self,
        entries: impl IntoIterator<Item = (K, u64)>,
        width: u64,
        capability_revision: u64,
        mut measure: impl FnMut(&K) -> u64,
    ) {
        let entries = entries.into_iter().collect::<Vec<_>>();
        {
            let mut seen = BTreeSet::new();
            for (key, _) in &entries {
                assert!(seen.insert(key), "measured-list keys must be unique");
            }
        }
        let matches = |item: &MeasuredListItem<K>, (key, revision): &(K, u64)| {
            item.key == *key && item.is_valid_for(*revision, width, capability_revision)
        };
        let (prefix, suffix) = {
            let current = self.iter().collect::<Vec<_>>();
            let prefix = current
                .iter()
                .zip(&entries)
                .take_while(|(item, entry)| matches(item, entry))
                .count();
            let limit = current.len().min(entries.len()) - prefix;
            let suffix = current
                .iter()
                .rev()
                .zip(entries.iter().rev())
                .take(limit)
                .take_while(|(item, entry)| matches(item, entry))
                .count();
            (prefix, suffix)
        };
        let old_end = self.len() - suffix;
        let new_end = entries.len() - suffix;
        if prefix == old_end && prefix == new_end {
            return;
        }
        let mut retained = self
            .detach_range(prefix..old_end)
            .into_iter()
            .map(|item| (item.key.clone(), item))
            .collect::<BTreeMap<_, _>>();
        let inserted = entries
            .into_iter()
            .take(new_end)
            .skip(prefix)
            .map(|(key, layout_revision)| {
                retained
                    .remove(&key)
                    .filter(|item| item.is_valid_for(layout_revision, width, capability_revision))
                    .unwrap_or_else(|| {
                        let height = measure(&key);
                        MeasuredListItem::new(
                            key,
                            layout_revision,
                            width,
                            capability_revision,
                            height,
                        )
                    })
            })
            .collect::<Vec<_>>();
        self.attach_at(prefix, inserted);
    }

    /// Insert exact measurements before index `at`.
    ///
    /// # Errors
    ///
    /// * [`MeasuredListError::OutOfBounds`] when `at > len()`.
    /// * [`MeasuredListError::DuplicateKey`] when an inserted key repeats within
    ///   the batch or is already indexed.
    pub fn insert(
        &mut self,
        at: usize,
        items: impl IntoIterator<Item = MeasuredListItem<K>>,
    ) -> Result<(), MeasuredListError> {
        self.splice(at..at, items).map(drop)
    }

    /// Insert exact measurements before the first item.
    ///
    /// # Errors
    ///
    /// * [`MeasuredListError::DuplicateKey`] when an inserted key repeats within
    ///   the batch or is already indexed.
    pub fn prepend(
        &mut self,
        items: impl IntoIterator<Item = MeasuredListItem<K>>,
    ) -> Result<(), MeasuredListError> {
        self.insert(0, items)
    }

    /// Insert exact measurements after the last item.
    ///
    /// # Errors
    ///
    /// * [`MeasuredListError::DuplicateKey`] when an inserted key repeats within
    ///   the batch or is already indexed.
    pub fn append(
        &mut self,
        items: impl IntoIterator<Item = MeasuredListItem<K>>,
    ) -> Result<(), MeasuredListError> {
        self.insert(self.len(), items)
    }

    /// Remove an index range, returning the removed measurements in order.
    ///
    /// # Errors
    ///
    /// * [`MeasuredListError::OutOfBounds`] when the range is reversed or
    ///   extends past `len()`.
    pub fn remove_range(
        &mut self,
        range: Range<usize>,
    ) -> Result<Vec<MeasuredListItem<K>>, MeasuredListError> {
        self.splice(range, std::iter::empty())
    }

    /// Remove one item by stable key, returning its measurement.
    pub fn remove(&mut self, key: &K) -> Option<MeasuredListItem<K>> {
        let index = self.index_of(key)?;
        self.detach_range(index..index + 1).pop()
    }

    /// Replace an index range with exact measurements, returning the removed
    /// measurements in order. Keys removed by the range may be reinserted.
    ///
    /// Costs `O(r + k + log n)` tree work plus `O((r + k) log n)` key
    /// bookkeeping for `r` removed and `k` inserted items.
    ///
    /// # Errors
    ///
    /// * [`MeasuredListError::OutOfBounds`] when the range is reversed or
    ///   extends past `len()`.
    /// * [`MeasuredListError::DuplicateKey`] when an inserted key repeats within
    ///   the batch or is indexed outside the replaced range.
    pub fn splice(
        &mut self,
        range: Range<usize>,
        items: impl IntoIterator<Item = MeasuredListItem<K>>,
    ) -> Result<Vec<MeasuredListItem<K>>, MeasuredListError> {
        let len = self.len();
        if range.start > range.end || range.end > len {
            return Err(MeasuredListError::OutOfBounds {
                start: range.start,
                end: range.end,
                len,
            });
        }
        let items = items.into_iter().collect::<Vec<_>>();
        {
            let mut batch = BTreeSet::new();
            for (position, item) in items.iter().enumerate() {
                let outside_range = self
                    .index_of(&item.key)
                    .is_some_and(|index| !range.contains(&index));
                if !batch.insert(&item.key) || outside_range {
                    return Err(MeasuredListError::DuplicateKey { position });
                }
            }
        }
        let start = range.start;
        let removed = self.detach_range(range);
        self.attach_at(start, items);
        Ok(removed)
    }

    /// Alias for [`Self::splice`] that names the replace-range operation.
    ///
    /// # Errors
    ///
    /// Same as [`Self::splice`].
    pub fn replace_range(
        &mut self,
        range: Range<usize>,
        items: impl IntoIterator<Item = MeasuredListItem<K>>,
    ) -> Result<Vec<MeasuredListItem<K>>, MeasuredListError> {
        self.splice(range, items)
    }

    /// Update one item's exact height in logarithmic time.
    pub fn update_height(&mut self, key: &K, height: u64) -> bool {
        let Some(node) = self.key_to_node.get(key).copied() else {
            return false;
        };
        self.nodes[node].item.height = height;
        self.refresh_ancestors(node);
        true
    }

    /// Replace the stored measurement for `item.key` (layout revision, width,
    /// capability revision, and height) in logarithmic time.
    ///
    /// Returns `false` without change when the key is not indexed.
    pub fn update_item(&mut self, item: MeasuredListItem<K>) -> bool {
        let Some(node) = self.key_to_node.get(&item.key).copied() else {
            return false;
        };
        self.nodes[node].item = item;
        self.refresh_ancestors(node);
        true
    }

    /// Re-measure every item whose width or capability revision differs from
    /// the supplied values, in collection order, and return how many were
    /// re-measured. Layout revisions are retained.
    ///
    /// This is the width and environment invalidation path; it is `O(n)`
    /// because a width change can affect every item.
    pub fn remeasure(
        &mut self,
        width: u64,
        capability_revision: u64,
        mut measure: impl FnMut(&K) -> u64,
    ) -> usize {
        let order = self.inorder_nodes(self.root);
        let mut count = 0usize;
        for node in order {
            let item = &mut self.nodes[node].item;
            if item.width != width || item.capability_revision != capability_revision {
                item.height = measure(&item.key);
                item.width = width;
                item.capability_revision = capability_revision;
                count += 1;
            }
        }
        if count > 0 {
            self.pull_all(self.root);
        }
        count
    }

    /// Change the logical gap after each item except the final item. Constant
    /// time.
    pub const fn set_gap(&mut self, gap: u64) {
        self.gap = gap;
    }

    /// Whether no items are indexed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.root == NIL
    }

    /// Logical gap after each indexed item except the final item.
    #[must_use]
    pub const fn gap(&self) -> u64 {
        self.gap
    }

    /// Indexed item at `index`, in logarithmic time.
    #[must_use]
    pub fn item(&self, index: usize) -> Option<&MeasuredListItem<K>> {
        self.node_at(index).map(|node| &self.nodes[node].item)
    }

    /// Indexed item for a stable key.
    #[must_use]
    pub fn item_by_key(&self, key: &K) -> Option<&MeasuredListItem<K>> {
        self.key_to_node
            .get(key)
            .map(|node| &self.nodes[*node].item)
    }

    /// Whether a stable key is indexed.
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.key_to_node.contains_key(key)
    }

    /// Current index for a stable key, in logarithmic time.
    #[must_use]
    pub fn index_of(&self, key: &K) -> Option<usize> {
        self.key_to_node
            .get(key)
            .map(|node| self.position_of(*node).0)
    }

    /// Logical start row for a stable key, in logarithmic time.
    #[must_use]
    pub fn key_offset(&self, key: &K) -> Option<u64> {
        self.key_to_node.get(key).map(|node| {
            let (index, heights) = self.position_of(*node);
            self.extent(index, heights)
        })
    }

    /// Logical start row for one item.
    #[must_use]
    pub fn item_offset(&self, index: usize) -> Option<u64> {
        if index >= self.len() {
            return None;
        }
        let mut node = self.root;
        let mut remaining = index;
        let mut heights = 0u64;
        while node != NIL {
            let current = &self.nodes[node];
            let left_size = self.size(current.left);
            if remaining < left_size {
                node = current.left;
                continue;
            }
            heights = heights.saturating_add(self.heights(current.left));
            if remaining == left_size {
                break;
            }
            heights = heights.saturating_add(current.item.height);
            remaining -= left_size + 1;
            node = current.right;
        }
        Some(self.extent(index, heights))
    }

    /// Total logical collection height, excluding a trailing gap.
    #[must_use]
    pub fn total_height(&self) -> u64 {
        let len = self.len();
        self.extent(len, self.heights(self.root))
            .saturating_sub(if len == 0 { 0 } else { self.gap })
    }

    /// Find the item containing one logical row. Rows in an inter-item gap map
    /// to the preceding item so viewport projection remains monotonic.
    /// Zero-height items without a gap occupy no rows and are never returned.
    #[must_use]
    pub fn item_at_offset(&self, offset: u64) -> Option<usize> {
        if self.is_empty() || offset >= self.total_height() {
            return None;
        }
        let mut node = self.root;
        let mut target = offset;
        let mut index = 0usize;
        while node != NIL {
            #[cfg(test)]
            DESCENT_STEPS.with(|steps| steps.set(steps.get() + 1));
            let current = &self.nodes[node];
            let left_size = self.size(current.left);
            let left_extent = self.extent(left_size, self.heights(current.left));
            if target < left_extent {
                node = current.left;
                continue;
            }
            target -= left_extent;
            let own = current.item.height.saturating_add(self.gap);
            if target < own {
                return Some(index + left_size);
            }
            target -= own;
            index += left_size + 1;
            node = current.right;
        }
        // Unreachable for in-range offsets; keep projection monotonic anyway.
        Some(self.len() - 1)
    }

    /// Find items intersecting a logical viewport.
    #[must_use]
    pub fn visible_range(&self, offset: u64, viewport_height: u64) -> VisibleItemRange {
        let total = self.total_height();
        if self.is_empty() || viewport_height == 0 || offset >= total {
            return VisibleItemRange {
                start: 0,
                end: 0,
                first_item_offset: 0,
            };
        }
        let start = self.item_at_offset(offset).unwrap_or(0);
        let start_offset = self.item_offset(start).unwrap_or(0);
        let viewport_end = offset.saturating_add(viewport_height);
        let end = self
            .item_at_offset(viewport_end.saturating_sub(1).min(total - 1))
            .map_or(self.len(), |index| index.saturating_add(1));
        VisibleItemRange {
            start,
            end,
            first_item_offset: offset.saturating_sub(start_offset),
        }
    }

    /// Iterate stable keys in collection order.
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &K> + '_ {
        self.iter().map(|item| &item.key)
    }

    fn heights(&self, node: usize) -> u64 {
        if node == NIL {
            0
        } else {
            self.nodes[node].heights
        }
    }

    /// Logical extent of `count` items whose heights sum to `heights`, each
    /// followed by one gap.
    fn extent(&self, count: usize, heights: u64) -> u64 {
        heights.saturating_add(
            self.gap
                .saturating_mul(u64::try_from(count).unwrap_or(u64::MAX)),
        )
    }

    fn node_at(&self, index: usize) -> Option<usize> {
        let mut node = self.root;
        let mut remaining = index;
        while node != NIL {
            let current = &self.nodes[node];
            let left_size = self.size(current.left);
            match remaining.cmp(&left_size) {
                Ordering::Less => node = current.left,
                Ordering::Equal => return Some(node),
                Ordering::Greater => {
                    remaining -= left_size + 1;
                    node = current.right;
                }
            }
        }
        None
    }

    /// Index and summed heights of all preceding items for one node.
    fn position_of(&self, node: usize) -> (usize, u64) {
        let left = self.nodes[node].left;
        let mut index = self.size(left);
        let mut heights = self.heights(left);
        let mut child = node;
        let mut parent = self.nodes[node].parent;
        while parent != NIL {
            let current = &self.nodes[parent];
            if current.right == child {
                index += self.size(current.left) + 1;
                heights = heights
                    .saturating_add(self.heights(current.left))
                    .saturating_add(current.item.height);
            }
            child = parent;
            parent = current.parent;
        }
        (index, heights)
    }

    const fn next_priority(&mut self) -> u64 {
        // SplitMix64: deterministic, well-distributed treap priorities.
        self.seed = self.seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.seed;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn pull(&mut self, node: usize) {
        let (left, right) = (self.nodes[node].left, self.nodes[node].right);
        let size = self.size(left) + self.size(right) + 1;
        let heights = self
            .heights(left)
            .saturating_add(self.heights(right))
            .saturating_add(self.nodes[node].item.height);
        let current = &mut self.nodes[node];
        current.size = size;
        current.heights = heights;
        if left != NIL {
            self.nodes[left].parent = node;
        }
        if right != NIL {
            self.nodes[right].parent = node;
        }
    }

    fn pull_all(&mut self, node: usize) {
        if node == NIL {
            return;
        }
        self.pull_all(self.nodes[node].left);
        self.pull_all(self.nodes[node].right);
        self.pull(node);
    }

    fn refresh_ancestors(&mut self, node: usize) {
        let mut cursor = node;
        while cursor != NIL {
            self.pull(cursor);
            cursor = self.nodes[cursor].parent;
        }
    }

    fn merge(&mut self, left: usize, right: usize) -> usize {
        if left == NIL {
            return right;
        }
        if right == NIL {
            return left;
        }
        if self.nodes[left].priority > self.nodes[right].priority {
            let merged = self.merge(self.nodes[left].right, right);
            self.nodes[left].right = merged;
            self.pull(left);
            left
        } else {
            let merged = self.merge(left, self.nodes[right].left);
            self.nodes[right].left = merged;
            self.pull(right);
            right
        }
    }

    /// Split a subtree into its first `count` items and the remainder. Both
    /// returned roots have no parent.
    fn split(&mut self, node: usize, count: usize) -> (usize, usize) {
        if node == NIL {
            return (NIL, NIL);
        }
        let left = self.nodes[node].left;
        let left_size = self.size(left);
        let (first, rest) = if count <= left_size {
            let (first, rest) = self.split(left, count);
            self.nodes[node].left = rest;
            self.pull(node);
            (first, node)
        } else {
            let (first, rest) = self.split(self.nodes[node].right, count - left_size - 1);
            self.nodes[node].right = first;
            self.pull(node);
            (node, rest)
        };
        for root in [first, rest] {
            if root != NIL {
                self.nodes[root].parent = NIL;
            }
        }
        (first, rest)
    }

    /// Build a detached subtree from items in order in linear time and register
    /// their keys. Callers must have validated key uniqueness.
    fn build(&mut self, items: Vec<MeasuredListItem<K>>) -> usize {
        let mut stack: Vec<usize> = Vec::new();
        for item in items {
            let node = self.nodes.len();
            let priority = self.next_priority();
            self.key_to_node.insert(item.key.clone(), node);
            self.nodes.push(Node {
                item,
                priority,
                left: NIL,
                right: NIL,
                parent: NIL,
                size: 1,
                heights: 0,
            });
            let mut last = NIL;
            while let Some(&top) = stack.last() {
                if self.nodes[top].priority >= priority {
                    break;
                }
                last = top;
                stack.pop();
            }
            self.nodes[node].left = last;
            if let Some(&top) = stack.last() {
                self.nodes[top].right = node;
            }
            stack.push(node);
        }
        let Some(&root) = stack.first() else {
            return NIL;
        };
        self.pull_all(root);
        self.nodes[root].parent = NIL;
        root
    }

    /// Insert validated items at `at` and register their keys.
    fn attach_at(&mut self, at: usize, items: Vec<MeasuredListItem<K>>) {
        if items.is_empty() {
            return;
        }
        let (left, right) = self.split(self.root, at);
        let middle = self.build(items);
        let joined = self.merge(left, middle);
        self.root = self.merge(joined, right);
        self.nodes[self.root].parent = NIL;
    }

    /// Remove an in-bounds range, unregister its keys, release its arena slots,
    /// and return its items in order.
    fn detach_range(&mut self, range: Range<usize>) -> Vec<MeasuredListItem<K>> {
        if range.is_empty() {
            return Vec::new();
        }
        let (left, rest) = self.split(self.root, range.start);
        let (middle, right) = self.split(rest, range.len());
        self.root = self.merge(left, right);
        if self.root != NIL {
            self.nodes[self.root].parent = NIL;
        }
        let order = self.inorder_nodes(middle);
        for node in &order {
            self.key_to_node.remove(&self.nodes[*node].item.key);
        }
        let mut slots = order
            .iter()
            .enumerate()
            .map(|(position, node)| (*node, position))
            .collect::<Vec<_>>();
        slots.sort_unstable_by_key(|slot| std::cmp::Reverse(slot.0));
        let mut removed = std::iter::repeat_with(|| None)
            .take(order.len())
            .collect::<Vec<Option<MeasuredListItem<K>>>>();
        for (node, position) in slots {
            removed[position] = Some(self.release(node));
        }
        removed.into_iter().flatten().collect()
    }

    /// Remove one detached arena slot, relocating the last slot into it.
    /// Slots must be released in descending order so the relocated slot is
    /// always attached to the live tree.
    fn release(&mut self, node: usize) -> MeasuredListItem<K> {
        let last = self.nodes.len() - 1;
        let released = self.nodes.swap_remove(node);
        if node != last {
            let parent = self.nodes[node].parent;
            if parent == NIL {
                if self.root == last {
                    self.root = node;
                }
            } else if self.nodes[parent].left == last {
                self.nodes[parent].left = node;
            } else {
                self.nodes[parent].right = node;
            }
            for child in [self.nodes[node].left, self.nodes[node].right] {
                if child != NIL {
                    self.nodes[child].parent = node;
                }
            }
            if let Some(slot) = self.key_to_node.get_mut(&self.nodes[node].item.key) {
                *slot = node;
            }
        }
        released.item
    }

    fn inorder_nodes(&self, root: usize) -> Vec<usize> {
        let mut order = Vec::with_capacity(self.size(root));
        let mut stack = Vec::new();
        let mut node = root;
        loop {
            while node != NIL {
                stack.push(node);
                node = self.nodes[node].left;
            }
            let Some(next) = stack.pop() else {
                break;
            };
            order.push(next);
            node = self.nodes[next].right;
        }
        order
    }
}

impl<K> MeasuredListIndex<K> {
    /// Number of indexed items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.size(self.root)
    }

    /// Iterate indexed items in collection order.
    #[must_use]
    pub fn iter(&self) -> Iter<'_, K> {
        self.iter_from(0)
    }

    /// Iterate indexed items in collection order starting at `index`. Creating
    /// the iterator is logarithmic; each step is amortized constant time.
    #[must_use]
    pub fn iter_from(&self, index: usize) -> Iter<'_, K> {
        let mut stack = Vec::new();
        let mut node = self.root;
        let mut remaining = index;
        while node != NIL {
            let current = &self.nodes[node];
            let left_size = self.size(current.left);
            match remaining.cmp(&left_size) {
                Ordering::Less => {
                    stack.push(node);
                    node = current.left;
                }
                Ordering::Equal => {
                    stack.push(node);
                    break;
                }
                Ordering::Greater => {
                    remaining -= left_size + 1;
                    node = current.right;
                }
            }
        }
        Iter {
            index: self,
            stack,
            remaining: self.len().saturating_sub(index),
        }
    }

    fn size(&self, node: usize) -> usize {
        if node == NIL {
            0
        } else {
            self.nodes[node].size
        }
    }
}

impl<K> Default for MeasuredListIndex<K>
where
    K: Clone + Ord,
{
    fn default() -> Self {
        Self::new(0)
    }
}

impl<K> PartialEq for MeasuredListIndex<K>
where
    K: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.gap == other.gap && self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl<K> Eq for MeasuredListIndex<K> where K: Eq {}

impl<K> fmt::Debug for MeasuredListIndex<K>
where
    K: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MeasuredListIndex")
            .field("gap", &self.gap)
            .field("items", &self.iter().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl<'a, K> IntoIterator for &'a MeasuredListIndex<K> {
    type Item = &'a MeasuredListItem<K>;
    type IntoIter = Iter<'a, K>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// In-order iterator over [`MeasuredListIndex`] items.
pub struct Iter<'a, K> {
    index: &'a MeasuredListIndex<K>,
    stack: Vec<usize>,
    remaining: usize,
}

impl<'a, K> Iterator for Iter<'a, K> {
    type Item = &'a MeasuredListItem<K>;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        self.remaining = self.remaining.saturating_sub(1);
        let mut cursor = self.index.nodes[node].right;
        while cursor != NIL {
            self.stack.push(cursor);
            cursor = self.index.nodes[cursor].left;
        }
        Some(&self.index.nodes[node].item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K> ExactSizeIterator for Iter<'_, K> {}

impl<K> std::iter::FusedIterator for Iter<'_, K> {}

#[cfg(test)]
mod tests;
