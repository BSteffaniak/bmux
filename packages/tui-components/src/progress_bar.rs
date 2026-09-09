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

use crate::common::u16_saturating;

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
                Some(u16::try_from(u128::from(clamped) * 100 / u128::from(total)).unwrap_or(100))
            }
            Self::Indeterminate { .. } => None,
        }
    }
}

/// Label placement for [`ProgressBarComponent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressLabelPlacement {
    /// Do not render a label.
    Hidden,
    /// Render label inside the bar.
    Inside,
    /// Render label after the bar when space allows.
    Right,
}

/// Render mode for [`ProgressBarComponent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressBarMode {
    /// Filled bar gauge.
    Bar,
    /// Compact line gauge.
    LineGauge,
}

/// Behavior policy for [`ProgressBarComponent`].
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

/// Visual styles for [`ProgressBarComponent`].
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
        self.policy.label.hash(&mut layout);
        self.policy.mode.hash(&mut layout);
        if !matches!(self.policy.label, ProgressLabelPlacement::Hidden) {
            self.label_width().hash(&mut layout);
        }

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.value.hash(&mut paint);
        self.label.hash(&mut paint);
        self.policy.percentage.hash(&mut paint);
        self.policy.filled.hash(&mut paint);
        self.policy.empty.hash(&mut paint);
        self.policy.partial.hash(&mut paint);
        self.policy.pulse.hash(&mut paint);
        self.policy.pulse_width.hash(&mut paint);
        self.policy.background.hash(&mut layout);
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
        let label_width = self.label_width();
        let intrinsic_width = match self.policy.label {
            ProgressLabelPlacement::Right if label_width > 0 => label_width.saturating_add(2),
            ProgressLabelPlacement::Inside
                if label_width > 0 && matches!(self.policy.mode, ProgressBarMode::LineGauge) =>
            {
                label_width.saturating_add(2)
            }
            ProgressLabelPlacement::Inside => label_width.max(1),
            ProgressLabelPlacement::Hidden | ProgressLabelPlacement::Right => 1,
        };
        let size = constraints.constrain(LogicalSize::new(u16_saturating(intrinsic_width), 0));
        let size = constraints.constrain(LogicalSize::new(size.width, usize::from(size.width > 0)));
        let children = if self.policy.background {
            vec![bmux_tui::component::ChildLayout::new(
                0,
                0,
                bmux_tui::composition::Surface::new(bmux_tui::composition::Stack::new())
                    .background(self.styles.background)
                    .layout(
                        Constraints::new(
                            size.width,
                            size.width,
                            size.height.min(1),
                            Some(size.height.min(1)),
                        ),
                        cx,
                    ),
            )]
        } else {
            Vec::new()
        };
        LayoutNode::with_children(self.id.clone(), size, children)
            .with_metadata(LayoutMetadata::new().semantic("progress"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        if let Some(surface) = layout.children.first() {
            bmux_tui::composition::Surface::new(bmux_tui::composition::Stack::new())
                .background(self.styles.background)
                .paint(&surface.node, cx);
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
        if matches!(self.value, ProgressBarValue::Determinate { .. })
            && matches!(self.policy.mode, ProgressBarMode::Bar)
            && matches!(self.policy.label, ProgressLabelPlacement::Inside)
            && let Some(label) = self.label_text()
        {
            let label = truncate_to_display_width(&label, usize::from(layout.size.width));
            let label_width = u16_saturating(display_width(&label));
            let left = (layout.size.width - label_width) / 2;
            cx.write_line(
                LocalRect::new(i32::from(left), 0, label_width, 1),
                &Line::from_spans([Span::styled(label, self.styles.label)]),
            );
        }
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "progress",
        ));
        cx.push_damage(area);
    }
}

impl ProgressBarComponent<'_> {
    fn label_width(&self) -> usize {
        if matches!(self.policy.label, ProgressLabelPlacement::Hidden) {
            return 0;
        }
        if let Some(label) = self.label {
            return display_width(label);
        }
        if !self.policy.percentage {
            return 0;
        }
        match self.value.percent() {
            Some(0..=9) => 2,
            Some(10..=99) => 3,
            Some(_) => 4,
            None => 0,
        }
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
                u16::try_from(u128::from(clamped) * u128::from(width) / u128::from(total))
                    .unwrap_or(width)
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
                let scaled = u128::from(clamped) * u128::from(width);
                let total = u128::from(total);
                u16::from(scaled % total > 0 && scaled / total < u128::from(width))
            }
            ProgressBarValue::Determinate { .. } | ProgressBarValue::Indeterminate { .. } => 0,
        }
    }

    fn determinate_line(&self, width: u16) -> Line {
        self.gauge_with_label(
            width,
            matches!(self.policy.label, ProgressLabelPlacement::Right),
        )
    }

    fn line_gauge_line(&self, width: u16) -> Line {
        self.gauge_with_label(
            width,
            !matches!(self.policy.label, ProgressLabelPlacement::Hidden),
        )
    }

    fn gauge_with_label(&self, width: u16, adjacent_label: bool) -> Line {
        if !adjacent_label {
            return Line::from_spans(self.gauge_spans(width));
        }
        let label = self.label_text();
        let label_width = label.as_ref().map_or(0, |label| display_width(label));
        let right_label = label_width > 0;
        let gap = u16::from(right_label);
        let gauge_width = if right_label {
            width.saturating_sub(u16_saturating(label_width).saturating_add(gap))
        } else {
            width
        };
        let gap = u16::from(right_label && gauge_width > 0);
        let mut spans = self.gauge_spans(gauge_width);
        if right_label {
            if gap > 0 {
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

    fn gauge_spans(&self, width: u16) -> Vec<Span> {
        let filled = self.filled_width(width);
        let partial = self.partial_width(width);
        let empty = width.saturating_sub(filled).saturating_sub(partial);
        [
            (filled, self.policy.filled, self.filled_style()),
            (partial, self.policy.partial, self.filled_style()),
            (empty, self.policy.empty, self.styles.empty),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, symbol, style)| Span::styled(symbol.repeat(usize::from(count)), style))
        .collect()
    }

    fn indeterminate_line(&self, width: u16, offset: u16) -> Line {
        let pulse_width = self.policy.pulse_width.max(1).min(width.max(1));
        let span = u32::from(width) + u32::from(pulse_width);
        let start = u32::from(offset) % span;
        let end = start.min(u32::from(width));
        let begin = start.saturating_sub(u32::from(pulse_width)).min(end);
        Line::from_spans(
            [
                (begin, self.policy.empty),
                (end - begin, self.policy.pulse),
                (u32::from(width) - end, self.policy.empty),
            ]
            .into_iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, symbol)| {
                Span::styled(
                    symbol.repeat(usize::try_from(count).unwrap_or(0)),
                    self.styles.indeterminate,
                )
            })
            .collect::<Vec<_>>(),
        )
    }

    fn filled_style(&self) -> Style {
        if matches!(self.value.percent(), Some(100)) {
            self.styles.complete
        } else {
            self.styles.filled
        }
    }
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
    fn indeterminate_runs_match_pulse_at_every_cycle_position() {
        for width in 1..=12_u16 {
            for pulse_width in [0, 1, 3, 20] {
                let effective = pulse_width.max(1).min(width);
                for offset in 0..2 * (width + effective) {
                    let component =
                        ProgressBarComponent::new("pulse", ProgressBarValue::indeterminate(offset))
                            .policy(ProgressBarPolicy {
                                pulse: "#",
                                empty: ".",
                                pulse_width,
                                ..ProgressBarPolicy::bare()
                            });
                    let start = offset % (width + effective);
                    let expected: String = (0..width)
                        .map(|x| {
                            if x + effective >= start && x < start {
                                '#'
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    assert_eq!(render(&component, width), expected);
                }
            }
        }
    }

    #[test]
    fn inside_label_and_metadata_share_translated_parent_clip() {
        let component = ProgressBarComponent::new("clipped", ProgressBarValue::ratio(1, 2))
            .policy(ProgressBarPolicy {
                label: ProgressLabelPlacement::Inside,
                ..ProgressBarPolicy::bare()
            })
            .label("DONE");
        let layout = component.layout(Constraints::tight(Size::new(10, 1)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 3));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(
            2,
            1,
            bmux_tui::paint::LocalRect::new(4, 0, 3, 1),
            |cx| component.paint(&layout, cx),
        );
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("              ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("      ONE     ")
        );
        assert_eq!(
            frame.buffer().row_symbols(2).as_deref(),
            Some("              ")
        );
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(6, 1, 3, 1));
        assert_eq!(
            frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .retained_regions(),
            &[Rect::new(6, 1, 3, 1)]
        );
    }

    #[test]
    fn equal_width_label_changes_reuse_layout_and_paint_current_text() {
        let mut cache = bmux_tui::component::LayoutCache::new();
        let mut cx = LayoutCx::new();
        let mut previous = None;
        for (label, width) in [("ab", 4), ("界", 4), ("long", 6)] {
            let component = ProgressBarComponent::new("progress", ProgressBarValue::ratio(1, 1))
                .policy(ProgressBarPolicy::bare().label(ProgressLabelPlacement::Right))
                .label(label);
            let revision = component.revision();
            if let Some(previous) = previous {
                assert_ne!(revision.paint, previous);
            }
            previous = Some(revision.paint);
            let layout = cache.layout(
                "progress".into(),
                &component,
                Constraints::new(0, 20, 0, None),
                &mut cx,
            );
            assert_eq!(layout.size.width, width);
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            assert_eq!(frame.buffer().row_symbols(0), Some(format!("█ {label}")));
            assert_eq!(
                frame.semantics().regions()[0].area,
                Rect::new(0, 0, width, 1)
            );
        }
        assert_eq!(cx.measured_nodes(), 2);
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn indeterminate_runs_cover_the_complete_animation_cycle() {
        for (offset, expected) in [
            (0, "...."),
            (1, "#..."),
            (2, "##.."),
            (3, ".##."),
            (4, "..##"),
            (5, "...#"),
            (6, "...."),
        ] {
            let component =
                ProgressBarComponent::new("pulse", ProgressBarValue::indeterminate(offset)).policy(
                    ProgressBarPolicy {
                        empty: ".",
                        pulse: "#",
                        pulse_width: 2,
                        ..ProgressBarPolicy::bare()
                    },
                );
            assert_eq!(render(&component, 4), expected);
        }
    }

    #[test]
    fn hidden_labels_do_not_reserve_or_paint_label_content() {
        for mode in [
            super::ProgressBarMode::Bar,
            super::ProgressBarMode::LineGauge,
        ] {
            for percentage in [false, true] {
                let component =
                    ProgressBarComponent::new("progress", ProgressBarValue::ratio(1, 2))
                        .policy(ProgressBarPolicy {
                            percentage,
                            mode,
                            ..ProgressBarPolicy::bare()
                        })
                        .label("界 hidden label");
                let layout =
                    component.layout(Constraints::new(0, 20, 0, None), &mut LayoutCx::new());
                assert_eq!(layout.size.width, 1);
                assert_eq!(render(&component, 4), "██░░");
            }
        }
    }

    #[test]
    fn percentage_measurement_matches_rendered_label_width() {
        for value in 0..=110 {
            let component =
                ProgressBarComponent::new("progress", ProgressBarValue::ratio(value, 100)).policy(
                    ProgressBarPolicy {
                        percentage: true,
                        label: ProgressLabelPlacement::Right,
                        ..ProgressBarPolicy::bare()
                    },
                );
            let label = component.label_text().unwrap();
            let layout = component.layout(Constraints::new(0, 20, 0, None), &mut LayoutCx::new());
            assert_eq!(usize::from(layout.size.width), label.len() + 2);
        }
    }

    #[test]
    fn symbol_changes_reuse_measurement_but_invalidate_paint() {
        let original = ProgressBarComponent::new("progress", ProgressBarValue::ratio(1, 2))
            .policy(ProgressBarPolicy::bare());
        let changed = ProgressBarComponent::new("progress", ProgressBarValue::ratio(1, 2)).policy(
            ProgressBarPolicy::bare()
                .symbols("#", ".", "+")
                .pulse_symbol("*"),
        );
        assert_eq!(original.revision().layout, changed.revision().layout);
        assert_ne!(original.revision().paint, changed.revision().paint);
        let mut cache = bmux_tui::component::LayoutCache::new();
        let mut cx = LayoutCx::new();
        let constraints = Constraints::new(4, 4, 1, Some(1));
        cache.layout("progress".into(), &original, constraints, &mut cx);
        let layout = cache.layout("progress".into(), &changed, constraints, &mut cx);
        assert_eq!(cx.measured_nodes(), 1);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(render(&changed, layout.size.width), "##..");
    }

    #[test]
    fn measurement_respects_width_and_height_constraints() {
        let component = ProgressBarComponent::new("progress", ProgressBarValue::ratio(1, 2))
            .policy(ProgressBarPolicy::bare());
        for (constraints, width, height) in [
            (Constraints::new(0, 20, 0, None), 1, 1),
            (Constraints::new(5, 20, 0, None), 5, 1),
            (Constraints::new(8, 8, 0, None), 8, 1),
            (Constraints::new(0, 0, 0, None), 0, 0),
            (Constraints::new(0, 20, 0, Some(0)), 1, 0),
            (Constraints::new(0, 20, 3, Some(3)), 1, 3),
        ] {
            let layout = component.layout(constraints, &mut LayoutCx::new());
            assert_eq!(layout.size.width, width);
            assert_eq!(layout.size.height, height);
        }
    }

    #[test]
    fn adjacent_unicode_label_is_clipped_equally_in_both_modes() {
        for mode in [
            super::ProgressBarMode::Bar,
            super::ProgressBarMode::LineGauge,
        ] {
            let component = ProgressBarComponent::new("label", ProgressBarValue::ratio(1, 1))
                .policy(ProgressBarPolicy {
                    mode,
                    label: ProgressLabelPlacement::Right,
                    ..ProgressBarPolicy::bare()
                })
                .label("界ab");
            assert_eq!(render(&component, 3), "界…");
            assert_eq!(render(&component, 6), "█ 界ab");
        }
    }

    #[test]
    fn label_only_width_has_no_leading_gauge_gap() {
        for mode in [
            super::ProgressBarMode::Bar,
            super::ProgressBarMode::LineGauge,
        ] {
            let component = ProgressBarComponent::new("label", ProgressBarValue::ratio(1, 1))
                .policy(ProgressBarPolicy {
                    mode,
                    label: ProgressLabelPlacement::Right,
                    ..ProgressBarPolicy::bare()
                })
                .label("done");
            assert_eq!(render(&component, 4), "done");
            assert_eq!(render(&component, 6), "█ done");
        }
    }

    #[test]
    fn line_gauge_inside_label_reserves_adjacent_label_space() {
        let component = ProgressBarComponent::new("gauge", ProgressBarValue::ratio(1, 1))
            .policy(ProgressBarPolicy {
                mode: super::ProgressBarMode::LineGauge,
                label: ProgressLabelPlacement::Inside,
                ..ProgressBarPolicy::bare()
            })
            .label("done");
        let layout = component.layout(Constraints::loose(Size::new(20, 1)), &mut LayoutCx::new());
        assert_eq!(layout.size.width, 6);
        assert_eq!(render(&component, layout.size.width), "█ done");
    }

    #[test]
    fn indeterminate_cycle_does_not_saturate_at_maximum_width() {
        let component =
            ProgressBarComponent::new("pulse", ProgressBarValue::indeterminate(u16::MAX)).policy(
                ProgressBarPolicy {
                    pulse: "#",
                    empty: ".",
                    pulse_width: 3,
                    ..ProgressBarPolicy::bare()
                },
            );
        let painted = render(&component, u16::MAX);
        assert_eq!(painted.len(), usize::from(u16::MAX));
        assert!(painted.ends_with("###"));
        assert!(painted[..painted.len() - 3].chars().all(|cell| cell == '.'));
        let wrapped = ProgressBarComponent::new("pulse", ProgressBarValue::indeterminate(13))
            .policy(ProgressBarPolicy {
                pulse: "#",
                empty: ".",
                pulse_width: 3,
                ..ProgressBarPolicy::bare()
            });
        assert_eq!(render(&wrapped, 10), "..........");
    }

    #[test]
    fn inside_label_overlays_bar_instead_of_appending() {
        let component = ProgressBarComponent::new("progress", ProgressBarValue::ratio(1, 2))
            .policy(ProgressBarPolicy {
                label: ProgressLabelPlacement::Inside,
                ..ProgressBarPolicy::bare()
            })
            .label("界");
        assert_eq!(render(&component, 10), "████界░░░░");
    }

    #[test]
    fn large_values_preserve_exact_progress_ratios() {
        for (value, total, percent, filled, partial) in [
            (u64::MAX, u64::MAX, 100, 10, 0),
            (u64::MAX, u64::MAX - 1, 100, 10, 0),
            (u64::MAX / 2, u64::MAX - 1, 50, 5, 0),
            (u64::MAX - 1, u64::MAX, 99, 9, 1),
            (0, u64::MAX, 0, 0, 0),
            (u64::MAX, 0, 0, 0, 0),
        ] {
            let progress = ProgressBarValue::determinate(value, total);
            let component =
                ProgressBarComponent::new("large", progress).policy(ProgressBarPolicy::bare());
            assert_eq!(progress.percent(), Some(percent));
            assert_eq!(component.filled_width(10), filled);
            assert_eq!(component.partial_width(10), partial);
            assert_eq!(component.filled_width(0), 0);
            assert_eq!(component.partial_width(0), 0);
        }
        let complete = ProgressBarComponent::new(
            "complete",
            ProgressBarValue::determinate(u64::MAX, u64::MAX),
        )
        .policy(ProgressBarPolicy::bare());
        assert_eq!(complete.filled_width(u16::MAX), u16::MAX);
        assert_eq!(complete.partial_width(u16::MAX), 0);
        assert_eq!(render(&complete, 10), "██████████");
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
    fn same_width_value_changes_are_paint_only() {
        let initial =
            ProgressBarComponent::new("download", ProgressBarValue::determinate(1, 2)).revision();
        let changed =
            ProgressBarComponent::new("download", ProgressBarValue::determinate(3, 4)).revision();
        assert_eq!(initial.layout, changed.layout);
        assert_ne!(initial.paint, changed.paint);
    }

    #[test]
    fn percentage_width_changes_invalidate_cached_layout() {
        let mut cache = bmux_tui::component::LayoutCache::new();
        let mut cx = LayoutCx::new();
        let constraints = Constraints::loose(Size::new(20, 1));
        for placement in [
            ProgressLabelPlacement::Inside,
            ProgressLabelPlacement::Right,
        ] {
            for value in [9, 10, 100, 0] {
                let component =
                    ProgressBarComponent::new("progress", ProgressBarValue::ratio(value, 100))
                        .policy(ProgressBarPolicy {
                            label: placement,
                            percentage: true,
                            ..ProgressBarPolicy::bare()
                        });
                let cached = cache.layout("progress".into(), &component, constraints, &mut cx);
                let fresh = component.layout(constraints, &mut LayoutCx::new());
                assert_eq!(cached.size, fresh.size);
            }
        }
        // Returning to a two-column label reuses the earlier layout for each placement.
        assert_eq!(cx.measured_nodes(), 6);
        assert_eq!(cache.stats().hits, 2);
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
        assert_eq!(render(&inside, 8), "loading ");

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
