//! Built-in neutral widgets.

use bmux_text_edit::{TextEditBuffer, TextSelection, VisualCursor};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::frame::Frame;
use crate::geometry::{Insets, Rect};
use crate::style::Style;
use crate::text::{Line, Text};
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

/// Border glyph set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderSet {
    /// Top-left corner.
    pub top_left: char,
    /// Top-right corner.
    pub top_right: char,
    /// Bottom-left corner.
    pub bottom_left: char,
    /// Bottom-right corner.
    pub bottom_right: char,
    /// Horizontal edge.
    pub horizontal: char,
    /// Vertical edge.
    pub vertical: char,
}

impl BorderSet {
    /// Single-line border glyphs.
    pub const SINGLE: Self = Self {
        top_left: '┌',
        top_right: '┐',
        bottom_left: '└',
        bottom_right: '┘',
        horizontal: '─',
        vertical: '│',
    };

    /// Rounded border glyphs.
    pub const ROUNDED: Self = Self {
        top_left: '╭',
        top_right: '╮',
        bottom_left: '╰',
        bottom_right: '╯',
        horizontal: '─',
        vertical: '│',
    };

    /// ASCII-safe border glyphs.
    pub const ASCII: Self = Self {
        top_left: '+',
        top_right: '+',
        bottom_left: '+',
        bottom_right: '+',
        horizontal: '-',
        vertical: '|',
    };
}

/// Border configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Border {
    /// Border glyphs.
    pub set: BorderSet,
    /// Border style.
    pub style: Style,
}

impl Border {
    /// Create a border with a glyph set and style.
    #[must_use]
    pub const fn new(set: BorderSet, style: Style) -> Self {
        Self { set, style }
    }

    /// Create a single-line border with default style.
    #[must_use]
    pub const fn single() -> Self {
        Self::new(BorderSet::SINGLE, Style::new())
    }

    /// Create a rounded border with default style.
    #[must_use]
    pub const fn rounded() -> Self {
        Self::new(BorderSet::ROUNDED, Style::new())
    }

    /// Create an ASCII-safe border with default style.
    #[must_use]
    pub const fn ascii() -> Self {
        Self::new(BorderSet::ASCII, Style::new())
    }

    /// Set border style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// A rectangular panel with optional border, title, padding, and background.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Panel {
    border: Option<Border>,
    title: Option<Line>,
    padding: Insets,
    background: Option<Style>,
}

impl Panel {
    /// Create an empty panel.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            border: None,
            title: None,
            padding: Insets::new(0, 0, 0, 0),
            background: None,
        }
    }

    /// Set the panel border.
    #[must_use]
    pub const fn border(mut self, border: Border) -> Self {
        self.border = Some(border);
        self
    }

    /// Set the panel title.
    #[must_use]
    pub fn title(mut self, title: impl Into<Line>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the panel padding.
    #[must_use]
    pub const fn padding(mut self, padding: Insets) -> Self {
        self.padding = padding;
        self
    }

    /// Set the background style for the full panel area.
    #[must_use]
    pub const fn background(mut self, style: Style) -> Self {
        self.background = Some(style);
        self
    }

    /// Return the content area after border and padding are applied.
    #[must_use]
    pub const fn inner_area(&self, area: Rect) -> Rect {
        let border_insets = if self.border.is_some() {
            Insets::all(1)
        } else {
            Insets::all(0)
        };
        area.inset(border_insets).inset(self.padding)
    }
}

impl Widget for Panel {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if let Some(style) = self.background {
            frame.fill(area, " ", style);
        }
        if let Some(border) = &self.border {
            render_border(area, border, frame);
            if let Some(title) = &self.title {
                render_title(area, title, border.style, frame);
            }
        }
    }
}

fn render_border(area: Rect, border: &Border, frame: &mut Frame<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let right = area.right().saturating_sub(1);
    let bottom = area.bottom().saturating_sub(1);

    if area.height == 1 {
        for x in area.x..area.right() {
            frame.buffer_mut().set_cell(
                crate::geometry::Point::new(x, area.y),
                border.set.horizontal.to_string(),
                border.style,
            );
        }
        return;
    }

    if area.width == 1 {
        for y in area.y..area.bottom() {
            frame.buffer_mut().set_cell(
                crate::geometry::Point::new(area.x, y),
                border.set.vertical.to_string(),
                border.style,
            );
        }
        return;
    }

    frame.buffer_mut().set_cell(
        crate::geometry::Point::new(area.x, area.y),
        border.set.top_left.to_string(),
        border.style,
    );
    frame.buffer_mut().set_cell(
        crate::geometry::Point::new(right, area.y),
        border.set.top_right.to_string(),
        border.style,
    );
    frame.buffer_mut().set_cell(
        crate::geometry::Point::new(area.x, bottom),
        border.set.bottom_left.to_string(),
        border.style,
    );
    frame.buffer_mut().set_cell(
        crate::geometry::Point::new(right, bottom),
        border.set.bottom_right.to_string(),
        border.style,
    );

    for x in area.x.saturating_add(1)..right {
        frame.buffer_mut().set_cell(
            crate::geometry::Point::new(x, area.y),
            border.set.horizontal.to_string(),
            border.style,
        );
        frame.buffer_mut().set_cell(
            crate::geometry::Point::new(x, bottom),
            border.set.horizontal.to_string(),
            border.style,
        );
    }

    for y in area.y.saturating_add(1)..bottom {
        frame.buffer_mut().set_cell(
            crate::geometry::Point::new(area.x, y),
            border.set.vertical.to_string(),
            border.style,
        );
        frame.buffer_mut().set_cell(
            crate::geometry::Point::new(right, y),
            border.set.vertical.to_string(),
            border.style,
        );
    }
}

fn render_title(area: Rect, title: &Line, style: Style, frame: &mut Frame<'_>) {
    if area.width <= 2 || area.height == 0 {
        return;
    }
    let title_area = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        1,
    );
    let styled_title = line_with_fallback_style(title, style);
    frame.write_line(title_area, &styled_title);
}

fn line_with_fallback_style(line: &Line, style: Style) -> Line {
    Line::from_spans(
        line.spans
            .iter()
            .map(|span| crate::text::Span::styled(span.content.clone(), style.patch(span.style)))
            .collect::<Vec<_>>(),
    )
}

/// A rendered text input projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInputProjection {
    /// Soft-wrapped visible lines.
    pub lines: Vec<String>,
    /// Cursor location in widget-local coordinates.
    pub cursor: VisualCursor,
}

/// A multiline text input widget backed by [`TextEditBuffer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput<'buffer> {
    buffer: &'buffer TextEditBuffer,
    style: Style,
    selection_style: Style,
    placeholder: Option<Line>,
    placeholder_style: Style,
    cursor_visible: bool,
    vertical_scroll: usize,
}

impl<'buffer> TextInput<'buffer> {
    /// Create a text input from an edit buffer.
    #[must_use]
    pub const fn new(buffer: &'buffer TextEditBuffer) -> Self {
        Self {
            buffer,
            style: Style::new(),
            selection_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
            placeholder: None,
            placeholder_style: Style::new(),
            cursor_visible: true,
            vertical_scroll: 0,
        }
    }

    /// Set rendered text style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set selected text style. This style patches over the base text style.
    #[must_use]
    pub const fn selection_style(mut self, style: Style) -> Self {
        self.selection_style = style;
        self
    }

    /// Set placeholder text.
    #[must_use]
    pub fn placeholder(mut self, placeholder: impl Into<Line>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }

    /// Set placeholder style.
    #[must_use]
    pub const fn placeholder_style(mut self, style: Style) -> Self {
        self.placeholder_style = style;
        self
    }

    /// Set cursor visibility.
    #[must_use]
    pub const fn cursor_visible(mut self, visible: bool) -> Self {
        self.cursor_visible = visible;
        self
    }

    /// Set vertical scroll offset in wrapped rows.
    #[must_use]
    pub const fn vertical_scroll(mut self, rows: usize) -> Self {
        self.vertical_scroll = rows;
        self
    }

    /// Project the buffer into visible wrapped lines for an area.
    #[must_use]
    pub fn project(&self, area: Rect) -> TextInputProjection {
        let width = usize::from(area.width.max(1));
        let layout = self.buffer.wrapped_layout(width);
        let lines = layout
            .lines
            .iter()
            .skip(self.vertical_scroll)
            .take(usize::from(area.height))
            .cloned()
            .collect();
        TextInputProjection {
            lines,
            cursor: VisualCursor {
                row: layout.cursor.row.saturating_sub(self.vertical_scroll),
                col: layout.cursor.col,
            },
        }
    }
}

impl Widget for TextInput<'_> {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }

        if self.buffer.is_empty() {
            if let Some(placeholder) = &self.placeholder {
                let styled = line_with_fallback_style(placeholder, self.placeholder_style);
                frame.write_line(Rect::new(area.x, area.y, area.width, 1), &styled);
            }
            if self.cursor_visible {
                frame.set_cursor(crate::frame::Cursor::visible(crate::geometry::Point::new(
                    area.x, area.y,
                )));
            }
            return;
        }

        let projection = self.project(area);
        let rendered_lines = selected_wrapped_lines(
            self.buffer.text(),
            usize::from(area.width.max(1)),
            self.buffer.selection(),
            self.style,
            self.selection_style,
        );
        for (row, line) in rendered_lines
            .into_iter()
            .skip(self.vertical_scroll)
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &line,
            );
        }

        if self.cursor_visible && projection.cursor.row < usize::from(area.height) {
            let cursor_col = u16::try_from(projection.cursor.col)
                .unwrap_or(u16::MAX)
                .min(area.width);
            let cursor_row = u16::try_from(projection.cursor.row).unwrap_or(u16::MAX);
            frame.set_cursor(crate::frame::Cursor::visible(crate::geometry::Point::new(
                area.x.saturating_add(cursor_col),
                area.y.saturating_add(cursor_row),
            )));
        }
    }
}

fn selected_wrapped_lines(
    text: &str,
    width: usize,
    selection: Option<TextSelection>,
    base_style: Style,
    selection_style: Style,
) -> Vec<Line> {
    let width = width.max(1);
    let mut lines = vec![Line::new()];
    let mut row = 0usize;
    let mut col = 0usize;

    for (start, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            lines.push(Line::new());
            row = row.saturating_add(1);
            col = 0;
            continue;
        }

        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if col > 0 && col.saturating_add(grapheme_width) > width {
            lines.push(Line::new());
            row = row.saturating_add(1);
            col = 0;
        }

        let style = if selection_contains(selection, start) {
            base_style.patch(selection_style)
        } else {
            base_style
        };
        push_styled_grapheme(&mut lines[row], grapheme, style);
        col = col.saturating_add(grapheme_width);
    }

    lines
}

fn selection_contains(selection: Option<TextSelection>, byte_index: usize) -> bool {
    selection.is_some_and(|selection| byte_index >= selection.start && byte_index < selection.end)
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
    use super::{Alignment, Border, Panel, TextBlock, TextInput, TextWrap};
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::{Insets, Rect};
    use crate::style::{Color, Modifier, Style};
    use crate::text::{Line, Text};
    use crate::widget::Widget;
    use bmux_text_edit::TextEditBuffer;

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
    fn text_input_renders_placeholder_and_cursor_for_empty_buffer() {
        let edit = TextEditBuffer::new();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        TextInput::new(&edit)
            .placeholder("Ask")
            .render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("Ask     "));
        assert_eq!(
            frame.cursor(),
            Some(crate::frame::Cursor::visible(crate::geometry::Point::new(
                0, 0
            )))
        );
    }

    #[test]
    fn text_input_renders_wrapped_text_and_cursor() {
        let edit = TextEditBuffer::from_text("hello world");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let mut frame = Frame::new(&mut buffer);

        TextInput::new(&edit).render(Rect::new(0, 0, 5, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("hello"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some(" worl"));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("d    "));
        assert_eq!(
            frame.cursor(),
            Some(crate::frame::Cursor::visible(crate::geometry::Point::new(
                1, 2
            )))
        );
    }

    #[test]
    fn text_input_supports_vertical_scroll() {
        let edit = TextEditBuffer::from_text("abcdef");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        TextInput::new(&edit)
            .vertical_scroll(1)
            .render(Rect::new(0, 0, 3, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("def"));
    }

    #[test]
    fn panel_reports_inner_area() {
        let panel = Panel::new()
            .border(Border::single())
            .padding(Insets::new(1, 2, 3, 4));

        assert_eq!(
            panel.inner_area(Rect::new(0, 0, 20, 10)),
            Rect::new(5, 2, 12, 4)
        );
    }

    #[test]
    fn panel_renders_single_border() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let mut frame = Frame::new(&mut buffer);

        Panel::new()
            .border(Border::single())
            .render(Rect::new(0, 0, 5, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("┌───┐"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("│   │"));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("└───┘"));
    }

    #[test]
    fn panel_renders_title_over_top_border() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        let mut frame = Frame::new(&mut buffer);

        Panel::new()
            .border(Border::ascii())
            .title("Title")
            .render(Rect::new(0, 0, 8, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("+Title-+"));
    }

    #[test]
    fn panel_background_fills_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        let mut frame = Frame::new(&mut buffer);
        let style = Style::new().bg(Color::Blue);

        Panel::new()
            .background(style)
            .render(Rect::new(0, 0, 4, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("    "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(style)
        );
    }

    #[test]
    fn text_input_styles_selection() {
        let mut edit = TextEditBuffer::from_text("hello");
        edit.move_cursor(bmux_text_edit::TextMotion::Start);
        edit.move_cursor_with_selection(
            bmux_text_edit::TextMotion::Right,
            bmux_text_edit::SelectionMode::Extend,
        );
        edit.move_cursor_with_selection(
            bmux_text_edit::TextMotion::Right,
            bmux_text_edit::SelectionMode::Extend,
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let mut frame = Frame::new(&mut buffer);
        let selection_style = Style::new().add_modifier(Modifier::REVERSED);

        TextInput::new(&edit)
            .selection_style(selection_style)
            .render(Rect::new(0, 0, 5, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("hello"));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(selection_style)
        );
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(1, 0))
                .map(|cell| cell.style),
            Some(selection_style)
        );
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(2, 0))
                .map(|cell| cell.style),
            Some(Style::new())
        );
    }

    #[test]
    fn text_input_selection_can_span_wrapped_lines() {
        let mut edit = TextEditBuffer::from_text("abcd");
        edit.move_cursor(bmux_text_edit::TextMotion::Start);
        edit.move_cursor_with_selection(
            bmux_text_edit::TextMotion::End,
            bmux_text_edit::SelectionMode::Extend,
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);
        let selection_style = Style::new().add_modifier(Modifier::REVERSED);

        TextInput::new(&edit)
            .selection_style(selection_style)
            .render(Rect::new(0, 0, 2, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("ab"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("cd"));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 1))
                .map(|cell| cell.style),
            Some(selection_style)
        );
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
    fn text_block_honors_center_alignment() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        TextBlock::new("hi")
            .alignment(Alignment::Center)
            .render(Rect::new(0, 0, 6, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("  hi  "));
    }
}
