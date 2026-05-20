//! Styled text block widget and wrapping helpers.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::style::Style;
use crate::text::{Line, Text};
use crate::widget::Widget;
use crate::widgets::Alignment;

/// Text wrapping policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextWrap {
    /// Do not wrap lines; rendering clips to the target area.
    #[default]
    None,
    /// Wrap at grapheme boundaries when a line exceeds the target width.
    Character,
}

/// A simple styled text block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBlock {
    text: Text,
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
            alignment: Alignment::Left,
            wrap: TextWrap::None,
            trim: false,
            vertical_scroll: 0,
        }
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
            frame.write_line(Rect::new(line_area.x, line_y, line_area.width, 1), line);
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

fn push_styled_grapheme(line: &mut Line, grapheme: &str, style: Style) {
    if let Some(last) = line.spans.last_mut()
        && last.style == style
    {
        last.content.push_str(grapheme);
        return;
    }
    line.push_span(crate::text::Span::styled(grapheme.to_owned(), style));
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
