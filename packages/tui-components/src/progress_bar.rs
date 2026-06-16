//! Generic progress bar / gauge component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::{display_width, truncate_to_display_width};

/// Progress value model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressLabelPlacement {
    /// Do not render a label.
    Hidden,
    /// Render label inside the bar.
    Inside,
    /// Render label after the bar when space allows.
    Right,
}

/// Render mode for [`ProgressBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        let mut spans = Vec::new();
        if filled_width > 0 {
            spans.push(Span::styled(
                self.policy.filled.repeat(usize::from(filled_width)),
                self.filled_style(),
            ));
        }
        let empty_width = bar_width.saturating_sub(filled_width);
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
        let mut spans = Vec::new();
        if filled_width > 0 {
            spans.push(Span::styled(
                self.policy.filled.repeat(usize::from(filled_width)),
                self.filled_style(),
            ));
        }
        let empty_width = gauge_width.saturating_sub(filled_width);
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
                spans.push(Span::styled(label, self.styles.label));
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

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;

    use super::{ProgressBar, ProgressBarPolicy, ProgressBarValue, ProgressLabelPlacement};

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
