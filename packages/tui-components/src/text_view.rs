//! Read-only text/paragraph viewer component.

use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Alignment, Line, Span, Text, TextBlock, TextWrap};
use bmux_tui::style::{Color, Style};
use bmux_tui::text_width::display_width;
use bmux_tui::widget::Widget;

/// Runtime state for [`TextView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextViewState {
    vertical_scroll: usize,
}

impl TextViewState {
    /// Create text-view state.
    #[must_use]
    pub const fn new() -> Self {
        Self { vertical_scroll: 0 }
    }

    /// Return vertical scroll offset in rendered rows.
    #[must_use]
    pub const fn vertical_scroll(&self) -> usize {
        self.vertical_scroll
    }

    /// Set vertical scroll offset in rendered rows.
    pub const fn set_vertical_scroll(&mut self, vertical_scroll: usize) {
        self.vertical_scroll = vertical_scroll;
    }
}

/// Text-view behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TextViewPolicy {
    /// Wrapping policy.
    pub wrap: TextWrap,
    /// Horizontal alignment.
    pub alignment: Alignment,
    /// Trim trailing whitespace before rendering.
    pub trim: bool,
    /// Keyboard scrolling enabled.
    pub keyboard: bool,
    /// Mouse-wheel scrolling enabled.
    pub mouse_wheel: bool,
    /// Fill background before rendering.
    pub background: bool,
}

impl TextViewPolicy {
    /// Bare render-only view.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            wrap: TextWrap::None,
            alignment: Alignment::Left,
            trim: false,
            keyboard: false,
            mouse_wheel: false,
            background: false,
        }
    }

    /// Scrollable paragraph view.
    #[must_use]
    pub const fn scrollable() -> Self {
        Self {
            wrap: TextWrap::Character,
            alignment: Alignment::Left,
            trim: true,
            keyboard: true,
            mouse_wheel: true,
            background: false,
        }
    }
}

impl Default for TextViewPolicy {
    fn default() -> Self {
        Self::scrollable()
    }
}

/// Visual styles for [`TextView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextViewStyles {
    /// Text style.
    pub text: Style,
    /// Empty-content style.
    pub empty: Style,
    /// Background fill style.
    pub background: Style,
}

impl Default for TextViewStyles {
    fn default() -> Self {
        Self {
            text: Style::new().fg(Color::White),
            empty: Style::new().fg(Color::BrightBlack),
            background: Style::new(),
        }
    }
}

/// Text-view input outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextViewOutcome {
    /// Event was ignored.
    Ignored,
    /// View should be redrawn.
    Redraw,
    /// Scroll offset changed.
    Scrolled { vertical_scroll: usize },
}

/// Pure text-view layout result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextViewLayout {
    /// Rendered lines after wrapping/trimming.
    pub lines: Vec<Line>,
    /// Clamped vertical scroll offset.
    pub vertical_scroll: usize,
}

/// Read-only rich text/paragraph viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextView<'a> {
    lines: &'a [Line],
    policy: TextViewPolicy,
    styles: TextViewStyles,
    empty: &'a str,
}

impl<'a> TextView<'a> {
    /// Create a text view over caller-owned lines.
    #[must_use]
    pub const fn new(lines: &'a [Line]) -> Self {
        Self {
            lines,
            policy: TextViewPolicy {
                wrap: TextWrap::Character,
                alignment: Alignment::Left,
                trim: true,
                keyboard: true,
                mouse_wheel: true,
                background: false,
            },
            styles: TextViewStyles {
                text: Style::new(),
                empty: Style::new(),
                background: Style::new(),
            },
            empty: "No content",
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TextViewPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TextViewStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set empty message.
    #[must_use]
    pub const fn empty(mut self, empty: &'a str) -> Self {
        self.empty = empty;
        self
    }

    /// Compute rendered lines and clamped scroll.
    #[must_use]
    pub fn layout(&self, area: Rect, state: &TextViewState) -> TextViewLayout {
        let lines = render_lines(self.lines, area.width, self.policy.wrap, self.policy.trim);
        let vertical_scroll = clamp_scroll(state.vertical_scroll, lines.len(), area.height);
        TextViewLayout {
            lines,
            vertical_scroll,
        }
    }

    /// Render text view.
    pub fn render(&self, area: Rect, state: &TextViewState, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if self.policy.background {
            frame.fill(area, " ", self.styles.background);
        }
        if self.lines.is_empty() {
            frame.write_line_with_fallback_style(area, &Line::from(self.empty), self.styles.empty);
            return;
        }
        let layout = self.layout(area, state);
        let text = Text::from_lines(layout.lines);
        TextBlock::new(text)
            .style(self.styles.text)
            .alignment(self.policy.alignment)
            .wrap(TextWrap::None)
            .vertical_scroll(layout.vertical_scroll)
            .render(area, frame);
    }

    /// Handle one event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut TextViewState,
        event: &Event,
    ) -> TextViewOutcome {
        match event {
            Event::Key(stroke) if self.policy.keyboard => match stroke.key {
                KeyCode::Up => self.scroll_by(area, state, -1),
                KeyCode::Down => self.scroll_by(area, state, 1),
                KeyCode::PageUp => self.scroll_by(area, state, -i32::from(area.height.max(1))),
                KeyCode::PageDown => self.scroll_by(area, state, i32::from(area.height.max(1))),
                KeyCode::Home => self.set_scroll(area, state, 0),
                KeyCode::End => {
                    let line_count = self.layout(area, state).lines.len();
                    self.set_scroll(area, state, line_count)
                }
                _ => TextViewOutcome::Ignored,
            },
            Event::Mouse(mouse) if self.policy.mouse_wheel && area.contains(mouse.position) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_by(area, state, -1),
                    MouseEventKind::ScrollDown => self.scroll_by(area, state, 1),
                    MouseEventKind::Down(_)
                    | MouseEventKind::Up(_)
                    | MouseEventKind::Drag(_)
                    | MouseEventKind::Move
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight => TextViewOutcome::Ignored,
                }
            }
            Event::Key(_)
            | Event::Mouse(_)
            | Event::Resize(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => TextViewOutcome::Ignored,
        }
    }

    fn scroll_by(&self, area: Rect, state: &mut TextViewState, delta: i32) -> TextViewOutcome {
        let current = i32::try_from(state.vertical_scroll).unwrap_or(i32::MAX);
        let next = usize::try_from(current.saturating_add(delta).max(0)).unwrap_or(usize::MAX);
        self.set_scroll(area, state, next)
    }

    fn set_scroll(&self, area: Rect, state: &mut TextViewState, scroll: usize) -> TextViewOutcome {
        let line_count = self.layout(area, state).lines.len();
        let next = clamp_scroll(scroll, line_count, area.height);
        if next == state.vertical_scroll {
            TextViewOutcome::Ignored
        } else {
            state.vertical_scroll = next;
            TextViewOutcome::Scrolled {
                vertical_scroll: next,
            }
        }
    }
}

fn render_lines(lines: &[Line], width: u16, wrap: TextWrap, trim: bool) -> Vec<Line> {
    let mut rendered = Vec::new();
    for line in lines {
        let line = if trim {
            trim_line_end(line)
        } else {
            line.clone()
        };
        match wrap {
            TextWrap::None => rendered.push(line),
            TextWrap::Character => rendered.extend(wrap_line(&line, usize::from(width.max(1)))),
            TextWrap::Word => rendered.extend(wrap_line_words(&line, usize::from(width.max(1)))),
        }
    }
    rendered
}

fn wrap_line(line: &Line, width: usize) -> Vec<Line> {
    let mut lines = vec![Line::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    for span in &line.spans {
        for ch in span.content.chars() {
            let ch_width = display_width(&ch.to_string());
            if col > 0 && col.saturating_add(ch_width) > width {
                lines.push(Line::new());
                row = row.saturating_add(1);
                col = 0;
            }
            push_styled_grapheme(&mut lines[row], ch, span.style);
            col = col.saturating_add(ch_width);
        }
    }
    lines
}

fn wrap_line_words(line: &Line, width: usize) -> Vec<Line> {
    let mut lines = vec![Line::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    for span in &line.spans {
        let mut segment = String::new();
        let mut segment_is_whitespace = false;
        for ch in span.content.chars() {
            let is_whitespace = ch.is_whitespace();
            if segment.is_empty() {
                segment_is_whitespace = is_whitespace;
            }
            if !segment.is_empty() && is_whitespace != segment_is_whitespace {
                push_word_segment(
                    &mut lines,
                    &mut row,
                    &mut col,
                    &segment,
                    span.style,
                    width,
                    segment_is_whitespace,
                );
                segment.clear();
                segment_is_whitespace = is_whitespace;
            }
            segment.push(ch);
        }
        if !segment.is_empty() {
            push_word_segment(
                &mut lines,
                &mut row,
                &mut col,
                &segment,
                span.style,
                width,
                segment_is_whitespace,
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
    let segment_width = display_width(segment);
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
        for ch in segment.chars() {
            let ch_width = display_width(&ch.to_string());
            if *col > 0 && col.saturating_add(ch_width) > width {
                lines.push(Line::new());
                *row = row.saturating_add(1);
                *col = 0;
            }
            push_styled_grapheme(&mut lines[*row], ch, style);
            *col = col.saturating_add(ch_width);
        }
        return;
    }
    for ch in segment.chars() {
        push_styled_grapheme(&mut lines[*row], ch, style);
    }
    *col = col.saturating_add(segment_width);
}

fn push_styled_grapheme(line: &mut Line, ch: char, style: Style) {
    if let Some(last) = line.spans.last_mut()
        && last.style == style
    {
        last.content.push(ch);
        return;
    }
    line.push_span(Span::styled(ch.to_string(), style));
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

fn clamp_scroll(scroll: usize, line_count: usize, height: u16) -> usize {
    if height == 0 {
        return 0;
    }
    scroll.min(line_count.saturating_sub(usize::from(height)))
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::prelude::{Alignment, Line};

    use super::{TextView, TextViewOutcome, TextViewPolicy, TextViewState};

    #[test]
    fn wraps_content_to_area_width() {
        let lines = [Line::from("abcdef")];
        let view = TextView::new(&lines);
        let layout = view.layout(Rect::new(0, 0, 3, 5), &TextViewState::new());

        assert_eq!(layout.lines.len(), 2);
    }

    #[test]
    fn word_wrap_prefers_word_boundaries() {
        let lines = [Line::from("one two")];
        let view = TextView::new(&lines).policy(TextViewPolicy {
            wrap: bmux_tui::prelude::TextWrap::Word,
            ..TextViewPolicy::bare()
        });
        let layout = view.layout(Rect::new(0, 0, 6, 2), &TextViewState::new());

        assert_eq!(layout.lines[0].plain_text(), "one");
        assert_eq!(layout.lines[1].plain_text(), "two");
    }

    #[test]
    fn no_wrap_keeps_source_lines() {
        let lines = [Line::from("abcdef")];
        let view = TextView::new(&lines).policy(TextViewPolicy::bare());
        let layout = view.layout(Rect::new(0, 0, 3, 5), &TextViewState::new());

        assert_eq!(layout.lines.len(), 1);
    }

    #[test]
    fn renders_center_aligned_text() {
        let lines = [Line::from("hi")];
        let view = TextView::new(&lines).policy(TextViewPolicy {
            alignment: Alignment::Center,
            ..TextViewPolicy::bare()
        });
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        view.render(Rect::new(0, 0, 6, 1), &TextViewState::new(), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("  hi  "));
    }

    #[test]
    fn keyboard_scrolls_and_clamps() {
        let lines = [Line::from("one"), Line::from("two"), Line::from("three")];
        let view = TextView::new(&lines);
        let mut state = TextViewState::new();

        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 10, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Down)),
            ),
            TextViewOutcome::Scrolled { vertical_scroll: 1 }
        );
        assert_eq!(state.vertical_scroll(), 1);
    }

    #[test]
    fn mouse_wheel_scrolls_when_inside_area() {
        let lines = [Line::from("one"), Line::from("two"), Line::from("three")];
        let view = TextView::new(&lines);
        let mut state = TextViewState::new();

        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 10, 1),
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::ScrollDown,
                    Point::new(1, 0)
                )),
            ),
            TextViewOutcome::Scrolled { vertical_scroll: 1 }
        );
    }

    #[test]
    fn renders_empty_content_message() {
        let lines = [];
        let view = TextView::new(&lines).empty("Nothing here");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 1));
        let mut frame = Frame::new(&mut buffer);

        view.render(Rect::new(0, 0, 14, 1), &TextViewState::new(), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Nothing here  ")
        );
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let lines = [Line::from("hello")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        TextView::new(&lines).render(Rect::new(0, 0, 0, 0), &TextViewState::new(), &mut frame);
    }
}
