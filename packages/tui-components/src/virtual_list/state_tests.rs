//! State, reconciliation, and lazy construction tests.

use std::cell::{Cell, RefCell};

use bmux_tui::buffer::Buffer;
use bmux_tui::component::{
    Component, Constraints, Element, LayoutCx, LayoutEnvironment, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::measured_list::{MeasuredListIndex, MeasuredListItem};
use bmux_tui::paint::PaintCx;
use bmux_tui::style::Style;

use super::{
    ActionDispatch, AnchorOffset, AnchorPolicy, Edge, ListAnchor, ListPosition, NoSourceMap,
    Overscroll, PageStep, PositionControl, PositionEstimate, ScrollInputSource, SourceMap,
    VirtualItemSource, VirtualList, VirtualListAction, VirtualListState, VirtualSourceList,
    WindowSplice,
};

/// Item whose height is `ceil(units / width)`: width-dependent reflow.
struct Unit {
    key: u64,
    units: u64,
}

impl Component for Unit {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = constraints.max_width();
        let height = self.units.div_ceil(width.max(1));
        LayoutNode::leaf(
            LayoutId::new(format!("unit.{}", self.key)),
            LogicalSize::new(width, height),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let glyph = char::from_digit(u32::try_from(self.key % 10).unwrap(), 10).unwrap();
        let area = cx.area();
        let _ = layout;
        cx.fill(area, &glyph.to_string(), Style::new());
    }
}

/// Item spec: key, layout revision, units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Spec {
    key: u64,
    revision: u64,
    units: u64,
}

const fn spec(key: u64, units: u64) -> Spec {
    Spec {
        key,
        revision: 0,
        units,
    }
}

/// Lazy source that records which items were constructed.
#[derive(Default)]
struct Lazy {
    items: Vec<Spec>,
    built: RefCell<Vec<u64>>,
    reads: Cell<usize>,
    retained: Option<Vec<LayoutNode>>,
}

impl Lazy {
    fn new(items: impl IntoIterator<Item = Spec>) -> Self {
        Self {
            items: items.into_iter().collect(),
            ..Self::default()
        }
    }

    fn take_built(&self) -> Vec<u64> {
        std::mem::take(&mut *self.built.borrow_mut())
    }
}

impl VirtualItemSource<u64> for Lazy {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn key(&self, index: usize) -> u64 {
        self.reads.set(self.reads.get() + 1);
        self.items[index].key
    }

    fn layout_revision(&self, index: usize) -> u64 {
        self.items[index].revision
    }

    fn build(&self, index: usize) -> Element<'_> {
        let spec = self.items[index];
        self.built.borrow_mut().push(spec.key);
        Element::new(Unit {
            key: spec.key,
            units: spec.units,
        })
    }

    fn retained_layout(
        &self,
        index: usize,
        _width: u64,
        _environment: LayoutEnvironment,
    ) -> Option<&LayoutNode> {
        self.retained.as_ref().map(|nodes| &nodes[index])
    }
}

fn keys(range: std::ops::Range<u64>) -> Vec<Spec> {
    range.map(|key| spec(key, 8)).collect()
}

const WIDTH: u64 = 8;
const NONE: [u64; 0] = [];

fn layout(
    source: &Lazy,
    state: &mut VirtualListState<u64>,
    viewport: u64,
) -> super::VirtualListLayout {
    VirtualSourceList::new("lazy", source).layout(
        WIDTH,
        LayoutEnvironment::default(),
        viewport,
        &NoSourceMap,
        state,
        &mut LayoutCx::new(),
    )
}

fn expected_index(source: &Lazy, width: u64, gap: u64) -> MeasuredListIndex<u64> {
    MeasuredListIndex::from_items(
        gap,
        source.items.iter().map(|spec| {
            MeasuredListItem::new(
                spec.key,
                spec.revision,
                width,
                0,
                spec.units.div_ceil(width),
            )
        }),
    )
    .unwrap()
}

fn paint_rows(source: &Lazy, state: &VirtualListState<u64>, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, 8, height);
    let mut buffer = Buffer::empty(area);
    VirtualSourceList::new("lazy", source).paint(
        area,
        state,
        &mut PaintCx::new(&mut Frame::new(&mut buffer)),
    );
    (0..height)
        .map(|row| buffer.row_symbols(row).unwrap_or_default())
        .collect()
}

#[test]
fn lazy_source_builds_only_items_needing_measurement_and_visible_items() {
    let source = Lazy::new(keys(0..1_000));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 5);
    // The first synchronization measures every item exactly once.
    assert_eq!(source.take_built().len(), 1_000);
    assert_eq!(state.render_stats().measured_items, 1_000);
    assert_eq!(state.total_height(), 1_000);

    // Steady state: no reconciliation, no construction.
    source.reads.set(0);
    for _ in 0..3 {
        layout(&source, &mut state, 5);
        assert_eq!(source.take_built(), NONE);
        assert_eq!(state.render_stats().measured_items, 0);
        assert_eq!(state.render_stats().built_items, 0);
    }
    // Only visible keys are read (layout re-reads visible revisions).
    assert!(source.reads.get() <= 3 * 5, "reads {}", source.reads.get());

    // Painting constructs only visible items.
    assert!(state.apply_action(&VirtualListAction::ScrollRequested {
        rows: 500,
        source: ScrollInputSource::Wheel,
    }));
    layout(&source, &mut state, 5);
    let rows = paint_rows(&source, &state, 5);
    assert_eq!(source.take_built(), vec![500, 501, 502, 503, 504]);
    assert_eq!(rows[0], "00000000");
    assert_eq!(rows[4], "44444444");
    let counters = state.render_stats();
    assert_eq!(counters.painted_items, 5);
    assert_eq!(counters.registered_items, 5);
    assert_eq!(counters.built_items, 5);
}

#[test]
fn noted_prepend_append_and_splice_measure_only_new_items() {
    let mut source = Lazy::new(keys(100..200));
    let mut state = VirtualListState::new(1);
    layout(&source, &mut state, 6);
    source.take_built();
    state.set_position(ListPosition::Anchored(ListAnchor::row(150, 0)));
    assert_eq!(layout(&source, &mut state, 6).resolved.top_offset, 100);

    // Prepend 10, append 5.
    source.items.splice(0..0, keys(0..10));
    state.note_splice(0..0, 10);
    source.items.extend(keys(1_000..1_005));
    state.note_splice(110..110, 5);
    let resolved = layout(&source, &mut state, 6).resolved;
    let mut built = source.take_built();
    built.sort_unstable();
    assert_eq!(
        built,
        (0..10).chain(1_000..1_005).collect::<Vec<_>>(),
        "only inserted items are constructed"
    );
    assert_eq!(state.render_stats().measured_items, 15);
    // The keyed anchor survives without correction.
    assert_eq!(resolved.top_offset, 120);
    assert_eq!(
        state.position(),
        ListPosition::Anchored(ListAnchor::top(150))
    );
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 1));

    // Splice in the middle, reinserting one removed key unchanged and one
    // with a new revision.
    // Rows 65..69 hold keys 155..159.
    let replaced = vec![
        spec(2_000, 16),
        spec(156, 8),
        Spec {
            key: 157,
            revision: 4,
            units: 24,
        },
    ];
    source.items.splice(65..69, replaced);
    state.note_splice(65..69, 3);
    layout(&source, &mut state, 6);
    let mut built = source.take_built();
    built.sort_unstable();
    assert_eq!(built, vec![157, 2_000]);
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 1));
    // The anchor key 150 sits before the splice and keeps its row.
    assert_eq!(state.resolved().unwrap().top_offset, 120);
}

#[test]
fn composed_notes_match_a_full_rebuild_under_random_operations() {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound.max(1)
    };
    let mut source = Lazy::new(keys(0..40));
    let mut state = VirtualListState::new(1);
    let mut fresh = 10_000;
    layout(&source, &mut state, 7);
    source.take_built();
    for round in 0..300 {
        let before = state.position();
        let mut inserted_total = 0;
        for _ in 0..=next(3) {
            let len = u64::try_from(source.items.len()).unwrap();
            let start = next(len + 1);
            let end = (start + next(4)).min(len);
            let start = usize::try_from(start).unwrap();
            let end = usize::try_from(end).unwrap();
            let count = usize::try_from(next(4)).unwrap();
            let items = (0..count)
                .map(|_| {
                    fresh += 1;
                    spec(fresh, next(20))
                })
                .collect::<Vec<_>>();
            inserted_total += items.len();
            source.items.splice(start..end, items);
            state.note_splice(start..end, count);
        }
        if next(4) == 0 && !source.items.is_empty() {
            let at = usize::try_from(next(u64::try_from(source.items.len()).unwrap())).unwrap();
            source.items[at].revision += 1;
            source.items[at].units += 3;
            inserted_total += 1;
            let key = source.items[at].key;
            state.invalidate(&key);
        }
        layout(&source, &mut state, 7);
        let built = source.take_built();
        assert_eq!(
            *state.index(),
            expected_index(&source, WIDTH, 1),
            "round {round}"
        );
        assert!(
            state.render_stats().measured_items <= inserted_total,
            "round {round}: measured {} > noted {inserted_total}",
            state.render_stats().measured_items
        );
        assert!(
            built.len() <= inserted_total,
            "round {round}: built {built:?} noted {inserted_total} stats {:?}",
            state.render_stats()
        );
        let resolved = state.resolved().unwrap();
        assert!(resolved.top_offset <= resolved.max_content_top);
        // A surviving anchor keeps its key and row; rows re-derive from the
        // updated index without correction.
        if let ListPosition::Anchored(anchor) = &before
            && state.index().contains_key(&anchor.key)
        {
            assert_eq!(state.position(), before, "round {round}");
            let AnchorOffset::Row(row) = anchor.offset else {
                unreachable!("no source map");
            };
            let position = state.index().index_of(&anchor.key).unwrap();
            let item = state.index().item(position).unwrap();
            let extent = item.height + u64::from(position + 1 < state.index().len());
            let row = row.min(extent.saturating_sub(1));
            assert_eq!(
                resolved.top_offset,
                (state.item_offset(&anchor.key).unwrap() + row).min(resolved.max_content_top),
                "round {round}"
            );
        }
        if next(2) == 0 {
            state.apply_action(&VirtualListAction::ScrollRequested {
                rows: i64::try_from(next(30)).unwrap() - 15,
                source: ScrollInputSource::Key,
            });
        }
    }
}

#[test]
fn invalidation_rereads_revisions_and_measures_only_stale_items() {
    let mut source = Lazy::new(keys(0..50));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 4);
    source.take_built();

    state.invalidate(&30);
    layout(&source, &mut state, 4);
    assert_eq!(source.take_built(), NONE, "unchanged revision");

    source.items[30] = Spec {
        key: 30,
        revision: 1,
        units: 24,
    };
    state.invalidate(&30);
    state.invalidate(&999);
    layout(&source, &mut state, 4);
    assert_eq!(source.take_built(), vec![30]);
    assert_eq!(state.item_height(&30), Some(3));

    // A revision change that is not noted is still caught when visible.
    source.items[1] = Spec {
        key: 1,
        revision: 9,
        units: 16,
    };
    layout(&source, &mut state, 4);
    assert_eq!(source.take_built(), vec![1]);
    assert_eq!(state.item_height(&1), Some(2));

    // invalidate_all compares everything but measures nothing unchanged.
    state.invalidate_all();
    layout(&source, &mut state, 4);
    assert_eq!(source.take_built(), NONE);
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 0));
}

#[test]
fn width_and_environment_changes_remeasure_and_keep_anchor_rows() {
    let source = Lazy::new(keys(0..20).into_iter().map(|item| spec(item.key, 16)));
    let mut state = VirtualListState::new(0);
    let list = VirtualSourceList::new("lazy", &source);
    let mut cx = LayoutCx::new();
    let at = |state: &mut VirtualListState<u64>, width, revision, cx: &mut LayoutCx| {
        list.layout(
            width,
            LayoutEnvironment::new(revision),
            4,
            &NoSourceMap,
            state,
            cx,
        )
    };
    at(&mut state, 8, 0, &mut cx);
    source.take_built();
    state.set_position(ListPosition::Anchored(ListAnchor::row(10, 1)));
    assert_eq!(at(&mut state, 8, 0, &mut cx).resolved.top_offset, 21);

    // Wider: each item shrinks to one row; the row clamps inside item 10.
    assert_eq!(at(&mut state, 16, 0, &mut cx).resolved.top_offset, 10);
    assert_eq!(source.take_built().len(), 20);
    assert_eq!(state.render_stats().measured_items, 20);
    // Narrower again: the anchor row is not lost.
    assert_eq!(at(&mut state, 8, 0, &mut cx).resolved.top_offset, 21);
    // Environment change re-measures without moving the keyed position.
    source.take_built();
    assert_eq!(at(&mut state, 8, 1, &mut cx).resolved.top_offset, 21);
    assert_eq!(state.render_stats().measured_items, 20);
}

#[test]
fn retained_layouts_replace_construction_for_measurement() {
    let mut source = Lazy::new(keys(0..30));
    source.retained = Some(
        source
            .items
            .iter()
            .map(|item| {
                LayoutNode::leaf(
                    LayoutId::new(format!("retained.{}", item.key)),
                    LogicalSize::new(WIDTH, 2),
                )
            })
            .collect(),
    );
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 5);
    assert_eq!(source.take_built(), NONE);
    assert_eq!(state.render_stats().measured_items, 30);
    assert_eq!(state.render_stats().built_items, 0);
    assert_eq!(state.total_height(), 60);
    let rows = paint_rows(&source, &state, 5);
    assert_eq!(source.take_built(), vec![0, 1, 2]);
    // The retained layout (two rows) is painted, not the component's own.
    assert_eq!(
        rows,
        vec!["00000000", "00000000", "11111111", "11111111", "22222222"]
    );
}

#[test]
fn caller_measured_splices_apply_immediately() {
    let mut source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 3);
    source.take_built();
    state.set_position(ListPosition::Anchored(ListAnchor::top(5)));
    layout(&source, &mut state, 3);

    // Caller measures two prepended items itself; one height is wrong.
    source.items.splice(0..0, [spec(100, 8), spec(101, 16)]);
    state
        .splice_measured(
            0..0,
            [
                MeasuredListItem::new(100, 0, WIDTH, 0, 1),
                MeasuredListItem::new(101, 0, WIDTH, 0, 5),
            ],
        )
        .unwrap();
    assert_eq!(state.total_height(), 16);
    assert_eq!(state.resolve(3, &NoSourceMap).unwrap().top_offset, 11);
    let resolved = layout(&source, &mut state, 3).resolved;
    // Off-screen caller measurements are trusted and nothing is built.
    assert_eq!(source.take_built(), NONE);
    assert_eq!(resolved.top_offset, 11);
    assert!(state.splice_measured(0..99, []).is_err());
    assert!(
        state
            .splice_measured(0..0, [MeasuredListItem::new(5, 0, WIDTH, 0, 1)])
            .is_err()
    );

    // Visible caller measurements without a retained layout are measured
    // exactly, and the exact height wins.
    state.set_position(ListPosition::Top);
    let resolved = layout(&source, &mut state, 3).resolved;
    assert_eq!(source.take_built(), vec![100, 101]);
    assert_eq!(state.item_height(&101), Some(2));
    assert_eq!(resolved.total_height, 13);
    assert!(state.update_measured(MeasuredListItem::new(3, 0, WIDTH, 0, 1)));
    assert!(!state.update_measured(MeasuredListItem::new(77, 0, WIDTH, 0, 1)));
}

#[test]
fn removed_anchor_remaps_deterministically_in_uncontrolled_mode() {
    let mut source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 3);
    state.set_position(ListPosition::Anchored(ListAnchor::row(4, 0)));
    assert_eq!(layout(&source, &mut state, 3).resolved.top_offset, 4);

    // Remove the anchor and the item before it: the anchor moves to the item
    // now occupying its adjusted index (the next survivor).
    source.items.drain(3..5);
    state.note_splice(3..5, 0);
    assert_eq!(layout(&source, &mut state, 3).resolved.top_offset, 3);
    assert_eq!(state.position(), ListPosition::Anchored(ListAnchor::top(5)));
    assert!(!state.anchor_missing());

    // Remove everything after the new anchor's predecessor: clamps to the last.
    source.items.truncate(3);
    state.note_splice(3..8, 0);
    let resolved = layout(&source, &mut state, 3).resolved;
    assert_eq!(state.position(), ListPosition::Anchored(ListAnchor::top(2)));
    assert_eq!(resolved.top_offset, 0);

    source.items.clear();
    state.note_splice(0..3, 0);
    layout(&source, &mut state, 3);
    assert_eq!(state.position(), ListPosition::Top);
}

#[test]
fn controlled_positions_never_move_and_input_is_queued() {
    let mut source = Lazy::new(keys(0..20));
    let mut state = VirtualListState::controlled(0);
    assert!(state.is_controlled());
    assert_eq!(state.control(), PositionControl::Controlled);
    layout(&source, &mut state, 4);
    let position = ListPosition::Anchored(ListAnchor::row(8, 0));
    state.set_position(position.clone());
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 8);
    assert_eq!(state.scroll.vertical_offset(), 8);

    for action in [
        VirtualListAction::ScrollRequested {
            rows: 3,
            source: ScrollInputSource::Wheel,
        },
        VirtualListAction::PageRequested(-1),
        VirtualListAction::EdgeRequested(Edge::Bottom),
        VirtualListAction::ScrollbarDrag(0.5),
        VirtualListAction::SelectionAutoScroll(-2),
        VirtualListAction::Activated(3),
    ] {
        assert!(!state.apply_action(&action));
        assert_eq!(
            state.dispatch_action(action, &NoSourceMap),
            ActionDispatch::Queued
        );
    }
    assert_eq!(state.position(), position);
    assert_eq!(state.pending_actions().len(), 6);
    assert_eq!(state.drain_actions().count(), 6);
    assert_eq!(state.pending_actions(), []);

    // Legacy offset writes do not override a controlled position.
    state.scroll.set_vertical_offset(0);
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 8);

    // Membership changes re-derive rows but keep the position.
    source.items.splice(0..0, keys(100..103));
    state.note_splice(0..0, 3);
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 11);
    assert_eq!(state.position(), position);

    // A missing anchor reuses the previous top, clamped, and reports it.
    source.items.retain(|item| item.key != 8);
    state.note_splice(11..12, 0);
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 11);
    assert!(state.anchor_missing());
    assert_eq!(state.position(), position);
    source.items.truncate(6);
    state.note_splice(6..22, 0);
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 2);
    assert!(state.anchor_missing());
    state.set_position(ListPosition::FollowBottom);
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 2);
    assert!(!state.anchor_missing());
    assert!(state.scroll.follows_bottom());
}

#[test]
fn uncontrolled_actions_follow_overscroll_and_release() {
    let source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 4);
    let scroll = |rows| VirtualListAction::ScrollRequested {
        rows,
        source: ScrollInputSource::Key,
    };

    assert!(!state.apply_action(&scroll(0)));
    assert!(state.apply_action(&scroll(3)));
    assert_eq!(state.position(), ListPosition::Anchored(ListAnchor::top(3)));
    // Ending exactly at max_content_top enters FollowBottom.
    assert!(state.apply_action(&scroll(3)));
    assert_eq!(state.position(), ListPosition::FollowBottom);
    assert!(state.scroll.follows_bottom());
    assert_eq!(state.scroll.vertical_offset(), 6);
    // Further downward input without overscroll changes nothing.
    assert!(!state.apply_action(&scroll(2)));
    assert_eq!(
        state.dispatch_action(scroll(2), &NoSourceMap),
        ActionDispatch::Unchanged
    );
    // Upward input releases.
    assert_eq!(
        state.dispatch_action(scroll(-1), &NoSourceMap),
        ActionDispatch::Applied
    );
    assert_eq!(state.position(), ListPosition::Anchored(ListAnchor::top(5)));

    // With overscroll, moving past max_content_top enters Overscrolled.
    state.set_overscroll(Overscroll::ViewportMinus(1));
    assert_eq!(state.overscroll(), Overscroll::ViewportMinus(1));
    assert!(state.apply_action(&scroll(1)));
    assert_eq!(state.position(), ListPosition::FollowBottom);
    assert!(state.apply_action(&scroll(2)));
    assert_eq!(
        state.position(),
        ListPosition::Overscrolled(ListAnchor::top(8))
    );
    assert!(state.apply_action(&scroll(100)));
    assert_eq!(state.scroll.vertical_offset(), 9);
    assert_eq!(
        layout(&source, &mut state, 4).resolved.blank_rows_below(),
        3
    );

    // Pages and edges.
    state.set_page_step(PageStep::ViewportMinus(1));
    assert_eq!(state.page_step(), PageStep::ViewportMinus(1));
    assert!(state.apply_action(&VirtualListAction::PageRequested(-2)));
    assert_eq!(state.scroll.vertical_offset(), 3);
    assert!(state.apply_action(&VirtualListAction::EdgeRequested(Edge::Bottom)));
    assert_eq!(state.position(), ListPosition::FollowBottom);
    assert!(state.apply_action(&VirtualListAction::EdgeRequested(Edge::Top)));
    assert_eq!(state.position(), ListPosition::Top);
    assert!(!state.apply_action(&VirtualListAction::EdgeRequested(Edge::Top)));
    assert!(state.apply_action(&VirtualListAction::SelectionAutoScroll(2)));
    assert_eq!(state.scroll.vertical_offset(), 2);

    // Activation is queued, never applied.
    assert!(!state.apply_action(&VirtualListAction::Activated(4)));
    assert_eq!(
        state.dispatch_action(VirtualListAction::Activated(4), &NoSourceMap),
        ActionDispatch::Queued
    );
    assert_eq!(
        state.drain_actions().collect::<Vec<_>>(),
        vec![VirtualListAction::Activated(4)]
    );
}

#[test]
fn scrollbar_drag_uses_exact_or_estimated_model() {
    let source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 4);
    assert!(state.apply_action(&VirtualListAction::ScrollbarDrag(0.5)));
    assert_eq!(state.scroll.vertical_offset(), 3);
    assert!(state.apply_action(&VirtualListAction::ScrollbarDrag(1.0)));
    assert_eq!(state.position(), ListPosition::FollowBottom);
    assert!(state.apply_action(&VirtualListAction::ScrollbarDrag(f64::NAN)));
    assert_eq!(state.scroll.vertical_offset(), 0);

    // With an estimate, the resident window spans estimated rows 90..100.
    let estimate = PositionEstimate {
        before: 90,
        resident: 10,
        after: 0,
    };
    state.set_position_estimate(Some(estimate));
    assert_eq!(state.position_estimate(), Some(estimate));
    assert!(!state.apply_action(&VirtualListAction::ScrollbarDrag(0.2)));
    assert_eq!(state.scroll.vertical_offset(), 0);
    assert!(state.apply_action(&VirtualListAction::ScrollbarDrag(94.0 / 96.0)));
    assert_eq!(state.scroll.vertical_offset(), 4);
    // An estimate whose resident rows disagree with the exact total is ignored.
    state.set_position_estimate(Some(PositionEstimate {
        before: 90,
        resident: 3,
        after: 0,
    }));
    assert!(state.apply_action(&VirtualListAction::ScrollbarDrag(0.0)));
    assert_eq!(state.scroll.vertical_offset(), 0);
}

#[test]
fn follow_bottom_survives_growth_and_release_is_explicit() {
    let mut source = Lazy::new(keys(0..5));
    let mut state = VirtualListState::new(0);
    state.set_position(ListPosition::FollowBottom);
    assert_eq!(layout(&source, &mut state, 3).resolved.top_offset, 2);
    source.items.extend(keys(5..9));
    state.note_splice(5..5, 4);
    assert_eq!(layout(&source, &mut state, 3).resolved.top_offset, 6);
    assert_eq!(state.position(), ListPosition::FollowBottom);
    // Shrinking content does not release either.
    source.items.truncate(2);
    state.note_splice(2..9, 0);
    assert_eq!(layout(&source, &mut state, 3).resolved.top_offset, 0);
    assert_eq!(state.position(), ListPosition::FollowBottom);
    source.items.extend(keys(20..30));
    state.note_splice(2..2, 10);
    assert_eq!(layout(&source, &mut state, 3).resolved.top_offset, 9);

    assert!(state.release_follow());
    assert!(!state.release_follow());
    assert_eq!(
        state.position(),
        ListPosition::Anchored(ListAnchor::top(27))
    );
    source.items.extend(keys(40..42));
    state.note_splice(12..12, 2);
    assert_eq!(layout(&source, &mut state, 3).resolved.top_offset, 9);
}

#[test]
fn overscroll_bounds_hold_across_shrink_and_grow_without_drift() {
    let mut source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(0);
    state.set_overscroll(Overscroll::ViewportMinus(1));
    state.set_position(ListPosition::Overscrolled(ListAnchor::top(8)));
    let resolved = layout(&source, &mut state, 4).resolved;
    assert_eq!(resolved.top_offset, 8);
    assert_eq!(resolved.max_overscroll_top, 9);
    assert!(resolved.is_overscrolled());

    // The anchored item grows: still anchored at its top.
    source.items[8] = Spec {
        key: 8,
        revision: 1,
        units: 40,
    };
    state.invalidate(&8);
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 8);
    // Content after it is removed: clamps to the new bound.
    source.items.truncate(9);
    state.note_splice(9..10, 0);
    let resolved = layout(&source, &mut state, 4).resolved;
    assert_eq!(resolved.top_offset, 8);
    assert_eq!(resolved.max_overscroll_top, 12);
    source.items[8] = spec(8, 8);
    state.invalidate(&8);
    let shrunk = layout(&source, &mut state, 4).resolved;
    assert_eq!(shrunk.top_offset, 8);
    assert_eq!(shrunk.max_overscroll_top, 8);
    assert_eq!(shrunk.blank_rows_below(), 3);
    // Growing back restores the same anchor row, with no drift.
    source.items.push(spec(9, 8));
    state.note_splice(9..9, 1);
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 8);
    assert_eq!(
        state.position(),
        ListPosition::Overscrolled(ListAnchor::top(8))
    );

    // Repeated layouts are idempotent.
    source.take_built();
    let generation = state.measurement_generation();
    let first = layout(&source, &mut state, 4);
    for _ in 0..5 {
        assert_eq!(layout(&source, &mut state, 4), first);
        assert_eq!(state.measurement_generation(), generation);
        assert_eq!(source.take_built(), NONE);
    }

    // Resizing the viewport re-derives the bound without moving the anchor.
    assert_eq!(layout(&source, &mut state, 2).resolved.top_offset, 8);
    assert_eq!(layout(&source, &mut state, 8).resolved.top_offset, 8);
    // Disabling overscroll clamps to content.
    state.set_overscroll(Overscroll::None);
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 6);
    state.set_overscroll(Overscroll::ViewportMinus(1));
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 8);
}

#[test]
fn legacy_offset_writes_are_adopted_as_keyed_positions() {
    let mut source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(1);
    layout(&source, &mut state, 3);
    state.scroll.set_vertical_offset(5);
    assert_eq!(
        state.position(),
        ListPosition::Anchored(ListAnchor::row(2, 1))
    );
    source.items.splice(0..0, keys(50..52));
    state.note_splice(0..0, 2);
    assert_eq!(layout(&source, &mut state, 3).resolved.top_offset, 9);

    assert!(state.scroll_by(100, 3));
    assert_eq!(state.scroll.vertical_offset(), 20);
    state.set_overscroll(Overscroll::Rows(2));
    assert!(state.scroll_by(100, 3));
    assert_eq!(state.scroll.vertical_offset(), 22);
    assert!(matches!(state.position(), ListPosition::Overscrolled(_)));
    state.scroll_to_bottom(3);
    assert_eq!(state.position(), ListPosition::FollowBottom);
    state.scroll_to_top();
    assert_eq!(
        state.position(),
        ListPosition::Anchored(ListAnchor::top(50))
    );
}

#[test]
fn anchor_for_and_policies_through_state() {
    let source = Lazy::new([spec(0, 8), spec(1, 8), spec(2, 80), spec(3, 8)]);
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 4);
    let tall = state
        .anchor_for(&2, AnchorPolicy::ItemBottomAtBottom, 4, &NoSourceMap)
        .unwrap();
    assert_eq!(tall, ListPosition::Anchored(ListAnchor::top(2)));
    let visible = state
        .anchor_for(
            &1,
            AnchorPolicy::EnsureVisibleFrom(Edge::Top),
            4,
            &NoSourceMap,
        )
        .unwrap();
    assert_eq!(visible, ListPosition::Anchored(ListAnchor::top(0)));
    state.set_position(tall);
    assert_eq!(layout(&source, &mut state, 4).resolved.top_offset, 2);
    assert!(
        state
            .anchor_for(&9, AnchorPolicy::Center, 4, &NoSourceMap)
            .is_none()
    );
}

/// Source map whose rows per source line follow the synchronized width.
struct Lines {
    rows_per_line: Cell<u64>,
}

impl SourceMap<u64, u64> for Lines {
    fn row_for_source(&self, _key: &u64, line: &u64) -> Option<u64> {
        Some(line * self.rows_per_line.get())
    }

    fn source_for_row(&self, _key: &u64, row: u64) -> Option<(u64, u64)> {
        let line = row / self.rows_per_line.get();
        Some((line, line * self.rows_per_line.get()))
    }
}

#[test]
fn source_anchors_survive_reflow_through_the_state() {
    // Each item has eight one-unit lines: eight rows at width 8, sixteen at 4.
    let source = Lazy::new(keys(0..6).into_iter().map(|item| spec(item.key, 64)));
    let lines = Lines {
        rows_per_line: Cell::new(1),
    };
    let mut state: VirtualListState<u64, u64> =
        VirtualListState::with_control(0, PositionControl::Uncontrolled);
    let list = VirtualSourceList::new("lazy", &source);
    let mut cx = LayoutCx::new();
    list.layout(
        8,
        LayoutEnvironment::default(),
        3,
        &lines,
        &mut state,
        &mut cx,
    );
    assert!(state.apply_action_with(
        &VirtualListAction::ScrollRequested {
            rows: 8 + 5,
            source: ScrollInputSource::Wheel,
        },
        &lines,
    ));
    assert_eq!(
        state.position(),
        ListPosition::Anchored(ListAnchor::new(
            1,
            AnchorOffset::Source {
                position: 5,
                row_delta: 0
            }
        ))
    );
    lines.rows_per_line.set(2);
    let resolved = list.layout(
        4,
        LayoutEnvironment::default(),
        3,
        &lines,
        &mut state,
        &mut cx,
    );
    assert_eq!(resolved.resolved.top_offset, 16 + 10);
}

#[test]
fn window_splices_and_tiny_lists() {
    let mut source = Lazy::new(keys(0..3));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 0);
    // A zero-row viewport resolves but paints nothing.
    assert_eq!(paint_rows(&source, &state, 0), Vec::<String>::new());
    source.items.splice(0..0, keys(10..12));
    state.note_window_splice(&WindowSplice {
        at: 0,
        removed: Vec::new(),
        inserted: vec![10, 11],
    });
    layout(&source, &mut state, 2);
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 0));

    // Single item taller than the viewport.
    let tall = Lazy::new([spec(0, 80)]);
    let mut state = VirtualListState::new(0);
    state.set_position(ListPosition::FollowBottom);
    assert_eq!(layout(&tall, &mut state, 4).resolved.top_offset, 6);
    assert_eq!(paint_rows(&tall, &state, 4), vec!["00000000"; 4]);

    // Zero-height items occupy no rows and are never painted.
    let zeros = Lazy::new([spec(0, 0), spec(1, 8), spec(2, 0)]);
    let mut state = VirtualListState::new(0);
    let laid = layout(&zeros, &mut state, 3);
    assert_eq!(laid.resolved.total_height, 1);
    assert_eq!(paint_rows(&zeros, &state, 3)[0], "11111111");
    assert_eq!(zeros.take_built().len(), 3 + 1);

    // Empty source.
    let empty = Lazy::new([]);
    let mut state = VirtualListState::new(0);
    let laid = layout(&empty, &mut state, 3);
    assert_eq!(laid.resolved.top_offset, 0);
    assert_eq!(laid.visible.start..laid.visible.end, 0..0);
    assert!(
        !state.apply_action(&VirtualListAction::ScrollRequested {
            rows: 3,
            source: ScrollInputSource::Key,
        }) || state.position() == ListPosition::FollowBottom
    );
}

#[test]
fn eager_builder_shares_the_lazy_implementation() {
    let source = Lazy::new(keys(0..12));
    let eager = (0..12u64).fold(VirtualList::new("lazy"), |list, key| {
        list.item(key, 0, Unit { key, units: 8 })
    });
    let mut lazy_state = VirtualListState::new(0);
    let mut eager_state = VirtualListState::new(0);
    layout(&source, &mut lazy_state, 4);
    let mut cx = LayoutCx::new();
    eager.layout(
        WIDTH,
        LayoutEnvironment::default(),
        4,
        &NoSourceMap,
        &mut eager_state,
        &mut cx,
    );
    for state in [&mut lazy_state, &mut eager_state] {
        state.apply_action(&VirtualListAction::ScrollRequested {
            rows: 5,
            source: ScrollInputSource::Wheel,
        });
    }
    let area = Rect::new(0, 0, 8, 4);
    let mut eager_buffer = Buffer::empty(area);
    let stats = eager.paint(
        area,
        &eager_state,
        &mut PaintCx::new(&mut Frame::new(&mut eager_buffer)),
    );
    assert_eq!(stats.painted_items, 4);
    assert_eq!(stats.built_items, 4);
    let lazy_rows = paint_rows(&source, &lazy_state, 4);
    for row in 0..4u16 {
        assert_eq!(
            eager_buffer.row_symbols(row).unwrap(),
            lazy_rows[usize::from(row)]
        );
        assert_eq!(
            eager_buffer.get(Point::new(0, row)).unwrap().symbol,
            (5 + row).to_string()
        );
    }
    // The eager builder compares every key at each layout, without measuring.
    eager.layout(
        WIDTH,
        LayoutEnvironment::default(),
        4,
        &NoSourceMap,
        &mut eager_state,
        &mut cx,
    );
    assert_eq!(eager_state.render_stats().measured_items, 0);
    assert_eq!(eager_state.scroll.vertical_offset(), 5);
}

#[cfg(not(debug_assertions))]
#[test]
fn unnoted_length_changes_fall_back_to_full_reconciliation() {
    let mut source = Lazy::new(keys(0..5));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 3);
    source.items.extend(keys(5..8));
    layout(&source, &mut state, 3);
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 0));
}

#[test]
fn shrinking_caller_heights_measure_every_revealed_item_in_one_layout() {
    // Every item lays out at one row, but the caller first indexes each at
    // 100 rows: each re-measurement reveals the next item.
    let source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(0);
    state
        .splice_measured(
            0..0,
            (0..10).map(|key| MeasuredListItem::new(key, 0, WIDTH, 0, 100)),
        )
        .unwrap();
    let laid = layout(&source, &mut state, 20);
    assert_eq!(laid.visible.start..laid.visible.end, 0..10);
    assert_eq!(laid.resolved.total_height, 10);
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 0));
    assert_eq!(source.take_built().len(), 10, "each item measured once");
    // Every visible item has a retained layout, so paint draws all of them.
    let rows = paint_rows(&source, &state, 20);
    for (row, painted) in rows.iter().take(10).enumerate() {
        assert_eq!(*painted, row.to_string().repeat(8));
    }
    assert_eq!(state.render_stats().painted_items, 10);

    // A tall viewport over a shrinking tail behaves the same from the bottom.
    let source = Lazy::new(keys(0..6));
    let mut state = VirtualListState::new(1);
    state
        .splice_measured(
            0..0,
            (0..6).map(|key| MeasuredListItem::new(key, 0, WIDTH, 0, 50)),
        )
        .unwrap();
    state.set_position(ListPosition::FollowBottom);
    let laid = layout(&source, &mut state, 40);
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 1));
    assert_eq!(laid.visible.start..laid.visible.end, 0..6);
    assert_eq!(laid.resolved.top_offset, 0);
}

#[test]
fn a_mismatched_visible_key_reconciles_within_the_same_layout() {
    let mut source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 4);
    // The source replaces a visible item without noting it (same length).
    source.items[2] = spec(77, 16);
    let laid = layout(&source, &mut state, 4);
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 0));
    assert_eq!(laid.resolved.total_height, 11);
    assert_eq!(
        paint_rows(&source, &state, 4),
        ["00000000", "11111111", "77777777", "77777777"]
    );
}

#[test]
fn disjoint_notes_read_only_noted_rows() {
    let mut source = Lazy::new(keys(0..10_000));
    let mut state = VirtualListState::new(0);
    state.set_position(ListPosition::Anchored(ListAnchor::top(5_000)));
    layout(&source, &mut state, 5);
    source.take_built();

    // A history prepend and a live append noted in the same frame.
    source.items.insert(0, spec(20_000, 8));
    state.note_splice(0..0, 1);
    source.items.push(spec(20_001, 8));
    state.note_splice(10_001..10_001, 1);
    source.reads.set(0);
    let laid = layout(&source, &mut state, 5);
    let reads = source.reads.get();
    assert!(reads < 64, "{reads} key reads for two noted rows");
    let mut built = source.take_built();
    built.sort_unstable();
    assert_eq!(built, vec![20_000, 20_001]);
    assert_eq!(laid.resolved.top_offset, 5_001);
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 0));

    // An eviction trimming both ends, as resident windows report it.
    source.items.drain(9_000..);
    state.note_window_splice(&WindowSplice {
        at: 9_000,
        removed: (9_000..10_002).map(|_| 0).collect(),
        inserted: Vec::new(),
    });
    source.items.drain(..1_000);
    state.note_window_splice(&WindowSplice {
        at: 0,
        removed: (0..1_000).map(|_| 0).collect(),
        inserted: Vec::new(),
    });
    source.reads.set(0);
    let laid = layout(&source, &mut state, 5);
    assert!(source.reads.get() < 64);
    assert_eq!(source.take_built(), NONE);
    assert_eq!(laid.resolved.top_offset, 4_001);
    assert_eq!(*state.index(), expected_index(&source, WIDTH, 0));
}

#[test]
fn many_disjoint_notes_compose_like_a_rebuild() {
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound.max(1)
    };
    let mut source = Lazy::new(keys(0..200));
    let mut state = VirtualListState::new(1);
    let mut fresh = 100_000;
    layout(&source, &mut state, 9);
    for round in 0..200 {
        let synchronized = state.index().clone();
        let mut removed_pool = Vec::new();
        for _ in 0..=next(24) {
            let len = u64::try_from(source.items.len()).unwrap();
            let start = usize::try_from(next(len + 1)).unwrap();
            let end = (start + usize::try_from(next(3)).unwrap()).min(source.items.len());
            let mut items = (0..next(3))
                .map(|_| {
                    fresh += 1;
                    spec(fresh, next(20))
                })
                .collect::<Vec<_>>();
            // Sometimes move a previously removed item elsewhere.
            if next(3) == 0
                && let Some(moved) = removed_pool.pop()
            {
                items.push(moved);
            }
            let count = items.len();
            removed_pool.extend(source.items.splice(start..end, items));
            state.note_splice(start..end, count);
            // Every unreplaced synchronized row maps to the same key.
            for index in 0..synchronized.len() {
                if let Some(position) = state.pending.source_row(index) {
                    assert_eq!(
                        source.items[position].key,
                        synchronized.item(index).unwrap().key,
                        "round {round}"
                    );
                }
            }
            assert_eq!(state.expected_source_len(), source.items.len());
        }
        layout(&source, &mut state, 9);
        assert_eq!(
            *state.index(),
            expected_index(&source, WIDTH, 1),
            "round {round}"
        );
    }
}

#[test]
fn paint_after_a_noted_prepend_draws_the_laid_out_items() {
    let mut source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(0);
    state.set_position(ListPosition::Anchored(ListAnchor::top(4)));
    layout(&source, &mut state, 3);
    let before = paint_rows(&source, &state, 3);
    assert_eq!(before, ["44444444", "55555555", "66666666"]);
    // Noted but not yet laid out: the old layout still paints the same keys.
    source.items.splice(0..0, keys(20..23));
    state.note_splice(0..0, 3);
    assert_eq!(paint_rows(&source, &state, 3), before);
    // An item replaced without a note is skipped rather than painted with
    // another item's layout.
    source.items[8] = spec(99, 8);
    assert_eq!(
        paint_rows(&source, &state, 3),
        ["44444444", "        ", "66666666"]
    );
    state.invalidate_all();
    layout(&source, &mut state, 3);
    assert_eq!(
        paint_rows(&source, &state, 3),
        ["44444444", "99999999", "66666666"]
    );
}

#[test]
fn estimated_content_after_the_window_never_enters_follow() {
    let source = Lazy::new(keys(0..10));
    let mut state = VirtualListState::new(0);
    layout(&source, &mut state, 4);
    state.set_position_estimate(Some(PositionEstimate {
        before: 0,
        resident: 10,
        after: 10_000,
    }));
    // A drag into the unloaded tail lands on the resident bottom, anchored.
    assert!(state.apply_action(&VirtualListAction::ScrollbarDrag(0.9)));
    assert_eq!(state.scroll.vertical_offset(), 6);
    assert!(!state.position().is_follow_bottom());
    assert_eq!(
        state.drain_actions().collect::<Vec<_>>(),
        [VirtualListAction::ScrollbarDrag(0.9)]
    );
    // Wheel and End reach the resident bottom without following.
    state.apply_action(&VirtualListAction::EdgeRequested(Edge::Top));
    assert_eq!(state.pending_actions(), []);
    state.apply_action(&VirtualListAction::ScrollRequested {
        rows: 6,
        source: ScrollInputSource::Wheel,
    });
    assert_eq!(state.scroll.vertical_offset(), 6);
    assert!(!state.position().is_follow_bottom());
    state.apply_action(&VirtualListAction::ScrollRequested {
        rows: -6,
        source: ScrollInputSource::Wheel,
    });
    assert!(state.apply_action(&VirtualListAction::EdgeRequested(Edge::Bottom)));
    assert!(!state.position().is_follow_bottom());
    assert_eq!(
        state.drain_actions().collect::<Vec<_>>(),
        [VirtualListAction::EdgeRequested(Edge::Bottom)]
    );
    // A resident drag queues nothing.
    assert!(state.apply_action(&VirtualListAction::ScrollbarDrag(0.0)));
    assert_eq!(state.pending_actions(), []);

    // Once the tail is resident, the bottom follows again.
    state.set_position_estimate(Some(PositionEstimate {
        before: 50,
        resident: 10,
        after: 0,
    }));
    assert!(state.apply_action(&VirtualListAction::EdgeRequested(Edge::Bottom)));
    assert_eq!(state.position(), ListPosition::FollowBottom);
    assert_eq!(state.pending_actions(), []);
    assert!(state.apply_action(&VirtualListAction::EdgeRequested(Edge::Top)));
    assert_eq!(
        state.drain_actions().collect::<Vec<_>>(),
        [VirtualListAction::EdgeRequested(Edge::Top)]
    );
}
