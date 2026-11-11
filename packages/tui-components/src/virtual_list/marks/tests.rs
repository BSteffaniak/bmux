use std::cell::RefCell;

use bmux_tui::buffer::Buffer;
use bmux_tui::component::{
    Component, Constraints, Element, EventCx, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Color, Style};

use super::{ScrollbarMark, ScrollbarMarkCells, ScrollbarMarkPosition};
use crate::scrollbar::ScrollbarStyles;
use crate::scrollbar_layout::ScrollbarAxisLayoutMode;
use crate::virtual_list::{
    PositionEstimate, VirtualItemSource, VirtualListChrome, VirtualListComponent, VirtualListState,
};

/// A blank item of a fixed height.
struct Blank(u64);

impl Component for Blank {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("blank"),
            LogicalSize::new(constraints.max_width(), self.0),
        )
    }

    fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        EventOutcome::Ignored
    }
}

/// Items `(key, height)`.
struct Items(RefCell<Vec<(u64, u64)>>);

impl Items {
    fn uniform(count: u64) -> Self {
        Self(RefCell::new((0..count).map(|key| (key, 1)).collect()))
    }
}

impl VirtualItemSource<u64> for Items {
    fn len(&self) -> usize {
        self.0.borrow().len()
    }

    fn key(&self, index: usize) -> u64 {
        self.0.borrow()[index].0
    }

    fn layout_revision(&self, _index: usize) -> u64 {
        0
    }

    fn build(&self, index: usize) -> Element<'_> {
        Element::new(Blank(self.0.borrow()[index].1))
    }
}

const RED: Style = Style::new().fg(Color::Red);
const GREEN: Style = Style::new().fg(Color::Green);
const BLUE: Style = Style::new().fg(Color::Blue);

fn layout(component: &impl Component, width: u16, height: u16) -> LayoutNode {
    component.layout(
        Constraints::tight(Size::new(width, height)),
        &mut LayoutCx::new(),
    )
}

/// Painted `(symbol, style)` of each cell in column `x`.
fn column(component: &impl Component, tree: &LayoutNode, x: u16) -> Vec<(String, Style)> {
    let width = u16::try_from(tree.size.width).unwrap_or(u16::MAX);
    let height = u16::try_from(tree.size.height).unwrap_or(u16::MAX);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    component.paint(tree, &mut PaintCx::new(&mut frame));
    frame.buffer().debug_assert_valid_wide_spans();
    (0..height)
        .map(|y| {
            let cell = frame.buffer().get(Point::new(x, y)).expect("cell");
            (cell.symbol.clone(), cell.style)
        })
        .collect()
}

const fn cells(mark: usize, y: u16, height: u16) -> ScrollbarMarkCells {
    ScrollbarMarkCells {
        mark,
        area: Rect::new(4, y, 1, height),
    }
}

#[test]
fn marks_scale_rows_and_keyed_items_onto_the_whole_track() {
    let items = Items::uniform(100);
    let state = RefCell::new(VirtualListState::new(0));
    let marks = [
        ScrollbarMark::row(0, RED),
        ScrollbarMark::row(55, GREEN),
        ScrollbarMark::new(ScrollbarMarkPosition::Rows(90..100), BLUE),
        ScrollbarMark::item(30, GREEN).symbol("●"),
        ScrollbarMark::new(
            ScrollbarMarkPosition::Items {
                first: 79,
                last: 70,
            },
            RED,
        ),
        ScrollbarMark::item(500, RED),
        ScrollbarMark::row(1_000, GREEN),
    ];
    let component = VirtualListComponent::new("list", &items, &state).scrollbar_marks(&marks);
    let tree = layout(&component, 5, 10);
    assert_eq!(
        component.scrollbar_mark_cells(&tree),
        [
            cells(0, 0, 1),
            cells(1, 5, 1),
            cells(2, 9, 1),
            cells(3, 3, 1),
            cells(4, 7, 1),
            // Rows past the end clamp to the last row.
            cells(6, 9, 1),
        ]
    );
    let painted = column(&component, &tree, 4);
    let styles = ScrollbarStyles::new();
    // The thumb (cell 0) hides the mark under it.
    assert_eq!(painted[0], ("█".to_owned(), styles.thumb));
    assert_eq!(painted[1], ("│".to_owned(), styles.track));
    assert_eq!(painted[3], ("●".to_owned(), GREEN));
    assert_eq!(painted[5], ("│".to_owned(), GREEN));
    assert_eq!(painted[7], ("│".to_owned(), RED));
    // The later mark wins a shared cell.
    assert_eq!(painted[9], ("│".to_owned(), GREEN));
}

#[test]
fn painted_cells_agree_with_reported_mark_cells() {
    let items = Items(RefCell::new(
        (0..40).map(|key| (key, 1 + key % 4)).collect(),
    ));
    let state = RefCell::new(VirtualListState::new(1));
    let marks = (0..40)
        .step_by(7)
        .map(|key| ScrollbarMark::item(key, GREEN))
        .collect::<Vec<_>>();
    for (width, height) in [(5, 10), (5, 3), (2, 1), (1, 7), (5, 23)] {
        let component = VirtualListComponent::new("list", &items, &state).scrollbar_marks(&marks);
        let tree = layout(&component, width, height);
        let reported = component.scrollbar_mark_cells(&tree);
        let x = width - 1;
        let painted = column(&component, &tree, x);
        let styles = ScrollbarStyles::new();
        for (y, (symbol, style)) in painted.iter().enumerate() {
            let y = u16::try_from(y).unwrap();
            let marked = reported
                .iter()
                .any(|cells| cells.area.contains(Point::new(x, y)));
            if *style == GREEN {
                assert!(marked, "{width}x{height}: cell {y} painted without a mark");
                assert_eq!(symbol, "│");
            } else if marked {
                assert_eq!(*style, styles.thumb, "{width}x{height}: cell {y}");
            }
        }
        assert!(
            reported
                .iter()
                .all(|cells| cells.area.width == 1 && cells.area.bottom() <= height)
        );
    }
}

#[test]
fn symbols_must_be_one_cell_and_over_thumb_marks_cover_the_thumb() {
    let items = Items::uniform(100);
    let state = RefCell::new(VirtualListState::new(0));
    let marks = [
        ScrollbarMark::row(0, RED)
            .symbol("e\u{301}")
            .over_thumb(true),
        ScrollbarMark::row(30, GREEN).symbol("界"),
        ScrollbarMark::row(50, GREEN).symbol("🔥"),
        ScrollbarMark::row(70, BLUE).symbol(""),
    ];
    let component = VirtualListComponent::new("list", &items, &state).scrollbar_marks(&marks);
    let tree = layout(&component, 5, 10);
    let painted = column(&component, &tree, 4);
    assert_eq!(painted[0], ("e\u{301}".to_owned(), RED));
    // Wide and empty symbols fall back to the scrollbar's own symbol.
    assert_eq!(painted[3], ("│".to_owned(), GREEN));
    assert_eq!(painted[5], ("│".to_owned(), GREEN));
    assert_eq!(painted[7], ("│".to_owned(), BLUE));
}

#[test]
fn tall_items_span_cells_and_estimates_offset_resident_rows() {
    let items = Items(RefCell::new(vec![(0, 10), (1, 50), (2, 40)]));
    let state = RefCell::new(VirtualListState::new(0));
    let marks = [ScrollbarMark::item(1, GREEN), ScrollbarMark::row(0, RED)];
    let component = VirtualListComponent::new("list", &items, &state).scrollbar_marks(&marks);
    let tree = layout(&component, 5, 10);
    // Item 1 covers rows 10..60 of 100: cells 1 through 5.
    assert_eq!(
        component.scrollbar_mark_cells(&tree),
        [cells(0, 1, 5), cells(1, 0, 1)]
    );

    // With 100 estimated rows before the resident 100, resident row 0 is
    // halfway down the track.
    let estimated = VirtualListComponent::new("list", &items, &state)
        .scrollbar_marks(&marks)
        .position_estimate(PositionEstimate {
            before: 100,
            resident: 100,
            after: 0,
        });
    let tree = layout(&estimated, 5, 10);
    assert_eq!(
        estimated.scrollbar_mark_cells(&tree),
        [cells(0, 5, 3), cells(1, 5, 1)]
    );
    // An estimate whose resident rows disagree with the exact total is ignored.
    let ignored = VirtualListComponent::new("list", &items, &state)
        .scrollbar_marks(&marks)
        .position_estimate(PositionEstimate {
            before: 100,
            resident: 7,
            after: 0,
        });
    let tree = layout(&ignored, 5, 10);
    assert_eq!(
        ignored.scrollbar_mark_cells(&tree),
        [cells(0, 1, 5), cells(1, 0, 1)]
    );
}

#[test]
fn marks_draw_only_with_the_scrollbar() {
    let items = Items::uniform(4);
    let state = RefCell::new(VirtualListState::new(0));
    let marks = [ScrollbarMark::row(2, GREEN).over_thumb(true)];
    let bare = VirtualListComponent::new("list", &items, &state)
        .chrome(VirtualListChrome::bare())
        .scrollbar_marks(&marks);
    let tree = layout(&bare, 5, 10);
    assert_eq!(
        bare.scrollbar_mark_cells(&tree),
        [] as [ScrollbarMarkCells; 0]
    );
    assert!(
        column(&bare, &tree, 4)
            .iter()
            .all(|(_, style)| *style != GREEN)
    );

    let on_overflow = VirtualListComponent::new("list", &items, &state)
        .chrome(
            VirtualListChrome::interactive().scrollbar(ScrollbarAxisLayoutMode::OverlayOnOverflow),
        )
        .scrollbar_marks(&marks);
    let tree = layout(&on_overflow, 5, 10);
    assert_eq!(
        on_overflow.scrollbar_mark_cells(&tree),
        [] as [ScrollbarMarkCells; 0]
    );

    // A gutter scrollbar draws without overflow, and so do its marks.
    let gutter = VirtualListComponent::new("list", &items, &state).scrollbar_marks(&marks);
    let tree = layout(&gutter, 5, 10);
    assert_eq!(gutter.scrollbar_mark_cells(&tree), [cells(0, 5, 1)]);
    for (width, height) in [(0, 10), (5, 0), (1, 1)] {
        let tree = layout(&gutter, width, height);
        let reported = gutter.scrollbar_mark_cells(&tree);
        assert!(
            reported
                .iter()
                .all(|cells| cells.area.right() <= width && cells.area.bottom() <= height)
        );
    }
    // A stale layout node reports nothing.
    let outdated = layout(&gutter, 5, 10);
    let _ = layout(&gutter, 5, 9);
    assert_eq!(
        gutter.scrollbar_mark_cells(&outdated),
        [] as [ScrollbarMarkCells; 0]
    );
}

#[test]
fn keyed_marks_follow_insertions_and_change_the_paint_revision() {
    let items = Items::uniform(100);
    let state = RefCell::new(VirtualListState::new(0));
    let marks = [ScrollbarMark::item(30, GREEN)];
    let component = VirtualListComponent::new("list", &items, &state).scrollbar_marks(&marks);
    let tree = layout(&component, 5, 10);
    assert_eq!(component.scrollbar_mark_cells(&tree), [cells(0, 3, 1)]);
    let before = component.revision();

    items
        .0
        .borrow_mut()
        .splice(0..0, (1_000..1_050).map(|key| (key, 1)));
    state.borrow_mut().note_splice(0..0, 50);
    let tree = layout(&component, 5, 10);
    // Item 30 is now row 80 of 150.
    assert_eq!(component.scrollbar_mark_cells(&tree), [cells(0, 5, 1)]);

    let restyled = [ScrollbarMark::item(30, RED)];
    let other = VirtualListComponent::new("list", &items, &state).scrollbar_marks(&restyled);
    let _ = layout(&other, 5, 10);
    assert_ne!(component.revision().paint, other.revision().paint);
    assert_ne!(before.paint, component.revision().paint);
}
