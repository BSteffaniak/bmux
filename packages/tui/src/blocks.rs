//! Transcript/status block primitives.

use crate::chrome::{Border, Panel};
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::style::{Color, Modifier, Style};
use crate::text::{Line, Span, Text};
use crate::text_block::{TextBlock, TextWrap};
use crate::widget::Widget;

/// Semantic status level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusLevel {
    /// Informational status.
    #[default]
    Info,
    /// Successful/completed status.
    Success,
    /// Warning status.
    Warning,
    /// Error status.
    Error,
    /// In-progress status.
    InProgress,
}

impl StatusLevel {
    /// Default style for this level.
    #[must_use]
    pub const fn default_style(self) -> Style {
        match self {
            Self::Info => Style::new().fg(Color::Cyan),
            Self::Success => Style::new().fg(Color::Green),
            Self::Warning => Style::new().fg(Color::Yellow),
            Self::Error => Style::new().fg(Color::Red),
            Self::InProgress => Style::new().fg(Color::Blue),
        }
    }

    const fn marker(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Success => "✓",
            Self::Warning => "!",
            Self::Error => "✗",
            Self::InProgress => "…",
        }
    }
}

/// A compact status line widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBlock {
    level: StatusLevel,
    message: Line,
}

impl StatusBlock {
    /// Create a status block.
    #[must_use]
    pub fn new(level: StatusLevel, message: impl Into<Line>) -> Self {
        Self {
            level,
            message: message.into(),
        }
    }
}

impl Widget for StatusBlock {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        let style = self.level.default_style();
        let mut spans = vec![
            Span::styled(self.level.marker(), style.add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ];
        spans.extend(
            self.message
                .spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), style.patch(span.style))),
        );
        frame.write_line(area, &Line::from_spans(spans));
    }
}

/// Progress block with optional total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressBlock {
    label: Line,
    current: u64,
    total: Option<u64>,
    style: Style,
}

impl ProgressBlock {
    /// Create progress with an optional total.
    #[must_use]
    pub fn new(label: impl Into<Line>, current: u64, total: Option<u64>) -> Self {
        Self {
            label: label.into(),
            current,
            total,
            style: StatusLevel::InProgress.default_style(),
        }
    }

    /// Set progress style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl Widget for ProgressBlock {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        let progress = self.total.map_or_else(
            || self.current.to_string(),
            |total| format!("{}/{}", self.current.min(total), total),
        );
        let mut spans = vec![Span::styled("… ", self.style)];
        spans.extend(
            self.label
                .spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), self.style.patch(span.style))),
        );
        spans.push(Span::styled(format!(" ({progress})"), self.style));
        frame.write_line(area, &Line::from_spans(spans));
    }
}

/// A generic tool-call/result transcript block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBlock {
    title: Line,
    body: Text,
    status: StatusLevel,
    panel: Panel,
}

impl ToolBlock {
    /// Create a tool block.
    #[must_use]
    pub fn new(title: impl Into<Line>, body: impl Into<Text>, status: StatusLevel) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            status,
            panel: Panel::new().border(Border::single()),
        }
    }

    /// Set panel chrome.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = panel;
        self
    }
}

impl Widget for ToolBlock {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        let title = Line::from_spans(vec![
            Span::styled(self.status.marker(), self.status.default_style()),
            Span::raw(" "),
            Span::styled(self.title.plain_text(), self.status.default_style()),
        ]);
        let panel = self.panel.clone().title(title);
        panel.render(area, frame);
        TextBlock::new(self.body.clone())
            .wrap(TextWrap::Word)
            .render(panel.inner_area(area), frame);
    }
}

#[cfg(test)]
mod tests {
    use super::{ProgressBlock, StatusBlock, StatusLevel, ToolBlock};
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::style::{Color, Style};
    use crate::text::{Line, Text};
    use crate::widget::Widget;

    #[test]
    fn status_block_renders_level_marker_and_message() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut frame = Frame::new(&mut buffer);

        StatusBlock::new(StatusLevel::Success, "done").render(Rect::new(0, 0, 12, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("✓ done      ")
        );
        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).map(|cell| cell.style),
            Some(
                StatusLevel::Success
                    .default_style()
                    .add_modifier(crate::style::Modifier::BOLD)
            )
        );
    }

    #[test]
    fn progress_block_renders_counts() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 18, 1));
        let mut frame = Frame::new(&mut buffer);

        ProgressBlock::new("tokens", 3, Some(5)).render(Rect::new(0, 0, 18, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("… tokens (3/5)    ")
        );
    }

    #[test]
    fn tool_block_renders_panel_and_body() {
        let body = Text::from_lines(vec![Line::raw("read file"), Line::raw("ok")]);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 4));
        let mut frame = Frame::new(&mut buffer);

        ToolBlock::new("tool", body, StatusLevel::Info).render(Rect::new(0, 0, 14, 4), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("┌ℹ tool──────┐")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("│read file   │")
        );
        assert_eq!(
            frame.buffer().row_symbols(2).as_deref(),
            Some("│ok          │")
        );
        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("└────────────┘")
        );
    }

    #[test]
    fn progress_block_style_can_be_overridden() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);
        let style = Style::new().fg(Color::Magenta);

        ProgressBlock::new("x", 1, None)
            .style(style)
            .render(Rect::new(0, 0, 4, 1), &mut frame);

        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).map(|cell| cell.style),
            Some(style)
        );
    }
}
