use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::buffer::Buffer;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::prelude::Line;
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBar};
use bmux_tui_components::menu::{Menu, MenuItem, MenuOutcome, MenuState};
use bmux_tui_components::pane::{Pane, PaneMousePolicy, PaneOutcome, PanePolicy, PaneState};
use bmux_tui_components::scroll_area::{ScrollArea, ScrollAreaOutcome, ScrollAreaState};
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListItem, SelectableListOutcome, SelectableListState,
};
use bmux_tui_components::status_bar::{MessageBar, StatusBar, StatusSegment, StatusSeverity};
use bmux_tui_components::tab_bar::{TabBar, TabBarOutcome, TabBarState, TabItem};
use bmux_tui_components::tree_view::{TreeView, TreeViewItem, TreeViewOutcome, TreeViewState};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 18;

pub struct NavigationDemo {
    tabs: TabBarState,
    tree: TreeViewState,
    list: SelectableListState,
    menu: MenuState,
    scroll: ScrollAreaState,
    pane_scroll: ScrollAreaState,
    message: String,
}

impl NavigationDemo {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tabs: TabBarState::new(Some(0)),
            tree: {
                let mut state = TreeViewState::new(Some(0));
                state.set_expanded("src", true);
                state
            },
            list: SelectableListState::new(Some(1)),
            menu: MenuState::new(Some(0)),
            scroll: {
                let mut state = ScrollAreaState::new();
                state.set_vertical_offset(1);
                state
            },
            pane_scroll: ScrollAreaState::new(),
            message: "Use arrows/Enter, wheel over scroll pane, q quits".to_string(),
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        render_navigation_with_state(frame, self);
    }

    pub fn handle_event(&mut self, event: &Event) -> bool {
        if matches!(event, Event::Key(stroke) if should_quit(*stroke)) {
            return true;
        }
        let tab_items = tab_items();
        match TabBar::new(&tab_items).handle_event(Rect::new(1, 0, 42, 1), &mut self.tabs, event) {
            TabBarOutcome::Selected(index) => {
                self.message = format!("Tab selected: {}", tab_items[index].label);
                return false;
            }
            TabBarOutcome::Ignored | TabBarOutcome::Redraw => {}
        }

        let tree_items = tree_items();
        match TreeView::new(&tree_items).handle_event(
            Rect::new(48, 1, 22, 6),
            &mut self.tree,
            event,
        ) {
            TreeViewOutcome::Selected { source, .. } => {
                self.message = format!("Tree selected: {}", tree_items[source].label);
                return false;
            }
            TreeViewOutcome::Toggled {
                source, expanded, ..
            } => {
                self.message = format!("Tree {} expanded: {expanded}", tree_items[source].label);
                return false;
            }
            TreeViewOutcome::Focused { source, .. } => {
                self.message = format!("Tree focus: {}", tree_items[source].label);
                return false;
            }
            TreeViewOutcome::Ignored | TreeViewOutcome::Redraw => {}
        }

        let list_items = list_items();
        match SelectableList::new(&list_items).handle_event(
            Rect::new(1, 1, 24, 3),
            &mut self.list,
            event,
        ) {
            SelectableListOutcome::Selected(index) => {
                self.message = format!("List selected: {}", list_items[index].label);
                return false;
            }
            SelectableListOutcome::Focused(index) => {
                self.message = format!("List focus: {}", list_items[index].label);
                return false;
            }
            SelectableListOutcome::Ignored | SelectableListOutcome::Redraw => {}
        }

        let menu_items = menu_items();
        match Menu::new(&menu_items).handle_event(Rect::new(30, 1, 18, 2), &mut self.menu, event) {
            MenuOutcome::Activated { id, .. } => self.message = format!("Menu action: {id}"),
            MenuOutcome::Cancelled => self.message = "Menu cancelled".to_string(),
            MenuOutcome::Ignored | MenuOutcome::Redraw | MenuOutcome::Focused(_) => {}
        }

        let lines = scroll_lines();
        if let ScrollAreaOutcome::Scrolled { vertical_offset } =
            ScrollArea::new(&lines).handle_event(Rect::new(1, 6, 24, 2), &mut self.scroll, event)
        {
            self.message = format!("Scroll offset: {vertical_offset}");
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
            if let ScrollAreaOutcome::Scrolled { vertical_offset } = ScrollArea::new(&pane_lines)
                .handle_event(
                    pane.inner_area(&pane_state),
                    &mut self.pane_scroll,
                    &Event::Mouse(bmux_tui::event::MouseEvent::new(
                        delegated,
                        bmux_tui::geometry::Point::new(
                            pane.inner_area(&pane_state).x,
                            pane.inner_area(&pane_state).y,
                        ),
                    )),
                )
            {
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
    NavigationDemo::new().render(&mut frame);
    buffer
}

fn render_navigation_with_state(frame: &mut Frame<'_>, demo: &NavigationDemo) {
    let tab_items = tab_items();
    TabBar::new(&tab_items).render(Rect::new(1, 0, 42, 1), &demo.tabs, frame);

    let list_items = list_items();
    SelectableList::new(&list_items).render(Rect::new(1, 1, 24, 3), &demo.list, frame);

    let menu_items = menu_items();
    Menu::new(&menu_items).render(Rect::new(30, 1, 18, 2), &demo.menu, frame);

    let tree_items = tree_items();
    TreeView::new(&tree_items).render(Rect::new(48, 1, 22, 6), &demo.tree, frame);

    let lines = scroll_lines();
    ScrollArea::new(&lines).render(Rect::new(1, 6, 24, 2), &demo.scroll, frame);

    let pane = scroll_delegate_pane();
    let pane_state = PaneState::new(scroll_delegate_pane_area());
    pane.render(&pane_state, frame);
    let pane_lines = pane_scroll_lines();
    ScrollArea::new(&pane_lines).render(pane.inner_area(&pane_state), &demo.pane_scroll, frame);
    MessageBar::new(&demo.message).render(Rect::new(1, 15, 60, 1), frame);

    let status_left = [StatusSegment::new("nav").severity(StatusSeverity::Info)];
    let status_right = [StatusSegment::new("ready").severity(StatusSeverity::Success)];
    StatusBar::new()
        .left(&status_left)
        .right(&status_right)
        .render(Rect::new(1, 16, 68, 1), frame);

    let hints = [
        KeyHint::new("↑↓", "move"),
        KeyHint::new("←→", "tabs/tree"),
        KeyHint::new("enter", "select"),
        KeyHint::new("q", "quit"),
    ];
    KeyHintBar::new(&hints).render(Rect::new(1, 17, 68, 1), frame);
}

pub fn demonstrate_menu_activation() -> MenuOutcome {
    let items = menu_items();
    let menu = Menu::new(&items);
    let mut state = MenuState::new(Some(0));
    menu.handle_event(
        Rect::new(0, 0, 16, 2),
        &mut state,
        &Event::Key(KeyStroke::simple(KeyCode::Enter)),
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

pub fn demonstrate_delegated_pane_scroll_offset() -> u16 {
    let mut demo = NavigationDemo::new();
    let _ = demo.handle_event(&Event::Mouse(bmux_tui::event::MouseEvent::new(
        bmux_tui::event::MouseEventKind::ScrollDown,
        bmux_tui::geometry::Point::new(32, 8),
    )));
    demo.pane_scroll.vertical_offset()
}

pub fn demonstrate_tree_selection() -> String {
    let mut demo = NavigationDemo::new();
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

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
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

fn list_items() -> [SelectableListItem; 3] {
    [
        SelectableListItem::new("one", "First item"),
        SelectableListItem::new("two", "Second item"),
        SelectableListItem::new("three", "Third item"),
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
        demonstrate_delegated_pane_scroll_offset, demonstrate_menu_activation,
        demonstrate_pane_scroll_delegation, demonstrate_tree_selection, render_navigation, rows,
    };

    #[test]
    fn navigation_renders_lists_menus_and_scroll_content() {
        let rendered = rows(&render_navigation()).join("\n");

        assert!(rendered.contains("List"));
        assert!(rendered.contains("src"));
        assert!(rendered.contains("Second item"));
        assert!(rendered.contains("> Open"));
        assert!(rendered.contains("Scroll one"));
        assert!(rendered.contains("Delegated line zero"));
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
    fn delegated_pane_scroll_updates_nested_scroll_area() {
        assert_eq!(demonstrate_delegated_pane_scroll_offset(), 3);
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
