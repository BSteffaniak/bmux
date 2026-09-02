//! Text input widget and key handling.

use std::hash::{Hash, Hasher};

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_text_edit::{TextEditBuffer, TextSelection, VisualCursor};
use unicode_segmentation::UnicodeSegmentation;

use crate::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use crate::geometry::{Point, Rect};
use crate::paint::{LocalRect, PaintCx};
use crate::style::Style;
use crate::text::Line;

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
    id: LayoutId,
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
    pub fn new(buffer: &'buffer TextEditBuffer) -> Self {
        Self {
            id: LayoutId::new("text-input"),
            buffer,
            style: Style::new(),
            selection_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
            placeholder: None,
            placeholder_style: Style::new(),
            cursor_visible: true,
            vertical_scroll: 0,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
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
        Self::project_with_layout(&layout, self.vertical_scroll, area.height)
    }

    fn project_with_layout(
        layout: &bmux_text_edit::WrapLayout,
        vertical_scroll: usize,
        height: u16,
    ) -> TextInputProjection {
        let lines = layout
            .lines
            .iter()
            .skip(vertical_scroll)
            .take(usize::from(height))
            .cloned()
            .collect();
        TextInputProjection {
            lines,
            cursor: VisualCursor {
                row: layout.cursor.row.saturating_sub(vertical_scroll),
                col: layout.cursor.col,
            },
        }
    }
    fn paint_scoped(&self, area: LocalRect, cx: &mut PaintCx<'_, '_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        if self.buffer.is_empty() {
            if let Some(placeholder) = &self.placeholder {
                let styled = placeholder.with_fallback_style(self.placeholder_style);
                cx.write_line_with_fallback_style(
                    LocalRect::new(area.x, area.y, area.width, 1),
                    &styled,
                    self.placeholder_style,
                );
            }
            if self.cursor_visible {
                cx.set_cursor(Point::new(0, 0), true);
            }
            return;
        }

        let layout = self.buffer.wrapped_layout(usize::from(area.width.max(1)));
        let vertical_scroll = if self.vertical_scroll == usize::MAX {
            scroll_offset_for_cursor_row(layout.cursor.row, area.height)
        } else {
            self.vertical_scroll
        };
        let projection = Self::project_with_layout(&layout, vertical_scroll, area.height);
        let rendered_lines = selected_wrapped_lines(
            self.buffer.text(),
            &layout,
            self.buffer.selection(),
            self.style,
            self.selection_style,
        );
        for (row, line) in rendered_lines
            .into_iter()
            .skip(vertical_scroll)
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(row) = i64::try_from(row) else {
                return;
            };
            cx.write_line_with_fallback_style(
                LocalRect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &line,
                self.style,
            );
        }

        if self.cursor_visible && projection.cursor.row < usize::from(area.height) {
            let cursor_col = u16::try_from(projection.cursor.col)
                .unwrap_or(u16::MAX)
                .min(area.width);
            let cursor_row = u16::try_from(projection.cursor.row).unwrap_or(u16::MAX);
            cx.set_cursor_local(cursor_col, cursor_row, true);
        }
    }

    fn layout_revision(&self) -> u64 {
        let mut state = std::collections::hash_map::DefaultHasher::new();
        self.buffer.text().hash(&mut state);
        self.vertical_scroll.hash(&mut state);
        self.placeholder
            .as_ref()
            .map(Line::plain_text)
            .hash(&mut state);
        state.finish()
    }

    fn paint_revision(&self) -> u64 {
        let mut state = std::collections::hash_map::DefaultHasher::new();
        if let Some(selection) = self.buffer.selection() {
            selection.start.hash(&mut state);
            selection.end.hash(&mut state);
        }
        self.buffer.cursor_grapheme_index().hash(&mut state);
        self.style.hash(&mut state);
        self.selection_style.hash(&mut state);
        self.placeholder_style.hash(&mut state);
        self.cursor_visible.hash(&mut state);
        state.finish()
    }
}

impl Component for TextInput<'_> {
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::new(self.layout_revision(), self.paint_revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = constraints.max_width();
        let rows = if self.buffer.is_empty() {
            usize::from(self.placeholder.is_some())
        } else {
            self.buffer
                .wrapped_layout(usize::from(width.max(1)))
                .lines
                .len()
                .max(1)
        };
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, rows)),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.paint_scoped(
            LocalRect::new(
                0,
                0,
                layout.size.width,
                u16::try_from(layout.size.height).unwrap_or(u16::MAX),
            ),
            cx,
        );
    }
}

fn scroll_offset_for_cursor_row(cursor_row: usize, height: u16) -> usize {
    cursor_row
        .saturating_add(1)
        .saturating_sub(usize::from(height))
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

#[cfg(test)]
mod tests {
    use super::{TextInput, TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
    use crate::buffer::Buffer;
    use crate::component::{Component, Constraints, LayoutCx};
    use crate::frame::Frame;
    use crate::geometry::{Rect, Size};
    use crate::paint::{LocalRect, PaintCx};
    use crate::style::{Modifier, Style};
    trait TextInputTestRender {
        fn render(&self, area: Rect, frame: &mut Frame<'_>);
    }

    impl TextInputTestRender for TextInput<'_> {
        fn render(&self, area: Rect, frame: &mut Frame<'_>) {
            let layout = self.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            PaintCx::new(frame).with_child(
                i32::from(area.x),
                i64::from(area.y),
                LocalRect::new(0, 0, area.width, area.height),
                |cx| self.paint(&layout, cx),
            );
        }
    }
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
    use bmux_text_edit::TextEditBuffer;

    #[test]
    fn text_input_key_handler_inserts_characters_and_deletes() {
        let mut edit = TextEditBuffer::new();
        let handler = TextInputKeyHandler::default();

        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Char('a'))),
            TextInputKeyOutcome::Edited
        );
        assert_eq!(edit.text(), "a");
        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Backspace)),
            TextInputKeyOutcome::Edited
        );
        assert_eq!(edit.text(), "");
    }

    #[test]
    fn text_input_key_handler_supports_submit_enter_behavior() {
        let mut edit = TextEditBuffer::from_text("run");
        let handler = TextInputKeyHandler::new(
            bmux_text_edit::keyboard::TextKeymap::default(),
            TextInputEnterBehavior::Submit,
        );

        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Enter)),
            TextInputKeyOutcome::Submitted
        );
        assert_eq!(edit.text(), "run");
    }

    #[test]
    fn text_input_key_handler_inserts_newline_by_default() {
        let mut edit = TextEditBuffer::from_text("a");
        let handler = TextInputKeyHandler::default();

        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Enter)),
            TextInputKeyOutcome::Edited
        );
        assert_eq!(edit.text(), "a\n");
    }

    #[test]
    fn text_input_key_handler_ignores_unknown_keys() {
        let mut edit = TextEditBuffer::new();
        let handler = TextInputKeyHandler::default();

        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Escape)),
            TextInputKeyOutcome::Ignored
        );
        assert_eq!(
            handler.handle_key(
                &mut edit,
                KeyStroke::with_modifiers(
                    KeyCode::Char('x'),
                    Modifiers {
                        super_key: true,
                        ..Modifiers::NONE
                    },
                ),
            ),
            TextInputKeyOutcome::Ignored
        );
    }

    #[test]
    fn component_paint_clips_text_and_cursor_to_the_scoped_viewport() {
        let edit = TextEditBuffer::from_text("abcdef");
        let input = TextInput::new(&edit);
        let layout = input.layout(Constraints::tight(Size::new(3, 2)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        let mut frame = Frame::new(&mut buffer);

        PaintCx::new(&mut frame).with_child(1, 0, LocalRect::new(0, 0, 2, 2), |cx| {
            input.paint(&layout, cx);
        });

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(" ab  "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some(" de  "));
        assert_eq!(frame.cursor(), None);
    }

    #[test]
    fn text_input_renders_placeholder_and_cursor_for_empty_buffer() {
        let edit = TextEditBuffer::new();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        TextInput::new(&edit)
            .placeholder("Ask")
            .render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("Ask     "));
        assert_eq!(
            frame.cursor(),
            Some(crate::frame::Cursor::visible(crate::geometry::Point::new(
                0, 0
            )))
        );
    }

    #[test]
    fn text_input_renders_wrapped_text_and_cursor() {
        let edit = TextEditBuffer::from_text("hello world");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let mut frame = Frame::new(&mut buffer);

        TextInput::new(&edit).render(Rect::new(0, 0, 5, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("hello"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("world"));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("     "));
        assert_eq!(
            frame.cursor(),
            Some(crate::frame::Cursor::visible(crate::geometry::Point::new(
                5, 1
            )))
        );
    }

    #[test]
    fn text_input_supports_vertical_scroll() {
        let edit = TextEditBuffer::from_text("abcdef");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        TextInput::new(&edit)
            .vertical_scroll(1)
            .render(Rect::new(0, 0, 3, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("def"));
    }

    #[test]
    fn text_input_styles_selection() {
        let mut edit = TextEditBuffer::from_text("hello");
        edit.move_cursor(bmux_text_edit::TextMotion::Start);
        edit.move_cursor_with_selection(
            bmux_text_edit::TextMotion::Right,
            bmux_text_edit::SelectionMode::Extend,
        );
        edit.move_cursor_with_selection(
            bmux_text_edit::TextMotion::Right,
            bmux_text_edit::SelectionMode::Extend,
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let mut frame = Frame::new(&mut buffer);
        let selection_style = Style::new().add_modifier(Modifier::REVERSED);

        TextInput::new(&edit)
            .selection_style(selection_style)
            .render(Rect::new(0, 0, 5, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("hello"));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(selection_style)
        );
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(1, 0))
                .map(|cell| cell.style),
            Some(selection_style)
        );
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(2, 0))
                .map(|cell| cell.style),
            Some(Style::new())
        );
    }

    #[test]
    fn text_input_selection_can_span_wrapped_lines() {
        let mut edit = TextEditBuffer::from_text("abcd");
        edit.move_cursor(bmux_text_edit::TextMotion::Start);
        edit.move_cursor_with_selection(
            bmux_text_edit::TextMotion::End,
            bmux_text_edit::SelectionMode::Extend,
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);
        let selection_style = Style::new().add_modifier(Modifier::REVERSED);

        TextInput::new(&edit)
            .selection_style(selection_style)
            .render(Rect::new(0, 0, 2, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("ab"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("cd"));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 1))
                .map(|cell| cell.style),
            Some(selection_style)
        );
    }
}
