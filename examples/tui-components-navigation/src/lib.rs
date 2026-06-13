use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::buffer::Buffer;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::prelude::Line;
use bmux_tui_components::menu::{Menu, MenuItem, MenuOutcome, MenuState};
use bmux_tui_components::pane::{Pane, PaneMousePolicy, PaneOutcome, PanePolicy, PaneState};
use bmux_tui_components::scroll_area::{ScrollArea, ScrollAreaState};
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListItem, SelectableListState,
};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 18;

pub fn render_navigation() -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut frame = Frame::new(&mut buffer);

    let list_items = [
        SelectableListItem::new("one", "First item"),
        SelectableListItem::new("two", "Second item"),
        SelectableListItem::new("three", "Third item"),
    ];
    SelectableList::new(&list_items).render(
        Rect::new(1, 1, 24, 3),
        &SelectableListState::new(Some(1)),
        &mut frame,
    );

    let menu_items = [
        MenuItem::new("open", "Open"),
        MenuItem::new("close", "Close"),
    ];
    Menu::new(&menu_items).render(
        Rect::new(30, 1, 18, 2),
        &MenuState::new(Some(0)),
        &mut frame,
    );

    let lines = [
        Line::from("Scroll zero"),
        Line::from("Scroll one"),
        Line::from("Scroll two"),
        Line::from("Scroll three"),
    ];
    let mut scroll_state = ScrollAreaState::new();
    scroll_state.set_vertical_offset(1);
    ScrollArea::new(&lines).render(Rect::new(1, 6, 24, 2), &scroll_state, &mut frame);

    let pane = Pane::new()
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
        });
    let pane_state = PaneState::new(Rect::new(30, 6, 28, 7));
    pane.render(&pane_state, &mut frame);
    frame.write_line(
        pane.inner_area(&pane_state),
        &Line::from("Wheel input delegates"),
    );

    buffer
}

pub fn demonstrate_menu_activation() -> MenuOutcome {
    let items = [
        MenuItem::new("open", "Open"),
        MenuItem::new("close", "Close"),
    ];
    let menu = Menu::new(&items);
    let mut state = MenuState::new(Some(0));
    menu.handle_event(
        Rect::new(0, 0, 16, 2),
        &mut state,
        &Event::Key(KeyStroke::simple(KeyCode::Enter)),
    )
}

pub fn demonstrate_pane_scroll_delegation() -> PaneOutcome {
    let pane = Pane::new().padding(Insets::all(1)).policy(PanePolicy {
        mouse: PaneMousePolicy {
            enabled: true,
            click_to_focus: false,
            title_bar_drag: false,
            scroll_wheel: true,
            resize_handles: bmux_tui_components::pane::ResizeHandles::NONE,
        },
        bounds: Default::default(),
    });
    let mut state = PaneState::new(Rect::new(0, 0, 10, 5));
    pane.handle_event(
        &mut state,
        &bmux_tui::event::Event::Mouse(bmux_tui::event::MouseEvent::new(
            bmux_tui::event::MouseEventKind::ScrollDown,
            bmux_tui::geometry::Point::new(2, 2),
        )),
    )
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

#[cfg(test)]
mod tests {
    use bmux_tui_components::menu::MenuOutcome;
    use bmux_tui_components::pane::{PaneOutcome, ScrollDirection};

    use super::{
        demonstrate_menu_activation, demonstrate_pane_scroll_delegation, render_navigation, rows,
    };

    #[test]
    fn navigation_renders_lists_menus_and_scroll_content() {
        let rendered = rows(&render_navigation()).join("\n");

        assert!(rendered.contains("Second item"));
        assert!(rendered.contains("> Open"));
        assert!(rendered.contains("Scroll one"));
        assert!(rendered.contains("Wheel input delegates"));
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
    fn pane_scroll_is_delegated_from_content() {
        assert_eq!(
            demonstrate_pane_scroll_delegation(),
            PaneOutcome::ScrollDelegated {
                direction: ScrollDirection::Down
            }
        );
    }
}
