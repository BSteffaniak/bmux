//! Styled text block widget and wrapping helpers.

use std::hash::{Hash, Hasher};

use crate::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata, LayoutNode,
    LogicalSize,
};
use crate::paint::{LocalRect, PaintCx};
use crate::style::Style;
use crate::text::{Line, Text, wrap_line_character, wrap_line_word};

pub use crate::text::TextWrap;

/// Horizontal text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Alignment {
    /// Align to the left edge.
    #[default]
    Left,
    /// Center within the available width.
    Center,
    /// Align to the right edge.
    Right,
}

/// A simple styled text block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBlock {
    id: LayoutId,
    text: Text,
    style: Style,
    alignment: Alignment,
    wrap: TextWrap,
    trim: bool,
    vertical_scroll: usize,
}

impl TextBlock {
    /// Create a text block.
    #[must_use]
    pub fn new(text: impl Into<Text>) -> Self {
        Self {
            id: LayoutId::new("text-block"),
            text: text.into(),
            style: Style::new(),
            alignment: Alignment::Left,
            wrap: TextWrap::None,
            trim: false,
            vertical_scroll: 0,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set base style used to fill rendered rows and as a fallback behind text
    /// spans.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set horizontal alignment.
    #[must_use]
    pub const fn alignment(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Set wrapping policy.
    #[must_use]
    pub const fn wrap(mut self, wrap: TextWrap) -> Self {
        self.wrap = wrap;
        self
    }

    /// Set whether trailing whitespace is trimmed before rendering.
    #[must_use]
    pub const fn trim(mut self, trim: bool) -> Self {
        self.trim = trim;
        self
    }

    /// Set vertical scroll offset in rendered rows.
    #[must_use]
    pub const fn vertical_scroll(mut self, rows: usize) -> Self {
        self.vertical_scroll = rows;
        self
    }

    /// Return this block's text.
    #[must_use]
    pub const fn text(&self) -> &Text {
        &self.text
    }
}

impl Component for TextBlock {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut layout);
        self.text.width().hash(&mut layout);
        self.text.lines.len().hash(&mut layout);
        format!("{:?}", self.wrap).hash(&mut layout);
        self.trim.hash(&mut layout);
        self.vertical_scroll.hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.text).hash(&mut paint);
        self.style.hash(&mut paint);
        self.alignment.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            u16::try_from(self.text.width())
                .unwrap_or(u16::MAX)
                .clamp(constraints.min_width(), constraints.max_width())
        };
        let rows = render_lines_for_text_block(&self.text, width, self.wrap, self.trim)
            .len()
            .saturating_sub(self.vertical_scroll);
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, rows)),
        )
        .with_metadata(LayoutMetadata::new().semantic("text-block"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let lines =
            render_lines_for_text_block(&self.text, layout.size.width, self.wrap, self.trim);
        for (line_index, line) in lines
            .iter()
            .skip(self.vertical_scroll)
            .take(usize::from(height))
            .enumerate()
        {
            let row = u16::try_from(line_index).unwrap_or(u16::MAX);
            let row_area = LocalRect::new(0, i64::from(row), layout.size.width, 1);
            cx.fill(row_area, " ", self.style);
            let width = line_width(line).min(layout.size.width);
            let remaining = layout.size.width.saturating_sub(width);
            let x = match self.alignment {
                Alignment::Left => 0,
                Alignment::Center => remaining / 2,
                Alignment::Right => remaining,
            };
            cx.write_line(
                LocalRect::new(i32::from(x), i64::from(row), width, 1),
                &line.with_fallback_style(self.style),
            );
            cx.push_damage(row_area);
        }
    }
}

fn render_lines_for_text_block(text: &Text, width: u16, wrap: TextWrap, trim: bool) -> Vec<Line> {
    let mut lines = Vec::new();
    for line in &text.lines {
        let line = if trim {
            trim_line_end(line)
        } else {
            line.clone()
        };
        match wrap {
            TextWrap::None => lines.push(line),
            TextWrap::Character => {
                let wrapped = wrap_line_character(&line, usize::from(width.max(1)));
                lines.extend(trim_wrapped_lines(wrapped, trim));
            }
            TextWrap::Word => {
                let wrapped = wrap_line_word(&line, usize::from(width.max(1)));
                lines.extend(trim_wrapped_lines(wrapped, trim));
            }
        }
    }
    lines
}

fn trim_wrapped_lines(lines: Vec<Line>, trim: bool) -> Vec<Line> {
    if trim {
        lines.into_iter().map(|line| trim_line_end(&line)).collect()
    } else {
        lines
    }
}

fn trim_line_end(line: &Line) -> Line {
    let mut spans = line.spans.clone();
    while let Some(last) = spans.last_mut() {
        let trimmed_len = last.content.trim_end().len();
        if trimmed_len == last.content.len() {
            break;
        }
        last.content.truncate(trimmed_len);
        if !last.content.is_empty() {
            break;
        }
        spans.pop();
    }
    Line::from_spans(spans)
}

fn line_width(line: &Line) -> u16 {
    let width: usize = line
        .spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_str()))
        .sum();
    u16::try_from(width).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::{Alignment, TextBlock, TextWrap};
    use crate::buffer::Buffer;
    use crate::component::{Component, Constraints, LayoutCx};
    use crate::frame::Frame;
    use crate::geometry::Rect;
    use crate::paint::{LocalRect, PaintCx};
    use crate::style::{Color, Style};
    use crate::text::{Line, Text};
    trait TextBlockTestRender {
        fn render(&self, area: Rect, frame: &mut Frame<'_>);
    }

    impl TextBlockTestRender for TextBlock {
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

    #[test]
    fn text_block_wraps_at_grapheme_boundaries() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 3));
        let mut frame = Frame::new(&mut buffer);

        TextBlock::new("abcdef")
            .wrap(TextWrap::Character)
            .render(Rect::new(0, 0, 4, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("abcd"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("ef  "));
    }

    #[test]
    fn text_block_wrap_preserves_styles() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);
        let style = Style::new().fg(Color::Red);
        let text = Text::from_lines(vec![Line::from_spans(vec![crate::text::Span::styled(
            "abcd", style,
        )])]);

        TextBlock::new(text)
            .wrap(TextWrap::Character)
            .render(Rect::new(0, 0, 2, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("cd"));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 1))
                .map(|cell| cell.style),
            Some(style)
        );
    }

    #[test]
    fn text_block_supports_trim_and_vertical_scroll() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);
        let text = Text::from_lines(vec![Line::raw("ab  "), Line::raw("cd")]);

        TextBlock::new(text)
            .trim(true)
            .vertical_scroll(1)
            .render(Rect::new(0, 0, 4, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("cd  "));
    }

    #[test]
    fn text_block_renders_lines() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        let mut frame = Frame::new(&mut buffer);
        let text = Text::from_lines(vec![Line::raw("hi"), Line::raw("yo")]);

        TextBlock::new(text).render(Rect::new(0, 0, 5, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("hi   "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("yo   "));
    }

    #[test]
    fn text_block_style_fills_rendered_rows() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let mut frame = Frame::new(&mut buffer);
        let style = Style::new().fg(Color::White).bg(Color::Black);

        TextBlock::new("hi")
            .style(style)
            .render(Rect::new(0, 0, 5, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("hi   "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(4, 0))
                .map(|cell| cell.style),
            Some(style)
        );
    }

    #[test]
    fn text_block_wrap_trim_removes_trailing_whitespace_from_wrapped_rows() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        let mut frame = Frame::new(&mut buffer);

        TextBlock::new("ab  cd")
            .wrap(TextWrap::Character)
            .trim(true)
            .render(Rect::new(0, 0, 4, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("ab  "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("cd  "));
    }

    #[test]
    fn text_block_word_wrap_prefers_word_boundaries() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);

        TextBlock::new("one two")
            .wrap(TextWrap::Word)
            .render(Rect::new(0, 0, 6, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("one   "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("two   "));
    }

    #[test]
    fn text_block_honors_center_alignment() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        TextBlock::new("hi")
            .alignment(Alignment::Center)
            .render(Rect::new(0, 0, 6, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("  hi  "));
    }

    #[test]
    fn text_block_honors_right_alignment() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        TextBlock::new("hi")
            .alignment(Alignment::Right)
            .render(Rect::new(0, 0, 6, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("    hi"));
    }
}
