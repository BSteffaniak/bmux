//! Virtualized styled-line viewport primitives.

use bmux_keyboard::{KeyCode, KeyStroke};
use unicode_segmentation::UnicodeSegmentation;

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::style::{Modifier, Style};
use crate::text::{Line, Span};
use crate::widget::StatefulWidget;

/// Scroll state for [`Viewport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewportState {
    /// First visible line index.
    pub offset: usize,
}

impl ViewportState {
    /// Scroll up by `rows`.
    pub const fn scroll_up(&mut self, rows: usize) {
        self.offset = self.offset.saturating_sub(rows);
    }

    /// Scroll down by `rows`, clamped to `line_count`.
    pub fn scroll_down(&mut self, rows: usize, line_count: usize, viewport_height: u16) {
        self.offset = self
            .offset
            .saturating_add(rows)
            .min(max_offset(line_count, viewport_height));
    }

    /// Scroll to the first line.
    pub const fn scroll_to_top(&mut self) {
        self.offset = 0;
    }

    /// Scroll to the last page.
    pub fn scroll_to_bottom(&mut self, line_count: usize, viewport_height: u16) {
        self.offset = max_offset(line_count, viewport_height);
    }
}

/// Result of viewport key handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportKeyOutcome {
    /// Key was not handled as viewport navigation.
    Ignored,
    /// Viewport scroll offset changed or was clamped.
    Scrolled,
}

/// Key handling policy for [`Viewport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewportKeyHandler;

impl ViewportKeyHandler {
    /// Apply a key stroke to viewport state.
    pub fn handle_key(
        self,
        state: &mut ViewportState,
        line_count: usize,
        viewport_height: u16,
        stroke: KeyStroke,
    ) -> ViewportKeyOutcome {
        if !stroke.modifiers.is_empty() {
            return ViewportKeyOutcome::Ignored;
        }
        match stroke.key {
            KeyCode::Up => state.scroll_up(1),
            KeyCode::Down => state.scroll_down(1, line_count, viewport_height),
            KeyCode::PageUp => state.scroll_up(usize::from(viewport_height.max(1))),
            KeyCode::PageDown => {
                state.scroll_down(
                    usize::from(viewport_height.max(1)),
                    line_count,
                    viewport_height,
                );
            }
            KeyCode::Home => state.scroll_to_top(),
            KeyCode::End => state.scroll_to_bottom(line_count, viewport_height),
            KeyCode::Char(_)
            | KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Escape
            | KeyCode::Space
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Insert
            | KeyCode::F(_) => return ViewportKeyOutcome::Ignored,
        }
        ViewportKeyOutcome::Scrolled
    }
}

/// A virtualized styled-line viewport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewport<'lines> {
    lines: &'lines [Line],
    search_query: Option<String>,
    search_style: Style,
}

impl<'lines> Viewport<'lines> {
    /// Create a viewport over styled lines.
    #[must_use]
    pub const fn new(lines: &'lines [Line]) -> Self {
        Self {
            lines,
            search_query: None,
            search_style: Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// Set a plain-text search query to highlight.
    #[must_use]
    pub fn search(mut self, query: impl Into<String>) -> Self {
        let query = query.into();
        self.search_query = if query.is_empty() { None } else { Some(query) };
        self
    }

    /// Set search highlight style. This style patches over existing span style.
    #[must_use]
    pub const fn search_style(mut self, style: Style) -> Self {
        self.search_style = style;
        self
    }

    /// Return total line count.
    #[must_use]
    pub const fn line_count(&self) -> usize {
        self.lines.len()
    }
}

impl StatefulWidget for Viewport<'_> {
    type State = ViewportState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        state.offset = state.offset.min(max_offset(self.lines.len(), area.height));
        for (row, line) in self
            .lines
            .iter()
            .skip(state.offset)
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            let line = self.search_query.as_ref().map_or_else(
                || line.clone(),
                |query| highlight_line(line, query, self.search_style),
            );
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &line,
            );
        }
    }
}

fn max_offset(line_count: usize, viewport_height: u16) -> usize {
    line_count.saturating_sub(usize::from(viewport_height))
}

fn highlight_line(line: &Line, query: &str, search_style: Style) -> Line {
    let plain = line.plain_text();
    if query.is_empty() || !plain.contains(query) {
        return line.clone();
    }

    let mut result = Line::new();
    let mut byte_offset = 0usize;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let selected = plain[byte_offset..].starts_with(query);
            let style = if selected {
                span.style.patch(search_style)
            } else {
                span.style
            };
            push_styled_grapheme(&mut result, grapheme, style);
            byte_offset = byte_offset.saturating_add(grapheme.len());
        }
    }
    result
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

#[cfg(test)]
mod tests {
    use super::{Viewport, ViewportKeyHandler, ViewportKeyOutcome, ViewportState};
    use bmux_keyboard::{KeyCode, KeyStroke};

    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::style::{Color, Style};
    use crate::text::{Line, Span};
    use crate::widget::StatefulWidget;

    #[test]
    fn viewport_renders_visible_window() {
        let lines = vec![Line::raw("one"), Line::raw("two"), Line::raw("three")];
        let mut state = ViewportState { offset: 1 };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);

        Viewport::new(&lines).render(Rect::new(0, 0, 6, 2), &mut frame, &mut state);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("two   "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("three "));
    }

    #[test]
    fn viewport_key_handler_scrolls() {
        let mut state = ViewportState::default();
        let handler = ViewportKeyHandler;

        assert_eq!(
            handler.handle_key(&mut state, 10, 3, KeyStroke::simple(KeyCode::PageDown)),
            ViewportKeyOutcome::Scrolled
        );
        assert_eq!(state.offset, 3);
        assert_eq!(
            handler.handle_key(&mut state, 10, 3, KeyStroke::simple(KeyCode::End)),
            ViewportKeyOutcome::Scrolled
        );
        assert_eq!(state.offset, 7);
        assert_eq!(
            handler.handle_key(&mut state, 10, 3, KeyStroke::simple(KeyCode::Home)),
            ViewportKeyOutcome::Scrolled
        );
        assert_eq!(state.offset, 0);
    }

    #[test]
    fn viewport_highlights_search_matches() {
        let base = Style::new().fg(Color::Green);
        let highlight = Style::new().bg(Color::Yellow);
        let lines = vec![Line::from_spans(vec![Span::styled("hello world", base)])];
        let mut state = ViewportState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 11, 1));
        let mut frame = Frame::new(&mut buffer);

        Viewport::new(&lines)
            .search("world")
            .search_style(highlight)
            .render(Rect::new(0, 0, 11, 1), &mut frame, &mut state);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("hello world")
        );
        assert_eq!(
            frame.buffer().get(Point::new(6, 0)).map(|cell| cell.style),
            Some(base.patch(highlight))
        );
        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).map(|cell| cell.style),
            Some(base)
        );
    }
}
