//! Generic progress bar / gauge component.

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
use bmux_tui::text_width::{display_width, truncate_to_display_width};

/// Progress value model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressBarValue {
    /// Determinate progress with current value and total value.
    Determinate { value: u64, total: u64 },
    /// Indeterminate progress. `offset` is caller-owned animation state.
    Indeterminate { offset: u16 },
}

impl ProgressBarValue {
    /// Create determinate progress.
    #[must_use]
    pub const fn determinate(value: u64, total: u64) -> Self {
        Self::Determinate { value, total }
    }

    /// Create determinate progress from a ratio numerator and denominator.
    #[must_use]
    pub const fn ratio(numerator: u64, denominator: u64) -> Self {
        Self::Determinate {
            value: numerator,
            total: denominator,
        }
    }

    /// Create indeterminate progress with caller-provided animation offset.
    #[must_use]
    pub const fn indeterminate(offset: u16) -> Self {
        Self::Indeterminate { offset }
    }

    /// Return clamped percentage for determinate progress.
    #[must_use]
    pub fn percent(self) -> Option<u16> {
        match self {
            Self::Determinate { value: _, total: 0 } => Some(0),
            Self::Determinate { value, total } => {
                let clamped = if value > total { total } else { value };
                Some(u16::try_from((clamped.saturating_mul(100)) / total).unwrap_or(100))
            }
            Self::Indeterminate { .. } => None,
        }
    }
}

/// Label placement for [`ProgressBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressLabelPlacement {
    /// Do not render a label.
    Hidden,
    /// Render label inside the bar.
    Inside,
    /// Render label after the bar when space allows.
    Right,
}

/// Render mode for [`ProgressBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressBarMode {
    /// Filled bar gauge.
    Bar,
    /// Compact line gauge.
    LineGauge,
}

/// Behavior policy for [`ProgressBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressBarPolicy {
    /// Filled cell symbol.
    pub filled: &'static str,
    /// Empty cell symbol.
    pub empty: &'static str,
    /// Partial determinate cell symbol used when progress falls between cells.
    pub partial: &'static str,
    /// Indeterminate pulse symbol.
    pub pulse: &'static str,
    /// Width of the indeterminate pulse in cells.
    pub pulse_width: u16,
    /// Label placement.
    pub label: ProgressLabelPlacement,
    /// Render percentage for determinate progress when no explicit label is supplied.
    pub percentage: bool,
    /// Fill row background before rendering.
    pub background: bool,
    /// Progress render mode.
    pub mode: ProgressBarMode,
}

impl ProgressBarPolicy {
    /// Compact default progress bar policy.
    #[must_use]
    pub const fn compact() -> Self {
        Self {
            filled: "█",
            empty: "░",
            partial: "▒",
            pulse: "█",
            pulse_width: 3,
            label: ProgressLabelPlacement::Inside,
            percentage: true,
            background: false,
            mode: ProgressBarMode::Bar,
        }
    }

    /// Bare progress bar with no label/background.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            filled: "█",
            empty: "░",
            partial: "▒",
            pulse: "█",
            pulse_width: 3,
            label: ProgressLabelPlacement::Hidden,
            percentage: false,
            background: false,
            mode: ProgressBarMode::Bar,
        }
    }

    /// Return this policy with label placement changed.
    #[must_use]
    pub const fn label(mut self, label: ProgressLabelPlacement) -> Self {
        self.label = label;
        self
    }

    /// Return this policy with background fill changed.
    #[must_use]
    pub const fn background(mut self, background: bool) -> Self {
        self.background = background;
        self
    }
    /// Return this policy with determinate bar symbols changed.
    #[must_use]
    pub const fn symbols(
        mut self,
        filled: &'static str,
        empty: &'static str,
        partial: &'static str,
    ) -> Self {
        self.filled = filled;
        self.empty = empty;
        self.partial = partial;
        self
    }

    /// Return this policy with indeterminate pulse symbol changed.
    #[must_use]
    pub const fn pulse_symbol(mut self, pulse: &'static str) -> Self {
        self.pulse = pulse;
        self
    }

    /// Return this policy with render mode changed.
    #[must_use]
    pub const fn mode(mut self, mode: ProgressBarMode) -> Self {
        self.mode = mode;
        self
    }

    /// Return this policy configured for line-gauge rendering.
    #[must_use]
    pub const fn line_gauge(mut self) -> Self {
        self.mode = ProgressBarMode::LineGauge;
        self.label = ProgressLabelPlacement::Right;
        self
    }
}

impl Default for ProgressBarPolicy {
    fn default() -> Self {
        Self::compact()
    }
}

/// Visual styles for [`ProgressBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressBarStyles {
    /// Filled portion style.
    pub filled: Style,
    /// Empty portion style.
    pub empty: Style,
    /// Label style.
    pub label: Style,
    /// Complete progress style.
    pub complete: Style,
    /// Indeterminate pulse style.
    pub indeterminate: Style,
    /// Background fill style.
    pub background: Style,
}

impl Default for ProgressBarStyles {
    fn default() -> Self {
        Self {
            filled: Style::new().fg(Color::Green),
            empty: Style::new().fg(Color::BrightBlack),
            label: Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
            complete: Style::new()
                .fg(Color::BrightGreen)
                .add_modifier(Modifier::BOLD),
            indeterminate: Style::new().fg(Color::Cyan),
            background: Style::new(),
        }
    }
}

/// Canonical component-lifecycle progress bar.
pub struct ProgressBarComponent<'a> {
    id: LayoutId,
    value: ProgressBarValue,
    label: Option<&'a str>,
    policy: ProgressBarPolicy,
    styles: ProgressBarStyles,
}

impl<'a> ProgressBarComponent<'a> {
    /// Create a progress-bar component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, value: ProgressBarValue) -> Self {
        Self {
            id: id.into(),
            value,
            label: None,
            policy: ProgressBarPolicy::compact(),
            styles: ProgressBarStyles {
                filled: Style::new(),
                empty: Style::new(),
                label: Style::new(),
                complete: Style::new(),
                indeterminate: Style::new(),
                background: Style::new(),
            },
        }
    }

    /// Set explicit label.
    #[must_use]
    pub const fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: ProgressBarPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: ProgressBarStyles) -> Self {
        self.styles = styles;
        self
    }
}

impl Component for ProgressBarComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.label.hash(&mut layout);
        self.policy.label.hash(&mut layout);
        self.policy.percentage.hash(&mut layout);
        self.policy.mode.hash(&mut layout);
        self.policy.filled.hash(&mut layout);
        self.policy.empty.hash(&mut layout);
        self.policy.partial.hash(&mut layout);
        self.policy.pulse.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.value.hash(&mut paint);
        self.policy.pulse_width.hash(&mut paint);
        self.policy.background.hash(&mut paint);
        self.styles.filled.hash(&mut paint);
        self.styles.empty.hash(&mut paint);
        self.styles.label.hash(&mut paint);
        self.styles.complete.hash(&mut paint);
        self.styles.indeterminate.hash(&mut paint);
        self.styles.background.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let label_width = self.label_text().as_deref().map_or(0, display_width);
        let intrinsic_width = match self.policy.label {
            ProgressLabelPlacement::Right if label_width > 0 => label_width.saturating_add(2),
            ProgressLabelPlacement::Inside => label_width.max(1),
            ProgressLabelPlacement::Hidden | ProgressLabelPlacement::Right => 1,
        };
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            u16::try_from(intrinsic_width)
                .unwrap_or(u16::MAX)
                .clamp(constraints.min_width(), constraints.max_width())
        };
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, usize::from(width > 0))),
        )
        .with_metadata(LayoutMetadata::new().semantic("progress"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        if self.policy.background {
            cx.fill(area, " ", self.styles.background);
        }
        let line = match self.value {
            ProgressBarValue::Determinate { .. }
                if matches!(self.policy.mode, ProgressBarMode::LineGauge) =>
            {
                self.line_gauge_line(layout.size.width)
            }
            ProgressBarValue::Determinate { .. } => self.determinate_line(layout.size.width),
            ProgressBarValue::Indeterminate { offset } => {
                self.indeterminate_line(layout.size.width, offset)
            }
        };
        cx.write_line(area, &line);
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "progress",
        ));
        cx.push_damage(area);
    }
}

impl ProgressBarComponent<'_> {
    /// Return rendered label text, if any.
    #[must_use]
    pub fn label_text(&self) -> Option<String> {
        self.label.map(str::to_owned).or_else(|| {
            (self.policy.percentage)
                .then(|| self.value.percent().map(|percent| format!("{percent}%")))
                .flatten()
        })
    }

    /// Return determinate filled cell count for `width`.
    #[must_use]
    pub fn filled_width(&self, width: u16) -> u16 {
        match self.value {
            ProgressBarValue::Determinate { value, total } if total > 0 => {
                let clamped = if value > total { total } else { value };
                u16::try_from((clamped.saturating_mul(u64::from(width))) / total).unwrap_or(width)
            }
            ProgressBarValue::Determinate { .. } | ProgressBarValue::Indeterminate { .. } => 0,
        }
    }

    /// Return determinate partial cell count for `width`.
    #[must_use]
    pub fn partial_width(&self, width: u16) -> u16 {
        match self.value {
            ProgressBarValue::Determinate { value, total } if total > 0 && width > 0 => {
                let clamped = value.min(total);
                let scaled = clamped.saturating_mul(u64::from(width));
                u16::from(scaled % total > 0 && scaled / total < u64::from(width))
            }
            ProgressBarValue::Determinate { .. } | ProgressBarValue::Indeterminate { .. } => 0,
        }
    }

    fn determinate_line(&self, width: u16) -> Line {
        let label = self.label_text();
        let label_width = label.as_ref().map_or(0, |label| display_width(label));
        let right_label =
            matches!(self.policy.label, ProgressLabelPlacement::Right) && label_width > 0;
        let gap = u16::from(right_label);
        let bar_width = if right_label {
            width.saturating_sub(u16_saturating(label_width).saturating_add(gap))
        } else {
            width
        };
        let filled_width = self.filled_width(bar_width).min(bar_width);
        let partial_width = self.partial_width(bar_width);
        let mut spans = Vec::new();
        if filled_width > 0 {
            spans.push(Span::styled(
                self.policy.filled.repeat(usize::from(filled_width)),
                self.filled_style(),
            ));
        }
        if partial_width > 0 {
            spans.push(Span::styled(
                self.policy.partial.repeat(usize::from(partial_width)),
                self.filled_style(),
            ));
        }
        let empty_width = bar_width
            .saturating_sub(filled_width)
            .saturating_sub(partial_width);
        if empty_width > 0 {
            spans.push(Span::styled(
                self.policy.empty.repeat(usize::from(empty_width)),
                self.styles.empty,
            ));
        }
        if right_label {
            if bar_width < width {
                spans.push(Span::raw(" "));
            }
            if let Some(label) = &label {
                spans.push(Span::styled(label.clone(), self.styles.label));
            }
        }
        if matches!(self.policy.label, ProgressLabelPlacement::Inside)
            && !matches!(self.policy.mode, ProgressBarMode::LineGauge)
        {
            overlay_label(
                Line::from_spans(spans),
                label.as_deref(),
                self.styles.label,
                usize::from(width),
            )
        } else {
            Line::from_spans(spans)
        }
    }

    fn line_gauge_line(&self, width: u16) -> Line {
        let label = self.label_text();
        let label_width = label.as_ref().map_or(0, |label| display_width(label));
        let right_label =
            label_width > 0 && !matches!(self.policy.label, ProgressLabelPlacement::Hidden);
        let gap = u16::from(right_label);
        let gauge_width = if right_label {
            width.saturating_sub(u16_saturating(label_width).saturating_add(gap))
        } else {
            width
        };
        let filled_width = self.filled_width(gauge_width).min(gauge_width);
        let partial_width = self.partial_width(gauge_width);
        let mut spans = Vec::new();
        if filled_width > 0 {
            spans.push(Span::styled(
                self.policy.filled.repeat(usize::from(filled_width)),
                self.filled_style(),
            ));
        }
        if partial_width > 0 {
            spans.push(Span::styled(
                self.policy.partial.repeat(usize::from(partial_width)),
                self.filled_style(),
            ));
        }
        let empty_width = gauge_width
            .saturating_sub(filled_width)
            .saturating_sub(partial_width);
        if empty_width > 0 {
            spans.push(Span::styled(
                self.policy.empty.repeat(usize::from(empty_width)),
                self.styles.empty,
            ));
        }
        if right_label {
            if gauge_width < width {
                spans.push(Span::raw(" "));
            }
            if let Some(label) = label {
                spans.push(Span::styled(
                    truncate_to_display_width(
                        &label,
                        usize::from(width.saturating_sub(gauge_width).saturating_sub(gap)),
                    ),
                    self.styles.label,
                ));
            }
        }
        Line::from_spans(spans)
    }

    fn indeterminate_line(&self, width: u16, offset: u16) -> Line {
        let pulse_width = self.policy.pulse_width.max(1).min(width.max(1));
        let span = width.saturating_add(pulse_width);
        let start = if span == 0 { 0 } else { offset % span };
        let mut cells = String::new();
        for x in 0..width {
            let in_pulse = x.saturating_add(pulse_width) >= start && x < start;
            cells.push_str(if in_pulse {
                self.policy.pulse
            } else {
                self.policy.empty
            });
        }
        Line::from_spans([Span::styled(cells, self.styles.indeterminate)])
    }

    fn filled_style(&self) -> Style {
        if matches!(self.value.percent(), Some(100)) {
            self.styles.complete
        } else {
            self.styles.filled
        }
    }
}

fn overlay_label(mut line: Line, label: Option<&str>, style: Style, width: usize) -> Line {
    let Some(label) = label else {
        return line;
    };
    if width == 0 {
        return line;
    }
    let label = truncate_to_display_width(label, width);
    let label_width = display_width(&label);
    let left = width.saturating_sub(label_width) / 2;
    let text = format!("{}{}", " ".repeat(left), label);
    line.spans.push(Span::styled(text, style));
    line
}

fn u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`ProgressBarStyles`].
    #[must_use]
    pub fn progress_bar_styles(self) -> ProgressBarStyles {
        ProgressBarStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for ProgressBarStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            filled: theme.success,
            empty: theme.muted,
            label: theme.text.add_modifier(bmux_tui::style::Modifier::BOLD),
            complete: theme.success.add_modifier(bmux_tui::style::Modifier::BOLD),
            indeterminate: theme.info,
            background: theme.surfaces.normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx, LogicalSize};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Rect, Size};
    use bmux_tui::paint::PaintCx;

    use super::{
        ProgressBarComponent, ProgressBarPolicy, ProgressBarValue, ProgressLabelPlacement,
    };

    fn render(component: &ProgressBarComponent<'_>, width: u16) -> String {
        let layout = component.layout(
            Constraints::tight(Size::new(width, 1)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        frame.buffer().row_symbols(0).unwrap_or_default()
    }

    #[test]
    fn component_measures_paints_and_registers_progress() {
        let component = ProgressBarComponent::new("download", ProgressBarValue::determinate(1, 2))
            .policy(ProgressBarPolicy::bare());
        let layout = component.layout(Constraints::for_width(10), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(10, 1));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("█████░░░░░"));
        assert_eq!(frame.semantics().regions().len(), 1);
        assert!(
            !frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .is_none()
        );
    }

    #[test]
    fn component_value_changes_are_paint_only() {
        let initial =
            ProgressBarComponent::new("download", ProgressBarValue::determinate(1, 2)).revision();
        let changed =
            ProgressBarComponent::new("download", ProgressBarValue::determinate(2, 2)).revision();
        assert_eq!(initial.layout, changed.layout);
        assert_ne!(initial.paint, changed.paint);
    }

    #[test]
    fn computes_percent_and_fill_geometry() {
        assert_eq!(ProgressBarValue::ratio(3, 4).percent(), Some(75));
        let bar = ProgressBarComponent::new("progress", ProgressBarValue::determinate(3, 10));
        assert_eq!(bar.filled_width(10), 3);
        assert_eq!(bar.partial_width(10), 0);
        let clamped = ProgressBarComponent::new("progress", ProgressBarValue::determinate(12, 10));
        assert_eq!(clamped.filled_width(10), 10);
        assert_eq!(clamped.partial_width(10), 0);
        let zero = ProgressBarComponent::new("progress", ProgressBarValue::determinate(1, 0));
        assert_eq!(zero.filled_width(10), 0);
        assert_eq!(zero.label_text().as_deref(), Some("0%"));
    }

    #[test]
    fn canonical_paint_supports_symbols_and_line_gauge() {
        let symbols = ProgressBarComponent::new("symbols", ProgressBarValue::ratio(1, 3))
            .policy(ProgressBarPolicy::bare().symbols("=", ".", ">"));
        assert_eq!(render(&symbols, 7), "==>....");

        let gauge = ProgressBarComponent::new("gauge", ProgressBarValue::ratio(1, 4))
            .policy(ProgressBarPolicy::compact().line_gauge());
        assert_eq!(render(&gauge, 10), "█▒░░░░ 25%");
    }

    #[test]
    fn canonical_paint_supports_inside_and_right_labels() {
        let inside = ProgressBarComponent::new("inside", ProgressBarValue::ratio(1, 2))
            .policy(ProgressBarPolicy::compact().line_gauge())
            .label("loading");
        assert_eq!(render(&inside, 8), " loading");

        let right = ProgressBarComponent::new("right", ProgressBarValue::ratio(1, 2))
            .policy(ProgressBarPolicy::compact().label(ProgressLabelPlacement::Right));
        assert_eq!(render(&right, 12), "████░░░░ 50%");
    }

    #[test]
    fn canonical_paint_handles_tiny_and_indeterminate_bars() {
        let tiny = ProgressBarComponent::new("tiny", ProgressBarValue::ratio(1, 2));
        assert_eq!(render(&tiny, 1).chars().count(), 1);

        let indeterminate =
            ProgressBarComponent::new("pending", ProgressBarValue::indeterminate(3))
                .policy(ProgressBarPolicy::bare());
        assert_eq!(render(&indeterminate, 8).chars().count(), 8);
    }
}
