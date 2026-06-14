//! Styled text primitives.

use crate::style::Style;
use crate::text_width::display_width;

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
    use super::{Line, Span, Text};
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
    fn text_primitives_patch_styles() {
        let line = Line::from_spans([Span::styled("hi", Style::new().fg(Color::Red))]);
        let patched = line.patch_style(Style::new().bg(Color::Blue));

        assert_eq!(
            patched.spans[0].style,
            Style::new().fg(Color::Red).bg(Color::Blue)
        );
    }
}
