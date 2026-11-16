//! Single-modal form engine: model, keyboard and mouse handling, and a view
//! built from measured `bmux_tui` components.
//!
//! Every field lives in one scrollable body. Focus moves through the enabled
//! fields and then the footer's Submit/Cancel buttons. Enter submits from
//! anywhere when the form is valid; otherwise it reveals every error and
//! focuses the first invalid field.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use bmux_terminal::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use bmux_text_edit::{TextDelete, TextEditBuffer, TextMotion};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::composition::{Column, HitArea, Row, SizeBox, Stack, TextBlock};
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::hit::{HitMap, HitRegion};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Text};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text_block::Alignment;
use bmux_tui_components::scroll_view::{ScrollViewComponent, ScrollViewState};
use bmux_tui_components::scrollbar::{
    ScrollbarComponent, ScrollbarPolicy, ScrollbarState, ScrollbarStyles,
};
use bmux_tui_components::text_input::{TextInputComponent, TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{
    TextInputBoxComponent, TextInputBoxPolicy, TextInputBoxStyles,
};

use super::scaffold::{ACTIONS_ID, PromptTheme, section_rule};
use super::{AttachPromptEnvelope, emit_form_changed, run_prompt_validation};
use crate::runtime::prompt::{
    PromptField, PromptFormField, PromptFormFieldKind, PromptFormLayout, PromptFormSection,
    PromptFormValue,
};

pub(super) const SUBMIT_ACTION: usize = 0;
pub(super) const CANCEL_ACTION: usize = 1;
const FIELD_ID: &str = "prompt.form.field";
const OPTION_ID: &str = "prompt.form.option";
const ACCENT_WIDTH: u16 = 2;
const LABEL_GAP: u16 = 2;
const WHEEL_ROWS: u64 = 3;
const MAX_INPUT_ROWS: u64 = 4;
/// Widest the label column grows; longer labels wrap.
const MAX_LABEL_WIDTH: usize = 22;
/// Widest a control asks the modal to be; option lists wrap beyond it.
const MAX_CONTROL_WIDTH: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormFocus {
    Field(usize),
    Action(usize),
}

#[derive(Debug)]
pub(super) struct FormState {
    pub(super) focus: FormFocus,
    /// Option under the cursor inside a focused multi-toggle field.
    option_cursor: usize,
    pub(super) values: BTreeMap<String, PromptFormValue>,
    editors: BTreeMap<String, TextEditBuffer>,
    pub(super) errors: BTreeMap<String, String>,
    touched: BTreeSet<String>,
    scroll: ScrollViewState,
    reveal_pending: bool,
    content_height: u64,
    viewport_height: u64,
}

#[derive(Debug)]
pub(super) enum FormOutcome {
    Consumed,
    Submit(BTreeMap<String, PromptFormValue>),
    Cancel,
}

pub(super) fn flatten(sections: &[PromptFormSection]) -> Vec<&PromptFormField> {
    sections
        .iter()
        .flat_map(|section| &section.fields)
        .collect()
}

const fn is_text_like(kind: &PromptFormFieldKind) -> bool {
    matches!(
        kind,
        PromptFormFieldKind::Text { .. }
            | PromptFormFieldKind::Integer { .. }
            | PromptFormFieldKind::Number { .. }
    )
}

fn default_value(field: &PromptFormField) -> PromptFormValue {
    match &field.kind {
        PromptFormFieldKind::Bool { default } => PromptFormValue::Bool(*default),
        PromptFormFieldKind::Text { initial_value, .. } => {
            PromptFormValue::Text(initial_value.clone())
        }
        PromptFormFieldKind::Integer { initial_value, .. } => {
            PromptFormValue::Integer(*initial_value)
        }
        PromptFormFieldKind::Number { initial_value, .. } => {
            PromptFormValue::Number(initial_value.clone())
        }
        PromptFormFieldKind::SingleSelect {
            options,
            default_index,
        } => PromptFormValue::Single(
            options
                .get((*default_index).min(options.len().saturating_sub(1)))
                .map_or_else(String::new, |option| option.value.clone()),
        ),
        PromptFormFieldKind::MultiToggle {
            options,
            default_indices,
            ..
        } => {
            let mut values = default_indices
                .iter()
                .filter_map(|index| options.get(*index).map(|option| option.value.clone()))
                .collect::<Vec<_>>();
            values.sort();
            values.dedup();
            PromptFormValue::Multi(values)
        }
    }
}

fn default_editor(field: &PromptFormField) -> Option<TextEditBuffer> {
    match &field.kind {
        PromptFormFieldKind::Text { initial_value, .. }
        | PromptFormFieldKind::Number { initial_value, .. } => {
            Some(TextEditBuffer::from_text(initial_value.clone()))
        }
        PromptFormFieldKind::Integer { initial_value, .. } => {
            Some(TextEditBuffer::from_text(initial_value.to_string()))
        }
        _ => None,
    }
}

impl FormState {
    pub(super) fn new(sections: &[PromptFormSection]) -> Self {
        let fields = flatten(sections);
        let errors = fields
            .iter()
            .filter_map(|field| {
                field
                    .error
                    .clone()
                    .map(|message| (field.id.clone(), message))
            })
            .collect::<BTreeMap<_, _>>();
        let focus = fields
            .iter()
            .position(|field| !field.disabled && errors.contains_key(&field.id))
            .or_else(|| fields.iter().position(|field| !field.disabled))
            .map_or(FormFocus::Action(SUBMIT_ACTION), FormFocus::Field);
        let mut state = Self {
            focus,
            option_cursor: 0,
            values: BTreeMap::new(),
            editors: BTreeMap::new(),
            errors,
            touched: BTreeSet::new(),
            scroll: ScrollViewState::new(),
            reveal_pending: true,
            content_height: 0,
            viewport_height: 0,
        };
        state.load_defaults(&fields);
        state
    }

    fn load_defaults(&mut self, fields: &[&PromptFormField]) {
        self.values = fields
            .iter()
            .map(|field| (field.id.clone(), default_value(field)))
            .collect();
        self.editors = fields
            .iter()
            .filter_map(|field| default_editor(field).map(|editor| (field.id.clone(), editor)))
            .collect();
    }

    #[cfg(test)]
    pub(super) const fn scroll_offset(&self) -> u64 {
        self.scroll.vertical_offset()
    }

    /// Focus stops in Tab order: enabled fields, then Submit and Cancel.
    fn stops(fields: &[&PromptFormField]) -> Vec<FormFocus> {
        fields
            .iter()
            .enumerate()
            .filter(|(_, field)| !field.disabled)
            .map(|(index, _)| FormFocus::Field(index))
            .chain([
                FormFocus::Action(SUBMIT_ACTION),
                FormFocus::Action(CANCEL_ACTION),
            ])
            .collect()
    }

    fn set_focus(&mut self, fields: &[&PromptFormField], focus: FormFocus) {
        if focus == self.focus {
            return;
        }
        if let FormFocus::Field(previous) = self.focus
            && let Some(field) = fields.get(previous)
            && self.touched.contains(&field.id)
        {
            match self.validate(field) {
                Ok(()) => self.errors.remove(&field.id),
                Err(message) => self.errors.insert(field.id.clone(), message),
            };
        }
        self.focus = focus;
        self.option_cursor = 0;
        self.reveal_pending = true;
    }

    fn step_focus(&mut self, fields: &[&PromptFormField], forward: bool, wrap: bool) {
        let stops = Self::stops(fields);
        let current = stops
            .iter()
            .position(|stop| *stop == self.focus)
            .unwrap_or(0);
        let last = stops.len().saturating_sub(1);
        let next = match (forward, wrap) {
            (true, true) if current >= last => 0,
            (false, true) if current == 0 => last,
            (true, _) => current.saturating_add(1).min(last),
            (false, _) => current.saturating_sub(1),
        };
        if let Some(stop) = stops.get(next) {
            self.set_focus(fields, *stop);
        }
    }

    fn validate(&self, field: &PromptFormField) -> Result<(), String> {
        validate_field(field, &self.values, &self.editors)
    }

    fn after_change(
        &mut self,
        envelope: &AttachPromptEnvelope,
        field: &PromptFormField,
        live_preview: bool,
    ) {
        self.touched.insert(field.id.clone());
        match self.validate(field) {
            Ok(()) => {
                self.errors.remove(&field.id);
                if live_preview {
                    emit_form_changed(envelope, field, &self.values);
                }
            }
            Err(message) => {
                // Keep showing an error the user is fixing; new errors wait
                // for blur or submit so typing is not interrupted.
                if let Some(existing) = self.errors.get_mut(&field.id) {
                    *existing = message;
                }
            }
        }
    }

    fn try_submit(&mut self, fields: &[&PromptFormField]) -> FormOutcome {
        self.errors.clear();
        for field in fields {
            if let Err(message) = self.validate(field) {
                self.errors.insert(field.id.clone(), message);
            }
        }
        if self.errors.is_empty() {
            return FormOutcome::Submit(self.values.clone());
        }
        if let Some(index) = fields
            .iter()
            .position(|field| self.errors.contains_key(&field.id))
        {
            self.set_focus(fields, FormFocus::Field(index));
            self.reveal_pending = true;
        }
        FormOutcome::Consumed
    }

    fn reset(
        &mut self,
        envelope: &AttachPromptEnvelope,
        fields: &[&PromptFormField],
        live_preview: bool,
    ) {
        self.load_defaults(fields);
        self.errors.clear();
        self.touched.clear();
        if live_preview {
            for field in fields {
                emit_form_changed(envelope, field, &self.values);
            }
        }
    }

    /// Copy the editor text of `field` into its typed value.
    fn sync_value(&mut self, field: &PromptFormField) {
        let Some(text) = self
            .editors
            .get(&field.id)
            .map(|editor| editor.text().to_owned())
        else {
            return;
        };
        match &field.kind {
            PromptFormFieldKind::Text { .. } => {
                self.values
                    .insert(field.id.clone(), PromptFormValue::Text(text));
            }
            PromptFormFieldKind::Number { .. } => {
                self.values
                    .insert(field.id.clone(), PromptFormValue::Number(text));
            }
            PromptFormFieldKind::Integer { .. } => {
                if let Ok(value) = text.trim().parse::<i64>() {
                    self.values
                        .insert(field.id.clone(), PromptFormValue::Integer(value));
                }
            }
            _ => {}
        }
    }

    fn edit_text(&mut self, field: &PromptFormField, edit: impl FnOnce(&mut TextEditBuffer)) {
        let editor = self.editors.entry(field.id.clone()).or_default();
        edit(editor);
        self.sync_value(field);
    }

    fn adjust_integer(&mut self, field: &PromptFormField, delta: i64) {
        let PromptFormFieldKind::Integer { min, max, .. } = &field.kind else {
            return;
        };
        let current = self
            .editors
            .get(&field.id)
            .and_then(|editor| editor.text().trim().parse::<i64>().ok())
            .or_else(|| match self.values.get(&field.id) {
                Some(PromptFormValue::Integer(value)) => Some(*value),
                _ => None,
            })
            .unwrap_or(0);
        let mut next = current.saturating_add(delta);
        if let Some(min) = min {
            next = next.max(*min);
        }
        if let Some(max) = max {
            next = next.min(*max);
        }
        self.editors.insert(
            field.id.clone(),
            TextEditBuffer::from_text(next.to_string()),
        );
        self.values
            .insert(field.id.clone(), PromptFormValue::Integer(next));
    }

    fn toggle_bool(&mut self, field: &PromptFormField) {
        if let Some(PromptFormValue::Bool(value)) = self.values.get_mut(&field.id) {
            *value = !*value;
        }
    }

    fn step_single(&mut self, field: &PromptFormField, forward: bool) {
        let PromptFormFieldKind::SingleSelect { options, .. } = &field.kind else {
            return;
        };
        let Some(PromptFormValue::Single(value)) = self.values.get_mut(&field.id) else {
            return;
        };
        if options.is_empty() {
            return;
        }
        let current = options
            .iter()
            .position(|option| option.value == *value)
            .unwrap_or(0);
        let next = if forward {
            current.saturating_add(1) % options.len()
        } else {
            current.checked_sub(1).unwrap_or(options.len() - 1)
        };
        value.clone_from(&options[next].value);
    }

    fn select_single(&mut self, field: &PromptFormField, index: usize) {
        let PromptFormFieldKind::SingleSelect { options, .. } = &field.kind else {
            return;
        };
        if let (Some(option), Some(PromptFormValue::Single(value))) =
            (options.get(index), self.values.get_mut(&field.id))
        {
            value.clone_from(&option.value);
        }
    }

    fn toggle_multi(&mut self, field: &PromptFormField, index: usize) {
        let PromptFormFieldKind::MultiToggle { options, .. } = &field.kind else {
            return;
        };
        let (Some(option), Some(PromptFormValue::Multi(selected))) =
            (options.get(index), self.values.get_mut(&field.id))
        else {
            return;
        };
        if selected.contains(&option.value) {
            selected.retain(|value| value != &option.value);
        } else {
            selected.push(option.value.clone());
            // Keep submitted values in option order, matching standalone toggles.
            selected.sort_by_key(|value| {
                options
                    .iter()
                    .position(|option| &option.value == value)
                    .unwrap_or(usize::MAX)
            });
        }
    }

    #[allow(clippy::too_many_lines)] // One dispatch per field kind keeps the key model readable.
    fn field_key(
        &mut self,
        envelope: &AttachPromptEnvelope,
        fields: &[&PromptFormField],
        field: &PromptFormField,
        key: &KeyEvent,
        live_preview: bool,
    ) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if is_text_like(&field.kind) {
            let integer = matches!(field.kind, PromptFormFieldKind::Integer { .. });
            let changed = match key.code {
                KeyCode::Left | KeyCode::Right if integer && (shift || ctrl) => {
                    let step = if ctrl { 10 } else { 5 };
                    let delta = if key.code == KeyCode::Left {
                        -step
                    } else {
                        step
                    };
                    self.adjust_integer(field, delta);
                    true
                }
                KeyCode::Char('a') if ctrl => {
                    self.edit_text(field, |editor| editor.move_cursor(TextMotion::Start));
                    false
                }
                KeyCode::Char('e') if ctrl => {
                    self.edit_text(field, |editor| editor.move_cursor(TextMotion::End));
                    false
                }
                KeyCode::Char('u') if ctrl => {
                    self.edit_text(field, TextEditBuffer::clear);
                    true
                }
                KeyCode::Char('w') if ctrl => {
                    self.edit_text(field, |editor| editor.delete(TextDelete::WordBackward));
                    true
                }
                KeyCode::Char(ch) if !ctrl && !alt => {
                    let accepted = !integer || ch.is_ascii_digit() || ch == '-';
                    if accepted {
                        self.edit_text(field, |editor| editor.insert_char(ch));
                    }
                    accepted
                }
                KeyCode::Backspace => {
                    let delete = if alt || ctrl {
                        TextDelete::WordBackward
                    } else {
                        TextDelete::Backward
                    };
                    self.edit_text(field, |editor| editor.delete(delete));
                    true
                }
                KeyCode::Delete => {
                    self.edit_text(field, |editor| editor.delete(TextDelete::Forward));
                    true
                }
                KeyCode::Left | KeyCode::Right => {
                    let motion = match (key.code == KeyCode::Left, alt || ctrl) {
                        (true, true) => TextMotion::WordLeft,
                        (false, true) => TextMotion::WordRight,
                        (true, false) => TextMotion::Left,
                        (false, false) => TextMotion::Right,
                    };
                    self.edit_text(field, |editor| editor.move_cursor(motion));
                    false
                }
                KeyCode::Home => {
                    self.edit_text(field, |editor| editor.move_cursor(TextMotion::Start));
                    false
                }
                KeyCode::End => {
                    self.edit_text(field, |editor| editor.move_cursor(TextMotion::End));
                    false
                }
                _ => false,
            };
            if changed {
                self.after_change(envelope, field, live_preview);
            }
            return;
        }

        let changed = match (&field.kind, key.code) {
            (_, KeyCode::Char('j')) => {
                self.step_focus(fields, true, false);
                false
            }
            (_, KeyCode::Char('k')) => {
                self.step_focus(fields, false, false);
                false
            }
            (
                PromptFormFieldKind::Bool { .. },
                KeyCode::Char(' ' | 'h' | 'l') | KeyCode::Left | KeyCode::Right,
            ) => {
                self.toggle_bool(field);
                true
            }
            (PromptFormFieldKind::SingleSelect { .. }, KeyCode::Left | KeyCode::Char('h')) => {
                self.step_single(field, false);
                true
            }
            (
                PromptFormFieldKind::SingleSelect { .. },
                KeyCode::Right | KeyCode::Char('l' | ' '),
            ) => {
                self.step_single(field, true);
                true
            }
            (PromptFormFieldKind::MultiToggle { .. }, KeyCode::Left | KeyCode::Char('h')) => {
                self.option_cursor = self.option_cursor.saturating_sub(1);
                false
            }
            (
                PromptFormFieldKind::MultiToggle { options, .. },
                KeyCode::Right | KeyCode::Char('l'),
            ) => {
                self.option_cursor = self
                    .option_cursor
                    .saturating_add(1)
                    .min(options.len().saturating_sub(1));
                false
            }
            (PromptFormFieldKind::MultiToggle { .. }, KeyCode::Char(' ')) => {
                self.toggle_multi(field, self.option_cursor);
                true
            }
            _ => false,
        };
        if changed {
            self.after_change(envelope, field, live_preview);
        }
    }
}

const fn form_spec(envelope: &AttachPromptEnvelope) -> Option<(&[PromptFormSection], bool, bool)> {
    match &envelope.request.field {
        PromptField::Form {
            sections,
            live_preview,
            resettable,
            ..
        } => Some((sections.as_slice(), *live_preview, *resettable)),
        _ => None,
    }
}

pub(super) fn handle_key(
    state: &mut FormState,
    envelope: &AttachPromptEnvelope,
    key: &KeyEvent,
) -> FormOutcome {
    let Some((sections, live_preview, resettable)) = form_spec(envelope) else {
        return FormOutcome::Consumed;
    };
    let fields = flatten(sections);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Tab => {
            state.step_focus(&fields, true, true);
            return FormOutcome::Consumed;
        }
        KeyCode::BackTab => {
            state.step_focus(&fields, false, true);
            return FormOutcome::Consumed;
        }
        KeyCode::Down => {
            state.step_focus(&fields, true, false);
            return FormOutcome::Consumed;
        }
        KeyCode::Up => {
            state.step_focus(&fields, false, false);
            return FormOutcome::Consumed;
        }
        KeyCode::Char('r' | 'R') if ctrl && resettable => {
            state.reset(envelope, &fields, live_preview);
            return FormOutcome::Consumed;
        }
        KeyCode::Enter => {
            return if state.focus == FormFocus::Action(CANCEL_ACTION) {
                FormOutcome::Cancel
            } else {
                state.try_submit(&fields)
            };
        }
        _ => {}
    }
    match state.focus {
        FormFocus::Action(action) => match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l') => {
                state.focus = FormFocus::Action(if action == SUBMIT_ACTION {
                    CANCEL_ACTION
                } else {
                    SUBMIT_ACTION
                });
                FormOutcome::Consumed
            }
            KeyCode::Char(' ') if action == CANCEL_ACTION => FormOutcome::Cancel,
            KeyCode::Char(' ') => state.try_submit(&fields),
            KeyCode::Char('k') => {
                state.step_focus(&fields, false, false);
                FormOutcome::Consumed
            }
            _ => FormOutcome::Consumed,
        },
        FormFocus::Field(index) => {
            if let Some(field) = fields.get(index).copied()
                && !field.disabled
            {
                state.field_key(envelope, &fields, field, key, live_preview);
            }
            FormOutcome::Consumed
        }
    }
}

pub(super) fn handle_paste(state: &mut FormState, envelope: &AttachPromptEnvelope, text: &str) {
    let Some((sections, live_preview, _)) = form_spec(envelope) else {
        return;
    };
    let fields = flatten(sections);
    let FormFocus::Field(index) = state.focus else {
        return;
    };
    let Some(field) = fields.get(index).copied() else {
        return;
    };
    if field.disabled || !is_text_like(&field.kind) {
        return;
    }
    let text = if matches!(field.kind, PromptFormFieldKind::Integer { .. }) {
        text.chars()
            .filter(|ch| ch.is_ascii_digit() || *ch == '-')
            .collect()
    } else {
        text.to_owned()
    };
    state.edit_text(field, |editor| editor.paste(&text));
    state.after_change(envelope, field, live_preview);
}

enum MouseTarget {
    Action(String),
    Option(usize, usize),
    Field(usize),
}

fn mouse_target(hits: &HitMap, point: Point) -> Option<MouseTarget> {
    let mut field = None;
    for hit in hits.regions().iter().rev() {
        if !hit.area.contains(point) {
            continue;
        }
        let id = hit.id.as_str();
        if let Some(action) = id
            .strip_prefix(ACTIONS_ID)
            .and_then(|rest| rest.strip_prefix('.'))
        {
            return Some(MouseTarget::Action(action.to_owned()));
        }
        if let Some((field, option)) = id
            .strip_prefix(OPTION_ID)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.split_once('.'))
            && let (Ok(field), Ok(option)) = (field.parse(), option.parse())
        {
            return Some(MouseTarget::Option(field, option));
        }
        if field.is_none() {
            field = id
                .strip_prefix(FIELD_ID)
                .and_then(|rest| rest.strip_prefix('.'))
                .and_then(|index| index.parse().ok());
        }
    }
    field.map(MouseTarget::Field)
}

pub(super) fn handle_mouse(
    state: &mut FormState,
    envelope: &AttachPromptEnvelope,
    hits: &HitMap,
    body: Rect,
    mouse: MouseEvent,
) -> FormOutcome {
    let Some((sections, live_preview, _)) = form_spec(envelope) else {
        return FormOutcome::Consumed;
    };
    let fields = flatten(sections);
    let point = Point::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if body.contains(point) => {
            let max = state.content_height.saturating_sub(state.viewport_height);
            let offset = state.scroll.vertical_offset();
            let next = if mouse.kind == MouseEventKind::ScrollUp {
                offset.saturating_sub(WHEEL_ROWS)
            } else {
                offset.saturating_add(WHEEL_ROWS).min(max)
            };
            state.scroll.set_vertical_offset(next);
            state.reveal_pending = false;
            FormOutcome::Consumed
        }
        MouseEventKind::Down(MouseButton::Left) => match mouse_target(hits, point) {
            Some(MouseTarget::Action(action)) if action == "cancel" => FormOutcome::Cancel,
            Some(MouseTarget::Action(_)) => {
                state.set_focus(&fields, FormFocus::Action(SUBMIT_ACTION));
                state.try_submit(&fields)
            }
            Some(MouseTarget::Option(index, option)) => {
                let Some(field) = fields.get(index).copied().filter(|field| !field.disabled) else {
                    return FormOutcome::Consumed;
                };
                state.set_focus(&fields, FormFocus::Field(index));
                // Keep the clicked field in view without jumping the viewport.
                state.reveal_pending = false;
                match field.kind {
                    PromptFormFieldKind::Bool { .. } => state.toggle_bool(field),
                    PromptFormFieldKind::SingleSelect { .. } => state.select_single(field, option),
                    PromptFormFieldKind::MultiToggle { .. } => {
                        state.option_cursor = option;
                        state.toggle_multi(field, option);
                    }
                    _ => return FormOutcome::Consumed,
                }
                state.after_change(envelope, field, live_preview);
                FormOutcome::Consumed
            }
            Some(MouseTarget::Field(index)) => {
                if fields.get(index).is_some_and(|field| !field.disabled) {
                    state.set_focus(&fields, FormFocus::Field(index));
                    state.reveal_pending = false;
                }
                FormOutcome::Consumed
            }
            None => FormOutcome::Consumed,
        },
        _ => FormOutcome::Consumed,
    }
}

fn range_message(min: Option<String>, max: Option<String>) -> String {
    match (min, max) {
        (Some(min), Some(max)) => format!("must be between {min} and {max}"),
        (Some(min), None) => format!("must be at least {min}"),
        (None, Some(max)) => format!("must be at most {max}"),
        (None, None) => "value is out of range".to_owned(),
    }
}

pub(super) fn validate_field(
    field: &PromptFormField,
    values: &BTreeMap<String, PromptFormValue>,
    editors: &BTreeMap<String, TextEditBuffer>,
) -> Result<(), String> {
    if field.disabled {
        return Ok(());
    }
    let Some(value) = values.get(&field.id) else {
        return Err("missing value".to_owned());
    };
    match (&field.kind, value) {
        (PromptFormFieldKind::Text { validation, .. }, PromptFormValue::Text(value)) => {
            if field.required && value.trim().is_empty() {
                return Err("required".to_owned());
            }
            if !value.trim().is_empty()
                && let Some(rule) = validation
            {
                run_prompt_validation(rule, value)?;
            }
        }
        (PromptFormFieldKind::Integer { min, max, .. }, PromptFormValue::Integer(value)) => {
            let text = editors.get(&field.id).map_or_else(
                || value.to_string(),
                |editor| editor.text().trim().to_owned(),
            );
            let parsed = text
                .parse::<i64>()
                .map_err(|_| "enter a whole number".to_owned())?;
            if min.is_some_and(|min| parsed < min) || max.is_some_and(|max| parsed > max) {
                return Err(range_message(
                    min.map(|min| min.to_string()),
                    max.map(|max| max.to_string()),
                ));
            }
        }
        (PromptFormFieldKind::Number { min, max, .. }, PromptFormValue::Number(value)) => {
            if value.trim().is_empty() && !field.required {
                return Ok(());
            }
            let parsed = value
                .trim()
                .parse::<f64>()
                .map_err(|_| "enter a number".to_owned())?;
            let below = min
                .as_deref()
                .and_then(|min| min.parse::<f64>().ok())
                .is_some_and(|min| parsed < min);
            let above = max
                .as_deref()
                .and_then(|max| max.parse::<f64>().ok())
                .is_some_and(|max| parsed > max);
            if below || above {
                return Err(range_message(min.clone(), max.clone()));
            }
        }
        (PromptFormFieldKind::SingleSelect { .. }, PromptFormValue::Single(value))
            if field.required && value.is_empty() =>
        {
            return Err("required".to_owned());
        }
        (PromptFormFieldKind::MultiToggle { min_selected, .. }, PromptFormValue::Multi(values))
            if values.len() < *min_selected =>
        {
            return Err(if *min_selected == 1 {
                "select at least one".to_owned()
            } else {
                format!("select at least {min_selected}")
            });
        }
        _ => {}
    }
    Ok(())
}

// ── Hints ───────────────────────────────────────────────────────────────────

pub(super) fn hints(
    state: &FormState,
    sections: &[PromptFormSection],
    submit_label: &str,
    cancel_label: &str,
    esc_cancels: bool,
    resettable: bool,
    live_preview: bool,
) -> Vec<(String, String)> {
    let fields = flatten(sections);
    let mut hints = Vec::new();
    let pair = |key: &str, label: &str| (key.to_owned(), label.to_owned());
    if let FormFocus::Field(index) = state.focus
        && let Some(field) = fields.get(index)
    {
        match field.kind {
            PromptFormFieldKind::Bool { .. } => hints.push(pair("Space", "toggle")),
            PromptFormFieldKind::SingleSelect { .. } => hints.push(pair("←→", "choose")),
            PromptFormFieldKind::MultiToggle { .. } => {
                hints.push(pair("←→", "move"));
                hints.push(pair("Space", "toggle"));
            }
            PromptFormFieldKind::Integer { .. } => hints.push(pair("⇧/^←→", "±5/10")),
            PromptFormFieldKind::Text { .. } | PromptFormFieldKind::Number { .. } => {}
        }
    }
    if fields.len() > 1 {
        hints.push(pair("Tab", "next"));
    }
    hints.push(pair("Enter", &submit_label.to_lowercase()));
    if esc_cancels {
        hints.push(pair("Esc", &cancel_label.to_lowercase()));
    }
    if resettable {
        hints.push(pair("^R", "reset"));
    }
    if live_preview {
        hints.push(pair("F6", "peek"));
    }
    hints
}

// ── View ────────────────────────────────────────────────────────────────────

/// Options rendered inline as `(•) label` radios or `[x] label` checkboxes,
/// wrapped greedily onto as many rows as the width needs.
struct Chips {
    id: String,
    items: Vec<Chip>,
    background: Style,
}

struct Chip {
    mark: &'static str,
    label: String,
    mark_style: Style,
    label_style: Style,
}

impl Chip {
    fn width(&self) -> u64 {
        let label = self.label.chars().count();
        let width = if label == 0 {
            self.mark.chars().count()
        } else {
            self.mark.chars().count() + 1 + label
        };
        u64::try_from(width).unwrap_or(u64::MAX)
    }
}

impl Chips {
    const GAP: u64 = 2;

    fn arrange(&self, width: u64) -> (Vec<(u64, u64)>, u64, u64) {
        let mut positions = Vec::with_capacity(self.items.len());
        let (mut x, mut y, mut used) = (0u64, 0u64, 0u64);
        for item in &self.items {
            let item_width = item.width();
            if x > 0 && x.saturating_add(item_width) > width {
                x = 0;
                y = y.saturating_add(1);
            }
            positions.push((x, y));
            x = x.saturating_add(item_width);
            used = used.max(x);
            x = x.saturating_add(Self::GAP);
        }
        (positions, y.saturating_add(1), used)
    }
}

impl Component for Chips {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for item in &self.items {
            item.mark.hash(&mut layout);
            item.label.hash(&mut layout);
            item.mark_style.hash(&mut paint);
            item.label_style.hash(&mut paint);
        }
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let (_, height, used) = self.arrange(constraints.max_width());
        LayoutNode::leaf(
            LayoutId::new(self.id.clone()),
            constraints.constrain(LogicalSize::new(used, height)),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let (positions, _, _) = self.arrange(layout.size.width);
        for (index, (item, (x, y))) in self.items.iter().zip(positions).enumerate() {
            let (Ok(x), Ok(y), Ok(width)) = (
                u16::try_from(x),
                u16::try_from(y),
                u16::try_from(item.width()),
            ) else {
                continue;
            };
            let mut spans = vec![Span::styled(item.mark, item.mark_style)];
            if !item.label.is_empty() {
                spans.push(Span::styled(format!(" {}", item.label), item.label_style));
            }
            cx.write_line_with_fallback_style(
                LocalRect::new(i32::from(x), i64::from(y), width, 1),
                &Line::from_spans(spans),
                self.background,
            );
            cx.push_hit(HitRegion::new(
                format!("{}.{index}", self.id),
                Rect::new(x, y, width, 1),
            ));
        }
    }
}

/// One frame's view over the form; owns the temporary per-field input state
/// the borrowed component tree needs.
pub(super) struct FormView<'a> {
    sections: &'a [PromptFormSection],
    state: &'a FormState,
    inputs: BTreeMap<String, RefCell<TextInputState>>,
    policy: TextInputPolicy,
    theme: PromptTheme,
    layout: PromptFormLayout,
    width: u16,
    label_width: u16,
}

fn label_width(sections: &[PromptFormSection]) -> usize {
    flatten(sections)
        .iter()
        .filter(|field| !field.label.is_empty())
        .map(|field| field.label.chars().count() + if field.required { 2 } else { 0 })
        .max()
        .unwrap_or(0)
        .min(MAX_LABEL_WIDTH)
}

fn control_width(field: &PromptFormField) -> usize {
    let chips = |labels: &mut dyn Iterator<Item = usize>| {
        labels
            // Mark, space, label, and the two-column gap to the next chip.
            .map(|label| label + 6)
            .sum::<usize>()
            .min(56)
    };
    match &field.kind {
        PromptFormFieldKind::Bool { .. } => 3,
        PromptFormFieldKind::Text {
            initial_value,
            placeholder,
            ..
        } => initial_value
            .chars()
            .count()
            .max(
                placeholder
                    .as_ref()
                    .map_or(0, |value| value.chars().count()),
            )
            .max(24),
        PromptFormFieldKind::Integer { .. } | PromptFormFieldKind::Number { .. } => 12,
        PromptFormFieldKind::SingleSelect { options, .. }
        | PromptFormFieldKind::MultiToggle { options, .. } => {
            chips(&mut options.iter().map(|option| option.label.chars().count()))
        }
    }
}

/// Content width a form would like, before clamping to the terminal.
pub(super) fn desired_width(sections: &[PromptFormSection], layout: PromptFormLayout) -> usize {
    let fields = flatten(sections);
    let control = fields
        .iter()
        .map(|field| control_width(field))
        .max()
        .unwrap_or(0)
        .min(MAX_CONTROL_WIDTH);
    let gutter = usize::from(ACCENT_WIDTH);
    let row = match layout {
        PromptFormLayout::Compact => label_width(sections)
            .saturating_add(gutter + usize::from(LABEL_GAP))
            .saturating_add(control),
        PromptFormLayout::Stacked => gutter.saturating_add(control).saturating_add(4),
    };
    sections
        .iter()
        .map(|section| section.title.chars().count().saturating_add(8))
        .fold(row, usize::max)
}

impl<'a> FormView<'a> {
    pub(super) fn new(
        sections: &'a [PromptFormSection],
        state: &'a FormState,
        theme: PromptTheme,
        layout: PromptFormLayout,
        width: u16,
    ) -> Self {
        let inputs = flatten(sections)
            .into_iter()
            .filter(|field| is_text_like(&field.kind))
            .map(|field| {
                let buffer = state
                    .editors
                    .get(&field.id)
                    .cloned()
                    .or_else(|| default_editor(field))
                    .unwrap_or_default();
                (field.id.clone(), RefCell::new(TextInputState::new(buffer)))
            })
            .collect();
        let max_label = u16::try_from(label_width(sections)).unwrap_or(u16::MAX);
        let label_width = max_label.min((width.saturating_mul(2) / 5).max(4));
        Self {
            sections,
            state,
            inputs,
            policy: TextInputPolicy::chat_composer(),
            theme,
            layout,
            width,
            label_width,
        }
    }

    const fn styled(&self, style: Style) -> Style {
        self.theme.background.patch(style)
    }

    fn spacer(width: u16, height: u64) -> SizeBox<'static> {
        SizeBox::new(Stack::new())
            .width(u64::from(width))
            .height(height)
    }

    /// The scrollable body: section headers and one measured row per field.
    pub(super) fn tree(&self) -> Column<'_> {
        let multiple_sections = self.sections.len() > 1;
        let mut column = Column::new().id("prompt.form.fields");
        let mut index = 0usize;
        for (section_index, section) in self.sections.iter().enumerate() {
            if section_index > 0 {
                column = column.child(Self::spacer(1, 1));
            }
            if multiple_sections && !section.title.is_empty() {
                column = column.child(
                    TextBlock::new(Text::from_lines([section_rule(
                        &section.title,
                        self.width,
                        self.theme,
                    )]))
                    .style(self.theme.background),
                );
            }
            if let Some(description) = &section.description {
                column = column.child(
                    TextBlock::new(description.clone()).style(self.styled(self.theme.muted)),
                );
            }
            for field in &section.fields {
                if self.layout == PromptFormLayout::Stacked && index > 0 {
                    column = column.child(Self::spacer(1, 1));
                }
                column = column.child(self.field_row(index, field));
                index += 1;
            }
        }
        column
    }

    fn is_focused(&self, index: usize) -> bool {
        self.state.focus == FormFocus::Field(index)
    }

    fn label_line(&self, field: &PromptFormField, focused: bool) -> Line {
        let style = if field.disabled {
            self.theme.muted
        } else if focused {
            self.theme.text.add_modifier(Modifier::BOLD)
        } else {
            self.theme.text
        };
        if field.label.is_empty() {
            return Line::raw("");
        }
        let mut spans = vec![Span::styled(field.label.clone(), style)];
        if field.required {
            spans.push(Span::styled(" *", self.theme.accent));
        }
        Line::from_spans(spans)
    }

    fn accent(&self, focused: bool) -> SizeBox<'static> {
        let mark = if focused { "▎" } else { " " };
        SizeBox::new(
            TextBlock::new(Text::from_lines([Line::from_spans([Span::styled(
                mark,
                self.theme.accent,
            )])]))
            .style(self.theme.background),
        )
        .width(u64::from(ACCENT_WIDTH))
    }

    fn field_row(&self, index: usize, field: &'a PromptFormField) -> HitArea<'_> {
        let focused = self.is_focused(index);
        let details = self.details(index, field, focused);
        let row = match self.layout {
            PromptFormLayout::Compact => {
                let label = SizeBox::new(
                    TextBlock::new(Text::from_lines([self.label_line(field, focused)]))
                        .alignment(Alignment::Right)
                        .style(self.theme.background),
                )
                .width(u64::from(self.label_width));
                Column::new().child(
                    Row::new()
                        .child(self.accent(focused))
                        .child(label)
                        .child(Self::spacer(
                            if self.label_width == 0 { 0 } else { LABEL_GAP },
                            1,
                        ))
                        .flex_child(1, details),
                )
            }
            PromptFormLayout::Stacked if field.label.is_empty() => Column::new().child(
                Row::new()
                    .child(self.accent(focused))
                    .flex_child(1, details),
            ),
            PromptFormLayout::Stacked => Column::new()
                .child(
                    Row::new().child(self.accent(focused)).flex_child(
                        1,
                        TextBlock::new(Text::from_lines([self.label_line(field, focused)]))
                            .style(self.theme.background),
                    ),
                )
                .child(
                    Row::new()
                        .child(Self::spacer(ACCENT_WIDTH, 1))
                        .flex_child(1, details),
                ),
        };
        HitArea::new(format!("{FIELD_ID}.{index}"), row)
    }

    /// Control plus the help and error lines beneath it.
    fn details(&self, index: usize, field: &'a PromptFormField, focused: bool) -> Column<'_> {
        let mut column = self.control(Column::new(), index, field, focused);
        let help = if field.disabled {
            field.disabled_reason.as_deref()
        } else if focused || self.layout == PromptFormLayout::Stacked {
            field.description.as_deref()
        } else {
            None
        };
        if let Some(help) = help {
            column =
                column.child(TextBlock::new(help.to_owned()).style(self.styled(self.theme.muted)));
        }
        if let Some(error) = self.state.errors.get(&field.id) {
            column = column
                .child(TextBlock::new(format!("✕ {error}")).style(self.styled(self.theme.error)));
        }
        column
    }

    const fn chip_styles(
        &self,
        field: &PromptFormField,
        active: bool,
        cursor: bool,
    ) -> (Style, Style) {
        if field.disabled {
            return (self.theme.muted, self.theme.muted);
        }
        let mark = if active {
            self.theme.accent.add_modifier(Modifier::BOLD)
        } else {
            self.theme.muted
        };
        let label = if cursor {
            self.theme.focused
        } else if active {
            self.theme.text
        } else {
            self.theme.muted.patch(self.theme.text)
        };
        (mark, label)
    }

    #[allow(clippy::too_many_lines)] // One arm per field kind keeps control projection in one place.
    fn control<'s>(
        &'s self,
        column: Column<'s>,
        index: usize,
        field: &'a PromptFormField,
        focused: bool,
    ) -> Column<'s> {
        let value = self.state.values.get(&field.id);
        let chips_id = format!("{OPTION_ID}.{index}");
        match &field.kind {
            PromptFormFieldKind::Bool { .. } => {
                let on = matches!(value, Some(PromptFormValue::Bool(true)));
                let (mark_style, _) = self.chip_styles(field, on, focused);
                let mark_style = if focused && !field.disabled {
                    mark_style.patch(self.theme.focused)
                } else {
                    mark_style
                };
                column.child(Chips {
                    id: chips_id,
                    items: vec![Chip {
                        mark: if on { "[x]" } else { "[ ]" },
                        label: String::new(),
                        mark_style,
                        label_style: mark_style,
                    }],
                    background: self.theme.background,
                })
            }
            PromptFormFieldKind::SingleSelect { options, .. } => {
                let selected = match value {
                    Some(PromptFormValue::Single(value)) => Some(value.as_str()),
                    _ => None,
                };
                let items = options
                    .iter()
                    .map(|option| {
                        let active = selected == Some(option.value.as_str());
                        let (mark_style, label_style) =
                            self.chip_styles(field, active, focused && active);
                        Chip {
                            mark: if active { "(•)" } else { "( )" },
                            label: option.label.clone(),
                            mark_style,
                            label_style,
                        }
                    })
                    .collect();
                column.child(Chips {
                    id: chips_id,
                    items,
                    background: self.theme.background,
                })
            }
            PromptFormFieldKind::MultiToggle { options, .. } => {
                let selected = match value {
                    Some(PromptFormValue::Multi(values)) => values.as_slice(),
                    _ => &[],
                };
                let items = options
                    .iter()
                    .enumerate()
                    .map(|(option_index, option)| {
                        let active = selected.contains(&option.value);
                        let cursor = focused && option_index == self.state.option_cursor;
                        let (mark_style, label_style) = self.chip_styles(field, active, cursor);
                        Chip {
                            mark: if active { "[x]" } else { "[ ]" },
                            label: option.label.clone(),
                            mark_style,
                            label_style,
                        }
                    })
                    .collect();
                column.child(Chips {
                    id: chips_id,
                    items,
                    background: self.theme.background,
                })
            }
            PromptFormFieldKind::Text { .. }
            | PromptFormFieldKind::Integer { .. }
            | PromptFormFieldKind::Number { .. } => {
                let Some(cell) = self.inputs.get(&field.id) else {
                    return column;
                };
                let placeholder = match &field.kind {
                    PromptFormFieldKind::Text {
                        placeholder: Some(placeholder),
                        ..
                    } => Some(placeholder.as_str()),
                    _ => None,
                };
                let text_style = if field.disabled {
                    self.styled(self.theme.muted)
                } else {
                    self.styled(self.theme.text)
                };
                match self.layout {
                    PromptFormLayout::Compact => {
                        let mut input = TextInputComponent::new(
                            format!("prompt.form.input.{index}"),
                            cell,
                            &self.policy,
                        )
                        .style(text_style)
                        .selection_style(self.theme.selected)
                        .focused(focused)
                        .disabled(field.disabled);
                        if let Some(placeholder) = placeholder {
                            input = input.placeholder(placeholder, self.styled(self.theme.muted));
                        }
                        column.child(SizeBox::new(input).max_height(MAX_INPUT_ROWS))
                    }
                    PromptFormLayout::Stacked => {
                        let mut input = TextInputBoxComponent::new(
                            format!("prompt.form.input.{index}"),
                            TextInputPolicy::chat_composer(),
                            cell,
                        )
                        .policy(
                            TextInputBoxPolicy::field()
                                .focused(focused)
                                .disabled(field.disabled)
                                .rows(1, Some(3)),
                        )
                        .styles(input_box_styles(self.theme));
                        if let Some(placeholder) = placeholder {
                            input = input.placeholder(placeholder);
                        }
                        column.child(input)
                    }
                }
            }
        }
    }
}

pub(super) const fn input_box_styles(theme: PromptTheme) -> TextInputBoxStyles {
    TextInputBoxStyles {
        text: theme.text,
        focused_text: theme.text,
        disabled_text: theme.muted,
        placeholder: theme.muted,
        selection: theme.selected,
        border: theme.muted,
        focused_border: theme.accent,
        background: theme.background,
        focused_background: theme.background,
        disabled_background: theme.background,
    }
}

fn measure(view: &FormView<'_>) -> u64 {
    view.tree()
        .layout(
            Constraints::for_width(u64::from(view.width)),
            &mut LayoutCx::new(),
        )
        .size
        .height
}

/// Natural body height of a form at `width` columns.
pub(super) fn natural_height(
    sections: &[PromptFormSection],
    state: &FormState,
    theme: PromptTheme,
    layout: PromptFormLayout,
    width: u16,
) -> u16 {
    let height = measure(&FormView::new(sections, state, theme, layout, width));
    u16::try_from(height).unwrap_or(u16::MAX)
}

/// Paint the form body into `body`, scrolling to keep focus visible.
pub(super) fn paint(
    state: &mut FormState,
    sections: &[PromptFormSection],
    theme: PromptTheme,
    layout: PromptFormLayout,
    body: Rect,
    cx: &mut PaintCx<'_, '_>,
) {
    if body.height == 0 || body.width == 0 {
        return;
    }
    let full = measure(&FormView::new(sections, state, theme, layout, body.width));
    let overflow = full > u64::from(body.height);
    let width = body.width.saturating_sub(u16::from(overflow));
    let (scroll, content_height) = {
        let view = FormView::new(sections, state, theme, layout, width);
        let content_height = measure(&view);
        let retained = Cell::new(state.scroll);
        let mut viewport = ScrollViewComponent::new(
            "prompt.form.viewport",
            LogicalSize::terminal(Size::new(width, body.height)),
            state.scroll,
            view.tree(),
        )
        .retain_state(&retained);
        if state.reveal_pending
            && let FormFocus::Field(index) = state.focus
        {
            viewport = viewport.reveal(format!("{FIELD_ID}.{index}"));
        }
        cx.paint_component(&viewport, Rect::new(body.x, body.y, width, body.height));
        (retained.get(), content_height)
    };
    state.scroll = scroll;
    state.reveal_pending = false;
    state.content_height = content_height;
    state.viewport_height = u64::from(body.height);
    if overflow {
        let scrollbar_state = Cell::new(
            ScrollbarState::new(
                u16::try_from(content_height).unwrap_or(u16::MAX),
                body.height,
            )
            .offset(u16::try_from(scroll.vertical_offset()).unwrap_or(u16::MAX)),
        );
        let scrollbar = ScrollbarComponent::new("prompt.form.scrollbar", &scrollbar_state)
            .policy(ScrollbarPolicy::bare())
            .styles(ScrollbarStyles {
                begin: theme.muted,
                track: theme.muted,
                thumb: theme.accent,
                end: theme.muted,
            });
        cx.paint_component(
            &scrollbar,
            Rect::new(body.right().saturating_sub(1), body.y, 1, body.height),
        );
    }
}
