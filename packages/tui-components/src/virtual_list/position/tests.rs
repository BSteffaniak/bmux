//! Position model tests.

use std::cell::Cell;

use bmux_tui::measured_list::{MeasuredListIndex, MeasuredListItem};

use super::{
    AnchorOffset, AnchorPolicy, Edge, ListAnchor, ListGeometry, ListPosition, NoSourceMap,
    Overscroll, ResolvedPosition, SourceMap, anchor_at_offset, anchor_for, anchor_for_below,
    position_at_offset, resolve_position, target_top_offset, target_top_offset_below,
};
use crate::resident_window::EdgeRows;

fn index(gap: u64, heights: &[u64]) -> MeasuredListIndex<u32> {
    MeasuredListIndex::from_items(
        gap,
        heights.iter().enumerate().map(|(key, height)| {
            MeasuredListItem::new(u32::try_from(key).unwrap(), 0, 10, 0, *height)
        }),
    )
    .unwrap()
}

fn resolve(
    index: &MeasuredListIndex<u32>,
    position: &ListPosition<u32>,
    geometry: ListGeometry,
) -> Option<u64> {
    resolve_position(index, position, geometry, &NoSourceMap).map(|resolved| resolved.top_offset)
}

/// Maps source position `p` (a logical line) to row `p * rows_per_line`.
struct LineMap {
    rows_per_line: Cell<u64>,
}

impl SourceMap<u32, u64> for LineMap {
    fn row_for_source(&self, _key: &u32, position: &u64) -> Option<u64> {
        Some(position * self.rows_per_line.get())
    }

    fn source_for_row(&self, _key: &u32, row: u64) -> Option<(u64, u64)> {
        let line = row / self.rows_per_line.get();
        Some((line, line * self.rows_per_line.get()))
    }
}

#[test]
fn overscroll_and_geometry_bounds() {
    assert_eq!(Overscroll::None.max_below(10), 0);
    assert_eq!(Overscroll::Rows(3).max_below(10), 3);
    assert_eq!(Overscroll::ViewportMinus(1).max_below(10), 9);
    assert_eq!(Overscroll::ViewportMinus(20).max_below(10), 0);

    let plain = ListGeometry::new(10);
    assert_eq!(plain.max_content_top(0), 0);
    assert_eq!(plain.max_content_top(4), 0);
    assert_eq!(plain.max_content_top(25), 15);
    for total in [0, 1, 4, 10, 25] {
        assert_eq!(
            plain.max_overscroll_top(total),
            plain.max_content_top(total)
        );
    }

    let tail = plain.overscroll(Overscroll::ViewportMinus(1));
    // The last content row may reach the viewport top.
    assert_eq!(tail.max_overscroll_top(25), 24);
    assert_eq!(tail.max_overscroll_top(4), 3);
    assert_eq!(tail.max_overscroll_top(1), 0);
    assert_eq!(tail.max_overscroll_top(0), 0);

    let rows = plain.overscroll(Overscroll::Rows(3));
    assert_eq!(rows.max_overscroll_top(25), 18);
    // Content shorter than the viewport: overscroll never hides every row.
    assert_eq!(rows.max_overscroll_top(4), 0);
    let huge = plain.overscroll(Overscroll::Rows(u64::MAX));
    assert_eq!(huge.max_overscroll_top(25), 24);
    // Saturating arithmetic never exceeds the content bound.
    assert_eq!(huge.max_overscroll_top(u64::MAX), u64::MAX - 10);

    let empty_viewport = ListGeometry::new(0).overscroll(Overscroll::Rows(5));
    assert_eq!(empty_viewport.max_content_top(7), 7);
    assert_eq!(empty_viewport.max_overscroll_top(7), 7);
}

#[test]
fn resolved_position_helpers() {
    let resolved = ResolvedPosition {
        top_offset: 18,
        viewport_height: 10,
        total_height: 25,
        max_content_top: 15,
        max_overscroll_top: 24,
    };
    assert_eq!(resolved.rows(), 18..28);
    assert_eq!(resolved.blank_rows_below(), 3);
    assert!(resolved.is_overscrolled());
    assert!(resolved.is_at_content_bottom());
    assert_eq!(
        resolved.edge_rows(),
        EdgeRows {
            before: 18,
            after: 0
        }
    );

    let middle = ResolvedPosition {
        top_offset: 5,
        ..resolved
    };
    assert_eq!(middle.blank_rows_below(), 0);
    assert!(!middle.is_overscrolled());
    assert!(!middle.is_at_content_bottom());
    assert_eq!(
        middle.edge_rows(),
        EdgeRows {
            before: 5,
            after: 10
        }
    );

    let empty = ResolvedPosition {
        top_offset: 0,
        viewport_height: 4,
        total_height: 0,
        max_content_top: 0,
        max_overscroll_top: 0,
    };
    assert_eq!(empty.blank_rows_below(), 4);
    assert!(empty.is_at_content_bottom());
    assert!(!empty.is_overscrolled());
}

#[test]
fn positions_resolve_with_clamps() {
    // Rows: 0 at 0..3 (gap 3), 1 at 4..6 (gap 6), 2 at 7..11; total 11.
    let index = index(1, &[3, 2, 4]);
    let geometry = ListGeometry::new(4);
    let anchored = |key, row| ListPosition::Anchored(ListAnchor::row(key, row));
    let overscrolled = |key, row| ListPosition::Overscrolled(ListAnchor::row(key, row));
    assert_eq!(resolve(&index, &ListPosition::Top, geometry), Some(0));
    assert_eq!(
        resolve(&index, &ListPosition::FollowBottom, geometry),
        Some(7)
    );
    // A row within the item, including its trailing gap row.
    assert_eq!(resolve(&index, &anchored(0, 3), geometry), Some(3));
    // Rows past the extent clamp to the last extent row.
    assert_eq!(resolve(&index, &anchored(1, 99), geometry), Some(6));
    // The final item's extent excludes the gap; Anchored clamps to max_content_top.
    assert_eq!(resolve(&index, &anchored(2, 3), geometry), Some(7));
    assert_eq!(resolve(&index, &overscrolled(2, 3), geometry), Some(7));
    let tail = geometry.overscroll(Overscroll::ViewportMinus(1));
    assert_eq!(resolve(&index, &overscrolled(2, 3), tail), Some(10));
    assert_eq!(resolve(&index, &overscrolled(2, 99), tail), Some(10));
    assert_eq!(resolve(&index, &anchored(2, 3), tail), Some(7));
    assert_eq!(resolve(&index, &anchored(99, 0), geometry), None);
    let resolved = resolve_position(
        &index,
        &ListPosition::<u32>::FollowBottom,
        tail,
        &NoSourceMap,
    )
    .unwrap();
    assert_eq!(resolved.total_height, 11);
    assert_eq!(resolved.max_content_top, 7);
    assert_eq!(resolved.max_overscroll_top, 10);
}

#[test]
fn zero_height_and_empty_positions() {
    let empty = index(1, &[]);
    let geometry = ListGeometry::new(5).overscroll(Overscroll::ViewportMinus(1));
    assert_eq!(resolve(&empty, &ListPosition::Top, geometry), Some(0));
    assert_eq!(
        resolve(&empty, &ListPosition::FollowBottom, geometry),
        Some(0)
    );
    assert_eq!(
        position_at_offset::<u32, ()>(&empty, 7, geometry, &NoSourceMap),
        ListPosition::Top
    );
    assert_eq!(anchor_at_offset::<u32, ()>(&empty, 0, &NoSourceMap), None);

    let zeros = index(0, &[0, 0]);
    assert_eq!(
        position_at_offset::<u32, ()>(&zeros, 3, geometry, &NoSourceMap),
        ListPosition::Anchored(ListAnchor::top(0))
    );
    assert_eq!(
        resolve(
            &zeros,
            &ListPosition::Anchored(ListAnchor::row(1, 5)),
            geometry
        ),
        Some(0)
    );

    // A zero-height item without a gap starts where the next item does.
    let mixed = index(0, &[2, 0, 3]);
    assert_eq!(
        resolve(
            &mixed,
            &ListPosition::Anchored(ListAnchor::row(1, 9)),
            ListGeometry::new(1)
        ),
        Some(2)
    );
    assert_eq!(
        anchor_at_offset::<u32, ()>(&mixed, 2, &NoSourceMap),
        Some(ListAnchor::top(2))
    );
}

#[test]
fn anchors_at_offsets_map_gap_rows_to_preceding_item() {
    let index = index(1, &[3, 2, 4]);
    assert_eq!(
        anchor_at_offset::<u32, ()>(&index, 3, &NoSourceMap),
        Some(ListAnchor::row(0, 3))
    );
    assert_eq!(
        anchor_at_offset::<u32, ()>(&index, 5, &NoSourceMap),
        Some(ListAnchor::row(1, 1))
    );
    assert_eq!(anchor_at_offset::<u32, ()>(&index, 11, &NoSourceMap), None);
    let geometry = ListGeometry::new(4).overscroll(Overscroll::ViewportMinus(1));
    assert_eq!(
        position_at_offset::<u32, ()>(&index, 8, geometry, &NoSourceMap),
        ListPosition::Overscrolled(ListAnchor::row(2, 1))
    );
    assert_eq!(
        position_at_offset::<u32, ()>(&index, 7, geometry, &NoSourceMap),
        ListPosition::Anchored(ListAnchor::row(2, 0))
    );
    assert_eq!(
        position_at_offset::<u32, ()>(&index, 0, geometry, &NoSourceMap),
        ListPosition::Anchored(ListAnchor::top(0))
    );
    // Offsets past the content anchor the last row.
    assert_eq!(
        position_at_offset::<u32, ()>(&index, 500, geometry, &NoSourceMap),
        ListPosition::Overscrolled(ListAnchor::row(2, 3))
    );
}

#[test]
fn offsets_round_trip_through_keyed_positions() {
    for gap in [0, 1, 3] {
        for heights in [
            vec![1, 1, 1],
            vec![3, 0, 2, 7, 0, 0, 1],
            vec![0, 5],
            vec![40],
            vec![2, 9, 1, 0, 4],
        ] {
            let index = index(gap, &heights);
            for viewport in [0, 1, 3, 8, 50] {
                for overscroll in [
                    Overscroll::None,
                    Overscroll::Rows(2),
                    Overscroll::ViewportMinus(1),
                ] {
                    let geometry = ListGeometry::new(viewport).overscroll(overscroll);
                    let total = index.total_height();
                    // A zero-row viewport admits top == total, which shows
                    // no row and has no anchor; it resolves to the last row.
                    let last = geometry
                        .max_overscroll_top(total)
                        .min(total.saturating_sub(1));
                    for top in 0..=last {
                        let position =
                            position_at_offset::<u32, ()>(&index, top, geometry, &NoSourceMap);
                        assert_ne!(position, ListPosition::FollowBottom);
                        assert_eq!(
                            resolve(&index, &position, geometry),
                            Some(top),
                            "gap {gap} heights {heights:?} viewport {viewport} {overscroll:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn source_anchors_resolve_through_the_caller_map() {
    let map = LineMap {
        rows_per_line: Cell::new(1),
    };
    let wide = index(0, &[2, 6, 2]);
    let geometry = ListGeometry::new(3);
    let anchor = anchor_at_offset(&wide, 5, &map).unwrap();
    assert_eq!(
        anchor,
        ListAnchor::new(
            1,
            AnchorOffset::Source {
                position: 3,
                row_delta: 0
            }
        )
    );
    let position = ListPosition::Anchored(anchor);
    // At a narrower width each source line takes two rows.
    let narrow = index(0, &[4, 12, 4]);
    map.rows_per_line.set(2);
    let top = |position: &ListPosition<u32, u64>, sources: &dyn SourceMap<u32, u64>| {
        resolve_position(&narrow, position, geometry, sources).map(|r| r.top_offset)
    };
    assert_eq!(top(&position, &map), Some(4 + 6));
    // Row deltas stay below the mapped row.
    let delta = ListPosition::Anchored(ListAnchor::new(
        1,
        AnchorOffset::Source {
            position: 2,
            row_delta: 1,
        },
    ));
    assert_eq!(top(&delta, &map), Some(4 + 5));
    // An unmapped source falls back to the row delta.
    assert_eq!(top(&delta, &NoSourceMap), Some(4 + 1));
    // Mapped rows still clamp to the item extent.
    let far = ListPosition::Anchored(ListAnchor::new(
        0,
        AnchorOffset::Source {
            position: 50,
            row_delta: 0,
        },
    ));
    assert_eq!(top(&far, &map), Some(3));
}

#[test]
fn anchor_policies_place_items() {
    // Rows: 0 at 0..2, 1 at 3..5, 2 at 6..16 (tall), 3 at 17..19; total 19.
    let index = index(1, &[2, 2, 10, 2]);
    let geometry = ListGeometry::new(5);
    let target = |key, policy, current| target_top_offset(&index, &key, policy, current, geometry);
    let from = AnchorPolicy::EnsureVisibleFrom;
    assert_eq!(target(1, AnchorPolicy::ItemTopAtTop, 0), Some(3));
    assert_eq!(target(1, AnchorPolicy::ItemBottomAtBottom, 0), Some(0));
    assert_eq!(target(3, AnchorPolicy::ItemBottomAtBottom, 0), Some(14));
    // Taller items are never pushed off the top.
    assert_eq!(target(2, AnchorPolicy::ItemBottomAtBottom, 0), Some(6));
    assert_eq!(target(2, AnchorPolicy::Center, 0), Some(6));
    // Centered: (5 - 2) / 2 = 1 row above.
    assert_eq!(target(1, AnchorPolicy::Center, 0), Some(2));
    assert_eq!(target(0, AnchorPolicy::Center, 9), Some(0));
    // Clamped to max_content_top without overscroll.
    assert_eq!(target(3, AnchorPolicy::ItemTopAtTop, 0), Some(14));
    let tail = geometry.overscroll(Overscroll::ViewportMinus(1));
    assert_eq!(
        target_top_offset(&index, &3, AnchorPolicy::ItemTopAtTop, 0, tail),
        Some(17)
    );

    // No movement when fully visible.
    assert_eq!(target(1, from(Edge::Top), 1), Some(1));
    assert_eq!(target(1, from(Edge::Bottom), 1), Some(1));
    // A taller item covering the viewport does not move.
    assert_eq!(target(2, from(Edge::Top), 8), Some(8));
    // Partially visible items are placed from the requested edge.
    assert_eq!(target(2, from(Edge::Top), 3), Some(6));
    assert_eq!(target(2, from(Edge::Bottom), 3), Some(6));
    assert_eq!(target(3, from(Edge::Bottom), 0), Some(14));
    assert_eq!(target(0, from(Edge::Bottom), 14), Some(0));
    assert_eq!(target(9, AnchorPolicy::ItemTopAtTop, 0), None);

    // A zero-row viewport degrades every placement to the item top.
    let none = ListGeometry::new(0);
    assert_eq!(
        target_top_offset(&index, &1, AnchorPolicy::ItemBottomAtBottom, 0, none),
        Some(3)
    );
    assert_eq!(
        target_top_offset(&index, &1, AnchorPolicy::Center, 0, none),
        Some(3)
    );
}

#[test]
fn anchor_for_produces_keyed_positions() {
    let index = index(0, &[2, 2, 2, 2]);
    let geometry = ListGeometry::new(3);
    let tail = geometry.overscroll(Overscroll::ViewportMinus(1));
    let place = |key, policy, geometry| {
        anchor_for::<u32, ()>(&index, &key, policy, 0, geometry, &NoSourceMap)
    };
    assert_eq!(
        place(2, AnchorPolicy::ItemTopAtTop, geometry),
        Some(ListPosition::Anchored(ListAnchor::top(2)))
    );
    assert_eq!(
        place(2, AnchorPolicy::ItemBottomAtBottom, geometry),
        Some(ListPosition::Anchored(ListAnchor::row(1, 1)))
    );
    assert_eq!(
        place(3, AnchorPolicy::ItemTopAtTop, tail),
        Some(ListPosition::Overscrolled(ListAnchor::top(3)))
    );
    assert_eq!(place(7, AnchorPolicy::Center, geometry), None);
}

#[test]
fn positions_survive_mutation_without_correction() {
    let mut index = index(1, &[3, 2, 4]);
    let geometry = ListGeometry::new(2);
    let position = ListPosition::Anchored(ListAnchor::row(1, 1));
    assert_eq!(resolve(&index, &position, geometry), Some(5));
    index
        .prepend([MeasuredListItem::new(10, 0, 10, 0, 6)])
        .unwrap();
    assert_eq!(resolve(&index, &position, geometry), Some(12));
    index.update_height(&1, 1);
    // Row 1 is now the item's gap row.
    assert_eq!(resolve(&index, &position, geometry), Some(12));
    index.update_height(&1, 0);
    assert_eq!(resolve(&index, &position, geometry), Some(11));
    index.remove(&10);
    assert_eq!(resolve(&index, &position, geometry), Some(4));
    index.remove(&1);
    assert_eq!(resolve(&index, &position, geometry), None);
}

#[test]
fn list_position_defaults_to_top_for_any_key() {
    struct Opaque;
    let position: ListPosition<Opaque> = ListPosition::default();
    assert!(matches!(position, ListPosition::Top));
    assert!(position.anchor().is_none());
    assert!(ListPosition::<u8>::FollowBottom.is_follow_bottom());
}

#[test]
fn covered_top_rows_shift_every_policy_into_the_uncovered_rows() {
    // Thirty one-row items; a ten-row viewport whose first three rows are
    // covered.
    let index = index(0, &[1; 30]);
    let geometry = ListGeometry::new(10);
    let below = |key, policy, current, inset| {
        target_top_offset_below(&index, &key, policy, current, geometry, inset)
    };
    // The item lands on the first uncovered row.
    assert_eq!(below(15, AnchorPolicy::ItemTopAtTop, 0, 3), Some(12));
    // Bottom alignment is unaffected by covered rows at the top.
    assert_eq!(below(15, AnchorPolicy::ItemBottomAtBottom, 0, 3), Some(6));
    // Centered within the seven uncovered rows (15 - 3 above it).
    assert_eq!(below(15, AnchorPolicy::Center, 0, 3), Some(9));
    // An item under the covered rows is not visible; one below them is.
    let ensure_top = AnchorPolicy::EnsureVisibleFrom(Edge::Top);
    assert_eq!(below(11, ensure_top, 10, 3), Some(8));
    assert_eq!(below(13, ensure_top, 10, 3), Some(10));
    assert_eq!(
        target_top_offset(&index, &11, ensure_top, 10, geometry),
        Some(10)
    );
    // A zero inset is the uncovered placement, and an inset of at least the
    // viewport leaves one uncovered row.
    for policy in [AnchorPolicy::ItemTopAtTop, AnchorPolicy::Center, ensure_top] {
        assert_eq!(
            below(15, policy, 4, 0),
            target_top_offset(&index, &15, policy, 4, geometry)
        );
    }
    assert_eq!(below(15, AnchorPolicy::ItemTopAtTop, 0, 50), Some(6));
    assert_eq!(below(15, AnchorPolicy::ItemTopAtTop, 0, 9), Some(6));
    // Clamped like any target, and unindexed keys have no offset.
    assert_eq!(below(1, AnchorPolicy::ItemTopAtTop, 0, 3), Some(0));
    assert_eq!(below(29, AnchorPolicy::ItemTopAtTop, 0, 3), Some(20));
    assert_eq!(below(99, AnchorPolicy::ItemTopAtTop, 0, 3), None);
    assert_eq!(
        anchor_for_below::<u32, ()>(
            &index,
            &15,
            AnchorPolicy::ItemTopAtTop,
            0,
            geometry,
            3,
            &NoSourceMap
        ),
        Some(ListPosition::Anchored(ListAnchor::top(12)))
    );
}
