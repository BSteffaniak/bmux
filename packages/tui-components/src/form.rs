//! Generic form state, validation, and focus traversal primitives.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;

use crate::common::{ComponentMousePolicy, InteractionState};

/// One field in a generic form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormFieldItem {
    /// Stable field id chosen by the caller.
    pub id: String,
    /// Whether the field must be non-empty for validation to pass.
    pub required: bool,
    /// Whether the field is skipped for focus and validation.
    pub disabled: bool,
}

impl FormFieldItem {
    /// Create an enabled optional form field item.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            required: false,
            disabled: false,
        }
    }

    /// Return this item marked as required or optional.
    #[must_use]
    pub const fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Return this item marked as disabled or enabled.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Configurable form behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct FormPolicy {
    /// Mouse behavior for click-to-focus over caller-provided field areas.
    pub mouse: ComponentMousePolicy,
    /// Whether Tab moves focus to the next enabled field.
    pub tab_moves_focus: bool,
    /// Whether `BackTab` moves focus to the previous enabled field.
    pub backtab_moves_focus: bool,
    /// Whether Enter submits the form.
    pub enter_submits: bool,
    /// Whether Escape cancels the form.
    pub escape_cancels: bool,
    /// Whether focus wraps at the first/last field.
    pub wrap_focus: bool,
}

impl FormPolicy {
    /// Common interactive form behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            tab_moves_focus: true,
            backtab_moves_focus: true,
            enter_submits: true,
            escape_cancels: true,
            wrap_focus: true,
        }
    }
}

impl Default for FormPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Runtime generic form state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormState {
    /// Common form interaction flags.
    pub interaction: InteractionState,
    focused: Option<usize>,
}

impl FormState {
    /// Create enabled form state.
    #[must_use]
    pub const fn new(focused: Option<usize>) -> Self {
        Self {
            interaction: InteractionState::new(),
            focused,
        }
    }

    /// Return focused field index.
    #[must_use]
    pub const fn focused(self) -> Option<usize> {
        self.focused
    }

    /// Set focused field index.
    pub const fn set_focused(&mut self, focused: Option<usize>) {
        self.focused = focused;
        self.interaction.focused = focused.is_some();
    }

    /// Set disabled state for the whole form.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
    }
}

/// Outcome from generic form input handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormOutcome {
    /// Event was not handled.
    Ignored,
    /// Visual state changed without semantic action.
    Redraw,
    /// Focus moved to the contained field index.
    Focused(usize),
    /// Form was submitted successfully.
    Submitted,
    /// Form submission failed; contained indices are invalid required fields.
    ValidationFailed(Vec<usize>),
    /// Form was cancelled.
    Cancelled,
}

/// Generic form controller over caller-owned field definitions and values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Form<'a> {
    fields: &'a [FormFieldItem],
    values: &'a [Option<&'a str>],
    field_areas: &'a [Rect],
    policy: FormPolicy,
}

impl<'a> Form<'a> {
    /// Create a form controller over field definitions and current values.
    #[must_use]
    pub const fn new(fields: &'a [FormFieldItem], values: &'a [Option<&'a str>]) -> Self {
        Self {
            fields,
            values,
            field_areas: &[],
            policy: FormPolicy {
                mouse: ComponentMousePolicy {
                    enabled: true,
                    hover: true,
                    click: true,
                },
                tab_moves_focus: true,
                backtab_moves_focus: true,
                enter_submits: true,
                escape_cancels: true,
                wrap_focus: true,
            },
        }
    }

    /// Set field areas used for mouse click-to-focus.
    #[must_use]
    pub const fn field_areas(mut self, field_areas: &'a [Rect]) -> Self {
        self.field_areas = field_areas;
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: FormPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Return invalid required field indices.
    #[must_use]
    pub fn validate(&self) -> Vec<usize> {
        self.fields
            .iter()
            .enumerate()
            .filter_map(|(index, field)| {
                let value = self.values.get(index).copied().flatten().unwrap_or("");
                (field.required && !field.disabled && value.trim().is_empty()).then_some(index)
            })
            .collect()
    }

    /// Return invalid required field ids.
    #[must_use]
    pub fn validate_ids(&self) -> Vec<&'a str> {
        self.validate()
            .into_iter()
            .filter_map(|index| self.fields.get(index).map(|field| field.id.as_str()))
            .collect()
    }

    /// Handle one input event.
    pub fn handle_event(&self, state: &mut FormState, event: &Event) -> FormOutcome {
        self.normalize_state(state);
        if state.interaction.disabled {
            return FormOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) => self.handle_key(state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(state, *mouse),
            Event::Resize(_) | Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                FormOutcome::Ignored
            }
        }
    }

    fn handle_key(&self, state: &mut FormState, stroke: KeyStroke) -> FormOutcome {
        match stroke.key {
            KeyCode::Tab if self.policy.tab_moves_focus && stroke.modifiers.is_empty() => {
                self.move_focus(state, Direction::Next)
            }
            KeyCode::Tab if self.policy.backtab_moves_focus && stroke.modifiers.shift => {
                self.move_focus(state, Direction::Previous)
            }
            KeyCode::Enter if self.policy.enter_submits => self.submit(),
            KeyCode::Escape if self.policy.escape_cancels => FormOutcome::Cancelled,
            KeyCode::Char(_)
            | KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Escape
            | KeyCode::Space
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Insert
            | KeyCode::F(_) => FormOutcome::Ignored,
        }
    }

    fn handle_mouse(&self, state: &mut FormState, mouse: MouseEvent) -> FormOutcome {
        if !self.policy.mouse.enabled || !self.policy.mouse.click {
            return FormOutcome::Ignored;
        }
        let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
            return FormOutcome::Ignored;
        };
        let Some(index) = self.hit_field(mouse) else {
            return FormOutcome::Ignored;
        };
        state.set_focused(Some(index));
        FormOutcome::Focused(index)
    }

    fn submit(&self) -> FormOutcome {
        let invalid = self.validate();
        if invalid.is_empty() {
            FormOutcome::Submitted
        } else {
            FormOutcome::ValidationFailed(invalid)
        }
    }

    fn move_focus(&self, state: &mut FormState, direction: Direction) -> FormOutcome {
        let Some(current) = state.focused.or_else(|| self.first_enabled()) else {
            return FormOutcome::Ignored;
        };
        let Some(next) = self.next_enabled(current, direction) else {
            return FormOutcome::Ignored;
        };
        if state.focused == Some(next) {
            FormOutcome::Ignored
        } else {
            state.set_focused(Some(next));
            FormOutcome::Focused(next)
        }
    }

    fn next_enabled(&self, current: usize, direction: Direction) -> Option<usize> {
        if self.fields.is_empty() {
            return None;
        }
        let mut index = current.min(self.fields.len().saturating_sub(1));
        for _ in 0..self.fields.len() {
            index = match direction {
                Direction::Previous if index == 0 && self.policy.wrap_focus => {
                    self.fields.len().saturating_sub(1)
                }
                Direction::Previous if index == 0 => return Some(current),
                Direction::Previous => index.saturating_sub(1),
                Direction::Next if index + 1 >= self.fields.len() && self.policy.wrap_focus => 0,
                Direction::Next if index + 1 >= self.fields.len() => return Some(current),
                Direction::Next => index + 1,
            };
            if self.is_enabled_field(index) {
                return Some(index);
            }
        }
        Some(current)
    }

    fn first_enabled(&self) -> Option<usize> {
        self.fields.iter().position(|field| !field.disabled)
    }

    fn hit_field(&self, mouse: MouseEvent) -> Option<usize> {
        self.field_areas
            .iter()
            .enumerate()
            .find_map(|(index, area)| {
                (area.contains(mouse.position) && self.is_enabled_field(index)).then_some(index)
            })
    }

    fn normalize_state(&self, state: &mut FormState) {
        if state
            .focused
            .is_some_and(|index| !self.is_enabled_field(index))
        {
            state.set_focused(self.first_enabled());
        }
    }

    fn is_enabled_field(&self, index: usize) -> bool {
        self.fields.get(index).is_some_and(|field| !field.disabled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Previous,
    Next,
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::{Point, Rect};

    use super::{Form, FormFieldItem, FormOutcome, FormState};

    #[test]
    fn tab_moves_focus_to_next_enabled_field() {
        let fields = vec![
            FormFieldItem::new("name"),
            FormFieldItem::new("internal").disabled(true),
            FormFieldItem::new("email"),
        ];
        let values = vec![Some("Ada"), None, Some("ada@example.test")];
        let form = Form::new(&fields, &values);
        let mut state = FormState::new(Some(0));

        let outcome = form.handle_event(&mut state, &Event::Key(KeyStroke::simple(KeyCode::Tab)));

        assert_eq!(outcome, FormOutcome::Focused(2));
        assert_eq!(state.focused(), Some(2));
    }

    #[test]
    fn backtab_moves_focus_to_previous_enabled_field() {
        let fields = vec![
            FormFieldItem::new("name"),
            FormFieldItem::new("internal").disabled(true),
            FormFieldItem::new("email"),
        ];
        let values = vec![Some("Ada"), None, Some("ada@example.test")];
        let form = Form::new(&fields, &values);
        let mut state = FormState::new(Some(2));

        let outcome = form.handle_event(
            &mut state,
            &Event::Key(KeyStroke {
                key: KeyCode::Tab,
                modifiers: bmux_keyboard::Modifiers {
                    shift: true,
                    ..bmux_keyboard::Modifiers::NONE
                },
            }),
        );

        assert_eq!(outcome, FormOutcome::Focused(0));
        assert_eq!(state.focused(), Some(0));
    }

    #[test]
    fn focus_normalizes_when_current_field_becomes_disabled() {
        let fields = vec![
            FormFieldItem::new("name"),
            FormFieldItem::new("email").disabled(true),
        ];
        let values = vec![Some("Ada"), Some("ada@example.test")];
        let form = Form::new(&fields, &values);
        let mut state = FormState::new(Some(1));

        let outcome = form.handle_event(&mut state, &Event::Key(KeyStroke::simple(KeyCode::Tab)));

        assert_eq!(outcome, FormOutcome::Ignored);
        assert_eq!(state.focused(), Some(0));
    }

    #[test]
    fn validation_exposes_invalid_field_ids() {
        let fields = vec![
            FormFieldItem::new("name").required(true),
            FormFieldItem::new("email").required(true),
            FormFieldItem::new("internal").required(true).disabled(true),
        ];
        let values = vec![Some("Ada"), Some("   "), None];
        let form = Form::new(&fields, &values);

        assert_eq!(form.validate_ids(), vec!["email"]);
    }

    #[test]
    fn submit_reports_invalid_required_fields() {
        let fields = vec![
            FormFieldItem::new("name").required(true),
            FormFieldItem::new("email").required(true),
        ];
        let values = vec![Some("Ada"), Some("   ")];
        let form = Form::new(&fields, &values);
        let mut state = FormState::new(Some(0));

        let outcome = form.handle_event(&mut state, &Event::Key(KeyStroke::simple(KeyCode::Enter)));

        assert_eq!(outcome, FormOutcome::ValidationFailed(vec![1]));
    }

    #[test]
    fn submit_succeeds_when_required_fields_have_values() {
        let fields = vec![FormFieldItem::new("name").required(true)];
        let values = vec![Some("Ada")];
        let form = Form::new(&fields, &values);
        let mut state = FormState::new(Some(0));

        let outcome = form.handle_event(&mut state, &Event::Key(KeyStroke::simple(KeyCode::Enter)));

        assert_eq!(outcome, FormOutcome::Submitted);
    }

    #[test]
    fn escape_cancels_form() {
        let fields = vec![FormFieldItem::new("name")];
        let values = vec![Some("Ada")];
        let form = Form::new(&fields, &values);
        let mut state = FormState::new(Some(0));

        let outcome =
            form.handle_event(&mut state, &Event::Key(KeyStroke::simple(KeyCode::Escape)));

        assert_eq!(outcome, FormOutcome::Cancelled);
    }

    #[test]
    fn mouse_click_focuses_hit_field() {
        let fields = vec![FormFieldItem::new("name"), FormFieldItem::new("email")];
        let values = vec![Some("Ada"), Some("ada@example.test")];
        let areas = vec![Rect::new(0, 0, 10, 1), Rect::new(0, 2, 10, 1)];
        let form = Form::new(&fields, &values).field_areas(&areas);
        let mut state = FormState::new(Some(0));

        let outcome = form.handle_event(
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(3, 2),
            )),
        );

        assert_eq!(outcome, FormOutcome::Focused(1));
        assert_eq!(state.focused(), Some(1));
    }
}
