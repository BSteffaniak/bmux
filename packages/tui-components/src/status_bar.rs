//! Generic status and message bar renderers.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::display_width;

/// Status severity for message/status segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSeverity {
    /// Default status.
    Default,
    /// Muted/secondary status.
    Muted,
    /// Informational status.
    Info,
    /// Success status.
    Success,
    /// Warning status.
    Warning,
    /// Error status.
    Error,
}

/// Alignment for a single message bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarAlign {
    /// Left align.
    Left,
    /// Center align.
    Center,
    /// Right align.
    Right,
}

/// One status-bar segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSegment<'a> {
    /// Segment text.
    pub text: &'a str,
    /// Segment severity.
    pub severity: StatusSeverity,
}

impl<'a> StatusSegment<'a> {
    /// Create a default status segment.
    #[must_use]
    pub const fn new(text: &'a str) -> Self {
        Self {
            text,
            severity: StatusSeverity::Default,
        }
    }

    /// Return this segment with severity set.
    #[must_use]
    pub const fn severity(mut self, severity: StatusSeverity) -> Self {
        self.severity = severity;
        self
    }
}

/// Behavior/rendering policy for status bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusBarPolicy<'a> {
    /// Separator between adjacent segments.
    pub separator: &'a str,
    /// Fill row background before rendering.
    pub background: bool,
    /// Truncate content to fit.
    pub truncate: bool,
}

impl<'a> StatusBarPolicy<'a> {
    /// Compact status-bar policy.
    #[must_use]
    pub const fn compact() -> Self {
        Self {
            separator: " · ",
            background: false,
            truncate: true,
        }
    }

    /// Return this policy with a custom separator.
    #[must_use]
    pub const fn separator(mut self, separator: &'a str) -> Self {
        self.separator = separator;
        self
    }

    /// Return this policy with background fill changed.
    #[must_use]
    pub const fn background(mut self, background: bool) -> Self {
        self.background = background;
        self
    }
}

impl Default for StatusBarPolicy<'_> {
    fn default() -> Self {
        Self::compact()
    }
}

/// Visual styles for status/message bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusBarStyles {
    /// Default segment style.
    pub default: Style,
    /// Muted segment style.
    pub muted: Style,
    /// Info segment style.
    pub info: Style,
    /// Success segment style.
    pub success: Style,
    /// Warning segment style.
    pub warning: Style,
    /// Error segment style.
    pub error: Style,
    /// Separator style.
    pub separator: Style,
    /// Background fill style.
    pub background: Style,
}

impl Default for StatusBarStyles {
    fn default() -> Self {
        Self {
            default: Style::new().fg(Color::White),
            muted: Style::new().fg(Color::BrightBlack),
            info: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            warning: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            separator: Style::new().fg(Color::BrightBlack),
            background: Style::new(),
        }
    }
}

/// Left/center/right status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusBar<'a> {
    left: &'a [StatusSegment<'a>],
    center: &'a [StatusSegment<'a>],
    right: &'a [StatusSegment<'a>],
    policy: StatusBarPolicy<'a>,
    styles: StatusBarStyles,
}

impl<'a> StatusBar<'a> {
    /// Create an empty status bar.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            left: &[],
            center: &[],
            right: &[],
            policy: StatusBarPolicy {
                separator: " · ",
                background: false,
                truncate: true,
            },
            styles: StatusBarStyles {
                default: Style::new(),
                muted: Style::new(),
                info: Style::new(),
                success: Style::new(),
                warning: Style::new(),
                error: Style::new(),
                separator: Style::new(),
                background: Style::new(),
            },
        }
    }

    /// Set left segments.
    #[must_use]
    pub const fn left(mut self, segments: &'a [StatusSegment<'a>]) -> Self {
        self.left = segments;
        self
    }

    /// Set center segments.
    #[must_use]
    pub const fn center(mut self, segments: &'a [StatusSegment<'a>]) -> Self {
        self.center = segments;
        self
    }

    /// Set right segments.
    #[must_use]
    pub const fn right(mut self, segments: &'a [StatusSegment<'a>]) -> Self {
        self.right = segments;
        self
    }

    /// Set policy.
    #[must_use]
    pub const fn policy(mut self, policy: StatusBarPolicy<'a>) -> Self {
        self.policy = policy;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: StatusBarStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Render status bar into one row.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if self.policy.background {
            frame.fill(area, " ", self.styles.background);
        }
        self.render_group(area, self.left, BarAlign::Left, frame);
        self.render_group(area, self.center, BarAlign::Center, frame);
        self.render_group(area, self.right, BarAlign::Right, frame);
    }

    fn render_group(
        &self,
        area: Rect,
        segments: &[StatusSegment<'_>],
        align: BarAlign,
        frame: &mut Frame<'_>,
    ) {
        if segments.is_empty() {
            return;
        }
        let text = segments_text(segments, self.policy.separator);
        let full_width = display_width(&text);
        let width = full_width.min(usize::from(area.width));
        let x = match align {
            BarAlign::Left => area.x,
            BarAlign::Center => area
                .x
                .saturating_add(area.width.saturating_sub(u16_saturating(width)) / 2),
            BarAlign::Right => area
                .x
                .saturating_add(area.width.saturating_sub(u16_saturating(width))),
        };
        let rect = Rect::new(x, area.y, u16_saturating(width), 1);
        let line = self.line(segments);
        if self.policy.truncate && full_width > usize::from(rect.width) {
            frame.write_line(rect, &line.truncate(usize::from(rect.width)));
        } else {
            frame.write_line_with_fallback_style(rect, &line, self.styles.default);
        }
    }

    fn line(&self, segments: &[StatusSegment<'_>]) -> Line {
        let mut spans = Vec::new();
        for (index, segment) in segments.iter().copied().enumerate() {
            if index > 0 {
                spans.push(Span::styled(self.policy.separator, self.styles.separator));
            }
            spans.push(Span::styled(segment.text, self.style_for(segment.severity)));
        }
        Line::from_spans(spans)
    }

    const fn style_for(&self, severity: StatusSeverity) -> Style {
        match severity {
            StatusSeverity::Default => self.styles.default,
            StatusSeverity::Muted => self.styles.muted,
            StatusSeverity::Info => self.styles.info,
            StatusSeverity::Success => self.styles.success,
            StatusSeverity::Warning => self.styles.warning,
            StatusSeverity::Error => self.styles.error,
        }
    }
}

impl Default for StatusBar<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Single message bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageBar<'a> {
    message: StatusSegment<'a>,
    align: BarAlign,
    policy: StatusBarPolicy<'a>,
    styles: StatusBarStyles,
}

impl<'a> MessageBar<'a> {
    /// Create a message bar.
    #[must_use]
    pub const fn new(message: &'a str) -> Self {
        Self {
            message: StatusSegment::new(message),
            align: BarAlign::Left,
            policy: StatusBarPolicy {
                separator: " · ",
                background: false,
                truncate: true,
            },
            styles: StatusBarStyles {
                default: Style::new(),
                muted: Style::new(),
                info: Style::new(),
                success: Style::new(),
                warning: Style::new(),
                error: Style::new(),
                separator: Style::new(),
                background: Style::new(),
            },
        }
    }

    /// Set message severity.
    #[must_use]
    pub const fn severity(mut self, severity: StatusSeverity) -> Self {
        self.message.severity = severity;
        self
    }

    /// Set alignment.
    #[must_use]
    pub const fn align(mut self, align: BarAlign) -> Self {
        self.align = align;
        self
    }

    /// Set policy.
    #[must_use]
    pub const fn policy(mut self, policy: StatusBarPolicy<'a>) -> Self {
        self.policy = policy;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: StatusBarStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Render message into one row.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        let bar = StatusBar::new().policy(self.policy).styles(self.styles);
        if self.policy.background {
            frame.fill(area, " ", self.styles.background);
        }
        bar.render_group(area, &[self.message], self.align, frame);
    }
}

fn segments_text(segments: &[StatusSegment<'_>], separator: &str) -> String {
    segments
        .iter()
        .map(|segment| segment.text)
        .collect::<Vec<_>>()
        .join(separator)
}

fn u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;

    use super::{BarAlign, MessageBar, StatusBar, StatusBarPolicy, StatusSegment, StatusSeverity};

    #[test]
    fn renders_left_center_and_right_segments() {
        let left = [StatusSegment::new("NORMAL")];
        let center = [StatusSegment::new("CENTER")];
        let right = [StatusSegment::new("RIGHT")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 1));
        let mut frame = Frame::new(&mut buffer);

        StatusBar::new()
            .left(&left)
            .center(&center)
            .right(&right)
            .render(Rect::new(0, 0, 30, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("NORMAL      CENTER       RIGHT")
        );
    }

    #[test]
    fn renders_multiple_segments_with_custom_separator() {
        let left = [StatusSegment::new("one"), StatusSegment::new("two")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut frame = Frame::new(&mut buffer);

        StatusBar::new()
            .left(&left)
            .policy(StatusBarPolicy::compact().separator(" | "))
            .render(Rect::new(0, 0, 12, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("one | two   ")
        );
    }

    #[test]
    fn truncates_when_too_small() {
        let left = [StatusSegment::new("very long status")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        StatusBar::new()
            .left(&left)
            .render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("very lo…"));
    }

    #[test]
    fn message_bar_aligns_center_and_right() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);

        MessageBar::new("ok")
            .align(BarAlign::Center)
            .render(Rect::new(0, 0, 12, 1), &mut frame);
        MessageBar::new("ok")
            .align(BarAlign::Right)
            .render(Rect::new(0, 1, 12, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("     ok     ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("          ok")
        );
    }

    #[test]
    fn background_fill_is_optional() {
        let left = [StatusSegment::new("x")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);

        StatusBar::new()
            .left(&left)
            .policy(StatusBarPolicy::compact().background(true))
            .render(Rect::new(0, 0, 4, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("x   "));
    }

    #[test]
    fn severity_segments_render() {
        let left = [
            StatusSegment::new("info").severity(StatusSeverity::Info),
            StatusSegment::new("warn").severity(StatusSeverity::Warning),
            StatusSegment::new("err").severity(StatusSeverity::Error),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 1));
        let mut frame = Frame::new(&mut buffer);

        StatusBar::new()
            .left(&left)
            .render(Rect::new(0, 0, 20, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("info · warn · err   ")
        );
    }
}
