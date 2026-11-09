use super::{
    DESCENT_STEPS, MeasuredListError, MeasuredListIndex, MeasuredListItem, NIL, VisibleItemRange,
};
use std::collections::BTreeMap;

fn item(key: u32, height: u64) -> MeasuredListItem<u32> {
    MeasuredListItem::new(key, 0, 80, 0, height)
}

fn items(keys: std::ops::Range<u32>, height: impl Fn(u32) -> u64) -> Vec<MeasuredListItem<u32>> {
    keys.map(|key| item(key, height(key))).collect()
}

/// Verify internal structure: parent links, subtree sizes and sums, heap
/// order, key map, and arena compactness.
fn check_invariants<K: Clone + Ord + std::fmt::Debug>(index: &MeasuredListIndex<K>) {
    assert_eq!(index.key_to_node.len(), index.nodes.len());
    if index.root == NIL {
        assert!(index.nodes.is_empty());
        return;
    }
    assert_eq!(index.nodes[index.root].parent, NIL);
    for (id, node) in index.nodes.iter().enumerate() {
        assert_eq!(index.key_to_node.get(&node.item.key), Some(&id));
        let mut size = 1;
        let mut heights = node.item.height;
        for child in [node.left, node.right] {
            if child != NIL {
                assert_eq!(index.nodes[child].parent, id);
                assert!(index.nodes[child].priority <= node.priority);
                size += index.nodes[child].size;
                heights = heights.saturating_add(index.nodes[child].heights);
            }
        }
        assert_eq!(node.size, size);
        assert_eq!(node.heights, heights);
    }
    assert_eq!(index.nodes[index.root].size, index.nodes.len());
}

/// Naive reference model with linear geometry.
struct Model {
    items: Vec<MeasuredListItem<u32>>,
    gap: u64,
}

impl Model {
    fn offsets(&self) -> Vec<u64> {
        let mut offsets = Vec::with_capacity(self.items.len());
        let mut offset = 0u64;
        for item in &self.items {
            offsets.push(offset);
            offset = offset.saturating_add(item.height).saturating_add(self.gap);
        }
        offsets
    }

    fn total(&self) -> u64 {
        let end = self.items.iter().fold(0u64, |sum, item| {
            sum.saturating_add(item.height).saturating_add(self.gap)
        });
        end.saturating_sub(if self.items.is_empty() { 0 } else { self.gap })
    }

    fn item_at_offset(&self, offset: u64) -> Option<usize> {
        if offset >= self.total() {
            return None;
        }
        let mut start = 0u64;
        for (index, item) in self.items.iter().enumerate() {
            let end = start.saturating_add(item.height).saturating_add(self.gap);
            if offset < end {
                return Some(index);
            }
            start = end;
        }
        None
    }

    fn visible_range(&self, offset: u64, viewport: u64) -> VisibleItemRange {
        let total = self.total();
        match self.item_at_offset(offset) {
            Some(start) if viewport > 0 => {
                let end_offset = offset
                    .saturating_add(viewport)
                    .saturating_sub(1)
                    .min(total - 1);
                let end = self
                    .item_at_offset(end_offset)
                    .map_or(self.items.len(), |index| index + 1);
                VisibleItemRange {
                    start,
                    end,
                    first_item_offset: offset - self.offsets()[start],
                }
            }
            _ => VisibleItemRange {
                start: 0,
                end: 0,
                first_item_offset: 0,
            },
        }
    }
}

/// Compare every query against the naive model and against a full rebuild
/// from the model's items.
fn assert_matches_model(index: &MeasuredListIndex<u32>, model: &Model, seed: u64) {
    check_invariants(index);
    assert_eq!(index.len(), model.items.len());
    assert_eq!(index.is_empty(), model.items.is_empty());
    assert_eq!(index.gap(), model.gap);
    assert_eq!(index.iter().len(), model.items.len());
    assert_eq!(index.iter().cloned().collect::<Vec<_>>(), model.items);
    let offsets = model.offsets();
    for (position, item) in model.items.iter().enumerate() {
        assert_eq!(index.item(position), Some(item));
        assert_eq!(index.index_of(&item.key), Some(position));
        assert!(index.contains_key(&item.key));
        assert_eq!(index.item_by_key(&item.key), Some(item));
        assert_eq!(index.item_offset(position), Some(offsets[position]));
        assert_eq!(index.key_offset(&item.key), Some(offsets[position]));
    }
    assert_eq!(index.item(model.items.len()), None);
    assert_eq!(index.item_offset(model.items.len()), None);
    let total = model.total();
    assert_eq!(index.total_height(), total);
    for offset in 0..total.saturating_add(2) {
        assert_eq!(
            index.item_at_offset(offset),
            model.item_at_offset(offset),
            "offset {offset}"
        );
    }
    let rebuilt = MeasuredListIndex::from_items(model.gap, model.items.clone())
        .expect("model keys are unique");
    assert_eq!(index, &rebuilt);
    for probe in 0..4u64 {
        let mixed = seed.wrapping_mul(probe + 1).rotate_left(13);
        let viewport = mixed % 13;
        let top = mixed % total.saturating_add(3);
        let expected = model.visible_range(top, viewport);
        assert_eq!(index.visible_range(top, viewport), expected);
        assert_eq!(rebuilt.visible_range(top, viewport), expected);
    }
}

struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
        }
    }
}

struct Fresh(u32);

impl Fresh {
    fn take(&mut self, count: usize, rng: &mut Rng) -> Vec<MeasuredListItem<u32>> {
        (0..count)
            .map(|_| {
                self.0 += 1;
                item(self.0, rng.next() % 6)
            })
            .collect()
    }
}

fn sync_step(index: &mut MeasuredListIndex<u32>, model: &mut Model, rng: &mut Rng) {
    let mut entries = model
        .items
        .iter()
        .map(|item| (item.key, item.layout_revision))
        .collect::<Vec<_>>();
    if entries.len() > 2 {
        let (left, right) = (rng.below(entries.len()), rng.below(entries.len()));
        entries.swap(left, right);
        let bumped = rng.below(entries.len());
        entries[bumped].1 += 1;
    }
    let previous = model
        .items
        .iter()
        .map(|item| (item.key, item.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut measured = Vec::new();
    index.sync(entries.iter().copied(), 80, 0, |key| {
        measured.push(*key);
        previous[key].height + 1
    });
    assert!(measured.len() <= 1, "only the bumped key is re-measured");
    model.items = entries
        .iter()
        .map(|(key, revision)| {
            let old = &previous[key];
            if old.layout_revision == *revision {
                old.clone()
            } else {
                MeasuredListItem::new(*key, *revision, 80, 0, old.height + 1)
            }
        })
        .collect();
}

#[test]
fn randomized_incremental_operations_match_full_rebuild() {
    for seed in [1u64, 7, 0x5eed, 0xdead_beef] {
        let mut rng = Rng(seed);
        let mut fresh = Fresh(0);
        let mut index = MeasuredListIndex::new(1);
        let mut model = Model {
            items: Vec::new(),
            gap: 1,
        };
        for step in 0..600 {
            let len = model.items.len();
            match rng.next() % 11 {
                0 => {
                    let batch = fresh.take(rng.below(5), &mut rng);
                    index.prepend(batch.clone()).unwrap();
                    model.items.splice(0..0, batch);
                }
                1 => {
                    let batch = fresh.take(rng.below(5), &mut rng);
                    index.append(batch.clone()).unwrap();
                    model.items.extend(batch);
                }
                2 => {
                    let at = rng.below(len + 1);
                    let batch = fresh.take(rng.below(4), &mut rng);
                    index.insert(at, batch.clone()).unwrap();
                    model.items.splice(at..at, batch);
                }
                3 => {
                    let start = rng.below(len + 1);
                    let end = start + rng.below(len - start + 1);
                    let removed = index.remove_range(start..end).unwrap();
                    let expected = model.items.drain(start..end).collect::<Vec<_>>();
                    assert_eq!(removed, expected);
                }
                4 => {
                    let start = rng.below(len + 1);
                    let end = start + rng.below(len - start + 1);
                    // Reinsert some removed keys in reverse plus fresh ones.
                    let mut batch = model.items[start..end]
                        .iter()
                        .rev()
                        .step_by(2)
                        .map(|old| item(old.key, rng.next() % 6))
                        .collect::<Vec<_>>();
                    batch.extend(fresh.take(rng.below(3), &mut rng));
                    let removed = index.replace_range(start..end, batch.clone()).unwrap();
                    let expected = model.items.splice(start..end, batch).collect::<Vec<_>>();
                    assert_eq!(removed, expected);
                }
                5 if len > 0 => {
                    let position = rng.below(len);
                    let height = rng.next() % 9;
                    assert!(index.update_height(&model.items[position].key, height));
                    model.items[position].height = height;
                }
                6 if len > 0 => {
                    let position = rng.below(len);
                    let key = model.items[position].key;
                    assert_eq!(index.remove(&key), Some(model.items.remove(position)));
                }
                7 => sync_step(&mut index, &mut model, &mut rng),
                8 => {
                    let gap = rng.next() % 3;
                    index.set_gap(gap);
                    model.gap = gap;
                }
                9 if len > 0 => {
                    let position = rng.below(len);
                    let mut updated = model.items[position].clone();
                    updated.layout_revision += 1;
                    updated.height = rng.next() % 7;
                    assert!(index.update_item(updated.clone()));
                    model.items[position] = updated;
                }
                _ if len > 0 => {
                    let key = model.items[rng.below(len)].key;
                    let before = index.clone();
                    assert_eq!(
                        index.insert(rng.below(len + 1), [item(key, 1)]),
                        Err(MeasuredListError::DuplicateKey { position: 0 })
                    );
                    assert_eq!(index, before);
                }
                _ => {}
            }
            assert_matches_model(&index, &model, seed ^ step);
        }
    }
}

#[test]
fn synchronizes_by_stable_key_revision_width_and_capability() {
    let mut index = MeasuredListIndex::new(1);
    let mut measured = Vec::new();
    index.sync([("a", 1), ("b", 1)], 10, 0, |key| {
        measured.push(*key);
        if *key == "a" { 2 } else { 3 }
    });
    index.sync([("b", 1), ("a", 1)], 10, 0, |key| {
        measured.push(*key);
        9
    });
    assert_eq!(measured, ["a", "b"]);
    assert_eq!(index.item(0).map(|item| item.height), Some(3));
    index.sync([("b", 2), ("a", 1)], 10, 0, |key| {
        measured.push(*key);
        4
    });
    assert_eq!(measured, ["a", "b", "b"]);
    index.sync([("b", 2), ("a", 1)], 11, 0, |key| {
        measured.push(*key);
        1
    });
    assert_eq!(measured, ["a", "b", "b", "b", "a"]);
    index.sync([("b", 2), ("a", 1)], 11, 3, |key| {
        measured.push(*key);
        1
    });
    assert_eq!(measured.len(), 7, "capability revision invalidates");
    check_invariants(&index);
}

#[test]
fn sync_prepend_and_append_measure_only_new_items() {
    let mut index = MeasuredListIndex::new(0);
    index.sync((10u32..20).map(|key| (key, 0)), 40, 0, |_| 2);
    let mut measured = Vec::new();
    index.sync((5u32..25).map(|key| (key, 0)), 40, 0, |key| {
        measured.push(*key);
        1
    });
    assert_eq!(measured, [5, 6, 7, 8, 9, 20, 21, 22, 23, 24]);
    assert_eq!(index.total_height(), 30);
    check_invariants(&index);
    index.sync(std::iter::empty(), 40, 0, |_| 0);
    assert!(index.is_empty());
    check_invariants(&index);
}

#[test]
#[should_panic(expected = "measured-list keys must be unique")]
fn sync_rejects_duplicate_keys() {
    let mut index = MeasuredListIndex::new(0);
    index.sync([(1u32, 0), (1, 0)], 10, 0, |_| 1);
}

#[test]
fn rejected_mutations_leave_index_unchanged() {
    let mut index = MeasuredListIndex::from_items(0, items(0..4, |_| 1)).unwrap();
    let before = index.clone();
    assert_eq!(
        index.insert(5, [item(9, 1)]),
        Err(MeasuredListError::OutOfBounds {
            start: 5,
            end: 5,
            len: 4
        })
    );
    #[allow(clippy::reversed_empty_ranges)] // Reversed input is the case under test.
    let reversed = index.remove_range(3..1);
    assert!(matches!(
        reversed,
        Err(MeasuredListError::OutOfBounds { .. })
    ));
    assert!(matches!(
        index.remove_range(2..9),
        Err(MeasuredListError::OutOfBounds { .. })
    ));
    assert_eq!(
        index.append([item(7, 1), item(7, 2)]),
        Err(MeasuredListError::DuplicateKey { position: 1 })
    );
    assert_eq!(
        index.splice(0..1, [item(2, 1)]),
        Err(MeasuredListError::DuplicateKey { position: 0 })
    );
    assert!(MeasuredListIndex::from_items(0, [item(1, 1), item(1, 1)]).is_err());
    assert_eq!(index, before);
    check_invariants(&index);
    // Keys inside the replaced range may be reinserted in any order.
    let removed = index.splice(1..3, [item(2, 5), item(1, 6)]).unwrap();
    assert_eq!(removed, vec![item(1, 1), item(2, 1)]);
    assert_eq!(index.keys().copied().collect::<Vec<_>>(), [0, 2, 1, 3]);
    assert!(
        MeasuredListError::DuplicateKey { position: 1 }
            .to_string()
            .contains("position 1")
    );
}

#[test]
fn empty_and_single_item_edges() {
    let mut index = MeasuredListIndex::<u32>::new(2);
    assert_eq!(index.total_height(), 0);
    assert_eq!(index.item_at_offset(0), None);
    assert_eq!(index.item_offset(0), None);
    assert_eq!(index.iter().count(), 0);
    assert_eq!(index.iter_from(3).count(), 0);
    assert_eq!(index.remove(&1), None);
    assert_eq!(index.remove_range(0..0), Ok(Vec::new()));
    assert!(index.prepend([]).is_ok());
    assert!(!index.update_height(&1, 3));
    assert!(!index.update_item(item(1, 3)));
    assert_eq!(index.remeasure(10, 1, |_| 1), 0);
    assert_eq!(
        index.visible_range(0, 10),
        VisibleItemRange {
            start: 0,
            end: 0,
            first_item_offset: 0
        }
    );

    index.append([item(1, 3)]).unwrap();
    assert_eq!(index.total_height(), 3, "trailing gap excluded");
    assert_eq!(index.item_at_offset(2), Some(0));
    assert_eq!(index.item_at_offset(3), None);
    assert_eq!(
        index.visible_range(1, 100),
        VisibleItemRange {
            start: 0,
            end: 1,
            first_item_offset: 1
        }
    );
    assert_eq!(index.remove(&1), Some(item(1, 3)));
    assert!(index.is_empty());
    check_invariants(&index);
}

#[test]
fn zero_height_items_occupy_no_rows_without_gap() {
    let index = MeasuredListIndex::from_items(
        0,
        [item(1, 0), item(2, 2), item(3, 0), item(4, 0), item(5, 1)],
    )
    .unwrap();
    assert_eq!(index.total_height(), 3);
    assert_eq!(index.item_at_offset(0), Some(1));
    assert_eq!(index.item_at_offset(1), Some(1));
    assert_eq!(index.item_at_offset(2), Some(4));
    assert_eq!(index.item_offset(2), Some(2));
    assert_eq!(index.item_offset(4), Some(2));
    let all_zero = MeasuredListIndex::from_items(0, [item(1, 0), item(2, 0)]).unwrap();
    assert_eq!(all_zero.total_height(), 0);
    assert_eq!(all_zero.item_at_offset(0), None);
    assert_eq!(all_zero.visible_range(0, 5).end, 0);
    let gapped = MeasuredListIndex::from_items(1, [item(1, 0), item(2, 0)]).unwrap();
    assert_eq!(gapped.total_height(), 1);
    assert_eq!(gapped.item_at_offset(0), Some(0));
}

#[test]
fn items_taller_than_viewport_and_large_heights() {
    let height = u64::from(u32::MAX) + 1;
    let mut index = MeasuredListIndex::new(3);
    index.sync([("a", 0), ("b", 0)], height, 0, |_| height);
    assert_eq!(index.item_offset(1), Some(height + 3));
    assert_eq!(index.total_height(), height * 2 + 3);
    assert_eq!(index.item_at_offset(height + 3), Some(1));
    assert_eq!(
        index.visible_range(height + 5, 1),
        VisibleItemRange {
            start: 1,
            end: 2,
            first_item_offset: 2
        }
    );
    assert_eq!(
        index.visible_range(10, 4),
        VisibleItemRange {
            start: 0,
            end: 1,
            first_item_offset: 10
        }
    );
    let saturated =
        MeasuredListIndex::from_items(0, [item(1, u64::MAX), item(2, u64::MAX)]).unwrap();
    assert_eq!(saturated.total_height(), u64::MAX);
    check_invariants(&saturated);
}

#[test]
fn reports_offsets_total_height_and_visible_boundaries() {
    let mut index = MeasuredListIndex::new(1);
    index.sync([("a", 0), ("b", 0), ("c", 0)], 8, 0, |key| match *key {
        "a" => 2,
        "b" => 4,
        _ => 3,
    });
    assert_eq!(index.item_offset(0), Some(0));
    assert_eq!(index.item_offset(1), Some(3));
    assert_eq!(index.item_offset(2), Some(8));
    assert_eq!(index.total_height(), 11);
    assert_eq!(index.item_at_offset(0), Some(0));
    assert_eq!(
        index.item_at_offset(2),
        Some(0),
        "gap row maps to the preceding item"
    );
    assert_eq!(index.item_at_offset(3), Some(1));
    assert_eq!(
        index.visible_range(4, 5),
        VisibleItemRange {
            start: 1,
            end: 3,
            first_item_offset: 1
        }
    );
}

#[test]
fn remeasure_updates_only_stale_width_or_capability() {
    let mut index = MeasuredListIndex::from_items(0, items(0..5, |_| 1)).unwrap();
    assert!(index.update_item(MeasuredListItem::new(2, 4, 40, 0, 7)));
    let mut measured = Vec::new();
    let count = index.remeasure(40, 0, |key| {
        measured.push(*key);
        u64::from(*key) + 10
    });
    assert_eq!(count, 4);
    assert_eq!(measured, [0, 1, 3, 4]);
    assert_eq!(
        index.item_by_key(&2).map(|item| item.layout_revision),
        Some(4)
    );
    assert_eq!(index.total_height(), 10 + 11 + 7 + 13 + 14);
    assert_eq!(index.remeasure(40, 0, |_| 0), 0);
    assert_eq!(index.remeasure(40, 1, |_| 1), 5);
    assert_eq!(index.total_height(), 5);
    check_invariants(&index);
}

#[test]
fn key_lookup_survives_prepend_and_removal() {
    let mut index = MeasuredListIndex::from_items(0, items(100..200, |_| 1)).unwrap();
    index.prepend(items(0..100, |_| 2)).unwrap();
    assert_eq!(index.index_of(&150), Some(150));
    assert_eq!(index.key_offset(&150), Some(250));
    index.remove_range(0..50).unwrap();
    assert_eq!(index.index_of(&150), Some(100));
    assert_eq!(index.index_of(&10), None);
    assert_eq!(
        index
            .iter_from(98)
            .map(|item| item.key)
            .take(4)
            .collect::<Vec<_>>(),
        [148, 149, 150, 151]
    );
    assert_eq!(index.iter_from(98).len(), 52);
    assert_eq!((&index).into_iter().count(), 150);
    check_invariants(&index);
}

#[test]
fn lookups_stay_logarithmic_across_mutation() {
    let mut index = MeasuredListIndex::new(1);
    index.sync((0..1_000_000usize).map(|key| (key, 0)), 80, 0, |_| 1);
    DESCENT_STEPS.with(|steps| steps.set(0));
    let range = index.visible_range(1_234_567, 40);
    let steps = DESCENT_STEPS.with(std::cell::Cell::get);
    assert!(range.start < range.end);
    assert!(steps <= 200, "two treap descents took {steps} steps");
    for chunk in 0..100usize {
        index
            .insert(
                chunk * 7_000,
                (0..10).map(|offset| {
                    MeasuredListItem::new(2_000_000 + chunk * 10 + offset, 0, 80, 0, 1)
                }),
            )
            .unwrap();
    }
    index.remove_range(0..250_000).unwrap();
    DESCENT_STEPS.with(|steps| steps.set(0));
    let _ = index.item_at_offset(1_000_000);
    let steps = DESCENT_STEPS.with(std::cell::Cell::get);
    assert!(steps <= 100, "descent took {steps} steps");
    check_invariants(&index);
}

#[test]
fn randomized_viewports_match_naive_visible_ranges() {
    let mut rng = Rng(0x00c0_ffee);
    for _ in 0..500 {
        let len = u32::try_from(rng.next() % 40).unwrap();
        let gap = rng.next() % 5;
        let viewport = rng.next() % 20;
        let model = Model {
            items: (0..len).map(|key| item(key, rng.next() % 12 + 1)).collect(),
            gap,
        };
        let index = MeasuredListIndex::from_items(gap, model.items.clone()).unwrap();
        let offset = rng.next() % (index.total_height() + 3);
        assert_eq!(
            index.visible_range(offset, viewport),
            model.visible_range(offset, viewport)
        );
    }
}
