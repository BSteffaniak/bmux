//! Button and dialog widgets.

use crate::chrome::{Border, Panel};
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::layout::{Direction, split_trailing};
use crate::style::Style;
use crate::text::{Line, Text};
use crate::text_block::{TextBlock, TextWrap};
use crate::widget::Widget;

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
            self.label
                .with_fallback_style(style)
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

#[cfg(test)]
mod tests {
    use super::{Button, Dialog, DialogAction, DialogState};
    use crate::buffer::Buffer;
    use crate::chrome::{Border, Panel};
    use crate::frame::Frame;
    use crate::geometry::Rect;
    use crate::style::{Color, Style};
    use crate::widget::{StatefulWidget, Widget};

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
}
