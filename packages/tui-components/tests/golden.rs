use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, TextWrap};
use bmux_tui::style::{Color, Style};
use bmux_tui_components::badge::Badge;
use bmux_tui_components::breadcrumbs::{BreadcrumbItem, Breadcrumbs, BreadcrumbsState};
use bmux_tui_components::chart::{
    Chart, ChartBounds, ChartDataset, ChartLegendPlacement, ChartPoint, ChartPolicy,
};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBar};
use bmux_tui_components::menu::{Menu, MenuItem, MenuPolicy};
use bmux_tui_components::scroll_area::{
    ScrollArea, ScrollAreaPolicy, ScrollAreaScrollbarMode, ScrollAreaState,
};
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListItem, SelectableListPolicy, SelectableListState,
};
use bmux_tui_components::status_bar::{StatusBar, StatusSegment, StatusSeverity};
use bmux_tui_components::tab_bar::{TabBar, TabBarPolicy, TabBarState};
use bmux_tui_components::table::{Table, TableColumn, TableRow, TableState};
use bmux_tui_components::text_view::{TextView, TextViewPolicy, TextViewState};

fn buffer_rows(buffer: &Buffer) -> Vec<String> {
    (buffer.area().y..buffer.area().bottom())
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

#[test]
fn golden_styled_truncation_components() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 6));
    let mut frame = Frame::new(&mut buffer);

    Badge::new("very-long").render(Rect::new(0, 0, 6, 1), &mut frame);

    let breadcrumbs_items = [
        BreadcrumbItem::new("home", "Home"),
        BreadcrumbItem::new("docs", "Documentation"),
    ];
    Breadcrumbs::new(&breadcrumbs_items).render(
        Rect::new(0, 1, 8, 1),
        &BreadcrumbsState::new(Some(1)),
        &mut frame,
    );

    let hints = [KeyHint::new("Ctrl+O", "Open")];
    KeyHintBar::new(&hints).render(Rect::new(0, 2, 8, 1), &mut frame);

    let status_segments = [
        StatusSegment::new("ready"),
        StatusSegment::new("warning").severity(StatusSeverity::Warning),
    ];
    StatusBar::new()
        .left(&status_segments)
        .render(Rect::new(0, 3, 9, 1), &mut frame);

    let tabs = [
        bmux_tui_components::tab_bar::TabItem::new("one", "One"),
        bmux_tui_components::tab_bar::TabItem::rich(
            "two",
            Line::from_spans([Span::styled("VeryLong", Style::new().fg(Color::Green))]),
        ),
    ];
    TabBar::new(&tabs).policy(TabBarPolicy::bare()).render(
        Rect::new(0, 4, 9, 1),
        &TabBarState::new(Some(1)),
        &mut frame,
    );

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
fn golden_scroll_area_and_text_view_both_axis_scrollbars() {
    let lines = [
        Line::from("abcdef"),
        Line::from("ghijkl"),
        Line::from("mnopqr"),
    ];

    let mut scroll_buffer = Buffer::empty(Rect::new(0, 0, 4, 3));
    let mut scroll_frame = Frame::new(&mut scroll_buffer);
    let mut scroll_state = ScrollAreaState::new();
    scroll_state.set_vertical_offset(1);
    scroll_state.set_horizontal_offset(1);
    ScrollArea::new(&lines)
        .policy(
            ScrollAreaPolicy::interactive()
                .scrollbar(ScrollAreaScrollbarMode::Gutter)
                .horizontal_scrollbar(ScrollAreaScrollbarMode::Gutter),
        )
        .render(Rect::new(0, 0, 4, 3), &scroll_state, &mut scroll_frame);

    assert_eq!(
        buffer_rows(scroll_frame.buffer()),
        vec!["hij│", "nop█", "█── "]
    );

    let mut text_buffer = Buffer::empty(Rect::new(0, 0, 4, 3));
    let mut text_frame = Frame::new(&mut text_buffer);
    let mut text_state = TextViewState::new();
    text_state.set_vertical_scroll(1);
    text_state.set_horizontal_scroll(1);
    TextView::new(&lines)
        .policy(
            TextViewPolicy::bare()
                .vertical_scrollbar(ScrollAreaScrollbarMode::Gutter)
                .horizontal_scrollbar(ScrollAreaScrollbarMode::Gutter),
        )
        .render(Rect::new(0, 0, 4, 3), &text_state, &mut text_frame);

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
    Table::new(&columns, &rows).render(
        Rect::new(0, 0, 5, 2),
        &TableState::new(Some(0)),
        &mut table_frame,
    );
    assert_eq!(buffer_rows(table_frame.buffer()), vec!["Name ", "abcd…"]);

    let text_lines = [Line::from_spans([
        Span::styled("one ", Style::new().fg(Color::Red)),
        Span::styled("two", Style::new().fg(Color::Blue)),
    ])];
    let mut text_buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
    let mut text_frame = Frame::new(&mut text_buffer);
    TextView::new(&text_lines)
        .policy(TextViewPolicy {
            wrap: TextWrap::Word,
            ..TextViewPolicy::bare()
        })
        .render(
            Rect::new(0, 0, 4, 2),
            &TextViewState::new(),
            &mut text_frame,
        );
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
    let mut list_buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
    let mut list_frame = Frame::new(&mut list_buffer);
    SelectableList::new(&list_items)
        .policy(SelectableListPolicy::interactive().scrollbar(ScrollAreaScrollbarMode::Gutter))
        .render(Rect::new(0, 0, 6, 2), &list_state, &mut list_frame);
    assert_eq!(buffer_rows(list_frame.buffer()), vec!["  Two│", "  Thr█"]);

    let menu_items = [MenuItem::rich(
        "new",
        Line::from_spans([Span::styled("New", Style::new().fg(Color::Yellow))]),
    )
    .submenu(true)];
    let mut menu_buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
    let mut menu_frame = Frame::new(&mut menu_buffer);
    Menu::new(&menu_items)
        .policy(MenuPolicy {
            submenu_indicator: ">",
            ..MenuPolicy::default()
        })
        .render(
            Rect::new(0, 0, 10, 1),
            &bmux_tui_components::menu::MenuState::new(Some(0)),
            &mut menu_frame,
        );
    assert_eq!(buffer_rows(menu_frame.buffer()), vec!["> New >   "]);

    let points_a = [ChartPoint::new(0.0, 0.0)];
    let points_b = [ChartPoint::new(1.0, 1.0)];
    let datasets = [
        ChartDataset::scatter("alpha", &points_a),
        ChartDataset::scatter("beta", &points_b),
    ];
    let mut chart_buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
    let mut chart_frame = Frame::new(&mut chart_buffer);
    Chart::new(&datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0))
        .policy(ChartPolicy::compact().legend(ChartLegendPlacement::TopRight))
        .render(Rect::new(0, 0, 8, 2), &mut chart_frame);
    assert_eq!(
        buffer_rows(chart_frame.buffer()),
        vec!["alpha b…", "•       "]
    );
}
