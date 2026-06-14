//! Generic stepper / multi-step progress indicator component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::{display_width, truncate_to_display_width};

/// Generic step status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepStatus {
    /// Pending step.
    #[default]
    Pending,
    /// Current step.
    Current,
    /// Completed step.
    Complete,
    /// Warning step.
    Warning,
    /// Error step.
    Error,
    /// Disabled step.
    Disabled,
}

/// One step item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepItem<'a> {
    /// Stable step id.
    pub id: &'a str,
    /// Display label.
    pub label: &'a str,
    /// Step status.
    pub status: StepStatus,
}

impl<'a> StepItem<'a> {
    /// Create a pending step item.
    #[must_use]
    pub const fn new(id: &'a str, label: &'a str) -> Self {
        Self {
            id,
            label,
            status: StepStatus::Pending,
        }
    }

    /// Return this step with status.
    #[must_use]
    pub const fn status(mut self, status: StepStatus) -> Self {
        self.status = status;
        self
    }
}

/// Stepper orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepperOrientation {
    /// Render steps left-to-right on one row.
    #[default]
    Horizontal,
    /// Render one step per row.
    Vertical,
}

/// Stepper behavior/layout policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepperPolicy {
    /// Orientation.
    pub orientation: StepperOrientation,
    /// Connector between steps.
    pub connector: &'static str,
    /// Truncate labels to the available area.
    pub truncate: bool,
    /// Show status marker before labels.
    pub markers: bool,
}

impl StepperPolicy {
    /// Horizontal stepper.
    #[must_use]
    pub const fn horizontal() -> Self {
        Self {
            orientation: StepperOrientation::Horizontal,
            connector: "──",
            truncate: true,
            markers: true,
        }
    }

    /// Vertical stepper.
    #[must_use]
    pub const fn vertical() -> Self {
        Self {
            orientation: StepperOrientation::Vertical,
            connector: "│",
            truncate: true,
            markers: true,
        }
    }
}

impl Default for StepperPolicy {
    fn default() -> Self {
        Self::horizontal()
    }
}

/// Stepper visual styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepperStyles {
    /// Pending step style.
    pub pending: Style,
    /// Current step style.
    pub current: Style,
    /// Complete step style.
    pub complete: Style,
    /// Warning step style.
    pub warning: Style,
    /// Error step style.
    pub error: Style,
    /// Disabled step style.
    pub disabled: Style,
    /// Connector style.
    pub connector: Style,
}

impl Default for StepperStyles {
    fn default() -> Self {
        Self {
            pending: Style::new().fg(Color::BrightBlack),
            current: Style::new()
                .fg(Color::BrightCyan)
                .add_modifier(Modifier::BOLD),
            complete: Style::new().fg(Color::BrightGreen),
            warning: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            disabled: Style::new().fg(Color::BrightBlack),
            connector: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Generic stepper component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stepper<'a> {
    steps: &'a [StepItem<'a>],
    policy: StepperPolicy,
    styles: StepperStyles,
}

impl<'a> Stepper<'a> {
    /// Create a stepper over caller-owned steps.
    #[must_use]
    pub const fn new(steps: &'a [StepItem<'a>]) -> Self {
        Self {
            steps,
            policy: StepperPolicy {
                orientation: StepperOrientation::Horizontal,
                connector: "──",
                truncate: true,
                markers: true,
            },
            styles: StepperStyles {
                pending: Style::new(),
                current: Style::new(),
                complete: Style::new(),
                warning: Style::new(),
                error: Style::new(),
                disabled: Style::new(),
                connector: Style::new(),
            },
        }
    }

    /// Set policy.
    #[must_use]
    pub const fn policy(mut self, policy: StepperPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: StepperStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Render stepper.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() || self.steps.is_empty() {
            return;
        }
        match self.policy.orientation {
            StepperOrientation::Horizontal => self.render_horizontal(area, frame),
            StepperOrientation::Vertical => self.render_vertical(area, frame),
        }
    }

    fn render_horizontal(&self, area: Rect, frame: &mut Frame<'_>) {
        let mut spans = Vec::new();
        for (index, step) in self.steps.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(
                    format!(" {} ", self.policy.connector),
                    self.styles.connector,
                ));
            }
            spans.push(Span::styled(
                self.step_text(step),
                self.style_for(step.status),
            ));
        }
        let mut line = Line::from_spans(spans);
        if self.policy.truncate {
            let text = line.plain_text();
            if display_width(&text) > usize::from(area.width) {
                line = Line::from(truncate_to_display_width(&text, usize::from(area.width)));
            }
        }
        frame.write_line(area, &line);
    }

    fn render_vertical(&self, area: Rect, frame: &mut Frame<'_>) {
        for (index, step) in self.steps.iter().take(usize::from(area.height)).enumerate() {
            let Ok(y_offset) = u16::try_from(index) else {
                return;
            };
            let y = area.y.saturating_add(y_offset);
            let prefix = if index > 0 {
                format!("{} ", self.policy.connector)
            } else {
                "  ".to_owned()
            };
            let text = format!("{prefix}{}", self.step_text(step));
            let text = if self.policy.truncate {
                truncate_to_display_width(&text, usize::from(area.width))
            } else {
                text
            };
            frame.write_line(
                Rect::new(area.x, y, area.width, 1),
                &Line::from_spans([Span::styled(text, self.style_for(step.status))]),
            );
        }
    }

    fn step_text(&self, step: &StepItem<'_>) -> String {
        if self.policy.markers {
            format!("{} {}", marker_for(step.status), step.label)
        } else {
            step.label.to_owned()
        }
    }

    const fn style_for(&self, status: StepStatus) -> Style {
        match status {
            StepStatus::Pending => self.styles.pending,
            StepStatus::Current => self.styles.current,
            StepStatus::Complete => self.styles.complete,
            StepStatus::Warning => self.styles.warning,
            StepStatus::Error => self.styles.error,
            StepStatus::Disabled => self.styles.disabled,
        }
    }
}

const fn marker_for(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "○",
        StepStatus::Current => "●",
        StepStatus::Complete => "✓",
        StepStatus::Warning => "!",
        StepStatus::Error => "×",
        StepStatus::Disabled => "-",
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;

    use super::{StepItem, StepStatus, Stepper, StepperOrientation, StepperPolicy};

    #[test]
    fn renders_horizontal_stepper() {
        let steps = [
            StepItem::new("one", "One").status(StepStatus::Complete),
            StepItem::new("two", "Two").status(StepStatus::Current),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 1));
        let mut frame = Frame::new(&mut buffer);

        Stepper::new(&steps).render(Rect::new(0, 0, 20, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("✓ One ── ● Two      ")
        );
    }

    #[test]
    fn renders_vertical_stepper() {
        let steps = [
            StepItem::new("one", "One").status(StepStatus::Complete),
            StepItem::new("two", "Two").status(StepStatus::Current),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        let mut frame = Frame::new(&mut buffer);

        Stepper::new(&steps)
            .policy(StepperPolicy::vertical())
            .render(Rect::new(0, 0, 10, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("  ✓ One   "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("│ ● Two   "));
    }

    #[test]
    fn supports_all_status_markers() {
        let statuses = [
            (StepStatus::Pending, "○"),
            (StepStatus::Current, "●"),
            (StepStatus::Complete, "✓"),
            (StepStatus::Warning, "!"),
            (StepStatus::Error, "×"),
            (StepStatus::Disabled, "-"),
        ];
        for (status, marker) in statuses {
            let steps = [StepItem::new("id", "Label").status(status)];
            let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
            let mut frame = Frame::new(&mut buffer);

            Stepper::new(&steps).render(Rect::new(0, 0, 8, 1), &mut frame);

            assert!(
                frame
                    .buffer()
                    .row_symbols(0)
                    .is_some_and(|row| row.starts_with(marker))
            );
        }
    }

    #[test]
    fn truncates_tiny_area() {
        let steps = [StepItem::new("one", "Long label").status(StepStatus::Current)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);

        Stepper::new(&steps).render(Rect::new(0, 0, 4, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("● L…"));
    }

    #[test]
    fn no_markers_policy_renders_labels_only() {
        let steps = [StepItem::new("one", "One")];
        let policy = StepperPolicy {
            markers: false,
            orientation: StepperOrientation::Horizontal,
            ..StepperPolicy::horizontal()
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        Stepper::new(&steps)
            .policy(policy)
            .render(Rect::new(0, 0, 6, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("One   "));
    }
}
