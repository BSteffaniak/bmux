//! Small generic bar chart component.

use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata, LayoutNode,
    LogicalSize,
};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::{display_width, truncate_to_display_width};

/// One bar chart item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarChartItem<'a> {
    /// Bar label.
    pub label: &'a str,
    /// Bar value.
    pub value: u64,
    /// Optional grouped values rendered as adjacent bar segments.
    pub group: &'a [u64],
}

impl<'a> BarChartItem<'a> {
    /// Create a bar chart item.
    #[must_use]
    pub const fn new(label: &'a str, value: u64) -> Self {
        Self {
            label,
            value,
            group: &[],
        }
    }
    /// Return this item with grouped values.
    #[must_use]
    pub const fn group(mut self, group: &'a [u64]) -> Self {
        self.group = group;
        self
    }
}

/// Value label placement for [`BarChart`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BarChartValuePlacement {
    /// Do not render value labels.
    Hidden,
    /// Render values after bars.
    Right,
    /// Render values over the bar area near the right edge.
    Inside,
}

/// Bar chart rendering policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarChartPolicy {
    /// Optional maximum value. When absent, max is derived from visible data.
    pub max: Option<u64>,
    /// Width reserved for labels.
    pub label_width: u16,
    /// Optional maximum width for the rendered bar area.
    pub bar_width: Option<u16>,
    /// Blank rows between bars.
    pub bar_gap: u16,
    /// Bar fill symbol.
    pub bar: &'static str,
    /// Empty bar symbol.
    pub empty: &'static str,
    /// Separator between label and bar.
    pub separator: &'static str,
    /// Value label placement.
    pub value_placement: BarChartValuePlacement,
    /// Render numeric values after bars.
    pub values: bool,
    /// Truncate labels to reserved width.
    pub truncate_labels: bool,
}

impl BarChartPolicy {
    /// Compact horizontal bar chart.
    #[must_use]
    pub const fn compact() -> Self {
        Self {
            max: None,
            label_width: 6,
            bar_width: None,
            bar_gap: 0,
            bar: "█",
            empty: "░",
            separator: " ",
            value_placement: BarChartValuePlacement::Hidden,
            values: false,
            truncate_labels: true,
        }
    }

    /// Compact chart with value labels.
    #[must_use]
    pub const fn with_values() -> Self {
        Self {
            value_placement: BarChartValuePlacement::Right,
            values: true,
            ..Self::compact()
        }
    }

    /// Return this policy with max override.
    #[must_use]
    pub const fn max(mut self, max: Option<u64>) -> Self {
        self.max = max;
        self
    }
    /// Return this policy with value label placement changed.
    #[must_use]
    pub const fn value_placement(mut self, value_placement: BarChartValuePlacement) -> Self {
        self.value_placement = value_placement;
        self.values = !matches!(value_placement, BarChartValuePlacement::Hidden);
        self
    }

    /// Return this policy with maximum bar width changed.
    #[must_use]
    pub const fn bar_width(mut self, bar_width: Option<u16>) -> Self {
        self.bar_width = bar_width;
        self
    }

    /// Return this policy with blank rows between bars changed.
    #[must_use]
    pub const fn bar_gap(mut self, bar_gap: u16) -> Self {
        self.bar_gap = bar_gap;
        self
    }
}

impl Default for BarChartPolicy {
    fn default() -> Self {
        Self::compact()
    }
}

/// Bar chart visual styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarChartStyles {
    /// Label style.
    pub label: Style,
    /// Filled bar style.
    pub bar: Style,
    /// Empty bar style.
    pub empty: Style,
    /// Value style.
    pub value: Style,
    /// Empty-data style.
    pub empty_message: Style,
}

impl Default for BarChartStyles {
    fn default() -> Self {
        Self {
            label: Style::new().fg(Color::BrightWhite),
            bar: Style::new()
                .fg(Color::BrightCyan)
                .add_modifier(Modifier::BOLD),
            empty: Style::new().fg(Color::BrightBlack),
            value: Style::new().fg(Color::BrightBlack),
            empty_message: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Canonical component-lifecycle bar chart.
pub struct BarChartComponent<'a> {
    id: LayoutId,
    chart: BarChart<'a>,
}

impl<'a> BarChartComponent<'a> {
    /// Create a bar-chart component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, items: &'a [BarChartItem<'a>]) -> Self {
        Self {
            id: id.into(),
            chart: BarChart::new(items),
        }
    }

    /// Set rendering policy.
    #[must_use]
    pub const fn policy(mut self, policy: BarChartPolicy) -> Self {
        self.chart.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: BarChartStyles) -> Self {
        self.chart.styles = styles;
        self
    }

    /// Set empty message.
    #[must_use]
    pub const fn empty(mut self, empty: &'a str) -> Self {
        self.chart.empty = empty;
        self
    }
}

impl Component for BarChartComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.chart.empty.hash(&mut layout);
        self.chart.policy.label_width.hash(&mut layout);
        self.chart.policy.bar_width.hash(&mut layout);
        self.chart.policy.bar_gap.hash(&mut layout);
        self.chart.policy.bar.hash(&mut layout);
        self.chart.policy.empty.hash(&mut layout);
        self.chart.policy.separator.hash(&mut layout);
        self.chart.policy.value_placement.hash(&mut layout);
        self.chart.policy.values.hash(&mut layout);
        self.chart.policy.truncate_labels.hash(&mut layout);
        for item in self.chart.items {
            item.label.hash(&mut layout);
            item.group.len().hash(&mut layout);
        }

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.chart.policy.max.hash(&mut paint);
        for item in self.chart.items {
            item.value.hash(&mut paint);
            item.group.hash(&mut paint);
        }
        self.chart.styles.label.hash(&mut paint);
        self.chart.styles.bar.hash(&mut paint);
        self.chart.styles.empty.hash(&mut paint);
        self.chart.styles.value.hash(&mut paint);
        self.chart.styles.empty_message.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else if self.chart.items.is_empty() {
            u16::try_from(display_width(self.chart.empty))
                .unwrap_or(u16::MAX)
                .clamp(constraints.min_width(), constraints.max_width())
        } else {
            let label_width = self
                .chart
                .items
                .iter()
                .map(|item| display_width(item.label))
                .max()
                .unwrap_or_default()
                .min(usize::from(self.chart.policy.label_width));
            let value_width = usize::from(self.chart.policy.values)
                * self
                    .chart
                    .items
                    .iter()
                    .map(|item| item.value.to_string().len().saturating_add(1))
                    .max()
                    .unwrap_or_default();
            u16::try_from(
                label_width
                    .saturating_add(display_width(self.chart.policy.separator))
                    .saturating_add(value_width)
                    .saturating_add(1),
            )
            .unwrap_or(u16::MAX)
            .clamp(constraints.min_width(), constraints.max_width())
        };
        let visible = self.chart.items.len();
        let height = if visible == 0 {
            usize::from(width > 0)
        } else {
            visible.saturating_add(
                usize::from(self.chart.policy.bar_gap).saturating_mul(visible.saturating_sub(1)),
            )
        };
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, height)),
        )
        .with_metadata(LayoutMetadata::new().semantic("chart"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        if self.chart.items.is_empty() {
            cx.write_line_with_fallback_style(
                LocalRect::new(0, 0, layout.size.width, 1),
                &Line::from(self.chart.empty),
                self.chart.styles.empty_message,
            );
        } else {
            let max = self.chart.policy.max.unwrap_or_else(|| {
                self.chart
                    .items
                    .iter()
                    .flat_map(|item| std::iter::once(item.value).chain(item.group.iter().copied()))
                    .max()
                    .unwrap_or(0)
            });
            let row_step = usize::from(self.chart.policy.bar_gap).saturating_add(1);
            for (index, item) in self.chart.items.iter().enumerate() {
                let row = index.saturating_mul(row_step);
                if row >= layout.size.height {
                    break;
                }
                cx.write_line(
                    LocalRect::new(
                        0,
                        i64::try_from(row).unwrap_or(i64::MAX),
                        layout.size.width,
                        1,
                    ),
                    &self.chart.item_line(item, max, layout.size.width),
                );
            }
        }
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let area = LocalRect::new(0, 0, layout.size.width, height);
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, height),
            "chart",
        ));
        cx.push_damage(area);
    }
}

/// Small horizontal bar chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarChart<'a> {
    items: &'a [BarChartItem<'a>],
    policy: BarChartPolicy,
    styles: BarChartStyles,
    empty: &'a str,
}

impl<'a> BarChart<'a> {
    /// Create a bar chart over caller-owned items.
    #[must_use]
    pub const fn new(items: &'a [BarChartItem<'a>]) -> Self {
        Self {
            items,
            policy: BarChartPolicy {
                max: None,
                label_width: 6,
                bar_width: None,
                bar_gap: 0,
                bar: "█",
                empty: "░",
                separator: " ",
                value_placement: BarChartValuePlacement::Hidden,
                values: false,
                truncate_labels: true,
            },
            styles: BarChartStyles {
                label: Style::new(),
                bar: Style::new(),
                empty: Style::new(),
                value: Style::new(),
                empty_message: Style::new(),
            },
            empty: "No data",
        }
    }

    /// Set policy.
    #[must_use]
    pub const fn policy(mut self, policy: BarChartPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: BarChartStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set empty message.
    #[must_use]
    pub const fn empty(mut self, empty: &'a str) -> Self {
        self.empty = empty;
        self
    }

    /// Compute filled bar width.
    #[must_use]
    pub fn filled_width(&self, value: u64, max: u64, width: u16) -> u16 {
        filled_width(value, max, width)
    }

    /// Render bar chart.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if self.items.is_empty() {
            frame.write_line_with_fallback_style(
                area,
                &Line::from(self.empty),
                self.styles.empty_message,
            );
            return;
        }
        let max = self.policy.max.unwrap_or_else(|| {
            self.items
                .iter()
                .flat_map(|item| std::iter::once(item.value).chain(item.group.iter().copied()))
                .max()
                .unwrap_or(0)
        });
        let row_step = usize::from(self.policy.bar_gap).saturating_add(1);
        for (index, item) in self.items.iter().enumerate() {
            let y_offset = index.saturating_mul(row_step);
            if y_offset >= usize::from(area.height) {
                return;
            }
            let Ok(y_offset) = u16::try_from(y_offset) else {
                return;
            };
            let row = Rect::new(area.x, area.y.saturating_add(y_offset), area.width, 1);
            frame.write_line(row, &self.item_line(item, max, area.width));
        }
    }

    fn item_line(&self, item: &BarChartItem<'_>, max: u64, width: u16) -> Line {
        let label_width = self.policy.label_width.min(width);
        let separator_width = u16_saturating(display_width(self.policy.separator));
        let value_text = if self.policy.values {
            item.value.to_string()
        } else {
            String::new()
        };
        let right_value = matches!(self.policy.value_placement, BarChartValuePlacement::Right)
            && !value_text.is_empty();
        let value_width = if right_value {
            u16_saturating(display_width(&format!(" {value_text}")))
        } else {
            0
        };
        let available_bar_width = width
            .saturating_sub(label_width)
            .saturating_sub(separator_width)
            .saturating_sub(value_width);
        let bar_width = self
            .policy
            .bar_width
            .unwrap_or(available_bar_width)
            .min(available_bar_width);
        let label = format_label(item.label, label_width, self.policy.truncate_labels);
        let segment_count = if item.group.is_empty() {
            1
        } else {
            item.group.len().saturating_add(1)
        };
        let segment_gap = u16::from(segment_count > 1);
        let total_gap = segment_gap.saturating_mul(u16_saturating(segment_count.saturating_sub(1)));
        let segment_width = if segment_count > 1 {
            bar_width
                .saturating_sub(total_gap)
                .checked_div(u16_saturating(segment_count).max(1))
                .unwrap_or(0)
        } else {
            bar_width
        };
        let mut spans = Vec::new();
        spans.push(Span::styled(label, self.styles.label));
        spans.push(Span::raw(self.policy.separator));
        if item.group.is_empty() {
            let filled = self.filled_width(item.value, max, bar_width).min(bar_width);
            let empty = bar_width.saturating_sub(filled);
            spans.push(Span::styled(
                self.policy.bar.repeat(usize::from(filled)),
                self.styles.bar,
            ));
            spans.push(Span::styled(
                self.policy.empty.repeat(usize::from(empty)),
                self.styles.empty,
            ));
        } else {
            for (index, value) in std::iter::once(item.value)
                .chain(item.group.iter().copied())
                .enumerate()
            {
                if index > 0 && segment_gap > 0 {
                    spans.push(Span::raw(" "));
                }
                let filled = self
                    .filled_width(value, max, segment_width)
                    .min(segment_width);
                let empty = segment_width.saturating_sub(filled);
                spans.push(Span::styled(
                    self.policy.bar.repeat(usize::from(filled)),
                    self.styles.bar,
                ));
                spans.push(Span::styled(
                    self.policy.empty.repeat(usize::from(empty)),
                    self.styles.empty,
                ));
            }
        }
        if matches!(self.policy.value_placement, BarChartValuePlacement::Inside)
            && !value_text.is_empty()
        {
            spans.push(Span::styled(
                format_inside_value(&value_text, bar_width),
                self.styles.value,
            ));
        }
        if right_value {
            spans.push(Span::styled(format!(" {value_text}"), self.styles.value));
        }
        Line::from_spans(spans)
    }
}

fn format_inside_value(value: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    let value = truncate_to_display_width(value, width);
    let left = width.saturating_sub(display_width(&value));
    format!("{}{}", " ".repeat(left), value)
}

fn filled_width(value: u64, max: u64, width: u16) -> u16 {
    if max == 0 || width == 0 {
        return 0;
    }
    u16::try_from((value.min(max).saturating_mul(u64::from(width))) / max).unwrap_or(width)
}

fn format_label(label: &str, width: u16, truncate: bool) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    let label = if truncate && display_width(label) > width {
        truncate_to_display_width(label, width)
    } else {
        label.to_owned()
    };
    let padding = width.saturating_sub(display_width(&label));
    format!("{label}{}", " ".repeat(padding))
}

fn u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`BarChartStyles`].
    #[must_use]
    pub fn bar_chart_styles(self) -> BarChartStyles {
        BarChartStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for BarChartStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            label: theme.text,
            bar: theme.info.add_modifier(bmux_tui::style::Modifier::BOLD),
            empty: theme.muted,
            value: theme.muted,
            empty_message: theme.muted,
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

    use super::{
        BarChart, BarChartComponent, BarChartItem, BarChartPolicy, BarChartValuePlacement,
    };

    #[test]
    fn component_measures_paints_and_registers_chart() {
        let items = [BarChartItem::new("alpha", 5), BarChartItem::new("beta", 10)];
        let component = BarChartComponent::new("usage", &items);
        let layout = component.layout(Constraints::for_width(16), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(16, 2));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("beta   █████████")
        );
        assert_eq!(frame.semantics().regions().len(), 1);
        assert!(
            !frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .is_none()
        );
    }

    #[test]
    fn component_values_are_paint_only() {
        let first = [BarChartItem::new("alpha", 5)];
        let second = [BarChartItem::new("alpha", 10)];
        let initial = BarChartComponent::new("usage", &first).revision();
        let changed = BarChartComponent::new("usage", &second).revision();
        assert_eq!(initial.layout, changed.layout);
        assert_ne!(initial.paint, changed.paint);
    }

    #[test]
    fn scales_bars_against_derived_max() {
        let items = [BarChartItem::new("a", 5), BarChartItem::new("b", 10)];
        let chart = BarChart::new(&items);

        assert_eq!(chart.filled_width(5, 10, 10), 5);
        assert_eq!(chart.filled_width(10, 10, 10), 10);
    }

    #[test]
    fn renders_labels_and_bars() {
        let items = [BarChartItem::new("alpha", 5), BarChartItem::new("beta", 10)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 2));
        let mut frame = Frame::new(&mut buffer);

        BarChart::new(&items).render(Rect::new(0, 0, 16, 2), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("alpha  ████░░░░░")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("beta   █████████")
        );
    }

    #[test]
    fn bar_width_and_gap_are_configurable() {
        let items = [BarChartItem::new("a", 5), BarChartItem::new("b", 10)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        let mut frame = Frame::new(&mut buffer);

        BarChart::new(&items)
            .policy(
                BarChartPolicy::compact()
                    .max(Some(10))
                    .bar_width(Some(4))
                    .bar_gap(1),
            )
            .render(Rect::new(0, 0, 12, 3), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("a      ██░░ ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("            ")
        );
        assert_eq!(
            frame.buffer().row_symbols(2).as_deref(),
            Some("b      ████ ")
        );
    }

    #[test]
    fn renders_inside_value_label() {
        let items = [BarChartItem::new("a", 5)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 17, 1));
        let mut frame = Frame::new(&mut buffer);

        BarChart::new(&items)
            .policy(
                BarChartPolicy::compact()
                    .max(Some(10))
                    .bar_width(Some(5))
                    .value_placement(BarChartValuePlacement::Inside),
            )
            .render(Rect::new(0, 0, 17, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("a      ██░░░    5")
        );
    }

    #[test]
    fn renders_grouped_bars() {
        let group = [5, 10];
        let items = [BarChartItem::new("a", 0).group(&group)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 1));
        let mut frame = Frame::new(&mut buffer);

        BarChart::new(&items)
            .policy(BarChartPolicy::compact().max(Some(10)).bar_width(Some(8)))
            .render(Rect::new(0, 0, 16, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("a      ░░ █░ ██ ")
        );
    }

    #[test]
    fn renders_empty_message() {
        let items = [];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        BarChart::new(&items).render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("No data "));
    }

    #[test]
    fn max_override_clips_values() {
        let items = [BarChartItem::new("a", 20)];
        let chart = BarChart::new(&items).policy(BarChartPolicy::compact().max(Some(10)));

        assert_eq!(chart.filled_width(20, 10, 4), 4);
    }

    #[test]
    fn renders_values_when_enabled() {
        let items = [BarChartItem::new("a", 5)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 1));
        let mut frame = Frame::new(&mut buffer);

        BarChart::new(&items)
            .policy(BarChartPolicy::with_values().max(Some(10)))
            .render(Rect::new(0, 0, 14, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("a      ██░░░ 5")
        );
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let items = [BarChartItem::new("a", 1)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        BarChart::new(&items).render(Rect::new(0, 0, 0, 0), &mut frame);
    }
}
