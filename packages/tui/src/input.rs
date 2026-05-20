//! Text input widget and key handling.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_text_edit::{TextEditBuffer, TextSelection, VisualCursor};
use unicode_segmentation::UnicodeSegmentation;

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::style::Style;
use crate::text::Line;
use crate::widget::Widget;
use crate::widgets::line_with_fallback_style;

/// Enter-key behavior for text input key handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextInputEnterBehavior {
    /// Enter inserts a newline into the edit buffer.
    #[default]
    InsertNewline,
    /// Enter reports submission and leaves the edit buffer unchanged.
    Submit,
}

/// Result of handling a key stroke for a text input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputKeyOutcome {
    /// The key was not recognized as text input.
    Ignored,
    /// The edit buffer changed or cursor moved.
    Edited,
    /// The key requested submission.
    Submitted,
}

/// Key handling policy for [`TextInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextInputKeyHandler {
    /// Standard editor keymap.
    pub keymap: TextKeymap,
    /// Enter-key behavior.
    pub enter_behavior: TextInputEnterBehavior,
}

impl TextInputKeyHandler {
    /// Create a key handler from a keymap and enter behavior.
    #[must_use]
    pub const fn new(keymap: TextKeymap, enter_behavior: TextInputEnterBehavior) -> Self {
        Self {
            keymap,
            enter_behavior,
        }
    }

    /// Apply a key stroke to an edit buffer.
    pub fn handle_key(self, buffer: &mut TextEditBuffer, stroke: KeyStroke) -> TextInputKeyOutcome {
        if stroke.key == KeyCode::Enter && stroke.modifiers.is_empty() {
            return match self.enter_behavior {
                TextInputEnterBehavior::InsertNewline => {
                    buffer.insert_newline();
                    TextInputKeyOutcome::Edited
                }
                TextInputEnterBehavior::Submit => TextInputKeyOutcome::Submitted,
            };
        }

        let Some(command) = self.keymap.command_for_key(stroke) else {
            return TextInputKeyOutcome::Ignored;
        };
        buffer.apply_command(command);
        TextInputKeyOutcome::Edited
    }
}

/// A rendered text input projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInputProjection {
    /// Soft-wrapped visible lines.
    pub lines: Vec<String>,
    /// Cursor location in widget-local coordinates.
    pub cursor: VisualCursor,
}

/// A multiline text input widget backed by [`TextEditBuffer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput<'buffer> {
    buffer: &'buffer TextEditBuffer,
    style: Style,
    selection_style: Style,
    placeholder: Option<Line>,
    placeholder_style: Style,
    cursor_visible: bool,
    vertical_scroll: usize,
}

impl<'buffer> TextInput<'buffer> {
    /// Create a text input from an edit buffer.
    #[must_use]
    pub const fn new(buffer: &'buffer TextEditBuffer) -> Self {
        Self {
            buffer,
            style: Style::new(),
            selection_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
            placeholder: None,
            placeholder_style: Style::new(),
            cursor_visible: true,
            vertical_scroll: 0,
        }
    }

    /// Set rendered text style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set selected text style. This style patches over the base text style.
    #[must_use]
    pub const fn selection_style(mut self, style: Style) -> Self {
        self.selection_style = style;
        self
    }

    /// Set placeholder text.
    #[must_use]
    pub fn placeholder(mut self, placeholder: impl Into<Line>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }

    /// Set placeholder style.
    #[must_use]
    pub const fn placeholder_style(mut self, style: Style) -> Self {
        self.placeholder_style = style;
        self
    }

    /// Set cursor visibility.
    #[must_use]
    pub const fn cursor_visible(mut self, visible: bool) -> Self {
        self.cursor_visible = visible;
        self
    }

    /// Set vertical scroll offset in wrapped rows.
    #[must_use]
    pub const fn vertical_scroll(mut self, rows: usize) -> Self {
        self.vertical_scroll = rows;
        self
    }

    /// Project the buffer into visible wrapped lines for an area.
    #[must_use]
    pub fn project(&self, area: Rect) -> TextInputProjection {
        let width = usize::from(area.width.max(1));
        let layout = self.buffer.wrapped_layout(width);
        let lines = layout
            .lines
            .iter()
            .skip(self.vertical_scroll)
            .take(usize::from(area.height))
            .cloned()
            .collect();
        TextInputProjection {
            lines,
            cursor: VisualCursor {
                row: layout.cursor.row.saturating_sub(self.vertical_scroll),
                col: layout.cursor.col,
            },
        }
    }
}

impl Widget for TextInput<'_> {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }

        if self.buffer.is_empty() {
            if let Some(placeholder) = &self.placeholder {
                let styled = line_with_fallback_style(placeholder, self.placeholder_style);
                frame.write_line(Rect::new(area.x, area.y, area.width, 1), &styled);
            }
            if self.cursor_visible {
                frame.set_cursor(crate::frame::Cursor::visible(crate::geometry::Point::new(
                    area.x, area.y,
                )));
            }
            return;
        }

        let projection = self.project(area);
        let rendered_lines = selected_wrapped_lines(
            self.buffer.text(),
            &self.buffer.wrapped_layout(usize::from(area.width.max(1))),
            self.buffer.selection(),
            self.style,
            self.selection_style,
        );
        for (row, line) in rendered_lines
            .into_iter()
            .skip(self.vertical_scroll)
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &line,
            );
        }

        if self.cursor_visible && projection.cursor.row < usize::from(area.height) {
            let cursor_col = u16::try_from(projection.cursor.col)
                .unwrap_or(u16::MAX)
                .min(area.width);
            let cursor_row = u16::try_from(projection.cursor.row).unwrap_or(u16::MAX);
            frame.set_cursor(crate::frame::Cursor::visible(crate::geometry::Point::new(
                area.x.saturating_add(cursor_col),
                area.y.saturating_add(cursor_row),
            )));
        }
    }
}

fn selected_wrapped_lines(
    text: &str,
    layout: &bmux_text_edit::WrapLayout,
    selection: Option<TextSelection>,
    base_style: Style,
    selection_style: Style,
) -> Vec<Line> {
    layout
        .line_ranges
        .iter()
        .map(|range| {
            let mut line = Line::new();
            for (offset, grapheme) in text[range.clone()].grapheme_indices(true) {
                let start = range.start.saturating_add(offset);
                let style = if selection_contains(selection, start) {
                    base_style.patch(selection_style)
                } else {
                    base_style
                };
                push_styled_grapheme(&mut line, grapheme, style);
            }
            line
        })
        .collect()
}

fn selection_contains(selection: Option<TextSelection>, byte_index: usize) -> bool {
    selection.is_some_and(|selection| byte_index >= selection.start && byte_index < selection.end)
}

fn push_styled_grapheme(line: &mut Line, grapheme: &str, style: Style) {
    if let Some(last) = line.spans.last_mut()
        && last.style == style
    {
        last.content.push_str(grapheme);
        return;
    }
    line.push_span(crate::text::Span::styled(grapheme.to_owned(), style));
}
