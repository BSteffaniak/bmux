#![cfg(feature = "all")]

use bmux_tui::buffer::Buffer;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, TextWrap};
use bmux_tui::style::{Color, Style};
use bmux_tui_components::badge::BadgeComponent;
use bmux_tui_components::breadcrumbs::{BreadcrumbItem, BreadcrumbsComponent, BreadcrumbsState};
use bmux_tui_components::chart::{
    Chart, ChartBounds, ChartDataset, ChartLegendPlacement, ChartPoint, ChartPolicy,
};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBarComponent};
use bmux_tui_components::menu::{MenuComponent, MenuItem, MenuPolicy};
use bmux_tui_components::scroll_view::ScrollViewState;
use bmux_tui_components::scrollbar_layout::ScrollbarAxisLayoutMode;
use bmux_tui_components::selectable_list::{
    SelectableListComponent, SelectableListItem, SelectableListPolicy, SelectableListState,
};
use bmux_tui_components::status_bar::{StatusBarComponent, StatusSegment, StatusSeverity};
use bmux_tui_components::tab_bar::{TabBarComponent, TabBarPolicy, TabBarState};
use bmux_tui_components::table::{TableColumn, TableComponent, TableRow, TableState};
use bmux_tui_components::text_view::{TextViewComponent, TextViewPolicy};

fn buffer_rows(buffer: &Buffer) -> Vec<String> {
    (buffer.area().y..buffer.area().bottom())
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

#[test]
fn golden_styled_truncation_components() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 6));
    let mut frame = Frame::new(&mut buffer);

    let badge = BadgeComponent::new("golden.badge", "very-long");
    let badge_layout = badge.layout(
        Constraints::tight(Rect::new(0, 0, 6, 1).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut frame).with_child(0, 0, LocalRect::new(0, 0, 6, 1), |cx| {
        badge.paint(&badge_layout, cx);
    });

    let breadcrumbs_items = [
        BreadcrumbItem::new("home", "Home"),
        BreadcrumbItem::new("docs", "Documentation"),
    ];
    let breadcrumbs_state = std::cell::Cell::new(BreadcrumbsState::new(Some(1)));
    let breadcrumbs =
        BreadcrumbsComponent::new("golden.breadcrumbs", &breadcrumbs_items, &breadcrumbs_state);
    let breadcrumbs_layout = breadcrumbs.layout(
        Constraints::tight(Rect::new(0, 1, 8, 1).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut frame).with_child(0, 1, LocalRect::new(0, 0, 8, 1), |cx| {
        breadcrumbs.paint(&breadcrumbs_layout, cx);
    });

    let hints = [KeyHint::new("Ctrl+O", "Open")];
    let hints_component = KeyHintBarComponent::new("golden.hints", &hints);
    let hints_layout = hints_component.layout(
        Constraints::tight(Rect::new(0, 2, 8, 1).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut frame).with_child(0, 2, LocalRect::new(0, 0, 8, 1), |cx| {
        hints_component.paint(&hints_layout, cx);
    });

    let status_segments = [
        StatusSegment::new("ready"),
        StatusSegment::new("warning").severity(StatusSeverity::Warning),
    ];
    let status = StatusBarComponent::new("golden.status").left(&status_segments);
    let status_layout = status.layout(
        Constraints::tight(Rect::new(0, 3, 9, 1).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut frame).with_child(0, 3, LocalRect::new(0, 0, 9, 1), |cx| {
        status.paint(&status_layout, cx);
    });

    let tabs = [
        bmux_tui_components::tab_bar::TabItem::new("one", "One"),
        bmux_tui_components::tab_bar::TabItem::rich(
            "two",
            Line::from_spans([Span::styled("VeryLong", Style::new().fg(Color::Green))]),
        ),
    ];
    let tabs_state = std::cell::RefCell::new(TabBarState::new(Some(1)));
    let tabs_component =
        TabBarComponent::new("golden.tabs", &tabs, &tabs_state).policy(TabBarPolicy::bare());
    let tabs_layout = tabs_component.layout(
        Constraints::tight(Rect::new(0, 4, 9, 1).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut frame).with_child(0, 4, LocalRect::new(0, 0, 9, 1), |cx| {
        tabs_component.paint(&tabs_layout, cx);
    });

    assert_eq!(
        buffer_rows(frame.buffer()),
        vec![
            "[ ver…      ",
            "Home / …    ",
            "Ctrl+O …    ",
            "ready · …   ",
            " One   V…   ",
            "            ",
        ]
    );
}

#[test]
fn golden_text_view_both_axis_scrollbars() {
    let lines = [
        Line::from("abcdef"),
        Line::from("ghijkl"),
        Line::from("mnopqr"),
    ];

    let mut text_buffer = Buffer::empty(Rect::new(0, 0, 4, 3));
    let mut text_frame = Frame::new(&mut text_buffer);
    let mut text_state = ScrollViewState::new();
    text_state.set_vertical_offset(1);
    text_state.set_horizontal_offset(1);
    let text_state = std::cell::Cell::new(text_state);
    let view = TextViewComponent::new("golden.text-view", &lines, &text_state).policy(
        TextViewPolicy::bare()
            .vertical_scrollbar(ScrollbarAxisLayoutMode::Gutter)
            .horizontal_scrollbar(ScrollbarAxisLayoutMode::Gutter),
    );
    let layout = view.layout(
        Constraints::tight(Rect::new(0, 0, 4, 3).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut text_frame).with_child(0, 0, LocalRect::new(0, 0, 4, 3), |cx| {
        view.paint(&layout, cx);
    });

    assert_eq!(
        buffer_rows(text_frame.buffer()),
        vec!["hij│", "nop█", "█── "]
    );
}

#[test]
fn golden_table_rich_truncation_and_text_view_wrapping() {
    let columns = [TableColumn::new("Name").fixed(5)];
    let rows = [TableRow::rich([Line::from_spans([
        Span::styled("abc", Style::new().fg(Color::Red)),
        Span::styled("def", Style::new().fg(Color::Blue)),
    ])])];
    let mut table_buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
    let mut table_frame = Frame::new(&mut table_buffer);
    let table_state = std::cell::RefCell::new(TableState::new(Some(0)));
    let table = TableComponent::new("golden.table", &columns, &rows, &table_state);
    let table_layout = table.layout(
        Constraints::tight(Rect::new(0, 0, 5, 2).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut table_frame).with_child(0, 0, LocalRect::new(0, 0, 5, 2), |cx| {
        table.paint(&table_layout, cx);
    });
    assert_eq!(buffer_rows(table_frame.buffer()), vec!["Name ", "abcd…"]);

    let text_lines = [Line::from_spans([
        Span::styled("one ", Style::new().fg(Color::Red)),
        Span::styled("two", Style::new().fg(Color::Blue)),
    ])];
    let mut text_buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
    let mut text_frame = Frame::new(&mut text_buffer);
    let text_state = std::cell::Cell::new(ScrollViewState::new());
    let view =
        TextViewComponent::new("golden.wrapped", &text_lines, &text_state).policy(TextViewPolicy {
            wrap: TextWrap::Word,
            ..TextViewPolicy::bare()
        });
    let layout = view.layout(
        Constraints::tight(Rect::new(0, 0, 4, 2).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut text_frame).with_child(0, 0, LocalRect::new(0, 0, 4, 2), |cx| {
        view.paint(&layout, cx);
    });
    assert_eq!(buffer_rows(text_frame.buffer()), vec!["one ", "two "]);
}

#[test]
fn golden_recent_list_menu_and_chart_polish() {
    let list_items = [
        SelectableListItem::new("one", "One"),
        SelectableListItem::new("two", "Two"),
        SelectableListItem::new("three", "Three"),
    ];
    let mut list_state = SelectableListState::new(Some(0));
    list_state.set_vertical_scroll(1);
    let list_state = std::cell::Cell::new(list_state);
    let mut list_buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
    let mut list_frame = Frame::new(&mut list_buffer);
    let list = SelectableListComponent::new("golden.list", &list_items, &list_state)
        .policy(SelectableListPolicy::interactive().scrollbar(ScrollbarAxisLayoutMode::Gutter));
    let list_layout = list.layout(
        Constraints::tight(Rect::new(0, 0, 6, 2).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut list_frame).with_child(0, 0, LocalRect::new(0, 0, 6, 2), |cx| {
        list.paint(&list_layout, cx);
    });
    assert_eq!(buffer_rows(list_frame.buffer()), vec!["  Two│", "  Thr█"]);

    let menu_items = [MenuItem::rich(
        "new",
        Line::from_spans([Span::styled("New", Style::new().fg(Color::Yellow))]),
    )
    .submenu(true)];
    let menu_state = std::cell::Cell::new(bmux_tui_components::menu::MenuState::new(Some(0)));
    let mut menu_buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
    let mut menu_frame = Frame::new(&mut menu_buffer);
    let menu = MenuComponent::new("golden.menu", &menu_items, &menu_state).policy(MenuPolicy {
        submenu_indicator: ">",
        ..MenuPolicy::default()
    });
    let menu_layout = menu.layout(
        Constraints::tight(Rect::new(0, 0, 10, 1).size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(&mut menu_frame).with_child(0, 0, LocalRect::new(0, 0, 10, 1), |cx| {
        menu.paint(&menu_layout, cx);
    });
    assert_eq!(buffer_rows(menu_frame.buffer()), vec!["> New >   "]);

    let points_a = [ChartPoint::new(0.0, 0.0)];
    let points_b = [ChartPoint::new(1.0, 1.0)];
    let datasets = [
        ChartDataset::scatter("alpha", &points_a),
        ChartDataset::scatter("beta", &points_b),
    ];
    let mut chart_buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
    let mut chart_frame = Frame::new(&mut chart_buffer);
    let chart = Chart::new(&datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0))
        .policy(ChartPolicy::compact().legend(ChartLegendPlacement::TopRight));
    let chart_layout = chart.layout(
        Constraints::tight(chart_frame.buffer().area().size()),
        &mut LayoutCx::new(),
    );
    let chart_clip = LocalRect::new(0, 0, 8, 2);
    PaintCx::new(&mut chart_frame).with_child(0, 0, chart_clip, |cx| {
        chart.paint(&chart_layout, cx);
    });
    assert_eq!(
        buffer_rows(chart_frame.buffer()),
        vec!["alpha b…", "•       "]
    );
}
