//! Built-in neutral widgets.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_text_edit::{TextEditBuffer, TextSelection, VisualCursor};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::frame::Frame;
use crate::geometry::{Insets, Rect, Size};
use crate::layout::centered;
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

/// A centered modal surface with optional scrim and child content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modal<'widget, W> {
    panel: Panel,
    size: Size,
    scrim: Option<Style>,
    child: Option<&'widget W>,
}

impl<'widget, W> Modal<'widget, W> {
    /// Create a modal with the requested maximum size.
    #[must_use]
    pub const fn new(size: Size) -> Self {
        Self {
            panel: Panel::new().border(Border::single()),
            size,
            scrim: None,
            child: None,
        }
    }

    /// Set the panel used for modal chrome.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = panel;
        self
    }

    /// Set an optional full-area scrim style.
    #[must_use]
    pub const fn scrim(mut self, style: Style) -> Self {
        self.scrim = Some(style);
        self
    }

    /// Set modal child content.
    #[must_use]
    pub const fn child(mut self, child: &'widget W) -> Self {
        self.child = Some(child);
        self
    }

    /// Return the modal panel area for a parent area.
    #[must_use]
    pub const fn panel_area(&self, area: Rect) -> Rect {
        centered(area, self.size)
    }

    /// Return the modal content area for a parent area.
    #[must_use]
    pub const fn content_area(&self, area: Rect) -> Rect {
        self.panel.inner_area(self.panel_area(area))
    }
}

impl<W: Widget> Widget for Modal<'_, W> {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if let Some(style) = self.scrim {
            frame.fill(area, " ", style);
        }
        let panel_area = self.panel_area(area);
        self.panel.render(panel_area, frame);
        if let Some(child) = self.child {
            child.render(self.panel.inner_area(panel_area), frame);
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

/// A list item rendered as one styled line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    line: Line,
}

impl ListItem {
    /// Create a list item from a line.
    #[must_use]
    pub fn new(line: impl Into<Line>) -> Self {
        Self { line: line.into() }
    }

    /// Return the rendered line.
    #[must_use]
    pub const fn line(&self) -> &Line {
        &self.line
    }
}

/// Scroll and selection state for [`List`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ListState {
    /// Selected item index, if any.
    pub selected: Option<usize>,
    /// First visible item index.
    pub offset: usize,
}

impl ListState {
    /// Create empty list state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            selected: None,
            offset: 0,
        }
    }

    /// Select an item by index.
    pub const fn select(&mut self, index: Option<usize>) {
        self.selected = index;
    }

    /// Move selection down by one item.
    pub fn select_next(&mut self, item_count: usize) {
        if item_count == 0 {
            self.selected = None;
            self.offset = 0;
            return;
        }
        self.selected = Some(
            self.selected
                .map_or(0, |selected| selected.saturating_add(1).min(item_count - 1)),
        );
    }

    /// Move selection up by one item.
    pub fn select_previous(&mut self, item_count: usize) {
        if item_count == 0 {
            self.selected = None;
            self.offset = 0;
            return;
        }
        self.selected = Some(
            self.selected
                .map_or(0, |selected| selected.saturating_sub(1)),
        );
    }

    /// Adjust offset so the selection is visible in a viewport of `height` rows.
    pub fn ensure_selected_visible(&mut self, height: u16, item_count: usize) {
        if item_count == 0 {
            self.selected = None;
            self.offset = 0;
            return;
        }
        let height = usize::from(height.max(1));
        let selected = self.selected.unwrap_or(0).min(item_count - 1);
        self.selected = Some(selected);
        if selected < self.offset {
            self.offset = selected;
        } else if selected >= self.offset.saturating_add(height) {
            self.offset = selected.saturating_add(1).saturating_sub(height);
        }
        self.offset = self.offset.min(item_count.saturating_sub(1));
    }
}

/// A virtualized single-line list widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List<'items> {
    items: &'items [ListItem],
    selected_style: Style,
    highlight_symbol: Option<String>,
}

impl<'items> List<'items> {
    /// Create a list from items.
    #[must_use]
    pub const fn new(items: &'items [ListItem]) -> Self {
        Self {
            items,
            selected_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
            highlight_symbol: None,
        }
    }

    /// Set selected item style. This style patches over item span styles.
    #[must_use]
    pub const fn selected_style(mut self, style: Style) -> Self {
        self.selected_style = style;
        self
    }

    /// Set an optional highlight symbol rendered before the selected item.
    #[must_use]
    pub fn highlight_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.highlight_symbol = Some(symbol.into());
        self
    }

    /// Return this list's items.
    #[must_use]
    pub const fn items(&self) -> &[ListItem] {
        self.items
    }
}

impl crate::widget::StatefulWidget for List<'_> {
    type State = ListState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        state.ensure_selected_visible(area.height, self.items.len());
        let visible_count = usize::from(area.height);
        for (row, (index, item)) in self
            .items
            .iter()
            .enumerate()
            .skip(state.offset)
            .take(visible_count)
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            let selected = state.selected == Some(index);
            let line = self.render_item_line(item, selected);
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &line,
            );
        }
    }
}

impl List<'_> {
    fn render_item_line(&self, item: &ListItem, selected: bool) -> Line {
        let line = if selected {
            line_with_fallback_style(item.line(), self.selected_style)
        } else {
            item.line().clone()
        };
        if selected && let Some(symbol) = &self.highlight_symbol {
            let mut spans = vec![crate::text::Span::styled(
                symbol.clone(),
                self.selected_style,
            )];
            spans.extend(line.spans);
            return Line::from_spans(spans);
        }
        line
    }
}

/// Enter-key behavior for text input key handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextInputEnterBehavior {
    /// Enter inserts a newline into the edit buffer.
    #[default]
    InsertNewline,
    /// Enter reports submission and leaves the edit buffer unchanged.
    Submit,
}

/// Result of handling a key stroke for a text input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputKeyOutcome {
    /// The key was not recognized as text input.
    Ignored,
    /// The edit buffer changed or cursor moved.
    Edited,
    /// The key requested submission.
    Submitted,
}

/// Key handling policy for [`TextInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextInputKeyHandler {
    /// Standard editor keymap.
    pub keymap: TextKeymap,
    /// Enter-key behavior.
    pub enter_behavior: TextInputEnterBehavior,
}

impl TextInputKeyHandler {
    /// Create a key handler from a keymap and enter behavior.
    #[must_use]
    pub const fn new(keymap: TextKeymap, enter_behavior: TextInputEnterBehavior) -> Self {
        Self {
            keymap,
            enter_behavior,
        }
    }

    /// Apply a key stroke to an edit buffer.
    pub fn handle_key(self, buffer: &mut TextEditBuffer, stroke: KeyStroke) -> TextInputKeyOutcome {
        if stroke.key == KeyCode::Enter && stroke.modifiers.is_empty() {
            return match self.enter_behavior {
                TextInputEnterBehavior::InsertNewline => {
                    buffer.insert_newline();
                    TextInputKeyOutcome::Edited
                }
                TextInputEnterBehavior::Submit => TextInputKeyOutcome::Submitted,
            };
        }

        let Some(command) = self.keymap.command_for_key(stroke) else {
            return TextInputKeyOutcome::Ignored;
        };
        buffer.apply_command(command);
        TextInputKeyOutcome::Edited
    }
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
    use super::{
        Alignment, Border, List, ListItem, ListState, Modal, Panel, TextBlock, TextInput,
        TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome, TextWrap,
    };
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::{Insets, Rect, Size};
    use crate::style::{Color, Modifier, Style};
    use crate::text::{Line, Text};
    use crate::widget::{StatefulWidget, Widget};
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
    use bmux_text_edit::TextEditBuffer;

    #[test]
    fn text_input_key_handler_inserts_characters_and_deletes() {
        let mut edit = TextEditBuffer::new();
        let handler = TextInputKeyHandler::default();

        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Char('a'))),
            TextInputKeyOutcome::Edited
        );
        assert_eq!(edit.text(), "a");
        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Backspace)),
            TextInputKeyOutcome::Edited
        );
        assert_eq!(edit.text(), "");
    }

    #[test]
    fn text_input_key_handler_supports_submit_enter_behavior() {
        let mut edit = TextEditBuffer::from_text("run");
        let handler = TextInputKeyHandler::new(
            bmux_text_edit::keyboard::TextKeymap::default(),
            TextInputEnterBehavior::Submit,
        );

        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Enter)),
            TextInputKeyOutcome::Submitted
        );
        assert_eq!(edit.text(), "run");
    }

    #[test]
    fn text_input_key_handler_inserts_newline_by_default() {
        let mut edit = TextEditBuffer::from_text("a");
        let handler = TextInputKeyHandler::default();

        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Enter)),
            TextInputKeyOutcome::Edited
        );
        assert_eq!(edit.text(), "a\n");
    }

    #[test]
    fn text_input_key_handler_ignores_unknown_keys() {
        let mut edit = TextEditBuffer::new();
        let handler = TextInputKeyHandler::default();

        assert_eq!(
            handler.handle_key(&mut edit, KeyStroke::simple(KeyCode::Escape)),
            TextInputKeyOutcome::Ignored
        );
        assert_eq!(
            handler.handle_key(
                &mut edit,
                KeyStroke::with_modifiers(
                    KeyCode::Char('x'),
                    Modifiers {
                        super_key: true,
                        ..Modifiers::NONE
                    },
                ),
            ),
            TextInputKeyOutcome::Ignored
        );
    }

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
    fn list_renders_visible_window_and_selection() {
        let items = vec![
            ListItem::new("one"),
            ListItem::new("two"),
            ListItem::new("three"),
            ListItem::new("four"),
        ];
        let mut state = ListState {
            selected: Some(2),
            offset: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 7, 2));
        let mut frame = Frame::new(&mut buffer);
        let selected_style = Style::new().add_modifier(Modifier::REVERSED);

        List::new(&items).selected_style(selected_style).render(
            Rect::new(0, 0, 7, 2),
            &mut frame,
            &mut state,
        );

        assert_eq!(state.offset, 1);
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("two    "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("three  "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 1))
                .map(|cell| cell.style),
            Some(selected_style)
        );
    }

    #[test]
    fn list_state_moves_selection_with_bounds() {
        let mut state = ListState::new();

        state.select_next(3);
        assert_eq!(state.selected, Some(0));
        state.select_next(3);
        state.select_next(3);
        state.select_next(3);
        assert_eq!(state.selected, Some(2));
        state.select_previous(3);
        assert_eq!(state.selected, Some(1));
        state.select_previous(0);
        assert_eq!(state.selected, None);
    }

    #[test]
    fn list_renders_highlight_symbol_for_selected_item() {
        let items = vec![ListItem::new("one"), ListItem::new("two")];
        let mut state = ListState {
            selected: Some(0),
            offset: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        List::new(&items).highlight_symbol("> ").render(
            Rect::new(0, 0, 6, 1),
            &mut frame,
            &mut state,
        );

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("> one "));
    }

    #[test]
    fn modal_centers_panel_and_renders_child_in_inner_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 5));
        let mut frame = Frame::new(&mut buffer);
        let child = TextBlock::new("Hi");
        let modal = Modal::new(Size::new(6, 3)).child(&child);

        modal.render(Rect::new(0, 0, 10, 5), &mut frame);

        assert_eq!(
            modal.panel_area(Rect::new(0, 0, 10, 5)),
            Rect::new(2, 1, 6, 3)
        );
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("  ┌────┐  "));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("  │Hi  │  "));
        assert_eq!(frame.buffer().row_symbols(3).as_deref(), Some("  └────┘  "));
    }

    #[test]
    fn modal_scrim_fills_parent_area_before_panel() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let mut frame = Frame::new(&mut buffer);
        let scrim = Style::new().bg(Color::BrightBlack);
        let panel = Panel::new().border(Border::ascii());
        let modal: Modal<'_, TextBlock> = Modal::new(Size::new(3, 3)).panel(panel).scrim(scrim);

        modal.render(Rect::new(0, 0, 5, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(" +-+ "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(scrim)
        );
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
