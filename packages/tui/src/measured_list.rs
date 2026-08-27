//! Keyed variable-height collection index.
//!
//! The index owns logical item heights and a Fenwick tree for logarithmic
//! prefix-height, item-offset, and offset-to-item queries. Rendering policy and
//! item component ownership remain in higher-level collection components.

use std::collections::{BTreeMap, BTreeSet};

/// One retained variable-height item measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredListItem<K> {
    /// Stable caller-owned item identity.
    pub key: K,
    /// Caller-owned layout revision.
    pub layout_revision: u64,
    /// Width at which `height` was measured.
    pub width: u16,
    /// Exact logical item height, excluding collection gap.
    pub height: usize,
}

impl<K> MeasuredListItem<K> {
    /// Create one exact item measurement.
    #[must_use]
    pub const fn new(key: K, layout_revision: u64, width: u16, height: usize) -> Self {
        Self {
            key,
            layout_revision,
            width,
            height,
        }
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
    pub first_item_offset: usize,
}

/// Retained keyed index for exact variable-height item geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredListIndex<K> {
    items: Vec<MeasuredListItem<K>>,
    key_to_index: BTreeMap<K, usize>,
    tree: FenwickTree,
    gap: usize,
}

impl<K> MeasuredListIndex<K>
where
    K: Clone + Ord,
{
    /// Create an empty measured collection with a logical inter-item gap.
    #[must_use]
    pub fn new(gap: usize) -> Self {
        Self {
            items: Vec::new(),
            key_to_index: BTreeMap::new(),
            tree: FenwickTree::default(),
            gap,
        }
    }

    /// Synchronize item order while retaining exact measurements for matching
    /// key, layout revision, and width.
    ///
    /// The callback is invoked only for new or invalidated items.
    ///
    /// # Panics
    ///
    /// Panics when the synchronized collection contains duplicate stable keys.
    pub fn sync(
        &mut self,
        entries: impl IntoIterator<Item = (K, u64)>,
        width: u16,
        mut measure: impl FnMut(&K) -> usize,
    ) {
        let mut retained = std::mem::take(&mut self.items)
            .into_iter()
            .map(|item| (item.key.clone(), item))
            .collect::<BTreeMap<_, _>>();
        let mut seen = BTreeSet::new();
        let mut items = Vec::new();
        for (key, layout_revision) in entries {
            assert!(
                seen.insert(key.clone()),
                "measured-list keys must be unique"
            );
            let item = retained
                .remove(&key)
                .filter(|item| item.layout_revision == layout_revision && item.width == width);
            items.push(item.unwrap_or_else(|| {
                let height = measure(&key);
                MeasuredListItem::new(key, layout_revision, width, height)
            }));
        }
        self.items = items;
        self.rebuild_index();
    }

    /// Update one item's exact height in logarithmic time.
    pub fn update_height(&mut self, key: &K, height: usize) -> bool {
        let Some(index) = self.key_to_index.get(key).copied() else {
            return false;
        };
        let old_extent = self.item_extent(index);
        self.items[index].height = height;
        let new_extent = self.item_extent(index);
        self.tree.set(index, old_extent, new_extent);
        true
    }

    /// Number of indexed items.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no items are indexed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Logical gap after each indexed item except the final item.
    #[must_use]
    pub const fn gap(&self) -> usize {
        self.gap
    }

    /// Indexed item at `index`.
    #[must_use]
    pub fn item(&self, index: usize) -> Option<&MeasuredListItem<K>> {
        self.items.get(index)
    }

    /// Index for a stable key.
    #[must_use]
    pub fn index_of(&self, key: &K) -> Option<usize> {
        self.key_to_index.get(key).copied()
    }

    /// Logical start row for one item.
    #[must_use]
    pub fn item_offset(&self, index: usize) -> Option<usize> {
        (index < self.items.len()).then(|| self.tree.prefix_sum(index))
    }

    /// Total logical collection height, excluding a trailing gap.
    #[must_use]
    pub fn total_height(&self) -> usize {
        self.tree
            .total()
            .saturating_sub(if self.items.is_empty() { 0 } else { self.gap })
    }

    /// Find the item containing one logical row. Rows in an inter-item gap map
    /// to the preceding item so viewport projection remains monotonic.
    #[must_use]
    pub fn item_at_offset(&self, offset: usize) -> Option<usize> {
        if self.items.is_empty() || offset >= self.total_height() {
            return None;
        }
        Some(self.tree.upper_bound(offset).min(self.items.len() - 1))
    }

    /// Find items intersecting a logical viewport.
    #[must_use]
    pub fn visible_range(&self, offset: usize, viewport_height: usize) -> VisibleItemRange {
        if self.items.is_empty() || viewport_height == 0 || offset >= self.total_height() {
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
            .item_at_offset(viewport_end.saturating_sub(1).min(self.total_height() - 1))
            .map_or(self.items.len(), |index| index.saturating_add(1));
        VisibleItemRange {
            start,
            end,
            first_item_offset: offset.saturating_sub(start_offset),
        }
    }

    fn item_extent(&self, index: usize) -> usize {
        self.items[index].height.saturating_add(self.gap)
    }

    fn rebuild_index(&mut self) {
        self.key_to_index = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| (item.key.clone(), index))
            .collect();
        self.tree = FenwickTree::from_values(
            self.items
                .iter()
                .map(|item| item.height.saturating_add(self.gap)),
        );
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FenwickTree {
    values: Vec<usize>,
    tree: Vec<usize>,
}

impl FenwickTree {
    fn from_values(values: impl IntoIterator<Item = usize>) -> Self {
        let values = values.into_iter().collect::<Vec<_>>();
        let mut result = Self {
            tree: vec![0; values.len().saturating_add(1)],
            values: vec![0; values.len()],
        };
        for (index, value) in values.into_iter().enumerate() {
            result.set(index, 0, value);
        }
        result
    }

    fn set(&mut self, index: usize, old: usize, new: usize) {
        if index >= self.values.len() {
            return;
        }
        self.values[index] = new;
        let increase = new >= old;
        let difference = new.abs_diff(old);
        let mut cursor = index.saturating_add(1);
        while cursor < self.tree.len() {
            self.tree[cursor] = if increase {
                self.tree[cursor].saturating_add(difference)
            } else {
                self.tree[cursor].saturating_sub(difference)
            };
            cursor = cursor.saturating_add(cursor & cursor.wrapping_neg());
        }
    }

    fn prefix_sum(&self, end: usize) -> usize {
        let mut cursor = end.min(self.values.len());
        let mut total = 0usize;
        while cursor > 0 {
            total = total.saturating_add(self.tree[cursor]);
            cursor &= cursor - 1;
        }
        total
    }

    fn total(&self) -> usize {
        self.prefix_sum(self.values.len())
    }

    /// Return the first value index whose inclusive prefix exceeds `target`.
    fn upper_bound(&self, target: usize) -> usize {
        let mut index = 0usize;
        let mut accumulated = 0usize;
        let mut step = if self.values.is_empty() {
            0
        } else {
            1usize << self.values.len().ilog2()
        };
        while step > 0 {
            let next = index.saturating_add(step);
            if next < self.tree.len() && accumulated.saturating_add(self.tree[next]) <= target {
                index = next;
                accumulated = accumulated.saturating_add(self.tree[next]);
            }
            step >>= 1;
        }
        index.min(self.values.len().saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::MeasuredListIndex;

    #[test]
    fn synchronizes_by_stable_key_revision_and_width() {
        let mut index = MeasuredListIndex::new(1);
        let mut measured = Vec::new();
        index.sync([("a", 1), ("b", 1)], 10, |key| {
            measured.push(*key);
            if *key == "a" { 2 } else { 3 }
        });
        index.sync([("b", 1), ("a", 1)], 10, |key| {
            measured.push(*key);
            9
        });
        assert_eq!(measured, ["a", "b"]);
        assert_eq!(index.item(0).map(|item| item.height), Some(3));
        index.sync([("b", 2), ("a", 1)], 10, |key| {
            measured.push(*key);
            4
        });
        assert_eq!(measured, ["a", "b", "b"]);
    }

    #[test]
    fn reports_offsets_total_height_and_visible_boundaries() {
        let mut index = MeasuredListIndex::new(1);
        index.sync([("a", 0), ("b", 0), ("c", 0)], 8, |key| match *key {
            "a" => 2,
            "b" => 4,
            _ => 3,
        });
        assert_eq!(index.item_offset(0), Some(0));
        assert_eq!(index.item_offset(1), Some(3));
        assert_eq!(index.item_offset(2), Some(8));
        assert_eq!(index.total_height(), 11);
        assert_eq!(index.item_at_offset(0), Some(0));
        assert_eq!(index.item_at_offset(3), Some(1));
        let visible = index.visible_range(4, 5);
        assert_eq!(visible.start, 1);
        assert_eq!(visible.end, 3);
        assert_eq!(visible.first_item_offset, 1);
    }

    #[test]
    fn randomized_mutations_match_naive_prefix_geometry() {
        let mut seed = 0x5eed_u64;
        let mut keys = (0_u32..20).collect::<Vec<_>>();
        let mut heights = keys
            .iter()
            .map(|key| (*key, usize::try_from(*key % 5 + 1).unwrap()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut index = MeasuredListIndex::new(2);

        for _ in 0..500 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            match seed % 4 {
                0 if !keys.is_empty() => {
                    let position = usize::try_from(seed >> 32).unwrap_or(0) % keys.len();
                    keys.remove(position);
                }
                1 => {
                    let key = u32::try_from(seed >> 32).unwrap_or(u32::MAX);
                    heights.entry(key).or_insert_with(|| {
                        let position = if keys.is_empty() {
                            0
                        } else {
                            usize::try_from(seed).unwrap_or(0) % (keys.len() + 1)
                        };
                        keys.insert(position, key);
                        usize::try_from(key % 7 + 1).unwrap()
                    });
                }
                2 if keys.len() > 1 => {
                    let left = usize::try_from(seed >> 32).unwrap_or(0) % keys.len();
                    let right = usize::try_from(seed).unwrap_or(0) % keys.len();
                    keys.swap(left, right);
                }
                _ if !keys.is_empty() => {
                    let position = usize::try_from(seed >> 32).unwrap_or(0) % keys.len();
                    let key = keys[position];
                    let height = usize::try_from(seed % 9 + 1).unwrap();
                    heights.insert(key, height);
                }
                _ => {}
            }

            index.sync(
                keys.iter().map(|key| (*key, heights[key] as u64)),
                20,
                |key| heights[key],
            );
            let mut expected_offset = 0usize;
            for (position, key) in keys.iter().enumerate() {
                assert_eq!(index.index_of(key), Some(position));
                assert_eq!(index.item_offset(position), Some(expected_offset));
                expected_offset = expected_offset
                    .saturating_add(heights[key])
                    .saturating_add(2);
            }
            let expected_total =
                expected_offset.saturating_sub(if keys.is_empty() { 0 } else { 2 });
            assert_eq!(index.total_height(), expected_total);
            for offset in 0..expected_total {
                let expected = naive_item_at_offset(&keys, &heights, 2, offset);
                assert_eq!(index.item_at_offset(offset), expected);
            }
        }
    }

    fn naive_item_at_offset(
        keys: &[u32],
        heights: &std::collections::BTreeMap<u32, usize>,
        gap: usize,
        offset: usize,
    ) -> Option<usize> {
        let mut start = 0usize;
        for (index, key) in keys.iter().enumerate() {
            let end = start.saturating_add(heights[key]).saturating_add(gap);
            if offset < end {
                return Some(index);
            }
            start = end;
        }
        None
    }

    #[test]
    fn updates_height_without_rebuilding_identity_index() {
        let mut index = MeasuredListIndex::new(0);
        index.sync([("a", 0), ("b", 0)], 8, |_| 2);
        assert!(index.update_height(&"a", 5));
        assert_eq!(index.item_offset(1), Some(5));
        assert_eq!(index.total_height(), 7);
        assert_eq!(index.index_of(&"b"), Some(1));
    }
}
