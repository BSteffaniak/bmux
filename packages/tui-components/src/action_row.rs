//! Reusable action-button row component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

/// One action button in an action row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionButton {
    /// Stable action id chosen by the caller.
    pub id: String,
    /// Visible button label.
    pub label: String,
}

impl ActionButton {
    /// Create an action button.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// Visual styles for an action row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRowStyles {
    /// Button style when not focused.
    pub button: Style,
    /// Button style when focused.
    pub focused_button: Style,
}

impl Default for ActionRowStyles {
    fn default() -> Self {
        Self {
            button: Style::new(),
            focused_button: Style::new().add_modifier(Modifier::REVERSED),
        }
    }
}

/// Horizontal action-button row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRow<'a> {
    actions: &'a [ActionButton],
    focused: usize,
    spacing: u16,
    styles: ActionRowStyles,
}

impl<'a> ActionRow<'a> {
    /// Create an action row.
    #[must_use]
    pub fn new(actions: &'a [ActionButton]) -> Self {
        Self {
            actions,
            focused: 0,
            spacing: 1,
            styles: ActionRowStyles::default(),
        }
    }

    /// Set focused action index.
    #[must_use]
    pub const fn focused(mut self, focused: usize) -> Self {
        self.focused = focused;
        self
    }

    /// Set horizontal spacing between buttons.
    #[must_use]
    pub const fn spacing(mut self, spacing: u16) -> Self {
        self.spacing = spacing;
        self
    }

    /// Set row styles.
    #[must_use]
    pub const fn styles(mut self, styles: ActionRowStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return button hit boxes for this row in `area`.
    #[must_use]
    pub fn action_areas(&self, area: Rect) -> Vec<Rect> {
        let mut x = area.x;
        let mut areas = Vec::with_capacity(self.actions.len());
        for action in self.actions {
            if x >= area.right() {
                break;
            }
            let width = action_width(action).min(area.right().saturating_sub(x));
            areas.push(Rect::new(x, area.y, width, area.height.min(1)));
            x = x.saturating_add(width).saturating_add(self.spacing);
        }
        areas
    }

    /// Render the action row.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        self.render_actions(area, frame, None);
    }

    /// Render the action row with a fallback style filling each button area.
    pub fn render_with_fallback_style(&self, area: Rect, frame: &mut Frame<'_>, style: Style) {
        self.render_actions(area, frame, Some(style));
    }

    fn render_actions(&self, area: Rect, frame: &mut Frame<'_>, fallback: Option<Style>) {
        for (index, action_area) in self.action_areas(area).into_iter().enumerate() {
            let Some(action) = self.actions.get(index) else {
                return;
            };
            let style = if index == self.focused {
                self.styles.focused_button
            } else {
                self.styles.button
            };
            let line = Line::from_spans(vec![Span::styled(format!("[ {} ]", action.label), style)]);
            if let Some(fallback) = fallback {
                frame.write_line_with_fallback_style(action_area, &line, fallback);
            } else {
                frame.write_line(action_area, &line);
            }
        }
    }
}

fn action_width(action: &ActionButton) -> u16 {
    u16::try_from(bmux_tui::text_width::display_width(&action.label))
        .unwrap_or(u16::MAX)
        .saturating_add(4)
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;

    use super::{ActionButton, ActionRow};

    #[test]
    fn action_areas_follow_rendered_button_widths() {
        let actions = [
            ActionButton::new("approve", "Approve"),
            ActionButton::new("deny", "Deny"),
        ];
        let row = ActionRow::new(&actions).spacing(2);

        let areas = row.action_areas(Rect::new(3, 4, 30, 1));

        assert_eq!(areas, vec![Rect::new(3, 4, 11, 1), Rect::new(16, 4, 8, 1)]);
    }

    #[test]
    fn renders_buttons() {
        let actions = [ActionButton::new("approve", "Approve")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut frame = Frame::new(&mut buffer);

        ActionRow::new(&actions).render(Rect::new(0, 0, 12, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("[ Approve ] ")
        );
    }
}
