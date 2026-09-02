//! Virtualized styled-line viewport primitives.

use std::hash::{Hash, Hasher};

use bmux_keyboard::{KeyCode, KeyStroke};
use unicode_segmentation::UnicodeSegmentation;

use crate::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata, LayoutNode,
    LogicalSize,
};
use crate::paint::{LocalRect, PaintCx};
use crate::style::{Modifier, Style};
use crate::text::{Line, Span};

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
    id: LayoutId,
    lines: &'lines [Line],
    offset: usize,
    search_query: Option<String>,
    search_style: Style,
}

impl<'lines> Viewport<'lines> {
    /// Create a viewport over styled lines.
    #[must_use]
    pub fn new(lines: &'lines [Line]) -> Self {
        Self {
            id: LayoutId::new("viewport"),
            lines,
            offset: 0,
            search_query: None,
            search_style: Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set the first visible line.
    #[must_use]
    pub const fn offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
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

impl Component for Viewport<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut layout);
        self.lines.len().hash(&mut layout);
        self.offset.hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.lines).hash(&mut paint);
        self.search_query.hash(&mut paint);
        self.search_style.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let height = constraints
            .max_height()
            .map_or_else(|| self.lines.len().saturating_sub(self.offset), usize::from);
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(constraints.max_width(), height)),
        )
        .with_metadata(LayoutMetadata::new().semantic("viewport"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let offset = self.offset.min(max_offset(
            self.lines.len(),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        ));
        for (row, line) in self
            .lines
            .iter()
            .skip(offset)
            .take(layout.size.height)
            .enumerate()
        {
            let line = self.search_query.as_ref().map_or_else(
                || line.clone(),
                |query| highlight_line(line, query, self.search_style),
            );
            cx.write_line(
                LocalRect::new(
                    0,
                    i64::try_from(row).unwrap_or(i64::MAX),
                    layout.size.width,
                    1,
                ),
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
    use crate::component::{Component, Constraints, LayoutCx};
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::paint::{LocalRect, PaintCx};
    use crate::style::{Color, Style};
    use crate::text::{Line, Span};

    fn paint_viewport(viewport: &Viewport<'_>, area: Rect, frame: &mut Frame<'_>) {
        let layout = viewport.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        PaintCx::new(frame).with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| viewport.paint(&layout, cx),
        );
    }

    #[test]
    fn viewport_renders_visible_window() {
        let lines = vec![Line::raw("one"), Line::raw("two"), Line::raw("three")];
        let state = ViewportState { offset: 1 };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);

        paint_viewport(
            &Viewport::new(&lines).offset(state.offset),
            Rect::new(0, 0, 6, 2),
            &mut frame,
        );

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
        let state = ViewportState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 11, 1));
        let mut frame = Frame::new(&mut buffer);

        paint_viewport(
            &Viewport::new(&lines)
                .offset(state.offset)
                .search("world")
                .search_style(highlight),
            Rect::new(0, 0, 11, 1),
            &mut frame,
        );

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
