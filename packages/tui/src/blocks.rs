//! Transcript/status block primitives.

use std::hash::{Hash, Hasher};

use crate::chrome::{Border, Panel, PanelComponent};
use crate::component::{
    ChildLayout, Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize, combine_child_revisions,
};
use crate::geometry::Rect;
use crate::paint::{LocalRect, PaintCx};
use crate::style::{Color, Modifier, Style};
use crate::text::{Line, Span, Text};
use crate::text_block::{TextBlock, TextWrap};

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

/// A compact status line component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBlock {
    id: LayoutId,
    level: StatusLevel,
    message: Line,
}

impl StatusBlock {
    /// Create a status block.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, level: StatusLevel, message: impl Into<Line>) -> Self {
        Self {
            id: id.into(),
            level,
            message: message.into(),
        }
    }

    fn line(&self) -> Line {
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
        Line::from_spans(spans)
    }
}

impl Component for StatusBlock {
    fn revision(&self) -> ComponentRevision {
        revisions(&self.id, &self.line())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(line_width(&self.line()), 1)),
        )
        .with_metadata(LayoutMetadata::new().semantic("status"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        cx.write_line(area, &self.line());
        cx.push_damage(area);
    }
}

/// Progress block with optional total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressBlock {
    id: LayoutId,
    label: Line,
    current: u64,
    total: Option<u64>,
    style: Style,
}

impl ProgressBlock {
    /// Create progress with an optional total.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        label: impl Into<Line>,
        current: u64,
        total: Option<u64>,
    ) -> Self {
        Self {
            id: id.into(),
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
    fn line(&self) -> Line {
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
        Line::from_spans(spans)
    }
}

impl Component for ProgressBlock {
    fn revision(&self) -> ComponentRevision {
        revisions(&self.id, &self.line())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(line_width(&self.line()), 1)),
        )
        .with_metadata(LayoutMetadata::new().semantic("progress"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        cx.write_line(area, &self.line());
        cx.push_damage(area);
    }
}

fn line_width(line: &Line) -> u16 {
    u16::try_from(line.width()).unwrap_or(u16::MAX)
}

fn revisions(id: &LayoutId, line: &Line) -> ComponentRevision {
    let mut layout = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut layout);
    line.width().hash(&mut layout);
    let mut paint = std::collections::hash_map::DefaultHasher::new();
    format!("{line:?}").hash(&mut paint);
    ComponentRevision::new(layout.finish(), paint.finish())
}

/// A generic tool-call/result transcript block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBlock {
    id: LayoutId,
    title: Line,
    body: Text,
    status: StatusLevel,
    panel: Panel,
}

impl ToolBlock {
    /// Create a tool block.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        title: impl Into<Line>,
        body: impl Into<Text>,
        status: StatusLevel,
    ) -> Self {
        Self {
            id: id.into(),
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

impl ToolBlock {
    fn titled_panel(&self) -> Panel {
        self.panel.clone().title(Line::from_spans(vec![
            Span::styled(self.status.marker(), self.status.default_style()),
            Span::raw(" "),
            Span::styled(self.title.plain_text(), self.status.default_style()),
        ]))
    }

    fn body_component(&self) -> TextBlock {
        TextBlock::new(self.body.clone())
            .id(format!("{}.body", self.id.as_str()))
            .wrap(TextWrap::Word)
    }
}

impl Component for ToolBlock {
    fn revision(&self) -> ComponentRevision {
        let panel = self.titled_panel();
        let panel = PanelComponent::new(format!("{}.panel", self.id.as_str()), &panel);
        combine_child_revisions(
            ComponentRevision::default(),
            [panel.revision(), self.body_component().revision()],
        )
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let size = constraints.constrain(LogicalSize::new(
            constraints.max_width(),
            constraints
                .max_height()
                .unwrap_or_else(|| constraints.min_height()),
        ));
        let panel = self.titled_panel();
        let panel_layout = PanelComponent::new(format!("{}.panel", self.id.as_str()), &panel)
            .layout(
                Constraints::new(size.width, size.width, size.height, Some(size.height)),
                cx,
            );
        let inner = panel.inner_area(Rect::new(
            0,
            0,
            size.width,
            u16::try_from(size.height).unwrap_or(u16::MAX),
        ));
        let body_layout = self.body_component().layout(
            Constraints::new(
                inner.width,
                inner.width,
                usize::from(inner.height),
                Some(usize::from(inner.height)),
            ),
            cx,
        );
        LayoutNode::with_children(
            self.id.clone(),
            size,
            vec![
                ChildLayout::new(0, 0, panel_layout),
                ChildLayout::new(inner.x, usize::from(inner.y), body_layout),
            ],
        )
        .with_metadata(LayoutMetadata::new().semantic("tool-block"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let Some(panel_layout) = layout.children.first() else {
            return;
        };
        let panel = self.titled_panel();
        PanelComponent::new(format!("{}.panel", self.id.as_str()), &panel)
            .paint(&panel_layout.node, cx);
        let Some(body_layout) = layout.children.get(1) else {
            return;
        };
        let body = self.body_component();
        cx.with_child(
            i32::from(body_layout.x),
            i64::try_from(body_layout.y).unwrap_or(i64::MAX),
            LocalRect::new(
                0,
                0,
                body_layout.node.size.width,
                u16::try_from(body_layout.node.size.height).unwrap_or(u16::MAX),
            ),
            |cx| body.paint(&body_layout.node, cx),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{ProgressBlock, StatusBlock, StatusLevel, ToolBlock};
    use crate::buffer::Buffer;
    use crate::component::{Component, Constraints, LayoutCx};
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::paint::PaintCx;
    use crate::style::{Color, Style};
    use crate::text::{Line, Text};

    #[test]
    fn status_block_renders_level_marker_and_message() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut frame = Frame::new(&mut buffer);

        let component = StatusBlock::new("status", StatusLevel::Success, "done");
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 12, 1).size()),
            &mut LayoutCx::new(),
        );
        component.paint(&layout, &mut PaintCx::new(&mut frame));

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

        let component = ProgressBlock::new("progress", "tokens", 3, Some(5));
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 18, 1).size()),
            &mut LayoutCx::new(),
        );
        component.paint(&layout, &mut PaintCx::new(&mut frame));

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

        let component = ToolBlock::new("tool", "tool", body, StatusLevel::Info);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 14, 4).size()),
            &mut LayoutCx::new(),
        );
        component.paint(&layout, &mut PaintCx::new(&mut frame));

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

        let component = ProgressBlock::new("progress", "x", 1, None).style(style);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 4, 1).size()),
            &mut LayoutCx::new(),
        );
        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).map(|cell| cell.style),
            Some(style)
        );
    }
}
