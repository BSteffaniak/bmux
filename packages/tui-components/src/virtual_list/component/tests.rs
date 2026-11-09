use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::buffer::Buffer;
use bmux_tui::component::{
    Component, Constraints, Element, EventCx, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::hit::{HitId, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::selection::{
    SelectionAutoScrollRequest, SelectionScopeId, SelectionScrollAxis, SelectionScrollDirection,
};
use bmux_tui::text::Line;

use super::{VirtualListChrome, VirtualListComponent};
use crate::scrollbar_layout::ScrollbarAxisLayoutMode;
use crate::virtual_list::{
    Edge, ListAnchor, ListPosition, Overscroll, PageStep, PositionEstimate, ScrollInputSource,
    VirtualItemSource, VirtualListAction, VirtualListState,
};

/// A card painting `"{key}:{row}"` on each of its rows.
struct Card {
    key: u64,
    height: u64,
    handles: bool,
}

impl Component for Card {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("card"),
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

/// A lazy source counting constructions.
struct Cards {
    items: Vec<(u64, u64)>,
    handling: BTreeSet<u64>,
    builds: Cell<usize>,
}

impl Cards {
    fn uniform(count: u64, height: u64) -> Self {
        Self::new((0..count).map(|key| (key, height)).collect())
    }

    fn new(items: Vec<(u64, u64)>) -> Self {
        Self {
            items,
            handling: BTreeSet::new(),
            builds: Cell::new(0),
        }
    }
}

impl VirtualItemSource<u64> for Cards {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn key(&self, index: usize) -> u64 {
        self.items[index].0
    }

    fn layout_revision(&self, _index: usize) -> u64 {
        0
    }

    fn build(&self, index: usize) -> Element<'_> {
        self.builds.set(self.builds.get() + 1);
        let (key, height) = self.items[index];
        Element::new(Card {
            key,
            height,
            handles: self.handling.contains(&key),
        })
    }
}

type State = RefCell<VirtualListState<u64>>;

fn list<'a>(cards: &'a Cards, state: &'a State) -> VirtualListComponent<'a, u64, Cards> {
    VirtualListComponent::new("list", cards, state)
}

fn layout(component: &impl Component, width: u16, height: u16) -> LayoutNode {
    component.layout(
        Constraints::tight(Size::new(width, height)),
        &mut LayoutCx::new(),
    )
}

struct Painted {
    rows: Vec<String>,
    hits: Vec<(String, Rect, HitRole)>,
    semantics: Vec<(String, Rect)>,
}

fn paint(component: &impl Component, layout: &LayoutNode) -> Painted {
    let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
    let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    component.paint(layout, &mut PaintCx::new(&mut frame));
    let hits = frame
        .hits()
        .regions()
        .iter()
        .map(|region| (region.id.as_str().to_owned(), region.area, region.role))
        .collect();
    let semantics = frame
        .semantics()
        .regions()
        .iter()
        .map(|region| (region.id.clone(), region.area))
        .collect();
    let rows = (0..height)
        .map(|y| frame.buffer().row_symbols(y).unwrap_or_default())
        .collect();
    Painted {
        rows,
        hits,
        semantics,
    }
}

fn send(component: &impl Component, layout: &LayoutNode, event: &Event) -> EventOutcome {
    component.event(event, layout, &mut EventCx::new(layout))
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
    Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyStroke::simple(code))
}

fn top(state: &State) -> u64 {
    state.borrow().resolved().expect("laid out").top_offset
}

fn content_children(layout: &LayoutNode) -> Vec<(String, u64, u64)> {
    layout.children[0]
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
}

#[test]
fn layout_places_only_visible_items_at_exact_rows_with_a_reserved_gutter() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 5);
    assert_eq!(tree.id.as_str(), "list");
    assert_eq!(tree.size, LogicalSize::new(12, 5));
    assert_eq!(tree.metadata.semantics, ["list"]);
    let content = &tree.children[0];
    assert_eq!(content.node.id.as_str(), "list.content");
    assert_eq!(content.node.size, LogicalSize::new(11, 30));
    assert_eq!(
        content_children(&tree),
        [
            ("list.item.0".to_owned(), 0, 3),
            ("list.item.1".to_owned(), 3, 3),
            ("list.window".to_owned(), 0, 5),
        ]
    );
    let scrollbar = &tree.children[1];
    assert_eq!(scrollbar.node.id.as_str(), "list.scrollbar");
    assert_eq!((scrollbar.x, scrollbar.y), (11, 0));
    assert_eq!(scrollbar.node.size, LogicalSize::new(1, 5));
    // Wrappers are leaves: retained item layouts are borrowed at paint, not
    // copied into the tree on every layout.
    assert_eq!(content.node.children[0].node.children.len(), 0);

    state
        .borrow_mut()
        .set_position(ListPosition::Anchored(ListAnchor::row(3, 1)));
    let tree = layout(&component, 12, 5);
    assert_eq!(
        content_children(&tree),
        [
            ("list.item.3".to_owned(), 9, 3),
            ("list.item.4".to_owned(), 12, 3),
            ("list.window".to_owned(), 10, 5),
        ]
    );
    assert_eq!(
        tree.find_logical_rect(&component.window_id())
            .map(|rect| rect.y),
        Some(10)
    );
}

#[test]
fn unbounded_height_takes_the_content_height_and_hidden_chrome_uses_full_width() {
    let cards = Cards::uniform(4, 2);
    let state = RefCell::new(VirtualListState::new(1));
    let component = list(&cards, &state).chrome(VirtualListChrome::bare());
    let tree = component.layout(Constraints::for_width(9), &mut LayoutCx::new());
    assert_eq!(tree.size, LogicalSize::new(9, 11));
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].node.size, LogicalSize::new(9, 11));
    assert_eq!(content_children(&tree).len(), 5);
}

#[test]
fn paint_draws_visible_rows_registers_visible_regions_and_the_scrollbar() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    state
        .borrow_mut()
        .set_position(ListPosition::Anchored(ListAnchor::row(3, 1)));
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 5);
    let painted = paint(&component, &tree);
    let content: Vec<_> = painted
        .rows
        .iter()
        .map(|row| row.chars().take(3).collect::<String>())
        .collect();
    assert_eq!(content, ["3:1", "3:2", "4:0", "4:1", "4:2"]);
    let gutter: Vec<_> = painted
        .rows
        .iter()
        .map(|row| row.chars().nth(11).expect("gutter column"))
        .collect();
    // top 10 of 25: the one-cell thumb sits on row 1 of 5.
    assert_eq!(gutter, ['│', '█', '│', '│', '│']);
    assert_eq!(
        painted.hits,
        [
            ("list".to_owned(), Rect::new(0, 0, 11, 5), HitRole::Scroll),
            (
                "list.item.3".to_owned(),
                Rect::new(0, 0, 11, 2),
                HitRole::ListItem
            ),
            // Focus geometry registers as an action region.
            (
                "list.item.3".to_owned(),
                Rect::new(0, 0, 11, 2),
                HitRole::Action
            ),
            (
                "list.item.4".to_owned(),
                Rect::new(0, 2, 11, 3),
                HitRole::ListItem
            ),
            (
                "list.item.4".to_owned(),
                Rect::new(0, 2, 11, 3),
                HitRole::Action
            ),
        ]
    );
    assert_eq!(
        painted.semantics,
        [
            ("list".to_owned(), Rect::new(0, 0, 11, 5)),
            ("list.item.3".to_owned(), Rect::new(0, 0, 11, 2)),
            ("list.item.4".to_owned(), Rect::new(0, 2, 11, 3)),
        ]
    );
    let report = state.borrow().render_stats();
    assert_eq!((report.painted_items, report.registered_items), (2, 2));
}

#[test]
fn overlay_scrollbars_share_content_width_and_hide_without_overflow() {
    let cards = Cards::uniform(2, 1);
    let state = RefCell::new(VirtualListState::new(0));
    let overlay = list(&cards, &state)
        .chrome(VirtualListChrome::interactive().scrollbar(ScrollbarAxisLayoutMode::Overlay));
    let tree = layout(&overlay, 6, 4);
    assert_eq!(tree.children[0].node.size.width, 6);
    let painted = paint(&overlay, &tree);
    // Overlay mode draws even without overflow, over the last content column.
    assert!(painted.rows[0].ends_with('█'));

    let on_overflow = list(&cards, &state).chrome(
        VirtualListChrome::interactive().scrollbar(ScrollbarAxisLayoutMode::OverlayOnOverflow),
    );
    let tree = layout(&on_overflow, 6, 4);
    let painted = paint(&on_overflow, &tree);
    assert_eq!(painted.rows[0], "0:0   ");
    assert_eq!(painted.rows[3], "      ");
    // Without overflow the hidden scrollbar ignores presses.
    assert_eq!(
        send(
            &on_overflow,
            &tree,
            &mouse(MouseEventKind::Down(MouseButton::Left), 5, 3)
        ),
        EventOutcome::Ignored
    );
    assert!(!state.borrow().scroll.dragging_scrollbar());
}

#[test]
fn stale_layout_nodes_paint_nothing_and_ignore_events() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state);
    let old = layout(&component, 12, 5);
    let current = layout(&component, 12, 6);
    let painted = paint(&component, &old);
    assert_eq!(painted.hits.len(), 0);
    assert!(painted.rows.iter().all(|row| row.trim().is_empty()));
    assert_eq!(
        send(&component, &old, &mouse(MouseEventKind::ScrollDown, 1, 1)),
        EventOutcome::Ignored
    );
    assert_eq!(top(&state), 0);
    assert_eq!(paint(&component, &current).rows[0], "0:0        █");
    // An index change after layout also invalidates the node.
    state
        .borrow_mut()
        .update_measured(crate::virtual_list::MeasuredListItem::new(0, 0, 11, 0, 4));
    assert_eq!(paint(&component, &current).hits.len(), 0);
}

#[test]
fn uncontrolled_wheel_scrolls_until_the_edge_then_hands_off() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 5);
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollUp, 2, 2)),
        EventOutcome::Ignored
    );
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollDown, 2, 2)),
        EventOutcome::Redraw
    );
    let tree = layout(&component, 12, 5);
    assert_eq!(top(&state), 3);
    // The gutter receives wheel input too.
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollDown, 11, 0)),
        EventOutcome::Redraw
    );
    state.borrow_mut().set_position(ListPosition::FollowBottom);
    let tree = layout(&component, 12, 5);
    assert_eq!(top(&state), 25);
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollDown, 2, 2)),
        EventOutcome::Ignored
    );
    // Upward wheel releases following.
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollUp, 2, 2)),
        EventOutcome::Redraw
    );
    assert!(!state.borrow().position().is_follow_bottom());
    // Outside the viewport nothing happens.
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollUp, 2, 9)),
        EventOutcome::Ignored
    );
}

#[test]
fn items_receive_pointer_and_key_input_before_list_navigation() {
    let mut cards = Cards::uniform(10, 3);
    cards.handling.insert(1);
    let state = RefCell::new(VirtualListState::new(0));
    state.borrow_mut().scroll.interaction.focused = true;
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 5);
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollDown, 1, 4)),
        EventOutcome::Handled
    );
    assert_eq!(
        send(&component, &tree, &key(KeyCode::Down)),
        EventOutcome::Handled
    );
    assert_eq!(top(&state), 0);
    let built = state.borrow().render_stats().built_items;
    assert!(built >= 2, "routing constructs visible items: {built}");
}

#[test]
fn focused_keyboard_navigation_maps_to_actions() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state)
        .chrome(VirtualListChrome::interactive().page(PageStep::ViewportMinus(1)));
    let tree = layout(&component, 12, 5);
    assert_eq!(
        send(&component, &tree, &key(KeyCode::Down)),
        EventOutcome::Ignored,
        "unfocused lists ignore navigation keys"
    );
    state.borrow_mut().scroll.interaction.focused = true;
    for (code, expected) in [
        (KeyCode::Down, 1),
        (KeyCode::PageDown, 5),
        (KeyCode::Up, 4),
        (KeyCode::End, 25),
        (KeyCode::PageUp, 21),
        (KeyCode::Home, 0),
    ] {
        let tree = layout(&component, 12, 5);
        assert_eq!(
            send(&component, &tree, &key(code)),
            EventOutcome::Redraw,
            "{code:?}"
        );
        layout(&component, 12, 5);
        assert_eq!(top(&state), expected, "{code:?}");
    }
    assert_eq!(state.borrow().position(), ListPosition::Top);
    let tree = layout(&component, 12, 5);
    assert_eq!(
        send(&component, &tree, &key(KeyCode::Char('x'))),
        EventOutcome::Ignored
    );
    let bare = list(&cards, &state).chrome(VirtualListChrome::bare());
    let tree = layout(&bare, 12, 5);
    assert_eq!(
        send(&bare, &tree, &key(KeyCode::Down)),
        EventOutcome::Ignored
    );
}

#[test]
fn controlled_lists_queue_every_input_without_moving() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::controlled(0));
    state.borrow_mut().scroll.interaction.focused = true;
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 5);
    for event in [
        mouse(MouseEventKind::ScrollDown, 1, 1),
        key(KeyCode::PageDown),
        key(KeyCode::End),
        mouse(MouseEventKind::Down(MouseButton::Left), 11, 4),
        mouse(MouseEventKind::Up(MouseButton::Left), 11, 4),
    ] {
        assert!(send(&component, &tree, &event).is_handled(), "{event:?}");
    }
    let actions: Vec<_> = state.borrow_mut().drain_actions().collect();
    assert_eq!(
        actions,
        [
            VirtualListAction::ScrollRequested {
                rows: 3,
                source: ScrollInputSource::Wheel
            },
            VirtualListAction::PageRequested(1),
            VirtualListAction::EdgeRequested(Edge::Bottom),
            VirtualListAction::ScrollbarDrag(1.0),
        ]
    );
    layout(&component, 12, 5);
    assert_eq!(top(&state), 0);
    assert_eq!(state.borrow().position(), ListPosition::Top);
}

#[test]
fn scrollbar_drag_inverts_the_drawn_model_and_keeps_capture() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 5);
    assert_eq!(
        send(
            &component,
            &tree,
            &mouse(MouseEventKind::Down(MouseButton::Left), 11, 4)
        ),
        EventOutcome::Redraw
    );
    assert!(state.borrow().scroll.dragging_scrollbar());
    assert_eq!(state.borrow().position(), ListPosition::FollowBottom);
    let tree = layout(&component, 12, 5);
    assert_eq!(top(&state), 25);
    // Captured drags continue outside the track.
    assert_eq!(
        send(
            &component,
            &tree,
            &mouse(MouseEventKind::Drag(MouseButton::Left), 30, 2)
        ),
        EventOutcome::Redraw
    );
    let tree = layout(&component, 12, 5);
    // Track row 2 of 4 travel rows: 2 * 25 / 4.
    assert_eq!(top(&state), 12);
    assert_eq!(
        send(
            &component,
            &tree,
            &mouse(MouseEventKind::Up(MouseButton::Left), 30, 2)
        ),
        EventOutcome::Redraw
    );
    assert!(!state.borrow().scroll.dragging_scrollbar());
    // Releasing capture never activates an item.
    assert!(state.borrow_mut().drain_actions().next().is_none());
}

#[test]
fn press_and_release_on_one_unhandled_item_reports_activation() {
    let mut cards = Cards::uniform(10, 3);
    cards.handling.insert(2);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 8);
    let press = |x, y| mouse(MouseEventKind::Down(MouseButton::Left), x, y);
    let release = |x, y| mouse(MouseEventKind::Up(MouseButton::Left), x, y);
    assert_eq!(send(&component, &tree, &press(1, 1)), EventOutcome::Ignored);
    assert_eq!(
        send(&component, &tree, &release(3, 2)),
        EventOutcome::Handled
    );
    // Different items, an intervening drag, or a handling item do not activate.
    send(&component, &tree, &press(1, 1));
    send(&component, &tree, &release(1, 4));
    send(&component, &tree, &press(1, 4));
    send(
        &component,
        &tree,
        &mouse(MouseEventKind::Drag(MouseButton::Left), 1, 4),
    );
    send(&component, &tree, &release(1, 4));
    assert_eq!(send(&component, &tree, &press(1, 7)), EventOutcome::Handled);
    assert_eq!(
        send(&component, &tree, &release(1, 7)),
        EventOutcome::Handled
    );
    let actions: Vec<_> = state.borrow_mut().drain_actions().collect();
    assert_eq!(actions, [VirtualListAction::Activated(0)]);
}

#[test]
fn overscroll_reveals_blank_rows_that_register_nothing() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    state
        .borrow_mut()
        .set_overscroll(Overscroll::ViewportMinus(1));
    state.borrow_mut().set_position(ListPosition::FollowBottom);
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 5);
    assert_eq!(top(&state), 25);
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollDown, 1, 1)),
        EventOutcome::Redraw
    );
    assert!(matches!(
        state.borrow().position(),
        ListPosition::Overscrolled(_)
    ));
    let tree = layout(&component, 12, 5);
    assert_eq!(top(&state), 28);
    assert_eq!(
        content_children(&tree),
        [
            ("list.item.9".to_owned(), 27, 3),
            ("list.window".to_owned(), 28, 5),
        ]
    );
    let painted = paint(&component, &tree);
    assert_eq!(&painted.rows[0][..3], "9:1");
    assert!(
        painted.rows[2..]
            .iter()
            .all(|row| row.starts_with("           "))
    );
    let items: Vec<_> = painted
        .hits
        .iter()
        .filter(|(_, _, role)| *role == HitRole::ListItem)
        .collect();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].1, Rect::new(0, 0, 11, 2));
    // The thumb stays pinned at the content bottom.
    assert!(painted.rows[4].ends_with('█'));
    // Further overscroll stops one content row short of empty.
    send(&component, &tree, &mouse(MouseEventKind::ScrollDown, 1, 1));
    layout(&component, 12, 5);
    assert_eq!(top(&state), 29);
}

#[test]
fn selection_autoscroll_routes_vertical_requests_through_actions() {
    let request = |axis, direction| SelectionAutoScrollRequest {
        scope_id: SelectionScopeId::new("list"),
        axis,
        direction,
        intensity: 2,
    };
    let down = request(
        SelectionScrollAxis::Vertical,
        SelectionScrollDirection::Forward,
    );
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state);
    layout(&component, 12, 5);
    assert!(state.borrow_mut().handle_selection_auto_scroll(&down));
    layout(&component, 12, 5);
    assert_eq!(top(&state), 2);
    assert!(!state.borrow_mut().handle_selection_auto_scroll(&request(
        SelectionScrollAxis::Horizontal,
        SelectionScrollDirection::Forward
    )));
    assert!(state.borrow_mut().handle_selection_auto_scroll(&request(
        SelectionScrollAxis::Vertical,
        SelectionScrollDirection::Backward
    )));
    assert!(!state.borrow_mut().handle_selection_auto_scroll(&request(
        SelectionScrollAxis::Vertical,
        SelectionScrollDirection::Backward
    )));
    state.borrow_mut().scroll.interaction.disabled = true;
    assert!(!state.borrow_mut().handle_selection_auto_scroll(&down));

    let controlled = RefCell::new(VirtualListState::<u64>::controlled(0));
    assert!(controlled.borrow_mut().handle_selection_auto_scroll(&down));
    let actions: Vec<_> = controlled.borrow_mut().drain_actions().collect();
    assert_eq!(actions, [VirtualListAction::SelectionAutoScroll(2)]);
}

#[test]
fn lazy_sources_construct_only_inserted_and_visible_items() {
    let mut cards = Cards::uniform(2_000, 2);
    let state = RefCell::new(VirtualListState::new(0));
    state
        .borrow_mut()
        .set_position(ListPosition::Anchored(ListAnchor::top(500)));
    {
        let component = list(&cards, &state);
        let tree = layout(&component, 12, 6);
        assert_eq!(cards.builds.get(), 2_000);
        paint(&component, &tree);
        assert_eq!(cards.builds.get(), 2_003);
    }
    cards
        .items
        .splice(0..0, (10_000..10_005).map(|key| (key, 4)));
    state.borrow_mut().note_splice(0..0, 5);
    cards.builds.set(0);
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 6);
    assert_eq!(cards.builds.get(), 5, "only inserted items are measured");
    assert_eq!(top(&state), 1_020);
    let painted = paint(&component, &tree);
    assert_eq!(
        cards.builds.get(),
        8,
        "paint builds the three visible items"
    );
    assert_eq!(&painted.rows[0][..5], "500:0");
    let report = state.borrow().render_stats();
    assert_eq!((report.measured_items, report.built_items), (5, 8));
    // Steady frames construct visible items only.
    cards.builds.set(0);
    let tree = layout(&component, 12, 6);
    paint(&component, &tree);
    assert_eq!(cards.builds.get(), 3);
}

#[test]
fn position_estimates_drive_the_thumb_and_drag_mapping() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    let estimate = PositionEstimate {
        before: 70,
        resident: 30,
        after: 0,
    };
    let component = list(&cards, &state).position_estimate(estimate);
    let tree = layout(&component, 12, 5);
    let painted = paint(&component, &tree);
    let thumb = painted
        .rows
        .iter()
        .position(|row| row.ends_with('█'))
        .expect("thumb drawn");
    assert_eq!(thumb, 2, "70 estimated rows above the window");
    // The top of the track maps before the window, clamped to its first row.
    send(
        &component,
        &tree,
        &mouse(MouseEventKind::Down(MouseButton::Left), 11, 0),
    );
    send(
        &component,
        &tree,
        &mouse(MouseEventKind::Up(MouseButton::Left), 11, 0),
    );
    let tree = layout(&component, 12, 5);
    assert_eq!(top(&state), 0);
    send(
        &component,
        &tree,
        &mouse(MouseEventKind::Down(MouseButton::Left), 11, 4),
    );
    layout(&component, 12, 5);
    assert_eq!(state.borrow().position(), ListPosition::FollowBottom);

    // An estimate that does not match the exact total is ignored.
    let mismatched = list(&cards, &state).position_estimate(PositionEstimate {
        before: 70,
        resident: 31,
        after: 0,
    });
    state.borrow_mut().set_position(ListPosition::Top);
    let tree = layout(&mismatched, 12, 5);
    assert!(paint(&mismatched, &tree).rows[0].ends_with('█'));
}

#[test]
fn revision_tracks_position_content_and_chrome() {
    let mut cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    let first = {
        let component = list(&cards, &state);
        layout(&component, 12, 5);
        let revision = component.revision();
        assert_eq!(revision, component.revision());
        let tree = layout(&component, 12, 5);
        send(&component, &tree, &mouse(MouseEventKind::ScrollDown, 1, 1));
        assert_ne!(component.revision().layout, revision.layout);
        let bare = list(&cards, &state).chrome(VirtualListChrome::bare());
        assert_ne!(bare.revision().layout, component.revision().layout);
        component.revision()
    };
    cards.items.push((10, 3));
    state.borrow_mut().note_splice(10..10, 1);
    let component = list(&cards, &state);
    assert_ne!(component.revision().layout, first.layout);
}

#[test]
fn tiny_and_empty_viewports_degrade_without_panicking() {
    let cards = Cards::uniform(5, 2);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state);
    for (width, height) in [(0, 0), (1, 3), (5, 0), (0, 4)] {
        let tree = layout(&component, width, height);
        assert_eq!(tree.size, LogicalSize::new(width.into(), height.into()));
        paint(&component, &tree);
        for event in [
            mouse(MouseEventKind::ScrollDown, 0, 0),
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            key(KeyCode::Down),
        ] {
            let _ = send(&component, &tree, &event);
        }
    }
    // A one-column gutter list has no content column.
    let tree = layout(&component, 1, 3);
    assert_eq!(tree.children[0].node.size.width, 0);

    let empty = Cards::uniform(0, 1);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&empty, &state);
    let tree = layout(&component, 8, 4);
    assert_eq!(content_children(&tree), [("list.window".to_owned(), 0, 4)]);
    let painted = paint(&component, &tree);
    assert_eq!(painted.hits.len(), 1);
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollDown, 1, 1)),
        EventOutcome::Ignored
    );
}

#[test]
fn items_taller_than_the_viewport_clip_to_it() {
    let cards = Cards::new(vec![(0, 2), (1, 20), (2, 2)]);
    let state = RefCell::new(VirtualListState::new(0));
    state
        .borrow_mut()
        .set_position(ListPosition::Anchored(ListAnchor::row(1, 7)));
    let component = list(&cards, &state);
    let tree = layout(&component, 8, 4);
    assert_eq!(
        content_children(&tree),
        [
            ("list.item.1".to_owned(), 2, 20),
            ("list.window".to_owned(), 9, 4),
        ]
    );
    let painted = paint(&component, &tree);
    let content: Vec<_> = painted.rows.iter().map(|row| row[..4].to_owned()).collect();
    assert_eq!(content, ["1:7 ", "1:8 ", "1:9 ", "1:10"]);
    assert!(painted.hits.contains(&(
        "list.item.1".to_owned(),
        Rect::new(0, 0, 7, 4),
        HitRole::ListItem
    )));
}

#[test]
fn disabled_lists_ignore_input_and_release_capture() {
    let cards = Cards::uniform(10, 3);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 5);
    send(
        &component,
        &tree,
        &mouse(MouseEventKind::Down(MouseButton::Left), 11, 2),
    );
    assert!(state.borrow().scroll.dragging_scrollbar());
    state.borrow_mut().scroll.interaction.disabled = true;
    let tree = layout(&component, 12, 5);
    assert_eq!(
        send(&component, &tree, &mouse(MouseEventKind::ScrollDown, 1, 1)),
        EventOutcome::Ignored
    );
    assert!(!state.borrow().scroll.dragging_scrollbar());
    let painted = paint(&component, &tree);
    assert_eq!(painted.hits[0].0, "list");
}

#[test]
fn hit_ids_match_item_identities() {
    let cards = Cards::uniform(3, 1);
    let state = RefCell::new(VirtualListState::new(0));
    let component = list(&cards, &state);
    let tree = layout(&component, 6, 3);
    let painted = paint(&component, &tree);
    let item = component.item_id(&2);
    assert!(
        painted
            .hits
            .iter()
            .any(|(id, _, _)| HitId::new(id.clone()) == HitId::new(item.as_str().to_owned()))
    );
    assert_eq!(component.content_id().as_str(), "list.content");
    assert_eq!(component.scrollbar_id().as_str(), "list.scrollbar");
}

#[test]
fn events_between_a_noted_splice_and_layout_reach_the_laid_out_item() {
    let mut cards = Cards::uniform(10, 3);
    cards.handling.insert(4);
    let state = RefCell::new(VirtualListState::new(0));
    state
        .borrow_mut()
        .set_position(ListPosition::Anchored(ListAnchor::top(4)));
    let component = list(&cards, &state);
    let tree = layout(&component, 12, 6);
    // Row 1 shows key 4, which handles pointer input.
    assert_eq!(
        send(
            &component,
            &tree,
            &mouse(MouseEventKind::Down(MouseButton::Left), 1, 1)
        ),
        EventOutcome::Handled
    );
    drop(component);

    // Prepend two items and note it; the old layout node is still current
    // for events until the next layout.
    cards.items.splice(0..0, [(100, 3), (101, 3)]);
    state.borrow_mut().note_splice(0..0, 2);
    let component = list(&cards, &state);
    assert_eq!(
        send(
            &component,
            &tree,
            &mouse(MouseEventKind::Down(MouseButton::Left), 1, 1)
        ),
        EventOutcome::Handled,
        "key 4, now at source row 6, still receives the press"
    );
    // Key 5 (row 4) does not handle input: press and release activate it,
    // reporting the key that received the events.
    send(
        &component,
        &tree,
        &mouse(MouseEventKind::Down(MouseButton::Left), 1, 4),
    );
    assert_eq!(
        send(
            &component,
            &tree,
            &mouse(MouseEventKind::Up(MouseButton::Left), 1, 4)
        ),
        EventOutcome::Handled
    );
    assert_eq!(
        state.borrow_mut().drain_actions().collect::<Vec<_>>(),
        [VirtualListAction::Activated(5)]
    );
    // Painting the old node draws the laid-out keys too.
    let painted = paint(&component, &tree);
    assert!(painted.rows[0].starts_with("4:0"));
    assert!(painted.rows[3].starts_with("5:0"));

    // An item replaced in place without a note is skipped, not routed with
    // another item's layout.
    cards.items[7].0 = 77;
    cards.handling.insert(77);
    let component = list(&cards, &state);
    assert_eq!(
        send(
            &component,
            &tree,
            &mouse(MouseEventKind::Down(MouseButton::Left), 1, 4)
        ),
        EventOutcome::Ignored
    );
    state.borrow_mut().invalidate_all();
    let tree = layout(&component, 12, 6);
    assert_eq!(content_children(&tree)[1].0, "list.item.77");
}

#[test]
fn drags_invert_cells_like_scroll_views_on_huge_lists() {
    use crate::scroll_view::{logical_offset_from_scrollbar, scrollbar_state};

    let cards = Cards::uniform(200_000, 1);
    let state = RefCell::new(VirtualListState::<u64>::new(0));
    let component = list(&cards, &state);
    layout(&component, 12, 10);
    let resolved = state.borrow().resolved().unwrap();
    let model = state.borrow().scrollbar_model(resolved);
    assert_eq!(
        model,
        scrollbar_state(resolved.total_height, 10, resolved.top_offset)
    );
    let maximum = u64::from(model.max_offset());
    for cell in [0, 1, 2, 777, 32_767, 65_000, model.max_offset() - 1] {
        let fraction = state.borrow().drag_fraction(resolved, model, cell);
        state
            .borrow_mut()
            .apply_action(&VirtualListAction::ScrollbarDrag(fraction));
        let expected =
            logical_offset_from_scrollbar(u64::from(cell), maximum, resolved.max_content_top);
        assert_eq!(
            state.borrow().scroll.vertical_offset(),
            expected,
            "cell {cell}"
        );
        state.borrow_mut().set_position(ListPosition::Top);
    }

    // Estimated geometry uses the same mapping over the estimated total.
    let estimate = PositionEstimate {
        before: 2_000_000,
        resident: 200_000,
        after: 0,
    };
    state.borrow_mut().set_position_estimate(Some(estimate));
    let model = state.borrow().scrollbar_model(resolved);
    let cell = model.max_offset() - 1;
    let fraction = state.borrow().drag_fraction(resolved, model, cell);
    state
        .borrow_mut()
        .apply_action(&VirtualListAction::ScrollbarDrag(fraction));
    let position = logical_offset_from_scrollbar(
        u64::from(cell),
        u64::from(model.max_offset()),
        estimate.total() - 10,
    );
    assert_eq!(
        state.borrow().scroll.vertical_offset(),
        position - 2_000_000
    );
}
