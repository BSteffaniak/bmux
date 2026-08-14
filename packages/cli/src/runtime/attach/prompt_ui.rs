use super::input::{TerminalGeometry, TerminalKeyEvent, TerminalMouseEvent};
use super::render::opaque_row_text;
use super::state::AttachCursorState;
use super::tui_surface::{buffer_render_ops, component_theme, surface_buffer};
use crate::runtime::prompt::{
    PromptField, PromptFormField, PromptFormFieldKind, PromptFormValue, PromptHostRequest,
    PromptOption, PromptPolicy, PromptRequest, PromptResponse, PromptValue,
};
use anyhow::Result;
use bmux_appearance::RuntimeAppearance;
use bmux_attach_layout_protocol::{
    AttachLayer as SurfaceLayer, AttachRect, AttachSurface, AttachSurfaceKind,
};
use bmux_plugin::RenderOp;
use bmux_text_edit::{TextDelete, TextEditBuffer, TextMotion};
use bmux_tui::chrome::Panel;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::hit::HitMap;
use bmux_tui::palette::{CommandPalette, CommandPaletteState, PaletteItem};
use bmux_tui::prelude::{Line, Span};
use bmux_tui_components::modal_frame::{ModalFrame, ModalSizing};
use bmux_tui_components::scrollbar::{Scrollbar, ScrollbarPolicy, ScrollbarState, ScrollbarStyles};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tokio::sync::oneshot;
use uuid::Uuid;

const PROMPT_OVERLAY_SURFACE_ID: Uuid = Uuid::from_u128(2);

#[derive(Debug, Clone)]
pub struct AttachPromptOverlayRender {
    pub surface: AttachSurface,
    pub ops: Vec<RenderOp>,
    pub cursor_state: Option<AttachCursorState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachCloseFallbackTarget {
    Context { context_id: Uuid },
    Session { session_id: Uuid },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachInternalPromptAction {
    QuitSession,
    CloseWindow {
        context_id: Uuid,
    },
    ClosePane {
        pane_id: Uuid,
    },
    CloseLastPaneAndSwitch {
        old_session_id: Uuid,
        target: AttachCloseFallbackTarget,
    },
    FinalPaneAction {
        pane_id: Uuid,
        session_id: Uuid,
    },
}

#[derive(Debug)]
pub enum AttachPromptOrigin {
    External {
        response_tx: oneshot::Sender<PromptResponse>,
        event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::runtime::prompt::PromptEvent>>,
    },
    Internal(AttachInternalPromptAction),
}

#[derive(Debug)]
pub struct AttachPromptCompletion {
    pub origin: AttachPromptOrigin,
    pub response: PromptResponse,
}

#[derive(Debug)]
struct AttachPromptEnvelope {
    request: PromptRequest,
    origin: AttachPromptOrigin,
}

#[derive(Debug)]
enum PromptWidgetState {
    Confirm {
        selected_yes: bool,
    },
    TextInput {
        buffer: TextEditBuffer,
        /// Inline validation error shown when the user tries to submit
        /// invalid input.  Cleared on the next keystroke.
        error: Option<String>,
    },
    SingleSelect {
        selected: usize,
        scroll: usize,
    },
    SearchSelect {
        palette: CommandPaletteState,
    },
    MultiToggle {
        cursor: usize,
        selected: BTreeSet<usize>,
        scroll: usize,
    },
    Form {
        cursor: usize,
        scroll: usize,
        values: BTreeMap<String, PromptFormValue>,
        editors: BTreeMap<String, TextEditBuffer>,
        errors: BTreeMap<String, String>,
    },
}

#[derive(Debug)]
struct ActivePrompt {
    envelope: AttachPromptEnvelope,
    state: PromptWidgetState,
    message_wrap_width: Option<usize>,
    message_wrapped_lines: Vec<String>,
    hits: HitMap,
}

impl ActivePrompt {
    fn from_envelope(envelope: AttachPromptEnvelope) -> Self {
        let state = match &envelope.request.field {
            PromptField::Confirm { default, .. } => PromptWidgetState::Confirm {
                selected_yes: *default,
            },
            PromptField::TextInput { initial_value, .. } => PromptWidgetState::TextInput {
                buffer: TextEditBuffer::from_text(initial_value.clone()),
                error: None,
            },
            PromptField::SingleSelect {
                options,
                default_index,
                ..
            } => {
                let selected = if options.is_empty() {
                    0
                } else {
                    (*default_index).min(options.len().saturating_sub(1))
                };
                PromptWidgetState::SingleSelect {
                    selected,
                    scroll: 0,
                }
            }
            PromptField::SearchSelect {
                options,
                default_index,
                ..
            } => {
                let selected = if options.is_empty() {
                    0
                } else {
                    (*default_index).min(options.len().saturating_sub(1))
                };
                let mut palette = CommandPaletteState::default();
                palette.list.selected = Some(selected);
                PromptWidgetState::SearchSelect { palette }
            }
            PromptField::MultiToggle {
                options,
                default_indices,
                ..
            } => {
                let selected = default_indices
                    .iter()
                    .copied()
                    .filter(|index| *index < options.len())
                    .collect::<BTreeSet<_>>();
                PromptWidgetState::MultiToggle {
                    cursor: 0,
                    selected,
                    scroll: 0,
                }
            }
            PromptField::Form { sections, .. } => PromptWidgetState::Form {
                cursor: 0,
                scroll: 0,
                values: initial_form_values(sections),
                editors: initial_form_editors(sections),
                errors: BTreeMap::new(),
            },
        };
        Self {
            envelope,
            state,
            message_wrap_width: None,
            message_wrapped_lines: Vec::new(),
            hits: HitMap::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct AttachPromptState {
    queue: VecDeque<AttachPromptEnvelope>,
    active: Option<ActivePrompt>,
}

pub enum PromptKeyDisposition {
    NotActive,
    Consumed,
    Completed(AttachPromptCompletion),
}

impl AttachPromptState {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active.is_some()
    }

    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }

    #[must_use]
    pub fn active_hint(&self) -> Option<&'static str> {
        let active = self.active.as_ref()?;
        let hint = match active.envelope.request.field {
            PromptField::Confirm { .. } => "Prompt | <-/-> choose | Enter submit | Esc cancel",
            PromptField::TextInput { .. } => "Prompt | type text | Enter submit | Esc cancel",
            PromptField::SingleSelect { .. } => {
                "Prompt | Up/Down choose | Enter submit | Esc cancel"
            }
            PromptField::SearchSelect { .. } => {
                "Prompt | type to search | Up/Down choose | Enter submit | Esc cancel"
            }
            PromptField::MultiToggle { .. } => {
                "Prompt | Up/Down move | Space toggle | Enter submit | Esc cancel"
            }
            PromptField::Form { .. } => {
                "Prompt | Up/Down move | Space edit/toggle | Enter apply | Esc cancel"
            }
        };
        Some(hint)
    }

    pub fn enqueue_external(&mut self, host_request: PromptHostRequest) {
        self.enqueue(AttachPromptEnvelope {
            request: host_request.request,
            origin: AttachPromptOrigin::External {
                response_tx: host_request.response_tx,
                event_tx: host_request.event_tx,
            },
        });
    }

    pub fn enqueue_internal(&mut self, request: PromptRequest, action: AttachInternalPromptAction) {
        self.enqueue(AttachPromptEnvelope {
            request,
            origin: AttachPromptOrigin::Internal(action),
        });
    }

    pub fn handle_paste(&mut self, text: &str) -> PromptKeyDisposition {
        let Some(active) = self.active.as_mut() else {
            return PromptKeyDisposition::NotActive;
        };

        match (&active.envelope.request.field, &mut active.state) {
            (PromptField::TextInput { .. }, PromptWidgetState::TextInput { buffer, error }) => {
                buffer.paste(text);
                *error = None;
            }
            (
                PromptField::SearchSelect { options, .. },
                PromptWidgetState::SearchSelect { palette },
            ) => {
                palette.query.paste(text);
                let len = filtered_option_indices(options, palette.query.text()).len();
                let selected = palette
                    .list
                    .selected
                    .unwrap_or(0)
                    .min(len.saturating_sub(1));
                palette.list.selected = (!options.is_empty()).then_some(selected);
                palette.list.offset = palette.list.offset.min(selected);
            }
            (
                PromptField::Form {
                    sections,
                    live_preview,
                },
                PromptWidgetState::Form {
                    cursor,
                    values,
                    editors,
                    errors,
                    ..
                },
            ) => {
                let fields = flatten_form_fields(sections);
                if let Some(field) = fields.get(*cursor)
                    && !field.disabled
                    && paste_form_text(field, values, editors, text)
                {
                    errors.remove(&field.id);
                    if *live_preview {
                        emit_form_changed(&active.envelope, field, values);
                    }
                }
            }
            _ => {}
        }

        PromptKeyDisposition::Consumed
    }

    #[allow(clippy::too_many_lines)] // Prompt key handling is a compact state machine.
    pub fn handle_key_event(&mut self, key: &KeyEvent) -> PromptKeyDisposition {
        if self.active.is_none() {
            return PromptKeyDisposition::NotActive;
        }
        if !prompt_accepts_key_kind(key.kind) {
            return PromptKeyDisposition::Consumed;
        }

        if matches!(key.code, KeyCode::Esc)
            && self
                .active
                .as_ref()
                .is_some_and(|active| active.envelope.request.esc_cancels)
        {
            return self.complete_active(PromptResponse::Cancelled);
        }

        let mut completion: Option<PromptResponse> = None;
        if let Some(active) = self.active.as_mut() {
            match (&active.envelope.request.field, &mut active.state) {
                (
                    PromptField::Confirm {
                        yes_label: _,
                        no_label: _,
                        ..
                    },
                    PromptWidgetState::Confirm { selected_yes },
                ) => match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        *selected_yes = true;
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        *selected_yes = false;
                    }
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Char(' ') => {
                        *selected_yes = !*selected_yes;
                    }
                    KeyCode::Char('y' | 'Y') => {
                        *selected_yes = true;
                        completion = Some(PromptResponse::Submitted(PromptValue::Confirm(true)));
                    }
                    KeyCode::Char('n' | 'N') => {
                        *selected_yes = false;
                        completion = Some(PromptResponse::Submitted(PromptValue::Confirm(false)));
                    }
                    KeyCode::Enter => {
                        completion = Some(PromptResponse::Submitted(PromptValue::Confirm(
                            *selected_yes,
                        )));
                    }
                    _ => {}
                },
                (
                    PromptField::TextInput {
                        required,
                        placeholder: _,
                        initial_value: _,
                        validation,
                    },
                    PromptWidgetState::TextInput { buffer, error },
                ) => match key.code {
                    KeyCode::Left
                        if key
                            .modifiers
                            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                    {
                        buffer.move_cursor(TextMotion::WordLeft);
                    }
                    KeyCode::Right
                        if key
                            .modifiers
                            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                    {
                        buffer.move_cursor(TextMotion::WordRight);
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        buffer.move_cursor(TextMotion::Start);
                    }
                    KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        buffer.move_cursor(TextMotion::End);
                    }
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        buffer.insert_char(ch);
                        *error = None;
                    }
                    KeyCode::Backspace => {
                        buffer.delete_backward();
                        *error = None;
                    }
                    KeyCode::Delete => {
                        buffer.delete_forward();
                        *error = None;
                    }
                    KeyCode::Left => {
                        buffer.move_cursor(TextMotion::Left);
                    }
                    KeyCode::Right => {
                        buffer.move_cursor(TextMotion::Right);
                    }
                    KeyCode::Home => {
                        buffer.move_cursor(TextMotion::Start);
                    }
                    KeyCode::End => {
                        buffer.move_cursor(TextMotion::End);
                    }
                    KeyCode::Enter => {
                        if *required && buffer.text().trim().is_empty() {
                            *error = Some("value is required".to_string());
                            return PromptKeyDisposition::Consumed;
                        }
                        if (!buffer.text().trim().is_empty() || *required)
                            && let Some(rule) = validation
                            && let Err(msg) = run_prompt_validation(rule, buffer.text())
                        {
                            *error = Some(msg);
                            return PromptKeyDisposition::Consumed;
                        }
                        completion = Some(PromptResponse::Submitted(PromptValue::Text(
                            buffer.text().to_string(),
                        )));
                    }
                    _ => {}
                },
                (
                    PromptField::SingleSelect {
                        options,
                        live_preview,
                        ..
                    },
                    PromptWidgetState::SingleSelect { selected, scroll },
                ) => {
                    let previous_selected = *selected;
                    if options.is_empty() {
                        if key.code == KeyCode::Enter {
                            completion = Some(PromptResponse::Submitted(PromptValue::Single(
                                String::new(),
                            )));
                        }
                    } else {
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => {
                                *selected = selected.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                *selected = selected
                                    .saturating_add(1)
                                    .min(options.len().saturating_sub(1));
                            }
                            KeyCode::Home => {
                                *selected = 0;
                            }
                            KeyCode::End => {
                                *selected = options.len().saturating_sub(1);
                            }
                            KeyCode::Enter => {
                                let value = options
                                    .get(*selected)
                                    .map_or_else(String::new, |option| option.value.clone());
                                completion =
                                    Some(PromptResponse::Submitted(PromptValue::Single(value)));
                            }
                            _ => {}
                        }
                        *scroll = (*scroll).min(*selected);
                        if *live_preview && *selected != previous_selected {
                            emit_selection_changed(&active.envelope, *selected);
                        }
                    }
                }
                (
                    PromptField::SearchSelect {
                        options,
                        live_preview,
                        ..
                    },
                    PromptWidgetState::SearchSelect { palette },
                ) => {
                    let query = &mut palette.query;
                    let selected = palette.list.selected.get_or_insert(0);
                    let scroll = &mut palette.list.offset;
                    let previous_selected_value = filtered_option_indices(options, query.text())
                        .get(*selected)
                        .and_then(|index| options.get(*index))
                        .map(|option| option.value.clone());
                    match key.code {
                        KeyCode::Char(ch)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            query.insert_char(ch);
                            *selected = 0;
                            *scroll = 0;
                        }
                        KeyCode::Backspace => {
                            query.delete_backward();
                            *selected = 0;
                            *scroll = 0;
                        }
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            query.clear();
                            *selected = 0;
                            *scroll = 0;
                        }
                        KeyCode::Left
                            if key
                                .modifiers
                                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                        {
                            query.move_cursor(TextMotion::WordLeft);
                        }
                        KeyCode::Right
                            if key
                                .modifiers
                                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                        {
                            query.move_cursor(TextMotion::WordRight);
                        }
                        KeyCode::Left => {
                            query.move_cursor(TextMotion::Left);
                        }
                        KeyCode::Right => {
                            query.move_cursor(TextMotion::Right);
                        }
                        KeyCode::Delete => {
                            query.delete_forward();
                            *selected = 0;
                            *scroll = 0;
                        }
                        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            query.move_cursor(TextMotion::Start);
                        }
                        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            query.move_cursor(TextMotion::End);
                        }
                        KeyCode::Up | KeyCode::Char('p')
                            if matches!(key.code, KeyCode::Up)
                                || key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            *selected = selected.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('n')
                            if matches!(key.code, KeyCode::Down)
                                || key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            let len = filtered_option_indices(options, query.text()).len();
                            *selected = selected.saturating_add(1).min(len.saturating_sub(1));
                        }
                        KeyCode::Home => {
                            *selected = 0;
                        }
                        KeyCode::End => {
                            let len = filtered_option_indices(options, query.text()).len();
                            *selected = len.saturating_sub(1);
                        }
                        KeyCode::Enter => {
                            let filtered = filtered_option_indices(options, query.text());
                            if let Some(option) = filtered
                                .get(*selected)
                                .and_then(|index| options.get(*index))
                            {
                                completion = Some(PromptResponse::Submitted(PromptValue::Single(
                                    option.value.clone(),
                                )));
                            }
                        }
                        _ => {}
                    }
                    let filtered = filtered_option_indices(options, query.text());
                    *selected = (*selected).min(filtered.len().saturating_sub(1));
                    *scroll = (*scroll).min(*selected);
                    if *live_preview {
                        let selected_value = filtered
                            .get(*selected)
                            .and_then(|index| options.get(*index))
                            .map(|option| option.value.clone());
                        if selected_value != previous_selected_value
                            && let Some(option_index) = filtered.get(*selected)
                        {
                            emit_selection_changed(&active.envelope, *option_index);
                        }
                    }
                }
                (
                    PromptField::MultiToggle {
                        options,
                        min_selected,
                        ..
                    },
                    PromptWidgetState::MultiToggle {
                        cursor,
                        selected,
                        scroll,
                    },
                ) => {
                    let len = options.len();
                    if len == 0 {
                        if matches!(key.code, KeyCode::Enter) && selected.len() >= *min_selected {
                            completion =
                                Some(PromptResponse::Submitted(PromptValue::Multi(Vec::new())));
                        }
                    } else {
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => {
                                *cursor = cursor.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                *cursor = cursor.saturating_add(1).min(len.saturating_sub(1));
                            }
                            KeyCode::Home => {
                                *cursor = 0;
                            }
                            KeyCode::End => {
                                *cursor = len.saturating_sub(1);
                            }
                            KeyCode::Char(' ') => {
                                if selected.contains(cursor) {
                                    selected.remove(cursor);
                                } else {
                                    selected.insert(*cursor);
                                }
                            }
                            KeyCode::Enter => {
                                if selected.len() < *min_selected {
                                    return PromptKeyDisposition::Consumed;
                                }
                                let mut values = selected
                                    .iter()
                                    .filter_map(|index| {
                                        options.get(*index).map(|option| option.value.clone())
                                    })
                                    .collect::<Vec<_>>();
                                values.sort();
                                completion =
                                    Some(PromptResponse::Submitted(PromptValue::Multi(values)));
                            }
                            _ => {}
                        }
                        *scroll = (*scroll).min(*cursor);
                    }
                }
                (
                    PromptField::Form {
                        sections,
                        live_preview,
                    },
                    PromptWidgetState::Form {
                        cursor,
                        scroll,
                        values,
                        editors,
                        errors,
                    },
                ) => {
                    let fields = flatten_form_fields(sections);
                    let len = fields.len();
                    if len == 0 {
                        if matches!(key.code, KeyCode::Enter) {
                            completion =
                                Some(PromptResponse::Submitted(PromptValue::Form(values.clone())));
                        }
                    } else {
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => {
                                *cursor = cursor.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                *cursor = cursor.saturating_add(1).min(len.saturating_sub(1));
                            }
                            KeyCode::Home => {
                                *cursor = 0;
                            }
                            KeyCode::End => {
                                *cursor = len.saturating_sub(1);
                            }
                            KeyCode::Char(' ') => {
                                if let Some(field) = fields.get(*cursor)
                                    && !field.disabled
                                {
                                    cycle_form_value(field, values);
                                    errors.remove(&field.id);
                                    if *live_preview {
                                        emit_form_changed(&active.envelope, field, values);
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                errors.clear();
                                for field in &fields {
                                    if let Err(message) = validate_form_field(field, values) {
                                        errors.insert(field.id.clone(), message);
                                    }
                                }
                                if errors.is_empty() {
                                    completion = Some(PromptResponse::Submitted(
                                        PromptValue::Form(values.clone()),
                                    ));
                                }
                            }
                            KeyCode::Char(ch)
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                if let Some(field) = fields.get(*cursor)
                                    && !field.disabled
                                    && edit_form_text(
                                        field,
                                        values,
                                        editors,
                                        FormEditAction::Insert(ch),
                                    )
                                {
                                    errors.remove(&field.id);
                                    if *live_preview {
                                        emit_form_changed(&active.envelope, field, values);
                                    }
                                }
                            }
                            KeyCode::Backspace => {
                                if let Some(field) = fields.get(*cursor)
                                    && !field.disabled
                                    && edit_form_text(
                                        field,
                                        values,
                                        editors,
                                        FormEditAction::Delete(TextDelete::Backward),
                                    )
                                {
                                    errors.remove(&field.id);
                                    if *live_preview {
                                        emit_form_changed(&active.envelope, field, values);
                                    }
                                }
                            }
                            KeyCode::Delete => {
                                if let Some(field) = fields.get(*cursor)
                                    && !field.disabled
                                    && edit_form_text(
                                        field,
                                        values,
                                        editors,
                                        FormEditAction::Delete(TextDelete::Forward),
                                    )
                                {
                                    errors.remove(&field.id);
                                    if *live_preview {
                                        emit_form_changed(&active.envelope, field, values);
                                    }
                                }
                            }
                            KeyCode::Left
                                if key
                                    .modifiers
                                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                            {
                                if let Some(field) = fields.get(*cursor)
                                    && !field.disabled
                                {
                                    edit_form_text(
                                        field,
                                        values,
                                        editors,
                                        FormEditAction::Move(TextMotion::WordLeft),
                                    );
                                }
                            }
                            KeyCode::Right
                                if key
                                    .modifiers
                                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                            {
                                if let Some(field) = fields.get(*cursor)
                                    && !field.disabled
                                {
                                    edit_form_text(
                                        field,
                                        values,
                                        editors,
                                        FormEditAction::Move(TextMotion::WordRight),
                                    );
                                }
                            }
                            KeyCode::Left => {
                                if let Some(field) = fields.get(*cursor)
                                    && !field.disabled
                                {
                                    edit_form_text(
                                        field,
                                        values,
                                        editors,
                                        FormEditAction::Move(TextMotion::Left),
                                    );
                                }
                            }
                            KeyCode::Right => {
                                if let Some(field) = fields.get(*cursor)
                                    && !field.disabled
                                {
                                    edit_form_text(
                                        field,
                                        values,
                                        editors,
                                        FormEditAction::Move(TextMotion::Right),
                                    );
                                }
                            }
                            _ => {}
                        }
                        *scroll = (*scroll).min(*cursor);
                    }
                }
                _ => {}
            }
        }

        if let Some(response) = completion {
            return self.complete_active(response);
        }

        PromptKeyDisposition::Consumed
    }

    pub fn handle_terminal_key_event(&mut self, key: &TerminalKeyEvent) -> PromptKeyDisposition {
        key.to_crossterm()
            .map_or(PromptKeyDisposition::Consumed, |key| {
                self.handle_key_event(&key)
            })
    }

    pub fn handle_terminal_mouse_event(
        &mut self,
        mouse: TerminalMouseEvent,
        geometry: TerminalGeometry,
    ) -> PromptKeyDisposition {
        mouse
            .to_crossterm()
            .map_or(PromptKeyDisposition::Consumed, |mouse| {
                self.handle_mouse_event(mouse, geometry)
            })
    }

    pub fn handle_mouse_event(
        &mut self,
        mouse: MouseEvent,
        geometry: TerminalGeometry,
    ) -> PromptKeyDisposition {
        let Some(layout) = prompt_overlay_layout(
            self.active.as_ref().map(|active| &active.envelope.request),
            geometry,
        ) else {
            return PromptKeyDisposition::NotActive;
        };
        let Some(active) = self.active.as_mut() else {
            return PromptKeyDisposition::NotActive;
        };

        if active.envelope.request.modal_id.as_deref() == Some("command-palette") {
            let Some(hit) = active.hits.hit_test(Point::new(mouse.column, mouse.row)) else {
                return PromptKeyDisposition::Consumed;
            };
            let Some(source_index) = CommandPalette::hit_item_index(hit.id(), "command") else {
                return PromptKeyDisposition::Consumed;
            };
            let PromptField::SearchSelect {
                options,
                live_preview,
                ..
            } = &active.envelope.request.field
            else {
                return PromptKeyDisposition::Consumed;
            };
            let PromptWidgetState::SearchSelect { palette } = &mut active.state else {
                return PromptKeyDisposition::Consumed;
            };
            let filtered = filtered_option_indices(options, palette.query.text());
            let Some(filtered_index) = filtered.iter().position(|index| *index == source_index)
            else {
                return PromptKeyDisposition::Consumed;
            };
            let previous = palette.list.selected;
            palette.list.selected = Some(filtered_index);
            if *live_preview && previous != Some(filtered_index) {
                emit_selection_changed(&active.envelope, source_index);
            }
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                let value = options
                    .get(source_index)
                    .map_or_else(String::new, |option| option.value.clone());
                return self.complete_active(PromptResponse::Submitted(PromptValue::Single(value)));
            }
            return PromptKeyDisposition::Consumed;
        }

        let PromptField::SingleSelect {
            options,
            live_preview,
            ..
        } = &active.envelope.request.field
        else {
            return PromptKeyDisposition::Consumed;
        };
        let PromptWidgetState::SingleSelect { selected, scroll } = &mut active.state else {
            return PromptKeyDisposition::Consumed;
        };
        if options.is_empty() {
            return PromptKeyDisposition::Consumed;
        }

        let width = usize::from(layout.surface.rect.w);
        let height = usize::from(layout.surface.rect.h);
        let x = usize::from(layout.surface.rect.x);
        let text_width = width.saturating_sub(4);
        let body_rows = height.saturating_sub(4).max(1);
        let message_rows = active
            .envelope
            .request
            .message
            .as_ref()
            .map_or(0, |message| wrap_lines(message, text_width).len());
        let field_rows = body_rows.saturating_sub(message_rows).max(1);
        *scroll = adjust_scroll(*scroll, *selected, options.len(), field_rows);

        let body_y = usize::from(layout.surface.rect.y).saturating_add(1);
        let field_y = body_y.saturating_add(message_rows);
        let column = usize::from(mouse.column);
        if column <= x || column >= x.saturating_add(width).saturating_sub(1) {
            return PromptKeyDisposition::Consumed;
        }
        let row = usize::from(mouse.row);
        if row < field_y || row >= field_y.saturating_add(field_rows) {
            return PromptKeyDisposition::Consumed;
        }
        let option_index = scroll.saturating_add(row.saturating_sub(field_y));
        if option_index >= options.len() {
            return PromptKeyDisposition::Consumed;
        }

        let previous = *selected;
        *selected = option_index;
        if *live_preview && previous != *selected {
            emit_selection_changed(&active.envelope, *selected);
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            let value = options
                .get(*selected)
                .map_or_else(String::new, |option| option.value.clone());
            return self.complete_active(PromptResponse::Submitted(PromptValue::Single(value)));
        }

        PromptKeyDisposition::Consumed
    }

    #[must_use]
    pub fn overlay_surface(&self, geometry: TerminalGeometry) -> Option<AttachSurface> {
        prompt_overlay_layout(
            self.active.as_ref().map(|active| &active.envelope.request),
            geometry,
        )
        .map(|layout| layout.surface)
    }

    #[allow(clippy::cast_possible_truncation)] // Overlay coordinates are bounded by explicit terminal geometry.
    pub fn attach_prompt_overlay_render(
        &mut self,
        geometry: TerminalGeometry,
        appearance: &RuntimeAppearance,
    ) -> Option<AttachPromptOverlayRender> {
        let layout = prompt_overlay_layout(
            self.active.as_ref().map(|active| &active.envelope.request),
            geometry,
        )?;
        let active = self.active.as_mut()?;

        let width = usize::from(layout.surface.rect.w);
        let height = usize::from(layout.surface.rect.h);
        let body_rows = height.saturating_sub(4).max(1);
        let text_width = width.saturating_sub(4);

        let body = render_prompt_body(active, text_width, body_rows);
        let footer = prompt_footer_text(&active.envelope.request);
        let area = Rect::new(
            layout.surface.rect.x,
            layout.surface.rect.y,
            layout.surface.rect.w,
            layout.surface.rect.h,
        );
        let theme = component_theme(appearance).modal_theme();
        let compact = layout.surface.rect.w < 24 || layout.surface.rect.h < 8;
        let modal = ModalFrame::new(
            ModalSizing::fixed(
                Size::new(layout.surface.rect.w, layout.surface.rect.h),
                Insets::new(0, 0, 0, 0),
            ),
            theme,
        )
        .title(Line::raw(active.envelope.request.title.clone()))
        .padding(if compact {
            Insets::new(0, 0, 0, 0)
        } else {
            Insets::new(0, 1, 0, 1)
        });
        let mut buffer = surface_buffer(area);
        let mut frame = Frame::new(&mut buffer);
        modal.render(area, &mut frame);
        let content = modal.content_area(area);
        let mut component_cursor = None;
        let rendered_palette =
            if active.envelope.request.modal_id.as_deref() == Some("command-palette") {
                render_command_palette(active, content, &mut frame, theme)
            } else {
                false
            };
        if rendered_palette {
            component_cursor = frame.cursor().map(|cursor| AttachCursorState {
                x: cursor.position.x,
                y: cursor.position.y,
                visible: cursor.visible,
            });
        } else {
            for (index, line) in body.lines.iter().take(body_rows).enumerate() {
                let row = content
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
                if row >= content.bottom().saturating_sub(1) {
                    break;
                }
                frame.write_line_with_fallback_style(
                    Rect::new(content.x, row, content.width, 1),
                    &prompt_body_line(&active.envelope.request, active, index, line, theme),
                    theme.text,
                );
            }
            let footer_y = content.bottom().saturating_sub(1);
            frame.write_line_with_fallback_style(
                Rect::new(content.x, footer_y, content.width, 1),
                &Line::raw(opaque_row_text(&footer, usize::from(content.width))),
                theme.muted,
            );
        }
        let ops = buffer_render_ops(&buffer);

        let cursor_state = component_cursor.or_else(|| {
            body.cursor.map(|(row, col)| AttachCursorState {
                x: (usize::from(content.x) + col).min(u16::MAX as usize) as u16,
                y: (usize::from(content.y) + row).min(u16::MAX as usize) as u16,
                visible: true,
            })
        });

        Some(AttachPromptOverlayRender {
            surface: layout.surface,
            ops,
            cursor_state,
        })
    }

    fn enqueue(&mut self, envelope: AttachPromptEnvelope) {
        match envelope.request.policy {
            PromptPolicy::Enqueue => {
                self.queue.push_back(envelope);
            }
            PromptPolicy::RejectIfBusy => {
                if self.is_busy() {
                    send_response(envelope.origin, PromptResponse::RejectedBusy);
                    return;
                }
                self.queue.push_back(envelope);
            }
            PromptPolicy::ReplaceActive => {
                if let Some(active) = self.active.take() {
                    send_response(active.envelope.origin, PromptResponse::Cancelled);
                }
                self.queue.push_front(envelope);
            }
        }
        self.activate_next();
    }

    fn activate_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        if let Some(next) = self.queue.pop_front() {
            self.active = Some(ActivePrompt::from_envelope(next));
        }
    }

    fn complete_active(&mut self, response: PromptResponse) -> PromptKeyDisposition {
        let Some(active) = self.active.take() else {
            return PromptKeyDisposition::NotActive;
        };
        self.activate_next();
        PromptKeyDisposition::Completed(AttachPromptCompletion {
            origin: active.envelope.origin,
            response,
        })
    }
}

#[allow(clippy::too_many_lines)] // Palette composition keeps one layout source for rendering, hits, cursor, footer, and scrollbar.
fn render_command_palette(
    active: &mut ActivePrompt,
    content: Rect,
    frame: &mut Frame<'_>,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> bool {
    let PromptField::SearchSelect {
        options,
        placeholder,
        ..
    } = &active.envelope.request.field
    else {
        return false;
    };
    let PromptWidgetState::SearchSelect { palette } = &mut active.state else {
        return false;
    };
    let items = options
        .iter()
        .map(|option| {
            let mut spans = vec![Span::styled(option.label.clone(), theme.text)];
            if let Some(key_hint) = &option.key_hint {
                spans.push(Span::styled(format!("  {key_hint}"), theme.focused));
            }
            if let Some(detail) = &option.detail {
                spans.push(Span::styled(format!("  —  {detail}"), theme.muted));
            }
            PaletteItem::new(option.value.clone(), Line::from_spans(spans)).search_text(format!(
                "{} {} {}",
                option.label,
                option.detail.as_deref().unwrap_or_default(),
                option.key_hint.as_deref().unwrap_or_default()
            ))
        })
        .collect::<Vec<_>>();
    let filtered = filtered_option_indices(options, palette.query.text());
    let message_rows = active
        .envelope
        .request
        .message
        .as_ref()
        .map_or(0, |message| {
            wrap_lines(message, usize::from(content.width)).len()
        });
    let footer_rows = u16::from(content.height > 4);
    let palette_y = content
        .y
        .saturating_add(u16::try_from(message_rows).unwrap_or(u16::MAX));
    let palette_height = content
        .bottom()
        .saturating_sub(palette_y)
        .saturating_sub(footer_rows);
    if let Some(message) = &active.envelope.request.message {
        for (row, line) in wrap_lines(message, usize::from(content.width))
            .into_iter()
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                break;
            };
            frame.write_line_with_fallback_style(
                Rect::new(content.x, content.y.saturating_add(row), content.width, 1),
                &Line::raw(line),
                theme.muted,
            );
        }
    }
    let palette_area = Rect::new(content.x, palette_y, content.width, palette_height);
    let list_viewport = palette_height.saturating_sub(2);
    let show_scrollbar = filtered.len() > usize::from(list_viewport) && content.width > 1;
    let component_area = Rect::new(
        palette_area.x,
        palette_area.y,
        palette_area.width.saturating_sub(u16::from(show_scrollbar)),
        palette_area.height,
    );
    let component = CommandPalette::new(&items)
        .panel(Panel::new().background(theme.background))
        .placeholder(
            placeholder
                .clone()
                .unwrap_or_else(|| "Search commands".to_owned()),
        )
        .list_styles(theme.text, theme.focused);
    active.hits = HitMap::new();
    component.register_projected_hits(
        component_area,
        palette,
        &filtered,
        &mut active.hits,
        "command",
    );
    component.render_projected(component_area, frame, palette, &filtered);
    if show_scrollbar {
        let scrollbar_area = Rect::new(
            palette_area.right().saturating_sub(1),
            palette_area.y.saturating_add(2),
            1,
            list_viewport,
        );
        let scrollbar_state = ScrollbarState::new(
            u16::try_from(filtered.len()).unwrap_or(u16::MAX),
            list_viewport,
        )
        .offset(u16::try_from(palette.list.offset).unwrap_or(u16::MAX));
        Scrollbar::new()
            .policy(ScrollbarPolicy::bare())
            .styles(ScrollbarStyles {
                begin: theme.muted,
                track: theme.muted,
                thumb: theme.focused,
                end: theme.muted,
            })
            .render(scrollbar_area, &scrollbar_state, frame);
    }
    if footer_rows > 0 {
        let selected = palette.list.selected.map_or(0, |index| index + 1);
        let footer = format!(
            "{selected}/{} matches  •  ↑↓ navigate  •  Enter run  •  Esc close",
            filtered.len()
        );
        frame.write_line_with_fallback_style(
            Rect::new(
                content.x,
                content.bottom().saturating_sub(1),
                content.width,
                1,
            ),
            &Line::raw(footer),
            theme.muted,
        );
    }
    true
}

fn prompt_body_line(
    request: &PromptRequest,
    active: &ActivePrompt,
    index: usize,
    rendered: &str,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> Line {
    let is_palette = request.modal_id.as_deref() == Some("command-palette");
    if !is_palette {
        return Line::raw(rendered.to_owned());
    }
    let message_rows = request
        .message
        .as_ref()
        .map_or(0, |message| message.lines().count().max(1));
    let list_row = index.checked_sub(message_rows.saturating_add(1));
    let selected = matches!(
        (&active.state, list_row),
        (
            PromptWidgetState::SearchSelect { palette },
            Some(row)
        ) if row == palette
            .list
            .selected
            .unwrap_or(0)
            .saturating_sub(palette.list.offset)
    );
    if selected {
        Line::from_spans(vec![Span::styled(rendered.to_owned(), theme.focused)])
    } else if index < message_rows {
        Line::from_spans(vec![Span::styled(rendered.to_owned(), theme.muted)])
    } else {
        Line::raw(rendered.to_owned())
    }
}

pub const fn prompt_accepts_key_kind(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn send_response(origin: AttachPromptOrigin, response: PromptResponse) {
    if let AttachPromptOrigin::External { response_tx, .. } = origin {
        let _ = response_tx.send(response);
    }
}

fn emit_selection_changed(envelope: &AttachPromptEnvelope, selected: usize) {
    let AttachPromptOrigin::External {
        event_tx: Some(event_tx),
        ..
    } = &envelope.origin
    else {
        return;
    };
    if let PromptField::SingleSelect { options, .. } | PromptField::SearchSelect { options, .. } =
        &envelope.request.field
        && let Some(option) = options.get(selected)
    {
        let _ = event_tx.send(crate::runtime::prompt::PromptEvent::SelectionChanged {
            index: selected,
            value: option.value.clone(),
        });
    }
}

fn emit_form_changed(
    envelope: &AttachPromptEnvelope,
    field: &PromptFormField,
    values: &BTreeMap<String, PromptFormValue>,
) {
    let AttachPromptOrigin::External {
        event_tx: Some(event_tx),
        ..
    } = &envelope.origin
    else {
        return;
    };
    if let Some(value) = values.get(&field.id) {
        let _ = event_tx.send(crate::runtime::prompt::PromptEvent::FormChanged {
            field_id: field.id.clone(),
            value: value.clone(),
            values: values.clone(),
        });
    }
}

fn flatten_form_fields(
    sections: &[crate::runtime::prompt::PromptFormSection],
) -> Vec<&PromptFormField> {
    sections
        .iter()
        .flat_map(|section| section.fields.iter())
        .collect()
}

fn initial_form_values(
    sections: &[crate::runtime::prompt::PromptFormSection],
) -> BTreeMap<String, PromptFormValue> {
    flatten_form_fields(sections)
        .into_iter()
        .map(|field| (field.id.clone(), default_form_value(field)))
        .collect()
}

fn initial_form_editors(
    sections: &[crate::runtime::prompt::PromptFormSection],
) -> BTreeMap<String, TextEditBuffer> {
    flatten_form_fields(sections)
        .into_iter()
        .filter_map(|field| match &field.kind {
            PromptFormFieldKind::Text { .. } | PromptFormFieldKind::Number { .. } => {
                values_text(&default_form_value(field))
                    .map(|value| (field.id.clone(), TextEditBuffer::from_text(value)))
            }
            _ => None,
        })
        .collect()
}

fn default_form_value(field: &PromptFormField) -> PromptFormValue {
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
            PromptFormValue::Multi(values)
        }
    }
}

fn cycle_form_value(field: &PromptFormField, values: &mut BTreeMap<String, PromptFormValue>) {
    match (&field.kind, values.get_mut(&field.id)) {
        (PromptFormFieldKind::Bool { .. }, Some(PromptFormValue::Bool(value))) => {
            *value = !*value;
        }
        (
            PromptFormFieldKind::SingleSelect { options, .. },
            Some(PromptFormValue::Single(value)),
        ) => {
            if options.is_empty() {
                return;
            }
            let current = options
                .iter()
                .position(|option| option.value == *value)
                .unwrap_or(0);
            let next = current.saturating_add(1) % options.len();
            value.clone_from(&options[next].value);
        }
        (
            PromptFormFieldKind::MultiToggle { options, .. },
            Some(PromptFormValue::Multi(selected)),
        ) => {
            if let Some(option) = options.first() {
                if selected.iter().any(|value| value == &option.value) {
                    selected.retain(|value| value != &option.value);
                } else {
                    selected.push(option.value.clone());
                    selected.sort();
                }
            }
        }
        (PromptFormFieldKind::Text { .. }, Some(PromptFormValue::Text(value)))
        | (PromptFormFieldKind::Number { .. }, Some(PromptFormValue::Number(value))) => {
            value.clear();
        }
        (PromptFormFieldKind::Integer { .. }, Some(PromptFormValue::Integer(value))) => {
            *value = 0;
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy)]
enum FormEditAction {
    Insert(char),
    Delete(TextDelete),
    Move(TextMotion),
}

fn edit_form_text(
    field: &PromptFormField,
    values: &mut BTreeMap<String, PromptFormValue>,
    editors: &mut BTreeMap<String, TextEditBuffer>,
    action: FormEditAction,
) -> bool {
    match (&field.kind, values.get_mut(&field.id)) {
        (PromptFormFieldKind::Text { .. }, Some(PromptFormValue::Text(value)))
        | (PromptFormFieldKind::Number { .. }, Some(PromptFormValue::Number(value))) => {
            let editor = editors
                .entry(field.id.clone())
                .or_insert_with(|| TextEditBuffer::from_text(value.clone()));
            apply_form_edit_action(editor, action);
            value.clear();
            value.push_str(editor.text());
            true
        }
        (PromptFormFieldKind::Integer { .. }, Some(PromptFormValue::Integer(value))) => {
            edit_form_integer(value, action)
        }
        _ => false,
    }
}

fn paste_form_text(
    field: &PromptFormField,
    values: &mut BTreeMap<String, PromptFormValue>,
    editors: &mut BTreeMap<String, TextEditBuffer>,
    text: &str,
) -> bool {
    match (&field.kind, values.get_mut(&field.id)) {
        (PromptFormFieldKind::Text { .. }, Some(PromptFormValue::Text(value)))
        | (PromptFormFieldKind::Number { .. }, Some(PromptFormValue::Number(value))) => {
            let editor = editors
                .entry(field.id.clone())
                .or_insert_with(|| TextEditBuffer::from_text(value.clone()));
            editor.paste(text);
            value.clear();
            value.push_str(editor.text());
            true
        }
        _ => false,
    }
}

fn apply_form_edit_action(editor: &mut TextEditBuffer, action: FormEditAction) {
    match action {
        FormEditAction::Insert(ch) => editor.insert_char(ch),
        FormEditAction::Delete(delete) => editor.delete(delete),
        FormEditAction::Move(motion) => editor.move_cursor(motion),
    }
}

fn edit_form_integer(value: &mut i64, action: FormEditAction) -> bool {
    let mut text = value.to_string();
    match action {
        FormEditAction::Insert('-') => {
            if text.starts_with('-') {
                text.remove(0);
            } else {
                text.insert(0, '-');
            }
        }
        FormEditAction::Insert(ch) if ch.is_ascii_digit() => {
            if text == "0" {
                text.clear();
            } else if text == "-0" {
                text = "-".to_string();
            }
            text.push(ch);
        }
        FormEditAction::Delete(TextDelete::Backward) => {
            text.pop();
        }
        FormEditAction::Delete(_) | FormEditAction::Move(_) | FormEditAction::Insert(_) => {
            return false;
        }
    }

    *value = if text.is_empty() || text == "-" {
        0
    } else if let Ok(parsed) = text.parse::<i64>() {
        parsed
    } else {
        return false;
    };
    true
}

fn validate_form_field(
    field: &PromptFormField,
    values: &BTreeMap<String, PromptFormValue>,
) -> Result<(), String> {
    if field.disabled {
        return Ok(());
    }
    let Some(value) = values.get(&field.id) else {
        return Err("missing value".to_string());
    };
    match (&field.kind, value) {
        (PromptFormFieldKind::Text { validation, .. }, PromptFormValue::Text(value)) => {
            if field.required && value.trim().is_empty() {
                return Err("value is required".to_string());
            }
            if (!value.trim().is_empty() || field.required)
                && let Some(rule) = validation
            {
                run_prompt_validation(rule, value)?;
            }
        }
        (PromptFormFieldKind::Integer { min, max, .. }, PromptFormValue::Integer(value))
            if min.is_some_and(|min| *value < min) || max.is_some_and(|max| *value > max) =>
        {
            return Err("value is out of range".to_string());
        }
        (PromptFormFieldKind::Number { min, max, .. }, PromptFormValue::Number(value)) => {
            let parsed = value
                .trim()
                .parse::<f64>()
                .map_err(|_| "value must be a number".to_string())?;
            if min
                .as_deref()
                .and_then(|min| min.parse::<f64>().ok())
                .is_some_and(|min| parsed < min)
                || max
                    .as_deref()
                    .and_then(|max| max.parse::<f64>().ok())
                    .is_some_and(|max| parsed > max)
            {
                return Err("value is out of range".to_string());
            }
        }
        (PromptFormFieldKind::MultiToggle { min_selected, .. }, PromptFormValue::Multi(values))
            if values.len() < *min_selected =>
        {
            return Err(format!("select at least {min_selected}"));
        }
        _ => {}
    }
    Ok(())
}

struct FormRenderRow {
    text: String,
    selectable: bool,
}

fn form_render_rows(
    sections: &[crate::runtime::prompt::PromptFormSection],
    values: &BTreeMap<String, PromptFormValue>,
    editors: &BTreeMap<String, TextEditBuffer>,
    errors: &BTreeMap<String, String>,
) -> Vec<FormRenderRow> {
    let mut rows = Vec::new();
    for section in sections {
        for field in &section.fields {
            let value = form_field_display(field, values, editors);
            let suffix = if field.disabled {
                field.disabled_reason.as_ref().map_or_else(
                    || " disabled".to_string(),
                    |reason| format!(" disabled: {reason}"),
                )
            } else if let Some(error) = errors.get(&field.id) {
                format!(" ! {error}")
            } else {
                String::new()
            };
            rows.push(FormRenderRow {
                text: format!("{}: {value}{suffix}", field.label),
                selectable: !field.disabled,
            });
        }
    }
    rows
}

fn form_field_display(
    field: &PromptFormField,
    values: &BTreeMap<String, PromptFormValue>,
    editors: &BTreeMap<String, TextEditBuffer>,
) -> String {
    match &field.kind {
        PromptFormFieldKind::Text { .. } | PromptFormFieldKind::Number { .. } => {
            editors.get(&field.id).map_or_else(
                || {
                    values
                        .get(&field.id)
                        .map_or_else(String::new, form_value_display)
                },
                |editor| editor.text().to_string(),
            )
        }
        _ => values
            .get(&field.id)
            .map_or_else(String::new, form_value_display),
    }
}

fn values_text(value: &PromptFormValue) -> Option<String> {
    match value {
        PromptFormValue::Text(value) | PromptFormValue::Number(value) => Some(value.clone()),
        _ => None,
    }
}

fn form_value_display(value: &PromptFormValue) -> String {
    match value {
        PromptFormValue::Bool(value) => if *value { "on" } else { "off" }.to_string(),
        PromptFormValue::Text(value)
        | PromptFormValue::Number(value)
        | PromptFormValue::Single(value) => value.clone(),
        PromptFormValue::Integer(value) => value.to_string(),
        PromptFormValue::Multi(values) => values.join(", "),
    }
}

#[allow(clippy::cast_possible_truncation)] // Overlay geometry is clamped to terminal bounds before u16 conversion.
fn prompt_overlay_layout(
    request: Option<&PromptRequest>,
    geometry: TerminalGeometry,
) -> Option<PromptOverlayLayout> {
    let request = request?;
    if geometry.cols < 8 || geometry.rows < 4 {
        return None;
    }
    let compact = geometry.cols < 24 || geometry.rows < 8;

    let content_width = prompt_estimated_width(request);
    let capped_max = request.width.max.max(request.width.min);
    let width = if compact {
        geometry.cols as usize
    } else {
        (content_width + 4)
            .max(usize::from(request.width.min.max(24)))
            .min(usize::from(capped_max.max(24)))
            .min((geometry.cols as usize).saturating_sub(2))
    };
    let estimated_lines = prompt_estimated_lines(request);
    let height = if compact {
        geometry.rows as usize
    } else {
        (estimated_lines + 4)
            .max(7)
            .min((geometry.rows as usize).saturating_sub(2))
    };
    let x = ((geometry.cols as usize).saturating_sub(width)) / 2;
    let centered_y = ((geometry.rows as usize).saturating_sub(height)) / 2;
    let y = if request.modal_id.as_deref() == Some("command-palette") {
        ((geometry.rows as usize).saturating_sub(height)) / 3
    } else {
        centered_y
    };

    Some(PromptOverlayLayout {
        surface: AttachSurface {
            id: PROMPT_OVERLAY_SURFACE_ID,
            kind: AttachSurfaceKind::Modal,
            layer: SurfaceLayer::Overlay,
            z: i32::MAX,
            rect: AttachRect {
                x: x as u16,
                y: y as u16,
                w: width as u16,
                h: height as u16,
            },
            content_rect: AttachRect {
                x: x as u16,
                y: y as u16,
                w: width as u16,
                h: height as u16,
            },
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: true,
            pane_id: None,
        },
    })
}

struct PromptOverlayLayout {
    surface: AttachSurface,
}

fn prompt_estimated_width(request: &PromptRequest) -> usize {
    let mut width = request.title.chars().count();
    if let Some(message) = &request.message {
        for line in message.lines() {
            width = width.max(line.chars().count());
        }
    }
    match &request.field {
        PromptField::Confirm {
            yes_label,
            no_label,
            ..
        } => {
            width = width.max(
                yes_label
                    .chars()
                    .count()
                    .saturating_add(no_label.chars().count())
                    .saturating_add(14),
            );
        }
        PromptField::TextInput {
            initial_value,
            placeholder,
            ..
        } => {
            width = width.max(
                initial_value
                    .chars()
                    .count()
                    .max(
                        placeholder
                            .as_ref()
                            .map_or(0, |value| value.chars().count()),
                    )
                    .saturating_add(4),
            );
        }
        PromptField::SingleSelect { options, .. }
        | PromptField::SearchSelect { options, .. }
        | PromptField::MultiToggle { options, .. } => {
            for option in options {
                width = width.max(option.label.chars().count().saturating_add(6));
            }
        }
        PromptField::Form { sections, .. } => {
            for section in sections {
                width = width.max(section.title.chars().count().saturating_add(2));
                for field in &section.fields {
                    width = width.max(field.label.chars().count().saturating_add(18));
                }
            }
        }
    }
    width
}

fn prompt_estimated_lines(request: &PromptRequest) -> usize {
    let mut lines = 0usize;
    if let Some(message) = &request.message {
        lines = lines.saturating_add(message.lines().count().max(1));
    }
    lines = lines.saturating_add(match &request.field {
        PromptField::Confirm { .. } => 1,
        // Reserve an extra line for inline validation errors.
        PromptField::TextInput { .. } => 2,
        PromptField::SingleSelect { options, .. } | PromptField::MultiToggle { options, .. } => {
            options.len().max(1)
        }
        PromptField::SearchSelect { options, .. } => options.len().saturating_add(1).max(2),
        PromptField::Form { sections, .. } => sections
            .iter()
            .map(|section| 1usize.saturating_add(section.fields.len()))
            .sum::<usize>()
            .max(1),
    });
    lines.max(1)
}

struct PromptBodyRender {
    lines: Vec<String>,
    cursor: Option<(usize, usize)>,
}

#[allow(clippy::too_many_lines)] // Field-specific rendering keeps prompt variants in one place.
fn render_prompt_body(
    active: &mut ActivePrompt,
    text_width: usize,
    body_rows: usize,
) -> PromptBodyRender {
    let mut lines = Vec::new();

    if let Some(message) = &active.envelope.request.message {
        if active.message_wrap_width != Some(text_width) {
            active.message_wrapped_lines = wrap_lines(message, text_width);
            active.message_wrap_width = Some(text_width);
        }
        lines.extend(
            active
                .message_wrapped_lines
                .iter()
                .map(|line| opaque_row_text(line, text_width)),
        );
    }

    let mut cursor = None;
    let mut field_lines = Vec::new();
    match (&active.envelope.request.field, &mut active.state) {
        (
            PromptField::Confirm {
                yes_label,
                no_label,
                ..
            },
            PromptWidgetState::Confirm { selected_yes },
        ) => {
            let yes = if *selected_yes {
                format!("> {yes_label}")
            } else {
                format!("  {yes_label}")
            };
            let no = if *selected_yes {
                format!("  {no_label}")
            } else {
                format!("> {no_label}")
            };
            let row = format!("{yes}    {no}");
            field_lines.push(opaque_row_text(
                &truncate_chars(&row, text_width),
                text_width,
            ));
        }
        (
            PromptField::TextInput { placeholder, .. },
            PromptWidgetState::TextInput { buffer, error },
        ) => {
            let visible_width = text_width.saturating_sub(2).max(1);
            let viewport = buffer.line_viewport(visible_width);
            let rendered = if viewport.text.is_empty() {
                placeholder
                    .as_ref()
                    .map_or_else(String::new, |hint| truncate_chars(hint, visible_width))
            } else {
                viewport.text
            };
            let row = format!("> {}", opaque_row_text(&rendered, visible_width));
            cursor = Some((lines.len(), 2 + viewport.cursor_col));
            field_lines.push(row);
            if let Some(err_msg) = error {
                let err_display = format!(
                    "! {}",
                    truncate_chars(err_msg, text_width.saturating_sub(2))
                );
                field_lines.push(opaque_row_text(&err_display, text_width));
            }
        }
        (
            PromptField::SingleSelect { options, .. },
            PromptWidgetState::SingleSelect { selected, scroll },
        ) => {
            if options.is_empty() {
                field_lines.push(opaque_row_text("(no options)", text_width));
            } else {
                let visible_rows = body_rows.saturating_sub(lines.len()).max(1);
                *selected = (*selected).min(options.len().saturating_sub(1));
                *scroll = adjust_scroll(*scroll, *selected, options.len(), visible_rows);
                let end = (*scroll).saturating_add(visible_rows).min(options.len());
                for (index, option) in options.iter().enumerate().take(end).skip(*scroll) {
                    let marker = if index == *selected { ">" } else { " " };
                    let row = format!(
                        "{marker} {}",
                        truncate_chars(&option.label, text_width.saturating_sub(2))
                    );
                    field_lines.push(opaque_row_text(&row, text_width));
                }
            }
        }
        (
            PromptField::SearchSelect {
                options,
                placeholder,
                ..
            },
            PromptWidgetState::SearchSelect { palette },
        ) => {
            let query = &palette.query;
            let selected = palette.list.selected.get_or_insert(0);
            let scroll = &mut palette.list.offset;
            let visible_width = text_width.saturating_sub(2).max(1);
            let viewport = query.line_viewport(visible_width);
            let rendered = if viewport.text.is_empty() {
                placeholder.as_deref().map_or_else(
                    || "Type to search".to_string(),
                    |hint| truncate_chars(hint, visible_width),
                )
            } else {
                viewport.text
            };
            let query_row = format!("> {}", opaque_row_text(&rendered, visible_width));
            cursor = Some((lines.len() + field_lines.len(), 2 + viewport.cursor_col));
            field_lines.push(query_row);
            let filtered = filtered_option_indices(options, query.text());
            if filtered.is_empty() {
                field_lines.push(opaque_row_text("(no matches)", text_width));
            } else {
                let visible_rows = body_rows
                    .saturating_sub(lines.len())
                    .saturating_sub(field_lines.len())
                    .max(1);
                *selected = (*selected).min(filtered.len().saturating_sub(1));
                *scroll = adjust_scroll(*scroll, *selected, filtered.len(), visible_rows);
                let end = (*scroll).saturating_add(visible_rows).min(filtered.len());
                for (filtered_index, option_index) in
                    filtered.iter().enumerate().take(end).skip(*scroll)
                {
                    let Some(option) = options.get(*option_index) else {
                        continue;
                    };
                    let marker = if filtered_index == *selected {
                        ">"
                    } else {
                        " "
                    };
                    let row = format!(
                        "{marker} {}",
                        truncate_chars(&option.label, text_width.saturating_sub(2))
                    );
                    field_lines.push(opaque_row_text(&row, text_width));
                }
            }
        }
        (
            PromptField::MultiToggle { options, .. },
            PromptWidgetState::MultiToggle {
                cursor: index,
                selected,
                scroll,
            },
        ) => {
            if options.is_empty() {
                field_lines.push(opaque_row_text("(no options)", text_width));
            } else {
                let visible_rows = body_rows.saturating_sub(lines.len()).max(1);
                *index = (*index).min(options.len().saturating_sub(1));
                *scroll = adjust_scroll(*scroll, *index, options.len(), visible_rows);
                let end = (*scroll).saturating_add(visible_rows).min(options.len());
                for (row_index, option) in options.iter().enumerate().take(end).skip(*scroll) {
                    let active_marker = if row_index == *index { ">" } else { " " };
                    let checked = if selected.contains(&row_index) {
                        "[x]"
                    } else {
                        "[ ]"
                    };
                    let row = format!(
                        "{active_marker} {checked} {}",
                        truncate_chars(&option.label, text_width.saturating_sub(6))
                    );
                    field_lines.push(opaque_row_text(&row, text_width));
                }
            }
        }
        (
            PromptField::Form { sections, .. },
            PromptWidgetState::Form {
                cursor: index,
                scroll,
                values,
                editors,
                errors,
            },
        ) => {
            let rows = form_render_rows(sections, values, editors, errors);
            if rows.is_empty() {
                field_lines.push(opaque_row_text("(no fields)", text_width));
            } else {
                let visible_rows = body_rows.saturating_sub(lines.len()).max(1);
                *index = (*index).min(rows.len().saturating_sub(1));
                *scroll = adjust_scroll(*scroll, *index, rows.len(), visible_rows);
                let end = (*scroll).saturating_add(visible_rows).min(rows.len());
                for (row_index, row) in rows.iter().enumerate().take(end).skip(*scroll) {
                    let marker = if row.selectable && row_index == *index {
                        ">"
                    } else {
                        " "
                    };
                    let line = format!("{marker} {}", row.text);
                    field_lines.push(opaque_row_text(
                        &truncate_chars(&line, text_width),
                        text_width,
                    ));
                }
            }
        }
        _ => {
            field_lines.push(opaque_row_text("invalid prompt state", text_width));
        }
    }

    if lines.len().saturating_add(field_lines.len()) > body_rows {
        let max_prefix = body_rows.saturating_sub(field_lines.len());
        lines.truncate(max_prefix);
    }
    lines.extend(field_lines);

    if lines.len() > body_rows {
        lines.truncate(body_rows);
    }
    while lines.len() < body_rows {
        lines.push(" ".repeat(text_width));
    }

    PromptBodyRender { lines, cursor }
}

fn prompt_footer_text(request: &PromptRequest) -> String {
    match request.field {
        PromptField::Confirm { .. } => format!(
            "<-/-> choose | Enter {} | Esc {}",
            request.submit_label, request.cancel_label
        ),
        PromptField::TextInput { .. } => {
            format!(
                "Type | Enter {} | Esc {}",
                request.submit_label, request.cancel_label
            )
        }
        PromptField::SingleSelect { .. } => format!(
            "Up/Down choose | Enter {} | Esc {}",
            request.submit_label, request.cancel_label
        ),
        PromptField::SearchSelect { .. } => format!(
            "Type search | Up/Down choose | Enter {} | Esc {}",
            request.submit_label, request.cancel_label
        ),
        PromptField::MultiToggle { .. } => format!(
            "Up/Down move | Space toggle | Enter {} | Esc {}",
            request.submit_label, request.cancel_label
        ),
        PromptField::Form { .. } => format!(
            "Up/Down move | Space clear/toggle | Type edit | Enter {} | Esc {}",
            request.submit_label, request.cancel_label
        ),
    }
}

fn filtered_option_indices(options: &[PromptOption], query: &str) -> Vec<usize> {
    let mut scored = options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            fuzzy_score(query, &option.label).map(|score| (index, score, option.label.as_str()))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.2.cmp(right.2))
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.into_iter().map(|(index, _, _)| index).collect()
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let candidate_lower = candidate.to_ascii_lowercase();
    let mut last_match: Option<usize> = None;
    let mut score = 0_i64;
    let mut search_from = 0_usize;
    for needle in query.chars() {
        let haystack = &candidate_lower[search_from..];
        let found = haystack.find(needle)?;
        let absolute = search_from.saturating_add(found);
        if absolute == 0 {
            score += 100;
        } else if candidate_lower
            .as_bytes()
            .get(absolute.saturating_sub(1))
            .is_some_and(|byte| matches!(*byte, b' ' | b'-' | b'_' | b':' | b'/'))
        {
            score += 50;
        }
        if let Some(previous) = last_match {
            let gap = absolute.saturating_sub(previous).saturating_sub(1);
            score -= i64::try_from(gap).unwrap_or(i64::MAX / 4);
        }
        score += 10;
        last_match = Some(absolute);
        search_from = absolute.saturating_add(needle.len_utf8());
    }
    if candidate_lower.starts_with(&query) {
        score += 200;
    }
    Some(score)
}

fn adjust_scroll(current: usize, cursor: usize, total: usize, visible: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let visible = visible.max(1);
    let max_scroll = total.saturating_sub(visible);
    if cursor < current {
        cursor
    } else if cursor >= current.saturating_add(visible) {
        cursor
            .saturating_sub(visible.saturating_sub(1))
            .min(max_scroll)
    } else {
        current.min(max_scroll)
    }
}

fn wrap_lines(input: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut wrapped = Vec::new();
    for line in input.lines() {
        if line.trim().is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in line.split_whitespace() {
            let word_len = word.chars().count();
            let current_len = current.chars().count();
            let additional = if current.is_empty() {
                word_len
            } else {
                word_len + 1
            };
            if current_len.saturating_add(additional) > width {
                if !current.is_empty() {
                    wrapped.push(current.clone());
                    current.clear();
                }
                if word_len > width {
                    wrapped.push(truncate_chars(word, width));
                } else {
                    current.push_str(word);
                }
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            wrapped.push(current);
        }
    }

    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

fn truncate_chars(input: &str, width: usize) -> String {
    input.chars().take(width).collect::<String>()
}

/// Run validation for a text input value.
///
/// For most [`PromptValidation`] variants the SDK-level
/// [`PromptValidation::validate`] is authoritative.  The
/// [`PromptValidation::Regex`] variant requires the `regex` crate which
/// the SDK intentionally does not depend on, so we handle it here.
fn run_prompt_validation(
    rule: &crate::runtime::prompt::PromptValidation,
    value: &str,
) -> Result<(), String> {
    use crate::runtime::prompt::PromptValidation;

    match rule {
        PromptValidation::Regex { pattern, message } => {
            regex::Regex::new(pattern).map_or_else(
                // Invalid pattern — treat as validation error so the user
                // (or config author) notices rather than silently accepting.
                |_| Err(format!("invalid regex pattern: {pattern}")),
                |re| {
                    if re.is_match(value) {
                        Ok(())
                    } else {
                        Err(message.clone())
                    }
                },
            )
        }
        other => other.validate(value),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AttachInternalPromptAction, AttachPromptState, PromptKeyDisposition, adjust_scroll,
        render_prompt_body,
    };
    use crate::runtime::attach::input::TerminalGeometry;
    use crate::runtime::attach::tui_surface::{component_theme, parse_tui_color};
    use crate::runtime::prompt::{
        PromptFormField, PromptFormFieldKind, PromptFormSection, PromptFormValue, PromptOption,
        PromptRequest, PromptResponse, PromptValidation, PromptValue,
    };
    use bmux_appearance::RuntimeAppearance;
    use bmux_tui::style::Color;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use uuid::Uuid;

    fn key_event(code: KeyCode) -> KeyEvent {
        modified_key_event(code, KeyModifiers::NONE)
    }

    fn modified_key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn command_palette_degrades_to_full_viewport_on_small_terminal() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select("Command Palette", vec![PromptOption::new("one", "One")])
                .modal_id("command-palette"),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 20, rows: 6 };

        let render = state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default())
            .expect("compact palette should render");

        assert_eq!(render.surface.rect.x, 0);
        assert_eq!(render.surface.rect.y, 0);
        assert_eq!(render.surface.rect.w, 20);
        assert_eq!(render.surface.rect.h, 6);
    }

    #[test]
    fn command_palette_renders_scrollbar_for_long_results() {
        let mut state = AttachPromptState::default();
        let options = (0..40)
            .map(|index| PromptOption::new(index.to_string(), format!("Command {index}")))
            .collect();
        state.enqueue_internal(
            PromptRequest::search_select("Command Palette", options).modal_id("command-palette"),
            AttachInternalPromptAction::QuitSession,
        );

        let render = state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 80, rows: 24 },
                &RuntimeAppearance::default(),
            )
            .expect("palette should render");

        assert!(render.ops.iter().any(|op| {
            matches!(op, bmux_plugin::RenderOp::TextRun { text, .. } if text.contains('█'))
        }));
    }

    #[test]
    fn command_palette_mouse_uses_component_hit_map() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Command Palette",
                vec![
                    PromptOption::new("one", "One"),
                    PromptOption::new("two", "Two"),
                ],
            )
            .modal_id("command-palette"),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 80, rows: 24 };
        let _ = state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default())
            .expect("palette should render");
        let active = state.active.as_ref().expect("active prompt");
        let hit = active.hits.regions().get(1).expect("second option hit");
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.area.x,
            row: hit.area.y,
            modifiers: KeyModifiers::NONE,
        };

        let PromptKeyDisposition::Completed(completion) = state.handle_mouse_event(mouse, geometry)
        else {
            panic!("palette click should complete");
        };

        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Single("two".to_owned()))
        );
    }

    #[test]
    fn command_palette_uses_upper_third_placement() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select("Command Palette", vec![PromptOption::new("one", "One")])
                .modal_id("command-palette"),
            AttachInternalPromptAction::QuitSession,
        );

        let render = state
            .attach_prompt_overlay_render(
                TerminalGeometry {
                    cols: 120,
                    rows: 60,
                },
                &RuntimeAppearance::default(),
            )
            .expect("palette should render");
        let centered_y = (60_u16.saturating_sub(render.surface.rect.h)) / 2;

        assert!(render.surface.rect.y < centered_y);
        assert_eq!(
            render.surface.rect.y,
            (60_u16.saturating_sub(render.surface.rect.h)) / 3
        );
    }

    #[test]
    fn prompt_overlay_uses_rounded_tui_chrome_and_opaque_rows() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::text_input("Name"),
            AttachInternalPromptAction::QuitSession,
        );

        let render = state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 80, rows: 24 },
                &RuntimeAppearance::default(),
            )
            .expect("prompt should render");
        let top_y = render.surface.rect.y;
        let top_x = render.surface.rect.x;
        let top = render.ops.iter().find_map(|op| match op {
            bmux_plugin::RenderOp::TextRun { x, y, text, .. } if *x == top_x && *y == top_y => {
                Some(text.as_str())
            }
            _ => None,
        });

        assert!(top.is_some_and(|line| line.starts_with('╭') && line.ends_with('╮')));
        assert!(
            !render
                .ops
                .iter()
                .any(|op| matches!(op, bmux_plugin::RenderOp::Border { .. }))
        );
        assert!(
            (top_y..top_y.saturating_add(render.surface.rect.h)).all(|row| {
                render
                    .ops
                    .iter()
                    .any(|op| matches!(op, bmux_plugin::RenderOp::TextRun { y, .. } if *y == row))
            })
        );
    }

    #[test]
    fn runtime_appearance_maps_to_opaque_component_theme() {
        let appearance = RuntimeAppearance {
            foreground: "#112233".to_owned(),
            background: "#040506".to_owned(),
            cursor: "#aabbcc".to_owned(),
            selection_background: "#778899".to_owned(),
            ..RuntimeAppearance::default()
        };

        let theme = component_theme(&appearance);

        assert_eq!(theme.text.fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
        assert_eq!(theme.surfaces.overlay.bg, Some(Color::Rgb(4, 5, 6)));
        assert_eq!(theme.focused.fg, Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(theme.selected.bg, Some(Color::Rgb(0x77, 0x88, 0x99)));
        assert!(theme.surfaces.scrim.is_none());
    }

    #[test]
    fn invalid_runtime_colors_use_safe_fallbacks() {
        assert_eq!(parse_tui_color("nope"), None);
        assert_eq!(parse_tui_color("#123"), None);
        let appearance = RuntimeAppearance {
            foreground: "invalid".to_owned(),
            background: "invalid".to_owned(),
            cursor: "invalid".to_owned(),
            selection_background: "invalid".to_owned(),
            ..RuntimeAppearance::default()
        };

        let theme = component_theme(&appearance);

        assert_eq!(theme.text.fg, Some(Color::BrightWhite));
        assert_eq!(theme.surfaces.overlay.bg, Some(Color::Black));
        assert_eq!(theme.focused.fg, Some(Color::BrightCyan));
        assert_eq!(theme.selected.bg, Some(Color::Cyan));
    }

    #[test]
    fn adjust_scroll_keeps_cursor_visible() {
        assert_eq!(adjust_scroll(0, 0, 10, 4), 0);
        assert_eq!(adjust_scroll(0, 5, 10, 4), 2);
        assert_eq!(adjust_scroll(6, 2, 10, 4), 2);
        assert_eq!(adjust_scroll(8, 9, 10, 4), 6);
    }

    #[test]
    fn confirm_prompt_submits_on_enter() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::confirm("Quit?").confirm_default(true),
            AttachInternalPromptAction::QuitSession,
        );

        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));
        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Confirm(true))
        );
    }

    #[test]
    fn text_input_prompt_accepts_multiline_unicode_paste() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::text_input("Value"),
            AttachInternalPromptAction::QuitSession,
        );

        assert!(matches!(
            state.handle_paste("hello\r\n世界"),
            PromptKeyDisposition::Consumed
        ));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));
        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Text("hello\n世界".to_string()))
        );
    }

    #[test]
    fn confirm_prompt_consumes_paste_without_changing_selection() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::confirm("Continue?").confirm_default(true),
            AttachInternalPromptAction::QuitSession,
        );

        assert!(matches!(
            state.handle_paste("n"),
            PromptKeyDisposition::Consumed
        ));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));
        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Confirm(true))
        );
    }

    #[test]
    fn text_input_prompt_accepts_typing_and_backspace() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::text_input("Name").input_required(true),
            AttachInternalPromptAction::ClosePane {
                pane_id: Uuid::new_v4(),
            },
        );

        let _ = state.handle_key_event(&key_event(KeyCode::Char('h')));
        let _ = state.handle_key_event(&key_event(KeyCode::Char('i')));
        let _ = state.handle_key_event(&key_event(KeyCode::Backspace));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Text("h".to_string()))
        );
    }

    #[test]
    fn optional_text_input_prompt_allows_blank_value_with_validation() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::text_input("Count").input_validation(PromptValidation::Integer),
            AttachInternalPromptAction::QuitSession,
        );

        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Text(String::new()))
        );
    }

    #[test]
    fn confirm_prompt_render_uses_caret_without_checkbox_markers() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::confirm("Prompt Showcase")
                .confirm_default(true)
                .confirm_labels("Continue", "Stop"),
            AttachInternalPromptAction::QuitSession,
        );

        let active = state.active.as_mut().expect("prompt should be active");
        let initial = render_prompt_body(active, 64, 2);
        let initial_row = &initial.lines[0];
        assert!(
            initial_row.contains("> Continue"),
            "initial row: {initial_row:?}"
        );
        assert!(
            initial_row.contains("  Stop"),
            "initial row: {initial_row:?}"
        );
        assert!(!initial_row.contains("[x]"));
        assert!(!initial_row.contains("[ ]"));

        let _ = state.handle_key_event(&key_event(KeyCode::Right));
        let active = state.active.as_mut().expect("prompt should remain active");
        let switched = render_prompt_body(active, 64, 2);
        let switched_row = &switched.lines[0];
        assert!(
            switched_row.contains("  Continue"),
            "switched row: {switched_row:?}"
        );
        assert!(
            switched_row.contains("> Stop"),
            "switched row: {switched_row:?}"
        );
    }

    #[test]
    fn search_select_query_supports_cursor_editing() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Command",
                vec![
                    PromptOption::new("commit", "Commit"),
                    PromptOption::new("compact", "Compact"),
                    PromptOption::new("connect", "Connect"),
                ],
            ),
            AttachInternalPromptAction::QuitSession,
        );

        for ch in "cmmit".chars() {
            let _ = state.handle_key_event(&key_event(KeyCode::Char(ch)));
        }
        let _ = state.handle_key_event(&modified_key_event(KeyCode::Left, KeyModifiers::ALT));
        let _ = state.handle_key_event(&key_event(KeyCode::Right));
        let _ = state.handle_key_event(&key_event(KeyCode::Char('o')));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Single("commit".to_string()))
        );
    }

    #[test]
    fn form_text_paste_preserves_multiline_content() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::form(
                "Settings",
                vec![PromptFormSection::new(
                    "general",
                    "General",
                    vec![PromptFormField::new(
                        "notes",
                        "Notes",
                        PromptFormFieldKind::Text {
                            initial_value: String::new(),
                            placeholder: None,
                            validation: None,
                        },
                    )],
                )],
            ),
            AttachInternalPromptAction::QuitSession,
        );

        assert!(matches!(
            state.handle_paste("one\r\ntwo\rthree"),
            PromptKeyDisposition::Consumed
        ));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));
        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        let PromptResponse::Submitted(PromptValue::Form(values)) = completion.response else {
            panic!("expected form response");
        };
        assert_eq!(
            values.get("notes"),
            Some(&PromptFormValue::Text("one\ntwo\nthree".to_string()))
        );
    }

    #[test]
    fn pasted_text_reflows_cursor_and_prompt_render() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::text_input("Value"),
            AttachInternalPromptAction::QuitSession,
        );
        assert!(matches!(
            state.handle_paste("abcdefghij"),
            PromptKeyDisposition::Consumed
        ));

        let active = state.active.as_mut().expect("prompt should be active");
        let narrow = render_prompt_body(active, 6, 2);
        assert_eq!(narrow.cursor, Some((0, 5)));
        assert!(narrow.lines[0].contains("hij"));

        let wide = render_prompt_body(active, 14, 2);
        assert_eq!(wide.cursor, Some((0, 12)));
        assert!(wide.lines[0].contains("abcdefghij"));
    }

    #[test]
    fn form_text_field_supports_cursor_editing() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::form(
                "Settings",
                vec![PromptFormSection::new(
                    "general",
                    "General",
                    vec![PromptFormField::new(
                        "name",
                        "Name",
                        PromptFormFieldKind::Text {
                            initial_value: "helo".to_string(),
                            placeholder: None,
                            validation: None,
                        },
                    )],
                )],
            ),
            AttachInternalPromptAction::QuitSession,
        );

        let _ = state.handle_key_event(&modified_key_event(KeyCode::Left, KeyModifiers::ALT));
        let _ = state.handle_key_event(&key_event(KeyCode::Right));
        let _ = state.handle_key_event(&key_event(KeyCode::Right));
        let _ = state.handle_key_event(&key_event(KeyCode::Char('l')));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        let PromptResponse::Submitted(PromptValue::Form(values)) = completion.response else {
            panic!("expected form response");
        };
        assert_eq!(
            values.get("name"),
            Some(&PromptFormValue::Text("hello".to_string()))
        );
    }

    #[test]
    fn single_select_prompt_moves_with_arrow_keys() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::single_select(
                "Layout",
                vec![
                    PromptOption::new("tall", "Tall"),
                    PromptOption::new("wide", "Wide"),
                    PromptOption::new("grid", "Grid"),
                ],
            ),
            AttachInternalPromptAction::ClosePane {
                pane_id: Uuid::new_v4(),
            },
        );

        let _ = state.handle_key_event(&key_event(KeyCode::Down));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Single("wide".to_string()))
        );
    }

    #[test]
    fn form_integer_field_can_be_cleared_and_typed() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::form(
                "Pong Settings",
                vec![PromptFormSection::new(
                    "pong",
                    "Pong",
                    vec![PromptFormField::new(
                        "rally_ms",
                        "Rally duration ms",
                        PromptFormFieldKind::Integer {
                            initial_value: 5_500,
                            min: Some(1_000),
                            max: Some(20_000),
                        },
                    )],
                )],
            ),
            AttachInternalPromptAction::QuitSession,
        );

        let _ = state.handle_key_event(&key_event(KeyCode::Char(' ')));
        for ch in "8000".chars() {
            let _ = state.handle_key_event(&key_event(KeyCode::Char(ch)));
        }
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        let PromptResponse::Submitted(PromptValue::Form(values)) = completion.response else {
            panic!("expected form response");
        };
        assert_eq!(
            values.get("rally_ms"),
            Some(&PromptFormValue::Integer(8_000))
        );
    }

    #[test]
    fn multi_toggle_prompt_moves_with_arrow_keys_and_toggles_selection() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::multi_toggle(
                "Features",
                vec![
                    PromptOption::new("line-numbers", "Line numbers"),
                    PromptOption::new("timestamps", "Timestamps"),
                    PromptOption::new("soft-wrap", "Soft wrap"),
                ],
            )
            .multi_min_selected(1),
            AttachInternalPromptAction::ClosePane {
                pane_id: Uuid::new_v4(),
            },
        );

        let _ = state.handle_key_event(&key_event(KeyCode::Down));
        let _ = state.handle_key_event(&key_event(KeyCode::Char(' ')));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Multi(vec!["timestamps".to_string()]))
        );
    }
}
