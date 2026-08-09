use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Size};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::style::{Color, Style};
use bmux_tui_components::button::{Button, ButtonState};
use bmux_tui_components::modal_frame::{ModalFrame, ModalSizing};
use bmux_tui_components::picker_frame::{PickerFrame, PickerFramePolicy};
use bmux_tui_components::theme::ComponentTheme;

fn rendered(theme: ComponentTheme) -> Buffer {
    let area = Rect::new(0, 0, 40, 12);
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    frame.fill(area, " ", theme.canvas);

    let mut focused = ButtonState::new();
    focused.set_focused(true);
    Button::new("Action").styles(theme.button_styles()).render(
        Rect::new(1, 1, 12, 1),
        &focused,
        &mut frame,
    );

    PickerFrame::new()
        .title("Picker")
        .policy(PickerFramePolicy::palette().max_size(Size::new(18, 6)))
        .styles(theme.picker_frame_styles())
        .render(Rect::new(1, 3, 20, 7), &mut frame);

    ModalFrame::new(
        ModalSizing::fixed(Size::new(14, 5), Insets::all(0)),
        theme.modal_theme(),
    )
    .title("Modal")
    .render(Rect::new(24, 3, 15, 6), &mut frame);
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
