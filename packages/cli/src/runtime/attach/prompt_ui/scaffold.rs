//! Shared prompt chrome: a fixed header (title, message), a body region that
//! each prompt kind fills, and a fixed footer (key hints, action buttons).
//!
//! Geometry comes from measuring the real components at the chosen width, so
//! the overlay size and the painted content cannot disagree.

use bmux_attach_layout_protocol::AttachRect;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Modifier, Style};
use bmux_tui_components::action_row::{ActionButton, ActionRowComponent, ActionRowState};
use bmux_tui_components::button::ButtonStyles;
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBar, KeyHintBarPolicy};
use bmux_tui_components::modal_frame::{ModalFrame, ModalFrameComponent, ModalSizing};
use std::cell::Cell;

/// Hit-region prefix of footer action buttons (`prompt.actions.<id>`).
pub(super) const ACTIONS_ID: &str = "prompt.actions";

/// Columns between the hint text and the action buttons on a shared row.
const FOOTER_GAP: usize = 2;
/// Widest a message may make the overlay before it wraps.
const MESSAGE_WRAP_WIDTH: usize = 64;
/// Widest the one-row footer may make the overlay before it splits.
const FOOTER_WIDTH_CAP: usize = 60;

/// Styles shared by every scaffold region.
#[derive(Debug, Clone, Copy)]
pub(super) struct PromptTheme {
    pub background: Style,
    pub text: Style,
    pub muted: Style,
    pub title: Style,
    pub accent: Style,
    pub focused: Style,
    pub selected: Style,
    pub error: Style,
    pub modal: bmux_tui_components::modal_frame::ModalTheme,
}

impl PromptTheme {
    pub(super) fn new(theme: &bmux_tui_components::theme::ComponentTheme) -> Self {
        let modal = theme.modal_theme();
        Self {
            background: modal.background,
            text: modal.text,
            muted: modal.muted,
            title: modal.title,
            accent: theme.accent,
            focused: modal.focused,
            selected: theme.selected,
            error: theme.error,
            modal,
        }
    }
}

/// What the scaffold shows around a prompt body.
#[derive(Debug, Clone, Default)]
pub(super) struct Chrome {
    pub title: String,
    pub message: Option<String>,
    pub hints: Vec<(String, String)>,
    pub actions: Vec<ActionButton>,
    /// Button drawn as the default (Enter) action.
    pub default_action: Option<usize>,
    /// Button that holds keyboard focus, if focus is in the footer.
    pub focused_action: Option<usize>,
}

impl Chrome {
    fn actions_width(&self) -> usize {
        let buttons = self
            .actions
            .iter()
            .map(|action| action.label.chars().count().saturating_add(4))
            .sum::<usize>();
        buttons.saturating_add(self.actions.len().saturating_sub(1))
    }

    fn hints_text(&self, count: usize) -> String {
        let hints = self.key_hints(count);
        KeyHintBar::new(&hints)
            .policy(KeyHintBarPolicy::compact())
            .text()
    }

    fn key_hints(&self, count: usize) -> Vec<KeyHint<'_>> {
        self.hints
            .iter()
            .take(count)
            .map(|(key, label)| KeyHint::new(key, label))
            .collect()
    }

    /// Hints that fit in `width`, dropping trailing hints first.
    fn fitting_hints(&self, width: usize) -> usize {
        (0..=self.hints.len())
            .rev()
            .find(|count| self.hints_text(*count).chars().count() <= width)
            .unwrap_or(0)
    }

    fn one_row_width(&self) -> usize {
        let hints = self.hints_text(self.hints.len()).chars().count();
        let actions = self.actions_width();
        match (hints, actions) {
            (0, actions) => actions,
            (hints, 0) => hints,
            (hints, actions) => hints.saturating_add(FOOTER_GAP).saturating_add(actions),
        }
    }

    fn footer_split(&self, width: usize) -> bool {
        !self.hints.is_empty() && !self.actions.is_empty() && self.one_row_width() > width
    }

    fn footer_height(&self, width: usize) -> u16 {
        match (self.hints.is_empty(), self.actions.is_empty()) {
            (true, true) => 0,
            _ if self.footer_split(width) => 2,
            _ => 1,
        }
    }
}

/// How a prompt kind's body wants to be sized.
pub(super) trait BodyMeasure {
    /// Content width the body would like, in columns.
    fn desired_width(&self) -> usize;
    /// Natural body height at `width` columns.
    fn natural_height(&self, width: u16) -> u16;
}

/// Absolute terminal regions of one scaffolded overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScaffoldGeometry {
    pub surface: AttachRect,
    /// Interior title row; only present when an extension owns the border.
    pub title: Option<Rect>,
    pub message: Rect,
    pub body: Rect,
    pub footer: Rect,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct GeometryRequest {
    pub min_width: u16,
    pub max_width: u16,
    pub max_height: Option<u16>,
    pub extension_chrome: bool,
}

fn message_rows(message: Option<&str>, width: u16) -> u16 {
    message.map_or(0, |message| {
        let block = TextBlock::new(message.to_owned());
        let rows = block
            .layout(
                Constraints::for_width(u64::from(width)),
                &mut LayoutCx::new(),
            )
            .size
            .height;
        u16::try_from(rows).unwrap_or(u16::MAX)
    })
}

#[allow(clippy::cast_possible_truncation)] // Geometry is clamped to terminal bounds before narrowing.
pub(super) fn scaffold_geometry(
    chrome: &Chrome,
    body: &dyn BodyMeasure,
    request: GeometryRequest,
    cols: u16,
    rows: u16,
) -> Option<ScaffoldGeometry> {
    if cols < 8 || rows < 4 {
        return None;
    }
    let small = cols < 24 || rows < 8;
    let max_outer_width = if small { cols } else { cols.saturating_sub(2) };
    let max_outer_height = if small { rows } else { rows.saturating_sub(2) };

    let message_width = chrome
        .message
        .as_deref()
        .map_or(0, |message| {
            message
                .lines()
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0)
        })
        .min(MESSAGE_WRAP_WIDTH);
    let desired_inner = chrome
        .title
        .chars()
        .count()
        .saturating_add(4)
        .max(message_width)
        .max(body.desired_width())
        .max(chrome.one_row_width().min(FOOTER_WIDTH_CAP));
    let min_width = request.min_width.max(24);
    let max_width = request.max_width.max(min_width);
    let outer_width = if small {
        max_outer_width
    } else {
        (desired_inner.saturating_add(4) as u16)
            .clamp(min_width, max_width)
            .min(max_outer_width)
    };
    let horizontal_padding = u16::from(outer_width >= 20);
    let inner_width = outer_width.saturating_sub(2 + 2 * horizontal_padding);

    let title_rows = u16::from(request.extension_chrome);
    let message_rows = message_rows(chrome.message.as_deref(), inner_width);
    let natural_body = body.natural_height(inner_width);
    let height_cap = request
        .max_height
        .map_or(max_outer_height, |cap| cap.max(3))
        .min(max_outer_height);
    // When space is tight, give the body room before decoration: first drop
    // the spacer rows, then the footer's hint row, then the footer itself.
    let wanted_body = natural_body.min(3);
    let mut footer_rows = chrome.footer_height(usize::from(inner_width));
    let mut gaps = true;
    let (gap_after_message, gap_before_footer, body_rows) = loop {
        let gap_after_message =
            u16::from(gaps && message_rows > 0 && (natural_body > 0 || footer_rows > 0));
        let gap_before_footer = u16::from(gaps && natural_body > 0 && footer_rows > 0);
        let fixed =
            2 + title_rows + message_rows + gap_after_message + gap_before_footer + footer_rows;
        let body_rows = natural_body.min(height_cap.saturating_sub(fixed));
        if body_rows >= wanted_body || (!gaps && footer_rows == 0) {
            break (gap_after_message, gap_before_footer, body_rows);
        }
        if gaps {
            gaps = false;
        } else {
            footer_rows = footer_rows.saturating_sub(1);
        }
    };
    let fixed = 2 + title_rows + message_rows + gap_after_message + gap_before_footer + footer_rows;
    let outer_height = if small {
        max_outer_height
    } else {
        fixed.saturating_add(body_rows).clamp(3, max_outer_height)
    };

    let x = cols.saturating_sub(outer_width) / 2;
    let y = rows.saturating_sub(outer_height) / 2;
    let inner_x = x.saturating_add(1 + horizontal_padding);
    let mut cursor_y = y.saturating_add(1);
    let title = (title_rows > 0).then(|| Rect::new(inner_x, cursor_y, inner_width, 1));
    cursor_y = cursor_y.saturating_add(title_rows);
    let message = Rect::new(inner_x, cursor_y, inner_width, message_rows);
    cursor_y = cursor_y.saturating_add(message_rows + gap_after_message);
    let bottom = y.saturating_add(outer_height).saturating_sub(1);
    let footer_y = bottom.saturating_sub(footer_rows);
    let body_bottom = footer_y.saturating_sub(gap_before_footer);
    let body = Rect::new(
        inner_x,
        cursor_y,
        inner_width,
        body_bottom.saturating_sub(cursor_y),
    );
    let footer = Rect::new(inner_x, footer_y, inner_width, footer_rows);
    Some(ScaffoldGeometry {
        surface: AttachRect {
            x,
            y,
            w: outer_width,
            h: outer_height,
        },
        title,
        message,
        body,
        footer,
    })
}

/// Paint the frame, header, and footer. Bodies are painted by the caller.
pub(super) fn paint_scaffold(
    chrome: &Chrome,
    geometry: &ScaffoldGeometry,
    extension_chrome: bool,
    theme: PromptTheme,
    cx: &mut PaintCx<'_, '_>,
) {
    let area = Rect::new(
        geometry.surface.x,
        geometry.surface.y,
        geometry.surface.w,
        geometry.surface.h,
    );
    let mut modal = ModalFrame::new(
        ModalSizing::fixed(Size::new(area.width, area.height), Insets::new(0, 0, 0, 0)),
        theme.modal,
    )
    .padding(Insets::new(0, 0, 0, 0));
    if !extension_chrome && !chrome.title.is_empty() {
        modal = modal.title(Line::raw(format!(" {} ", chrome.title)));
    }
    let frame = ModalFrameComponent::new("prompt.overlay", modal, TextBlock::new(""))
        .chrome(!extension_chrome);
    cx.paint_component(&frame, area);

    if let Some(title) = geometry.title {
        cx.write_line_with_fallback_style(
            LocalRect::terminal(title),
            &Line::raw(chrome.title.clone()),
            theme.background.patch(theme.title),
        );
    }
    if let Some(message) = chrome.message.as_deref()
        && geometry.message.height > 0
    {
        let block = TextBlock::new(message.to_owned()).style(theme.background.patch(theme.text));
        cx.paint_component(&block, geometry.message);
    }
    paint_footer(chrome, geometry.footer, theme, cx);
}

fn paint_footer(chrome: &Chrome, footer: Rect, theme: PromptTheme, cx: &mut PaintCx<'_, '_>) {
    if footer.height == 0 || footer.width == 0 {
        return;
    }
    let width = usize::from(footer.width);
    let split = footer.height >= 2 && chrome.footer_split(width);
    let actions_width = u16::try_from(chrome.actions_width()).unwrap_or(u16::MAX);
    let hints_width = if split || chrome.actions.is_empty() {
        width
    } else {
        width.saturating_sub(usize::from(actions_width) + FOOTER_GAP)
    };
    let hints = chrome.key_hints(chrome.fitting_hints(hints_width));
    if !hints.is_empty() {
        let line = KeyHintBar::new(&hints)
            .policy(KeyHintBarPolicy::compact())
            .styles(bmux_tui_components::key_hint_bar::KeyHintBarStyles {
                key: theme.text.add_modifier(Modifier::BOLD),
                label: theme.muted,
                separator: theme.muted,
                disabled: theme.muted,
                background: theme.background,
            })
            .styled_line();
        cx.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(footer.x, footer.y, footer.width, 1)),
            &line,
            theme.background,
        );
    }
    if chrome.actions.is_empty() {
        return;
    }
    let row_y = if split {
        footer.y.saturating_add(1)
    } else {
        footer.y
    };
    let actions_width = actions_width.min(footer.width);
    let actions_area = Rect::new(
        footer.right().saturating_sub(actions_width),
        row_y,
        actions_width,
        1,
    );
    let in_footer = chrome.focused_action.is_some();
    let state = Cell::new({
        let mut state = ActionRowState::new();
        state.set_focused(chrome.focused_action.or(chrome.default_action));
        state
    });
    // Footer focus must not rely on color alone: bold + underline.
    let focused = if in_footer {
        theme
            .selected
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINE)
    } else {
        theme.accent.add_modifier(Modifier::BOLD)
    };
    let row = ActionRowComponent::new(ACTIONS_ID, &chrome.actions, &state)
        .spacing(1)
        .styles(ButtonStyles {
            normal: theme.background.patch(theme.muted),
            focused: theme.background.patch(focused),
            hovered: theme.background.patch(theme.text),
            pressed: theme.background.patch(focused),
            disabled: theme.background.patch(theme.muted),
        });
    cx.paint_component(&row, actions_area);
}

/// Line with `label` followed by a muted rule filling `width`.
pub(super) fn section_rule(label: &str, width: u16, theme: PromptTheme) -> Line {
    let used = label.chars().count().saturating_add(1);
    let rule = "─".repeat(usize::from(width).saturating_sub(used));
    Line::from_spans([
        Span::styled(label.to_owned(), theme.title),
        Span::styled(format!(" {rule}"), theme.muted),
    ])
}
