//! Compact sparkline/trend visualization component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};

/// Sparkline render direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            empty: Style::new().fg(Color::BrightBlack),
            background: Style::new(),
        }
    }
}

/// Compact sparkline over caller-owned samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sparkline<'a> {
    samples: &'a [u64],
    policy: SparklinePolicy,
    styles: SparklineStyles,
    empty: &'a str,
}

impl<'a> Sparkline<'a> {
    /// Create a sparkline over caller-owned samples.
    #[must_use]
    pub const fn new(samples: &'a [u64]) -> Self {
        Self {
            samples,
            policy: SparklinePolicy {
                max: None,
                window: None,
                direction: SparklineDirection::LeftToRight,
                symbols: &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"],
                highlight_latest: true,
                highlight_first: false,
                highlight_high: false,
                background: false,
            },
            styles: SparklineStyles {
                normal: Style::new(),
                latest: Style::new(),
                first: Style::new(),
                high: Style::new(),
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

    /// Render sparkline.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if self.policy.background {
            frame.fill(area, " ", self.styles.background);
        }
        let samples = self.visible_samples(area.width);
        if samples.is_empty() || self.policy.symbols.is_empty() {
            frame.write_line_with_fallback_style(area, &Line::from(self.empty), self.styles.empty);
            return;
        }
        let max = self
            .policy
            .max
            .unwrap_or_else(|| samples.iter().copied().max().unwrap_or(0));
        let high = samples.iter().copied().max().unwrap_or(0);
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
                } else {
                    self.styles.normal
                };
                Span::styled(self.glyph_for(*sample, max), style)
            })
            .collect::<Vec<_>>();
        frame.write_line(area, &Line::from_spans(spans));
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

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::style::{Color, Style};

    use super::{Sparkline, SparklineDirection, SparklinePolicy, SparklineStyles};

    #[test]
    fn maps_empty_samples_to_empty_message() {
        let samples = [];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        Sparkline::new(&samples).render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("No data "));
    }

    #[test]
    fn renders_ascending_samples() {
        let samples = [0, 1, 2, 3];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);

        Sparkline::new(&samples)
            .policy(SparklinePolicy::bare().max(Some(3)))
            .render(Rect::new(0, 0, 4, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("▁▃▅█"));
    }

    #[test]
    fn renders_descending_samples() {
        let samples = [3, 2, 1, 0];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);

        Sparkline::new(&samples)
            .policy(SparklinePolicy::bare().max(Some(3)))
            .render(Rect::new(0, 0, 4, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("█▅▃▁"));
    }

    #[test]
    fn flat_zero_samples_use_lowest_symbol() {
        let samples = [0, 0, 0];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        Sparkline::new(&samples).render(Rect::new(0, 0, 3, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("▁▁▁"));
    }

    #[test]
    fn max_override_scales_non_zero_flat_samples() {
        let samples = [5, 5, 5];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        Sparkline::new(&samples)
            .policy(SparklinePolicy::bare().max(Some(10)))
            .render(Rect::new(0, 0, 3, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("▄▄▄"));
    }

    #[test]
    fn first_latest_and_high_samples_can_be_styled() {
        let samples = [1, 3, 2];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);
        let styles = SparklineStyles {
            first: Style::new().fg(Color::Blue),
            high: Style::new().fg(Color::Green),
            latest: Style::new().fg(Color::Yellow),
            ..SparklineStyles::default()
        };

        Sparkline::new(&samples)
            .policy(
                SparklinePolicy::bare()
                    .highlight_first(true)
                    .highlight_high(true),
            )
            .styles(styles)
            .render(Rect::new(0, 0, 3, 1), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 0))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Blue))
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(1, 0))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Green))
        );
    }

    #[test]
    fn right_to_left_direction_renders_latest_first() {
        let samples = [1, 2, 3];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        Sparkline::new(&samples)
            .policy(
                SparklinePolicy::bare()
                    .max(Some(3))
                    .direction(SparklineDirection::RightToLeft),
            )
            .render(Rect::new(0, 0, 3, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("█▅▃"));
    }

    #[test]
    fn window_and_width_clip_to_latest_samples() {
        let samples = [1, 2, 3, 4, 5];
        let sparkline = Sparkline::new(&samples).policy(SparklinePolicy::bare().window(Some(4)));

        assert_eq!(sparkline.visible_samples(2), &[4, 5]);
    }

    #[test]
    fn tiny_width_does_not_panic() {
        let samples = [1, 2, 3];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        Sparkline::new(&samples).render(Rect::new(0, 0, 0, 0), &mut frame);
    }
}
