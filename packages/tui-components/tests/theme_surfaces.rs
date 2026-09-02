use std::cell::Cell;

use bmux_tui::buffer::Buffer;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextContent;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Size};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::{Color, Style};
use bmux_tui_components::button::{ButtonComponent, ButtonState};
use bmux_tui_components::modal_frame::{ModalFrame, ModalFrameComponent, ModalSizing};
use bmux_tui_components::picker_frame::{PickerFrame, PickerFrameComponent, PickerFramePolicy};
use bmux_tui_components::theme::ComponentTheme;

fn rendered(theme: ComponentTheme) -> Buffer {
    let area = Rect::new(0, 0, 40, 12);
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    frame.fill(area, " ", theme.canvas);

    let mut focused = ButtonState::new();
    focused.set_focused(true);
    let focused = Cell::new(focused);
    let button =
        ButtonComponent::new("theme.action", "Action", &focused).styles(theme.button_styles());
    let button_layout = button.layout(Constraints::for_width(12), &mut LayoutCx::new());
    PaintCx::new(&mut frame).with_child(1, 1, LocalRect::new(0, 0, 12, 1), |cx| {
        button.paint(&button_layout, cx)
    });

    let picker = PickerFrameComponent::new(
        "theme.picker",
        PickerFrame::new()
            .title("Picker")
            .policy(PickerFramePolicy::palette().max_size(Size::new(18, 6)))
            .styles(theme.picker_frame_styles()),
        TextContent::new("").id("theme.picker.list"),
    );
    let picker_area = Rect::new(1, 3, 20, 7);
    let layout = picker.layout(Constraints::tight(picker_area.size()), &mut LayoutCx::new());
    PaintCx::new(&mut frame).with_child(
        i32::from(picker_area.x),
        i64::from(picker_area.y),
        LocalRect::new(0, 0, picker_area.width, picker_area.height),
        |cx| picker.paint(&layout, cx),
    );

    let modal_area = Rect::new(24, 3, 15, 6);
    let modal = ModalFrameComponent::new(
        "theme.modal",
        ModalFrame::new(
            ModalSizing::fixed(Size::new(14, 5), Insets::all(0)),
            theme.modal_theme(),
        )
        .title("Modal"),
        TextContent::new(""),
    );
    let layout = modal.layout(Constraints::tight(modal_area.size()), &mut LayoutCx::new());
    PaintCx::new(&mut frame).with_child(
        i32::from(modal_area.x),
        i64::from(modal_area.y),
        LocalRect::new(0, 0, modal_area.width, modal_area.height),
        |cx| modal.paint(&layout, cx),
    );
    buffer
}

#[test]
fn golden_dark_light_and_terminal_native_surface_hierarchy() {
    for (name, theme) in [
        ("dark", ComponentTheme::opaque_dark()),
        ("light", ComponentTheme::opaque_light()),
        ("terminal", ComponentTheme::terminal_default()),
    ] {
        let buffer = rendered(theme);
        assert_eq!(
            buffer.get(Point::new(0, 0)).expect("canvas").style.bg,
            theme.canvas.bg,
            "{name} canvas"
        );
        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.style.bg == theme.surfaces.raised.bg),
            "{name} raised picker"
        );
        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.style.bg == theme.surfaces.overlay.bg),
            "{name} overlay modal"
        );
    }
}

#[test]
fn explicit_role_background_wins_over_surface_background() {
    let theme = ComponentTheme {
        selected: Style::new().fg(Color::White).bg(Color::Magenta),
        ..ComponentTheme::opaque_dark()
    };
    let resolved = theme.for_surface(bmux_tui_components::theme::ComponentSurfaceDepth::Raised);
    assert_eq!(resolved.selected.bg, Some(Color::Magenta));
    assert_eq!(resolved.text.bg, theme.surfaces.raised.bg);
}
