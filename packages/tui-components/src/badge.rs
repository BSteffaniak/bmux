//! Compact badge / pill label component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::{display_width, truncate_to_display_width};

/// Generic badge severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadgeSeverity {
    /// Default badge.
    #[default]
    Default,
    /// Informational badge.
    Info,
    /// Success badge.
    Success,
    /// Warning badge.
    Warning,
    /// Error badge.
    Error,
    /// Muted badge.
    Muted,
}

/// Badge chrome policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadgePolicy {
    /// Left delimiter.
    pub left: &'static str,
    /// Right delimiter.
    pub right: &'static str,
    /// Horizontal padding cells inside delimiters.
    pub padding: u16,
    /// Uppercase label before rendering.
    pub uppercase: bool,
    /// Truncate content to area width.
    pub truncate: bool,
}

impl BadgePolicy {
    /// Square-bracket badge.
    #[must_use]
    pub const fn bracketed() -> Self {
        Self {
            left: "[",
            right: "]",
            padding: 1,
            uppercase: false,
            truncate: true,
        }
    }

    /// Rounded pill-like badge.
    #[must_use]
    pub const fn pill() -> Self {
        Self {
            left: "‹",
            right: "›",
            padding: 1,
            uppercase: false,
            truncate: true,
        }
    }

    /// Bare label without chrome.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            left: "",
            right: "",
            padding: 0,
            uppercase: false,
            truncate: true,
        }
    }

    /// Return this policy with uppercase behavior changed.
    #[must_use]
    pub const fn uppercase(mut self, uppercase: bool) -> Self {
        self.uppercase = uppercase;
        self
    }
}

impl Default for BadgePolicy {
    fn default() -> Self {
        Self::bracketed()
    }
}

/// Badge visual styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadgeStyles {
    /// Default style.
    pub default: Style,
    /// Info style.
    pub info: Style,
    /// Success style.
    pub success: Style,
    /// Warning style.
    pub warning: Style,
    /// Error style.
    pub error: Style,
    /// Muted style.
    pub muted: Style,
}

impl Default for BadgeStyles {
    fn default() -> Self {
        Self {
            default: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            info: Style::new()
                .fg(Color::BrightCyan)
                .add_modifier(Modifier::BOLD),
            success: Style::new()
                .fg(Color::BrightGreen)
                .add_modifier(Modifier::BOLD),
            warning: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            muted: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Compact badge / pill label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Badge<'a> {
    label: &'a str,
    severity: BadgeSeverity,
    policy: BadgePolicy,
    styles: BadgeStyles,
}

impl<'a> Badge<'a> {
    /// Create a badge.
    #[must_use]
    pub const fn new(label: &'a str) -> Self {
        Self {
            label,
            severity: BadgeSeverity::Default,
            policy: BadgePolicy {
                left: "[",
                right: "]",
                padding: 1,
                uppercase: false,
                truncate: true,
            },
            styles: BadgeStyles {
                default: Style::new(),
                info: Style::new(),
                success: Style::new(),
                warning: Style::new(),
                error: Style::new(),
                muted: Style::new(),
            },
        }
    }

    /// Set severity.
    #[must_use]
    pub const fn severity(mut self, severity: BadgeSeverity) -> Self {
        self.severity = severity;
        self
    }

    /// Set policy.
    #[must_use]
    pub const fn policy(mut self, policy: BadgePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: BadgeStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Rendered badge text before area truncation.
    #[must_use]
    pub fn text(&self) -> String {
        let label = if self.policy.uppercase {
            self.label.to_uppercase()
        } else {
            self.label.to_owned()
        };
        format!(
            "{}{}{}{}{}",
            self.policy.left,
            " ".repeat(usize::from(self.policy.padding)),
            label,
            " ".repeat(usize::from(self.policy.padding)),
            self.policy.right
        )
    }

    /// Render the badge.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        let mut text = self.text();
        if self.policy.truncate && display_width(&text) > usize::from(area.width) {
            text = truncate_to_display_width(&text, usize::from(area.width));
        }
        frame.write_line(area, &Line::from_spans([Span::styled(text, self.style())]));
    }

    const fn style(&self) -> Style {
        match self.severity {
            BadgeSeverity::Default => self.styles.default,
            BadgeSeverity::Info => self.styles.info,
            BadgeSeverity::Success => self.styles.success,
            BadgeSeverity::Warning => self.styles.warning,
            BadgeSeverity::Error => self.styles.error,
            BadgeSeverity::Muted => self.styles.muted,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;

    use super::{Badge, BadgePolicy, BadgeSeverity};

    #[test]
    fn renders_bracketed_badge() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut frame = Frame::new(&mut buffer);

        Badge::new("ok").render(Rect::new(0, 0, 10, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("[ ok ]    "));
    }

    #[test]
    fn renders_uppercase_pill() {
        assert_eq!(
            Badge::new("info")
                .policy(BadgePolicy::pill().uppercase(true))
                .text(),
            "‹ INFO ›"
        );
    }

    #[test]
    fn supports_all_severities() {
        for severity in [
            BadgeSeverity::Default,
            BadgeSeverity::Info,
            BadgeSeverity::Success,
            BadgeSeverity::Warning,
            BadgeSeverity::Error,
            BadgeSeverity::Muted,
        ] {
            assert_eq!(Badge::new("x").severity(severity).text(), "[ x ]");
        }
    }

    #[test]
    fn truncates_to_tiny_width() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        Badge::new("abcdef").render(Rect::new(0, 0, 3, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("[ …"));
    }

    #[test]
    fn bare_policy_has_no_chrome() {
        assert_eq!(Badge::new("raw").policy(BadgePolicy::bare()).text(), "raw");
    }
}
