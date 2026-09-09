//! Generic stepper / multi-step progress indicator component.

use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata, LayoutNode,
    LogicalSize,
};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::display_width;

/// Generic step status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
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

/// Canonical component-lifecycle stepper.
pub struct StepperComponent<'a> {
    id: LayoutId,
    steps: &'a [StepItem<'a>],
    policy: StepperPolicy,
    styles: StepperStyles,
}

impl<'a> StepperComponent<'a> {
    /// Create a stepper component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, steps: &'a [StepItem<'a>]) -> Self {
        Self {
            id: id.into(),
            steps,
            policy: StepperPolicy::horizontal(),
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

    /// Set layout and rendering policy.
    #[must_use]
    pub const fn policy(mut self, policy: StepperPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: StepperStyles) -> Self {
        self.styles = styles;
        self
    }
}

impl Component for StepperComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.policy.orientation.hash(&mut layout);
        self.policy.connector.hash(&mut layout);
        self.policy.markers.hash(&mut layout);
        for step in self.steps {
            step.id.hash(&mut layout);
            step.label.hash(&mut layout);
        }

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.policy.truncate.hash(&mut paint);
        for step in self.steps {
            step.status.hash(&mut paint);
        }
        self.styles.pending.hash(&mut paint);
        self.styles.current.hash(&mut paint);
        self.styles.complete.hash(&mut paint);
        self.styles.warning.hash(&mut paint);
        self.styles.error.hash(&mut paint);
        self.styles.disabled.hash(&mut paint);
        self.styles.connector.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let intrinsic_width = match self.policy.orientation {
            StepperOrientation::Horizontal => {
                let connector_width = display_width(&format!(" {} ", self.policy.connector));
                self.steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| {
                        display_width(&self.step_text(step))
                            + usize::from(index > 0) * connector_width
                    })
                    .sum()
            }
            StepperOrientation::Vertical => self
                .steps
                .iter()
                .enumerate()
                .map(|(index, step)| {
                    let prefix = if index == 0 {
                        2
                    } else {
                        display_width(self.policy.connector).saturating_add(1)
                    };
                    prefix.saturating_add(display_width(&self.step_text(step)))
                })
                .max()
                .unwrap_or_default(),
        };
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            u16::try_from(intrinsic_width)
                .unwrap_or(u16::MAX)
                .clamp(
                    constraints.min_width().try_into().unwrap_or(u16::MAX),
                    constraints.max_width().try_into().unwrap_or(u16::MAX),
                )
                .into()
        };
        let intrinsic_height = match self.policy.orientation {
            StepperOrientation::Horizontal => usize::from(!self.steps.is_empty()),
            StepperOrientation::Vertical => self.steps.len(),
        };
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                width,
                intrinsic_height.try_into().unwrap_or(u64::MAX),
            )),
        )
        .with_metadata(LayoutMetadata::new().semantic("progress"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 || self.steps.is_empty() {
            return;
        }
        let viewport = cx.area();
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let logical_width = usize::try_from(layout.size.width).unwrap_or(usize::MAX);
        let content_height = match self.policy.orientation {
            StepperOrientation::Horizontal => 1,
            StepperOrientation::Vertical => self.steps.len(),
        };
        if viewport.width == 0
            || viewport.height == 0
            || viewport.y.saturating_add(i64::from(viewport.height)) <= 0
            || usize::try_from(viewport.y.max(0)).unwrap_or(usize::MAX)
                >= content_height.min(layout.size.height.try_into().unwrap_or(usize::MAX))
        {
            return;
        }
        match self.policy.orientation {
            StepperOrientation::Horizontal => {
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
                if self.policy.truncate && line.width() > logical_width {
                    line = line.truncate(logical_width);
                }
                cx.write_line(LocalRect::new(0, 0, width, 1), &line);
            }
            StepperOrientation::Vertical => {
                let viewport = cx.area();
                let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
                let logical_width = usize::try_from(layout.size.width).unwrap_or(usize::MAX);
                let start = usize::try_from(viewport.y.max(0)).unwrap_or(usize::MAX);
                let end =
                    usize::try_from(viewport.y.saturating_add(i64::from(viewport.height)).max(0))
                        .unwrap_or(usize::MAX)
                        .min(layout.size.height.try_into().unwrap_or(usize::MAX))
                        .min(self.steps.len());
                for index in start..end {
                    let step = &self.steps[index];
                    let prefix = if index > 0 {
                        format!("{} ", self.policy.connector)
                    } else {
                        "  ".to_owned()
                    };
                    let mut line = Line::from_spans([
                        Span::styled(prefix, self.styles.connector),
                        Span::styled(self.step_text(step), self.style_for(step.status)),
                    ]);
                    if self.policy.truncate {
                        line = line.truncate(logical_width);
                    }
                    cx.write_line(
                        LocalRect::new(0, i64::try_from(index).unwrap_or(i64::MAX), width, 1),
                        &line,
                    );
                }
            }
        }
        let start = usize::try_from(viewport.y.max(0)).unwrap_or(usize::MAX);
        let end = usize::try_from(viewport.y.saturating_add(i64::from(viewport.height)).max(0))
            .unwrap_or(usize::MAX)
            .min(layout.size.height.try_into().unwrap_or(usize::MAX))
            .min(content_height);
        let height = u16::try_from(end.saturating_sub(start)).unwrap_or(u16::MAX);
        cx.with_child(
            0,
            i64::try_from(start).unwrap_or(i64::MAX),
            LocalRect::new(0, 0, width, height),
            |cx| {
                cx.push_semantic(SemanticRegion::new(
                    self.id.as_str(),
                    Rect::new(0, 0, width, height),
                    "progress",
                ));
                cx.push_damage(LocalRect::new(0, 0, width, height));
            },
        );
    }
}

impl StepperComponent<'_> {
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

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`StepperStyles`].
    #[must_use]
    pub fn stepper_styles(self) -> StepperStyles {
        StepperStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for StepperStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            pending: theme.muted,
            current: theme.info.add_modifier(bmux_tui::style::Modifier::BOLD),
            complete: theme.success,
            warning: theme.warning.add_modifier(bmux_tui::style::Modifier::BOLD),
            error: theme.error.add_modifier(bmux_tui::style::Modifier::BOLD),
            disabled: theme.disabled,
            connector: theme.border,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx, LogicalSize};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::paint::PaintCx;
    use bmux_tui::style::{Color, Style};

    use super::{
        StepItem, StepStatus, StepperComponent, StepperOrientation, StepperPolicy, StepperStyles,
    };

    #[test]
    fn truncation_changes_paint_revision_without_remeasurement() {
        let steps = [
            StepItem::new("one", "Long first step"),
            StepItem::new("two", "Second"),
        ];
        for orientation in [StepperOrientation::Horizontal, StepperOrientation::Vertical] {
            let policy = StepperPolicy {
                orientation,
                ..StepperPolicy::horizontal()
            };
            let truncated = StepperComponent::new("steps", &steps).policy(StepperPolicy {
                truncate: true,
                ..policy
            });
            let clipped = StepperComponent::new("steps", &steps).policy(StepperPolicy {
                truncate: false,
                ..policy
            });
            assert_eq!(truncated.revision().layout, clipped.revision().layout);
            assert_ne!(truncated.revision().paint, clipped.revision().paint);
            for width in [0, 4, 80] {
                let constraints = Constraints::for_width(width);
                assert_eq!(
                    truncated.layout(constraints, &mut LayoutCx::new()).size,
                    clipped.layout(constraints, &mut LayoutCx::new()).size
                );
            }
        }
    }

    #[test]
    fn vertical_connector_uses_connector_style_not_step_status() {
        let steps = [StepItem::new("one", "One"), StepItem::new("two", "Two")];
        let connector = Style::new().fg(Color::Red);
        let pending = Style::new().fg(Color::Blue);
        let component = StepperComponent::new("steps", &steps)
            .policy(StepperPolicy::vertical())
            .styles(StepperStyles {
                pending,
                connector,
                ..StepperStyles::default()
            });
        let layout = component.layout(Constraints::for_width(10), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let at = |x| {
            frame
                .buffer()
                .get(bmux_tui::geometry::Point::new(x, 1))
                .unwrap()
        };
        assert_eq!(at(0).symbol, "│");
        assert_eq!(at(0).style, connector);
        assert_eq!(at(2).style, pending);
        assert_eq!(at(4).symbol, "T");
        assert_eq!(at(4).style, pending);
    }

    #[test]
    fn oversized_layout_metadata_covers_only_content_rows() {
        let steps = [StepItem::new("one", "One"), StepItem::new("two", "Two")];
        for (orientation, height) in [
            (StepperOrientation::Horizontal, 1),
            (StepperOrientation::Vertical, 2),
        ] {
            let component = StepperComponent::new("steps", &steps).policy(StepperPolicy {
                orientation,
                ..StepperPolicy::horizontal()
            });
            let layout = component.layout(
                Constraints::tight(Rect::new(0, 0, 30, 5).size()),
                &mut LayoutCx::new(),
            );
            let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 5));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            assert_eq!(
                frame.semantics().regions()[0].area,
                Rect::new(0, 0, 30, height)
            );
            assert_eq!(
                frame
                    .damage(bmux_tui::damage::DamagePolicy::default())
                    .retained_regions(),
                &[Rect::new(0, 0, 30, height)]
            );
            assert_eq!(
                frame.buffer().row_symbols(height).as_deref(),
                Some("                              ")
            );
        }
    }

    #[test]
    fn horizontal_custom_connector_measurement_matches_rendering() {
        let steps = [
            StepItem::new("one", "One"),
            StepItem::new("two", "Two"),
            StepItem::new("three", "Three"),
        ];
        for connector in ["", "中", "->"] {
            let component = StepperComponent::new("steps", &steps).policy(StepperPolicy {
                connector,
                markers: false,
                ..StepperPolicy::horizontal()
            });
            let expected = format!("One {connector} Two {connector} Three");
            let layout = component.layout(
                Constraints::loose(Rect::new(0, 0, 80, 1).size()),
                &mut LayoutCx::new(),
            );
            assert_eq!(
                usize::try_from(layout.size.width).unwrap_or(usize::MAX),
                bmux_tui::text_width::display_width(&expected)
            );
            let mut buffer = Buffer::empty(Rect::new(
                0,
                0,
                layout.size.width.try_into().unwrap_or(u16::MAX),
                1,
            ));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            assert_eq!(
                frame.buffer().row_symbols(0).as_deref(),
                Some(expected.as_str())
            );
        }
    }

    #[test]
    fn scrolling_past_content_publishes_no_paint_metadata() {
        let steps = [StepItem::new("one", "One"), StepItem::new("two", "Two")];
        for orientation in [StepperOrientation::Horizontal, StepperOrientation::Vertical] {
            let component = StepperComponent::new("steps", &steps).policy(StepperPolicy {
                orientation,
                ..StepperPolicy::horizontal()
            });
            let layout = component.layout(
                Constraints::tight(Rect::new(0, 0, 20, 8).size()),
                &mut LayoutCx::new(),
            );
            let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
            let mut frame = Frame::new(&mut buffer);
            PaintCx::new(&mut frame).with_child(
                0,
                -3,
                bmux_tui::paint::LocalRect::new(0, 0, 20, 8),
                |cx| component.paint(&layout, cx),
            );
            assert!(frame.semantics().regions().is_empty());
            assert!(
                frame
                    .damage(bmux_tui::damage::DamagePolicy::default())
                    .retained_regions()
                    .is_empty()
            );
            assert_eq!(
                frame.buffer().row_symbols(0).as_deref(),
                Some("                    ")
            );
        }
    }

    #[test]
    fn deep_scroll_keeps_vertical_steps_and_semantics_visible() {
        let mut steps = vec![StepItem::new("earlier", "Earlier"); 70_000];
        steps.extend([
            StepItem::new("one", "One"),
            StepItem::new("two", "Two"),
            StepItem::new("three", "Three"),
        ]);
        let component = StepperComponent::new("setup", &steps).policy(StepperPolicy::vertical());
        let layout = component.layout(Constraints::for_width(12), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 5));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(
            2,
            -69_999,
            bmux_tui::paint::LocalRect::new(0, 70_000, 12, 3),
            |cx| component.paint(&layout, cx),
        );
        for (row, label) in [(1, "│ ○ One"), (2, "│ ○ Two"), (3, "│ ○ Three")] {
            assert!(frame.buffer().row_symbols(row).unwrap().contains(label));
        }
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("                ")
        );
        assert_eq!(
            frame.buffer().row_symbols(4).as_deref(),
            Some("                ")
        );
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(2, 1, 12, 3));
    }

    #[test]
    fn component_measures_and_paints_horizontal_progress() {
        let steps = [
            StepItem::new("one", "One").status(StepStatus::Complete),
            StepItem::new("two", "Two").status(StepStatus::Current),
        ];
        let component = StepperComponent::new("setup", &steps);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(20, 1));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("✓ One ── ● Two      ")
        );
        assert_eq!(frame.semantics().regions().len(), 1);
        assert!(
            !frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .is_none()
        );
    }

    #[test]
    fn component_vertical_layout_respects_height_constraint() {
        let steps = [
            StepItem::new("one", "One"),
            StepItem::new("two", "Two"),
            StepItem::new("three", "Three"),
        ];
        let component = StepperComponent::new("setup", &steps).policy(StepperPolicy::vertical());
        let layout = component.layout(Constraints::new(10, 10, 0, Some(2)), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(10, 2));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("│ ○ Two   "));
    }

    #[test]
    fn component_status_and_styles_are_paint_only() {
        let pending = [StepItem::new("one", "One")];
        let current = [StepItem::new("one", "One").status(StepStatus::Current)];
        let initial = StepperComponent::new("setup", &pending).revision();
        let status = StepperComponent::new("setup", &current).revision();
        let styled = StepperComponent::new("setup", &pending)
            .styles(StepperStyles {
                current: Style::new().fg(Color::Red),
                ..StepperStyles::default()
            })
            .revision();
        assert_eq!(initial.layout, status.layout);
        assert_ne!(initial.paint, status.paint);
        assert_eq!(initial.layout, styled.layout);
        assert_ne!(initial.paint, styled.paint);
    }

    #[test]
    fn no_markers_policy_renders_labels_only() {
        let steps = [StepItem::new("one", "One")];
        let policy = StepperPolicy {
            markers: false,
            orientation: StepperOrientation::Horizontal,
            ..StepperPolicy::horizontal()
        };
        let component = StepperComponent::new("setup", &steps).policy(policy);
        let layout = component.layout(Constraints::for_width(6), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("One   "));
    }
}
