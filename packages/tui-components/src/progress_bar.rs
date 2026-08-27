//! Generic progress bar / gauge component.

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
    bar: ProgressBar<'a>,
}

impl<'a> ProgressBarComponent<'a> {
    /// Create a progress-bar component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, value: ProgressBarValue) -> Self {
        Self {
            id: id.into(),
            bar: ProgressBar::new(value),
        }
    }

    /// Set explicit label.
    #[must_use]
    pub const fn label(mut self, label: &'a str) -> Self {
        self.bar.label = Some(label);
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: ProgressBarPolicy) -> Self {
        self.bar.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: ProgressBarStyles) -> Self {
        self.bar.styles = styles;
        self
    }
}

impl Component for ProgressBarComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.bar.label.hash(&mut layout);
        self.bar.policy.label.hash(&mut layout);
        self.bar.policy.percentage.hash(&mut layout);
        self.bar.policy.mode.hash(&mut layout);
        self.bar.policy.filled.hash(&mut layout);
        self.bar.policy.empty.hash(&mut layout);
        self.bar.policy.partial.hash(&mut layout);
        self.bar.policy.pulse.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.bar.value.hash(&mut paint);
        self.bar.policy.pulse_width.hash(&mut paint);
        self.bar.policy.background.hash(&mut paint);
        self.bar.styles.filled.hash(&mut paint);
        self.bar.styles.empty.hash(&mut paint);
        self.bar.styles.label.hash(&mut paint);
        self.bar.styles.complete.hash(&mut paint);
        self.bar.styles.indeterminate.hash(&mut paint);
        self.bar.styles.background.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let label_width = self.bar.label_text().as_deref().map_or(0, display_width);
        let intrinsic_width = match self.bar.policy.label {
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
        if self.bar.policy.background {
            cx.fill(area, " ", self.bar.styles.background);
        }
        let line = match self.bar.value {
            ProgressBarValue::Determinate { .. }
                if matches!(self.bar.policy.mode, ProgressBarMode::LineGauge) =>
            {
                self.bar.line_gauge_line(layout.size.width)
            }
            ProgressBarValue::Determinate { .. } => self.bar.determinate_line(layout.size.width),
            ProgressBarValue::Indeterminate { offset } => {
                self.bar.indeterminate_line(layout.size.width, offset)
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

/// Generic progress bar / gauge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressBar<'a> {
    value: ProgressBarValue,
    label: Option<&'a str>,
    policy: ProgressBarPolicy,
    styles: ProgressBarStyles,
}

impl<'a> ProgressBar<'a> {
    /// Create a progress bar.
    #[must_use]
    pub const fn new(value: ProgressBarValue) -> Self {
        Self {
            value,
            label: None,
            policy: ProgressBarPolicy {
                filled: "█",
                empty: "░",
                partial: "▒",
                pulse: "█",
                pulse_width: 3,
                label: ProgressLabelPlacement::Inside,
                percentage: true,
                background: false,
                mode: ProgressBarMode::Bar,
            },
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

    /// Create a progress bar from a ratio numerator and denominator.
    #[must_use]
    pub const fn ratio(numerator: u64, denominator: u64) -> Self {
        Self::new(ProgressBarValue::ratio(numerator, denominator))
    }

    /// Create a compact line-gauge style progress bar from a ratio.
    #[must_use]
    pub const fn line_gauge(numerator: u64, denominator: u64) -> Self {
        Self::ratio(numerator, denominator).policy(ProgressBarPolicy::compact().line_gauge())
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

    /// Render progress bar.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if self.policy.background {
            frame.fill(area, " ", self.styles.background);
        }
        match self.value {
            ProgressBarValue::Determinate { .. }
                if matches!(self.policy.mode, ProgressBarMode::LineGauge) =>
            {
                self.render_line_gauge(area, frame);
            }
            ProgressBarValue::Determinate { .. } => self.render_determinate(area, frame),
            ProgressBarValue::Indeterminate { offset } => {
                self.render_indeterminate(area, offset, frame);
            }
        }
    }

    fn render_determinate(&self, area: Rect, frame: &mut Frame<'_>) {
        let label = self.label_text();
        let label_width = label.as_ref().map_or(0, |label| display_width(label));
        let right_label =
            matches!(self.policy.label, ProgressLabelPlacement::Right) && label_width > 0;
        let gap = u16::from(right_label);
        let bar_width = if right_label {
            area.width
                .saturating_sub(u16_saturating(label_width).saturating_add(gap))
        } else {
            area.width
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
            if bar_width < area.width {
                spans.push(Span::raw(" "));
            }
            if let Some(label) = &label {
                spans.push(Span::styled(label.clone(), self.styles.label));
            }
        }
        let line = if matches!(self.policy.label, ProgressLabelPlacement::Inside)
            && !matches!(self.policy.mode, ProgressBarMode::LineGauge)
        {
            overlay_label(
                Line::from_spans(spans),
                label.as_deref(),
                self.styles.label,
                usize::from(area.width),
            )
        } else {
            Line::from_spans(spans)
        };
        frame.write_line(area, &line);
    }

    fn render_line_gauge(&self, area: Rect, frame: &mut Frame<'_>) {
        let label = self.label_text();
        let label_width = label.as_ref().map_or(0, |label| display_width(label));
        let right_label =
            label_width > 0 && !matches!(self.policy.label, ProgressLabelPlacement::Hidden);
        let gap = u16::from(right_label);
        let gauge_width = if right_label {
            area.width
                .saturating_sub(u16_saturating(label_width).saturating_add(gap))
        } else {
            area.width
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
            if gauge_width < area.width {
                spans.push(Span::raw(" "));
            }
            if let Some(label) = label {
                spans.push(Span::styled(
                    truncate_to_display_width(
                        &label,
                        usize::from(area.width.saturating_sub(gauge_width).saturating_sub(gap)),
                    ),
                    self.styles.label,
                ));
            }
        }
        frame.write_line(area, &Line::from_spans(spans));
    }

    fn render_indeterminate(&self, area: Rect, offset: u16, frame: &mut Frame<'_>) {
        let width = area.width;
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
        frame.write_line_with_fallback_style(area, &Line::from(cells), self.styles.indeterminate);
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
    use bmux_tui::geometry::Rect;
    use bmux_tui::paint::PaintCx;

    use super::{
        ProgressBar, ProgressBarComponent, ProgressBarPolicy, ProgressBarValue,
        ProgressLabelPlacement,
    };

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
    fn ratio_constructor_builds_determinate_progress() {
        let bar = ProgressBar::ratio(3, 4);

        assert_eq!(bar.label_text().as_deref(), Some("75%"));
        assert_eq!(bar.filled_width(8), 6);
    }

    #[test]
    fn computes_determinate_percent_and_fill_width() {
        let bar = ProgressBar::new(ProgressBarValue::determinate(3, 10));

        assert_eq!(bar.label_text().as_deref(), Some("30%"));
        assert_eq!(bar.filled_width(20), 6);
    }

    #[test]
    fn clamps_over_total_to_complete() {
        let bar = ProgressBar::new(ProgressBarValue::determinate(12, 10));

        assert_eq!(bar.label_text().as_deref(), Some("100%"));
        assert_eq!(bar.filled_width(8), 8);
    }

    #[test]
    fn zero_total_renders_as_zero_percent() {
        let bar = ProgressBar::new(ProgressBarValue::determinate(1, 0));

        assert_eq!(bar.label_text().as_deref(), Some("0%"));
        assert_eq!(bar.filled_width(8), 0);
    }

    #[test]
    fn custom_symbols_render_partial_segment() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);

        ProgressBar::ratio(1, 3)
            .policy(ProgressBarPolicy::bare().symbols("=", ".", ">"))
            .render(Rect::new(0, 0, 4, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("=>.."));
    }

    #[test]
    fn renders_bare_determinate_bar() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut frame = Frame::new(&mut buffer);

        ProgressBar::new(ProgressBarValue::determinate(1, 2))
            .policy(ProgressBarPolicy::bare())
            .render(Rect::new(0, 0, 10, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("█████░░░░░"));
    }

    #[test]
    fn renders_line_gauge_mode() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut frame = Frame::new(&mut buffer);

        ProgressBar::new(ProgressBarValue::determinate(1, 2))
            .policy(ProgressBarPolicy::compact().line_gauge())
            .render(Rect::new(0, 0, 12, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("████░░░░ 50%")
        );
    }

    #[test]
    fn line_gauge_constructor_renders_compact_gauge() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut frame = Frame::new(&mut buffer);

        ProgressBar::line_gauge(1, 4).render(Rect::new(0, 0, 10, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("█▒░░░░ 25%"));
    }

    #[test]
    fn line_gauge_truncates_long_right_label() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        ProgressBar::line_gauge(1, 2)
            .label("loading")
            .render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(" loading"));
    }

    #[test]
    fn renders_right_label() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut frame = Frame::new(&mut buffer);

        ProgressBar::new(ProgressBarValue::determinate(1, 2))
            .policy(ProgressBarPolicy::compact().label(ProgressLabelPlacement::Right))
            .render(Rect::new(0, 0, 12, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("████░░░░ 50%")
        );
    }

    #[test]
    fn explicit_label_replaces_percentage() {
        let bar = ProgressBar::new(ProgressBarValue::determinate(1, 2)).label("loading");

        assert_eq!(bar.label_text().as_deref(), Some("loading"));
    }

    #[test]
    fn tiny_width_does_not_panic() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut frame = Frame::new(&mut buffer);

        ProgressBar::new(ProgressBarValue::determinate(1, 2))
            .render(Rect::new(0, 0, 1, 1), &mut frame);

        assert!(frame.buffer().row_symbols(0).is_some());
    }

    #[test]
    fn renders_indeterminate_bar() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        ProgressBar::new(ProgressBarValue::indeterminate(3))
            .policy(ProgressBarPolicy::bare())
            .render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("███░░░░░"));
    }
}
