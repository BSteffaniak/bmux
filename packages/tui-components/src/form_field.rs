//! Reusable labeled form-field component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Styles for a [`FormField`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormFieldStyles {
    /// Label style.
    pub label: Style,
    /// Required marker style.
    pub required_marker: Style,
    /// Help text style.
    pub help: Style,
    /// Error text style.
    pub error: Style,
}

impl Default for FormFieldStyles {
    fn default() -> Self {
        Self {
            label: Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
            required_marker: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            help: Style::new().fg(Color::BrightBlack),
            error: Style::new().fg(Color::Red),
        }
    }
}

/// Labeled form-field layout with optional required marker, help text, and error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormField<'a> {
    label: &'a str,
    required: bool,
    help: Option<&'a str>,
    error: Option<&'a str>,
    styles: FormFieldStyles,
}

impl<'a> FormField<'a> {
    /// Create a labeled form field.
    #[must_use]
    pub const fn new(label: &'a str) -> Self {
        Self {
            label,
            required: false,
            help: None,
            error: None,
            styles: FormFieldStyles {
                label: Style::new(),
                required_marker: Style::new(),
                help: Style::new(),
                error: Style::new(),
            },
        }
    }

    /// Mark this field as required.
    #[must_use]
    pub const fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Set optional help text.
    #[must_use]
    pub const fn help(mut self, help: &'a str) -> Self {
        self.help = Some(help);
        self
    }

    /// Set optional validation error text.
    #[must_use]
    pub const fn error(mut self, error: &'a str) -> Self {
        self.error = Some(error);
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: FormFieldStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Compute label area and control area within `area`.
    #[must_use]
    pub fn layout(&self, area: Rect) -> FormFieldLayout {
        if area.is_empty() {
            return FormFieldLayout {
                label: Rect::new(area.x, area.y, area.width, 0),
                control: Rect::new(area.x, area.y, area.width, 0),
                help: None,
                error: None,
            };
        }

        let mut y = area.y;
        let label = Rect::new(area.x, y, area.width, 1);
        y = y.saturating_add(1);
        let footer_rows = u16::from(self.help.is_some()) + u16::from(self.error.is_some());
        let used_rows = y.saturating_sub(area.y).saturating_add(footer_rows);
        let control_height = area.height.saturating_sub(used_rows);
        let control = Rect::new(area.x, y, area.width, control_height);
        y = y.saturating_add(control_height);
        let help = self.help.and_then(|_| {
            if y < area.bottom() {
                let rect = Rect::new(area.x, y, area.width, 1);
                y = y.saturating_add(1);
                Some(rect)
            } else {
                None
            }
        });
        let error = self.error.and_then(|_| {
            if y < area.bottom() {
                Some(Rect::new(area.x, y, area.width, 1))
            } else {
                None
            }
        });

        FormFieldLayout {
            label,
            control,
            help,
            error,
        }
    }

    /// Render label/help/error chrome and return the area reserved for the field control.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) -> Rect {
        self.render_with_fallback_style(area, frame, None)
    }

    /// Render label/help/error chrome with a fallback style and return the field control area.
    pub fn render_with_fallback_style(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        fallback: Option<Style>,
    ) -> Rect {
        let layout = self.layout(area);
        write_line(layout.label, &self.label_line(), frame, fallback);
        if let (Some(help), Some(text)) = (layout.help, self.help) {
            write_line(
                help,
                &Line::from_spans(vec![Span::styled(text, self.styles.help)]),
                frame,
                fallback,
            );
        }
        if let (Some(error), Some(text)) = (layout.error, self.error) {
            write_line(
                error,
                &Line::from_spans(vec![Span::styled(text, self.styles.error)]),
                frame,
                fallback,
            );
        }
        layout.control
    }

    fn label_line(&self) -> Line {
        let mut spans = vec![Span::styled(self.label, self.styles.label)];
        if self.required {
            spans.push(Span::styled(" *", self.styles.required_marker));
        }
        Line::from_spans(spans)
    }
}

fn write_line(area: Rect, line: &Line, frame: &mut Frame<'_>, fallback: Option<Style>) {
    if area.is_empty() {
        return;
    }
    if let Some(fallback) = fallback {
        frame.write_line_with_fallback_style(area, line, fallback);
    } else {
        frame.write_line(area, line);
    }
}

impl Default for FormField<'_> {
    fn default() -> Self {
        Self::new("").styles(FormFieldStyles::default())
    }
}

/// Areas produced by [`FormField::layout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormFieldLayout {
    /// Label row.
    pub label: Rect,
    /// Area reserved for the nested control.
    pub control: Rect,
    /// Help text row.
    pub help: Option<Rect>,
    /// Error text row.
    pub error: Option<Rect>,
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;

    use super::{FormField, FormFieldStyles};

    #[test]
    fn layout_reserves_label_control_help_and_error_rows() {
        let field = FormField::new("Name").help("Required").error("Missing");

        let layout = field.layout(Rect::new(2, 3, 20, 5));

        assert_eq!(layout.label, Rect::new(2, 3, 20, 1));
        assert_eq!(layout.control, Rect::new(2, 4, 20, 2));
        assert_eq!(layout.help, Some(Rect::new(2, 6, 20, 1)));
        assert_eq!(layout.error, Some(Rect::new(2, 7, 20, 1)));
    }

    #[test]
    fn renders_required_label_and_messages() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
        let mut frame = Frame::new(&mut buffer);

        FormField::new("Name")
            .required(true)
            .help("Enter name")
            .error("Missing")
            .styles(FormFieldStyles::default())
            .render(Rect::new(0, 0, 20, 4), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Name *              ")
        );
        assert_eq!(
            frame.buffer().row_symbols(2).as_deref(),
            Some("Enter name          ")
        );
        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("Missing             ")
        );
    }
}
