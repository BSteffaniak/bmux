//! Built-in neutral widgets.

use bmux_text_edit::TextEditBuffer;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::chrome::{Border, Panel, line_with_fallback_style};
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::input::TextInput;
use crate::layout::{Direction, split_leading, split_trailing};
use crate::list::{List, ListItem, ListState};
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

/// A simple button widget.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    label: Line,
    style: Style,
    focused_style: Style,
    focused: bool,
}

impl Button {
    /// Create a button with a label.
    #[must_use]
    pub fn new(label: impl Into<Line>) -> Self {
        Self {
            label: label.into(),
            style: Style::new(),
            focused_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
            focused: false,
        }
    }

    /// Set base style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set focused style.
    #[must_use]
    pub const fn focused_style(mut self, style: Style) -> Self {
        self.focused_style = style;
        self
    }

    /// Set focused state.
    #[must_use]
    pub const fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }
}

impl Widget for Button {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        let style = if self.focused {
            self.style.patch(self.focused_style)
        } else {
            self.style
        };
        let line = Line::from_spans(vec![
            crate::text::Span::styled("[ ", style),
            line_with_fallback_style(&self.label, style)
                .spans
                .into_iter()
                .next()
                .unwrap_or_else(|| crate::text::Span::styled(String::new(), style)),
            crate::text::Span::styled(" ]", style),
        ]);
        frame.write_line(area, &line);
    }
}

/// A dialog action button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogAction {
    /// Stable action id chosen by the caller.
    pub id: String,
    /// Action label.
    pub label: Line,
}

impl DialogAction {
    /// Create a dialog action.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<Line>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// Selection state for dialog actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DialogState {
    /// Focused action index.
    pub focused_action: usize,
}

impl DialogState {
    /// Move focus to the next action.
    pub const fn focus_next(&mut self, action_count: usize) {
        if action_count == 0 {
            self.focused_action = 0;
        } else {
            self.focused_action = self.focused_action.saturating_add(1) % action_count;
        }
    }

    /// Move focus to the previous action.
    pub const fn focus_previous(&mut self, action_count: usize) {
        if action_count == 0 {
            self.focused_action = 0;
        } else if self.focused_action == 0 {
            self.focused_action = action_count.saturating_sub(1);
        } else {
            self.focused_action = self.focused_action.saturating_sub(1);
        }
    }
}

/// A generic modal-style dialog with body text and action buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog<'a> {
    panel: Panel,
    body: TextBlock,
    actions: &'a [DialogAction],
    button_style: Style,
    focused_button_style: Style,
}

impl<'a> Dialog<'a> {
    /// Create a dialog from body text and actions.
    #[must_use]
    pub fn new(body: impl Into<Text>, actions: &'a [DialogAction]) -> Self {
        Self {
            panel: Panel::new().border(Border::single()),
            body: TextBlock::new(body.into()).wrap(TextWrap::Character),
            actions,
            button_style: Style::new(),
            focused_button_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
        }
    }

    /// Set panel chrome.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = panel;
        self
    }

    /// Set button style.
    #[must_use]
    pub const fn button_style(mut self, style: Style) -> Self {
        self.button_style = style;
        self
    }

    /// Set focused button style.
    #[must_use]
    pub const fn focused_button_style(mut self, style: Style) -> Self {
        self.focused_button_style = style;
        self
    }

    /// Return the panel inner area.
    #[must_use]
    pub const fn content_area(&self, area: Rect) -> Rect {
        self.panel.inner_area(area)
    }
}

impl crate::widget::StatefulWidget for Dialog<'_> {
    type State = DialogState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        self.panel.render(area, frame);
        let inner = self.content_area(area);
        let action_height = u16::from(!self.actions.is_empty());
        let split = split_trailing(inner, Direction::Vertical, action_height);
        self.body.render(split.first, frame);
        render_dialog_actions(
            self.actions,
            state,
            split.second,
            frame,
            self.button_style,
            self.focused_button_style,
        );
    }
}

fn render_dialog_actions(
    actions: &[DialogAction],
    state: &mut DialogState,
    area: Rect,
    frame: &mut Frame<'_>,
    button_style: Style,
    focused_button_style: Style,
) {
    if actions.is_empty() || area.is_empty() {
        return;
    }
    state.focused_action = state.focused_action.min(actions.len().saturating_sub(1));
    let mut x = area.x;
    for (index, action) in actions.iter().enumerate() {
        if x >= area.right() {
            return;
        }
        let width = u16::try_from(unicode_width::UnicodeWidthStr::width(
            action.label.plain_text().as_str(),
        ))
        .unwrap_or(u16::MAX)
        .saturating_add(4)
        .min(area.right().saturating_sub(x));
        let button = Button::new(action.label.clone())
            .style(button_style)
            .focused_style(focused_button_style)
            .focused(index == state.focused_action);
        button.render(Rect::new(x, area.y, width, 1), frame);
        x = x.saturating_add(width).saturating_add(1);
    }
}

/// A lightweight dropdown/list popup widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropdown<'items> {
    list: List<'items>,
    panel: Option<Panel>,
    max_height: Option<u16>,
}

impl<'items> Dropdown<'items> {
    /// Create a dropdown from list items.
    #[must_use]
    pub const fn new(items: &'items [ListItem]) -> Self {
        Self {
            list: List::new(items),
            panel: None,
            max_height: None,
        }
    }

    /// Set the list widget used by this dropdown.
    #[must_use]
    pub fn list(mut self, list: List<'items>) -> Self {
        self.list = list;
        self
    }

    /// Add panel chrome around the dropdown.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = Some(panel);
        self
    }

    /// Set maximum rendered height.
    #[must_use]
    pub const fn max_height(mut self, height: u16) -> Self {
        self.max_height = Some(height);
        self
    }

    /// Return the content area inside optional panel chrome and max-height limit.
    #[must_use]
    pub fn content_area(&self, area: Rect) -> Rect {
        let limited = self.max_height.map_or(area, |max_height| {
            Rect::new(area.x, area.y, area.width, min_u16(area.height, max_height))
        });
        self.panel
            .as_ref()
            .map_or(limited, |panel| panel.inner_area(limited))
    }
}

impl crate::widget::StatefulWidget for Dropdown<'_> {
    type State = ListState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        let area = self.max_height.map_or(area, |max_height| {
            Rect::new(area.x, area.y, area.width, min_u16(area.height, max_height))
        });
        if let Some(panel) = &self.panel {
            panel.render(area, frame);
        }
        self.list.render(self.content_area(area), frame, state);
    }
}

const fn min_u16(a: u16, b: u16) -> u16 {
    if a < b { a } else { b }
}

/// A command-palette-style list picker composed from a panel, text input, and list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPicker<'a> {
    input: TextInput<'a>,
    items: &'a [ListItem],
    panel: Panel,
    input_height: u16,
    gap: u16,
    list: List<'a>,
}

impl<'a> ListPicker<'a> {
    /// Create a list picker from an input buffer and list items.
    #[must_use]
    pub const fn new(input: &'a TextEditBuffer, items: &'a [ListItem]) -> Self {
        Self {
            input: TextInput::new(input),
            items,
            panel: Panel::new().border(Border::single()),
            input_height: 1,
            gap: 1,
            list: List::new(items),
        }
    }

    /// Set the panel chrome.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = panel;
        self
    }

    /// Set the text input widget.
    #[must_use]
    pub fn input(mut self, input: TextInput<'a>) -> Self {
        self.input = input;
        self
    }

    /// Set the list widget.
    #[must_use]
    pub fn list(mut self, list: List<'a>) -> Self {
        self.list = list;
        self
    }

    /// Set the input area height.
    #[must_use]
    pub const fn input_height(mut self, height: u16) -> Self {
        self.input_height = height;
        self
    }

    /// Set the gap between input and list.
    #[must_use]
    pub const fn gap(mut self, rows: u16) -> Self {
        self.gap = rows;
        self
    }

    /// Return the input and list areas inside the picker.
    #[must_use]
    pub const fn content_areas(&self, area: Rect) -> ListPickerAreas {
        let inner = self.panel.inner_area(area);
        let input_split = split_leading(inner, Direction::Vertical, self.input_height);
        let list_split = split_leading(input_split.second, Direction::Vertical, self.gap);
        ListPickerAreas {
            input: input_split.first,
            list: list_split.second,
        }
    }
}

impl crate::widget::StatefulWidget for ListPicker<'_> {
    type State = ListState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        self.panel.render(area, frame);
        let areas = self.content_areas(area);
        self.input.render(areas.input, frame);
        self.list.render(areas.list, frame, state);
    }
}

/// Content areas computed by [`ListPicker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListPickerAreas {
    /// Text input area.
    pub input: Rect,
    /// List area.
    pub list: Rect,
}

impl ListPicker<'_> {
    /// Return picker item count.
    #[must_use]
    pub const fn item_count(&self) -> usize {
        self.items.len()
    }
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

fn push_styled_grapheme(line: &mut Line, grapheme: &str, style: Style) {
    if let Some(last) = line.spans.last_mut()
        && last.style == style
    {
        last.content.push_str(grapheme);
        return;
    }
    line.push_span(crate::text::Span::styled(grapheme.to_owned(), style));
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
        Alignment, Button, Dialog, DialogAction, DialogState, Dropdown, ListPicker, TextBlock,
        TextWrap,
    };
    use crate::buffer::Buffer;
    use crate::chrome::{Border, Modal, Panel};
    use crate::frame::Frame;
    use crate::geometry::{Insets, Rect, Size};
    use crate::input::{
        TextInput, TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome,
    };
    use crate::list::{List, ListItem, ListKeyHandler, ListKeyOutcome, ListState};
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
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("world"));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("     "));
        assert_eq!(
            frame.cursor(),
            Some(crate::frame::Cursor::visible(crate::geometry::Point::new(
                5, 1
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
    fn list_registers_visible_row_hit_regions() {
        let items = vec![
            ListItem::new("one"),
            ListItem::new("two"),
            ListItem::new("three"),
            ListItem::new("four"),
        ];
        let list = List::new(&items);
        let state = ListState {
            selected: Some(2),
            offset: 1,
        };
        let mut hits = crate::hit::HitMap::new();

        list.register_hits(Rect::new(5, 2, 10, 2), &state, &mut hits, "files");

        assert_eq!(hits.regions().len(), 2);
        let hit = hits
            .hit_test(crate::geometry::Point::new(6, 3))
            .expect("second visible row should be hittable");
        assert_eq!(hit.id().as_str(), "files:2");
        assert_eq!(hit.role(), crate::hit::HitRole::ListItem);
        assert_eq!(List::hit_item_index(hit.id(), "files"), Some(2));
    }

    #[test]
    fn list_hit_item_index_rejects_other_prefixes() {
        assert_eq!(
            List::hit_item_index(&crate::hit::HitId::new("other:7"), "files"),
            None
        );
        assert_eq!(
            List::hit_item_index(&crate::hit::HitId::new("files:not-number"), "files"),
            None
        );
    }

    #[test]
    fn list_key_handler_moves_and_pages_selection() {
        let mut state = ListState::new();
        let handler = ListKeyHandler;

        assert_eq!(
            handler.handle_key(&mut state, 10, 3, KeyStroke::simple(KeyCode::Down)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(0));
        assert_eq!(
            handler.handle_key(&mut state, 10, 3, KeyStroke::simple(KeyCode::PageDown)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(3));
        assert_eq!(state.offset, 1);
        assert_eq!(
            handler.handle_key(&mut state, 10, 3, KeyStroke::simple(KeyCode::PageUp)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.offset, 0);
    }

    #[test]
    fn list_key_handler_supports_home_end_activate_and_cancel() {
        let mut state = ListState {
            selected: Some(1),
            offset: 0,
        };
        let handler = ListKeyHandler;

        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::End)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(3));
        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::Home)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(0));
        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::Enter)),
            ListKeyOutcome::Activated
        );
        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::Escape)),
            ListKeyOutcome::Canceled
        );
    }

    #[test]
    fn list_key_handler_ignores_modified_and_unmapped_keys() {
        let mut state = ListState::new();
        let handler = ListKeyHandler;

        assert_eq!(
            handler.handle_key(
                &mut state,
                4,
                2,
                KeyStroke::with_modifiers(
                    KeyCode::Down,
                    Modifiers {
                        shift: true,
                        ..Modifiers::NONE
                    },
                ),
            ),
            ListKeyOutcome::Ignored
        );
        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::Char('x'))),
            ListKeyOutcome::Ignored
        );
    }

    #[test]
    fn button_renders_focus_style() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);
        let focus = Style::new().bg(Color::Blue);

        Button::new("Run")
            .focused_style(focus)
            .focused(true)
            .render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("[ Run ] "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(focus)
        );
    }

    #[test]
    fn dialog_renders_body_and_actions() {
        let actions = vec![
            DialogAction::new("allow", "Allow"),
            DialogAction::new("deny", "Deny"),
        ];
        let mut state = DialogState { focused_action: 1 };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 5));
        let mut frame = Frame::new(&mut buffer);

        Dialog::new("Permit action?", &actions)
            .panel(Panel::new().border(Border::ascii()).title("Permission"))
            .render(Rect::new(0, 0, 20, 5), &mut frame, &mut state);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("+Permission--------+")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("|Permit action?    |")
        );
        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("|[ Allow ] [ Deny ]|")
        );
        assert_eq!(state.focused_action, 1);
    }

    #[test]
    fn dialog_state_cycles_actions() {
        let mut state = DialogState::default();

        state.focus_next(2);
        assert_eq!(state.focused_action, 1);
        state.focus_next(2);
        assert_eq!(state.focused_action, 0);
        state.focus_previous(2);
        assert_eq!(state.focused_action, 1);
    }

    #[test]
    fn dropdown_renders_list_with_panel_and_height_limit() {
        let items = vec![
            ListItem::new("one"),
            ListItem::new("two"),
            ListItem::new("three"),
        ];
        let mut state = ListState {
            selected: Some(1),
            offset: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut frame = Frame::new(&mut buffer);

        Dropdown::new(&items)
            .panel(Panel::new().border(Border::ascii()))
            .max_height(3)
            .render(Rect::new(0, 0, 8, 4), &mut frame, &mut state);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("+------+"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("|two   |"));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("+------+"));
        assert_eq!(state.offset, 1);
    }

    #[test]
    fn dropdown_content_area_accounts_for_panel_and_limit() {
        let items = vec![ListItem::new("one")];
        let dropdown = Dropdown::new(&items)
            .panel(Panel::new().border(Border::single()))
            .max_height(5);

        assert_eq!(
            dropdown.content_area(Rect::new(2, 3, 10, 8)),
            Rect::new(3, 4, 8, 3)
        );
    }

    #[test]
    fn list_picker_computes_content_areas() {
        let input = TextEditBuffer::new();
        let items = vec![ListItem::new("one")];
        let picker = ListPicker::new(&input, &items)
            .panel(Panel::new().border(Border::ascii()))
            .input_height(2)
            .gap(1);

        assert_eq!(picker.item_count(), 1);
        assert_eq!(
            picker.content_areas(Rect::new(0, 0, 10, 6)),
            super::ListPickerAreas {
                input: Rect::new(1, 1, 8, 2),
                list: Rect::new(1, 4, 8, 1),
            }
        );
    }

    #[test]
    fn list_picker_renders_panel_input_and_list() {
        let input = TextEditBuffer::from_text("f");
        let items = vec![ListItem::new("foo"), ListItem::new("bar")];
        let mut state = ListState {
            selected: Some(1),
            offset: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 5));
        let mut frame = Frame::new(&mut buffer);

        ListPicker::new(&input, &items).render(Rect::new(0, 0, 8, 5), &mut frame, &mut state);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("┌──────┐"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("│f     │"));
        assert_eq!(frame.buffer().row_symbols(3).as_deref(), Some("│bar   │"));
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
