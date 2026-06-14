//! Styled text block widget and wrapping helpers.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::style::Style;
use crate::text::{Line, Span, Text};
use crate::widget::Widget;

/// Horizontal text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    /// Align to the left edge.
    #[default]
    Left,
    /// Center within the available width.
    Center,
    /// Align to the right edge.
    Right,
}

/// Text wrapping policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextWrap {
    /// Do not wrap lines; rendering clips to the target area.
    #[default]
    None,
    /// Wrap at grapheme boundaries when a line exceeds the target width.
    Character,
    /// Wrap at word boundaries when possible, falling back to grapheme wrapping
    /// for words longer than the target width.
    Word,
}

/// A simple styled text block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBlock {
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
            text: text.into(),
            style: Style::new(),
            alignment: Alignment::Left,
            wrap: TextWrap::None,
            trim: false,
            vertical_scroll: 0,
        }
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

impl Widget for TextBlock {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }

        let lines = render_lines_for_text_block(&self.text, area.width, self.wrap, self.trim);
        for (line_index, line) in lines
            .iter()
            .skip(self.vertical_scroll)
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(line_offset) = u16::try_from(line_index) else {
                return;
            };
            let line_y = area.y.saturating_add(line_offset);
            let line_area = aligned_line_area(area, line, self.alignment)
                .unwrap_or_else(|| Rect::new(area.x, line_y, 0, 1));
            frame.fill(Rect::new(area.x, line_y, area.width, 1), " ", self.style);
            frame.write_line(
                Rect::new(line_area.x, line_y, line_area.width, 1),
                &line.with_fallback_style(self.style),
            );
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
            TextWrap::Character => lines.extend(wrap_line(&line, usize::from(width.max(1)))),
            TextWrap::Word => lines.extend(wrap_line_words(&line, usize::from(width.max(1)))),
        }
    }
    lines
}

fn wrap_line(line: &Line, width: usize) -> Vec<Line> {
    let mut lines = vec![Line::new()];
    let mut row = 0usize;
    let mut col = 0usize;

    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if col > 0 && col.saturating_add(grapheme_width) > width {
                lines.push(Line::new());
                row = row.saturating_add(1);
                col = 0;
            }
            push_styled_grapheme(&mut lines[row], grapheme, span.style);
            col = col.saturating_add(grapheme_width);
        }
    }

    lines
}

fn wrap_line_words(line: &Line, width: usize) -> Vec<Line> {
    let mut lines = vec![Line::new()];
    let mut row = 0usize;
    let mut col = 0usize;

    for span in &line.spans {
        let mut current_word = String::new();
        let mut current_is_whitespace = false;
        for grapheme in span.content.graphemes(true) {
            let is_whitespace = grapheme.chars().all(char::is_whitespace);
            if current_word.is_empty() {
                current_is_whitespace = is_whitespace;
            }
            if !current_word.is_empty() && is_whitespace != current_is_whitespace {
                push_word_segment(
                    &mut lines,
                    &mut row,
                    &mut col,
                    &current_word,
                    span.style,
                    width,
                    current_is_whitespace,
                );
                current_word.clear();
                current_is_whitespace = is_whitespace;
            }
            current_word.push_str(grapheme);
        }
        if !current_word.is_empty() {
            push_word_segment(
                &mut lines,
                &mut row,
                &mut col,
                &current_word,
                span.style,
                width,
                current_is_whitespace,
            );
        }
    }

    lines.into_iter().map(|line| trim_line_end(&line)).collect()
}

fn push_word_segment(
    lines: &mut Vec<Line>,
    row: &mut usize,
    col: &mut usize,
    segment: &str,
    style: Style,
    width: usize,
    is_whitespace: bool,
) {
    let segment_width = UnicodeWidthStr::width(segment);
    if is_whitespace && *col == 0 {
        return;
    }
    if *col > 0 && col.saturating_add(segment_width) > width {
        lines.push(Line::new());
        *row = row.saturating_add(1);
        *col = 0;
        if is_whitespace {
            return;
        }
    }
    if segment_width > width {
        for grapheme in segment.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if *col > 0 && col.saturating_add(grapheme_width) > width {
                lines.push(Line::new());
                *row = row.saturating_add(1);
                *col = 0;
            }
            push_styled_grapheme(&mut lines[*row], grapheme, style);
            *col = col.saturating_add(grapheme_width);
        }
        return;
    }
    push_styled_grapheme(&mut lines[*row], segment, style);
    *col = col.saturating_add(segment_width);
}

fn push_styled_grapheme(line: &mut Line, grapheme: &str, style: Style) {
    if let Some(last) = line.spans.last_mut()
        && last.style == style
    {
        last.content.push_str(grapheme);
        return;
    }
    line.push_span(Span::styled(grapheme.to_owned(), style));
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

fn aligned_line_area(area: Rect, line: &Line, alignment: Alignment) -> Option<Rect> {
    let width = line_width(line).min(area.width);
    if width == 0 {
        return None;
    }
    let remaining = area.width.saturating_sub(width);
    let x_offset = match alignment {
        Alignment::Left => 0,
        Alignment::Center => remaining / 2,
        Alignment::Right => remaining,
    };
    Some(Rect::new(area.x.saturating_add(x_offset), area.y, width, 1))
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
    use crate::frame::Frame;
    use crate::geometry::Rect;
    use crate::style::{Color, Style};
    use crate::text::{Line, Text};
    use crate::widget::Widget;

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
}
