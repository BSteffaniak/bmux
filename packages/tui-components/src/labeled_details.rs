//! Reusable labeled detail list component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::{display_width, wrap_text_with_continuation};

/// One labeled detail item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailItem {
    /// Field label.
    pub label: String,
    /// Field value.
    pub value: String,
}

impl DetailItem {
    /// Create a detail item.
    #[must_use]
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Styles for a labeled detail list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabeledDetailsStyles {
    /// Label style.
    pub label: Style,
    /// Value style.
    pub value: Style,
    /// Continuation indentation style.
    pub continuation: Style,
}

impl Default for LabeledDetailsStyles {
    fn default() -> Self {
        Self {
            label: Style::new()
                .fg(Color::BrightBlack)
                .add_modifier(Modifier::BOLD),
            value: Style::new().fg(Color::BrightWhite),
            continuation: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Vertical list of labeled, wrapped details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledDetails<'a> {
    items: &'a [DetailItem],
    styles: LabeledDetailsStyles,
    item_spacing: bool,
}

impl<'a> LabeledDetails<'a> {
    /// Create a labeled details component.
    #[must_use]
    pub fn new(items: &'a [DetailItem]) -> Self {
        Self {
            items,
            styles: LabeledDetailsStyles::default(),
            item_spacing: true,
        }
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: LabeledDetailsStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set whether to insert a blank row between detail items.
    #[must_use]
    pub const fn item_spacing(mut self, item_spacing: bool) -> Self {
        self.item_spacing = item_spacing;
        self
    }

    /// Render this detail list to lines for a given width.
    #[must_use]
    pub fn lines(&self, width: u16) -> Vec<Line> {
        let mut rows = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            rows.push(Line::from_spans(vec![Span::styled(
                item.label.clone(),
                self.styles.label,
            )]));
            for line in item.value.lines() {
                push_wrapped_value(&mut rows, line, width, self.styles);
            }
            if self.item_spacing && index + 1 < self.items.len() {
                rows.push(Line::default());
            }
        }
        rows
    }

    /// Render this detail list directly.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        self.render_lines(area, frame, None);
    }

    /// Render this detail list directly with a fallback style for each row.
    pub fn render_with_fallback_style(&self, area: Rect, frame: &mut Frame<'_>, style: Style) {
        self.render_lines(area, frame, Some(style));
    }

    fn render_lines(&self, area: Rect, frame: &mut Frame<'_>, fallback: Option<Style>) {
        if area.is_empty() {
            return;
        }
        for (index, line) in self
            .lines(area.width)
            .iter()
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(offset) = u16::try_from(index) else {
                return;
            };
            let line_area = Rect::new(area.x, area.y.saturating_add(offset), area.width, 1);
            if let Some(fallback) = fallback {
                frame.write_line_with_fallback_style(line_area, line, fallback);
            } else {
                frame.write_line(line_area, line);
            }
        }
    }
}

fn push_wrapped_value(rows: &mut Vec<Line>, text: &str, width: u16, styles: LabeledDetailsStyles) {
    let max_width = usize::from(width.max(1));
    let prefix = "  ";
    let first_width = max_width.saturating_sub(display_width(prefix)).max(1);
    let next_width = first_width;
    for chunk in wrap_text_with_continuation(text, first_width, next_width) {
        rows.push(Line::from_spans(vec![
            Span::styled(prefix, styles.continuation),
            Span::styled(chunk, styles.value),
        ]));
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`LabeledDetailsStyles`].
    #[must_use]
    pub fn labeled_details_styles(self) -> LabeledDetailsStyles {
        LabeledDetailsStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for LabeledDetailsStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            label: theme.muted.add_modifier(bmux_tui::style::Modifier::BOLD),
            value: theme.text,
            continuation: theme.muted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DetailItem, LabeledDetails};

    #[test]
    fn wraps_detail_values() {
        let items = [DetailItem::new("command", "abcdef")];
        let lines = LabeledDetails::new(&items).lines(5);

        assert_eq!(lines[0].plain_text(), "command");
        assert_eq!(lines[1].plain_text(), "  abc");
        assert_eq!(lines[2].plain_text(), "  def");
    }
}
