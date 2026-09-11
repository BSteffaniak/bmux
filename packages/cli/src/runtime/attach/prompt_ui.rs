use std::cell::RefCell;

use super::input::{TerminalGeometry, TerminalKeyEvent, TerminalMouseEvent};
use super::render::opaque_row_text;
use super::state::AttachCursorState;
use super::tui_surface::{buffer_render_ops, component_theme, surface_buffer};
use crate::runtime::prompt::{
    PromptField, PromptFormField, PromptFormFieldKind, PromptFormValue, PromptHostRequest,
    PromptOption, PromptPolicy, PromptRequest, PromptResponse, PromptSearchMatchMode, PromptValue,
};
use anyhow::Result;
use bmux_appearance::RuntimeAppearance;
use bmux_attach_layout_protocol::{
    AttachLayer as SurfaceLayer, AttachRect, AttachSurface, AttachSurfaceKind,
};
use bmux_plugin::RenderOp;
use bmux_text_edit::{TextDelete, TextEditBuffer, TextMotion};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCache, LayoutCx, LayoutNode, LogicalSize,
};
use bmux_tui::composition::{Column, SizeBox, TextBlock};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::hit::{HitMap, HitRegion};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Text};
use bmux_tui_components::action_row::{ActionButton, ActionRowComponent, ActionRowState};
use bmux_tui_components::button::ButtonStyles;
use bmux_tui_components::checkbox::{CheckboxComponent, CheckboxState, CheckboxStyles};
use bmux_tui_components::dialog::{Dialog, DialogComponent};
use bmux_tui_components::modal_frame::{ModalFrame, ModalFrameComponent, ModalSizing};
use bmux_tui_components::scroll_view::{ScrollViewComponent, ScrollViewState};
use bmux_tui_components::scrollbar::{ScrollbarComponent, ScrollbarPolicy, ScrollbarStyles};
use bmux_tui_components::text_input::{TextInputComponent, TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{
    TextInputBoxComponent, TextInputBoxPolicy, TextInputBoxStyles,
};
use bmux_tui_components::virtual_list::{VirtualList, VirtualListState};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::cell::Cell;
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
        list: VirtualListState<usize>,
    },
    SearchSelect {
        query: TextEditBuffer,
        selected: usize,
        list: VirtualListState<usize>,
        // Separate width-specific probe; released with the prompt.
        width_probe: Box<VirtualListState<usize>>,
        viewport_height: u64,
        viewport_width: u16,
    },
    MultiToggle {
        cursor: usize,
        selected: BTreeSet<usize>,
        scroll: ScrollViewState,
    },
    Form {
        cursor: usize,
        scroll: ScrollViewState,
        values: BTreeMap<String, PromptFormValue>,
        editors: BTreeMap<String, TextEditBuffer>,
        errors: BTreeMap<String, String>,
        page: usize,
    },
}

#[derive(Debug)]
struct ActivePrompt {
    envelope: AttachPromptEnvelope,
    state: PromptWidgetState,
    hits: HitMap,
    manual_list_scroll: bool,
    list_viewport: Rect,
    // One active scrolling layout; dropped with the prompt, replaced on geometry changes.
    scroll_layout: LayoutCache,
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
                    list: VirtualListState::new(0),
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
                    query: TextEditBuffer::new(),
                    selected,
                    list: VirtualListState::new(0),
                    width_probe: Box::new(VirtualListState::new(0)),
                    viewport_height: 0,
                    viewport_width: 0,
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
                    scroll: ScrollViewState::new(),
                }
            }
            PromptField::Form { sections, .. } => PromptWidgetState::Form {
                cursor: 0,
                scroll: ScrollViewState::new(),
                values: initial_form_values(sections),
                editors: initial_form_editors(sections),
                errors: BTreeMap::new(),
                page: 0,
            },
        };
        Self {
            envelope,
            state,
            hits: HitMap::new(),
            manual_list_scroll: false,
            list_viewport: Rect::new(0, 0, 0, 0),
            scroll_layout: LayoutCache::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct AttachPromptState {
    queue: VecDeque<AttachPromptEnvelope>,
    active: Option<ActivePrompt>,
    overlay_visible: bool,
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
                PromptField::SearchSelect { .. },
                PromptWidgetState::SearchSelect {
                    query, selected, ..
                },
            ) => {
                active.manual_list_scroll = false;
                query.paste(text);
                *selected = 0;
            }
            (
                PromptField::Form {
                    sections,
                    live_preview,
                    ..
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

        if matches!(key.code, KeyCode::F(6)) {
            self.overlay_visible = !self.overlay_visible;
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
            active.manual_list_scroll = false;
            match (&active.envelope.request.field, &mut active.state) {
                (PromptField::Confirm { .. }, PromptWidgetState::Confirm { selected_yes }) => {
                    match key.code {
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
                            completion =
                                Some(PromptResponse::Submitted(PromptValue::Confirm(true)));
                        }
                        KeyCode::Char('n' | 'N') => {
                            *selected_yes = false;
                            completion =
                                Some(PromptResponse::Submitted(PromptValue::Confirm(false)));
                        }
                        KeyCode::Enter => {
                            completion = Some(PromptResponse::Submitted(PromptValue::Confirm(
                                *selected_yes,
                            )));
                        }
                        _ => {}
                    }
                }
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
                    PromptWidgetState::SingleSelect { selected, .. },
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
                        if *live_preview && *selected != previous_selected {
                            emit_selection_changed(&active.envelope, *selected);
                        }
                    }
                }
                (
                    PromptField::SearchSelect {
                        options,
                        match_mode,
                        live_preview,
                        ..
                    },
                    PromptWidgetState::SearchSelect {
                        query,
                        selected,
                        list,
                        viewport_height,
                        viewport_width,
                        width_probe,
                    },
                ) => {
                    let previous_selected_value =
                        filtered_option_indices(options, query.text(), *match_mode)
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
                        }
                        KeyCode::Backspace => {
                            query.delete_backward();
                            *selected = 0;
                        }
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            query.clear();
                            *selected = 0;
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
                            let len =
                                filtered_option_indices(options, query.text(), *match_mode).len();
                            *selected = selected.saturating_add(1).min(len.saturating_sub(1));
                        }
                        KeyCode::Home => {
                            *selected = 0;
                        }
                        KeyCode::PageUp | KeyCode::PageDown => {
                            let filtered =
                                filtered_option_indices(options, query.text(), *match_mode);
                            if *viewport_width > 0 {
                                let measured = filtered.iter().copied().fold(
                                    VirtualList::new("command-list"),
                                    |measured, source| measured.component(source,
                                        TextBlock::new(Text::from_lines([search_select_label(
                                            &options[source],
                                            active.envelope.request.modal_id.as_deref() == Some("command-palette"),
                                            bmux_tui_components::modal_frame::ModalTheme::dark(bmux_tui::style::Color::White),
                                        )])).id(format!("command-list.label.{source}"))),
                                );
                                sync_search_select_width(
                                    &measured,
                                    width_probe,
                                    list,
                                    *viewport_width,
                                    *viewport_height,
                                );
                            }
                            *selected = search_select_page_target(
                                list,
                                &filtered,
                                *selected,
                                *viewport_height,
                                key.code == KeyCode::PageDown,
                            );
                        }
                        KeyCode::End => {
                            let len =
                                filtered_option_indices(options, query.text(), *match_mode).len();
                            *selected = len.saturating_sub(1);
                        }
                        KeyCode::Enter => {
                            let filtered =
                                filtered_option_indices(options, query.text(), *match_mode);
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
                    let filtered = filtered_option_indices(options, query.text(), *match_mode);
                    *selected = (*selected).min(filtered.len().saturating_sub(1));
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
                        scroll: _,
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
                    }
                }
                (
                    PromptField::Form {
                        sections,
                        live_preview,
                        resettable,
                        paged_on_small,
                    },
                    PromptWidgetState::Form {
                        cursor,
                        scroll: _,
                        values,
                        editors,
                        errors,
                        page,
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
                            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                                *cursor = cursor.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                                *cursor = cursor.saturating_add(1).min(len.saturating_sub(1));
                            }
                            KeyCode::PageUp if *paged_on_small => {
                                *page = page.saturating_sub(1);
                                *cursor = sections
                                    .iter()
                                    .take(*page)
                                    .map(|section| section.fields.len())
                                    .sum();
                            }
                            KeyCode::PageDown if *paged_on_small => {
                                *page =
                                    page.saturating_add(1).min(sections.len().saturating_sub(1));
                                *cursor = sections
                                    .iter()
                                    .take(*page)
                                    .map(|section| section.fields.len())
                                    .sum();
                            }
                            KeyCode::Home => {
                                *cursor = 0;
                            }
                            KeyCode::End => {
                                *cursor = len.saturating_sub(1);
                            }
                            KeyCode::Char('r' | 'R') if *resettable => {
                                *values = initial_form_values(sections);
                                *editors = initial_form_editors(sections);
                                errors.clear();
                                if *live_preview {
                                    for field in &fields {
                                        emit_form_changed(&active.envelope, field, values);
                                    }
                                }
                            }
                            KeyCode::Char(' ') => {
                                if let Some(field) = fields.get(*cursor)
                                    && !field.disabled
                                {
                                    cycle_form_value(field, values);
                                    validate_and_emit_form_change(
                                        &active.envelope,
                                        field,
                                        values,
                                        errors,
                                        *live_preview,
                                    );
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
                                    validate_and_emit_form_change(
                                        &active.envelope,
                                        field,
                                        values,
                                        errors,
                                        *live_preview,
                                    );
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
                                    validate_and_emit_form_change(
                                        &active.envelope,
                                        field,
                                        values,
                                        errors,
                                        *live_preview,
                                    );
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
                                    validate_and_emit_form_change(
                                        &active.envelope,
                                        field,
                                        values,
                                        errors,
                                        *live_preview,
                                    );
                                }
                            }
                            KeyCode::Left | KeyCode::Right
                                if key.modifiers.intersects(KeyModifiers::SHIFT)
                                    && let Some(field) = fields.get(*cursor)
                                    && !field.disabled =>
                            {
                                let sign = if matches!(key.code, KeyCode::Left) {
                                    -1
                                } else {
                                    1
                                };
                                if adjust_form_integer(field, values, sign * 5) {
                                    validate_and_emit_form_change(
                                        &active.envelope,
                                        field,
                                        values,
                                        errors,
                                        *live_preview,
                                    );
                                }
                            }
                            KeyCode::Left | KeyCode::Right
                                if key.modifiers.intersects(KeyModifiers::CONTROL)
                                    && let Some(field) = fields.get(*cursor)
                                    && !field.disabled =>
                            {
                                let sign = if matches!(key.code, KeyCode::Left) {
                                    -1
                                } else {
                                    1
                                };
                                if adjust_form_integer(field, values, sign * 10) {
                                    validate_and_emit_form_change(
                                        &active.envelope,
                                        field,
                                        values,
                                        errors,
                                        *live_preview,
                                    );
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

    #[allow(clippy::too_many_lines)] // Mouse routing keeps modal containment and concrete prompt interactions in one state-machine entry point.
    pub fn handle_mouse_event(
        &mut self,
        mouse: MouseEvent,
        geometry: TerminalGeometry,
    ) -> PromptKeyDisposition {
        if !self.overlay_visible {
            return PromptKeyDisposition::Consumed;
        }
        let Some(layout) = prompt_overlay_layout(
            self.active.as_ref().map(|active| &active.envelope.request),
            geometry,
        ) else {
            return PromptKeyDisposition::NotActive;
        };
        let Some(active) = self.active.as_mut() else {
            return PromptKeyDisposition::NotActive;
        };

        if let (
            PromptField::Form {
                sections,
                live_preview,
                ..
            },
            PromptWidgetState::Form {
                cursor,
                scroll: _,
                values,
                errors,
                ..
            },
        ) = (&active.envelope.request.field, &mut active.state)
        {
            if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                return PromptKeyDisposition::Consumed;
            }
            let content_y = layout.surface.rect.y.saturating_add(1);
            let content_bottom = layout
                .surface
                .rect
                .y
                .saturating_add(layout.surface.rect.h.saturating_sub(1));
            if mouse.column <= layout.surface.rect.x
                || mouse.column
                    >= layout
                        .surface
                        .rect
                        .x
                        .saturating_add(layout.surface.rect.w)
                        .saturating_sub(1)
                || mouse.row < content_y
                || mouse.row >= content_bottom
            {
                return PromptKeyDisposition::Consumed;
            }
            let fields = flatten_form_fields(sections);
            let Some(index) = active.hits.regions().iter().rev().find_map(|hit| {
                hit.area
                    .contains(Point::new(mouse.column, mouse.row))
                    .then(|| {
                        hit.id
                            .as_str()
                            .strip_prefix("prompt.form.field.")?
                            .parse::<usize>()
                            .ok()
                    })
                    .flatten()
            }) else {
                // No painted target: do not infer interaction geometry from raw rows.
                return PromptKeyDisposition::Consumed;
            };
            let Some(field) = fields.get(index) else {
                return PromptKeyDisposition::Consumed;
            };
            *cursor = index;
            if !field.disabled && matches!(field.kind, PromptFormFieldKind::Bool { .. }) {
                cycle_form_value(field, values);
                validate_and_emit_form_change(
                    &active.envelope,
                    field,
                    values,
                    errors,
                    *live_preview,
                );
            }
            return PromptKeyDisposition::Consumed;
        }

        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) && active
            .list_viewport
            .contains(Point::new(mouse.column, mouse.row))
            && let PromptWidgetState::SearchSelect { list, .. }
            | PromptWidgetState::SingleSelect { list, .. } = &mut active.state
        {
            let delta = if mouse.kind == MouseEventKind::ScrollUp {
                -3
            } else {
                3
            };
            list.scroll_by(delta, u64::from(active.list_viewport.height));
            active.manual_list_scroll = true;
            return PromptKeyDisposition::Consumed;
        }
        if !matches!(
            mouse.kind,
            MouseEventKind::Moved | MouseEventKind::Down(MouseButton::Left)
        ) {
            return PromptKeyDisposition::Consumed;
        }

        if matches!(
            active.envelope.request.field,
            PromptField::SearchSelect { .. }
        ) {
            let Some(hit) = active.hits.hit_test(Point::new(mouse.column, mouse.row)) else {
                return PromptKeyDisposition::Consumed;
            };
            let Some(source_index) = hit
                .id()
                .as_str()
                .strip_prefix("command-list.item.")
                .and_then(|value| value.parse::<usize>().ok())
            else {
                return PromptKeyDisposition::Consumed;
            };
            let PromptField::SearchSelect {
                options,
                match_mode,
                live_preview,
                ..
            } = &active.envelope.request.field
            else {
                return PromptKeyDisposition::Consumed;
            };
            let PromptWidgetState::SearchSelect {
                query, selected, ..
            } = &mut active.state
            else {
                return PromptKeyDisposition::Consumed;
            };
            let filtered = filtered_option_indices(options, query.text(), *match_mode);
            let Some(filtered_index) = filtered.iter().position(|index| *index == source_index)
            else {
                return PromptKeyDisposition::Consumed;
            };
            let previous = *selected;
            *selected = filtered_index;
            if *live_preview && previous != filtered_index {
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
        let PromptWidgetState::SingleSelect { selected, .. } = &mut active.state else {
            return PromptKeyDisposition::Consumed;
        };
        if options.is_empty() {
            return PromptKeyDisposition::Consumed;
        }

        let Some(option_index) = active
            .hits
            .hit_test(Point::new(mouse.column, mouse.row))
            .and_then(|hit| {
                hit.id()
                    .as_str()
                    .strip_prefix("single-select-list.item.")
                    .and_then(|index| index.parse::<usize>().ok())
            })
        else {
            return PromptKeyDisposition::Consumed;
        };
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
        if !self.overlay_visible {
            return None;
        }
        prompt_overlay_layout(
            self.active.as_ref().map(|active| &active.envelope.request),
            geometry,
        )
        .map(|layout| layout.surface)
    }

    #[allow(clippy::cast_possible_truncation, clippy::too_many_lines)] // Overlay composition keeps one source for component selection, cursor, footer, and retained output.
    pub fn attach_prompt_overlay_render(
        &mut self,
        geometry: TerminalGeometry,
        appearance: &RuntimeAppearance,
        extension_chrome: bool,
    ) -> Option<AttachPromptOverlayRender> {
        if !self.overlay_visible {
            return None;
        }
        let layout = prompt_overlay_layout(
            self.active.as_ref().map(|active| &active.envelope.request),
            geometry,
        )?;
        let active = self.active.as_mut()?;

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
        let (rendered_palette, cursor_state) = {
            let mut cx = PaintCx::new(&mut frame);
            let modal_component =
                ModalFrameComponent::new("prompt.overlay", modal.clone(), TextBlock::new(""))
                    .chrome(!extension_chrome);
            paint_component(&modal_component, area, &mut cx);
            let content = modal.content_area(area);
            let rendered_palette = render_command_palette(active, content, &mut cx, theme);
            let rendered_single_select =
                !rendered_palette && render_single_select(active, content, &mut cx, theme);
            let rendered_text_input = !rendered_palette
                && !rendered_single_select
                && render_text_input(active, content, &mut cx, theme);
            let rendered_confirm = !rendered_palette
                && !rendered_single_select
                && !rendered_text_input
                && render_confirm(active, area, &mut cx, theme);
            let rendered_multi_toggle = !rendered_palette
                && !rendered_single_select
                && !rendered_text_input
                && !rendered_confirm
                && render_multi_toggle(active, content, &mut cx, theme);
            let rendered_form = !rendered_palette
                && !rendered_single_select
                && !rendered_text_input
                && !rendered_confirm
                && !rendered_multi_toggle
                && render_form(active, content, &mut cx, theme);
            debug_assert!(
                rendered_palette
                    || rendered_single_select
                    || rendered_text_input
                    || rendered_confirm
                    || rendered_multi_toggle
                    || rendered_form,
                "every prompt field must have a component renderer"
            );
            if !rendered_palette && !rendered_confirm && content.height > 1 {
                let footer_y = content.bottom().saturating_sub(1);
                cx.write_line_with_fallback_style(
                    LocalRect::terminal(Rect::new(content.x, footer_y, content.width, 1)),
                    &Line::raw(opaque_row_text(&footer, usize::from(content.width))),
                    theme.muted,
                );
            }
            let cursor_state = (rendered_palette || rendered_text_input)
                .then(|| cx.cursor())
                .flatten()
                .map(|cursor| AttachCursorState {
                    x: cursor.position.x,
                    y: cursor.position.y,
                    visible: cursor.visible,
                });
            (rendered_palette, cursor_state)
        };
        if rendered_palette
            || matches!(
                active.state,
                PromptWidgetState::SingleSelect { .. }
                    | PromptWidgetState::Form { .. }
                    | PromptWidgetState::MultiToggle { .. }
            )
        {
            active.hits = frame.hits().clone();
        }
        let ops = buffer_render_ops(&buffer);

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
            self.overlay_visible = true;
            self.active = Some(ActivePrompt::from_envelope(next));
        }
    }

    fn complete_active(&mut self, response: PromptResponse) -> PromptKeyDisposition {
        let Some(active) = self.active.take() else {
            return PromptKeyDisposition::NotActive;
        };
        self.overlay_visible = true;
        self.activate_next();
        PromptKeyDisposition::Completed(AttachPromptCompletion {
            origin: active.envelope.origin,
            response,
        })
    }
}

fn paint_retained_scroll(
    component: &impl Component,
    area: Rect,
    cache: &mut LayoutCache,
    cx: &mut PaintCx<'_, '_>,
) {
    let id = bmux_tui::component::LayoutId::new("prompt.scroll");
    let constraints = Constraints::tight(area.size());
    if cache
        .get(&id, component.revision().layout, constraints)
        .is_none()
    {
        cache.clear();
    }
    let layout = cache.layout(id, component, constraints, &mut LayoutCx::new());
    cx.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
}

fn paint_component(component: &impl Component, area: Rect, cx: &mut PaintCx<'_, '_>) {
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    cx.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
}

fn paint_checkbox(
    id: impl Into<bmux_tui::component::LayoutId>,
    label: &str,
    state: CheckboxState,
    styles: CheckboxStyles,
    fallback: bmux_tui::style::Style,
    area: Rect,
    cx: &mut PaintCx<'_, '_>,
) {
    let state = std::cell::Cell::new(state);
    let component = CheckboxComponent::new(id, label, &state)
        .styles(styles)
        .fallback_style(fallback);
    paint_component(&component, area, cx);
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Rendering one field keeps all concrete control projections together over canonical form state.
fn render_form_control(
    field: &PromptFormField,
    values: &BTreeMap<String, PromptFormValue>,
    editors: &BTreeMap<String, TextEditBuffer>,
    errors: &BTreeMap<String, String>,
    focused: bool,
    area: Rect,
    cx: &mut PaintCx<'_, '_>,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> bool {
    match (&field.kind, values.get(&field.id)) {
        (PromptFormFieldKind::Bool { .. }, Some(PromptFormValue::Bool(value))) => {
            let mut state = CheckboxState::new(*value);
            state.set_focused(focused);
            state.set_disabled(field.disabled);
            paint_checkbox(
                format!("prompt.form.{}", field.id),
                &field.label,
                state,
                CheckboxStyles {
                    normal: theme.text,
                    focused: theme.focused,
                    hovered: theme.focused,
                    pressed: theme.focused,
                    disabled: theme.muted,
                },
                theme.background,
                area,
                cx,
            );
            true
        }
        (
            PromptFormFieldKind::Text { .. } | PromptFormFieldKind::Number { .. },
            Some(PromptFormValue::Text(_) | PromptFormValue::Number(_)),
        ) => {
            let display = form_field_display(field, values, editors);
            let state = std::cell::RefCell::new(TextInputState::new(
                editors
                    .get(&field.id)
                    .cloned()
                    .unwrap_or_else(|| TextEditBuffer::from_text(display)),
            ));
            let mut component = TextInputBoxComponent::new(
                format!("prompt.form.{}", field.id),
                TextInputPolicy::chat_composer(),
                &state,
            )
            .label(&field.label)
            .required(field.required)
            .policy(
                TextInputBoxPolicy::bare()
                    .focused(focused)
                    .disabled(field.disabled)
                    .rows(1, Some(1)),
            )
            .styles(form_text_input_styles(theme));
            if let PromptFormFieldKind::Text {
                placeholder: Some(placeholder),
                ..
            } = &field.kind
            {
                component = component.placeholder(placeholder);
            }
            if let Some(error) = errors.get(&field.id) {
                component = component.error(error);
            }
            paint_component(&component, area, cx);
            true
        }
        (
            PromptFormFieldKind::SingleSelect { options, .. },
            Some(PromptFormValue::Single(value)),
        ) => {
            let label = options
                .iter()
                .find(|option| option.value == *value)
                .map_or("Select an option", |option| option.label.as_str());
            let style = if field.disabled {
                theme.muted
            } else if focused {
                theme.focused
            } else {
                theme.text
            };
            let component = TextBlock::new(Text::from_lines([Line::from_spans([
                Span::styled(label, style),
                Span::styled(" ▾", style),
            ])]))
            .id(format!("prompt.form.{}.select", field.id))
            .style(theme.background);
            paint_component(&component, area, cx);
            true
        }
        (PromptFormFieldKind::Integer { .. }, Some(PromptFormValue::Integer(value))) => {
            let state = std::cell::RefCell::new(TextInputState::new(
                editors
                    .get(&field.id)
                    .cloned()
                    .unwrap_or_else(|| TextEditBuffer::from_text(value.to_string())),
            ));
            let mut component = TextInputBoxComponent::new(
                format!("prompt.form.{}", field.id),
                TextInputPolicy::chat_composer(),
                &state,
            )
            .label(&field.label)
            .required(field.required)
            .policy(
                TextInputBoxPolicy::bare()
                    .focused(focused)
                    .disabled(field.disabled)
                    .rows(1, Some(1)),
            )
            .styles(form_text_input_styles(theme));
            if let Some(error) = errors.get(&field.id) {
                component = component.error(error);
            }
            paint_component(&component, area, cx);
            true
        }
        (
            PromptFormFieldKind::MultiToggle { options, .. },
            Some(PromptFormValue::Multi(selected)),
        ) => {
            let summary = if selected.is_empty() {
                "None".to_owned()
            } else {
                options
                    .iter()
                    .filter(|option| selected.contains(&option.value))
                    .map(|option| option.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut state = CheckboxState::new(!selected.is_empty());
            state.set_focused(focused);
            state.set_disabled(field.disabled);
            let label = format!("{}: {summary}", field.label);
            paint_checkbox(
                format!("prompt.form.{}", field.id),
                &label,
                state,
                CheckboxStyles {
                    normal: theme.text,
                    focused: theme.focused,
                    hovered: theme.focused,
                    pressed: theme.focused,
                    disabled: theme.muted,
                },
                theme.background,
                area,
                cx,
            );
            true
        }
        _ => false,
    }
}

const fn form_text_input_styles(
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> TextInputBoxStyles {
    TextInputBoxStyles {
        text: theme.text,
        focused_text: theme.focused,
        disabled_text: theme.muted,
        placeholder: theme.muted,
        selection: theme.focused,
        border: theme.border,
        focused_border: theme.focused,
        background: theme.background,
        focused_background: theme.background,
        disabled_background: theme.background,
    }
}

struct FormFieldComponent<'a> {
    id: String,
    field: &'a PromptFormField,
    values: &'a BTreeMap<String, PromptFormValue>,
    editors: &'a BTreeMap<String, TextEditBuffer>,
    errors: &'a BTreeMap<String, String>,
    focused: bool,
    text: String,
    theme: bmux_tui_components::modal_frame::ModalTheme,
}

impl Component for FormFieldComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        SizeBox::new(TextBlock::new(self.text.clone()))
            .id(self.id.clone())
            .height(1)
            .revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        // Existing form controls occupy one terminal row, including error text.
        SizeBox::new(TextBlock::new(self.text.clone()))
            .id(self.id.clone())
            .height(1)
            .layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = Rect::new(
            0,
            0,
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            1,
        );
        cx.push_hit(HitRegion::new(self.id.clone(), area));
        if !self.errors.contains_key(&self.field.id)
            && render_form_control(
                self.field,
                self.values,
                self.editors,
                self.errors,
                self.focused,
                area,
                cx,
                self.theme,
            )
        {
            return;
        }
        let style = if self.field.disabled {
            self.theme.muted
        } else if self.focused || self.errors.contains_key(&self.field.id) {
            self.theme.focused
        } else {
            self.theme.text
        };
        cx.write_line_with_fallback_style(
            LocalRect::terminal(area),
            &Line::raw(self.text.clone()),
            self.theme.background.patch(style),
        );
    }
}

#[allow(clippy::too_many_lines)] // Form rendering keeps pagination, canonical values, controls, errors, and action rows synchronized.
fn render_form(
    active: &mut ActivePrompt,
    content: Rect,
    cx: &mut PaintCx<'_, '_>,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> bool {
    let PromptField::Form {
        sections,
        paged_on_small,
        ..
    } = &active.envelope.request.field
    else {
        return false;
    };
    let PromptWidgetState::Form {
        cursor,
        scroll,
        values,
        editors,
        errors,
        page,
    } = &mut active.state
    else {
        return false;
    };
    let small_paged = *paged_on_small && (content.width < 24 || content.height < 8);
    let visible_sections = if small_paged {
        sections.get(*page).map_or(
            &[] as &[crate::runtime::prompt::PromptFormSection],
            std::slice::from_ref,
        )
    } else {
        sections.as_slice()
    };
    let page_offset = if small_paged {
        sections
            .iter()
            .take(*page)
            .map(|section| section.fields.len())
            .sum()
    } else {
        0
    };
    let fields = flatten_form_fields(visible_sections);
    let rows = form_render_rows(visible_sections, values, editors, errors);
    let actions_height = u16::from(content.height > 1);
    let fields_area = Rect::new(
        content.x,
        content.y,
        content.width,
        content.height.saturating_sub(actions_height),
    );
    let mut column = Column::new().id("prompt.form.fields");
    for (index, row) in rows.iter().enumerate() {
        column = column.child(FormFieldComponent {
            id: format!("prompt.form.field.{}", index.saturating_add(page_offset)),
            field: fields[index],
            values,
            editors,
            errors,
            focused: index.saturating_add(page_offset) == *cursor,
            text: row.text.clone(),
            theme,
        });
    }
    let retained_scroll = Cell::new(*scroll);
    let viewport = ScrollViewComponent::new(
        "prompt.form.viewport",
        LogicalSize::terminal(fields_area.size()),
        *scroll,
        column,
    )
    .retain_state(&retained_scroll)
    .reveal(format!("prompt.form.field.{cursor}"));
    paint_retained_scroll(&viewport, fields_area, &mut active.scroll_layout, cx);
    *scroll = retained_scroll.get();
    if actions_height > 0 {
        let actions = [
            ActionButton::new("submit", active.envelope.request.submit_label.clone()),
            ActionButton::new("cancel", active.envelope.request.cancel_label.clone()),
        ];
        let state = Cell::new({
            let mut state = ActionRowState::new();
            state.set_focused(Some(0));
            state
        });
        let area = Rect::new(
            content.x,
            content.bottom().saturating_sub(1),
            content.width,
            1,
        );
        let component =
            ActionRowComponent::new("prompt.form.actions", &actions, &state).styles(ButtonStyles {
                normal: theme.background,
                focused: theme.background.patch(theme.focused),
                hovered: theme.background,
                pressed: theme.background,
                disabled: theme.background,
            });
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        cx.with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&layout, cx),
        );
    }
    true
}

fn render_multi_toggle(
    active: &mut ActivePrompt,
    content: Rect,
    cx: &mut PaintCx<'_, '_>,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> bool {
    let PromptField::MultiToggle { options, .. } = &active.envelope.request.field else {
        return false;
    };
    let PromptWidgetState::MultiToggle {
        cursor,
        selected,
        scroll,
    } = &mut active.state
    else {
        return false;
    };
    let states = options
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let mut state = CheckboxState::new(selected.contains(&index));
            state.set_focused(index == *cursor);
            Cell::new(state)
        })
        .collect::<Vec<_>>();
    let mut column = Column::new().id("prompt.multi-toggle.options");
    for (index, (option, state)) in options.iter().zip(&states).enumerate() {
        column = column.child(
            SizeBox::new(
                CheckboxComponent::new(
                    format!("prompt.multi-toggle.{index}"),
                    &option.label,
                    state,
                )
                .styles(CheckboxStyles {
                    normal: theme.text,
                    focused: theme.focused,
                    hovered: theme.focused,
                    pressed: theme.focused,
                    disabled: theme.muted,
                })
                .fallback_style(theme.background),
            )
            .height(1),
        );
    }
    let content = Rect::new(
        content.x,
        content.y,
        content.width,
        content.height.saturating_sub(u16::from(content.height > 1)),
    );
    let retained = Cell::new(*scroll);
    let viewport = ScrollViewComponent::new(
        "prompt.multi-toggle.viewport",
        LogicalSize::terminal(content.size()),
        *scroll,
        column,
    )
    .retain_state(&retained)
    .reveal(format!("prompt.multi-toggle.{cursor}"));
    paint_retained_scroll(&viewport, content, &mut active.scroll_layout, cx);
    *scroll = retained.get();
    true
}

fn render_confirm(
    active: &ActivePrompt,
    area: Rect,
    cx: &mut PaintCx<'_, '_>,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> bool {
    let PromptField::Confirm {
        yes_label,
        no_label,
        ..
    } = &active.envelope.request.field
    else {
        return false;
    };
    let PromptWidgetState::Confirm { selected_yes } = active.state else {
        return false;
    };
    let body = active
        .envelope
        .request
        .message
        .as_ref()
        .map(|message| message.lines().map(Line::raw).collect::<Vec<_>>())
        .unwrap_or_default();
    let actions = [
        ActionButton::new("yes", yes_label.clone()),
        ActionButton::new("no", no_label.clone()),
    ];
    let mut actions_state = ActionRowState::new();
    actions_state.set_focused(Some(usize::from(!selected_yes)));
    let actions_state = std::cell::Cell::new(actions_state);
    let dialog = Dialog::new(&body, &actions, theme)
        .title(active.envelope.request.title.clone())
        .sizing(ModalSizing::fixed(
            Size::new(area.width, area.height),
            Insets::new(0, 0, 0, 0),
        ))
        .padding(Insets::new(0, 1, 0, 1));
    let component = DialogComponent::new("prompt.confirm", dialog, &actions_state);
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    cx.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
    true
}

fn render_text_input(
    active: &ActivePrompt,
    content: Rect,
    cx: &mut PaintCx<'_, '_>,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> bool {
    let PromptField::TextInput {
        placeholder,
        required,
        ..
    } = &active.envelope.request.field
    else {
        return false;
    };
    let PromptWidgetState::TextInput { buffer, error } = &active.state else {
        return false;
    };
    let state = std::cell::RefCell::new(TextInputState::new(buffer.clone()));
    let mut component = TextInputBoxComponent::new(
        "prompt.text-input",
        TextInputPolicy::chat_composer(),
        &state,
    )
    .required(*required)
    .policy(TextInputBoxPolicy::field().focused(true).rows(1, Some(1)))
    .styles(TextInputBoxStyles {
        text: theme.text,
        focused_text: theme.text,
        disabled_text: theme.muted,
        placeholder: theme.muted,
        selection: theme.focused,
        border: theme.border,
        focused_border: theme.focused,
        background: theme.background,
        focused_background: theme.background,
        disabled_background: theme.background,
    });
    if let Some(placeholder) = placeholder {
        component = component.placeholder(placeholder);
    }
    if let Some(error) = error {
        component = component.error(error);
    }
    paint_component(&component, content, cx);
    true
}

// Page through measured rows, not option counts. Before the first render (or
// after filtering invalidates the selected key), fall back to one-item movement.
fn search_select_page_target(
    list: &VirtualListState<usize>,
    filtered: &[usize],
    selected: usize,
    viewport_height: u64,
    forward: bool,
) -> usize {
    let final_index = filtered.len().saturating_sub(1);
    let selected = selected.min(final_index);
    let adjacent = if forward {
        selected.saturating_add(1).min(final_index)
    } else {
        selected.saturating_sub(1)
    };
    // Filtering may change between input events without a render. Reuse keyed
    // heights, but never reuse offsets that include now-hidden options.
    let mut target = selected;
    let mut remaining = viewport_height.max(1);
    while if forward {
        target < final_index
    } else {
        target > 0
    } {
        let measured = if forward { target } else { target - 1 };
        let Some(height) = filtered.get(measured).and_then(|key| list.item_height(key)) else {
            return adjacent;
        };
        if forward && height > remaining {
            break;
        }
        target = if forward { target + 1 } else { target - 1 };
        remaining = remaining.saturating_sub(height);
        if remaining == 0 {
            break;
        }
    }
    if forward {
        target.max(adjacent)
    } else {
        target.min(adjacent)
    }
}

fn sync_search_select_width(
    list: &VirtualList<'_, usize>,
    probe: &mut VirtualListState<usize>,
    painted: &mut VirtualListState<usize>,
    available_width: u16,
    viewport_height: u64,
) -> u16 {
    list.sync(u64::from(available_width), probe, &mut LayoutCx::new());
    let gutter = available_width > 1 && probe.total_height() > viewport_height;
    let width = available_width.saturating_sub(u16::from(gutter));
    list.sync(u64::from(width), painted, &mut LayoutCx::new());
    width
}

fn search_select_label(
    option: &crate::runtime::prompt::PromptOption,
    is_command_palette: bool,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> Line {
    let mut spans = vec![Span::styled(option.label.clone(), theme.text)];
    if let Some(key_hint) = &option.key_hint
        && is_command_palette
    {
        spans.push(Span::styled(format!("  {key_hint}"), theme.focused));
    }
    if let Some(detail) = &option.detail {
        spans.push(Span::styled(format!("  —  {detail}"), theme.muted));
    }
    Line::from_spans(spans)
}

fn render_single_select(
    active: &mut ActivePrompt,
    content: Rect,
    cx: &mut PaintCx<'_, '_>,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> bool {
    let PromptField::SingleSelect { options, .. } = &active.envelope.request.field else {
        return false;
    };
    let PromptWidgetState::SingleSelect {
        selected,
        list: state,
    } = &mut active.state
    else {
        return false;
    };
    let items = options.iter().enumerate().fold(
        VirtualList::new("single-select-list"),
        |list, (index, option)| {
            let style = if index == *selected {
                theme
                    .focused
                    .add_modifier(bmux_tui::style::Modifier::REVERSED)
            } else {
                theme.text
            };
            let detail = option
                .detail
                .as_ref()
                .map_or_else(String::new, |detail| format!("  —  {detail}"));
            list.component(
                index,
                TextBlock::new(Text::from_lines([Line::from_spans(vec![
                    Span::styled(option.label.clone(), style),
                    Span::styled(
                        detail,
                        if index == *selected {
                            theme.muted.patch(style)
                        } else {
                            theme.muted
                        },
                    ),
                ])]))
                .id(format!("single-select-list.label.{index}"))
                .style(theme.background),
            )
        },
    );
    if active.manual_list_scroll {
        state.capture_anchor();
    }
    items.sync(u64::from(content.width), state, &mut LayoutCx::new());
    active.list_viewport = content;
    if active.manual_list_scroll {
        state.restore_anchor(u64::from(content.height));
    } else if !options.is_empty() {
        items.ensure_item_visible(state, selected, u64::from(content.height));
    }
    cx.with_child(
        i32::from(content.x),
        i64::from(content.y),
        LocalRect::new(0, 0, content.width, content.height),
        |cx| {
            items.paint(Rect::new(0, 0, content.width, content.height), state, cx);
        },
    );
    true
}

#[allow(clippy::too_many_lines)] // Palette composition keeps one layout source for rendering, hits, cursor, footer, and scrollbar.
fn render_command_palette(
    active: &mut ActivePrompt,
    content: Rect,
    cx: &mut PaintCx<'_, '_>,
    theme: bmux_tui_components::modal_frame::ModalTheme,
) -> bool {
    let PromptField::SearchSelect {
        options,
        placeholder,
        match_mode,
        ..
    } = &active.envelope.request.field
    else {
        return false;
    };
    let PromptWidgetState::SearchSelect {
        query,
        selected,
        list: list_state,
        width_probe,
        viewport_height,
        viewport_width,
    } = &mut active.state
    else {
        return false;
    };
    let is_command_palette = active.envelope.request.modal_id.as_deref() == Some("command-palette");
    let items = options
        .iter()
        .map(|option| search_select_label(option, is_command_palette, theme))
        .collect::<Vec<_>>();
    let filtered = filtered_option_indices(options, query.text(), *match_mode);
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
            cx.write_line_with_fallback_style(
                LocalRect::terminal(Rect::new(
                    content.x,
                    content.y.saturating_add(row),
                    content.width,
                    1,
                )),
                &Line::raw(line),
                theme.muted,
            );
        }
    }
    let palette_area = Rect::new(content.x, palette_y, content.width, palette_height);
    let list_viewport = palette_height.saturating_sub(2);
    *viewport_height = u64::from(list_viewport);
    let list =
        filtered
            .iter()
            .copied()
            .fold(VirtualList::new("command-list"), |list, source_index| {
                let style = if filtered.get(*selected) == Some(&source_index) {
                    theme
                        .focused
                        .add_modifier(bmux_tui::style::Modifier::REVERSED)
                } else {
                    theme.text
                };
                let mut line = items[source_index].clone();
                if filtered.get(*selected) == Some(&source_index) {
                    for span in &mut line.spans {
                        span.style = span.style.patch(style);
                    }
                }
                list.component(
                    source_index,
                    TextBlock::new(Text::from_lines([line]))
                        .id(format!("command-list.label.{source_index}"))
                        .style(style),
                )
            });
    // Decide overflow from the same measured rows used for painting and hits.
    *viewport_width = palette_area.width;
    if active.manual_list_scroll {
        list_state.capture_anchor();
    }
    let resolved_width = sync_search_select_width(
        &list,
        width_probe,
        list_state,
        palette_area.width,
        u64::from(list_viewport),
    );
    let show_scrollbar = resolved_width < palette_area.width;
    let component_area = Rect::new(
        palette_area.x,
        palette_area.y,
        palette_area.width.saturating_sub(u16::from(show_scrollbar)),
        palette_area.height,
    );
    let input_area = Rect::new(component_area.x, component_area.y, component_area.width, 1);
    let placeholder = placeholder.clone().unwrap_or_else(|| {
        if is_command_palette {
            "Search commands".to_owned()
        } else {
            "Search options".to_owned()
        }
    });
    let input_state = RefCell::new(TextInputState::new(query.clone()));
    let input_policy = TextInputPolicy::raw();
    let input = TextInputComponent::new("command-palette.query", &input_state, &input_policy)
        .placeholder(&placeholder, theme.muted)
        .focused(true);
    paint_component(&input, input_area, cx);

    let list_area = Rect::new(
        component_area.x,
        component_area.y.saturating_add(2),
        component_area.width,
        list_viewport,
    );
    active.list_viewport = list_area;
    if active.manual_list_scroll {
        list_state.restore_anchor(u64::from(list_area.height));
    } else if let Some(source_index) = filtered.get(*selected) {
        list.ensure_item_visible(list_state, source_index, u64::from(list_area.height));
    }
    if filtered.is_empty() {
        cx.write_line_with_fallback_style(
            LocalRect::terminal(list_area),
            &Line::raw("No matches"),
            theme.muted,
        );
    } else {
        cx.with_child(
            i32::from(list_area.x),
            i64::from(list_area.y),
            LocalRect::new(0, 0, list_area.width, list_area.height),
            |cx| {
                list.paint(
                    Rect::new(0, 0, list_area.width, list_area.height),
                    list_state,
                    cx,
                );
            },
        );
    }
    if show_scrollbar {
        let scrollbar_area = Rect::new(
            palette_area.right().saturating_sub(1),
            palette_area.y.saturating_add(2),
            1,
            list_viewport,
        );
        let scrollbar_state =
            std::cell::Cell::new(list_state.scrollbar_state(u64::from(list_viewport)));
        let scrollbar = ScrollbarComponent::new("command-palette-scrollbar", &scrollbar_state)
            .policy(ScrollbarPolicy::bare())
            .styles(ScrollbarStyles {
                begin: theme.muted,
                track: theme.muted,
                thumb: theme.focused,
                end: theme.muted,
            });
        paint_component(&scrollbar, scrollbar_area, cx);
    }
    if footer_rows > 0 {
        let selected = if filtered.is_empty() {
            0
        } else {
            selected.saturating_add(1)
        };
        let footer = if is_command_palette {
            format!(
                "{selected}/{} matches  •  ↑↓ navigate  •  Enter run  •  Esc close",
                filtered.len()
            )
        } else {
            format!(
                "{selected}/{} matches  •  ↑↓ navigate  •  Enter select  •  Esc cancel",
                filtered.len()
            )
        };
        cx.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(
                content.x,
                content.bottom().saturating_sub(1),
                content.width,
                1,
            )),
            &Line::raw(footer),
            theme.muted,
        );
    }
    true
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

fn validate_and_emit_form_change(
    envelope: &AttachPromptEnvelope,
    field: &PromptFormField,
    values: &BTreeMap<String, PromptFormValue>,
    errors: &mut BTreeMap<String, String>,
    live_preview: bool,
) {
    match validate_form_field(field, values) {
        Ok(()) => {
            errors.remove(&field.id);
            if live_preview {
                emit_form_changed(envelope, field, values);
            }
        }
        Err(message) => {
            errors.insert(field.id.clone(), message);
        }
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

fn adjust_form_integer(
    field: &PromptFormField,
    values: &mut BTreeMap<String, PromptFormValue>,
    delta: i64,
) -> bool {
    let PromptFormFieldKind::Integer { min, max, .. } = &field.kind else {
        return false;
    };
    let Some(PromptFormValue::Integer(value)) = values.get_mut(&field.id) else {
        return false;
    };
    *value = value.saturating_add(delta);
    if let Some(min) = min {
        *value = (*value).max(*min);
    }
    if let Some(max) = max {
        *value = (*value).min(*max);
    }
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
    let small = geometry.cols < 24 || geometry.rows < 8;
    let compact = !small && (geometry.cols < 72 || geometry.rows < 20);

    let content_width = prompt_estimated_width(request);
    let capped_max = request.width.max.max(request.width.min);
    let width = if small {
        geometry.cols as usize
    } else if compact && request.max_height.is_none() {
        (geometry.cols as usize).saturating_sub(2)
    } else {
        (content_width + 4)
            .max(usize::from(request.width.min.max(24)))
            .min(usize::from(capped_max.max(24)))
            .min((geometry.cols as usize).saturating_sub(2))
    };
    let estimated_lines = prompt_estimated_lines(request);
    let height = if small {
        geometry.rows as usize
    } else if compact && request.max_height.is_none() {
        (geometry.rows as usize).saturating_sub(2)
    } else {
        (estimated_lines + 4)
            .max(7)
            .min(usize::from(request.max_height.unwrap_or(u16::MAX).max(7)))
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
        PromptField::Form { resettable, .. } => {
            if resettable {
                format!(
                    "F6 hide/show | R reset | Tab move | Shift/Ctrl Left/Right adjust | Enter {} | Esc {}",
                    request.submit_label, request.cancel_label
                )
            } else {
                format!(
                    "F6 hide/show | Tab/Up/Down move | Shift/Ctrl Left/Right adjust | Enter {} | Esc {}",
                    request.submit_label, request.cancel_label
                )
            }
        }
    }
}

fn filtered_option_indices(
    options: &[PromptOption],
    query: &str,
    match_mode: PromptSearchMatchMode,
) -> Vec<usize> {
    let mut scored = options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            let search_text = option
                .search_text
                .as_deref()
                .unwrap_or(option.label.as_str());
            search_score(query, search_text, match_mode)
                .map(|score| (index, score, option.label.as_str()))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        if let PromptSearchMatchMode::OrderedV1 { relevance, .. } = match_mode {
            let key = |index: usize| {
                options[index].search_order.map_or((0, index), |order| {
                    (
                        order.group,
                        if query.trim().is_empty() {
                            order.initial
                        } else {
                            order.filtered
                        },
                    )
                })
            };
            let left_key = key(left.0);
            let right_key = key(right.0);
            return left_key
                .0
                .cmp(&right_key.0)
                .then_with(|| {
                    if relevance && !query.trim().is_empty() {
                        right.1.cmp(&left.1)
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| left_key.1.cmp(&right_key.1))
                .then_with(|| left.0.cmp(&right.0));
        }
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.2.cmp(right.2))
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.into_iter().map(|(index, _, _)| index).collect()
}

fn search_score(query: &str, candidate: &str, match_mode: PromptSearchMatchMode) -> Option<i64> {
    let query = query.trim();
    if query.is_empty() {
        return Some(0);
    }
    match match_mode {
        PromptSearchMatchMode::OrderedV1 { matching, .. } => {
            use bmux_plugin_sdk::prompt::PromptOrderedMatchMode;
            search_score(
                query,
                candidate,
                match matching {
                    PromptOrderedMatchMode::Fuzzy => PromptSearchMatchMode::Fuzzy,
                    PromptOrderedMatchMode::Prefix => PromptSearchMatchMode::Prefix,
                    PromptOrderedMatchMode::Substring => PromptSearchMatchMode::Substring,
                },
            )
        }
        PromptSearchMatchMode::Fuzzy => fuzzy_score(query, candidate),
        PromptSearchMatchMode::Prefix => candidate
            .to_ascii_lowercase()
            .starts_with(&query.trim().to_ascii_lowercase())
            .then_some(0),
        PromptSearchMatchMode::Substring => candidate
            .to_ascii_lowercase()
            .contains(&query.trim().to_ascii_lowercase())
            .then_some(0),
    }
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
        AttachInternalPromptAction, AttachPromptState, PromptKeyDisposition, PromptWidgetState,
        filtered_option_indices, prompt_overlay_layout,
    };
    use crate::runtime::attach::input::TerminalGeometry;
    use crate::runtime::attach::tui_surface::{component_theme, parse_tui_color};
    use crate::runtime::prompt::{
        PromptField, PromptFormField, PromptFormFieldKind, PromptFormSection, PromptFormValue,
        PromptOption, PromptRequest, PromptResponse, PromptSearchMatchMode, PromptValidation,
        PromptValue,
    };
    use bmux_appearance::RuntimeAppearance;
    use bmux_plugin::prompt::PromptHostRequest;
    use bmux_tui::style::Color;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use tokio::sync::{mpsc, oneshot};
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
    fn paged_form_click_targets_global_field_after_resize() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::form(
                "Paged",
                vec![
                    PromptFormSection::new(
                        "one",
                        "One",
                        vec![PromptFormField::new(
                            "first",
                            "First",
                            PromptFormFieldKind::Bool { default: false },
                        )],
                    ),
                    PromptFormSection::new(
                        "two",
                        "Two",
                        vec![PromptFormField::new(
                            "second",
                            "Second",
                            PromptFormFieldKind::Bool { default: false },
                        )],
                    ),
                ],
            )
            .form_paged_on_small(true),
            AttachInternalPromptAction::QuitSession,
        );
        state.handle_key_event(&key_event(KeyCode::PageDown));
        for geometry in [
            TerminalGeometry { cols: 24, rows: 10 },
            TerminalGeometry { cols: 80, rows: 24 },
        ] {
            state
                .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
                .expect("render");
            let active = state.active.as_ref().expect("active");
            let area = active
                .hits
                .regions()
                .iter()
                .find(|hit| hit.id.as_str() == "prompt.form.field.1")
                .expect("second field visible")
                .area;
            state.handle_mouse_event(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                },
                geometry,
            );
            let PromptWidgetState::Form { cursor, values, .. } =
                &state.active.as_ref().expect("active").state
            else {
                panic!("form");
            };
            assert_eq!(*cursor, 1);
            assert_eq!(values.get("first"), Some(&PromptFormValue::Bool(false)));
        }
    }

    #[test]
    fn small_paged_form_switches_sections_without_losing_values() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::form(
                "Paged",
                vec![
                    PromptFormSection::new(
                        "one",
                        "One",
                        vec![PromptFormField::new(
                            "first",
                            "First",
                            PromptFormFieldKind::Integer {
                                initial_value: 1,
                                min: Some(0),
                                max: Some(20),
                            },
                        )],
                    ),
                    PromptFormSection::new(
                        "two",
                        "Two",
                        vec![PromptFormField::new(
                            "second",
                            "Second",
                            PromptFormFieldKind::Integer {
                                initial_value: 2,
                                min: Some(0),
                                max: Some(20),
                            },
                        )],
                    ),
                    PromptFormSection::new(
                        "three",
                        "Three",
                        vec![PromptFormField::new(
                            "third",
                            "Third",
                            PromptFormFieldKind::Integer {
                                initial_value: 3,
                                min: Some(0),
                                max: Some(20),
                            },
                        )],
                    ),
                ],
            )
            .form_paged_on_small(true),
            AttachInternalPromptAction::QuitSession,
        );

        let _ = state.handle_key_event(&key_event(KeyCode::PageDown));
        let _ = state.handle_key_event(&key_event(KeyCode::PageDown));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));
        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected completion");
        };
        let PromptResponse::Submitted(PromptValue::Form(values)) = completion.response else {
            panic!("expected form values");
        };
        assert_eq!(values.get("first"), Some(&PromptFormValue::Integer(1)));
        assert_eq!(values.get("second"), Some(&PromptFormValue::Integer(2)));
        assert_eq!(values.get("third"), Some(&PromptFormValue::Integer(3)));
    }

    #[test]
    fn form_overlay_uses_centered_full_compact_and_small_layouts() {
        let request = PromptRequest::form(
            "Padding",
            vec![PromptFormSection::new(
                "padding",
                "Padding",
                (0..12)
                    .map(|index| {
                        PromptFormField::new(
                            format!("field-{index}"),
                            format!("Field {index}"),
                            PromptFormFieldKind::Integer {
                                initial_value: 0,
                                min: Some(0),
                                max: Some(100),
                            },
                        )
                    })
                    .collect(),
            )],
        )
        .width_range(52, 88);

        let large = prompt_overlay_layout(
            Some(&request),
            TerminalGeometry {
                cols: 140,
                rows: 50,
            },
        )
        .expect("large layout");
        assert!(large.surface.rect.x > 0);
        assert!(large.surface.rect.y > 0);
        assert!(large.surface.rect.w >= 52);
        assert!(large.surface.rect.w <= 88);

        let compact =
            prompt_overlay_layout(Some(&request), TerminalGeometry { cols: 60, rows: 18 })
                .expect("compact layout");
        assert_eq!(compact.surface.rect.x, 1);
        assert_eq!(compact.surface.rect.w, 58);
        assert_eq!(compact.surface.rect.y, 1);
        assert_eq!(compact.surface.rect.h, 16);

        let small = prompt_overlay_layout(Some(&request), TerminalGeometry { cols: 20, rows: 6 })
            .expect("small layout");
        assert_eq!((small.surface.rect.x, small.surface.rect.y), (0, 0));
        assert_eq!((small.surface.rect.w, small.surface.rect.h), (20, 6));
    }

    #[test]
    fn bounded_prompt_layout_adapts_to_terminal_and_entry_count() {
        let request = PromptRequest::search_select(
            "Search",
            (0..100)
                .map(|index| PromptOption::new(index.to_string(), "Short"))
                .collect(),
        )
        .width_range(90, 90)
        .max_height(30);
        for (cols, rows, width, height) in [
            (200, 60, 90, 30),
            (120, 40, 90, 30),
            (80, 24, 78, 22),
            (60, 18, 58, 16),
            (200, 18, 90, 16),
            (60, 60, 58, 30),
            (20, 6, 20, 6),
        ] {
            let layout = prompt_overlay_layout(Some(&request), TerminalGeometry { cols, rows })
                .expect("layout");
            let rect = layout.surface.rect;
            assert_eq!((rect.w, rect.h), (width, height));
            assert_eq!((rect.x, rect.y), ((cols - width) / 2, (rows - height) / 2));
        }
        let short = PromptRequest::search_select("Search", vec![PromptOption::new("one", "One")])
            .width_range(90, 90)
            .max_height(30);
        let layout = prompt_overlay_layout(
            Some(&short),
            TerminalGeometry {
                cols: 200,
                rows: 60,
            },
        )
        .expect("short layout");
        assert_eq!((layout.surface.rect.w, layout.surface.rect.h), (90, 7));
    }

    #[test]
    fn command_palette_query_cursor_tracks_caller_owned_edits() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Command Palette",
                vec![PromptOption::new("commit", "Commit")],
            )
            .modal_id("command-palette"),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 80, rows: 24 };

        let empty = state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("empty palette should render");
        let empty_cursor = empty.cursor_state.expect("empty query cursor");

        for ch in "co".chars() {
            let _ = state.handle_key_event(&key_event(KeyCode::Char(ch)));
        }
        let edited = state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("edited palette should render");
        let edited_cursor = edited.cursor_state.expect("edited query cursor");

        assert!(empty_cursor.visible);
        assert!(edited_cursor.visible);
        assert_eq!(edited_cursor.y, empty_cursor.y);
        assert_eq!(edited_cursor.x, empty_cursor.x.saturating_add(2));
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
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("compact palette should render");

        assert_eq!(render.surface.rect.x, 0);
        assert_eq!(render.surface.rect.y, 0);
        assert_eq!(render.surface.rect.w, 20);
        assert_eq!(render.surface.rect.h, 6);
    }

    #[test]
    fn prompt_highlight_tracks_arrow_navigation() {
        for mode in 0..3 {
            let options = vec![
                PromptOption::new("one", "First label")
                    .detail("First detail")
                    .key_hint("F1"),
                PromptOption::new("two", "Second label")
                    .detail("Second detail")
                    .key_hint("F2"),
            ];
            let request = match mode {
                0 => PromptRequest::search_select("Search", options),
                1 => PromptRequest::search_select("Search", options).modal_id("command-palette"),
                _ => PromptRequest::single_select("Select", options),
            };
            let mut state = AttachPromptState::default();
            state.enqueue_internal(request, AttachInternalPromptAction::QuitSession);
            let geometry = TerminalGeometry { cols: 80, rows: 24 };
            let appearance = RuntimeAppearance::default();
            let before = state
                .attach_prompt_overlay_render(geometry, &appearance, false)
                .unwrap();
            state.handle_key_event(&key_event(KeyCode::Down));
            let after = state
                .attach_prompt_overlay_render(geometry, &appearance, false)
                .unwrap();
            let style_for = |render: &super::AttachPromptOverlayRender, needle: &str| {
                render
                    .ops
                    .iter()
                    .find_map(|op| match op {
                        bmux_plugin::RenderOp::TextRun { text, style, .. }
                            if text.contains(needle) =>
                        {
                            Some(*style)
                        }
                        _ => None,
                    })
                    .expect("rendered text")
            };
            for (first, second) in [
                ("First label", "Second label"),
                ("First detail", "Second detail"),
            ] {
                assert_ne!(style_for(&before, first), style_for(&after, first));
                assert_eq!(style_for(&before, first), style_for(&after, second));
            }
            if mode == 1 {
                assert!(style_for(&before, "F1").reverse);
                assert!(!style_for(&after, "F1").reverse);
                assert!(style_for(&after, "F2").reverse);
            }
            state.handle_key_event(&key_event(KeyCode::Up));
            let restored = state
                .attach_prompt_overlay_render(geometry, &appearance, false)
                .unwrap();
            assert_eq!(
                style_for(&before, "First label"),
                style_for(&restored, "First label")
            );
        }
    }

    fn modal_focus_cases() -> Vec<(PromptRequest, KeyCode)> {
        let form = PromptRequest::form(
            "Form",
            vec![PromptFormSection::new(
                "main",
                "Main",
                vec![
                    PromptFormField::new(
                        "first",
                        "First",
                        PromptFormFieldKind::SingleSelect {
                            options: vec![PromptOption::new("first", "First choice")],
                            default_index: 0,
                        },
                    ),
                    PromptFormField::new(
                        "second",
                        "Second",
                        PromptFormFieldKind::Bool { default: false },
                    ),
                ],
            )],
        );
        let mut cases = vec![
            (
                PromptRequest::confirm("Confirm").confirm_labels("First", "Second"),
                KeyCode::Tab,
            ),
            (
                PromptRequest::multi_toggle(
                    "Toggle",
                    vec![
                        PromptOption::new("first", "First"),
                        PromptOption::new("second", "Second"),
                    ],
                ),
                KeyCode::Down,
            ),
            (form, KeyCode::Tab),
        ];
        for kind in [
            PromptFormFieldKind::Text {
                initial_value: "First value".to_owned(),
                placeholder: None,
                validation: None,
            },
            PromptFormFieldKind::Number {
                initial_value: "12345".to_owned(),
                min: None,
                max: None,
            },
            PromptFormFieldKind::Integer {
                initial_value: 12345,
                min: None,
                max: None,
            },
        ] {
            cases.push((
                PromptRequest::form(
                    "Edit",
                    vec![PromptFormSection::new(
                        "main",
                        "Main",
                        vec![
                            PromptFormField::new("first", "First", kind),
                            PromptFormField::new(
                                "second",
                                "Second",
                                PromptFormFieldKind::Bool { default: false },
                            ),
                        ],
                    )],
                ),
                KeyCode::Tab,
            ));
        }
        cases
    }

    #[test]
    fn modal_controls_keep_focus_visible_without_color_contrast() {
        for (request, key) in modal_focus_cases() {
            let first_label = match &request.field {
                PromptField::Form { sections, .. } => match &sections[0].fields[0].kind {
                    PromptFormFieldKind::Number { .. } | PromptFormFieldKind::Integer { .. } => {
                        "12345"
                    }
                    PromptFormFieldKind::Text { .. } => "First value",
                    _ => "First",
                },
                _ => "First",
            };
            let mut state = AttachPromptState::default();
            state.enqueue_internal(request, AttachInternalPromptAction::QuitSession);
            let appearance = RuntimeAppearance {
                foreground: "#ffffff".to_owned(),
                cursor: "#ffffff".to_owned(),
                ..RuntimeAppearance::default()
            };
            let geometry = TerminalGeometry { cols: 80, rows: 24 };
            let before = state
                .attach_prompt_overlay_render(geometry, &appearance, false)
                .unwrap();
            state.handle_key_event(&key_event(key));
            let after = state
                .attach_prompt_overlay_render(geometry, &appearance, false)
                .unwrap();
            let focused = |render: &super::AttachPromptOverlayRender, label: &str| {
                render.ops.iter().any(|op| {
                    matches!(op,
                    bmux_plugin::RenderOp::TextRun { text, style, .. }
                    if text.contains(label) && style.bold && style.underline)
                })
            };
            // Confirm defaults to No; other controls start at the first item.
            let initially_first = focused(&before, first_label);
            assert_ne!(initially_first, focused(&before, "Second"));
            assert_ne!(initially_first, focused(&after, first_label));
            assert_eq!(initially_first, focused(&after, "Second"));
        }
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
                false,
            )
            .expect("palette should render");

        assert!(render.ops.iter().any(|op| {
            matches!(op, bmux_plugin::RenderOp::TextRun { text, .. } if text.contains('█'))
        }));
    }

    #[test]
    fn search_select_pages_by_measured_rows() {
        use super::{
            LayoutCx, Line, Text, TextBlock, VirtualList, VirtualListState,
            search_select_page_target,
        };
        let list = VirtualList::new("paging")
            .component(
                10,
                TextBlock::new(Text::from_lines([Line::raw("abcdefgh")])).id("first"),
            )
            .component(
                20,
                TextBlock::new(Text::from_lines([Line::raw("ijklmnop")])).id("second"),
            )
            .component(
                30,
                TextBlock::new(Text::from_lines([Line::raw("qrstuvwx")])).id("third"),
            );
        let mut state = VirtualListState::new(0);
        list.sync(2, &mut state, &mut LayoutCx::new());
        assert_eq!(state.total_height(), 12);
        assert_eq!(state.item_height(&20), Some(4));
        assert_eq!(state.item_height(&99), None);
        let filtered = [10, 20, 30];
        // No sync between filtering/reordering and paging: offsets are stale,
        // but each retained keyed height remains usable.
        assert_eq!(
            search_select_page_target(&state, &[30, 10, 20], 0, 8, true),
            2
        );
        assert_eq!(
            search_select_page_target(&state, &[30, 10, 20], 2, 8, false),
            0
        );
        assert_eq!(search_select_page_target(&state, &filtered, 0, 4, true), 1);
        assert_eq!(search_select_page_target(&state, &filtered, 2, 4, false), 1);
        assert_eq!(search_select_page_target(&state, &filtered, 0, 1, true), 1);
        assert_eq!(search_select_page_target(&state, &filtered, 2, 4, true), 2);
        assert_eq!(search_select_page_target(&state, &filtered, 0, 4, false), 0);
        assert_eq!(search_select_page_target(&state, &[], 0, 4, true), 0);
        assert_eq!(search_select_page_target(&state, &[40, 50], 0, 4, true), 1);
    }

    #[test]
    fn search_select_reuses_gutter_and_full_width_measurements() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Search",
                vec![PromptOption::new("long", "wrapped label ".repeat(100))],
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 40, rows: 16 };
        let misses = |state: &AttachPromptState| {
            let PromptWidgetState::SearchSelect {
                list, width_probe, ..
            } = &state.active.as_ref().unwrap().state
            else {
                panic!("search select");
            };
            (
                list.layout_cache().stats().misses,
                width_probe.layout_cache().stats().misses,
            )
        };
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let initial = misses(&state);
        assert!(initial.0 > 0 && initial.1 > 0);
        for _ in 0..3 {
            state
                .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
                .unwrap();
            assert_eq!(misses(&state), initial);
        }
    }

    #[test]
    fn command_palette_scrollbar_uses_wrapped_row_extent() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Command Palette",
                vec![PromptOption::new("long", "wrapped label ".repeat(100))],
            )
            .modal_id("command-palette"),
            AttachInternalPromptAction::QuitSession,
        );
        let render = state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 40, rows: 16 },
                &RuntimeAppearance::default(),
                false,
            )
            .expect("palette should render");
        let active = state.active.as_ref().unwrap();
        let PromptWidgetState::SearchSelect {
            list,
            viewport_height,
            ..
        } = &active.state
        else {
            panic!("expected search select");
        };
        assert!(*viewport_height > 1);
        assert!(list.total_height() > *viewport_height);
        assert!(render.ops.iter().any(|op| {
            matches!(op, bmux_plugin::RenderOp::TextRun { text, .. } if text.contains('█'))
        }));
    }

    #[test]
    fn pasted_search_reveals_first_match_after_manual_scroll() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Search",
                (0..40)
                    .map(|i| PromptOption::new(i.to_string(), format!("Item {i:02}")))
                    .collect(),
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 40, rows: 16 };
        state.handle_key_event(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let area = state.active.as_ref().unwrap().list_viewport;
        state.handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
            geometry,
        );
        assert!(state.active.as_ref().unwrap().manual_list_scroll);
        state.handle_paste("Item 1");
        assert!(!state.active.as_ref().unwrap().manual_list_scroll);
        for _ in 0..2 {
            state
                .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
                .unwrap();
            let PromptWidgetState::SearchSelect { selected, list, .. } =
                &state.active.as_ref().unwrap().state
            else {
                panic!("search select");
            };
            assert_eq!(*selected, 0);
            assert_eq!(list.scroll.vertical_offset(), 0);
            assert_eq!(list.key_at_offset(0), Some(&10));
        }
    }

    #[test]
    fn manual_select_scroll_preserves_key_and_row_on_resize() {
        for searchable in [false, true] {
            let options = (0..40)
                .map(|i| {
                    PromptOption::new(
                        i.to_string(),
                        format!("Item {i:02} {}", "wrapped ".repeat(10)),
                    )
                })
                .collect();
            let request = if searchable {
                PromptRequest::search_select("Search", options)
            } else {
                PromptRequest::single_select("Select", options)
            };
            let mut state = AttachPromptState::default();
            state.enqueue_internal(request, AttachInternalPromptAction::QuitSession);
            let narrow = TerminalGeometry { cols: 40, rows: 16 };
            state
                .attach_prompt_overlay_render(narrow, &RuntimeAppearance::default(), false)
                .unwrap();
            let area = state.active.as_ref().unwrap().list_viewport;
            for _ in 0..5 {
                state.handle_mouse_event(
                    MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        column: area.x,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    narrow,
                );
            }
            let anchor = |state: &AttachPromptState| {
                let (PromptWidgetState::SingleSelect { list, .. }
                | PromptWidgetState::SearchSelect { list, .. }) =
                    &state.active.as_ref().unwrap().state
                else {
                    panic!("select");
                };
                let offset = list.scroll.vertical_offset();
                let key = *list.key_at_offset(offset).unwrap();
                (
                    key,
                    offset - list.item_offset(&key).unwrap(),
                    list.item_height(&key).unwrap(),
                )
            };
            let before = anchor(&state);
            state
                .attach_prompt_overlay_render(
                    TerminalGeometry { cols: 80, rows: 24 },
                    &RuntimeAppearance::default(),
                    false,
                )
                .unwrap();
            let after = anchor(&state);
            assert_eq!(after.0, before.0);
            assert_eq!(after.1, before.1.min(after.2 - 1));
        }
    }

    #[test]
    fn single_select_wheel_clamps_and_survives_repaint() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::single_select(
                "Select",
                (0..40)
                    .map(|i| PromptOption::new(i.to_string(), format!("Item {i}")))
                    .collect(),
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 80, rows: 16 };
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let area = state.active.as_ref().unwrap().list_viewport;
        for _ in 0..50 {
            state.handle_mouse_event(
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                },
                geometry,
            );
        }
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let PromptWidgetState::SingleSelect { list, selected } =
            &state.active.as_ref().unwrap().state
        else {
            panic!("select");
        };
        assert_eq!(*selected, 0);
        assert_eq!(
            list.scroll.vertical_offset(),
            list.total_height() - u64::from(area.height)
        );
        state.handle_key_event(&key_event(KeyCode::Down));
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let PromptWidgetState::SingleSelect { list, selected } =
            &state.active.as_ref().unwrap().state
        else {
            panic!("select");
        };
        assert_eq!(*selected, 1);
        assert_eq!(list.scroll.vertical_offset(), 1);
    }

    #[test]
    fn search_select_wheel_survives_render_and_keys_restore_reveal() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Search",
                (0..40)
                    .map(|i| PromptOption::new(i.to_string(), format!("Result {i:02}")))
                    .collect(),
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 80, rows: 24 };
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let area = state.active.as_ref().unwrap().list_viewport;
        state.handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
            geometry,
        );
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let PromptWidgetState::SearchSelect { list, selected, .. } =
            &state.active.as_ref().unwrap().state
        else {
            panic!("search");
        };
        assert_eq!(*selected, 0);
        assert_eq!(list.scroll.vertical_offset(), 3);
        state.handle_key_event(&key_event(KeyCode::Home));
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let PromptWidgetState::SearchSelect { list, .. } = &state.active.as_ref().unwrap().state
        else {
            panic!("search");
        };
        assert_eq!(list.scroll.vertical_offset(), 0);
    }

    #[test]
    fn search_select_non_selection_mouse_events_preserve_selection() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Search",
                vec![
                    PromptOption::new("one", "One"),
                    PromptOption::new("two", "Two"),
                ],
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 80, rows: 24 };
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let hit = state
            .active
            .as_ref()
            .unwrap()
            .hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "command-list.item.1")
            .unwrap()
            .area;
        for kind in [
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollUp,
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            state.handle_mouse_event(
                MouseEvent {
                    kind,
                    column: hit.x,
                    row: hit.y,
                    modifiers: KeyModifiers::NONE,
                },
                geometry,
            );
            let PromptWidgetState::SearchSelect { selected, .. } =
                &state.active.as_ref().unwrap().state
            else {
                panic!("search select");
            };
            assert_eq!(*selected, 0, "{kind:?} must not select the hovered item");
        }
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
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("palette should render");
        let active = state.active.as_ref().expect("active prompt");
        let hit = active
            .hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "command-list.item.1")
            .expect("second option hit");
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
                false,
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
    fn extension_owned_prompt_chrome_suppresses_fallback_border() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::text_input("Name"),
            AttachInternalPromptAction::QuitSession,
        );

        let render = state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 80, rows: 24 },
                &RuntimeAppearance::default(),
                true,
            )
            .expect("prompt should render");
        let top_y = render.surface.rect.y;
        let top = render.ops.iter().find_map(|op| match op {
            bmux_plugin::RenderOp::TextRun { y, text, .. } if *y == top_y => Some(text.as_str()),
            _ => None,
        });

        assert!(top.is_some_and(|line| !line.contains('╭') && !line.contains('╮')));
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
                false,
            )
            .expect("prompt should render");
        let top_y = render.surface.rect.y;
        let top_x = render.surface.rect.x;
        let top_runs = render
            .ops
            .iter()
            .filter_map(|op| match op {
                bmux_plugin::RenderOp::TextRun { x, y, text, .. } if *y == top_y => {
                    Some((*x, text.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            top_runs
                .first()
                .is_some_and(|(x, text)| *x == top_x && text.starts_with('╭'))
                && top_runs.last().is_some_and(|(_, text)| text.ends_with('╮'))
        );
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
    fn confirm_prompt_renders_dialog_actions() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::confirm("Prompt Showcase")
                .confirm_default(true)
                .confirm_labels("Continue", "Stop"),
            AttachInternalPromptAction::QuitSession,
        );

        let render = state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 80, rows: 24 },
                &RuntimeAppearance::default(),
                false,
            )
            .expect("confirm should render");
        let text = render
            .ops
            .iter()
            .filter_map(|op| match op {
                bmux_plugin::RenderOp::TextRun { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("Continue"), "rendered: {text:?}");
        assert!(text.contains("Stop"));
        assert!(!text.contains("[x]"));
        assert!(!text.contains("[ ]"));
    }

    #[test]
    fn ordered_search_preserves_ranks_and_group_before_relevance() {
        use bmux_plugin_sdk::prompt::{PromptOrderedMatchMode, PromptSearchOrder};
        let mut options = vec![
            PromptOption::new("recent", "e-x"),
            PromptOption::new("exact", "ex"),
            PromptOption::new("current", "example"),
        ];
        for (index, option) in options.iter_mut().enumerate() {
            option.search_order = Some(PromptSearchOrder {
                group: u8::from(index == 2),
                initial: index,
                filtered: index,
            });
        }
        let mode = |matching, relevance| PromptSearchMatchMode::OrderedV1 {
            matching,
            relevance,
        };
        assert_eq!(
            filtered_option_indices(&options, "", mode(PromptOrderedMatchMode::Fuzzy, false)),
            vec![0, 1, 2]
        );
        assert_eq!(
            filtered_option_indices(&options, "ex", mode(PromptOrderedMatchMode::Fuzzy, false)),
            vec![0, 1, 2]
        );
        assert_eq!(
            filtered_option_indices(&options, "ex", mode(PromptOrderedMatchMode::Fuzzy, true)),
            vec![1, 0, 2]
        );
        for matching in [
            PromptOrderedMatchMode::Prefix,
            PromptOrderedMatchMode::Substring,
        ] {
            assert_eq!(
                filtered_option_indices(&options, "e", mode(matching, false)),
                vec![0, 1, 2]
            );
            assert!(filtered_option_indices(&options, "missing", mode(matching, false)).is_empty());
        }
    }

    #[test]
    fn search_match_modes_filter_against_custom_search_text() {
        let options = vec![
            PromptOption::new("editor", "project/editor").search_text("project editor"),
            PromptOption::new("shell", "other/shell").search_text("other shell"),
        ];

        assert_eq!(
            filtered_option_indices(&options, "pro ed", PromptSearchMatchMode::Fuzzy),
            vec![0]
        );
        assert_eq!(
            filtered_option_indices(&options, "project", PromptSearchMatchMode::Prefix),
            vec![0]
        );
        assert!(
            filtered_option_indices(&options, "editor", PromptSearchMatchMode::Prefix).is_empty()
        );
        assert_eq!(
            filtered_option_indices(&options, "editor", PromptSearchMatchMode::Substring),
            vec![0]
        );
        assert_eq!(
            filtered_option_indices(&options, "", PromptSearchMatchMode::Prefix),
            vec![1, 0]
        );
    }

    #[test]
    fn command_palette_page_navigation_uses_rendered_virtual_viewport() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Command Palette",
                (0..40)
                    .map(|index| {
                        PromptOption::new(format!("command-{index}"), format!("Command {index}"))
                    })
                    .collect(),
            )
            .modal_id("command-palette"),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 80, rows: 16 };
        let _ = state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("palette should render");

        let _ = state.handle_key_event(&key_event(KeyCode::PageDown));
        let _ = state.handle_key_event(&key_event(KeyCode::PageDown));
        // Selection is revealed against measured item geometry during rendering.
        let _ = state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("palette should render updated selection");
        let active = state.active.as_ref().expect("active prompt");
        let expected_index = match &active.state {
            PromptWidgetState::SearchSelect {
                selected,
                list,
                viewport_height,
                ..
            } => {
                assert!(*viewport_height > 0);
                assert!(list.scroll.vertical_offset() > 0);
                let PromptField::SearchSelect {
                    options,
                    match_mode,
                    ..
                } = &active.envelope.request.field
                else {
                    panic!("expected search-select field");
                };
                filtered_option_indices(options, "", *match_mode)[*selected]
            }
            _ => panic!("expected search-select state"),
        };
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected prompt completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Single(format!("command-{expected_index}")))
        );
    }

    #[test]
    fn search_select_overflow_resize_preserves_pointer_activation() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Search",
                (0..30)
                    .map(|index| {
                        PromptOption::new(
                            index.to_string(),
                            format!("Result {index:02} {}", "wrapped ".repeat(8)),
                        )
                    })
                    .collect(),
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let narrow = TerminalGeometry { cols: 40, rows: 16 };
        state
            .attach_prompt_overlay_render(narrow, &RuntimeAppearance::default(), false)
            .unwrap();
        for _ in 0..40 {
            state.handle_key_event(&key_event(KeyCode::PageDown));
        }
        state
            .attach_prompt_overlay_render(narrow, &RuntimeAppearance::default(), false)
            .unwrap();
        let selected_key = |state: &AttachPromptState| {
            let active = state.active.as_ref().unwrap();
            let PromptWidgetState::SearchSelect {
                selected,
                list,
                viewport_height,
                ..
            } = &active.state
            else {
                panic!("search select");
            };
            assert_eq!(*selected, 29);
            assert!(list.total_height() > *viewport_height);
            assert!(list.scroll.vertical_offset() > 0);
            let PromptField::SearchSelect {
                options,
                match_mode,
                ..
            } = &active.envelope.request.field
            else {
                panic!("search select");
            };
            filtered_option_indices(options, "", *match_mode)[*selected]
        };
        let source = selected_key(&state);
        state.handle_key_event(&key_event(KeyCode::PageDown));
        assert_eq!(selected_key(&state), source);
        let wide = TerminalGeometry { cols: 80, rows: 24 };
        state
            .attach_prompt_overlay_render(wide, &RuntimeAppearance::default(), false)
            .unwrap();
        assert_eq!(selected_key(&state), source);
        let hit = state
            .active
            .as_ref()
            .unwrap()
            .hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == format!("command-list.item.{source}"))
            .expect("selected item remains visible after resize");
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.area.x,
            row: hit.area.y,
            modifiers: KeyModifiers::NONE,
        };
        let PromptKeyDisposition::Completed(completion) = state.handle_mouse_event(mouse, wide)
        else {
            panic!("click completes");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Single(source.to_string()))
        );
    }

    #[test]
    fn search_select_width_tracks_filter_overflow_transitions() {
        use super::{
            LayoutCx, Line, Text, TextBlock, VirtualList, VirtualListState,
            sync_search_select_width,
        };
        let mut probe = VirtualListState::new(0);
        let mut painted = VirtualListState::new(0);
        let short = VirtualList::new("command-list").component(
            0,
            TextBlock::new(Text::from_lines([Line::raw("abcde")])).id("label"),
        );
        assert_eq!(
            sync_search_select_width(&short, &mut probe, &mut painted, 5, 2),
            5
        );
        let long = short.component(
            1,
            TextBlock::new(Text::from_lines([Line::raw("abcdefghij")])).id("other"),
        );
        assert_eq!(
            sync_search_select_width(&long, &mut probe, &mut painted, 5, 2),
            4
        );
        assert_eq!(painted.item_height(&0), Some(2));
        assert_eq!(painted.item_height(&1), Some(3));
        let short = VirtualList::new("command-list").component(
            0,
            TextBlock::new(Text::from_lines([Line::raw("abcde")])).id("label"),
        );
        assert_eq!(
            sync_search_select_width(&short, &mut probe, &mut painted, 5, 2),
            5
        );
        assert_eq!(painted.item_height(&0), Some(1));
        assert_eq!(painted.item_height(&1), None);
        // Same resolved width remains a cache hit.
        let misses = painted.layout_cache().stats().misses;
        short.sync(5, &mut painted, &mut LayoutCx::new());
        assert_eq!(painted.layout_cache().stats().misses, misses);
    }

    #[test]
    fn search_select_reintroduced_results_are_measured_before_paging() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::search_select(
                "Search",
                (0..30)
                    .map(|index| PromptOption::new(index.to_string(), format!("Result {index:02}")))
                    .collect(),
            ),
            AttachInternalPromptAction::QuitSession,
        );
        state.handle_key_event(&key_event(KeyCode::Char('9')));
        state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 80, rows: 24 },
                &RuntimeAppearance::default(),
                false,
            )
            .unwrap();
        state.handle_key_event(&key_event(KeyCode::Backspace));
        state.handle_key_event(&key_event(KeyCode::PageDown));
        let PromptWidgetState::SearchSelect { selected, list, .. } =
            &state.active.as_ref().unwrap().state
        else {
            panic!("search select");
        };
        let PromptWidgetState::SearchSelect {
            viewport_height, ..
        } = &state.active.as_ref().unwrap().state
        else {
            unreachable!()
        };
        assert_eq!(
            *selected,
            usize::try_from((*viewport_height).min(29)).unwrap(),
            "one-row results page by exactly the viewport height"
        );
        assert!(
            list.item_height(&0).is_some(),
            "reintroduced option measured before repaint"
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
    fn pasted_text_preserves_component_cursor_across_widths() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::text_input("Value"),
            AttachInternalPromptAction::QuitSession,
        );
        assert!(matches!(
            state.handle_paste("abcdefghij"),
            PromptKeyDisposition::Consumed
        ));

        let narrow = state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 24, rows: 8 },
                &RuntimeAppearance::default(),
                false,
            )
            .expect("narrow input should render");
        let wide = state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 80, rows: 24 },
                &RuntimeAppearance::default(),
                false,
            )
            .expect("wide input should render");

        assert!(narrow.cursor_state.is_some());
        assert!(wide.cursor_state.is_some());
        assert!(
            wide.cursor_state.expect("wide cursor").x
                >= narrow.cursor_state.expect("narrow cursor").x
        );
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
    fn single_select_wrapped_row_click_uses_retained_item_geometry() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::single_select(
                "Choose",
                vec![
                    PromptOption::new(
                        "first",
                        "A long option label that wraps across several terminal rows",
                    ),
                    PromptOption::new("second", "Second"),
                ],
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 28, rows: 20 };
        let mut previous = None;
        let mut previous_misses = None;
        for _ in 0..2 {
            state
                .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
                .expect("render");
            let active = state.active.as_ref().expect("active");
            let region = active
                .hits
                .regions()
                .iter()
                .find(|hit| hit.id.as_str() == "single-select-list.item.0")
                .expect("first item");
            assert!(region.area.height > 1, "fixture must wrap");
            if let Some(area) = previous {
                assert_eq!(region.area, area);
            }
            previous = Some(region.area);
            let PromptWidgetState::SingleSelect { list, .. } = &active.state else {
                panic!("single select");
            };
            assert!(list.total_height() > 2);
            let misses = list.layout_cache().stats().misses;
            assert!(misses > 0);
            if let Some(previous) = previous_misses {
                assert_eq!(
                    misses, previous,
                    "unchanged render must reuse measured layouts"
                );
            }
            previous_misses = Some(misses);
        }
        let area = previous.expect("painted item");
        let outcome = state.handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y + 1,
                modifiers: KeyModifiers::NONE,
            },
            geometry,
        );
        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Single("first".to_string()))
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
    fn form_focus_reveals_last_field_and_clicks_resolved_geometry() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::form(
                "Settings",
                vec![PromptFormSection::new(
                    "general",
                    "General",
                    (0..30)
                        .map(|index| {
                            PromptFormField::new(
                                format!("field-{index}"),
                                format!("Field {index}"),
                                PromptFormFieldKind::Bool { default: false },
                            )
                        })
                        .collect(),
                )],
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let PromptWidgetState::Form { cursor, .. } =
            &mut state.active.as_mut().expect("active").state
        else {
            panic!("form");
        };
        *cursor = 29;
        let geometry = TerminalGeometry { cols: 80, rows: 16 };
        for _ in 0..2 {
            state
                .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
                .expect("render");
        }
        let active = state.active.as_ref().expect("active");
        let PromptWidgetState::Form { scroll, .. } = &active.state else {
            panic!("form");
        };
        assert!(scroll.vertical_offset() > 0);
        assert_eq!(active.scroll_layout.len(), 1);
        assert_eq!(active.scroll_layout.stats().misses, 1);
        assert!(active.scroll_layout.stats().hits > 0);
        let area = active
            .hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "prompt.form.field.29")
            .unwrap_or_else(|| panic!("last field visible: {:?}", active.hits.regions()))
            .area;
        state.handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
            geometry,
        );
        let PromptWidgetState::Form { values, .. } = &state.active.as_ref().expect("active").state
        else {
            panic!("form");
        };
        assert_eq!(values.get("field-29"), Some(&PromptFormValue::Bool(true)));
        assert_eq!(values.get("field-0"), Some(&PromptFormValue::Bool(false)));
    }

    #[test]
    fn form_mouse_click_focuses_fields_and_toggles_boole() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::form(
                "Settings",
                vec![PromptFormSection::new(
                    "general",
                    "General",
                    vec![
                        PromptFormField::new(
                            "enabled",
                            "Enabled",
                            PromptFormFieldKind::Bool { default: false },
                        ),
                        PromptFormField::new(
                            "count",
                            "Count",
                            PromptFormFieldKind::Integer {
                                initial_value: 2,
                                min: Some(0),
                                max: Some(20),
                            },
                        ),
                    ],
                )],
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 80, rows: 24 };
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("form should paint interaction geometry");
        let surface = state.overlay_surface(geometry).expect("overlay");
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: surface.rect.x.saturating_add(2),
            row: surface.rect.y.saturating_add(1),
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            state.handle_mouse_event(mouse, geometry),
            PromptKeyDisposition::Consumed
        ));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));
        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected completion");
        };
        let PromptResponse::Submitted(PromptValue::Form(values)) = completion.response else {
            panic!("expected form values");
        };
        assert_eq!(values.get("enabled"), Some(&PromptFormValue::Bool(true)));
    }

    #[test]
    fn form_keyboard_navigation_and_integer_steps_work() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::form(
                "Settings",
                vec![PromptFormSection::new(
                    "general",
                    "General",
                    vec![
                        PromptFormField::new(
                            "first",
                            "First",
                            PromptFormFieldKind::Integer {
                                initial_value: 1,
                                min: Some(0),
                                max: Some(20),
                            },
                        ),
                        PromptFormField::new(
                            "second",
                            "Second",
                            PromptFormFieldKind::Integer {
                                initial_value: 2,
                                min: Some(0),
                                max: Some(20),
                            },
                        ),
                    ],
                )],
            ),
            AttachInternalPromptAction::QuitSession,
        );

        let _ = state.handle_key_event(&key_event(KeyCode::Tab));
        let _ = state.handle_key_event(&modified_key_event(KeyCode::Right, KeyModifiers::SHIFT));
        let _ = state.handle_key_event(&modified_key_event(KeyCode::Right, KeyModifiers::CONTROL));
        let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

        let PromptKeyDisposition::Completed(completion) = outcome else {
            panic!("expected completion");
        };
        let PromptResponse::Submitted(PromptValue::Form(values)) = completion.response else {
            panic!("expected form values");
        };
        assert_eq!(values.get("first"), Some(&PromptFormValue::Integer(1)));
        assert_eq!(values.get("second"), Some(&PromptFormValue::Integer(17)));
    }

    #[test]
    fn form_temporary_hide_keeps_prompt_active_and_consumes_input() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::form(
                "Settings",
                vec![PromptFormSection::new(
                    "general",
                    "General",
                    vec![PromptFormField::new(
                        "enabled",
                        "Enabled",
                        PromptFormFieldKind::Bool { default: false },
                    )],
                )],
            ),
            AttachInternalPromptAction::QuitSession,
        );
        let geometry = TerminalGeometry { cols: 80, rows: 24 };
        assert!(state.overlay_surface(geometry).is_some());

        assert!(matches!(
            state.handle_key_event(&key_event(KeyCode::F(6))),
            PromptKeyDisposition::Consumed
        ));
        assert!(state.is_active());
        assert!(state.overlay_surface(geometry).is_none());
        assert!(matches!(
            state.handle_key_event(&key_event(KeyCode::Char(' '))),
            PromptKeyDisposition::Consumed
        ));

        let _ = state.handle_key_event(&key_event(KeyCode::F(6)));
        assert!(state.overlay_surface(geometry).is_some());
    }

    #[test]
    fn invalid_live_form_edit_shows_error_without_emitting_preview() {
        let mut state = AttachPromptState::default();
        let (response_tx, _response_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        state.enqueue_external(PromptHostRequest {
            request: PromptRequest::form(
                "Padding",
                vec![PromptFormSection::new(
                    "limits",
                    "Limits",
                    vec![PromptFormField::new(
                        "width",
                        "Width",
                        PromptFormFieldKind::Integer {
                            initial_value: 1,
                            min: Some(1),
                            max: Some(10),
                        },
                    )],
                )],
            )
            .form_live_preview(true),
            response_tx,
            event_tx: Some(event_tx),
        });

        let _ = state.handle_key_event(&key_event(KeyCode::Char(' ')));

        assert!(event_rx.try_recv().is_err());
        let render = state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 80, rows: 24 },
                &RuntimeAppearance::default(),
                false,
            )
            .expect("form should render");
        let text = render
            .ops
            .iter()
            .filter_map(|op| match op {
                bmux_plugin::RenderOp::TextRun { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("value is out of range"), "rendered: {text:?}");
    }

    #[test]
    fn valid_live_form_edit_emits_complete_values() {
        let mut state = AttachPromptState::default();
        let (response_tx, _response_rx) = oneshot::channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        state.enqueue_external(PromptHostRequest {
            request: PromptRequest::form(
                "Padding",
                vec![PromptFormSection::new(
                    "padding",
                    "Padding",
                    vec![
                        PromptFormField::new(
                            "left",
                            "Left",
                            PromptFormFieldKind::Integer {
                                initial_value: 1,
                                min: Some(0),
                                max: Some(20),
                            },
                        ),
                        PromptFormField::new(
                            "right",
                            "Right",
                            PromptFormFieldKind::Integer {
                                initial_value: 2,
                                min: Some(0),
                                max: Some(10),
                            },
                        ),
                    ],
                )],
            )
            .form_live_preview(true),
            response_tx,
            event_tx: Some(event_tx),
        });

        let _ = state.handle_key_event(&key_event(KeyCode::Char('3')));

        let crate::runtime::prompt::PromptEvent::FormChanged {
            field_id, values, ..
        } = event_rx.try_recv().expect("live event")
        else {
            panic!("expected form change");
        };
        assert_eq!(field_id, "left");
        assert_eq!(values.get("left"), Some(&PromptFormValue::Integer(13)));
        assert_eq!(values.get("right"), Some(&PromptFormValue::Integer(2)));
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
    fn multi_toggle_reveals_selection_beyond_viewport() {
        let mut state = AttachPromptState::default();
        state.enqueue_internal(
            PromptRequest::multi_toggle(
                "Features",
                (0..30)
                    .map(|i| PromptOption::new(format!("option-{i}"), format!("Option {i}")))
                    .collect(),
            ),
            AttachInternalPromptAction::QuitSession,
        );
        for _ in 0..29 {
            state.handle_key_event(&key_event(KeyCode::Down));
        }
        for geometry in [
            TerminalGeometry { cols: 80, rows: 16 },
            TerminalGeometry { cols: 40, rows: 10 },
            TerminalGeometry {
                cols: 100,
                rows: 24,
            },
        ] {
            state
                .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
                .expect("render");
            let active = state.active.as_ref().expect("active");
            let area = active
                .hits
                .regions()
                .iter()
                .find(|hit| hit.id.as_str() == "prompt.multi-toggle.29")
                .expect("focused checkbox must be visible after resize")
                .area;
            assert!(area.height > 0 && area.width > 0);
            assert!(area.bottom() < geometry.rows);
            let misses = active.scroll_layout.stats().misses;
            assert_eq!(active.scroll_layout.len(), 1);
            state
                .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
                .expect("repeat render");
            let cache = &state.active.as_ref().expect("active").scroll_layout;
            assert_eq!(cache.stats().misses, misses);
            assert!(cache.stats().hits > 0);
        }
        assert!(
            state
                .active
                .as_ref()
                .expect("active")
                .scroll_layout
                .stats()
                .released
                >= 2
        );
        let PromptWidgetState::MultiToggle { cursor, scroll, .. } =
            &state.active.as_ref().expect("active").state
        else {
            panic!("multi toggle");
        };
        assert_eq!(*cursor, 29);
        assert!(scroll.vertical_offset() > 0);
        state.handle_key_event(&key_event(KeyCode::Char(' ')));
        let PromptKeyDisposition::Completed(completion) =
            state.handle_key_event(&key_event(KeyCode::Enter))
        else {
            panic!("completion");
        };
        assert_eq!(
            completion.response,
            PromptResponse::Submitted(PromptValue::Multi(vec!["option-29".to_string()]))
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
