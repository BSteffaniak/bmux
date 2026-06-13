use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::buffer::Buffer;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::Line;
use bmux_tui_components::checkbox::{Checkbox, CheckboxOutcome, CheckboxState};
use bmux_tui_components::form::{Form, FormFieldItem, FormOutcome, FormState};
use bmux_tui_components::form_field::FormField;
use bmux_tui_components::radio_group::{
    RadioGroup, RadioGroupOutcome, RadioGroupState, RadioOption,
};
use bmux_tui_components::select_dropdown::{
    SelectDropdown, SelectDropdownOutcome, SelectDropdownState, SelectOption,
};
use bmux_tui_components::text_input::{
    TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 16;

pub struct InputsDemo {
    text_policy: TextInputPolicy,
    text: TextInputState,
    checkbox: CheckboxState,
    radio: RadioGroupState,
    select: SelectDropdownState,
    form: FormState,
    message: String,
}

impl InputsDemo {
    #[must_use]
    pub fn new() -> Self {
        let mut checkbox = CheckboxState::new(true);
        checkbox.set_focused(true);
        Self {
            text_policy: TextInputPolicy::chat_composer(),
            text: TextInputState::new(TextEditBuffer::from_text("Ada")),
            checkbox,
            radio: RadioGroupState::new(Some(1)),
            select: SelectDropdownState::new(Some(1)),
            form: FormState::new(Some(0)),
            message: "Tab through controls; Enter/Space activates; q quits".to_string(),
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        render_inputs_with_state(
            frame,
            &self.text,
            &self.checkbox,
            &self.radio,
            &self.select,
            &self.message,
        );
    }

    pub fn handle_event(&mut self, event: &Event) -> bool {
        if matches!(event, Event::Key(stroke) if should_quit(*stroke)) {
            return true;
        }
        if matches!(event, Event::Key(stroke) if stroke.key == KeyCode::Tab && stroke.modifiers.is_empty())
        {
            self.advance_focus();
            return false;
        }
        match self.focused_index() {
            0 => {
                if matches!(
                    TextInputControl::new(&self.text_policy).handle_event(&mut self.text, event),
                    TextInputOutcome::Edited | TextInputOutcome::Redraw
                ) {
                    self.message = format!("Text: {}", self.text.buffer().text());
                }
            }
            1 => match Checkbox::new("Subscribe to updates").handle_event(
                Rect::new(1, 6, 30, 1),
                &mut self.checkbox,
                event,
            ) {
                CheckboxOutcome::Toggled(checked) => self.message = format!("Checkbox: {checked}"),
                CheckboxOutcome::Ignored | CheckboxOutcome::Redraw => {}
            },
            2 => {
                let radios = radio_options();
                if let RadioGroupOutcome::Selected(index) = RadioGroup::new(&radios).handle_event(
                    Rect::new(1, 8, 18, 2),
                    &mut self.radio,
                    event,
                ) {
                    self.message = format!("Radio selected: {}", radios[index].label);
                }
            }
            3 => {
                let options = select_options();
                if let SelectDropdownOutcome::Selected(index) = SelectDropdown::new(&options)
                    .handle_event(Rect::new(38, 1, 24, 3), &mut self.select, event)
                {
                    self.message = format!("Select: {}", options[index].label);
                }
            }
            _ => {}
        }
        false
    }

    fn advance_focus(&mut self) {
        let next = (self.focused_index() + 1) % 4;
        self.checkbox.set_focused(next == 1);
        self.radio
            .set_focused((next == 2).then_some(self.radio.selected().unwrap_or(0)));
        self.select.interaction.focused = next == 3;
        self.form.set_focused(Some(next));
    }

    fn focused_index(&self) -> usize {
        self.form.focused().unwrap_or(0)
    }
}

impl Default for InputsDemo {
    fn default() -> Self {
        Self::new()
    }
}

pub fn render_inputs() -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut frame = Frame::new(&mut buffer);
    InputsDemo::new().render(&mut frame);
    buffer
}

fn render_inputs_with_state(
    frame: &mut Frame<'_>,
    text: &TextInputState,
    checkbox: &CheckboxState,
    radio: &RadioGroupState,
    select: &SelectDropdownState,
    message: &str,
) {
    let name_area = FormField::new("Name")
        .required(true)
        .help("TextInputState owns editable text")
        .render(Rect::new(1, 1, 34, 4), frame);
    frame.write_line(name_area, &Line::from(text.buffer().text()));

    Checkbox::new("Subscribe to updates").render(Rect::new(1, 6, 30, 1), checkbox, frame);

    let radios = radio_options();
    RadioGroup::new(&radios).render(Rect::new(1, 8, 18, 2), radio, frame);

    let options = select_options();
    SelectDropdown::new(&options).render(Rect::new(38, 1, 24, 3), select, frame);

    frame.write_line(Rect::new(38, 6, 32, 1), &Line::from(message));
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

fn radio_options() -> [RadioOption; 2] {
    [
        RadioOption::new("daily", "Daily"),
        RadioOption::new("weekly", "Weekly"),
    ]
}

fn select_options() -> [SelectOption; 2] {
    [
        SelectOption::new("draft", "Draft"),
        SelectOption::new("published", "Published"),
    ]
}

fn should_quit(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Escape || stroke.key == KeyCode::Char('q')
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
