#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Unicode-aware terminal text editing primitives.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// A terminal visual cursor position measured in rows and columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VisualCursor {
    /// Zero-based visual row.
    pub row: usize,
    /// Zero-based terminal display column.
    pub col: usize,
}

/// A soft-wrapped text projection and the projected cursor position.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WrapLayout {
    /// Renderable visual lines.
    pub lines: Vec<String>,
    /// Cursor position in the visual line grid.
    pub cursor: VisualCursor,
}

/// A single-line viewport projection and the projected cursor column.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LineViewport {
    /// Visible text slice rendered into the viewport.
    pub text: String,
    /// Cursor display column within the visible text.
    pub cursor_col: usize,
}

/// Cursor movement commands for [`TextEditBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextMotion {
    /// Move one grapheme left.
    Left,
    /// Move one grapheme right.
    Right,
    /// Move to the previous word boundary.
    WordLeft,
    /// Move to the next word boundary.
    WordRight,
    /// Move to the start of the buffer.
    Start,
    /// Move to the end of the buffer.
    End,
    /// Move to the start of the current hard line.
    LineStart,
    /// Move to the end of the current hard line.
    LineEnd,
}

/// Editable UTF-8 text plus cursor state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TextEditBuffer {
    text: String,
    cursor: usize,
}

impl TextEditBuffer {
    /// Create an empty text buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
    }

    /// Create a text buffer with the cursor at the end of `text`.
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self { text, cursor }
    }

    /// Return the full buffer text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Return true when the buffer is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Return the cursor byte index.
    #[must_use]
    pub const fn cursor_byte_index(&self) -> usize {
        self.cursor
    }

    /// Return the cursor grapheme index.
    #[must_use]
    pub fn cursor_grapheme_index(&self) -> usize {
        self.text[..self.cursor].graphemes(true).count()
    }

    /// Clear all text and reset the cursor.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Insert one character at the cursor.
    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor = self.cursor.saturating_add(ch.len_utf8());
    }

    /// Insert a string at the cursor.
    pub fn insert_str(&mut self, value: &str) {
        self.text.insert_str(self.cursor, value);
        self.cursor = self.cursor.saturating_add(value.len());
    }

    /// Insert a newline at the cursor.
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Delete the grapheme before the cursor.
    pub fn delete_backward(&mut self) {
        if let Some(start) = previous_grapheme_boundary(&self.text, self.cursor) {
            self.text.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    /// Delete the grapheme at the cursor.
    pub fn delete_forward(&mut self) {
        if let Some(end) = next_grapheme_boundary(&self.text, self.cursor) {
            self.text.drain(self.cursor..end);
        }
    }

    /// Move the cursor according to `motion`.
    pub fn move_cursor(&mut self, motion: TextMotion) {
        self.cursor = match motion {
            TextMotion::Left => previous_grapheme_boundary(&self.text, self.cursor).unwrap_or(0),
            TextMotion::Right => {
                next_grapheme_boundary(&self.text, self.cursor).unwrap_or(self.text.len())
            }
            TextMotion::WordLeft => previous_word_boundary(&self.text, self.cursor),
            TextMotion::WordRight => next_word_boundary(&self.text, self.cursor),
            TextMotion::Start => 0,
            TextMotion::End => self.text.len(),
            TextMotion::LineStart => self.text[..self.cursor]
                .rfind('\n')
                .map_or(0, |index| index.saturating_add(1)),
            TextMotion::LineEnd => self.text[self.cursor..]
                .find('\n')
                .map_or(self.text.len(), |index| self.cursor.saturating_add(index)),
        };
    }

    /// Return a terminal-width single-line viewport ending near the cursor.
    #[must_use]
    pub fn line_viewport(&self, width: usize) -> LineViewport {
        if width == 0 {
            return LineViewport {
                text: String::new(),
                cursor_col: 0,
            };
        }

        let graphemes = grapheme_spans(&self.text);
        let cursor_index = graphemes
            .iter()
            .position(|span| span.start == self.cursor)
            .unwrap_or(graphemes.len());
        let mut start = cursor_index;
        let mut col = 0usize;
        while start > 0 {
            let width_next = graphemes[start - 1].width;
            if col.saturating_add(width_next) > width.saturating_sub(1) {
                break;
            }
            col = col.saturating_add(width_next);
            start -= 1;
        }

        let mut text = String::new();
        let mut used = 0usize;
        let mut cursor_col = 0usize;
        for (index, span) in graphemes.iter().enumerate().skip(start) {
            if index == cursor_index {
                cursor_col = used;
            }
            if used.saturating_add(span.width) > width {
                break;
            }
            text.push_str(&self.text[span.start..span.end]);
            used = used.saturating_add(span.width);
        }
        if cursor_index >= graphemes.len() {
            cursor_col = used.min(width);
        }

        LineViewport { text, cursor_col }
    }

    /// Return a soft-wrapped projection for `width` terminal columns.
    #[must_use]
    pub fn wrapped_layout(&self, width: usize) -> WrapLayout {
        let width = width.max(1);
        let mut lines = vec![String::new()];
        let mut row = 0usize;
        let mut col = 0usize;
        let mut cursor = VisualCursor::default();

        for span in grapheme_spans(&self.text) {
            if span.start == self.cursor {
                cursor = VisualCursor { row, col };
            }
            let g = &self.text[span.start..span.end];
            if g == "\n" {
                lines.push(String::new());
                row = row.saturating_add(1);
                col = 0;
                continue;
            }
            if col > 0 && col.saturating_add(span.width) > width {
                lines.push(String::new());
                row = row.saturating_add(1);
                col = 0;
            }
            lines[row].push_str(g);
            col = col.saturating_add(span.width);
        }
        if self.cursor == self.text.len() {
            cursor = VisualCursor { row, col };
        }

        WrapLayout { lines, cursor }
    }
}

#[derive(Debug, Clone, Copy)]
struct GraphemeSpan {
    start: usize,
    end: usize,
    width: usize,
}

fn grapheme_spans(text: &str) -> Vec<GraphemeSpan> {
    text.grapheme_indices(true)
        .map(|(start, grapheme)| GraphemeSpan {
            start,
            end: start.saturating_add(grapheme.len()),
            width: UnicodeWidthStr::width(grapheme).max(1),
        })
        .collect()
}

fn previous_grapheme_boundary(text: &str, cursor: usize) -> Option<usize> {
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index)
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> Option<usize> {
    if cursor >= text.len() {
        return None;
    }
    text[cursor..]
        .grapheme_indices(true)
        .nth(1)
        .map_or(Some(text.len()), |(index, _)| {
            Some(cursor.saturating_add(index))
        })
}

fn previous_word_boundary(text: &str, cursor: usize) -> usize {
    let spans = grapheme_spans(text);
    let mut index = spans
        .iter()
        .position(|span| span.start == cursor)
        .unwrap_or(spans.len());
    while index > 0 && is_word_separator(&text[spans[index - 1].start..spans[index - 1].end]) {
        index -= 1;
    }
    while index > 0 && !is_word_separator(&text[spans[index - 1].start..spans[index - 1].end]) {
        index -= 1;
    }
    spans.get(index).map_or(0, |span| span.start)
}

fn next_word_boundary(text: &str, cursor: usize) -> usize {
    let spans = grapheme_spans(text);
    let mut index = spans
        .iter()
        .position(|span| span.start == cursor)
        .unwrap_or(spans.len());
    while index < spans.len() && is_word_separator(&text[spans[index].start..spans[index].end]) {
        index += 1;
    }
    while index < spans.len() && !is_word_separator(&text[spans[index].start..spans[index].end]) {
        index += 1;
    }
    spans.get(index).map_or(text.len(), |span| span.start)
}

fn is_word_separator(grapheme: &str) -> bool {
    grapheme.chars().all(char::is_whitespace)
        || grapheme.chars().all(|ch| ch.is_ascii_punctuation())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_and_deletes_at_cursor() {
        let mut buffer = TextEditBuffer::from_text("ac");
        buffer.move_cursor(TextMotion::Left);
        buffer.insert_char('b');
        assert_eq!(buffer.text(), "abc");
        buffer.delete_backward();
        assert_eq!(buffer.text(), "ac");
        buffer.delete_forward();
        assert_eq!(buffer.text(), "a");
    }

    #[test]
    fn moves_over_combining_grapheme() {
        let mut buffer = TextEditBuffer::from_text("e\u{301}x");
        buffer.move_cursor(TextMotion::Start);
        buffer.move_cursor(TextMotion::Right);
        assert_eq!(buffer.cursor_byte_index(), "e\u{301}".len());
        buffer.delete_backward();
        assert_eq!(buffer.text(), "x");
    }

    #[test]
    fn line_viewport_uses_display_width() {
        let buffer = TextEditBuffer::from_text("a界b");
        let viewport = buffer.line_viewport(3);
        assert_eq!(viewport.text, "b");
        assert_eq!(viewport.cursor_col, 1);
    }

    #[test]
    fn word_movement_skips_whitespace() {
        let mut buffer = TextEditBuffer::from_text("hello, world");
        buffer.move_cursor(TextMotion::WordLeft);
        assert_eq!(buffer.cursor_byte_index(), 7);
        buffer.move_cursor(TextMotion::WordRight);
        assert_eq!(buffer.cursor_byte_index(), buffer.text().len());
    }

    #[test]
    fn wraps_and_projects_cursor() {
        let mut buffer = TextEditBuffer::from_text("ab界c");
        buffer.move_cursor(TextMotion::Left);
        let layout = buffer.wrapped_layout(4);
        assert_eq!(layout.lines, vec!["ab界".to_string(), "c".to_string()]);
        assert_eq!(layout.cursor, VisualCursor { row: 0, col: 4 });
    }
}
