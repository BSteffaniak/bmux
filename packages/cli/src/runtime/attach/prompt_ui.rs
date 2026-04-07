use super::render::{AttachLayer, AttachLayerSurface, opaque_row_text, queue_layer_fill};
use super::state::{AttachCursorState, PaneRect};
use crate::runtime::prompt::{
    PromptField, PromptHostRequest, PromptPolicy, PromptRequest, PromptResponse, PromptValue,
};
use anyhow::{Context, Result};
use bmux_ipc::{AttachLayer as SurfaceLayer, AttachRect, AttachSurface, AttachSurfaceKind};
use crossterm::cursor::MoveTo;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal;
use std::collections::{BTreeSet, VecDeque};
use std::io::Write;
use tokio::sync::oneshot;
use uuid::Uuid;

const PROMPT_OVERLAY_SURFACE_ID: Uuid = Uuid::from_u128(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachInternalPromptAction {
    QuitSession,
    ClosePane { pane_id: Uuid },
}

#[derive(Debug)]
pub enum AttachPromptOrigin {
    External {
        response_tx: oneshot::Sender<PromptResponse>,
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
    },
    SingleSelect {
        selected: usize,
        scroll: usize,
    },
    MultiToggle {
        cursor: usize,
        selected: BTreeSet<usize>,
        scroll: usize,
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
                }
            }
            PromptField::SingleSelect {
                options,
                default_index,
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
            PromptField::MultiToggle { .. } => {
                "Prompt | Up/Down move | Space toggle | Enter submit | Esc cancel"
            }
        };
        Some(hint)
    }

    pub fn enqueue_external(&mut self, host_request: PromptHostRequest) {
        self.enqueue(AttachPromptEnvelope {
            request: host_request.request,
            origin: AttachPromptOrigin::External {
                response_tx: host_request.response_tx,
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
                    },
                    PromptWidgetState::TextInput { value, cursor },
                ) => match key.code {
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        insert_char(value, *cursor, ch);
                        *cursor = cursor.saturating_add(1);
                    }
                    KeyCode::Backspace => {
                        if *cursor > 0 {
                            *cursor = cursor.saturating_sub(1);
                            remove_char(value, *cursor);
                        }
                    }
                    KeyCode::Delete => {
                        remove_char(value, *cursor);
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
                            return PromptKeyDisposition::Consumed;
                        }
                        completion =
                            Some(PromptResponse::Submitted(PromptValue::Text(value.clone())));
                    }
                    _ => {}
                },
                (
                    PromptField::SingleSelect { options, .. },
                    PromptWidgetState::SingleSelect { selected, scroll },
                ) => {
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
                _ => {}
            }
        }

        if let Some(response) = completion {
            return self.complete_active(response);
        }

        PromptKeyDisposition::Consumed
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

        let surface = AttachLayerSurface::new(
            PaneRect {
                x: layout.surface.rect.x,
                y: layout.surface.rect.y,
                w: layout.surface.rect.w,
                h: layout.surface.rect.h,
            },
            AttachLayer::Overlay,
            true,
        );

        let top = format!("+{}+", "-".repeat(width.saturating_sub(2)));
        queue!(stdout, MoveTo(x as u16, y as u16), Print(&top))
            .context("failed drawing prompt overlay top")?;

        let title = format!(
            " {} ",
            truncate_chars(&active.envelope.request.title, text_width)
        );
        let title_x = x + ((width.saturating_sub(title.len())) / 2);
        queue!(stdout, MoveTo(title_x as u16, y as u16), Print(title))
            .context("failed drawing prompt overlay title")?;

        for row in 1..height.saturating_sub(1) {
            let y_row = (y + row) as u16;
            queue!(
                stdout,
                MoveTo(x as u16, y_row),
                Print("|"),
                MoveTo((x + width - 1) as u16, y_row),
                Print("|")
            )
            .context("failed drawing prompt overlay border")?;
        }

        queue_layer_fill(stdout, surface).context("failed filling prompt overlay body")?;

        queue!(
            stdout,
            MoveTo(x as u16, (y + height - 1) as u16),
            Print(&top)
        )
        .context("failed drawing prompt overlay bottom")?;

        let body = render_prompt_body(active, text_width, body_rows);
        for (index, line) in body.lines.iter().take(body_rows).enumerate() {
            let row = y + 1 + index;
            queue!(stdout, MoveTo((x + 2) as u16, row as u16), Print(line))
                .context("failed drawing prompt overlay body")?;
        }

        let footer = prompt_footer_text(&active.envelope.request);
        let footer_rendered = opaque_row_text(&footer, text_width);
        queue!(
            stdout,
            MoveTo((x + 2) as u16, (y + height - 2) as u16),
            Print(footer_rendered)
        )
        .context("failed drawing prompt overlay footer")?;

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
    if let AttachPromptOrigin::External { response_tx } = origin {
        let _ = response_tx.send(response);
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
        PromptField::SingleSelect { options, .. } | PromptField::MultiToggle { options, .. } => {
            for option in options {
                width = width.max(option.label.chars().count().saturating_add(6));
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
        PromptField::Confirm { .. } | PromptField::TextInput { .. } => 1,
        PromptField::SingleSelect { options, .. } | PromptField::MultiToggle { options, .. } => {
            options.len().max(1)
        }
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
            PromptWidgetState::TextInput { value, cursor: pos },
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
        PromptField::MultiToggle { .. } => format!(
            "Up/Down move | Space toggle | Enter {} | Esc {}",
            request.submit_label, request.cancel_label
        ),
    }
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

#[cfg(test)]
mod tests {
    use super::{
        AttachInternalPromptAction, AttachPromptState, PromptKeyDisposition, adjust_scroll,
        render_prompt_body,
    };
    use crate::runtime::prompt::{PromptOption, PromptRequest, PromptResponse, PromptValue};
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
