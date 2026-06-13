use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::Line;
use bmux_tui::style::Color;
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowState};
use bmux_tui_components::button::{Button, ButtonState};
use bmux_tui_components::dialog::{Dialog, DialogState};
use bmux_tui_components::form_field::FormField;
use bmux_tui_components::labeled_details::{DetailItem, LabeledDetails};
use bmux_tui_components::modal_frame::{ModalFrame, ModalSizing, ModalTheme};
use bmux_tui_components::pane::{Pane, PaneState};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 24;

pub fn render_gallery() -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut frame = Frame::new(&mut buffer);
    let theme = ModalTheme::dark(Color::Cyan);

    render_buttons(&mut frame);
    render_details(&mut frame);
    render_field(&mut frame);
    render_pane(&mut frame);
    render_modal(&mut frame, theme);
    render_dialog(&mut frame, theme);

    buffer
}

fn render_buttons(frame: &mut Frame<'_>) {
    let mut focused = ButtonState::new();
    focused.interaction.focused = true;
    Button::new("Save").render(Rect::new(1, 1, 10, 1), &focused, frame);

    let mut disabled = ButtonState::new();
    disabled.interaction.disabled = true;
    Button::new("Disabled").render(Rect::new(13, 1, 14, 1), &disabled, frame);

    let actions = [
        ActionButton::new("accept", "Accept"),
        ActionButton::new("cancel", "Cancel"),
    ];
    let mut state = ActionRowState::new();
    state.set_focused(Some(0));
    ActionRow::new(&actions).render_state(Rect::new(1, 3, 30, 1), &state, frame);
}

fn render_details(frame: &mut Frame<'_>) {
    let items = [
        DetailItem::new("Component", "LabeledDetails"),
        DetailItem::new("Purpose", "Wrapped labels and values"),
    ];
    LabeledDetails::new(&items).render(Rect::new(1, 5, 34, 5), frame);
}

fn render_field(frame: &mut Frame<'_>) {
    let field = FormField::new("Project")
        .required(true)
        .help("Shown with a required marker")
        .error("Example error text");
    let control_area = field.render(Rect::new(38, 1, 30, 5), frame);
    frame.write_line(control_area, &Line::from("bmux"));
}

fn render_pane(frame: &mut Frame<'_>) {
    let pane = Pane::new().title("Pane").padding(Insets::all(1));
    let state = PaneState::new(Rect::new(1, 11, 28, 8));
    pane.render(&state, frame);
    frame.write_line(pane.inner_area(&state), &Line::from("Pane content area"));
}

fn render_modal(frame: &mut Frame<'_>, theme: ModalTheme) {
    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(24, 7), Size::new(24, 7), Insets::all(0)),
        theme,
    )
    .title("Modal");
    modal.render(Rect::new(34, 8, 34, 10), frame);
    modal.render_line(
        modal.content_area(Rect::new(34, 8, 34, 10)),
        &Line::from("Opaque modal frame"),
        frame,
    );
}

fn render_dialog(frame: &mut Frame<'_>, theme: ModalTheme) {
    let body = [Line::from("Dialog body with actions")];
    let actions = [ActionButton::new("ok", "OK")];
    let mut state = DialogState::new();
    state.actions.set_focused(Some(0));
    Dialog::new(&body, &actions, theme)
        .title("Dialog")
        .sizing(ModalSizing::new(
            Size::new(30, 7),
            Size::new(30, 7),
            Insets::all(0),
        ))
        .render(Rect::new(35, 15, 34, 9), &state, frame);
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{render_gallery, rows};

    #[test]
    fn gallery_renders_representative_components() {
        let rendered = rows(&render_gallery()).join("\n");

        assert!(rendered.contains("[ Save ]"));
        assert!(rendered.contains("LabeledDetails"));
        assert!(rendered.contains("Pane content area"));
        assert!(rendered.contains("Dialog body with actions"));
    }
}
