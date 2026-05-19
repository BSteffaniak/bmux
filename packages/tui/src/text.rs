//! Styled text primitives.

use crate::style::Style;

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
    use super::{Line, Span};
    use crate::style::{Color, Style};

    #[test]
    fn line_plain_text_concatenates_span_content() {
        let line = Line::from_spans(vec![
            Span::styled("hello", Style::new().fg(Color::Green)),
            Span::raw(" world"),
        ]);

        assert_eq!(line.plain_text(), "hello world");
    }
}
