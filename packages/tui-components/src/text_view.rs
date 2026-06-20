//! Read-only text/paragraph viewer component.

use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Alignment, Line, Span, Text, TextBlock, TextWrap};
use bmux_tui::style::{Color, Style};
use bmux_tui::text::line_viewport;
use bmux_tui::text_width::display_width;
use bmux_tui::widget::Widget;

use crate::scroll_area::ScrollAreaScrollbarMode;
use crate::scrollbar::{Scrollbar, ScrollbarPolicy, ScrollbarState};

/// Runtime state for [`TextView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextViewState {
    vertical_scroll: usize,
    horizontal_scroll: usize,
}

impl TextViewState {
    /// Create text-view state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            vertical_scroll: 0,
            horizontal_scroll: 0,
        }
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

    /// Return horizontal scroll offset in cells for no-wrap mode.
    #[must_use]
    pub const fn horizontal_scroll(&self) -> usize {
        self.horizontal_scroll
    }

    /// Set horizontal scroll offset in cells for no-wrap mode.
    pub const fn set_horizontal_scroll(&mut self, horizontal_scroll: usize) {
        self.horizontal_scroll = horizontal_scroll;
    }

    /// Clamp scroll offsets to the supplied rendered content bounds.
    pub fn clamp_to(&mut self, line_count: usize, height: u16, max_horizontal_scroll: usize) {
        self.vertical_scroll = clamp_scroll(self.vertical_scroll, line_count, height);
        self.horizontal_scroll = self.horizontal_scroll.min(max_horizontal_scroll);
    }
}

/// Highlight range applied to caller-owned source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextViewHighlight {
    /// Source line index.
    pub line: usize,
    /// Start character offset, inclusive.
    pub start: usize,
    /// End character offset, exclusive.
    pub end: usize,
    /// Style patched onto highlighted text.
    pub style: Style,
}

impl TextViewHighlight {
    /// Create a highlight range.
    #[must_use]
    pub const fn new(line: usize, start: usize, end: usize, style: Style) -> Self {
        Self {
            line,
            start,
            end,
            style,
        }
    }
}

/// Selection range applied to caller-owned source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextViewSelection {
    /// Source line index.
    pub line: usize,
    /// Start character offset, inclusive.
    pub start: usize,
    /// End character offset, exclusive.
    pub end: usize,
    /// Style patched onto selected text.
    pub style: Style,
}

impl TextViewSelection {
    /// Create a selection range.
    #[must_use]
    pub const fn new(line: usize, start: usize, end: usize, style: Style) -> Self {
        Self {
            line,
            start,
            end,
            style,
        }
    }
}

/// Cursor rendering hook for read-only text views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextViewCursor {
    /// Source line index.
    pub line: usize,
    /// Character offset to style as cursor.
    pub column: usize,
    /// Style patched onto the cursor cell.
    pub style: Style,
}

impl TextViewCursor {
    /// Create a cursor hook.
    #[must_use]
    pub const fn new(line: usize, column: usize, style: Style) -> Self {
        Self {
            line,
            column,
            style,
        }
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
    /// Integrated vertical scrollbar layout mode.
    pub vertical_scrollbar: ScrollAreaScrollbarMode,
    /// Integrated horizontal scrollbar layout mode.
    pub horizontal_scrollbar: ScrollAreaScrollbarMode,
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
            vertical_scrollbar: ScrollAreaScrollbarMode::Hidden,
            horizontal_scrollbar: ScrollAreaScrollbarMode::Hidden,
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
            vertical_scrollbar: ScrollAreaScrollbarMode::Hidden,
            horizontal_scrollbar: ScrollAreaScrollbarMode::Hidden,
        }
    }
    /// Return this policy with integrated vertical scrollbar mode changed.
    #[must_use]
    pub const fn vertical_scrollbar(mut self, vertical_scrollbar: ScrollAreaScrollbarMode) -> Self {
        self.vertical_scrollbar = vertical_scrollbar;
        self
    }

    /// Return this policy with integrated horizontal scrollbar mode changed.
    #[must_use]
    pub const fn horizontal_scrollbar(
        mut self,
        horizontal_scrollbar: ScrollAreaScrollbarMode,
    ) -> Self {
        self.horizontal_scrollbar = horizontal_scrollbar;
        self
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
    highlights: &'a [TextViewHighlight],
    selection: Option<TextViewSelection>,
    cursor: Option<TextViewCursor>,
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
            highlights: &[],
            selection: None,
            cursor: None,
            policy: TextViewPolicy {
                wrap: TextWrap::Character,
                alignment: Alignment::Left,
                trim: true,
                keyboard: true,
                mouse_wheel: true,
                background: false,
                vertical_scrollbar: ScrollAreaScrollbarMode::Hidden,
                horizontal_scrollbar: ScrollAreaScrollbarMode::Hidden,
            },
            styles: TextViewStyles {
                text: Style::new(),
                empty: Style::new(),
                background: Style::new(),
            },
            empty: "No content",
        }
    }

    /// Set highlighted source text ranges.
    #[must_use]
    pub const fn highlights(mut self, highlights: &'a [TextViewHighlight]) -> Self {
        self.highlights = highlights;
        self
    }

    /// Set selected source text range.
    #[must_use]
    pub const fn selection(mut self, selection: Option<TextViewSelection>) -> Self {
        self.selection = selection;
        self
    }

    /// Set read-only cursor rendering hook.
    #[must_use]
    pub const fn cursor(mut self, cursor: Option<TextViewCursor>) -> Self {
        self.cursor = cursor;
        self
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
        let render_area = self.content_area(area);
        let lines = apply_ranges(self.lines, self.highlights, self.selection, self.cursor);
        let lines = render_lines(
            &lines,
            render_area.width,
            self.policy.wrap,
            self.policy.trim,
        );
        let vertical_scroll = self.clamped_vertical_scroll(area, state);
        TextViewLayout {
            lines,
            vertical_scroll,
        }
    }

    /// Return content area after integrated scrollbar reservation.
    #[must_use]
    pub const fn content_area(&self, area: Rect) -> Rect {
        let reserve_vertical = matches!(
            self.policy.vertical_scrollbar,
            ScrollAreaScrollbarMode::Gutter
        ) && area.width > 0;
        let reserve_horizontal = matches!(
            self.policy.horizontal_scrollbar,
            ScrollAreaScrollbarMode::Gutter
        ) && area.height > 0;
        Rect::new(
            area.x,
            area.y,
            if reserve_vertical {
                area.width.saturating_sub(1)
            } else {
                area.width
            },
            if reserve_horizontal {
                area.height.saturating_sub(1)
            } else {
                area.height
            },
        )
    }

    /// Return integrated vertical scrollbar area when enabled.
    #[must_use]
    pub const fn vertical_scrollbar_area(&self, area: Rect) -> Option<Rect> {
        if matches!(
            self.policy.vertical_scrollbar,
            ScrollAreaScrollbarMode::Hidden
        ) || area.width == 0
        {
            None
        } else {
            Some(Rect::new(
                area.right().saturating_sub(1),
                area.y,
                1,
                self.content_area(area).height,
            ))
        }
    }

    /// Return integrated horizontal scrollbar area when enabled.
    #[must_use]
    pub const fn horizontal_scrollbar_area(&self, area: Rect) -> Option<Rect> {
        if matches!(
            self.policy.horizontal_scrollbar,
            ScrollAreaScrollbarMode::Hidden
        ) || area.height == 0
        {
            None
        } else {
            Some(Rect::new(
                area.x,
                area.bottom().saturating_sub(1),
                self.content_area(area).width,
                1,
            ))
        }
    }

    /// Return maximum source line display width.
    #[must_use]
    pub fn content_width(&self) -> usize {
        self.lines.iter().map(Line::width).max().unwrap_or(0)
    }

    /// Return the maximum vertical scroll for this area.
    #[must_use]
    pub fn max_vertical_scroll(&self, area: Rect) -> usize {
        let content_area = self.content_area(area);
        let lines = apply_ranges(self.lines, self.highlights, self.selection, self.cursor);
        let line_count = render_lines(
            &lines,
            content_area.width,
            self.policy.wrap,
            self.policy.trim,
        )
        .len();
        clamp_scroll(line_count, line_count, content_area.height)
    }

    /// Return the maximum horizontal scroll for this area.
    #[must_use]
    pub fn max_horizontal_scroll(&self, area: Rect) -> usize {
        let content_area = self.content_area(area);
        if self.policy.wrap == TextWrap::None {
            max_horizontal_scroll(self.lines, content_area.width)
        } else {
            0
        }
    }

    /// Return the clamped vertical scroll for this area.
    #[must_use]
    pub fn clamped_vertical_scroll(&self, area: Rect, state: &TextViewState) -> usize {
        let content_area = self.content_area(area);
        let lines = apply_ranges(self.lines, self.highlights, self.selection, self.cursor);
        let line_count = render_lines(
            &lines,
            content_area.width,
            self.policy.wrap,
            self.policy.trim,
        )
        .len();
        clamp_scroll(state.vertical_scroll, line_count, content_area.height)
    }

    /// Return the clamped horizontal scroll for this area.
    #[must_use]
    pub fn clamped_horizontal_scroll(&self, area: Rect, state: &TextViewState) -> usize {
        state
            .horizontal_scroll
            .min(self.max_horizontal_scroll(area))
    }

    /// Clamp caller-owned state to valid scroll bounds for this area.
    pub fn clamp_state(&self, area: Rect, state: &mut TextViewState) {
        let content_area = self.content_area(area);
        let lines = apply_ranges(self.lines, self.highlights, self.selection, self.cursor);
        let line_count = render_lines(
            &lines,
            content_area.width,
            self.policy.wrap,
            self.policy.trim,
        )
        .len();
        state.clamp_to(
            line_count,
            content_area.height,
            self.max_horizontal_scroll(area),
        );
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
        let content_area = self.content_area(area);
        let layout = self.layout(area, state);
        let lines = if self.policy.wrap == TextWrap::None && state.horizontal_scroll > 0 {
            let horizontal_scroll = self.clamped_horizontal_scroll(area, state);
            layout
                .lines
                .into_iter()
                .map(|line| {
                    line_viewport(&line, horizontal_scroll, usize::from(content_area.width))
                })
                .collect()
        } else {
            layout.lines
        };
        let text = Text::from_lines(lines);
        TextBlock::new(text)
            .style(self.styles.text)
            .alignment(self.policy.alignment)
            .wrap(TextWrap::None)
            .vertical_scroll(layout.vertical_scroll)
            .render(content_area, frame);
        self.render_scrollbars(area, content_area, state, layout.vertical_scroll, frame);
    }

    fn render_scrollbars(
        &self,
        area: Rect,
        content_area: Rect,
        state: &TextViewState,
        vertical_scroll: usize,
        frame: &mut Frame<'_>,
    ) {
        if let Some(scrollbar_area) = self.vertical_scrollbar_area(area) {
            let content_len = self.layout(area, state).lines.len();
            let scrollbar_state = ScrollbarState::new(
                u16::try_from(content_len).unwrap_or(u16::MAX),
                content_area.height,
            )
            .offset(u16::try_from(vertical_scroll).unwrap_or(u16::MAX));
            Scrollbar::new().policy(ScrollbarPolicy::vertical()).render(
                scrollbar_area,
                &scrollbar_state,
                frame,
            );
        }
        if let Some(scrollbar_area) = self.horizontal_scrollbar_area(area) {
            let scrollbar_state = ScrollbarState::new(
                u16::try_from(self.content_width()).unwrap_or(u16::MAX),
                content_area.width,
            )
            .offset(u16::try_from(self.clamped_horizontal_scroll(area, state)).unwrap_or(u16::MAX));
            Scrollbar::new()
                .policy(ScrollbarPolicy::horizontal())
                .render(scrollbar_area, &scrollbar_state, frame);
        }
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
                KeyCode::Left => self.scroll_horizontal_by(area, state, -1),
                KeyCode::Right => self.scroll_horizontal_by(area, state, 1),
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

    fn scroll_horizontal_by(
        &self,
        area: Rect,
        state: &mut TextViewState,
        delta: i32,
    ) -> TextViewOutcome {
        if self.policy.wrap != TextWrap::None {
            return TextViewOutcome::Ignored;
        }
        let current = i32::try_from(state.horizontal_scroll).unwrap_or(i32::MAX);
        let next = usize::try_from(current.saturating_add(delta).max(0)).unwrap_or(usize::MAX);
        let next = next.min(self.max_horizontal_scroll(area));
        if next == state.horizontal_scroll {
            TextViewOutcome::Ignored
        } else {
            state.horizontal_scroll = next;
            TextViewOutcome::Redraw
        }
    }
}

fn apply_ranges(
    lines: &[Line],
    highlights: &[TextViewHighlight],
    selection: Option<TextViewSelection>,
    cursor: Option<TextViewCursor>,
) -> Vec<Line> {
    if highlights.is_empty() && selection.is_none() && cursor.is_none() {
        return lines.to_vec();
    }
    lines
        .iter()
        .enumerate()
        .map(|(line_index, line)| {
            apply_line_ranges(line, line_index, highlights, selection, cursor)
        })
        .collect()
}

fn apply_line_ranges(
    line: &Line,
    line_index: usize,
    highlights: &[TextViewHighlight],
    selection: Option<TextViewSelection>,
    cursor: Option<TextViewCursor>,
) -> Line {
    let line_highlights = highlights
        .iter()
        .copied()
        .filter(|highlight| highlight.line == line_index && highlight.start < highlight.end)
        .collect::<Vec<_>>();
    let line_selection = selection
        .filter(|selection| selection.line == line_index && selection.start < selection.end);
    let line_cursor = cursor.filter(|cursor| cursor.line == line_index);
    if line_highlights.is_empty() && line_selection.is_none() && line_cursor.is_none() {
        return line.clone();
    }
    let mut spans = Vec::new();
    let mut char_index = 0usize;
    for span in &line.spans {
        for ch in span.content.chars() {
            let mut style = line_highlights
                .iter()
                .filter(|highlight| char_index >= highlight.start && char_index < highlight.end)
                .fold(span.style, |style, highlight| style.patch(highlight.style));
            if let Some(selection) = line_selection
                && char_index >= selection.start
                && char_index < selection.end
            {
                style = style.patch(selection.style);
            }
            if let Some(cursor) = line_cursor
                && char_index == cursor.column
            {
                style = style.patch(cursor.style);
            }
            spans.push(Span::styled(ch.to_string(), style));
            char_index = char_index.saturating_add(1);
        }
    }
    Line::from_spans(spans)
}

fn max_horizontal_scroll(lines: &[Line], width: u16) -> usize {
    lines
        .iter()
        .map(|line| display_width(&line.plain_text()).saturating_sub(usize::from(width)))
        .max()
        .unwrap_or(0)
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
            TextWrap::Character => {
                let wrapped = wrap_line(&line, usize::from(width.max(1)));
                rendered.extend(trim_wrapped_lines(wrapped, trim));
            }
            TextWrap::Word => {
                let wrapped = wrap_line_words(&line, usize::from(width.max(1)));
                rendered.extend(trim_wrapped_lines(wrapped, trim));
            }
        }
    }
    rendered
}

fn trim_wrapped_lines(lines: Vec<Line>, trim: bool) -> Vec<Line> {
    if trim {
        lines.into_iter().map(|line| trim_line_end(&line)).collect()
    } else {
        lines
    }
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
    lines
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
    use bmux_tui::style::{Color, Style};

    use super::{
        TextView, TextViewCursor, TextViewHighlight, TextViewOutcome, TextViewPolicy,
        TextViewSelection, TextViewState,
    };
    use crate::scroll_area::ScrollAreaScrollbarMode;

    #[test]
    fn wraps_content_to_area_width() {
        let lines = [Line::from("abcdef")];
        let view = TextView::new(&lines);
        let layout = view.layout(Rect::new(0, 0, 3, 5), &TextViewState::new());

        assert_eq!(layout.lines.len(), 2);
    }

    #[test]
    fn highlights_source_ranges_without_owning_search_state() {
        let lines = [Line::from("abcdef")];
        let highlights = [TextViewHighlight::new(
            0,
            2,
            4,
            Style::new().fg(Color::Yellow),
        )];
        let view = TextView::new(&lines).highlights(&highlights);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        view.render(Rect::new(0, 0, 6, 1), &TextViewState::new(), &mut frame);

        assert_eq!(
            frame.buffer().get(Point::new(1, 0)).map(|cell| cell.style),
            Some(Style::new())
        );
        assert_eq!(
            frame.buffer().get(Point::new(2, 0)).map(|cell| cell.style),
            Some(Style::new().fg(Color::Yellow))
        );
        assert_eq!(
            frame.buffer().get(Point::new(3, 0)).map(|cell| cell.style),
            Some(Style::new().fg(Color::Yellow))
        );
    }

    #[test]
    fn unicode_width_is_respected_when_wrapping_and_scrolling() {
        let lines = [Line::from("a界b")];
        let view = TextView::new(&lines);
        let layout = view.layout(Rect::new(0, 0, 3, 2), &TextViewState::new());

        assert_eq!(layout.lines[0].plain_text(), "a界");
        assert_eq!(layout.lines[1].plain_text(), "b");

        let mut state = TextViewState::new();
        state.set_horizontal_scroll(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let mut frame = Frame::new(&mut buffer);

        TextView::new(&lines).policy(TextViewPolicy::bare()).render(
            Rect::new(0, 0, 2, 1),
            &state,
            &mut frame,
        );

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("界"));
    }

    #[test]
    fn selection_and_cursor_hooks_patch_styles_without_text_edit_behavior() {
        let lines = [Line::from("abcdef")];
        let view = TextView::new(&lines)
            .selection(Some(TextViewSelection::new(
                0,
                1,
                3,
                Style::new().bg(Color::Blue),
            )))
            .cursor(Some(TextViewCursor::new(0, 4, Style::new().fg(Color::Red))));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        view.render(Rect::new(0, 0, 6, 1), &TextViewState::new(), &mut frame);

        assert_eq!(
            frame.buffer().get(Point::new(1, 0)).map(|cell| cell.style),
            Some(Style::new().bg(Color::Blue))
        );
        assert_eq!(
            frame.buffer().get(Point::new(2, 0)).map(|cell| cell.style),
            Some(Style::new().bg(Color::Blue))
        );
        assert_eq!(
            frame.buffer().get(Point::new(4, 0)).map(|cell| cell.style),
            Some(Style::new().fg(Color::Red))
        );
    }

    #[test]
    fn wrap_trim_removes_trailing_whitespace_from_wrapped_rows() {
        let lines = [Line::from("ab  cd")];
        let view = TextView::new(&lines).policy(TextViewPolicy {
            trim: true,
            ..TextViewPolicy::default()
        });
        let layout = view.layout(Rect::new(0, 0, 4, 2), &TextViewState::new());

        assert_eq!(layout.lines[0].plain_text(), "ab");
        assert_eq!(layout.lines[1].plain_text(), "cd");
    }

    #[test]
    fn clamp_helpers_account_for_wrapped_line_count() {
        let lines = [Line::from("abcdef")];
        let view = TextView::new(&lines);
        let mut state = TextViewState::new();
        state.set_vertical_scroll(99);

        assert_eq!(view.max_vertical_scroll(Rect::new(0, 0, 3, 1)), 1);
        assert_eq!(
            view.clamped_vertical_scroll(Rect::new(0, 0, 3, 1), &state),
            1
        );
        view.clamp_state(Rect::new(0, 0, 3, 1), &mut state);
        assert_eq!(state.vertical_scroll(), 1);
    }

    #[test]
    fn clamp_helpers_account_for_horizontal_scroll() {
        let lines = [Line::from("abcdef")];
        let view = TextView::new(&lines).policy(TextViewPolicy::bare());
        let mut state = TextViewState::new();
        state.set_horizontal_scroll(99);

        assert_eq!(view.max_horizontal_scroll(Rect::new(0, 0, 3, 1)), 3);
        assert_eq!(
            view.clamped_horizontal_scroll(Rect::new(0, 0, 3, 1), &state),
            3
        );
        view.clamp_state(Rect::new(0, 0, 3, 1), &mut state);
        assert_eq!(state.horizontal_scroll(), 3);
    }

    #[test]
    fn horizontal_scroll_clips_no_wrap_content() {
        let lines = [Line::from("abcdef")];
        let mut state = TextViewState::new();
        state.set_horizontal_scroll(2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        TextView::new(&lines).policy(TextViewPolicy::bare()).render(
            Rect::new(0, 0, 3, 1),
            &state,
            &mut frame,
        );

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("cde"));
    }

    #[test]
    fn renders_integrated_scrollbars() {
        let lines = [
            Line::from("abcdef"),
            Line::from("ghijkl"),
            Line::from("mnopqr"),
        ];
        let mut state = TextViewState::new();
        state.set_vertical_scroll(1);
        state.set_horizontal_scroll(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 3));
        let mut frame = Frame::new(&mut buffer);

        TextView::new(&lines)
            .policy(
                TextViewPolicy::bare()
                    .vertical_scrollbar(ScrollAreaScrollbarMode::Gutter)
                    .horizontal_scrollbar(ScrollAreaScrollbarMode::Gutter),
            )
            .render(Rect::new(0, 0, 4, 3), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("hij│"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("nop█"));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("█── "));
    }

    #[test]
    fn horizontal_scroll_handles_wide_characters() {
        let lines = [Line::from("a界b")];
        let mut state = TextViewState::new();
        state.set_horizontal_scroll(2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let mut frame = Frame::new(&mut buffer);

        TextView::new(&lines).policy(TextViewPolicy::bare()).render(
            Rect::new(0, 0, 2, 1),
            &state,
            &mut frame,
        );

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("b "));
    }

    #[test]
    fn keyboard_left_right_scrolls_horizontally_in_no_wrap_mode() {
        let lines = [Line::from("abcdef")];
        let view = TextView::new(&lines).policy(TextViewPolicy {
            keyboard: true,
            ..TextViewPolicy::bare()
        });
        let mut state = TextViewState::new();

        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 3, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Right)),
            ),
            TextViewOutcome::Redraw
        );
        assert_eq!(state.horizontal_scroll(), 1);
        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 3, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Left)),
            ),
            TextViewOutcome::Redraw
        );
        assert_eq!(state.horizontal_scroll(), 0);
    }

    #[test]
    fn word_wrap_prefers_word_boundaries() {
        let lines = [Line::from("one two")];
        let view = TextView::new(&lines).policy(TextViewPolicy {
            wrap: bmux_tui::prelude::TextWrap::Word,
            trim: true,
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
