//! Text input widget and key handling.

use std::hash::{Hash, Hasher};

use bmux_text_edit::{TextEditBuffer, TextSelection};
use unicode_segmentation::UnicodeSegmentation;

use crate::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use crate::geometry::Point;
use crate::paint::{LocalRect, PaintCx};
use crate::style::Style;
use crate::text::Line;

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
    wrapped: Option<&'buffer bmux_text_edit::WrapLayout>,
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
            wrapped: None,
        }
    }

    /// Use a projection measured for the current buffer, cursor, and paint width.
    #[must_use]
    pub const fn wrapped_layout(mut self, layout: &'buffer bmux_text_edit::WrapLayout) -> Self {
        self.wrapped = Some(layout);
        self
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

    fn paint_scoped(&self, size: LogicalSize, cx: &mut PaintCx<'_, '_>) {
        let clip = cx.area();
        let left = u64::try_from(clip.x.max(0)).unwrap_or(0).min(size.width);
        let right = u64::try_from(i64::from(clip.x) + i64::from(clip.width))
            .unwrap_or(0)
            .min(size.width);
        let top = u64::try_from(clip.y.max(0)).unwrap_or(0).min(size.height);
        let bottom = u64::try_from(clip.y.saturating_add(i64::from(clip.height)))
            .unwrap_or(0)
            .min(size.height);
        let area = LocalRect::new(
            i32::try_from(left).unwrap_or(i32::MAX),
            i64::try_from(top).unwrap_or(i64::MAX),
            u16::try_from(right.saturating_sub(left)).unwrap_or(0),
            u16::try_from(bottom.saturating_sub(top)).unwrap_or(0),
        );
        if area.width == 0 || area.height == 0 {
            return;
        }

        if self.buffer.is_empty() {
            if let Some(placeholder) = &self.placeholder {
                let styled = placeholder
                    .with_fallback_style(self.placeholder_style)
                    .viewport(
                        usize::try_from(left).unwrap_or(usize::MAX),
                        usize::from(area.width),
                    );
                cx.write_line_with_fallback_style(
                    LocalRect::new(area.x, 0, area.width, 1),
                    &styled,
                    self.placeholder_style,
                );
            }
            if self.cursor_visible {
                cx.set_cursor(Point::new(0, 0), true);
            }
            return;
        }

        let measured = self.wrapped.is_none().then(|| {
            self.buffer
                .wrapped_layout(usize::try_from(size.width.max(1)).unwrap_or(usize::MAX))
        });
        let layout = self
            .wrapped
            .or(measured.as_ref())
            .expect("wrapped projection");
        let vertical_scroll = if self.vertical_scroll == usize::MAX {
            scroll_offset_for_cursor_row(layout.cursor.row, size.height)
        } else {
            self.vertical_scroll
        };
        let source_start =
            vertical_scroll.saturating_add(usize::try_from(top).unwrap_or(usize::MAX));
        let rendered_lines = selected_wrapped_lines(
            self.buffer.text(),
            layout,
            source_start..source_start.saturating_add(usize::from(area.height)),
            self.buffer.selection(),
            self.style,
            self.selection_style,
        );
        for (row, line) in rendered_lines.into_iter().enumerate() {
            let Ok(row) = i64::try_from(row) else {
                return;
            };
            cx.write_line_with_fallback_style(
                LocalRect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &line.viewport(
                    usize::try_from(left).unwrap_or(usize::MAX),
                    usize::from(area.width),
                ),
                self.style,
            );
        }

        if self.cursor_visible
            && let Some(row) = layout.cursor.row.checked_sub(vertical_scroll)
            && u64::try_from(row).unwrap_or(u64::MAX) < size.height
        {
            cx.set_cursor_logical(
                u64::try_from(layout.cursor.col).unwrap_or(u64::MAX),
                u64::try_from(row).unwrap_or(u64::MAX),
                true,
            );
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
                .wrapped_layout(usize::try_from(width.max(1)).unwrap_or(usize::MAX))
                .lines
                .len()
                .max(1)
        };
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                width,
                u64::try_from(rows).expect("row count fits u64"),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.paint_scoped(layout.size, cx);
    }
}

fn scroll_offset_for_cursor_row(cursor_row: usize, height: u64) -> usize {
    cursor_row
        .saturating_add(1)
        .saturating_sub(usize::try_from(height).unwrap_or(usize::MAX))
}

fn selected_wrapped_lines(
    text: &str,
    layout: &bmux_text_edit::WrapLayout,
    rows: std::ops::Range<usize>,
    selection: Option<TextSelection>,
    base_style: Style,
    selection_style: Style,
) -> Vec<Line> {
    layout
        .line_ranges
        .iter()
        .skip(rows.start)
        .take(rows.end.saturating_sub(rows.start))
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
    use super::TextInput;
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
    use bmux_text_edit::TextEditBuffer;

    #[test]
    fn logical_width_preserves_long_line_and_cursor() {
        let edit = TextEditBuffer::from_text(format!("{}end", "x".repeat(70_000)));
        let input = TextInput::new(&edit);
        let layout = input.layout(Constraints::for_width(70_004), &mut LayoutCx::new());
        assert_eq!(layout.size.height, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame)
            .with_child_size(-70_000, 0, layout.size, |cx| input.paint(&layout, cx));
        assert_eq!(frame.cursor().unwrap().position.x, 3);
        let row: String = buffer
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect();
        assert_eq!(row, "end ");
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
