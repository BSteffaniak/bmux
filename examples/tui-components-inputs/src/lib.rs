use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::buffer::Buffer;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::Line;
use bmux_tui_components::checkbox::{Checkbox, CheckboxComponent, CheckboxOutcome, CheckboxState};
use bmux_tui_components::form::{Form, FormFieldItem, FormOutcome, FormState};
use bmux_tui_components::radio_group::{
    RadioGroup, RadioGroupComponent, RadioGroupOutcome, RadioGroupState, RadioOption,
};
use bmux_tui_components::select_dropdown::{
    SelectDropdown, SelectDropdownOutcome, SelectDropdownState, SelectOption,
};
use bmux_tui_components::text_input::{
    TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};
use bmux_tui_components::text_input_box::{TextInputBoxComponent, TextInputBoxPolicy};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 16;

const TEXT_CONTENT_AREA: Rect = Rect::new(3, 3, 30, 1);
const CHECKBOX_AREA: Rect = Rect::new(1, 6, 30, 1);
const RADIO_AREA: Rect = Rect::new(1, 8, 18, 2);
const SELECT_AREA: Rect = Rect::new(38, 1, 24, 3);

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
        let mut text = TextInputState::new(TextEditBuffer::from_text("Ada"));
        let policy = TextInputPolicy::chat_composer();
        text.set_content_area(TEXT_CONTENT_AREA, &policy);
        let mut checkbox = CheckboxState::new(true);
        checkbox.set_focused(false);
        Self {
            text_policy: policy,
            text,
            checkbox,
            radio: RadioGroupState::new(Some(1)),
            select: SelectDropdownState::new(Some(1)),
            form: FormState::new(Some(0)),
            message: "Click controls or Tab through them; q quits".to_string(),
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        render_inputs_with_state(
            frame,
            &self.text,
            &self.checkbox,
            &self.radio,
            &self.select,
            self.focused_index(),
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
        if let Some(focus) = focus_for_mouse_event(event) {
            self.set_focus(focus);
        }
        match self.focused_index() {
            0 => self.handle_text_event(event),
            1 => self.handle_checkbox_event(event),
            2 => self.handle_radio_event(event),
            3 => self.handle_select_event(event),
            _ => {}
        }
        false
    }

    fn handle_text_event(&mut self, event: &Event) {
        self.text
            .set_content_area(TEXT_CONTENT_AREA, &self.text_policy);
        if matches!(
            TextInputControl::new(&self.text_policy).handle_event(&mut self.text, event),
            TextInputOutcome::Edited | TextInputOutcome::Redraw
        ) {
            self.message = format!("Text: {}", self.text.buffer().text());
        }
    }

    fn handle_checkbox_event(&mut self, event: &Event) {
        match Checkbox::new("Subscribe to updates").handle_event(
            CHECKBOX_AREA,
            &mut self.checkbox,
            event,
        ) {
            CheckboxOutcome::Toggled(checked) => self.message = format!("Checkbox: {checked}"),
            CheckboxOutcome::Ignored | CheckboxOutcome::Redraw => {}
        }
    }

    fn handle_radio_event(&mut self, event: &Event) {
        let radios = radio_options();
        if let RadioGroupOutcome::Selected(index) =
            RadioGroup::new(&radios).handle_event(RADIO_AREA, &mut self.radio, event)
        {
            self.message = format!("Radio selected: {}", radios[index].label);
        }
    }

    fn handle_select_event(&mut self, event: &Event) {
        let options = select_options();
        match SelectDropdown::new(&options).handle_event(SELECT_AREA, &mut self.select, event) {
            SelectDropdownOutcome::Selected(index) => {
                self.message = format!("Select: {}", options[index].label);
            }
            SelectDropdownOutcome::Opened => self.message = "Select opened".to_string(),
            SelectDropdownOutcome::Closed => self.message = "Select closed".to_string(),
            SelectDropdownOutcome::Ignored
            | SelectDropdownOutcome::Redraw
            | SelectDropdownOutcome::Focused(_) => {}
        }
    }

    fn advance_focus(&mut self) {
        let next = (self.focused_index() + 1) % 4;
        self.set_focus(next);
    }

    fn set_focus(&mut self, focus: usize) {
        self.checkbox.set_focused(focus == 1);
        self.radio
            .set_focused((focus == 2).then_some(self.radio.selected().unwrap_or(0)));
        self.select.interaction.focused = focus == 3;
        if focus != 3 {
            self.select.set_open(false);
        }
        self.form.set_focused(Some(focus));
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
    focused: usize,
    message: &str,
) {
    let text_state = std::cell::RefCell::new(text.clone());
    let component =
        TextInputBoxComponent::new("inputs.name", TextInputPolicy::chat_composer(), &text_state)
            .label("Name")
            .required(true)
            .help("Click the field, then type")
            .policy(TextInputBoxPolicy::labeled_field().focused(focused == 0));
    let text_area = Rect::new(1, 1, 34, 5);
    let layout = component.layout(Constraints::tight(text_area.size()), &mut LayoutCx::new());
    PaintCx::new(frame).with_child(
        i32::from(text_area.x),
        i64::from(text_area.y),
        LocalRect::new(0, 0, text_area.width, text_area.height),
        |cx| component.paint(&layout, cx),
    );

    let checkbox_state = std::cell::Cell::new(*checkbox);
    let component =
        CheckboxComponent::new("inputs.subscribe", "Subscribe to updates", &checkbox_state);
    let layout = component.layout(
        Constraints::tight(CHECKBOX_AREA.size()),
        &mut LayoutCx::new(),
    );
    PaintCx::new(frame).with_child(
        i32::from(CHECKBOX_AREA.x),
        i64::from(CHECKBOX_AREA.y),
        LocalRect::new(0, 0, CHECKBOX_AREA.width, CHECKBOX_AREA.height),
        |cx| component.paint(&layout, cx),
    );

    let radios = radio_options();
    let radio_state = std::cell::Cell::new(*radio);
    let component = RadioGroupComponent::new("inputs.frequency", &radios, &radio_state);
    let layout = component.layout(Constraints::tight(RADIO_AREA.size()), &mut LayoutCx::new());
    PaintCx::new(frame).with_child(
        i32::from(RADIO_AREA.x),
        i64::from(RADIO_AREA.y),
        LocalRect::new(0, 0, RADIO_AREA.width, RADIO_AREA.height),
        |cx| component.paint(&layout, cx),
    );

    let options = select_options();
    SelectDropdown::new(&options).render(SELECT_AREA, select, frame);

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

pub fn demonstrate_text_input_uppercase_edit() -> String {
    let policy = TextInputPolicy::chat_composer();
    let control = TextInputControl::new(&policy);
    let mut state = TextInputState::new(TextEditBuffer::from_text("Ada"));
    let _ = control.handle_event(
        &mut state,
        &Event::Key(KeyStroke::with_modifiers(
            KeyCode::Char('b'),
            bmux_keyboard::Modifiers {
                shift: true,
                ..bmux_keyboard::Modifiers::NONE
            },
        )),
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

pub fn demonstrate_form_validation_errors() -> FormOutcome {
    let fields = [
        FormFieldItem::new("name").required(true),
        FormFieldItem::new("email").required(true),
    ];
    let values = [Some("Ada"), Some("   ")];
    let form = Form::new(&fields, &values);
    let mut state = FormState::new(Some(0));
    form.handle_event(&mut state, &Event::Key(KeyStroke::simple(KeyCode::Enter)))
}

pub fn demonstrate_click_checkbox_toggles_from_text_focus() -> bool {
    let mut demo = InputsDemo::new();
    let position = Point::new(CHECKBOX_AREA.x, CHECKBOX_AREA.y);
    let _ = demo.handle_event(&mouse_down(position));
    let _ = demo.handle_event(&mouse_up(position));
    demo.checkbox.checked()
}

pub fn demonstrate_click_radio_selects_and_focuses() -> Option<usize> {
    let mut demo = InputsDemo::new();
    let position = Point::new(RADIO_AREA.x, RADIO_AREA.y);
    let _ = demo.handle_event(&mouse_down(position));
    let _ = demo.handle_event(&mouse_up(position));
    demo.radio.selected()
}

pub fn demonstrate_click_text_focuses() -> usize {
    let mut demo = InputsDemo::new();
    demo.set_focus(1);
    let _ = demo.handle_event(&mouse_down(Point::new(
        TEXT_CONTENT_AREA.x,
        TEXT_CONTENT_AREA.y,
    )));
    demo.focused_index()
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

fn focus_for_mouse_event(event: &Event) -> Option<usize> {
    let Event::Mouse(mouse) = event else {
        return None;
    };
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return None;
    }
    control_at(mouse.position)
}

fn control_at(position: Point) -> Option<usize> {
    if TEXT_CONTENT_AREA.contains(position) {
        Some(0)
    } else if CHECKBOX_AREA.contains(position) {
        Some(1)
    } else if RADIO_AREA.contains(position) {
        Some(2)
    } else if SELECT_AREA.contains(position) {
        Some(3)
    } else {
        None
    }
}

fn mouse_down(position: Point) -> Event {
    Event::Mouse(MouseEvent::new(
        MouseEventKind::Down(MouseButton::Left),
        position,
    ))
}

fn mouse_up(position: Point) -> Event {
    Event::Mouse(MouseEvent::new(
        MouseEventKind::Up(MouseButton::Left),
        position,
    ))
}

fn should_quit(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Escape || stroke.key == KeyCode::Char('q')
}

#[cfg(test)]
mod tests {
    use bmux_tui_components::form::FormOutcome;

    use super::{
        demonstrate_click_checkbox_toggles_from_text_focus,
        demonstrate_click_radio_selects_and_focuses, demonstrate_click_text_focuses,
        demonstrate_form_submit, demonstrate_form_validation_errors, demonstrate_text_input_edit,
        demonstrate_text_input_uppercase_edit, render_inputs, rows,
    };

    #[test]
    fn inputs_render_form_controls() {
        let rendered = rows(&render_inputs()).join("\n");

        assert!(rendered.contains("Name *"));
        assert!(rendered.contains("Ada"));
        assert!(rendered.contains("[x] Subscribe"));
        assert!(rendered.contains("Published"));
    }

    #[test]
    fn text_input_policy_edits_buffer() {
        assert_eq!(demonstrate_text_input_edit(), "Ada!");
        assert_eq!(demonstrate_text_input_uppercase_edit(), "AdaB");
    }

    #[test]
    fn form_submit_validates_values() {
        assert_eq!(demonstrate_form_submit(), FormOutcome::Submitted);
        assert_eq!(
            demonstrate_form_validation_errors(),
            FormOutcome::ValidationFailed(vec![1])
        );
    }

    #[test]
    fn mouse_click_changes_focus_and_activates_controls() {
        assert!(!demonstrate_click_checkbox_toggles_from_text_focus());
        assert_eq!(demonstrate_click_radio_selects_and_focuses(), Some(0));
        assert_eq!(demonstrate_click_text_focuses(), 0);
    }
}
