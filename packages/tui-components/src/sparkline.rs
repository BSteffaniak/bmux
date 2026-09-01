//! Compact sparkline/trend visualization component.

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

/// Sparkline render direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SparklineDirection {
    /// Render samples from oldest to newest, left to right.
    LeftToRight,
    /// Render samples from newest to oldest, left to right.
    RightToLeft,
}

/// Sparkline rendering policy.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparklinePolicy {
    /// Optional maximum sample value. When absent, the maximum is derived from samples.
    pub max: Option<u64>,
    /// Render only the latest `window` samples when set.
    pub window: Option<usize>,
    /// Render direction.
    pub direction: SparklineDirection,
    /// Glyphs from lowest to highest value.
    pub symbols: &'static [&'static str],
    /// Whether the latest visible sample gets a distinct style.
    pub highlight_latest: bool,
    /// Whether the first visible sample gets a distinct style.
    pub highlight_first: bool,
    /// Whether visible high samples get a distinct style.
    pub highlight_high: bool,
    /// Whether visible low samples get a distinct style.
    pub highlight_low: bool,
    /// Fill background before rendering.
    pub background: bool,
}

impl SparklinePolicy {
    /// Standard sparkline policy.
    #[must_use]
    pub const fn compact() -> Self {
        Self {
            max: None,
            window: None,
            direction: SparklineDirection::LeftToRight,
            symbols: &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"],
            highlight_latest: true,
            highlight_first: false,
            highlight_high: false,
            highlight_low: false,
            background: false,
        }
    }

    /// Bare sparkline policy with no latest-sample highlight or background.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            max: None,
            window: None,
            direction: SparklineDirection::LeftToRight,
            symbols: &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"],
            highlight_latest: false,
            highlight_first: false,
            highlight_high: false,
            highlight_low: false,
            background: false,
        }
    }

    /// Return this policy with a max-value override.
    #[must_use]
    pub const fn max(mut self, max: Option<u64>) -> Self {
        self.max = max;
        self
    }

    /// Return this policy with sample windowing.
    #[must_use]
    pub const fn window(mut self, window: Option<usize>) -> Self {
        self.window = window;
        self
    }

    /// Return this policy with render direction changed.
    #[must_use]
    pub const fn direction(mut self, direction: SparklineDirection) -> Self {
        self.direction = direction;
        self
    }

    /// Return this policy with first-sample highlighting changed.
    #[must_use]
    pub const fn highlight_first(mut self, highlight_first: bool) -> Self {
        self.highlight_first = highlight_first;
        self
    }

    /// Return this policy with high-sample highlighting changed.
    #[must_use]
    pub const fn highlight_high(mut self, highlight_high: bool) -> Self {
        self.highlight_high = highlight_high;
        self
    }

    /// Return this policy with low-sample highlighting changed.
    #[must_use]
    pub const fn highlight_low(mut self, highlight_low: bool) -> Self {
        self.highlight_low = highlight_low;
        self
    }

    /// Return this policy with background fill changed.
    #[must_use]
    pub const fn background(mut self, background: bool) -> Self {
        self.background = background;
        self
    }
}

impl Default for SparklinePolicy {
    fn default() -> Self {
        Self::compact()
    }
}

/// Sparkline visual styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparklineStyles {
    /// Normal sample style.
    pub normal: Style,
    /// Latest sample style.
    pub latest: Style,
    /// First visible sample style.
    pub first: Style,
    /// High visible sample style.
    pub high: Style,
    /// Low visible sample style.
    pub low: Style,
    /// Empty-content style.
    pub empty: Style,
    /// Background fill style.
    pub background: Style,
}

impl Default for SparklineStyles {
    fn default() -> Self {
        Self {
            normal: Style::new().fg(Color::Cyan),
            latest: Style::new()
                .fg(Color::BrightCyan)
                .add_modifier(Modifier::BOLD),
            first: Style::new().fg(Color::BrightBlue),
            high: Style::new()
                .fg(Color::BrightGreen)
                .add_modifier(Modifier::BOLD),
            low: Style::new().fg(Color::BrightRed),
            empty: Style::new().fg(Color::BrightBlack),
            background: Style::new(),
        }
    }
}

/// Canonical component-lifecycle sparkline.
pub struct SparklineComponent<'a> {
    id: LayoutId,
    samples: &'a [u64],
    policy: SparklinePolicy,
    styles: SparklineStyles,
    empty: &'a str,
}

impl<'a> SparklineComponent<'a> {
    /// Create a sparkline component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, samples: &'a [u64]) -> Self {
        Self {
            id: id.into(),
            samples,
            policy: SparklinePolicy::compact(),
            styles: SparklineStyles {
                normal: Style::new(),
                latest: Style::new(),
                first: Style::new(),
                high: Style::new(),
                low: Style::new(),
                empty: Style::new(),
                background: Style::new(),
            },
            empty: "No data",
        }
    }

    /// Set rendering policy.
    #[must_use]
    pub const fn policy(mut self, policy: SparklinePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: SparklineStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set empty message.
    #[must_use]
    pub const fn empty(mut self, empty: &'a str) -> Self {
        self.empty = empty;
        self
    }
}

impl Component for SparklineComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.samples.hash(&mut layout);
        self.empty.hash(&mut layout);
        self.policy.max.hash(&mut layout);
        self.policy.window.hash(&mut layout);
        self.policy.symbols.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.policy.direction.hash(&mut paint);
        self.policy.highlight_latest.hash(&mut paint);
        self.policy.highlight_first.hash(&mut paint);
        self.policy.highlight_high.hash(&mut paint);
        self.policy.highlight_low.hash(&mut paint);
        self.policy.background.hash(&mut paint);
        self.styles.normal.hash(&mut paint);
        self.styles.latest.hash(&mut paint);
        self.styles.first.hash(&mut paint);
        self.styles.high.hash(&mut paint);
        self.styles.low.hash(&mut paint);
        self.styles.empty.hash(&mut paint);
        self.styles.background.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let maximum_samples = self
            .policy
            .window
            .unwrap_or(self.samples.len())
            .min(self.samples.len());
        let intrinsic_width = if maximum_samples == 0 || self.policy.symbols.is_empty() {
            bmux_tui::text_width::display_width(self.empty)
        } else {
            let samples = &self.samples[self.samples.len().saturating_sub(maximum_samples)..];
            let max = self
                .policy
                .max
                .unwrap_or_else(|| samples.iter().copied().max().unwrap_or(0));
            samples
                .iter()
                .map(|sample| bmux_tui::text_width::display_width(self.glyph_for(*sample, max)))
                .sum()
        };
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            u16::try_from(intrinsic_width)
                .unwrap_or(u16::MAX)
                .clamp(constraints.min_width(), constraints.max_width())
        };
        let height = usize::from(width > 0);
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
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        if self.policy.background {
            cx.fill(area, " ", self.styles.background);
        }
        let samples = self.visible_samples(layout.size.width);
        if samples.is_empty() || self.policy.symbols.is_empty() {
            cx.write_line_with_fallback_style(area, &Line::from(self.empty), self.styles.empty);
        } else {
            let max = self
                .policy
                .max
                .unwrap_or_else(|| samples.iter().copied().max().unwrap_or(0));
            let high = samples.iter().copied().max().unwrap_or(0);
            let low = samples.iter().copied().min().unwrap_or(0);
            let last = samples.len().saturating_sub(1);
            let iter: Box<dyn Iterator<Item = (usize, &u64)> + '_> = match self.policy.direction {
                SparklineDirection::LeftToRight => Box::new(samples.iter().enumerate()),
                SparklineDirection::RightToLeft => Box::new(samples.iter().enumerate().rev()),
            };
            let spans = iter
                .map(|(index, sample)| {
                    let style = if self.policy.highlight_latest && index == last {
                        self.styles.latest
                    } else if self.policy.highlight_first && index == 0 {
                        self.styles.first
                    } else if self.policy.highlight_high && *sample == high {
                        self.styles.high
                    } else if self.policy.highlight_low && *sample == low {
                        self.styles.low
                    } else {
                        self.styles.normal
                    };
                    Span::styled(self.glyph_for(*sample, max), style)
                })
                .collect::<Vec<_>>();
            cx.write_line(area, &Line::from_spans(spans));
        }
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "chart",
        ));
        cx.push_damage(area);
    }
}

impl<'a> SparklineComponent<'a> {
    /// Return visible samples for `width`.
    #[must_use]
    pub fn visible_samples(&self, width: u16) -> &'a [u64] {
        visible_samples(self.samples, width, self.policy.window)
    }

    /// Map a sample value to a configured glyph.
    #[must_use]
    pub fn glyph_for(&self, value: u64, max: u64) -> &'static str {
        glyph_for(value, max, self.policy.symbols)
    }
}

fn visible_samples(samples: &[u64], width: u16, window: Option<usize>) -> &[u64] {
    let limit = window.unwrap_or(usize::MAX).min(usize::from(width));
    if limit == 0 || samples.is_empty() {
        return &samples[0..0];
    }
    let start = samples.len().saturating_sub(limit);
    &samples[start..]
}

fn glyph_for(value: u64, max: u64, symbols: &[&'static str]) -> &'static str {
    if symbols.is_empty() {
        return "";
    }
    if max == 0 {
        return symbols[0];
    }
    let last = symbols.len().saturating_sub(1);
    let scaled =
        (u128::from(value.min(max)) * u128::try_from(last).unwrap_or(u128::MAX)) / u128::from(max);
    symbols[usize::try_from(scaled).unwrap_or(last).min(last)]
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`SparklineStyles`].
    #[must_use]
    pub fn sparkline_styles(self) -> SparklineStyles {
        SparklineStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for SparklineStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            normal: theme.info,
            latest: theme.info.add_modifier(bmux_tui::style::Modifier::BOLD),
            first: theme.text,
            high: theme.success.add_modifier(bmux_tui::style::Modifier::BOLD),
            low: theme.error,
            empty: theme.muted,
            background: theme.surfaces.normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx, LogicalSize};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::style::{Color, Style};

    use super::{SparklineComponent, SparklineDirection, SparklinePolicy, SparklineStyles};

    fn render(component: &SparklineComponent<'_>, width: u16) -> Buffer {
        let layout = component.layout(
            Constraints::tight(Size::new(width, 1)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        buffer
    }

    #[test]
    fn canonical_paint_maps_empty_and_ascending_samples() {
        let empty = render(&SparklineComponent::new("empty", &[]), 8);
        assert_eq!(empty.row_symbols(0).as_deref(), Some("No data "));

        let samples = [0, 1, 2, 3];
        let ascending = render(
            &SparklineComponent::new("ascending", &samples)
                .policy(SparklinePolicy::bare().max(Some(3))),
            4,
        );
        assert_eq!(ascending.row_symbols(0).as_deref(), Some("▁▃▅█"));
    }

    #[test]
    fn canonical_paint_supports_direction_window_and_highlights() {
        let samples = [0, 1, 2, 3, 4];
        let reversed = render(
            &SparklineComponent::new("reversed", &samples).policy(
                SparklinePolicy::bare()
                    .max(Some(4))
                    .window(Some(4))
                    .direction(SparklineDirection::RightToLeft),
            ),
            4,
        );
        assert_eq!(reversed.row_symbols(0).as_deref(), Some("█▆▄▂"));

        let styles = SparklineStyles {
            high: Style::new().fg(Color::Green),
            low: Style::new().fg(Color::Red),
            ..SparklineStyles::default()
        };
        let highlighted = render(
            &SparklineComponent::new("highlighted", &samples)
                .policy(
                    SparklinePolicy::bare()
                        .max(Some(4))
                        .highlight_high(true)
                        .highlight_low(true),
                )
                .styles(styles),
            5,
        );
        assert_eq!(
            highlighted.get(Point::new(0, 0)).expect("low").style.fg,
            Some(Color::Red)
        );
        assert_eq!(
            highlighted.get(Point::new(4, 0)).expect("high").style.fg,
            Some(Color::Green)
        );
    }

    #[test]
    fn component_queries_visible_samples_and_glyphs() {
        let samples = [1, 2, 3, 4, 5];
        let component = SparklineComponent::new("traffic", &samples)
            .policy(SparklinePolicy::bare().window(Some(4)));
        assert_eq!(component.visible_samples(2), &[4, 5]);
        assert_eq!(component.glyph_for(3, 5), "▅");
    }

    #[test]
    fn component_measures_paints_and_registers_chart() {
        let samples = [0, 1, 2, 3];
        let component = SparklineComponent::new("traffic", &samples)
            .policy(SparklinePolicy::bare().max(Some(3)));
        let layout = component.layout(Constraints::for_width(4), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(4, 1));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("▁▃▅█"));
        assert_eq!(frame.semantics().regions().len(), 1);
        assert!(
            !frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .is_none()
        );
    }

    #[test]
    fn component_measures_empty_message() {
        let component = SparklineComponent::new("traffic", &[]).empty("Waiting");
        let layout = component.layout(Constraints::new(0, 20, 0, None), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(7, 1));
    }

    #[test]
    fn component_visual_policy_and_styles_are_paint_only() {
        let samples = [1, 2, 3];
        let initial = SparklineComponent::new("traffic", &samples).revision();
        let reversed = SparklineComponent::new("traffic", &samples)
            .policy(SparklinePolicy::compact().direction(SparklineDirection::RightToLeft))
            .revision();
        let styled = SparklineComponent::new("traffic", &samples)
            .styles(SparklineStyles {
                normal: Style::new().fg(Color::Red),
                ..SparklineStyles::default()
            })
            .revision();
        assert_eq!(initial.layout, reversed.layout);
        assert_ne!(initial.paint, reversed.paint);
        assert_eq!(initial.layout, styled.layout);
        assert_ne!(initial.paint, styled.paint);
    }

    #[test]
    fn zero_width_canonical_layout_and_paint_do_not_panic() {
        let samples = [1, 2, 3];
        let component = SparklineComponent::new("tiny", &samples);
        let layout = component.layout(Constraints::tight(Size::new(0, 0)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
    }
}
