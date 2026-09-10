use std::cell::{Cell, RefCell};

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::buffer::Buffer;
use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Text};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::breadcrumbs::{
    BreadcrumbItem, BreadcrumbsComponent, BreadcrumbsOutcome, BreadcrumbsState,
};
use bmux_tui_components::key_hint_bar::{
    KeyHint, KeyHintBarComponent, KeyHintBarPolicy, KeyHintBarStyles,
};
use bmux_tui_components::menu::{MenuComponent, MenuItem, MenuOutcome, MenuState};
use bmux_tui_components::pane::{
    Pane, PaneComponent, PaneMousePolicy, PaneOutcome, PanePolicy, PaneState,
};
use bmux_tui_components::scroll_view::{ScrollView, ScrollViewComponent, ScrollViewState};
use bmux_tui_components::selectable_list::{
    SelectableListComponent, SelectableListItem, SelectableListOutcome, SelectableListState,
};
use bmux_tui_components::status_bar::{
    MessageBarComponent, StatusBarComponent, StatusBarPolicy, StatusBarStyles, StatusSegment,
    StatusSeverity,
};
use bmux_tui_components::tab_bar::{TabBarComponent, TabBarState, TabBarStyles, TabItem};
use bmux_tui_components::table::{TableColumn, TableComponent, TableOutcome, TableRow, TableState};
use bmux_tui_components::text_view::{
    TextViewComponent, TextViewCursor, TextViewHighlight, TextViewSelection,
};
use bmux_tui_components::tree_view::{
    TreeViewComponent, TreeViewItem, TreeViewOutcome, TreeViewState, TreeViewStyles,
};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 18;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Breadcrumbs,
    Tabs,
    Tree,
    List,
    Menu,
    Scroll,
    Table,
    Text,
    Pane,
}

impl Target {
    const ORDER: [Self; 9] = [
        Self::Tabs,
        Self::Breadcrumbs,
        Self::List,
        Self::Menu,
        Self::Tree,
        Self::Scroll,
        Self::Table,
        Self::Text,
        Self::Pane,
    ];

    fn next(self, backwards: bool) -> Self {
        let index = Self::ORDER
            .iter()
            .position(|target| *target == self)
            .expect("target in focus order");
        Self::ORDER[(index + if backwards { Self::ORDER.len() - 1 } else { 1 }) % Self::ORDER.len()]
    }
}

pub struct NavigationDemo {
    focused: Target,
    terminal_focused: bool,
    captured: Option<Target>,
    hovered: Option<Target>,
    committed_hits: Option<bmux_tui::hit::HitMap>,
    breadcrumbs: BreadcrumbsState,
    tabs: TabBarState,
    tree: TreeViewState,
    list: SelectableListState,
    menu: MenuState,
    table: TableState,
    scroll: ScrollViewState,
    text: ScrollViewState,
    pane_scroll: ScrollViewState,
    message: String,
}

impl NavigationDemo {
    #[must_use]
    pub fn new() -> Self {
        let mut demo = Self {
            focused: Target::Tabs,
            terminal_focused: true,
            captured: None,
            hovered: None,
            committed_hits: None,
            breadcrumbs: BreadcrumbsState::new(Some(1)),
            tabs: TabBarState::new(Some(0)),
            tree: {
                let mut state = TreeViewState::new(Some(0));
                state.set_expanded("src", true);
                state
            },
            list: SelectableListState::new(Some(1)),
            menu: MenuState::new(Some(0)),
            table: TableState::new(Some(0)),
            scroll: {
                let mut state = ScrollViewState::new();
                state.set_vertical_offset(1);
                state
            },
            text: ScrollViewState::new(),
            pane_scroll: ScrollViewState::new(),
            message: "Use arrows/Enter, wheel over scroll pane, q quits".to_string(),
        };
        // No committed scene exists yet, so no control claims visual focus.
        demo.sync_focus();
        demo
    }

    /// Install geometry only after terminal output has committed successfully.
    pub fn commit_hits(&mut self, hits: &bmux_tui::hit::HitMap) {
        self.committed_hits = Some(hits.clone());
        for target in [self.captured, self.hovered].into_iter().flatten() {
            if !self.target_visible(target) {
                self.dispatch_target(
                    &Event::Focus(bmux_tui::event::FocusEvent::Lost),
                    Some(target),
                );
            }
        }
        if self
            .captured
            .is_some_and(|target| !self.target_visible(target))
        {
            self.captured = None;
        }
        if self
            .hovered
            .is_some_and(|target| !self.target_visible(target))
        {
            self.hovered = None;
        }
        self.sync_focus();
    }

    fn target_visible(&self, target: Target) -> bool {
        self.committed_hits.as_ref().is_some_and(|hits| {
            hits.regions().iter().any(|region| {
                region.enabled
                    && region.visible
                    && region.focus_scope.is_none()
                    && !region.area.is_empty()
                    && Self::target_for_id(region.id.as_str()) == Some(target)
            })
        })
    }

    fn focus_eligible(&self, target: Target) -> bool {
        self.committed_hits.as_ref().is_some_and(|hits| {
            hits.focus_targets(None)
                .iter()
                .any(|id| Self::target_for_id(id.as_str()) == Some(target))
        })
    }

    fn sync_focus(&mut self) {
        let active = self.terminal_focused && self.focus_eligible(self.focused);
        self.tabs
            .set_focused(active && self.focused == Target::Tabs);
        self.breadcrumbs
            .set_focused(active && self.focused == Target::Breadcrumbs);
        self.tree.interaction.focused = active && self.focused == Target::Tree;
        self.list.interaction.focused = active && self.focused == Target::List;
        self.menu.list.interaction.focused = active && self.focused == Target::Menu;
        self.table.interaction.focused = active && self.focused == Target::Table;
    }

    fn target_at(&self, point: bmux_tui::geometry::Point) -> Option<Target> {
        let hits = self.committed_hits.as_ref()?;
        let hit = hits.hit_test(point)?;
        Self::target_for_id(hit.id().as_str())
    }

    fn mouse_target(&self, mouse: bmux_tui::event::MouseEvent) -> Option<Target> {
        let Some(hits) = &self.committed_hits else {
            return self.target_at(mouse.position);
        };
        Self::target_for_id(hits.hit_mouse(mouse)?.id().as_str())
    }

    fn target_for_id(id: &str) -> Option<Target> {
        [
            ("navigation.breadcrumbs", Target::Breadcrumbs),
            ("navigation.tabs", Target::Tabs),
            ("navigation.tree", Target::Tree),
            ("navigation.list", Target::List),
            ("navigation.menu", Target::Menu),
            ("navigation.scroll-pane", Target::Pane),
            ("navigation.scroll", Target::Scroll),
            ("navigation.table", Target::Table),
            ("navigation.text", Target::Text),
        ]
        .into_iter()
        .find_map(|(prefix, target)| {
            (id == prefix
                || id
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('.')))
            .then_some(target)
        })
    }

    pub fn render(&self, cx: &mut PaintCx<'_, '_>) {
        render_navigation_with_state(cx, self);
    }

    pub fn handle_event(&mut self, event: &Event) -> bool {
        if matches!(event, Event::Key(stroke) if should_quit(*stroke)) {
            return true;
        }
        if matches!(event, Event::Focus(_) | Event::Resize(_)) {
            if let Event::Focus(focus) = event {
                self.terminal_focused = *focus == bmux_tui::event::FocusEvent::Gained;
            }
            self.captured = None;
            self.hovered = None;
            for target in Target::ORDER {
                self.dispatch_target(event, Some(target));
            }
            self.sync_focus();
            return false;
        }
        if !self.terminal_focused {
            return false;
        }
        if let Event::Key(stroke) = event
            && stroke.key == KeyCode::Tab
            && !stroke.modifiers.ctrl
            && !stroke.modifiers.alt
            && !stroke.modifiers.super_key
        {
            for _ in Target::ORDER {
                self.focused = self.focused.next(stroke.modifiers.shift);
                if self.focus_eligible(self.focused) {
                    break;
                }
            }
            self.sync_focus();
            return false;
        }
        if let Event::Mouse(mouse) = event
            && matches!(mouse.kind, bmux_tui::event::MouseEventKind::Move)
        {
            let next = self.target_at(mouse.position);
            if self.hovered != next {
                if let Some(previous) = self.hovered {
                    // The old control receives the outside position to clear hover;
                    // this is not an activation or keyboard broadcast.
                    self.dispatch_target(event, Some(previous));
                }
                self.hovered = next;
            }
        }
        let target = match event {
            Event::Mouse(mouse) => {
                use bmux_tui::event::MouseEventKind;
                match mouse.kind {
                    MouseEventKind::Down(_) => {
                        self.captured = self.mouse_target(*mouse);
                        if let Some(target) = self.captured {
                            self.focused = target;
                        }
                        self.captured
                    }
                    MouseEventKind::Drag(_) => self.captured,
                    MouseEventKind::Up(_) => self.captured.take(),
                    _ => self.mouse_target(*mouse),
                }
            }
            Event::Key(_) | Event::Paste(_) => {
                self.focus_eligible(self.focused).then_some(self.focused)
            }
            _ => None,
        };
        let outcome = self.dispatch_target(event, target);
        self.sync_focus();
        outcome
    }

    fn dispatch_target(&mut self, event: &Event, target: Option<Target>) -> bool {
        if target == Some(Target::Breadcrumbs) {
            let breadcrumb_items = breadcrumb_items();
            let breadcrumb_state = Cell::new(self.breadcrumbs);
            let component = BreadcrumbsComponent::new(
                "navigation.breadcrumbs",
                &breadcrumb_items,
                &breadcrumb_state,
            );
            let area = Rect::new(30, 0, 38, 1);
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            let outcome = EventCx::new(&layout).with_transform(
                0,
                0,
                i32::from(area.x),
                i64::from(area.y),
                area,
                |cx| component.handle_event(event, &layout, cx),
            );
            self.breadcrumbs = breadcrumb_state.get();
            if let BreadcrumbsOutcome::Activated { id, .. } = outcome {
                self.message = format!("Breadcrumb activated: {id}");
                return false;
            }
        }
        if target == Some(Target::Tabs) {
            let tab_items = tab_items();
            let previous_selection = self.tabs.selected();
            let tab_state = RefCell::new(std::mem::take(&mut self.tabs));
            let component = TabBarComponent::new("navigation.tabs", &tab_items, &tab_state);
            let area = TABS_AREA;
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            EventCx::new(&layout).with_transform(
                0,
                0,
                i32::from(area.x),
                i64::from(area.y),
                area,
                |cx| component.event(event, &layout, cx),
            );
            self.tabs = tab_state.into_inner();
            if self.tabs.selected() != previous_selection
                && let Some(index) = self.tabs.selected()
            {
                self.message = format!("Tab selected: {}", tab_items[index].label());
                return false;
            }
        }
        if target == Some(Target::Tree) {
            let tree_items = tree_items();
            let tree_state = RefCell::new(std::mem::take(&mut self.tree));
            let component = TreeViewComponent::new("navigation.tree", &tree_items, &tree_state);
            let area = TREE_AREA;
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            let outcome = EventCx::new(&layout).with_transform(
                0,
                0,
                i32::from(area.x),
                i64::from(area.y),
                area,
                |cx| component.handle_event(event, &layout, cx),
            );
            self.tree = tree_state.into_inner();
            match outcome {
                TreeViewOutcome::Selected { source, .. } => {
                    self.message = format!("Tree selected: {}", tree_items[source].label);
                    return false;
                }
                TreeViewOutcome::Toggled {
                    source, expanded, ..
                } => {
                    self.message =
                        format!("Tree {} expanded: {expanded}", tree_items[source].label);
                    return false;
                }
                TreeViewOutcome::Focused { source, .. } => {
                    self.message = format!("Tree focus: {}", tree_items[source].label);
                    return false;
                }
                TreeViewOutcome::Ignored | TreeViewOutcome::Redraw => {}
            }
        }
        if target == Some(Target::List) {
            let list_items = list_items();
            let list_state = Cell::new(self.list);
            let component =
                SelectableListComponent::new("navigation.list", &list_items, &list_state);
            let area = LIST_AREA;
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            let outcome = EventCx::new(&layout).with_transform(
                0,
                0,
                i32::from(area.x),
                i64::from(area.y),
                area,
                |cx| component.handle_event(event, &layout, cx),
            );
            self.list = list_state.get();
            match outcome {
                SelectableListOutcome::Selected(index) => {
                    self.message = format!("List selected: {}", list_item_text(&list_items[index]));
                    return false;
                }
                SelectableListOutcome::Focused(index) => {
                    self.message = format!("List focus: {}", list_item_text(&list_items[index]));
                    return false;
                }
                SelectableListOutcome::Ignored | SelectableListOutcome::Redraw => {}
            }
        }
        if target == Some(Target::Menu) {
            let menu_items = menu_items();
            let menu_state = Cell::new(self.menu);
            let component = MenuComponent::new("navigation.menu", &menu_items, &menu_state);
            let area = Rect::new(30, 1, 18, 2);
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            let outcome = EventCx::new(&layout).with_transform(
                0,
                0,
                i32::from(area.x),
                i64::from(area.y),
                area,
                |cx| component.handle_event(event, &layout, cx),
            );
            self.menu = menu_state.get();
            match outcome {
                MenuOutcome::Activated { id, .. } => self.message = format!("Menu action: {id}"),
                MenuOutcome::Cancelled => self.message = "Menu cancelled".to_string(),
                MenuOutcome::Ignored
                | MenuOutcome::Redraw
                | MenuOutcome::Focused(_)
                | MenuOutcome::Typeahead(_) => {}
            }
        }
        if target == Some(Target::Scroll) {
            let lines = scroll_lines();
            let area = Rect::new(1, 6, 24, 2);
            let layout = scroll_layout("navigation.scroll", area, &lines, self.scroll);
            if let bmux_tui_components::scroll_view::ScrollViewOutcome::Scrolled {
                vertical_offset,
            } = ScrollView::new().handle_event(area, &layout, &mut self.scroll, event)
            {
                self.message = format!("Scroll offset: {vertical_offset}");
            }
        }
        if target == Some(Target::Table) {
            let table_columns = table_columns();
            let table_rows = table_rows();
            let table_state = RefCell::new(std::mem::take(&mut self.table));
            let component = TableComponent::new(
                "navigation.table",
                &table_columns,
                &table_rows,
                &table_state,
            );
            let area = TABLE_AREA;
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            let outcome = EventCx::new(&layout).with_transform(
                0,
                0,
                i32::from(area.x),
                i64::from(area.y),
                area,
                |cx| component.handle_event(event, &layout, cx),
            );
            self.table = table_state.into_inner();
            match outcome {
                TableOutcome::Selected(index) => {
                    self.message =
                        format!("Table selected: {}", table_rows[index].cell_plain_text(0));
                    return false;
                }
                TableOutcome::Focused(index) => {
                    self.message = format!("Table focus: {}", table_rows[index].cell_plain_text(0));
                    return false;
                }
                TableOutcome::Ignored | TableOutcome::Redraw => {}
            }
        }
        if target == Some(Target::Text) {
            let text_lines = text_view_lines();
            let text_highlights = text_view_highlights();
            let text_state = Cell::new(self.text);
            let text_area = Rect::new(1, 13, 68, 2);
            let text_view = text_view_component(&text_lines, &text_highlights, &text_state);
            let text_layout =
                text_view.layout(Constraints::tight(text_area.size()), &mut LayoutCx::new());
            let text_outcome = text_view.handle_event(text_area, &text_layout, event);
            self.text = text_state.get();
            if let bmux_tui_components::scroll_view::ScrollViewOutcome::Scrolled {
                vertical_offset,
            } = text_outcome
            {
                self.message = format!("Text scrolled: {vertical_offset}");
                return false;
            }
        }
        if target != Some(Target::Pane) {
            return false;
        }
        let pane = scroll_delegate_pane();
        let mut pane_state = PaneState::new(scroll_delegate_pane_area());
        if let PaneOutcome::ScrollDelegated { direction } =
            pane.handle_event(&mut pane_state, event)
        {
            let delegated = match direction {
                bmux_tui_components::pane::ScrollDirection::Up => {
                    bmux_tui::event::MouseEventKind::ScrollUp
                }
                bmux_tui_components::pane::ScrollDirection::Down => {
                    bmux_tui::event::MouseEventKind::ScrollDown
                }
                bmux_tui_components::pane::ScrollDirection::Left
                | bmux_tui_components::pane::ScrollDirection::Right => return false,
            };
            let pane_lines = pane_scroll_lines();
            let area = pane.inner_area(&pane_state);
            let layout = scroll_layout(
                "navigation.scroll-pane.viewport",
                area,
                &pane_lines,
                self.pane_scroll,
            );
            if let bmux_tui_components::scroll_view::ScrollViewOutcome::Scrolled {
                vertical_offset,
            } = ScrollView::new().handle_event(
                area,
                &layout,
                &mut self.pane_scroll,
                &Event::Mouse(bmux_tui::event::MouseEvent::new(
                    delegated,
                    bmux_tui::geometry::Point::new(area.x, area.y),
                )),
            ) {
                self.message = format!("Delegated pane scroll offset: {vertical_offset}");
            }
        }
        false
    }
}

impl Default for NavigationDemo {
    fn default() -> Self {
        Self::new()
    }
}

pub fn render_navigation() -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut frame = Frame::new(&mut buffer);
    NavigationDemo::new().render(&mut PaintCx::new(&mut frame));
    buffer
}

fn scroll_layout(
    id: &str,
    area: Rect,
    lines: &[Line],
    state: ScrollViewState,
) -> bmux_tui::component::LayoutNode {
    ScrollViewComponent::new(
        id.to_owned(),
        bmux_tui::component::LogicalSize::new(
            u64::from(area.width.saturating_sub(1)),
            u64::try_from(usize::from(area.height)).unwrap_or(u64::MAX),
        ),
        state,
        TextBlock::new(Text::from_lines(lines.to_vec())).id(format!("{id}.content")),
    )
    .layout(Constraints::tight(area.size()), &mut LayoutCx::new())
}

const TABS_AREA: Rect = Rect::new(1, 0, 26, 1);
const LIST_AREA: Rect = Rect::new(1, 1, 24, 4);
const TABLE_AREA: Rect = Rect::new(1, 9, 32, 4);
const TREE_AREA: Rect = Rect::new(48, 1, 22, 6);

fn render_component(component: &impl Component, area: Rect, cx: &mut PaintCx<'_, '_>) {
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    cx.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
}

fn render_navigation_with_state(cx: &mut PaintCx<'_, '_>, demo: &NavigationDemo) {
    let tab_items = tab_items();
    let tab_state = RefCell::new(demo.tabs.clone());
    render_component(
        &TabBarComponent::new("navigation.tabs", &tab_items, &tab_state).styles(TabBarStyles {
            normal: Style::new().fg(Color::BrightBlack),
            selected: Style::new()
                .fg(Color::Black)
                .bg(Color::BrightCyan)
                .add_modifier(Modifier::BOLD),
            focused: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            hovered: Style::new().fg(Color::BrightWhite),
            pressed: Style::new().fg(Color::Black).bg(Color::Cyan),
            disabled: Style::new().fg(Color::BrightBlack),
            separator: Style::new().fg(Color::BrightBlack),
        }),
        TABS_AREA,
        cx,
    );

    let breadcrumb_items = breadcrumb_items();
    let breadcrumb_state = Cell::new(demo.breadcrumbs);
    render_component(
        &BreadcrumbsComponent::new(
            "navigation.breadcrumbs",
            &breadcrumb_items,
            &breadcrumb_state,
        ),
        Rect::new(30, 0, 38, 1),
        cx,
    );

    let list_items = list_items();
    let list_state = Cell::new(demo.list);
    render_component(
        &SelectableListComponent::new("navigation.list", &list_items, &list_state),
        LIST_AREA,
        cx,
    );

    let menu_items = menu_items();
    let menu_state = Cell::new(demo.menu);
    render_component(
        &MenuComponent::new("navigation.menu", &menu_items, &menu_state),
        Rect::new(30, 1, 18, 2),
        cx,
    );

    let tree_items = tree_items();
    let tree_state = RefCell::new(demo.tree.clone());
    render_component(
        &TreeViewComponent::new("navigation.tree", &tree_items, &tree_state).styles(
            TreeViewStyles {
                normal: Style::new().fg(Color::BrightWhite),
                selected: Style::new()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
                hovered: Style::new().fg(Color::White),
                pressed: Style::new().fg(Color::Black).bg(Color::BrightGreen),
                disabled: Style::new().fg(Color::BrightBlack),
                marker: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            },
        ),
        TREE_AREA,
        cx,
    );

    let lines = scroll_lines();
    let area = Rect::new(1, 6, 24, 2);
    let component = ScrollViewComponent::new(
        "navigation.scroll",
        bmux_tui::component::LogicalSize::new(
            u64::from(area.width.saturating_sub(1)),
            u64::try_from(usize::from(area.height)).unwrap_or(u64::MAX),
        ),
        demo.scroll,
        TextBlock::new(Text::from_lines(lines)).id("navigation.scroll.content"),
    );
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    cx.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| {
            component.paint(&layout, cx);
            ScrollView::new().paint_chrome(
                "navigation.scroll",
                Rect::new(0, 0, area.width, area.height),
                &layout,
                &demo.scroll,
                cx,
            );
        },
    );

    let table_columns = table_columns();
    let table_rows = table_rows();
    let table_state = RefCell::new(demo.table.clone());
    render_component(
        &TableComponent::new(
            "navigation.table",
            &table_columns,
            &table_rows,
            &table_state,
        ),
        TABLE_AREA,
        cx,
    );

    let pane_area = scroll_delegate_pane_area();
    let pane_state = Cell::new(PaneState::new(pane_area));
    let pane_lines = pane_scroll_lines();
    let content_area = scroll_delegate_pane().inner_area(&pane_state.get());
    render_component(
        &PaneComponent::new(
            "navigation.scroll-pane",
            scroll_delegate_pane(),
            &pane_state,
            ScrollViewComponent::new(
                "navigation.scroll-pane.viewport",
                bmux_tui::component::LogicalSize::new(
                    u64::from(content_area.width),
                    u64::try_from(usize::from(content_area.height)).unwrap_or(u64::MAX),
                ),
                demo.pane_scroll,
                TextBlock::new(Text::from_lines(pane_lines)).id("navigation.scroll-pane.content"),
            ),
        ),
        pane_area,
        cx,
    );

    let text_lines = text_view_lines();
    let text_highlights = text_view_highlights();
    let text_state = Cell::new(demo.text);
    render_component(
        &text_view_component(&text_lines, &text_highlights, &text_state),
        Rect::new(1, 13, 68, 2),
        cx,
    );
    render_component(
        &MessageBarComponent::new("navigation.message", &demo.message)
            .severity(StatusSeverity::Info)
            .styles(StatusBarStyles {
                default: Style::new().fg(Color::BrightYellow),
                muted: Style::new().fg(Color::BrightBlack),
                info: Style::new()
                    .fg(Color::BrightYellow)
                    .add_modifier(Modifier::BOLD),
                success: Style::new().fg(Color::Green),
                warning: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                separator: Style::new().fg(Color::BrightBlack),
                background: Style::new().bg(Color::Black),
            })
            .policy(StatusBarPolicy::compact().background(true)),
        Rect::new(1, 15, 60, 1),
        cx,
    );

    let status_left = [StatusSegment::new("nav").severity(StatusSeverity::Info)];
    let status_right = [StatusSegment::new("ready").severity(StatusSeverity::Success)];
    render_component(
        &StatusBarComponent::new("navigation.status")
            .left(&status_left)
            .right(&status_right)
            .styles(StatusBarStyles {
                default: Style::new().fg(Color::White).bg(Color::Blue),
                muted: Style::new().fg(Color::BrightBlack).bg(Color::Blue),
                info: Style::new().fg(Color::BrightCyan).bg(Color::Blue),
                success: Style::new().fg(Color::BrightGreen).bg(Color::Blue),
                warning: Style::new().fg(Color::Yellow).bg(Color::Blue),
                error: Style::new().fg(Color::Red).bg(Color::Blue),
                separator: Style::new().fg(Color::BrightBlack).bg(Color::Blue),
                background: Style::new().bg(Color::Blue),
            })
            .policy(StatusBarPolicy::compact().background(true)),
        Rect::new(1, 16, 68, 1),
        cx,
    );

    let hints = [
        KeyHint::new("↑↓", "move"),
        KeyHint::new("←→", "tabs/tree"),
        KeyHint::new("enter", "select"),
        KeyHint::new("q", "quit"),
    ];
    render_component(
        &KeyHintBarComponent::new("navigation.hints", &hints)
            .styles(KeyHintBarStyles {
                key: Style::new()
                    .fg(Color::BrightWhite)
                    .bg(Color::BrightBlack)
                    .add_modifier(Modifier::BOLD),
                label: Style::new().fg(Color::BrightBlack).bg(Color::Black),
                separator: Style::new().fg(Color::BrightBlack).bg(Color::Black),
                disabled: Style::new().fg(Color::BrightBlack).bg(Color::Black),
                background: Style::new().bg(Color::Black),
            })
            .policy(KeyHintBarPolicy::compact().background(true)),
        Rect::new(1, 17, 68, 1),
        cx,
    );
}

fn committed_demo() -> NavigationDemo {
    let mut demo = NavigationDemo::new();
    let mut terminal =
        bmux_tui::terminal::Terminal::new(std::io::sink(), Rect::new(0, 0, WIDTH, HEIGHT));
    terminal
        .draw(|cx| demo.render(cx))
        .expect("headless draw succeeds");
    demo.commit_hits(terminal.hits());
    demo
}

pub fn demonstrate_menu_activation() -> MenuOutcome {
    let items = menu_items();
    let state = Cell::new(MenuState::new(Some(0)));
    let menu = MenuComponent::new("navigation.menu.demo", &items, &state);
    let layout = menu.layout(
        Constraints::tight(Rect::new(0, 0, 16, 2).size()),
        &mut LayoutCx::new(),
    );
    menu.handle_event(
        &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        &layout,
        &mut EventCx::new(&layout),
    )
}

pub fn demonstrate_pane_scroll_delegation() -> PaneOutcome {
    let pane = scroll_delegate_pane();
    let mut state = PaneState::new(Rect::new(0, 0, 10, 5));
    pane.handle_event(
        &mut state,
        &bmux_tui::event::Event::Mouse(bmux_tui::event::MouseEvent::new(
            bmux_tui::event::MouseEventKind::ScrollDown,
            bmux_tui::geometry::Point::new(2, 2),
        )),
    )
}

pub fn demonstrate_delegated_pane_scroll_offset() -> u64 {
    let mut demo = committed_demo();
    let _ = demo.handle_event(&Event::Mouse(bmux_tui::event::MouseEvent::new(
        bmux_tui::event::MouseEventKind::ScrollDown,
        bmux_tui::geometry::Point::new(32, 8),
    )));
    demo.pane_scroll.vertical_offset()
}

pub fn demonstrate_tree_selection() -> String {
    let mut demo = committed_demo();
    let _ = demo.handle_event(&Event::Mouse(bmux_tui::event::MouseEvent::new(
        bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
        bmux_tui::geometry::Point::new(50, 3),
    )));
    let _ = demo.handle_event(&Event::Mouse(bmux_tui::event::MouseEvent::new(
        bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
        bmux_tui::geometry::Point::new(50, 3),
    )));
    demo.message
}

pub fn demonstrate_breadcrumb_activation() -> String {
    let mut demo = committed_demo();
    demo.focused = Target::Breadcrumbs;
    let _ = demo.handle_event(&Event::Key(KeyStroke::simple(KeyCode::Enter)));
    demo.message
}

pub fn demonstrate_text_view_scroll() -> u64 {
    let mut demo = committed_demo();
    let _ = demo.handle_event(&Event::Mouse(bmux_tui::event::MouseEvent::new(
        bmux_tui::event::MouseEventKind::ScrollDown,
        bmux_tui::geometry::Point::new(2, 13),
    )));
    demo.text.vertical_offset()
}

pub fn demonstrate_table_selection() -> String {
    let columns = table_columns();
    let rows = table_rows();
    let state = RefCell::new(TableState::new(Some(0)));
    let table = TableComponent::new("navigation.table.demo", &columns, &rows, &state);
    let layout = table.layout(
        Constraints::tight(Rect::new(0, 0, 24, 4).size()),
        &mut LayoutCx::new(),
    );
    let _ = table.handle_event(
        &Event::Key(KeyStroke::simple(KeyCode::Down)),
        &layout,
        &mut EventCx::new(&layout),
    );
    // Resolve again after selection changes, as a component host does before dispatch.
    let layout = table.layout(
        Constraints::tight(Rect::new(0, 0, 24, 4).size()),
        &mut LayoutCx::new(),
    );
    match table.handle_event(
        &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        &layout,
        &mut EventCx::new(&layout),
    ) {
        TableOutcome::Selected(index) => {
            format!("Table selected: {}", rows[index].cell_plain_text(0))
        }
        TableOutcome::Ignored | TableOutcome::Redraw | TableOutcome::Focused(_) => {
            "Table ignored".to_string()
        }
    }
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

fn breadcrumb_items() -> [BreadcrumbItem<'static>; 3] {
    [
        BreadcrumbItem::new("home", "Home"),
        BreadcrumbItem::new("library", "Library"),
        BreadcrumbItem::new("details", "Details"),
    ]
}

fn tab_items() -> [TabItem<'static>; 3] {
    [
        TabItem::new("list", "List"),
        TabItem::new("tree", "Tree"),
        TabItem::new("scroll", "Scroll"),
    ]
}

fn tree_items() -> [TreeViewItem; 4] {
    [
        TreeViewItem::new("src", "src", 0).expandable(true),
        TreeViewItem::new("lib", "lib.rs", 1),
        TreeViewItem::new("main", "main.rs", 1),
        TreeViewItem::new("readme", "README.md", 0),
    ]
}

fn list_item_text(item: &SelectableListItem) -> String {
    item.lines
        .iter()
        .map(Line::plain_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn list_items() -> [SelectableListItem; 3] {
    [
        SelectableListItem::new("one", "First item"),
        SelectableListItem::multiline(
            "two",
            [
                Line::from_spans([
                    Span::raw("Second "),
                    Span::styled(
                        "rich",
                        Style::new()
                            .fg(Color::BrightYellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from("details line"),
            ],
        ),
        SelectableListItem::new("three", "Third item"),
    ]
}

fn table_columns() -> [TableColumn<'static>; 3] {
    [
        TableColumn::new("Name").min(8),
        TableColumn::new("Kind").fixed(8),
        TableColumn::new("Progress").percentage(25),
    ]
}

fn table_rows() -> [TableRow; 3] {
    [
        TableRow::rich(vec![
            Line::from_spans([Span::styled(
                "alpha",
                Style::new()
                    .fg(Color::BrightCyan)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from("file"),
            Line::from("75%"),
        ]),
        TableRow::new(vec!["beta", "dir", "40%"]),
        TableRow::new(vec!["gamma", "link", "10%"]),
    ]
}

fn menu_items() -> [MenuItem; 2] {
    [
        MenuItem::new("open", "Open"),
        MenuItem::new("close", "Close"),
    ]
}

fn scroll_lines() -> [Line; 4] {
    [
        Line::from("Scroll zero"),
        Line::from("Scroll one"),
        Line::from("Scroll two"),
        Line::from("Scroll three"),
    ]
}

fn text_view_component<'a>(
    lines: &'a [Line],
    highlights: &'a [TextViewHighlight],
    state: &'a Cell<ScrollViewState>,
) -> TextViewComponent<'a, 'a> {
    TextViewComponent::new("navigation.text", lines, state)
        .highlights(highlights)
        .selection(Some(TextViewSelection::new(
            1,
            0,
            11,
            Style::new().bg(Color::Blue),
        )))
        .cursor(Some(TextViewCursor::new(
            0,
            8,
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )))
}

fn text_view_lines() -> [Line; 3] {
    [
        Line::from("TextView wraps long read-only content for details panes."),
        Line::from("Mouse wheel or PageDown scrolls without owning app state."),
        Line::from("The caller still owns the text lines."),
    ]
}

fn text_view_highlights() -> [TextViewHighlight; 1] {
    [TextViewHighlight::new(
        0,
        0,
        8,
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )]
}

fn pane_scroll_lines() -> [Line; 6] {
    [
        Line::from("Delegated line zero"),
        Line::from("Delegated line one"),
        Line::from("Delegated line two"),
        Line::from("Delegated line three"),
        Line::from("Delegated line four"),
        Line::from("Delegated line five"),
    ]
}

const fn scroll_delegate_pane_area() -> Rect {
    Rect::new(30, 6, 28, 7)
}

fn scroll_delegate_pane() -> Pane<'static> {
    Pane::new()
        .title("Scroll delegate")
        .padding(Insets::all(1))
        .policy(PanePolicy {
            mouse: PaneMousePolicy {
                enabled: true,
                click_to_focus: false,
                title_bar_drag: false,
                scroll_wheel: true,
                resize_handles: bmux_tui_components::pane::ResizeHandles::NONE,
            },
            bounds: Default::default(),
        })
}

fn should_quit(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Escape || stroke.key == KeyCode::Char('q')
}

#[cfg(test)]
mod tests {
    use bmux_tui_components::menu::MenuOutcome;
    use bmux_tui_components::pane::{PaneOutcome, ScrollDirection};

    use super::{
        demonstrate_breadcrumb_activation, demonstrate_delegated_pane_scroll_offset,
        demonstrate_menu_activation, demonstrate_pane_scroll_delegation,
        demonstrate_table_selection, demonstrate_text_view_scroll, demonstrate_tree_selection,
        render_navigation, rows,
    };

    #[test]
    fn focus_traversal_and_resize_clear_capture_without_broadcasting_keys() {
        let mut demo = super::committed_demo();
        let mut tab = bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Tab);
        demo.handle_event(&bmux_tui::event::Event::Key(tab));
        assert!(demo.focused == super::Target::Breadcrumbs);
        tab.modifiers.shift = true;
        demo.handle_event(&bmux_tui::event::Event::Key(tab));
        assert!(demo.focused == super::Target::Tabs);
        demo.captured = Some(super::Target::List);
        demo.hovered = Some(super::Target::List);
        demo.handle_event(&bmux_tui::event::Event::Resize(
            bmux_tui::geometry::Size::new(72, 18),
        ));
        assert!(demo.captured.is_none());
        assert!(demo.hovered.is_none());
        assert_eq!(demo.tabs.selected(), Some(0));
    }

    #[test]
    fn focus_loss_clears_visual_focus_and_suspends_input() {
        let mut demo = super::committed_demo();
        demo.focused = super::Target::List;
        demo.sync_focus();
        assert!(demo.list.interaction.focused);
        demo.handle_event(&bmux_tui::event::Event::Focus(
            bmux_tui::event::FocusEvent::Lost,
        ));
        assert!(!demo.list.interaction.focused);
        let before = demo.list.selected();
        demo.handle_event(&bmux_tui::event::Event::Key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Down),
        ));
        assert_eq!(demo.list.selected(), before);
    }

    #[test]
    fn committed_pane_wheel_reaches_delegated_content() {
        let mut demo = super::committed_demo();
        let mut terminal = bmux_tui::terminal::Terminal::new(
            Vec::<u8>::new(),
            bmux_tui::geometry::Rect::new(0, 0, 72, 18),
        );
        terminal.draw(|cx| demo.render(cx)).unwrap();
        demo.commit_hits(terminal.hits());
        demo.handle_event(&bmux_tui::event::Event::Mouse(
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::ScrollDown,
                bmux_tui::geometry::Point::new(32, 8),
            ),
        ));
        assert!(demo.pane_scroll.vertical_offset() > 0);
    }

    #[test]
    fn committed_wheel_routes_to_visible_scroll_content() {
        let mut demo = super::committed_demo();
        let mut terminal = bmux_tui::terminal::Terminal::new(
            Vec::<u8>::new(),
            bmux_tui::geometry::Rect::new(0, 0, 72, 18),
        );
        terminal.draw(|cx| demo.render(cx)).unwrap();
        demo.commit_hits(terminal.hits());
        for point in [
            bmux_tui::geometry::Point::new(2, 6),
            bmux_tui::geometry::Point::new(2, 13),
        ] {
            let before = (demo.scroll.vertical_offset(), demo.text.vertical_offset());
            demo.handle_event(&bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::ScrollDown,
                    point,
                ),
            ));
            assert_ne!(
                (demo.scroll.vertical_offset(), demo.text.vertical_offset()),
                before
            );
        }
    }

    #[test]
    fn committed_empty_scene_excludes_unpainted_allocations() {
        let mut demo = super::committed_demo();
        demo.commit_hits(&bmux_tui::hit::HitMap::new());
        assert!(
            demo.target_at(bmux_tui::geometry::Point::new(2, 1))
                .is_none()
        );
        let mut hits = bmux_tui::hit::HitMap::new();
        // Use a real committed terminal scene to preserve clipping and hit order.
        let mut terminal = bmux_tui::terminal::Terminal::new(
            Vec::<u8>::new(),
            bmux_tui::geometry::Rect::new(0, 0, 10, 2),
        );
        terminal.draw(|cx| demo.render(cx)).unwrap();
        hits.clone_from(terminal.hits());
        demo.commit_hits(&hits);
        assert!(
            demo.target_at(bmux_tui::geometry::Point::new(50, 2))
                .is_none()
        );
    }

    #[test]
    fn ignored_focused_key_does_not_reach_other_controls() {
        let mut demo = super::committed_demo();
        demo.focused = super::Target::Menu;
        let tabs = demo.tabs.selected();
        let scroll = demo.scroll.vertical_offset();
        demo.handle_event(&bmux_tui::event::Event::Key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Right),
        ));
        assert_eq!(demo.tabs.selected(), tabs);
        assert_eq!(demo.scroll.vertical_offset(), scroll);
    }

    #[test]
    fn pointer_release_does_not_transfer_capture_to_another_control() {
        let mut demo = super::committed_demo();
        for (kind, point) in [
            (
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(2, 1),
            ),
            (
                bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(10, 0),
            ),
        ] {
            demo.handle_event(&bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(kind, point),
            ));
        }
        assert_eq!(demo.tabs.selected(), Some(0));
        assert!(demo.captured.is_none());
    }

    #[test]
    fn pointer_only_region_does_not_grant_keyboard_focus() {
        let mut demo = super::committed_demo();
        let mut hits = bmux_tui::hit::HitMap::new();
        hits.push(
            bmux_tui::hit::HitRegion::new("navigation.list", super::LIST_AREA).focusable(false),
        );
        demo.focused = super::Target::List;
        demo.commit_hits(&hits);
        assert!(demo.target_visible(super::Target::List));
        assert!(!demo.focus_eligible(super::Target::List));
        assert!(!demo.list.interaction.focused);
        let before = demo.list.focused();
        demo.handle_event(&bmux_tui::event::Event::Key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Down),
        ));
        assert_eq!(demo.list.focused(), before);
    }

    #[test]
    fn hidden_committed_regions_cannot_keep_focus_or_capture() {
        let mut demo = super::committed_demo();
        demo.focused = super::Target::List;
        demo.captured = Some(super::Target::List);
        let mut hits = bmux_tui::hit::HitMap::new();
        hits.push(
            bmux_tui::hit::HitRegion::new(
                "navigation.list",
                bmux_tui::geometry::Rect::new(1, 1, 24, 4),
            )
            .visible(false),
        );
        demo.commit_hits(&hits);
        assert!(!demo.target_visible(super::Target::List));
        assert!(!demo.list.interaction.focused);
        assert!(demo.captured.is_none());
    }

    #[test]
    fn uncommitted_input_has_no_target() {
        let mut demo = super::NavigationDemo::new();
        assert!(
            demo.target_at(bmux_tui::geometry::Point::new(2, 1))
                .is_none()
        );
        demo.handle_event(&bmux_tui::event::Event::Key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Right),
        ));
        assert_eq!(demo.tabs.selected(), Some(0));
    }

    #[test]
    fn committed_overlap_does_not_select_the_underlying_table() {
        let mut demo = super::committed_demo();
        for kind in [
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
        ] {
            demo.handle_event(&bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(kind, bmux_tui::geometry::Point::new(30, 11)),
            ));
        }
        assert_eq!(demo.table.selected(), Some(0));
    }

    #[test]
    fn tab_mouse_selection_updates_navigation_message() {
        let mut demo = super::committed_demo();
        for kind in [
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
        ] {
            assert!(!demo.handle_event(&bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(kind, bmux_tui::geometry::Point::new(9, 0)),
            )));
        }
        assert_eq!(demo.tabs.selected(), Some(1));
        assert_eq!(demo.message, "Tab selected: Tree");
        assert!(!demo.handle_event(&bmux_tui::event::Event::Key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Right),
        )));
        assert_eq!(demo.tabs.selected(), Some(2));
        assert_eq!(demo.message, "Tab selected: Scroll");
    }

    #[test]
    fn list_mouse_selection_updates_navigation_message() {
        let mut demo = super::committed_demo();
        for kind in [
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
        ] {
            assert!(!demo.handle_event(&bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(kind, bmux_tui::geometry::Point::new(2, 1)),
            )));
        }
        assert_eq!(demo.list.selected(), Some(0));
        assert_eq!(demo.message, "List selected: First item");
    }

    #[test]
    fn navigation_renders_lists_menus_and_scroll_content() {
        let rendered = rows(&render_navigation()).join("\n");

        assert!(rendered.contains("Library"));
        assert!(rendered.contains("List"));
        assert!(rendered.contains("Tree"));
        assert!(rendered.contains("Scroll"));
        assert!(rendered.contains("Library"));
        assert!(rendered.contains("Details"));
        assert!(rendered.contains("src"));
        assert!(rendered.contains("Second rich"));
        assert!(rendered.contains("details line"));
        assert!(rendered.contains("> Open"));
        assert!(rendered.contains("Scroll one"));
        assert!(rendered.contains("Delegated line zero"));
        assert!(rendered.contains("Name"));
        assert!(rendered.contains("75%"));
        assert!(rendered.contains("█"));
        assert!(rendered.contains("TextView wraps"));
        assert!(rendered.contains("enter select"));
        assert!(rendered.contains("ready"));
    }

    #[test]
    fn menu_activation_returns_action_id() {
        assert_eq!(
            demonstrate_menu_activation(),
            MenuOutcome::Activated {
                index: 0,
                id: "open".to_string()
            }
        );
    }

    #[test]
    fn delegated_pane_scroll_updates_nested_scroll_view() {
        assert_eq!(demonstrate_delegated_pane_scroll_offset(), 3);
    }

    #[test]
    fn breadcrumb_enter_activates_current_item() {
        assert_eq!(
            demonstrate_breadcrumb_activation(),
            "Breadcrumb activated: library"
        );
    }

    #[test]
    fn text_view_mouse_wheel_updates_scroll() {
        assert_eq!(demonstrate_text_view_scroll(), 1);
    }

    #[test]
    fn table_mouse_selection_updates_navigation_message() {
        let mut demo = super::committed_demo();
        for kind in [
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
        ] {
            assert!(!demo.handle_event(&bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(kind, bmux_tui::geometry::Point::new(2, 11)),
            )));
        }
        assert_eq!(demo.table.selected(), Some(1));
        assert_eq!(demo.message, "Table focus: beta");
    }

    #[test]
    fn table_keyboard_selection_updates_navigation_message() {
        assert_eq!(demonstrate_table_selection(), "Table selected: beta");
    }

    #[test]
    fn tree_click_updates_navigation_message() {
        assert_eq!(demonstrate_tree_selection(), "Tree selected: main.rs");
    }

    #[test]
    fn pane_scroll_is_delegated_from_content() {
        assert_eq!(
            demonstrate_pane_scroll_delegation(),
            PaneOutcome::ScrollDelegated {
                direction: ScrollDirection::Down
            }
        );
    }
}
