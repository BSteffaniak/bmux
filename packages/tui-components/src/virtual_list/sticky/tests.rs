use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::buffer::Buffer;
use bmux_tui::component::{
    Component, Constraints, Element, EventCx, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::hit::HitRole;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::Line;

use super::StickyHeaders;
use crate::virtual_list::{
    AnchorPolicy, Edge, ItemFocusTarget, ListAnchor, ListPosition, NoSourceMap, PinnedPlacement,
    VirtualItemSource, VirtualList, VirtualListAction, VirtualListChrome, VirtualListComponent,
    VirtualListState,
};

/// A fixed-height block painting `"{key}:{row}"` on each row.
struct Block {
    key: u64,
    height: u64,
    handles: bool,
}

impl Component for Block {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("block"),
            LogicalSize::new(constraints.max_width(), self.height),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        for row in 0..self.height {
            cx.write_line(
                LocalRect::new(0, i64::try_from(row).unwrap_or(i64::MAX), width, 1),
                &Line::raw(format!("{}:{row}", self.key)),
            );
        }
    }

    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        if self.handles {
            EventOutcome::Handled
        } else {
            EventOutcome::Ignored
        }
    }
}

#[derive(Clone)]
enum Body {
    /// A [`Block`] of this many rows.
    Rows(u64),
    /// Word-wrapped text.
    Text(&'static str),
}

#[derive(Clone)]
struct Entry {
    key: u64,
    level: Option<u16>,
    parent: Option<usize>,
    body: Body,
    revision: u64,
    handles: bool,
}

type Spec = (u64, Option<u16>, Body);

/// A lazy sectioned document; parents are derived from header levels.
struct Doc {
    entries: RefCell<Vec<Entry>>,
    builds: Cell<usize>,
    /// Keys built as pinned headers, with their placement, in build order.
    pinned_builds: RefCell<Vec<(u64, PinnedPlacement)>>,
}

const fn header(key: u64, level: u16, rows: u64) -> Spec {
    (key, Some(level), Body::Rows(rows))
}

const fn line(key: u64) -> Spec {
    (key, None, Body::Rows(1))
}

impl Doc {
    fn new(spec: Vec<Spec>) -> Self {
        let doc = Self {
            entries: RefCell::new(Vec::new()),
            builds: Cell::new(0),
            pinned_builds: RefCell::new(Vec::new()),
        };
        doc.replace(spec);
        doc
    }

    fn replace(&self, spec: Vec<Spec>) {
        let mut open: Vec<(usize, u16)> = Vec::new();
        let entries = spec
            .into_iter()
            .enumerate()
            .map(|(position, (key, level, body))| {
                if let Some(level) = level {
                    open.retain(|&(_, open_level)| open_level < level);
                }
                let parent = open.last().map(|&(parent, _)| parent);
                if let Some(level) = level {
                    open.push((position, level));
                }
                Entry {
                    key,
                    level,
                    parent,
                    body,
                    revision: 0,
                    handles: false,
                }
            })
            .collect();
        *self.entries.borrow_mut() = entries;
    }

    fn update(&self, key: u64, change: impl FnOnce(&mut Entry)) {
        let mut entries = self.entries.borrow_mut();
        let entry = entries
            .iter_mut()
            .find(|entry| entry.key == key)
            .expect("entry exists");
        change(entry);
    }
}

impl VirtualItemSource<u64> for Doc {
    fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    fn key(&self, index: usize) -> u64 {
        self.entries.borrow()[index].key
    }

    fn layout_revision(&self, index: usize) -> u64 {
        self.entries.borrow()[index].revision
    }

    fn build(&self, index: usize) -> Element<'_> {
        self.builds.set(self.builds.get() + 1);
        let entry = self.entries.borrow()[index].clone();
        match entry.body {
            Body::Rows(height) => Element::new(Block {
                key: entry.key,
                height,
                handles: entry.handles,
            }),
            Body::Text(text) => Element::new(TextBlock::new(text)),
        }
    }

    fn build_pinned(&self, index: usize, placement: PinnedPlacement) -> Element<'_> {
        let key = self.entries.borrow()[index].key;
        self.pinned_builds.borrow_mut().push((key, placement));
        self.build(index)
    }

    fn section_header_level(&self, index: usize) -> Option<u16> {
        self.entries.borrow()[index].level
    }

    fn section_parent(&self, index: usize) -> Option<usize> {
        self.entries.borrow()[index].parent
    }
}

/// Two groups of one-row headers: group 0 (subgroups 1 and 7) and group 12
/// (subgroup 13). Every item is one row, so item keys equal collection rows.
fn single_row_spec() -> Vec<Spec> {
    let mut spec = vec![header(0, 0, 1), header(1, 1, 1)];
    spec.extend((2..=6).map(line));
    spec.push(header(7, 1, 1));
    spec.extend((8..=11).map(line));
    spec.push(header(12, 0, 1));
    spec.push(header(13, 1, 1));
    spec.extend((14..=23).map(line));
    spec
}

fn single_row_doc() -> Doc {
    Doc::new(single_row_spec())
}

/// Two-row headers. Rows: group 0 at 0-1, subgroup 1 at 2-3, lines 2-4 at 4-6,
/// subgroup 5 at 7-8, lines 6-9 at 9-12, group 10 at 13-14, lines 11-14 at 15-18.
fn tall_header_doc() -> Doc {
    let mut spec = vec![header(0, 0, 2), header(1, 1, 2)];
    spec.extend((2..=4).map(line));
    spec.push(header(5, 1, 2));
    spec.extend((6..=9).map(line));
    spec.push(header(10, 0, 2));
    spec.extend((11..=14).map(line));
    Doc::new(spec)
}

type State = RefCell<VirtualListState<u64>>;

fn list<'a>(doc: &'a Doc, state: &'a State) -> VirtualListComponent<'a, u64, Doc> {
    VirtualListComponent::new("list", doc, state).chrome(VirtualListChrome::bare())
}

fn layout(component: &impl Component, width: u16, height: u16) -> LayoutNode {
    component.layout(
        Constraints::tight(Size::new(width, height)),
        &mut LayoutCx::new(),
    )
}

struct Painted {
    rows: Vec<String>,
    /// `HitRole::ListItem` regions by id.
    items: BTreeMap<String, Rect>,
}

fn paint(component: &impl Component, tree: &LayoutNode) -> Painted {
    let width = u16::try_from(tree.size.width).unwrap_or(u16::MAX);
    let height = u16::try_from(tree.size.height).unwrap_or(u16::MAX);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    component.paint(tree, &mut PaintCx::new(&mut frame));
    let mut items = BTreeMap::new();
    for region in frame.hits().regions() {
        if region.role == HitRole::ListItem {
            assert!(
                items
                    .insert(region.id.as_str().to_owned(), region.area)
                    .is_none(),
                "one list-item region per item"
            );
        }
    }
    frame.buffer().debug_assert_valid_wide_spans();
    let rows = (0..height)
        .map(|y| {
            frame
                .buffer()
                .row_symbols(y)
                .unwrap_or_default()
                .trim_end()
                .to_owned()
        })
        .collect();
    Painted { rows, items }
}

fn anchor(state: &State, key: u64) {
    state
        .borrow_mut()
        .set_position(ListPosition::Anchored(ListAnchor::top(key)));
}

fn rows(state: &State, doc: &Doc, top_key: u64, width: u16, height: u16) -> Vec<String> {
    anchor(state, top_key);
    let component = list(doc, state);
    let tree = layout(&component, width, height);
    paint(&component, &tree).rows
}

fn sticky_children(tree: &LayoutNode) -> Vec<(String, u64, u64)> {
    tree.children
        .iter()
        .find(|child| child.node.id.as_str() == "list.sticky")
        .map(|sticky| {
            sticky
                .node
                .children
                .iter()
                .map(|child| {
                    (
                        child.node.id.as_str().to_owned(),
                        child.y,
                        child.node.size.height,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

const fn rect(y: u16, width: u16, height: u16) -> Rect {
    Rect::new(0, y, width, height)
}

fn items(entries: &[(&str, Rect)]) -> BTreeMap<String, Rect> {
    entries
        .iter()
        .map(|(id, rect)| ((*id).to_owned(), *rect))
        .collect()
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
    Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
}

fn click(component: &impl Component, tree: &LayoutNode, x: u16, y: u16) {
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        component.event(&mouse(kind, x, y), tree, &mut EventCx::new(tree));
    }
}

#[test]
fn nested_headers_pin_and_scrolled_rows_under_them_neither_paint_nor_register() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 4);
    let component = list(&doc, &state);
    let tree = layout(&component, 8, 5);
    let painted = paint(&component, &tree);
    // Rows 4 and 5 (keys 4 and 5) lie under the pinned group and subgroup headers.
    assert_eq!(painted.rows, ["0:0", "1:0", "6:0", "7:0", "8:0"]);
    assert_eq!(
        painted.items,
        items(&[
            ("list.item.0", rect(0, 8, 1)),
            ("list.item.1", rect(1, 8, 1)),
            ("list.item.6", rect(2, 8, 1)),
            ("list.item.7", rect(3, 8, 1)),
            ("list.item.8", rect(4, 8, 1)),
        ])
    );
    assert_eq!(
        sticky_children(&tree),
        [
            ("list.sticky.0".to_owned(), 0, 1),
            ("list.sticky.1".to_owned(), 1, 1)
        ]
    );
    assert_eq!(
        tree.find_logical_rect(&component.sticky_id())
            .map(|rect| rect.height),
        Some(2)
    );
    assert_eq!(state.borrow().pinned_header_keys(), [0, 1]);
    assert_eq!(state.borrow().sticky_header_rows(), 2);
    // The scrolled position is unchanged: the window still starts at row 4.
    assert_eq!(
        state
            .borrow()
            .resolved()
            .map(|resolved| resolved.top_offset),
        Some(4)
    );
    // Paint work: three scrolled items plus two pinned headers.
    let report = state.borrow().render_stats();
    assert_eq!((report.painted_items, report.registered_items), (5, 5));
}

#[test]
fn headers_at_their_natural_row_are_not_duplicated() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::new(0));
    assert_eq!(
        rows(&state, &doc, 0, 8, 4),
        ["0:0", "1:0", "2:0", "3:0"],
        "nothing pins at the top"
    );
    assert_eq!(state.borrow().pinned_header_keys(), [] as [u64; 0]);
    // Subgroup 7 arrives directly under the pinned group header.
    assert_eq!(rows(&state, &doc, 6, 8, 4), ["0:0", "7:0", "8:0", "9:0"]);
    assert_eq!(state.borrow().pinned_header_keys(), [0]);
    // One row later it pins in place of subgroup 1.
    assert_eq!(rows(&state, &doc, 7, 8, 4), ["0:0", "7:0", "9:0", "10:0"]);
    assert_eq!(state.borrow().pinned_header_keys(), [0, 7]);
    // The next group's header arrives under the pinned group header, and then
    // replaces it.
    assert_eq!(
        rows(&state, &doc, 11, 8, 4),
        ["0:0", "12:0", "13:0", "14:0"]
    );
    assert_eq!(
        rows(&state, &doc, 12, 8, 4),
        ["12:0", "13:0", "14:0", "15:0"]
    );
    assert_eq!(state.borrow().pinned_header_keys(), [] as [u64; 0]);
    assert_eq!(
        rows(&state, &doc, 14, 8, 4),
        ["12:0", "13:0", "16:0", "17:0"]
    );
}

#[test]
fn arriving_headers_push_pinned_headers_out_and_children_clip_below_parents() {
    let doc = tall_header_doc();
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 2);
    let component = list(&doc, &state);
    let tree = layout(&component, 6, 6);
    let painted = paint(&component, &tree);
    // Top row 4: subgroup 1 is pushed up by subgroup 5 (row 7) and slides under the
    // pinned group header, which hides its first row.
    assert_eq!(painted.rows, ["0:0", "0:1", "1:1", "5:0", "5:1", "6:0"]);
    assert_eq!(
        painted.items,
        items(&[
            ("list.item.0", rect(0, 6, 2)),
            ("list.item.1", rect(2, 6, 1)),
            ("list.item.5", rect(3, 6, 2)),
            ("list.item.6", rect(5, 6, 1)),
        ])
    );
    assert_eq!(
        sticky_children(&tree),
        [
            ("list.sticky.0".to_owned(), 0, 2),
            ("list.sticky.1".to_owned(), 2, 1)
        ]
    );

    // Top row 12: group 10 (row 13) pushes group 0 up by one row.
    anchor(&state, 9);
    let tree = layout(&component, 6, 6);
    let painted = paint(&component, &tree);
    assert_eq!(
        painted.rows,
        ["0:1", "10:0", "10:1", "11:0", "12:0", "13:0"]
    );
    assert_eq!(painted.items["list.item.0"], rect(0, 6, 1));
    assert_eq!(painted.items["list.item.10"], rect(1, 6, 2));
    assert_eq!(state.borrow().sticky_header_rows(), 1);
}

#[test]
fn pinned_headers_are_built_as_pinned_with_their_placement() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 4);
    let component = list(&doc, &state);
    let tree = layout(&component, 8, 5);
    doc.pinned_builds.borrow_mut().clear();
    paint(&component, &tree);
    // The group header pins above the subgroup header, which is the deepest.
    let placement = |deepest, pushed| PinnedPlacement { deepest, pushed };
    assert_eq!(
        *doc.pinned_builds.borrow(),
        [(0, placement(false, false)), (1, placement(true, false))]
    );
    // Events reach the same pinned components.
    doc.pinned_builds.borrow_mut().clear();
    click(&component, &tree, 3, 1);
    assert!(
        doc.pinned_builds
            .borrow()
            .iter()
            .all(|(key, placement)| *key == 1 && placement.deepest)
    );

    // A header pushed out by the next section says so.
    let doc = tall_header_doc();
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 9);
    let component = list(&doc, &state);
    let tree = layout(&component, 6, 6);
    doc.pinned_builds.borrow_mut().clear();
    paint(&component, &tree);
    assert_eq!(*doc.pinned_builds.borrow(), [(0, placement(true, true))]);
}

#[test]
fn pinned_headers_receive_clicks_and_report_activation() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 4);
    let component = list(&doc, &state);
    let tree = layout(&component, 8, 5);
    click(&component, &tree, 3, 1);
    click(&component, &tree, 3, 0);
    click(&component, &tree, 3, 2);
    let actions = state.borrow_mut().drain_actions().collect::<Vec<_>>();
    assert_eq!(
        actions,
        [
            VirtualListAction::Activated(1),
            VirtualListAction::Activated(0),
            VirtualListAction::Activated(6),
        ]
    );

    // With a gutter scrollbar, pinned headers cover the content width only
    // and the scrollbar still paints beside them.
    let interactive = VirtualListComponent::new("list", &doc, &state);
    let tree = layout(&interactive, 8, 5);
    let painted = paint(&interactive, &tree);
    assert_eq!(painted.items["list.item.0"], rect(0, 7, 1));
    assert_eq!(painted.items["list.item.1"], rect(1, 7, 1));
    assert!(painted.rows.iter().all(|row| row.chars().count() == 8));
    assert_eq!(
        sticky_children(&tree)[0],
        ("list.sticky.0".to_owned(), 0, 1)
    );
    assert_eq!(
        tree.find_logical_rect(&interactive.sticky_id())
            .map(|rect| (rect.x, rect.width)),
        Some((0, 7))
    );
    // An unhandled wheel over a pinned header still scrolls the list.
    let outcome = interactive.event(
        &mouse(MouseEventKind::ScrollDown, 2, 0),
        &tree,
        &mut EventCx::new(&tree),
    );
    assert_eq!(outcome, EventOutcome::Redraw);
}

#[test]
fn occluded_items_receive_no_events_and_pinned_headers_receive_keys() {
    let doc = single_row_doc();
    doc.update(5, |entry| entry.handles = true);
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 4);
    let component = list(&doc, &state);
    let tree = layout(&component, 8, 5);
    let key = Event::Key(KeyStroke::simple(KeyCode::Char('x')));
    // Item 5 (row 5) lies under the pinned subgroup header.
    assert_eq!(
        component.event(&key, &tree, &mut EventCx::new(&tree)),
        EventOutcome::Ignored
    );
    doc.update(1, |entry| entry.handles = true);
    let tree = layout(&component, 8, 5);
    assert_eq!(
        component.event(&key, &tree, &mut EventCx::new(&tree)),
        EventOutcome::Handled
    );
}

#[test]
fn narrow_viewports_pin_fewer_headers_and_never_cover_every_row() {
    let doc = tall_header_doc();
    let state = RefCell::new(VirtualListState::new(0));
    // Height 3 leaves room for the two-row group header only.
    assert_eq!(rows(&state, &doc, 2, 6, 3), ["0:0", "0:1", "4:0"]);
    assert_eq!(state.borrow().pinned_header_keys(), [0]);
    // Height 2 cannot pin a two-row header and keep a content row.
    assert_eq!(rows(&state, &doc, 2, 6, 2), ["2:0", "3:0"]);
    assert_eq!(state.borrow().pinned_header_keys(), [] as [u64; 0]);
    assert_eq!(rows(&state, &doc, 2, 6, 1), ["2:0"]);
    assert_eq!(state.borrow().pinned_header_keys(), [] as [u64; 0]);

    // Zero and tiny sizes lay out and paint without regions escaping the
    // viewport, and the pinned rows always leave a content row.
    for (width, height) in [(0, 6), (6, 0), (0, 0), (1, 6), (2, 1)] {
        anchor(&state, 2);
        let component = list(&doc, &state);
        let tree = layout(&component, width, height);
        let painted = paint(&component, &tree);
        let viewport = Rect::new(0, 0, width, height);
        for area in painted.items.values() {
            assert!(area.is_empty() || area.intersection(viewport) == *area);
        }
        assert!(state.borrow().sticky_header_rows() < u64::from(height.max(1)));
    }

    // An explicit row limit stops at the outer header.
    anchor(&state, 2);
    let limited = list(&doc, &state).sticky_headers(StickyHeaders::enabled().max_rows(3));
    let tree = layout(&limited, 6, 6);
    assert_eq!(
        paint(&limited, &tree).rows,
        ["0:0", "0:1", "4:0", "5:0", "5:1", "6:0"]
    );
    assert_eq!(state.borrow().pinned_header_keys(), [0]);
}

#[test]
fn controlled_lists_pin_without_moving_the_caller_position() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::controlled(0));
    anchor(&state, 4);
    let component = list(&doc, &state);
    let tree = layout(&component, 8, 5);
    assert_eq!(
        paint(&component, &tree).rows,
        ["0:0", "1:0", "6:0", "7:0", "8:0"]
    );
    assert_eq!(
        state.borrow().position(),
        ListPosition::Anchored(ListAnchor::top(4))
    );
    click(&component, &tree, 0, 0);
    assert_eq!(
        state.borrow_mut().drain_actions().collect::<Vec<_>>(),
        [VirtualListAction::Activated(0)]
    );
}

#[test]
fn disabled_policy_scrolls_headers_like_other_items() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 4);
    let component = list(&doc, &state).sticky_headers(StickyHeaders::disabled());
    let tree = layout(&component, 8, 3);
    assert_eq!(paint(&component, &tree).rows, ["4:0", "5:0", "6:0"]);
    assert_eq!(sticky_children(&tree), [] as [(String, u64, u64); 0]);
    assert_eq!(tree.children.len(), 1);
    assert_eq!(state.borrow().sticky_header_rows(), 0);
}

#[test]
fn eager_builder_section_headers_pin_like_lazy_sources() {
    let block = |key| Block {
        key,
        height: 1,
        handles: false,
    };
    let builder = VirtualList::new("list")
        .section_header(0, 0, 0, block(0))
        .section_header(1, 1, 0, block(1))
        .item(2, 0, block(2))
        .item(3, 0, block(3))
        .item(4, 0, block(4))
        .section_header(5, 0, 0, block(5))
        .item(6, 0, block(6));
    assert_eq!(builder.section_header_level(1), Some(1));
    assert_eq!(builder.section_header_level(2), None);
    assert_eq!(builder.section_parent(4), Some(1));
    assert_eq!(builder.section_parent(1), Some(0));
    // A new outer header closes both open sections.
    assert_eq!(builder.section_parent(5), None);
    assert_eq!(builder.section_parent(6), Some(5));
    let state = RefCell::new(VirtualListState::new(0));
    state
        .borrow_mut()
        .set_position(ListPosition::Anchored(ListAnchor::top(3)));
    let component =
        VirtualListComponent::new("list", &builder, &state).chrome(VirtualListChrome::bare());
    let tree = layout(&component, 6, 3);
    assert_eq!(paint(&component, &tree).rows, ["0:0", "1:0", "5:0"]);
}

#[test]
fn far_pinned_headers_are_remeasured_and_painting_is_bounded() {
    let mut spec = vec![header(0, 0, 1)];
    spec.extend((1..=5_000).map(line));
    let doc = Doc::new(spec);
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 4_000);
    let component = list(&doc, &state);
    let tree = layout(&component, 8, 4);
    doc.builds.set(0);
    let painted = paint(&component, &tree);
    assert_eq!(painted.rows, ["0:0", "4001:0", "4002:0", "4003:0"]);
    // Only the pinned header and the unoccluded visible items were built.
    assert_eq!(doc.builds.get(), 4);

    // The header's revision changes far above the viewport without an
    // explicit invalidation; layout re-measures it because it is pinned.
    doc.update(0, |entry| {
        entry.body = Body::Rows(2);
        entry.revision = 1;
    });
    let tree = layout(&component, 8, 4);
    assert_eq!(state.borrow().sticky_header_rows(), 2);
    assert_eq!(
        paint(&component, &tree).rows,
        ["0:0", "0:1", "4002:0", "4003:0"]
    );
}

#[test]
fn wrapped_unicode_headers_reflow_and_hit_regions_match_painted_rows() {
    const HEADER: &str = "界面 🔥 cafe\u{301} 変更 ファイル";
    let mut spec = vec![(0, Some(0), Body::Text(HEADER))];
    spec.extend((1..=40).map(line));
    let doc = Doc::new(spec);
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 20);
    let component = list(&doc, &state);
    let mut heights = Vec::new();
    for width in [40, 12, 7, 4, 1] {
        let tree = layout(&component, width, 8);
        let painted = paint(&component, &tree);
        let header_height = state.borrow().item_height(&0).expect("header is measured");
        let pinned = state.borrow().pinned_header_keys();
        if header_height <= 7 {
            assert_eq!(pinned, [0], "width {width}");
            let area = painted.items["list.item.0"];
            assert_eq!(u64::from(area.height), header_height, "width {width}");
            assert_eq!(state.borrow().sticky_header_rows(), header_height);
            // Every pinned row shows header text, and the first scrolled row
            // directly follows it.
            for row in &painted.rows[..usize::from(area.height)] {
                assert!(
                    !row.is_empty() && !row.contains(':'),
                    "width {width}: {row:?}"
                );
            }
            let first = &painted.rows[usize::from(area.height)];
            assert!(first.ends_with(":0"), "width {width}: {first:?}");
            assert!(!painted.items.contains_key("list.item.20"));
        } else {
            // Taller than the viewport allows: the header scrolls normally.
            assert!(pinned.is_empty(), "width {width}");
            assert_eq!(painted.rows[0], "2");
            assert_eq!(painted.items["list.item.20"], rect(0, width, 1));
        }
        heights.push(header_height);
        // The keyed position survives every reflow.
        assert_eq!(
            state.borrow().position(),
            ListPosition::Anchored(ListAnchor::top(20))
        );
    }
    assert!(heights.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(heights[0] < heights[3]);
    assert!(heights[4] > 7, "one-column wrapping exceeds the viewport");
}

#[test]
fn reveal_positions_and_keyed_anchors_account_for_pinned_headers() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&doc, &state);
    let _ = layout(&component, 8, 5);
    let sticky = StickyHeaders::enabled();
    let inset = |key| state.borrow().sticky_inset(&doc, &key, 5, sticky);
    assert_eq!(inset(9), Some(2));
    assert_eq!(inset(7), Some(1));
    assert_eq!(inset(0), Some(0));
    assert_eq!(inset(99), None);
    assert_eq!(state.borrow().sticky_inset(&doc, &9, 2, sticky), Some(1));

    for (key, row, sticky_rows) in [(9, 2, 2), (7, 1, 1), (13, 1, 0), (0, 0, 0)] {
        let position = state
            .borrow()
            .sticky_reveal_position(&doc, &key, 5, sticky, &NoSourceMap)
            .expect("indexed");
        state.borrow_mut().set_position(position);
        let tree = layout(&component, 8, 5);
        let painted = paint(&component, &tree);
        assert_eq!(painted.rows[row], format!("{key}:0"), "key {key}");
        assert_eq!(
            state.borrow().sticky_header_rows(),
            sticky_rows,
            "key {key}"
        );
    }

    // Inserting items above keeps the keyed anchor and its pinned headers.
    anchor(&state, 9);
    let tree = layout(&component, 8, 5);
    let before = paint(&component, &tree).rows;
    assert_eq!(before, ["0:0", "7:0", "11:0", "12:0", "13:0"]);
    let mut spec = vec![line(100), line(101)];
    spec.extend(single_row_spec());
    doc.replace(spec);
    state.borrow_mut().note_splice(0..0, 2);
    assert_eq!(state.borrow().sticky_inset(&doc, &9, 5, sticky), None);
    let tree = layout(&component, 8, 5);
    assert_eq!(paint(&component, &tree).rows, before);
    assert_eq!(state.borrow().pinned_header_keys(), [0, 7]);
}

#[test]
fn component_jumps_keep_items_clear_of_pinned_headers() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&doc, &state);
    let ensure_top = AnchorPolicy::EnsureVisibleFrom(Edge::Top);
    let painted_rows = || paint(&component, &layout(&component, 8, 5)).rows;

    // At row 8 the headers of 0 and 7 cover items 8 and 9.
    anchor(&state, 8);
    assert_eq!(painted_rows(), ["0:0", "7:0", "10:0", "11:0", "12:0"]);
    // The unsticky placement considers item 9 visible and keeps it covered.
    let unsticky = state
        .borrow()
        .anchor_for(&9, ensure_top, 5, &NoSourceMap)
        .expect("indexed");
    assert_eq!(unsticky, ListPosition::Anchored(ListAnchor::top(8)));
    // The component reveals it on the first row below the pinned headers.
    let position = component.anchor_for(&9, ensure_top, 5).expect("indexed");
    state.borrow_mut().set_position(position);
    assert_eq!(painted_rows()[2], "9:0");
    assert_eq!(state.borrow().pinned_header_keys(), [0, 7]);

    // Items already clear of the headers do not move the list.
    let before = state.borrow().position();
    assert_eq!(component.anchor_for(&11, ensure_top, 5), Some(before));

    // Other policies place the item in the uncovered rows.
    for (key, policy, row) in [
        (9, AnchorPolicy::ItemTopAtTop, 2),
        (9, AnchorPolicy::Center, 3),
        (20, AnchorPolicy::ItemBottomAtBottom, 4),
        (20, AnchorPolicy::ItemTopAtTop, 2),
    ] {
        let position = component.anchor_for(&key, policy, 5).expect("indexed");
        state.borrow_mut().set_position(position);
        let rows = painted_rows();
        assert_eq!(rows[row], format!("{key}:0"), "{key} {policy:?}: {rows:?}");
        assert!(
            u64::try_from(row).unwrap() >= state.borrow().sticky_header_rows(),
            "{key} {policy:?}"
        );
    }
    assert_eq!(component.anchor_for(&99, ensure_top, 5), None);
}

#[test]
fn component_focus_reveal_keeps_targets_clear_of_pinned_headers() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&doc, &state);
    let painted_rows = || paint(&component, &layout(&component, 8, 5)).rows;
    let target = |key| ItemFocusTarget::new(key, key, 0..1);

    anchor(&state, 8);
    let _ = painted_rows();
    // Focus moving up onto the covered item 9: the unsticky reveal stays.
    let unsticky = state
        .borrow()
        .reveal_for_focus(&target(9), Edge::Top, 5, &NoSourceMap)
        .expect("indexed");
    assert_eq!(unsticky, state.borrow().position());
    let position = component
        .reveal_for_focus(&target(9), Edge::Top, 5)
        .expect("indexed");
    state.borrow_mut().set_position(position);
    assert_eq!(painted_rows()[2], "9:0");

    // A visible target returns the current position unchanged.
    let current = state.borrow().position();
    assert_eq!(
        component.reveal_for_focus(&target(10), Edge::Bottom, 5),
        Some(current)
    );
    // Focus moving down lands on the bottom row.
    let position = component
        .reveal_for_focus(&target(20), Edge::Bottom, 5)
        .expect("indexed");
    state.borrow_mut().set_position(position);
    assert_eq!(painted_rows()[4], "20:0");
    assert_eq!(component.reveal_for_focus(&target(99), Edge::Top, 5), None);
}

#[test]
fn jumps_without_pinning_match_the_unsticky_placement() {
    let doc = single_row_doc();
    let state = RefCell::new(VirtualListState::new(0));
    anchor(&state, 8);
    let disabled = list(&doc, &state).sticky_headers(StickyHeaders::disabled());
    let _ = layout(&disabled, 8, 5);
    let ensure_top = AnchorPolicy::EnsureVisibleFrom(Edge::Top);
    for key in [0, 9, 13, 20] {
        let expected = state.borrow().anchor_for(&key, ensure_top, 5, &NoSourceMap);
        assert_eq!(disabled.anchor_for(&key, ensure_top, 5), expected, "{key}");
        let target = ItemFocusTarget::new(key, (), 0..1);
        let expected = state
            .borrow()
            .reveal_for_focus(&target, Edge::Top, 5, &NoSourceMap);
        assert_eq!(
            disabled.reveal_for_focus(&target, Edge::Top, 5),
            expected,
            "{key}"
        );
    }
    // A one-row viewport pins nothing, so nothing is inset.
    let sticky = list(&doc, &state);
    let _ = layout(&sticky, 8, 1);
    let expected = state.borrow().anchor_for(&9, ensure_top, 1, &NoSourceMap);
    assert_eq!(sticky.anchor_for(&9, ensure_top, 1), expected);
}

#[test]
fn malformed_parent_chains_are_cut_instead_of_followed() {
    let mut spec = vec![header(0, 0, 1)];
    spec.extend((1..=6).map(line));
    let doc = Doc::new(spec);
    // Item 2 names a parent that is not a header; item 3 names a later one.
    doc.update(2, |entry| entry.parent = Some(1));
    doc.update(3, |entry| entry.parent = Some(4));
    let state = RefCell::new(VirtualListState::new(0));
    assert_eq!(rows(&state, &doc, 1, 6, 3), ["0:0", "2:0", "3:0"]);
    assert_eq!(rows(&state, &doc, 2, 6, 3), ["2:0", "3:0", "4:0"]);
    assert_eq!(rows(&state, &doc, 3, 6, 3), ["3:0", "4:0", "5:0"]);
}
