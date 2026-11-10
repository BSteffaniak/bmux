//! Logical selection tests.

use bmux_tui::geometry::{Point, Rect};
use bmux_tui::measured_list::{MeasuredListIndex, MeasuredListItem};
use bmux_tui::selection::{
    SelectionAffinity, SelectionAutoScrollRequest, SelectionContentId, SelectionFragment,
    SelectionScene, SelectionScope, SelectionScrollAxis, SelectionScrollDirection,
};

use super::{
    ItemOrder, KeyOrder, LogicalSelection, LogicalSelectionEndpoint, LogicalSelectionFragment,
};
use crate::virtual_list::{ActionDispatch, NoSourceMap, VirtualListAction, VirtualListState};

const fn at(key: u32, offset: usize) -> LogicalSelectionEndpoint<u32> {
    LogicalSelectionEndpoint::new(key, offset)
}

fn resident(keys: &[u32]) -> MeasuredListIndex<u32> {
    MeasuredListIndex::from_items(
        0,
        keys.iter()
            .map(|key| MeasuredListItem::new(*key, 0, 10, 0, 2)),
    )
    .unwrap()
}

#[test]
fn ranges_cover_endpoint_items_partially_and_inner_items_fully() {
    let selection = LogicalSelection::new(at(4, 3), at(2, 5));
    assert_eq!(selection.is_reversed(&KeyOrder), Some(true));
    let (start, end) = selection.ordered(&KeyOrder).unwrap();
    assert_eq!((start, end), (&at(2, 5), &at(4, 3)));
    assert_eq!(selection.range_in(&2, 10, &KeyOrder), Some(5..10));
    assert_eq!(selection.range_in(&3, 7, &KeyOrder), Some(0..7));
    assert_eq!(selection.range_in(&4, 10, &KeyOrder), Some(0..3));
    assert_eq!(selection.range_in(&1, 10, &KeyOrder), None);
    assert_eq!(selection.range_in(&5, 10, &KeyOrder), None);
    assert_eq!(
        selection.range_in(&2, 4, &KeyOrder),
        None,
        "offset past a shorter item"
    );

    let same = LogicalSelection::new(at(3, 6), at(3, 2));
    assert_eq!(same.range_in(&3, 10, &KeyOrder), Some(2..6));
    assert!(LogicalSelection::collapsed(at(3, 2)).is_collapsed());
    assert_eq!(
        LogicalSelection::collapsed(at(3, 2)).range_in(&3, 10, &KeyOrder),
        None
    );
}

#[test]
fn endpoints_survive_eviction_with_key_order() {
    // Items 0..=9 were resident when the selection was made; now only 6..=9.
    let selection = LogicalSelection::new(at(2, 1), at(8, 4));
    let index = resident(&[6, 7, 8, 9]);
    let items = [(&6, 5), (&7, 5), (&8, 5), (&9, 5)];
    assert_eq!(
        selection.fragments(items, &KeyOrder),
        [
            LogicalSelectionFragment {
                key: 6,
                source_range: 0..5
            },
            LogicalSelectionFragment {
                key: 7,
                source_range: 0..5
            },
            LogicalSelectionFragment {
                key: 8,
                source_range: 0..4
            },
        ]
    );
    // An index order cannot order the evicted anchor, so nothing resolves.
    assert_eq!(index.compare_items(&2, &8), None);
    assert_eq!(selection.fragments(items, &index), []);
    assert!(selection.spans(&2, &KeyOrder), "evicted endpoint item");
    assert!(selection.spans(&5, &KeyOrder));
    assert!(!selection.spans(&9, &KeyOrder));
}

#[test]
fn index_order_follows_list_positions_not_keys() {
    // Keys out of numeric order in the list.
    let index = resident(&[30, 10, 20]);
    let selection = LogicalSelection::new(at(30, 1), at(20, 1));
    assert_eq!(selection.is_reversed(&index), Some(false));
    assert_eq!(selection.range_in(&10, 4, &index), Some(0..4));
    assert_eq!(selection.is_reversed(&KeyOrder), Some(true));
}

/// Two items of two rows each; each fragment maps one byte.
fn scene() -> SelectionScene {
    let mut scene = SelectionScene::default();
    scene.push_scope(SelectionScope::new("list", Rect::new(0, 0, 4, 5)));
    for (item, top) in [(1_u32, 0_u16), (2, 2)] {
        for row in 0..2_u16 {
            for column in 0..4_u16 {
                let offset = usize::from(row * 4 + column);
                scene.push_fragment(SelectionFragment::new(
                    "list",
                    format!("item-{item}"),
                    Rect::new(column, top + row, 1, 1),
                    u64::from(item),
                    offset..offset + 1,
                ));
            }
        }
    }
    // An empty line in item 2 at offset 8.
    scene.push_fragment(SelectionFragment::new(
        "list",
        "item-2",
        Rect::new(0, 4, 1, 1),
        2,
        8..8,
    ));
    scene
}

fn key_for(content: &SelectionContentId) -> Option<u32> {
    content.as_str().strip_prefix("item-")?.parse().ok()
}

#[test]
fn highlights_follow_the_scene_being_painted() {
    let scene = scene();
    let selection = LogicalSelection::new(at(1, 6), at(2, 2));
    assert_eq!(
        selection.highlights(&scene, key_for, &KeyOrder),
        [
            Rect::new(2, 1, 1, 1),
            Rect::new(3, 1, 1, 1),
            Rect::new(0, 2, 1, 1),
            Rect::new(1, 2, 1, 1),
        ]
    );
    let through_blank = LogicalSelection::new(at(2, 7), at(3, 0));
    assert_eq!(
        through_blank.highlights(&scene, key_for, &KeyOrder),
        [Rect::new(3, 3, 1, 1), Rect::new(0, 4, 1, 1)]
    );
    assert_eq!(
        LogicalSelection::collapsed(at(1, 1)).highlights(&scene, key_for, &KeyOrder),
        []
    );
}

#[test]
fn scene_endpoints_convert_to_logical_endpoints() {
    let scene = scene();
    let endpoint = scene
        .endpoint_at(&"list".into(), Point::new(1, 3), SelectionAffinity::Before)
        .unwrap();
    assert_eq!(
        LogicalSelectionEndpoint::from_scene(&endpoint, key_for),
        Some(at(2, 5))
    );
    assert_eq!(
        LogicalSelectionEndpoint::<u32>::from_scene(&endpoint, |_| None),
        None
    );
}

#[test]
fn vertical_autoscroll_requests_become_list_actions() {
    let request = |axis, direction, intensity| SelectionAutoScrollRequest {
        scope_id: "list".into(),
        axis,
        direction,
        intensity,
    };
    assert_eq!(
        VirtualListAction::<u32>::selection_auto_scroll(&request(
            SelectionScrollAxis::Vertical,
            SelectionScrollDirection::Backward,
            3
        )),
        Some(VirtualListAction::SelectionAutoScroll(-3))
    );
    assert_eq!(
        VirtualListAction::<u32>::selection_auto_scroll(&request(
            SelectionScrollAxis::Vertical,
            SelectionScrollDirection::Forward,
            0
        )),
        Some(VirtualListAction::SelectionAutoScroll(1))
    );
    assert_eq!(
        VirtualListAction::<u32>::selection_auto_scroll(&request(
            SelectionScrollAxis::Horizontal,
            SelectionScrollDirection::Forward,
            2
        )),
        None
    );

    // A controlled list queues the request for the caller's navigation policy.
    let mut state = VirtualListState::<u32>::controlled(0);
    let action = VirtualListAction::selection_auto_scroll(&request(
        SelectionScrollAxis::Vertical,
        SelectionScrollDirection::Forward,
        2,
    ))
    .unwrap();
    assert_eq!(
        state.dispatch_action(action, &NoSourceMap),
        ActionDispatch::Queued
    );
    assert_eq!(
        state.pending_actions(),
        [VirtualListAction::SelectionAutoScroll(2)]
    );
}
