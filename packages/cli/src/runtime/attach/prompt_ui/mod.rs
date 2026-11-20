mod form;
mod scaffold;

use std::cell::RefCell;

use super::input::{TerminalGeometry, TerminalKeyEvent, TerminalMouseEvent};
use super::prompt_search::filtered_option_indices;
use super::state::AttachCursorState;
use super::tui_surface::{buffer_render_ops, component_theme, surface_buffer};
use crate::runtime::prompt::{
    PromptField, PromptFormField, PromptFormLayout, PromptFormValue, PromptHostRequest,
    PromptPolicy, PromptRequest, PromptResponse, PromptValue,
};
use anyhow::Result;
use bmux_appearance::RuntimeAppearance;
use bmux_attach_layout_protocol::{
    AttachLayer as SurfaceLayer, AttachRect, AttachSurface, AttachSurfaceKind,
};
use bmux_config::PromptFormLayoutSetting;
use bmux_plugin::RenderOp;
use bmux_terminal::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use bmux_text_edit::{TextEditBuffer, TextMotion};
use bmux_tui::component::{Component, Constraints, LayoutCache, LayoutCx, LogicalSize};
use bmux_tui::composition::{Column, SizeBox, TextBlock};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::hit::HitMap;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Text};
use bmux_tui_components::action_row::ActionButton;
use bmux_tui_components::checkbox::{CheckboxComponent, CheckboxState, CheckboxStyles};
use bmux_tui_components::modal_frame::{ModalFrame, ModalFrameComponent, ModalSizing};
use bmux_tui_components::scroll_view::{ScrollViewComponent, ScrollViewState};
use bmux_tui_components::scrollbar::{ScrollbarComponent, ScrollbarPolicy, ScrollbarStyles};
use bmux_tui_components::text_input::{TextInputComponent, TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{TextInputBoxComponent, TextInputBoxPolicy};
use bmux_tui_components::virtual_list::{VirtualList, VirtualListState};
use scaffold::{BodyMeasure, Chrome, GeometryRequest, PromptTheme, ScaffoldGeometry};
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
    /// A pane's program set the clipboard (OSC 52) under the `ask` policy.
    ClipboardWrite {
        pane_id: Uuid,
        text: String,
    },
}

#[derive(Debug)]
pub enum AttachPromptOrigin {
    Scoped(ScopedPrompt),
    External {
        response_tx: oneshot::Sender<PromptResponse>,
        event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::runtime::prompt::PromptEvent>>,
    },
    Internal(AttachInternalPromptAction),
}

/// Retains the owner's admission until response/cancellation. Event backpressure
/// disconnects the producer rather than silently discarding preview updates.
pub struct ScopedPrompt(std::sync::Mutex<Option<Box<bmux_plugin::prompt::AdmittedPrompt>>>);
impl std::fmt::Debug for ScopedPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScopedPrompt")
    }
}
impl ScopedPrompt {
    pub(super) fn respond(self, response: PromptResponse) {
        match self.0.into_inner() {
            Ok(Some(prompt)) => {
                let _ = (*prompt).respond(response);
            }
            Ok(None) => {}
            Err(_) => tracing::error!("scoped prompt state poisoned; response disconnected"),
        }
    }
    fn emit(&self, event: crate::runtime::prompt::PromptEvent) {
        let Ok(mut state) = self.0.lock() else {
            tracing::error!("scoped prompt state poisoned; event rejected");
            return;
        };
        if state
            .as_ref()
            .is_some_and(|prompt| prompt.emit(event).is_err())
        {
            state.take();
        }
    }
    fn cancelled(&self) -> bool {
        !self
            .0
            .lock()
            .is_ok_and(|state| state.as_ref().is_some_and(|prompt| !prompt.is_cancelled()))
    }
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
        // Boxed so this variant does not size the whole enum.
        query: Box<TextEditBuffer>,
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
    // Boxed so this variant does not size the whole enum.
    Form(Box<form::FormState>),
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
                    query: Box::new(TextEditBuffer::new()),
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
            PromptField::Form { sections, .. } => {
                PromptWidgetState::Form(Box::new(form::FormState::new(sections)))
            }
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
    /// Whether an extension drew the border on the last render; the title
    /// then needs an interior row.
    extension_chrome: bool,
    form_layout: PromptFormLayoutSetting,
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

    /// Apply the user's preferred form field layout.
    pub const fn set_form_layout(&mut self, layout: PromptFormLayoutSetting) {
        self.form_layout = layout;
    }

    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }

    pub(super) fn enqueue_scoped(&mut self, prompt: Box<bmux_plugin::prompt::AdmittedPrompt>) {
        self.enqueue(AttachPromptEnvelope {
            request: prompt.request().clone(),
            origin: AttachPromptOrigin::Scoped(ScopedPrompt(std::sync::Mutex::new(Some(prompt)))),
        });
    }

    pub(super) fn discard_cancelled_scoped(&mut self) -> bool {
        let before = self.queue.len();
        self.queue.retain(|envelope| !matches!(&envelope.origin, AttachPromptOrigin::Scoped(prompt) if prompt.cancelled()));
        let active_cancelled = self.active.as_ref().is_some_and(|active|
            matches!(&active.envelope.origin, AttachPromptOrigin::Scoped(prompt) if prompt.cancelled()));
        if active_cancelled {
            self.active = None;
        }
        active_cancelled || self.queue.len() != before
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
            (PromptField::Form { .. }, PromptWidgetState::Form(form_state)) => {
                form::handle_paste(form_state, &active.envelope, text);
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
        // While hidden for a peek, only Esc may act on the prompt.
        if !self.overlay_visible && !matches!(key.code, KeyCode::Esc) {
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
                        wrap_selection,
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
                            let len =
                                filtered_option_indices(options, query.text(), *match_mode).len();
                            *selected =
                                step_search_selection(*selected, len, false, *wrap_selection);
                        }
                        KeyCode::Down | KeyCode::Char('n')
                            if matches!(key.code, KeyCode::Down)
                                || key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            let len =
                                filtered_option_indices(options, query.text(), *match_mode).len();
                            *selected =
                                step_search_selection(*selected, len, true, *wrap_selection);
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
                (PromptField::Form { .. }, PromptWidgetState::Form(form_state)) => {
                    match form::handle_key(form_state, &active.envelope, key) {
                        form::FormOutcome::Consumed => {}
                        form::FormOutcome::Submit(values) => {
                            completion = Some(PromptResponse::Submitted(PromptValue::Form(values)));
                        }
                        form::FormOutcome::Cancel => completion = Some(PromptResponse::Cancelled),
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
        key.to_terminal()
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
            .to_terminal()
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
        let Some(layout) = self.overlay_layout(geometry) else {
            return PromptKeyDisposition::NotActive;
        };
        let surface = layout.surface.rect;
        let point = Point::new(mouse.column, mouse.row);
        let inside = Rect::new(surface.x, surface.y, surface.w, surface.h).contains(point);
        let Some(active) = self.active.as_mut() else {
            return PromptKeyDisposition::NotActive;
        };

        if let PromptWidgetState::Form(form_state) = &mut active.state {
            if !inside {
                return PromptKeyDisposition::Consumed;
            }
            let body = layout
                .scaffold
                .map_or_else(Rect::default, |scaffold| scaffold.body);
            let outcome =
                form::handle_mouse(form_state, &active.envelope, &active.hits, body, mouse);
            return match outcome {
                form::FormOutcome::Consumed => PromptKeyDisposition::Consumed,
                form::FormOutcome::Submit(values) => {
                    self.complete_active(PromptResponse::Submitted(PromptValue::Form(values)))
                }
                form::FormOutcome::Cancel => self.complete_active(PromptResponse::Cancelled),
            };
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some(action) = footer_action_at(&active.hits, point)
        {
            return match action.as_str() {
                "cancel" => self.complete_active(PromptResponse::Cancelled),
                "yes" => {
                    self.complete_active(PromptResponse::Submitted(PromptValue::Confirm(true)))
                }
                "no" => {
                    self.complete_active(PromptResponse::Submitted(PromptValue::Confirm(false)))
                }
                _ => self.handle_key_event(&KeyEvent {
                    code: KeyCode::Enter,
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    state: bmux_terminal::event::KeyEventState::NONE,
                }),
            };
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
        self.overlay_layout(geometry).map(|layout| layout.surface)
    }

    fn resolved_form_layout(&self, request: &PromptRequest) -> PromptFormLayout {
        let hint = match &request.field {
            PromptField::Form { layout, .. } => *layout,
            _ => None,
        };
        match self.form_layout {
            PromptFormLayoutSetting::Auto => hint.unwrap_or_default(),
            PromptFormLayoutSetting::Compact => PromptFormLayout::Compact,
            PromptFormLayoutSetting::Stacked => PromptFormLayout::Stacked,
        }
    }

    fn overlay_layout(&self, geometry: TerminalGeometry) -> Option<OverlayLayout> {
        let active = self.active.as_ref()?;
        let request = &active.envelope.request;
        if matches!(request.field, PromptField::SearchSelect { .. }) {
            return prompt_overlay_layout(Some(request), geometry).map(|layout| OverlayLayout {
                surface: layout.surface,
                scaffold: None,
            });
        }
        let chrome = chrome_for(active);
        let body = ActiveBody {
            active,
            layout: self.resolved_form_layout(request),
            theme: PromptTheme::new(&component_theme(&RuntimeAppearance::default())),
        };
        let scaffold = scaffold::scaffold_geometry(
            &chrome,
            &body,
            GeometryRequest {
                min_width: request.width.min,
                max_width: request.width.max,
                max_height: request.max_height,
                extension_chrome: self.extension_chrome,
            },
            geometry.cols,
            geometry.rows,
        )?;
        Some(OverlayLayout {
            surface: overlay_surface_at(scaffold.surface),
            scaffold: Some(scaffold),
        })
    }

    pub fn attach_prompt_overlay_render(
        &mut self,
        geometry: TerminalGeometry,
        appearance: &RuntimeAppearance,
        extension_chrome: bool,
    ) -> Option<AttachPromptOverlayRender> {
        if !self.overlay_visible {
            return None;
        }
        self.extension_chrome = extension_chrome;
        let layout = self.overlay_layout(geometry)?;
        let form_layout = self.resolved_form_layout(&self.active.as_ref()?.envelope.request);
        let active = self.active.as_mut()?;
        let Some(scaffold) = layout.scaffold else {
            return Some(render_search_select(
                active,
                layout.surface,
                appearance,
                extension_chrome,
            ));
        };
        let theme = PromptTheme::new(&component_theme(appearance));
        let chrome = chrome_for(active);
        let area = Rect::new(
            layout.surface.rect.x,
            layout.surface.rect.y,
            layout.surface.rect.w,
            layout.surface.rect.h,
        );
        let mut buffer = surface_buffer(area);
        let mut frame = Frame::new(&mut buffer);
        let cursor_state = {
            let mut cx = PaintCx::new(&mut frame);
            scaffold::paint_scaffold(&chrome, &scaffold, extension_chrome, theme, &mut cx);
            let wants_cursor = paint_body(active, &scaffold, theme, form_layout, &mut cx);
            wants_cursor
                .then(|| cx.cursor())
                .flatten()
                .map(|cursor| AttachCursorState {
                    x: cursor.position.x,
                    y: cursor.position.y,
                    visible: cursor.visible,
                })
        };
        active.hits = frame.hits().clone();
        Some(AttachPromptOverlayRender {
            surface: layout.surface,
            ops: buffer_render_ops(&buffer),
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

struct OverlayLayout {
    surface: AttachSurface,
    /// Scaffold regions; `None` for search prompts, which lay themselves out.
    scaffold: Option<ScaffoldGeometry>,
}

const fn overlay_surface_at(rect: AttachRect) -> AttachSurface {
    AttachSurface {
        id: PROMPT_OVERLAY_SURFACE_ID,
        kind: AttachSurfaceKind::Modal,
        layer: SurfaceLayer::Overlay,
        z: i32::MAX,
        rect,
        content_rect: rect,
        interactive_regions: Vec::new(),
        opaque: true,
        visible: true,
        accepts_input: true,
        cursor_owner: true,
        pane_id: None,
    }
}

fn footer_action_at(hits: &HitMap, point: Point) -> Option<String> {
    hits.regions().iter().rev().find_map(|hit| {
        hit.area
            .contains(point)
            .then(|| {
                hit.id
                    .as_str()
                    .strip_prefix(scaffold::ACTIONS_ID)?
                    .strip_prefix('.')
                    .map(str::to_owned)
            })
            .flatten()
    })
}

fn hint(key: &str, label: &str) -> (String, String) {
    (key.to_owned(), label.to_lowercase())
}

/// Header and footer content for the active prompt.
fn chrome_for(active: &ActivePrompt) -> Chrome {
    let request = &active.envelope.request;
    let submit = request.submit_label.as_str();
    let cancel = request.cancel_label.as_str();
    let mut chrome = Chrome {
        title: request.title.clone(),
        message: request
            .message
            .clone()
            .filter(|message| !message.trim().is_empty()),
        ..Chrome::default()
    };
    let esc = request.esc_cancels.then(|| hint("Esc", cancel));
    let submit_actions = || {
        vec![
            ActionButton::new("submit", submit),
            ActionButton::new("cancel", cancel),
        ]
    };
    match (&request.field, &active.state) {
        (
            PromptField::Confirm {
                yes_label,
                no_label,
                ..
            },
            PromptWidgetState::Confirm { selected_yes },
        ) => {
            chrome.hints = [hint("←→", "choose"), hint("Enter", "confirm")]
                .into_iter()
                .chain(esc)
                .collect();
            chrome.actions = vec![
                ActionButton::new("yes", yes_label.clone()),
                ActionButton::new("no", no_label.clone()),
            ];
            let selected = usize::from(!*selected_yes);
            chrome.default_action = Some(selected);
            chrome.focused_action = Some(selected);
        }
        (PromptField::TextInput { .. }, _) => {
            chrome.hints = std::iter::once(hint("Enter", submit)).chain(esc).collect();
            chrome.actions = submit_actions();
            chrome.default_action = Some(form::SUBMIT_ACTION);
        }
        (PromptField::SingleSelect { .. }, _) => {
            chrome.hints = [hint("↑↓", "choose"), hint("Enter", submit)]
                .into_iter()
                .chain(esc)
                .collect();
        }
        (PromptField::MultiToggle { .. }, _) => {
            chrome.hints = [
                hint("↑↓", "move"),
                hint("Space", "toggle"),
                hint("Enter", submit),
            ]
            .into_iter()
            .chain(esc)
            .collect();
            chrome.actions = submit_actions();
            chrome.default_action = Some(form::SUBMIT_ACTION);
        }
        (
            PromptField::Form {
                sections,
                live_preview,
                resettable,
                ..
            },
            PromptWidgetState::Form(form_state),
        ) => {
            chrome.hints = form::hints(
                form_state,
                sections,
                submit,
                cancel,
                request.esc_cancels,
                *resettable,
                *live_preview,
            );
            chrome.actions = submit_actions();
            chrome.default_action = Some(form::SUBMIT_ACTION);
            chrome.focused_action = match form_state.focus {
                form::FormFocus::Action(action) => Some(action),
                form::FormFocus::Field(_) => None,
            };
        }
        _ => {}
    }
    chrome
}

/// Sizing of the active prompt's body for the scaffold.
struct ActiveBody<'a> {
    active: &'a ActivePrompt,
    layout: PromptFormLayout,
    theme: PromptTheme,
}

fn options_width(options: &[crate::runtime::prompt::PromptOption]) -> usize {
    options
        .iter()
        .map(|option| {
            option.label.chars().count()
                + option
                    .detail
                    .as_ref()
                    .map_or(0, |detail| detail.chars().count() + 5)
                + 6
        })
        .max()
        .unwrap_or(0)
}

impl BodyMeasure for ActiveBody<'_> {
    fn desired_width(&self) -> usize {
        match &self.active.envelope.request.field {
            PromptField::Confirm { .. } => 0,
            PromptField::TextInput {
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
                .max(32)
                .saturating_add(4),
            PromptField::SingleSelect { options, .. }
            | PromptField::SearchSelect { options, .. }
            | PromptField::MultiToggle { options, .. } => options_width(options),
            PromptField::Form { sections, .. } => form::desired_width(sections, self.layout),
        }
    }

    fn natural_height(&self, width: u16) -> u16 {
        let request = &self.active.envelope.request;
        match (&request.field, &self.active.state) {
            (
                PromptField::TextInput { placeholder, .. },
                PromptWidgetState::TextInput { buffer, error },
            ) => {
                let state = RefCell::new(TextInputState::new(buffer.clone()));
                let tree =
                    text_input_body(&state, placeholder.as_deref(), error.as_deref(), self.theme);
                let height = tree
                    .layout(
                        Constraints::for_width(u64::from(width)),
                        &mut LayoutCx::new(),
                    )
                    .size
                    .height;
                u16::try_from(height).unwrap_or(u16::MAX)
            }
            (
                PromptField::SingleSelect { options, .. }
                | PromptField::MultiToggle { options, .. },
                _,
            ) => u16::try_from(options.len().max(1)).unwrap_or(u16::MAX),
            (PromptField::Form { sections, .. }, PromptWidgetState::Form(form_state)) => {
                form::natural_height(sections, form_state, self.theme, self.layout, width)
            }
            _ => 0,
        }
    }
}

/// Bordered single text input with its validation error beneath it.
fn text_input_body<'a>(
    state: &'a RefCell<TextInputState>,
    placeholder: Option<&'a str>,
    error: Option<&str>,
    theme: PromptTheme,
) -> Column<'a> {
    let mut input =
        TextInputBoxComponent::new("prompt.text-input", TextInputPolicy::chat_composer(), state)
            .policy(TextInputBoxPolicy::field().focused(true).rows(1, Some(3)))
            .styles(form::input_box_styles(theme));
    if let Some(placeholder) = placeholder {
        input = input.placeholder(placeholder);
    }
    let mut column = Column::new().child(input);
    if let Some(error) = error {
        column = column
            .child(TextBlock::new(format!("✕ {error}")).style(theme.background.patch(theme.error)));
    }
    column
}

/// Paint the body of a scaffolded prompt. Returns whether the prompt owns the
/// terminal cursor (text entry).
fn paint_body(
    active: &mut ActivePrompt,
    scaffold: &ScaffoldGeometry,
    theme: PromptTheme,
    form_layout: PromptFormLayout,
    cx: &mut PaintCx<'_, '_>,
) -> bool {
    let body = scaffold.body;
    match active.envelope.request.field {
        PromptField::SingleSelect { .. } => {
            render_single_select(active, body, cx, theme.modal);
            return false;
        }
        PromptField::MultiToggle { .. } => {
            render_multi_toggle(active, body, cx, theme.modal);
            return false;
        }
        _ => {}
    }
    let ActivePrompt {
        envelope, state, ..
    } = active;
    match (&envelope.request.field, state) {
        (
            PromptField::TextInput { placeholder, .. },
            PromptWidgetState::TextInput { buffer, error },
        ) => {
            let input_state = RefCell::new(TextInputState::new(buffer.clone()));
            let tree = text_input_body(
                &input_state,
                placeholder.as_deref(),
                error.as_deref(),
                theme,
            );
            cx.paint_component(&tree, body);
            true
        }
        (PromptField::Form { sections, .. }, PromptWidgetState::Form(form_state)) => {
            form::paint(form_state, sections, theme, form_layout, body, cx);
            true
        }
        _ => false,
    }
}

/// Search prompts (command palette and generic search) own their layout:
/// query row, measured result list, scrollbar, and match-count footer.
fn render_search_select(
    active: &mut ActivePrompt,
    surface: AttachSurface,
    appearance: &RuntimeAppearance,
    extension_chrome: bool,
) -> AttachPromptOverlayRender {
    let area = Rect::new(
        surface.rect.x,
        surface.rect.y,
        surface.rect.w,
        surface.rect.h,
    );
    let theme = component_theme(appearance).modal_theme();
    let compact = surface.rect.w < 24 || surface.rect.h < 8;
    let mut modal = ModalFrame::new(
        ModalSizing::fixed(
            Size::new(surface.rect.w, surface.rect.h),
            Insets::new(0, 0, 0, 0),
        ),
        theme,
    )
    .padding(if compact {
        Insets::new(0, 0, 0, 0)
    } else {
        Insets::new(0, 1, 0, 1)
    });
    if !extension_chrome {
        modal = modal.title(Line::raw(format!(" {} ", active.envelope.request.title)));
    }
    let mut buffer = surface_buffer(area);
    let mut frame = Frame::new(&mut buffer);
    let cursor_state = {
        let mut cx = PaintCx::new(&mut frame);
        let modal_component =
            ModalFrameComponent::new("prompt.overlay", modal.clone(), TextBlock::new(""))
                .chrome(!extension_chrome);
        cx.paint_component(&modal_component, area);
        let mut content = modal.content_area(area);
        if extension_chrome && content.height > 0 {
            // Extensions paint border cells after prompt content. Keep the
            // title inside their border and reserve its row.
            cx.write_line_with_fallback_style(
                LocalRect::terminal(Rect::new(content.x, content.y, content.width, 1)),
                &Line::raw(active.envelope.request.title.clone()),
                theme.title,
            );
            content.y = content.y.saturating_add(1);
            content.height = content.height.saturating_sub(1);
        }
        render_command_palette(active, content, &mut cx, theme);
        cx.cursor().map(|cursor| AttachCursorState {
            x: cursor.position.x,
            y: cursor.position.y,
            visible: cursor.visible,
        })
    };
    active.hits = frame.hits().clone();
    AttachPromptOverlayRender {
        surface,
        ops: buffer_render_ops(&buffer),
        cursor_state,
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
    let content = Rect::new(content.x, content.y, content.width, content.height);
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
    let input_state = RefCell::new(TextInputState::new((**query).clone()));
    let input_policy = TextInputPolicy::raw();
    let input = TextInputComponent::new("command-palette.query", &input_state, &input_policy)
        .placeholder(&placeholder, theme.muted)
        .focused(true);
    cx.paint_component(&input, input_area);

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
        cx.paint_component(&scrollbar, scrollbar_area);
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
    match origin {
        AttachPromptOrigin::External { response_tx, .. } => {
            let _ = response_tx.send(response);
        }
        AttachPromptOrigin::Scoped(prompt) => prompt.respond(response),
        AttachPromptOrigin::Internal(_) => {}
    }
}

fn emit_origin_event(origin: &AttachPromptOrigin, event: crate::runtime::prompt::PromptEvent) {
    match origin {
        AttachPromptOrigin::External {
            event_tx: Some(events),
            ..
        } => {
            let _ = events.send(event);
        }
        AttachPromptOrigin::Scoped(prompt) => prompt.emit(event),
        AttachPromptOrigin::External { event_tx: None, .. } | AttachPromptOrigin::Internal(_) => {}
    }
}

fn emit_selection_changed(envelope: &AttachPromptEnvelope, selected: usize) {
    if let PromptField::SingleSelect { options, .. } | PromptField::SearchSelect { options, .. } =
        &envelope.request.field
        && let Some(option) = options.get(selected)
    {
        emit_origin_event(
            &envelope.origin,
            crate::runtime::prompt::PromptEvent::SelectionChanged {
                index: selected,
                value: option.value.clone(),
            },
        );
    }
}

fn emit_form_changed(
    envelope: &AttachPromptEnvelope,
    field: &PromptFormField,
    values: &BTreeMap<String, PromptFormValue>,
) {
    if let Some(value) = values.get(&field.id) {
        emit_origin_event(
            &envelope.origin,
            crate::runtime::prompt::PromptEvent::FormChanged {
                field_id: field.id.clone(),
                value: value.clone(),
                values: values.clone(),
            },
        );
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
    // Reserve an interior title row even when fallback chrome can place it on
    // the border: extension-owned borders must not occlude the prompt title.
    let mut lines = 1usize;
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
        // Query input and its separating blank row precede the result list.
        PromptField::SearchSelect { options, .. } => options.len().saturating_add(2).max(3),
        PromptField::Form { sections, .. } => sections
            .iter()
            .map(|section| 1usize.saturating_add(section.fields.len()))
            .sum::<usize>()
            .max(1),
    });
    lines.max(1)
}

fn step_search_selection(selected: usize, len: usize, forward: bool, wrap: bool) -> usize {
    if len == 0 {
        return 0;
    }
    let last = len - 1;
    let selected = selected.min(last);
    if forward {
        if wrap && selected == last {
            0
        } else {
            selected.saturating_add(1).min(last)
        }
    } else if wrap && selected == 0 {
        last
    } else {
        selected.saturating_sub(1)
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
mod tests;
