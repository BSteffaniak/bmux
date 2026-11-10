//! Focus target traversal and reveal tests.

use bmux_tui::measured_list::{MeasuredListIndex, MeasuredListItem};

use super::{FocusTraversal, ItemFocusTarget, reveal_for_focus, reveal_top_offset};
use crate::virtual_list::{
    Edge, ListAnchor, ListGeometry, ListPosition, NoSourceMap, VirtualListState,
};

/// Items 0..n with the given heights and no gap.
fn index(heights: &[u64]) -> MeasuredListIndex<u32> {
    MeasuredListIndex::from_items(
        0,
        heights.iter().enumerate().map(|(key, height)| {
            MeasuredListItem::new(u32::try_from(key).unwrap(), 0, 10, 0, *height)
        }),
    )
    .unwrap()
}

fn target(
    key: u32,
    id: &'static str,
    rows: std::ops::Range<u64>,
) -> ItemFocusTarget<u32, &'static str> {
    ItemFocusTarget::new(key, id, rows)
}

fn ids<'a>(
    targets: impl Iterator<Item = &'a ItemFocusTarget<u32, &'static str>>,
) -> Vec<&'static str> {
    targets.map(|target| target.id).collect()
}

#[test]
fn traversal_orders_by_item_then_row_with_parents_first() {
    let index = index(&[4, 6, 3]);
    let traversal = FocusTraversal::new(
        [
            target(2, "c", 0..1),
            target(1, "b-body", 2..3).parent("b"),
            target(1, "b", 0..6),
            target(0, "a", 0..1),
            target(1, "b-head", 0..1).parent("b"),
            target(9, "evicted", 0..1),
            target(0, "a", 2..3),
        ],
        &index,
    );
    assert_eq!(
        ids(traversal.targets().iter()),
        ["a", "b", "b-head", "b-body", "c"]
    );
    assert_eq!(
        traversal.get(&"a").unwrap().rows,
        0..1,
        "first identity wins"
    );
    assert_eq!(traversal.next(&"b").unwrap().id, "b-head");
    assert_eq!(traversal.previous(&"b").unwrap().id, "a");
    assert!(traversal.previous(&"a").is_none());
    assert!(traversal.next(&"c").is_none());
    assert!(traversal.next(&"evicted").is_none());
    assert_eq!(traversal.parent(&"b-body").unwrap().id, "b");
    assert!(traversal.parent(&"b").is_none());
    assert_eq!(traversal.first().unwrap().id, "a");
    assert_eq!(traversal.last().unwrap().id, "c");
}

#[test]
fn list_rows_clamp_to_the_item() {
    let index = index(&[4, 6]);
    assert_eq!(target(1, "x", 2..4).list_rows(&index), Some(6..8));
    assert_eq!(
        target(1, "x", 3..3).list_rows(&index),
        Some(7..8),
        "empty is one row"
    );
    assert_eq!(target(1, "x", 9..12).list_rows(&index), Some(9..10));
    assert_eq!(target(7, "x", 0..1).list_rows(&index), None);
}

#[test]
fn visible_and_nearest_above_use_collection_rows() {
    let index = index(&[4, 6, 3]);
    let traversal = FocusTraversal::new(
        [
            target(0, "a", 0..1),
            target(1, "b", 0..1),
            target(1, "b2", 4..5),
            target(2, "c", 0..1),
        ],
        &index,
    );
    assert_eq!(ids(traversal.visible(&index, 5..9)), ["b2"]);
    assert_eq!(ids(traversal.visible(&index, 3..5)), ["b"]);
    assert_eq!(
        traversal.last_starting_at_or_before(&index, 9).unwrap().id,
        "b2"
    );
    assert_eq!(
        traversal.last_starting_at_or_before(&index, 4).unwrap().id,
        "b"
    );
    assert_eq!(
        traversal.last_starting_at_or_before(&index, 0).unwrap().id,
        "a"
    );
}

#[test]
fn focus_moves_to_the_nearest_surviving_target() {
    let before = index(&[1, 1, 1, 1, 1]);
    let previous = FocusTraversal::new(
        ["t0", "t1", "t2", "t3", "t4"]
            .into_iter()
            .zip(0..)
            .map(|(id, key)| target(key, id, 0..1)),
        &before,
    );
    // Items 1-3 leave the window.
    let after = MeasuredListIndex::from_items(
        0,
        [0_u32, 4].map(|key| MeasuredListItem::new(key, 0, 10, 0, 1)),
    )
    .unwrap();
    let current = FocusTraversal::new([target(0, "t0", 0..1), target(4, "t4", 0..1)], &after);
    assert_eq!(current.surviving(&previous, &"t0").unwrap().id, "t0");
    assert_eq!(current.surviving(&previous, &"t1").unwrap().id, "t0");
    assert_eq!(
        current.surviving(&previous, &"t2").unwrap().id,
        "t0",
        "ties prefer the earlier target"
    );
    assert_eq!(current.surviving(&previous, &"t3").unwrap().id, "t4");
    assert!(current.surviving(&previous, &"unknown").is_none());
    assert!(
        FocusTraversal::<u32, &str>::default()
            .surviving(&previous, &"t1")
            .is_none()
    );
}

#[test]
fn reveal_approaches_from_the_requested_edge() {
    let index = index(&[10, 10, 10, 10]);
    let geometry = ListGeometry::new(8);
    // Viewport rows 10..18; target rows 22..24 lie below it.
    let below = target(2, "below", 2..4);
    assert_eq!(
        reveal_top_offset(&index, &below, Edge::Bottom, 10, geometry),
        Some(16),
        "moving down shows the target on the bottom rows"
    );
    assert_eq!(
        reveal_top_offset(&index, &below, Edge::Top, 10, geometry),
        Some(22)
    );
    let visible = target(1, "visible", 1..3);
    assert_eq!(
        reveal_top_offset(&index, &visible, Edge::Bottom, 10, geometry),
        Some(10),
        "a visible target never moves the list"
    );
    let tall = target(0, "tall", 0..10);
    assert_eq!(
        reveal_top_offset(&index, &tall, Edge::Bottom, 20, geometry),
        Some(0),
        "a taller target keeps its first row visible"
    );
    let last = target(3, "last", 9..10);
    assert_eq!(
        reveal_top_offset(&index, &last, Edge::Top, 0, geometry),
        Some(32),
        "clamped to the last content top"
    );
    assert_eq!(
        reveal_for_focus::<u32, (), _>(&index, &below, Edge::Bottom, 10, geometry, &NoSourceMap),
        Some(ListPosition::Anchored(ListAnchor::row(1, 6)))
    );
    assert_eq!(
        reveal_top_offset(&index, &target(8, "gone", 0..1), Edge::Top, 0, geometry),
        None
    );
}

#[test]
fn state_reveal_keeps_the_current_position_when_visible() {
    let mut state = VirtualListState::<u32>::controlled(0);
    state
        .splice_measured(
            0..0,
            (0..4).map(|key| MeasuredListItem::new(key, 0, 10, 0, 10)),
        )
        .unwrap();
    state.set_position(ListPosition::FollowBottom);
    let visible = target(3, "tail", 5..6);
    assert_eq!(
        state.reveal_for_focus(&visible, Edge::Bottom, 8, &NoSourceMap),
        Some(ListPosition::FollowBottom)
    );
    let above = target(1, "above", 0..1);
    assert_eq!(
        state.reveal_for_focus(&above, Edge::Top, 8, &NoSourceMap),
        Some(ListPosition::Anchored(ListAnchor::top(1)))
    );
    assert_eq!(
        state.position(),
        ListPosition::FollowBottom,
        "the list never moves itself"
    );
}
