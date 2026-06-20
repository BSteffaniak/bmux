//! Styled text primitives.

use crate::style::Style;
use crate::text_width::display_width;
use unicode_segmentation::UnicodeSegmentation;

/// A styled text span.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Span {
    /// Span text.
    pub content: String,
    /// Span style.
    pub style: Style,
}

impl Span {
    /// Create an unstyled span.
    #[must_use]
    pub fn raw(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            style: Style::new(),
        }
    }

    /// Create a styled span.
    #[must_use]
    pub fn styled(content: impl Into<String>, style: Style) -> Self {
        Self {
            content: content.into(),
            style,
        }
    }
    /// Return a copy of this span with `style` patched over its current style.
    #[must_use]
    pub fn patch_style(&self, style: Style) -> Self {
        Self::styled(self.content.clone(), self.style.patch(style))
    }

    /// Return the terminal display width of this span.
    #[must_use]
    pub fn width(&self) -> usize {
        display_width(&self.content)
    }
}

/// A line of styled text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line {
    /// Ordered spans in this line.
    pub spans: Vec<Span>,
}

impl Line {
    /// Create an empty line.
    #[must_use]
    pub const fn new() -> Self {
        Self { spans: Vec::new() }
    }

    /// Create a line from unstyled text.
    #[must_use]
    pub fn raw(content: impl Into<String>) -> Self {
        Self {
            spans: vec![Span::raw(content)],
        }
    }

    /// Create a line from spans.
    #[must_use]
    pub fn from_spans(spans: impl Into<Vec<Span>>) -> Self {
        Self {
            spans: spans.into(),
        }
    }

    /// Return a copy of this line with `style` applied behind each span's
    /// explicit style.
    ///
    /// This is useful when rendering text on an opaque surface: callers can
    /// supply the surface style as a fallback while preserving span-specific
    /// foreground colors and modifiers.
    #[must_use]
    pub fn with_fallback_style(&self, style: Style) -> Self {
        Self::from_spans(
            self.spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), style.patch(span.style)))
                .collect::<Vec<_>>(),
        )
    }

    /// Return a copy of this line with `style` patched over each span.
    #[must_use]
    pub fn patch_style(&self, style: Style) -> Self {
        Self::from_spans(
            self.spans
                .iter()
                .map(|span| span.patch_style(style))
                .collect::<Vec<_>>(),
        )
    }

    /// Return the terminal display width of this line.
    #[must_use]
    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }

    /// Return a copy truncated to terminal display width with an ellipsis when clipped.
    #[must_use]
    pub fn truncate(&self, width: usize) -> Self {
        truncate_line_to_display_width(self, width)
    }

    /// Return a styled viewport clipped to terminal display cells.
    ///
    /// Graphemes that would be split by the left or right viewport edge are
    /// omitted, preserving valid terminal cell alignment and span styles.
    #[must_use]
    pub fn viewport(&self, horizontal_offset: usize, width: usize) -> Self {
        line_viewport(self, horizontal_offset, width)
    }

    /// Append a span to the line.
    pub fn push_span(&mut self, span: Span) {
        self.spans.push(span);
    }

    /// Return the plain text for this line.
    #[must_use]
    pub fn plain_text(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.content.as_str())
            .collect()
    }
}

/// Return a copy of a styled line truncated to terminal display width.
///
/// The ellipsis inherits the style of the first clipped grapheme, or the final
/// visible span when clipping happens at the end of the line.
#[must_use]
pub fn truncate_line_to_display_width(line: &Line, width: usize) -> Line {
    if line.width() <= width {
        return line.clone();
    }
    if width == 0 {
        return Line::new();
    }
    if width == 1 {
        let style = line
            .spans
            .iter()
            .find(|span| !span.content.is_empty())
            .map_or_else(Style::new, |span| span.style);
        return Line::from_spans([Span::styled("…", style)]);
    }

    let body_width = width.saturating_sub(1);
    let mut used = 0usize;
    let mut spans: Vec<Span> = Vec::new();
    let mut ellipsis_style = None;
    'outer: for span in &line.spans {
        let mut content = String::new();
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if grapheme_width == 0 {
                continue;
            }
            if used.saturating_add(grapheme_width) > body_width {
                if !content.is_empty() {
                    push_or_merge_span(&mut spans, content, span.style);
                }
                ellipsis_style = Some(span.style);
                break 'outer;
            }
            content.push_str(grapheme);
            used = used.saturating_add(grapheme_width);
        }
        if !content.is_empty() {
            push_or_merge_span(&mut spans, content, span.style);
        }
    }
    let style = ellipsis_style
        .or_else(|| spans.last().map(|span| span.style))
        .unwrap_or_else(Style::new);
    push_or_merge_span(&mut spans, "…".to_owned(), style);
    Line::from_spans(spans)
}

/// Return a styled viewport clipped to terminal display cells.
///
/// This helper preserves span styles and never splits a Unicode grapheme. When
/// the viewport begins or ends inside a wide grapheme, that grapheme is omitted
/// so subsequent cells remain aligned.
#[must_use]
pub fn line_viewport(line: &Line, horizontal_offset: usize, width: usize) -> Line {
    if width == 0 {
        return Line::new();
    }
    if horizontal_offset == 0 && line.width() <= width {
        return line.clone();
    }

    let start = horizontal_offset;
    let end = start.saturating_add(width);
    let mut cursor = 0usize;
    let mut spans: Vec<Span> = Vec::new();
    for span in &line.spans {
        let mut content = String::new();
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if grapheme_width == 0 {
                continue;
            }
            let next = cursor.saturating_add(grapheme_width);
            if next <= start || cursor < start {
                cursor = next;
                continue;
            }
            if cursor >= end || next > end {
                break;
            }
            content.push_str(grapheme);
            cursor = next;
        }
        if !content.is_empty() {
            push_or_merge_span(&mut spans, content, span.style);
        }
        if cursor >= end {
            break;
        }
    }
    Line::from_spans(spans)
}

fn push_or_merge_span(spans: &mut Vec<Span>, content: String, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.push_str(&content);
        return;
    }
    spans.push(Span::styled(content, style));
}

/// Multiple lines of styled text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Text {
    /// Ordered lines.
    pub lines: Vec<Line>,
}

impl Text {
    /// Create empty text.
    #[must_use]
    pub const fn new() -> Self {
        Self { lines: Vec::new() }
    }

    /// Create text from one unstyled line.
    #[must_use]
    pub fn raw(content: impl Into<String>) -> Self {
        Self {
            lines: vec![Line::raw(content)],
        }
    }

    /// Create text from lines.
    #[must_use]
    pub fn from_lines(lines: impl Into<Vec<Line>>) -> Self {
        Self {
            lines: lines.into(),
        }
    }

    /// Return a copy of this text with `style` patched over each line.
    #[must_use]
    pub fn patch_style(&self, style: Style) -> Self {
        Self::from_lines(
            self.lines
                .iter()
                .map(|line| line.patch_style(style))
                .collect::<Vec<_>>(),
        )
    }

    /// Return the maximum terminal display width of all lines.
    #[must_use]
    pub fn width(&self) -> usize {
        self.lines.iter().map(Line::width).max().unwrap_or(0)
    }

    /// Append a line.
    pub fn push_line(&mut self, line: Line) {
        self.lines.push(line);
    }
}

impl From<&str> for Span {
    fn from(value: &str) -> Self {
        Self::raw(value)
    }
}

impl From<String> for Span {
    fn from(value: String) -> Self {
        Self::raw(value)
    }
}

impl From<&str> for Line {
    fn from(value: &str) -> Self {
        Self::raw(value)
    }
}

impl From<String> for Line {
    fn from(value: String) -> Self {
        Self::raw(value)
    }
}

impl From<&str> for Text {
    fn from(value: &str) -> Self {
        Self::raw(value)
    }
}

impl From<String> for Text {
    fn from(value: String) -> Self {
        Self::raw(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{Line, Span, Text, line_viewport};
    use crate::style::{Color, Style};

    #[test]
    fn line_plain_text_concatenates_span_content() {
        let line = Line::from_spans(vec![
            Span::styled("hello", Style::new().fg(Color::Green)),
            Span::raw(" world"),
        ]);

        assert_eq!(line.plain_text(), "hello world");
    }

    #[test]
    fn line_with_fallback_style_preserves_explicit_fields() {
        let fallback = Style::new().fg(Color::White).bg(Color::Black);
        let explicit = Style::new().fg(Color::Red);
        let line = Line::from_spans(vec![Span::styled("hello", explicit)]);

        let styled = line.with_fallback_style(fallback);

        assert_eq!(styled.spans[0].style, fallback.patch(explicit));
    }

    #[test]
    fn text_primitives_report_unicode_display_widths() {
        let span = Span::raw("a界");
        let line = Line::from_spans([span.clone(), Span::raw("b")]);
        let text = Text::from_lines([line.clone(), Line::from("x")]);

        assert_eq!(span.width(), 3);
        assert_eq!(line.width(), 4);
        assert_eq!(text.width(), 4);
    }

    #[test]
    fn line_truncate_preserves_styles_and_adds_ellipsis() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let line = Line::from_spans([Span::styled("ab", red), Span::styled("界cd", blue)]);
        let truncated = line.truncate(4);

        assert_eq!(truncated.plain_text(), "ab…");
        assert_eq!(truncated.spans[0], Span::styled("ab", red));
        assert_eq!(truncated.spans[1], Span::styled("…", blue));
    }

    #[test]
    fn line_truncate_handles_tiny_widths() {
        let style = Style::new().fg(Color::Red);
        let line = Line::from_spans([Span::styled("abc", style)]);

        assert_eq!(line.truncate(0), Line::new());
        assert_eq!(
            line.truncate(1),
            Line::from_spans([Span::styled("…", style)])
        );
    }

    #[test]
    fn line_truncate_does_not_split_graphemes() {
        let line = Line::from("a👨‍👩‍👧‍👦b");

        assert_eq!(line.truncate(3).plain_text(), "a…");
    }

    #[test]
    fn line_viewport_clips_ascii_and_preserves_styles() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let line = Line::from_spans([Span::styled("ab", red), Span::styled("cde", blue)]);
        let viewport = line_viewport(&line, 1, 3);

        assert_eq!(viewport.plain_text(), "bcd");
        assert_eq!(viewport.spans.len(), 2);
        assert_eq!(viewport.spans[0], Span::styled("b", red));
        assert_eq!(viewport.spans[1], Span::styled("cd", blue));
    }

    #[test]
    fn line_viewport_merges_adjacent_same_style_spans() {
        let style = Style::new().fg(Color::Green);
        let line = Line::from_spans([Span::styled("ab", style), Span::styled("cd", style)]);
        let viewport = line_viewport(&line, 1, 2);

        assert_eq!(viewport.spans, vec![Span::styled("bc", style)]);
    }

    #[test]
    fn line_viewport_does_not_split_combining_graphemes() {
        let line = Line::from("e\u{301}e\u{301}x");

        assert_eq!(line_viewport(&line, 0, 2).plain_text(), "e\u{301}e\u{301}");
        assert_eq!(line_viewport(&line, 1, 1).plain_text(), "e\u{301}");
    }

    #[test]
    fn line_viewport_does_not_split_emoji_zwj_sequences() {
        let family = "👨‍👩‍👧‍👦";
        let line = Line::from(format!("a{family}b"));

        assert_eq!(
            line_viewport(&line, 0, 3).plain_text(),
            format!("a{family}")
        );
        assert_eq!(line_viewport(&line, 2, 2).plain_text(), "b");
        assert_eq!(line_viewport(&line, 3, 1).plain_text(), "b");
    }

    #[test]
    fn line_viewport_omits_wide_graphemes_cut_by_edges() {
        let line = Line::from("a界b");

        assert_eq!(line_viewport(&line, 0, 2).plain_text(), "a");
        assert_eq!(line_viewport(&line, 1, 2).plain_text(), "界");
        assert_eq!(line_viewport(&line, 2, 2).plain_text(), "b");
    }

    #[test]
    fn text_primitives_patch_styles() {
        let line = Line::from_spans([Span::styled("hi", Style::new().fg(Color::Red))]);
        let patched = line.patch_style(Style::new().bg(Color::Blue));

        assert_eq!(
            patched.spans[0].style,
            Style::new().fg(Color::Red).bg(Color::Blue)
        );
    }
}
