use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::buffer::Buffer;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::Line;
use bmux_tui_components::checkbox::{Checkbox, CheckboxState};
use bmux_tui_components::form::{Form, FormFieldItem, FormOutcome, FormState};
use bmux_tui_components::form_field::FormField;
use bmux_tui_components::radio_group::{RadioGroup, RadioGroupState, RadioOption};
use bmux_tui_components::select_dropdown::{SelectDropdown, SelectDropdownState, SelectOption};
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 16;

pub fn render_inputs() -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut frame = Frame::new(&mut buffer);

    let name_area = FormField::new("Name")
        .required(true)
        .help("TextInputState owns editable text")
        .render(Rect::new(1, 1, 34, 4), &mut frame);
    frame.write_line(name_area, &Line::from("Ada Lovelace"));

    let checkbox = Checkbox::new("Subscribe to updates");
    checkbox.render(
        Rect::new(1, 6, 30, 1),
        &CheckboxState::new(true),
        &mut frame,
    );

    let radios = [
        RadioOption::new("daily", "Daily"),
        RadioOption::new("weekly", "Weekly"),
    ];
    RadioGroup::new(&radios).render(
        Rect::new(1, 8, 18, 2),
        &RadioGroupState::new(Some(1)),
        &mut frame,
    );

    let options = [
        SelectOption::new("draft", "Draft"),
        SelectOption::new("published", "Published"),
    ];
    SelectDropdown::new(&options).render(
        Rect::new(38, 1, 24, 3),
        &SelectDropdownState::new(Some(1)),
        &mut frame,
    );

    frame.write_line(Rect::new(38, 6, 30, 1), &Line::from("Form submit: valid"));
    buffer
}

pub fn demonstrate_text_input_edit() -> String {
    let policy = TextInputPolicy::chat_composer();
    let control = TextInputControl::new(&policy);
    let mut state = TextInputState::new(TextEditBuffer::from_text("Ada"));
    let _ = control.handle_event(
        &mut state,
        &Event::Key(KeyStroke::simple(KeyCode::Char('!'))),
    );
    state.buffer().text().to_string()
}

pub fn demonstrate_form_submit() -> FormOutcome {
    let fields = [
        FormFieldItem::new("name").required(true),
        FormFieldItem::new("email").required(true),
    ];
    let values = [Some("Ada"), Some("ada@example.test")];
    let form = Form::new(&fields, &values);
    let mut state = FormState::new(Some(0));
    form.handle_event(&mut state, &Event::Key(KeyStroke::simple(KeyCode::Enter)))
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

#[cfg(test)]
mod tests {
    use bmux_tui_components::form::FormOutcome;

    use super::{demonstrate_form_submit, demonstrate_text_input_edit, render_inputs, rows};

    #[test]
    fn inputs_render_form_controls() {
        let rendered = rows(&render_inputs()).join("\n");

        assert!(rendered.contains("Name *"));
        assert!(rendered.contains("[x] Subscribe"));
        assert!(rendered.contains("Published"));
    }

    #[test]
    fn text_input_policy_edits_buffer() {
        assert_eq!(demonstrate_text_input_edit(), "Ada!");
    }

    #[test]
    fn form_submit_validates_values() {
        assert_eq!(demonstrate_form_submit(), FormOutcome::Submitted);
    }
}
