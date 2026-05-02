use super::render::{opaque_row_text, queue_render_ops};
use super::state::AttachCursorState;
use crate::runtime::prompt::{
    PromptField, PromptFormField, PromptFormFieldKind, PromptFormValue, PromptHostRequest,
    PromptOption, PromptPolicy, PromptRequest, PromptResponse, PromptValue,
};
use anyhow::{Context, Result};
use bmux_attach_layout_protocol::{
    AttachLayer as SurfaceLayer, AttachRect, AttachSurface, AttachSurfaceKind,
};
use bmux_plugin::{BorderGlyphs, ExtensionRect, RenderDamage, RenderOp, RenderStyle};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use tokio::sync::oneshot;
use uuid::Uuid;

const PROMPT_OVERLAY_SURFACE_ID: Uuid = Uuid::from_u128(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachCloseFallbackTarget {
    Context { context_id: Uuid },
    Session { session_id: Uuid },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachInternalPromptAction {
    QuitSession,
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
        value: String,
        cursor: usize,
        /// Inline validation error shown when the user tries to submit
        /// invalid input.  Cleared on the next keystroke.
        error: Option<String>,
    },
    SingleSelect {
        selected: usize,
        scroll: usize,
    },
    SearchSelect {
        query: String,
        selected: usize,
        scroll: usize,
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
        errors: BTreeMap<String, String>,
    },
}

#[derive(Debug)]
struct ActivePrompt {
    envelope: AttachPromptEnvelope,
    state: PromptWidgetState,
    message_wrap_width: Option<usize>,
    message_wrapped_lines: Vec<String>,
}

impl ActivePrompt {
    fn from_envelope(envelope: AttachPromptEnvelope) -> Self {
        let state = match &envelope.request.field {
            PromptField::Confirm { default, .. } => PromptWidgetState::Confirm {
                selected_yes: *default,
            },
            PromptField::TextInput { initial_value, .. } => {
                let cursor = initial_value.chars().count();
                PromptWidgetState::TextInput {
                    value: initial_value.clone(),
                    cursor,
                    error: None,
                }
            }
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
                PromptWidgetState::SearchSelect {
                    query: String::new(),
                    selected,
                    scroll: 0,
                }
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
                errors: BTreeMap::new(),
            },
        };
        Self {
            envelope,
            state,
            message_wrap_width: None,
            message_wrapped_lines: Vec::new(),
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
                    PromptWidgetState::TextInput {
                        value,
                        cursor,
                        error,
                    },
                ) => match key.code {
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        insert_char(value, *cursor, ch);
                        *cursor = cursor.saturating_add(1);
                        *error = None;
                    }
                    KeyCode::Backspace => {
                        if *cursor > 0 {
                            *cursor = cursor.saturating_sub(1);
                            remove_char(value, *cursor);
                        }
                        *error = None;
                    }
                    KeyCode::Delete => {
                        remove_char(value, *cursor);
                        *error = None;
                    }
                    KeyCode::Left => {
                        *cursor = cursor.saturating_sub(1);
                    }
                    KeyCode::Right => {
                        *cursor = cursor.saturating_add(1).min(value.chars().count());
                    }
                    KeyCode::Home => {
                        *cursor = 0;
                    }
                    KeyCode::End => {
                        *cursor = value.chars().count();
                    }
                    KeyCode::Enter => {
                        if *required && value.trim().is_empty() {
                            *error = Some("value is required".to_string());
                            return PromptKeyDisposition::Consumed;
                        }
                        if (!value.trim().is_empty() || *required)
                            && let Some(rule) = validation
                            && let Err(msg) = run_prompt_validation(rule, value)
                        {
                            *error = Some(msg);
                            return PromptKeyDisposition::Consumed;
                        }
                        completion =
                            Some(PromptResponse::Submitted(PromptValue::Text(value.clone())));
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
                    PromptWidgetState::SearchSelect {
                        query,
                        selected,
                        scroll,
                    },
                ) => {
                    let previous_selected_value = filtered_option_indices(options, query)
                        .get(*selected)
                        .and_then(|index| options.get(*index))
                        .map(|option| option.value.clone());
                    match key.code {
                        KeyCode::Char(ch)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            query.push(ch);
                            *selected = 0;
                            *scroll = 0;
                        }
                        KeyCode::Backspace => {
                            query.pop();
                            *selected = 0;
                            *scroll = 0;
                        }
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            query.clear();
                            *selected = 0;
                            *scroll = 0;
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
                            let len = filtered_option_indices(options, query).len();
                            *selected = selected.saturating_add(1).min(len.saturating_sub(1));
                        }
                        KeyCode::Home => {
                            *selected = 0;
                        }
                        KeyCode::End => {
                            let len = filtered_option_indices(options, query).len();
                            *selected = len.saturating_sub(1);
                        }
                        KeyCode::Enter => {
                            let filtered = filtered_option_indices(options, query);
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
                    let filtered = filtered_option_indices(options, query);
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
                                    && append_form_text_char(field, values, ch)
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
                                    && pop_form_text_char(field, values)
                                {
                                    errors.remove(&field.id);
                                    if *live_preview {
                                        emit_form_changed(&active.envelope, field, values);
                                    }
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

    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) -> PromptKeyDisposition {
        let Some(layout) =
            prompt_overlay_layout(self.active.as_ref().map(|active| &active.envelope.request))
        else {
            return PromptKeyDisposition::NotActive;
        };
        let Some(active) = self.active.as_mut() else {
            return PromptKeyDisposition::NotActive;
        };

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
    pub fn overlay_surface(&self) -> Option<AttachSurface> {
        prompt_overlay_layout(self.active.as_ref().map(|active| &active.envelope.request))
            .map(|layout| layout.surface)
    }

    #[allow(clippy::cast_possible_truncation)] // Terminal coordinates are bounded by terminal::size() u16 dimensions.
    pub fn queue_attach_prompt_overlay(
        &mut self,
        stdout: &mut impl Write,
    ) -> Result<Option<AttachCursorState>> {
        let Some(layout) =
            prompt_overlay_layout(self.active.as_ref().map(|active| &active.envelope.request))
        else {
            return Ok(None);
        };

        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };

        let width = usize::from(layout.surface.rect.w);
        let height = usize::from(layout.surface.rect.h);
        let x = usize::from(layout.surface.rect.x);
        let y = usize::from(layout.surface.rect.y);
        let body_rows = height.saturating_sub(4).max(1);
        let text_width = width.saturating_sub(4);

        let body = render_prompt_body(active, text_width, body_rows);
        let footer = prompt_footer_text(&active.envelope.request);
        let style = RenderStyle::new();
        let surface_rect = ExtensionRect::new(
            layout.surface.rect.x,
            layout.surface.rect.y,
            layout.surface.rect.w,
            layout.surface.rect.h,
        );
        let interior = ExtensionRect::new(
            layout.surface.rect.x.saturating_add(1),
            layout.surface.rect.y.saturating_add(1),
            layout.surface.rect.w.saturating_sub(2),
            layout.surface.rect.h.saturating_sub(2),
        );
        let title = format!(
            " {} ",
            truncate_chars(&active.envelope.request.title, text_width)
        );
        let title_x = x + ((width.saturating_sub(title.len())) / 2);
        let mut ops = vec![
            RenderOp::clear_rect(interior, style),
            RenderOp::border(surface_rect, BorderGlyphs::ascii(), style),
            RenderOp::text_run(title_x as u16, y as u16, title, style),
        ];
        for (index, line) in body.lines.iter().take(body_rows).enumerate() {
            let row = y + 1 + index;
            ops.push(RenderOp::text_run(
                (x + 2) as u16,
                row as u16,
                line.clone(),
                style,
            ));
        }
        ops.push(RenderOp::text_run(
            (x + 2) as u16,
            (y + height - 2) as u16,
            opaque_row_text(&footer, text_width),
            style,
        ));
        queue_render_ops(stdout, surface_rect, &RenderDamage::FullSurface, &ops)
            .context("failed queueing declarative prompt overlay")?;

        let cursor_state = body.cursor.map(|(row, col)| AttachCursorState {
            x: (x + 2 + col).min(u16::MAX as usize) as u16,
            y: (y + 1 + row).min(u16::MAX as usize) as u16,
            visible: true,
        });

        Ok(cursor_state)
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
        _ => {}
    }
}

fn append_form_text_char(
    field: &PromptFormField,
    values: &mut BTreeMap<String, PromptFormValue>,
    ch: char,
) -> bool {
    match (&field.kind, values.get_mut(&field.id)) {
        (PromptFormFieldKind::Text { .. }, Some(PromptFormValue::Text(value)))
        | (PromptFormFieldKind::Number { .. }, Some(PromptFormValue::Number(value))) => {
            value.push(ch);
            true
        }
        _ => false,
    }
}

fn pop_form_text_char(
    field: &PromptFormField,
    values: &mut BTreeMap<String, PromptFormValue>,
) -> bool {
    match (&field.kind, values.get_mut(&field.id)) {
        (PromptFormFieldKind::Text { .. }, Some(PromptFormValue::Text(value)))
        | (PromptFormFieldKind::Number { .. }, Some(PromptFormValue::Number(value))) => {
            value.pop();
            true
        }
        _ => false,
    }
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
    errors: &BTreeMap<String, String>,
) -> Vec<FormRenderRow> {
    let mut rows = Vec::new();
    for section in sections {
        for field in &section.fields {
            let value = values
                .get(&field.id)
                .map_or_else(String::new, form_value_display);
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
fn prompt_overlay_layout(request: Option<&PromptRequest>) -> Option<PromptOverlayLayout> {
    let request = request?;
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols < 24 || rows < 8 {
        return None;
    }

    let content_width = prompt_estimated_width(request);
    let capped_max = request.width.max.max(request.width.min);
    let width = (content_width + 4)
        .max(usize::from(request.width.min.max(24)))
        .min(usize::from(capped_max.max(24)))
        .min((cols as usize).saturating_sub(2));
    let estimated_lines = prompt_estimated_lines(request);
    let height = (estimated_lines + 4)
        .max(7)
        .min((rows as usize).saturating_sub(2));
    let x = ((cols as usize).saturating_sub(width)) / 2;
    let y = ((rows as usize).saturating_sub(height)) / 2;

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
            PromptWidgetState::TextInput {
                value,
                cursor: pos,
                error,
            },
        ) => {
            let visible_width = text_width.saturating_sub(2).max(1);
            let char_len = value.chars().count();
            let bounded_cursor = (*pos).min(char_len);
            let offset = bounded_cursor.saturating_sub(visible_width.saturating_sub(1));
            let visible = take_chars(value, offset, visible_width);
            let rendered = if visible.is_empty() {
                placeholder
                    .as_ref()
                    .map_or_else(String::new, |hint| truncate_chars(hint, visible_width))
            } else {
                visible
            };
            let row = format!("> {}", opaque_row_text(&rendered, visible_width));
            cursor = Some((lines.len(), 2 + bounded_cursor.saturating_sub(offset)));
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
            PromptWidgetState::SearchSelect {
                query,
                selected,
                scroll,
            },
        ) => {
            let query_hint = if query.is_empty() {
                placeholder.as_deref().unwrap_or("Type to search")
            } else {
                query.as_str()
            };
            let query_row = format!(
                "> {}",
                truncate_chars(query_hint, text_width.saturating_sub(2))
            );
            field_lines.push(opaque_row_text(&query_row, text_width));
            let filtered = filtered_option_indices(options, query);
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
                errors,
            },
        ) => {
            let rows = form_render_rows(sections, values, errors);
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
            "Up/Down move | Space edit | Enter {} | Esc {}",
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

fn take_chars(input: &str, start: usize, count: usize) -> String {
    input.chars().skip(start).take(count).collect::<String>()
}

fn byte_index_for_char(input: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    input
        .char_indices()
        .nth(char_index)
        .map_or(input.len(), |(index, _)| index)
}

fn insert_char(input: &mut String, char_index: usize, ch: char) {
    let byte_index = byte_index_for_char(input, char_index);
    input.insert(byte_index, ch);
}

fn remove_char(input: &mut String, char_index: usize) {
    if char_index >= input.chars().count() {
        return;
    }
    let start = byte_index_for_char(input, char_index);
    let end = byte_index_for_char(input, char_index.saturating_add(1));
    input.replace_range(start..end, "");
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
    use crate::runtime::prompt::{
        PromptOption, PromptRequest, PromptResponse, PromptValidation, PromptValue,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use uuid::Uuid;

    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
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
