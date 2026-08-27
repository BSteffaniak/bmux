//! Compact badge / pill label component.

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

/// Generic badge severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Canonical component-lifecycle badge leaf.
pub struct BadgeComponent<'a> {
    id: LayoutId,
    badge: Badge<'a>,
}

impl<'a> BadgeComponent<'a> {
    /// Create a badge component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, label: &'a str) -> Self {
        Self {
            id: id.into(),
            badge: Badge::new(label),
        }
    }

    /// Set severity.
    #[must_use]
    pub const fn severity(mut self, severity: BadgeSeverity) -> Self {
        self.badge.severity = severity;
        self
    }

    /// Set chrome policy.
    #[must_use]
    pub const fn policy(mut self, policy: BadgePolicy) -> Self {
        self.badge.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: BadgeStyles) -> Self {
        self.badge.styles = styles;
        self
    }
}

impl Component for BadgeComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.badge.label.hash(&mut layout);
        self.badge.policy.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.badge.severity.hash(&mut paint);
        format!("{:?}", self.badge.styles).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = u16::try_from(bmux_tui::text_width::display_width(&self.badge.text()))
            .unwrap_or(u16::MAX);
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, 1)),
        )
        .with_metadata(LayoutMetadata::new().semantic("status"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let mut line = Line::from_spans([Span::styled(self.badge.text(), self.badge.style())]);
        if self.badge.policy.truncate {
            line = line.truncate(usize::from(layout.size.width));
        }
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        cx.write_line(area, &line);
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "status",
        ));
        cx.push_damage(area);
    }
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
        let mut line = Line::from_spans([Span::styled(self.text(), self.style())]);
        if self.policy.truncate {
            line = line.truncate(usize::from(area.width));
        }
        frame.write_line(area, &line);
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

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`BadgeStyles`].
    #[must_use]
    pub fn badge_styles(self) -> BadgeStyles {
        BadgeStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for BadgeStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            default: theme.text.add_modifier(bmux_tui::style::Modifier::BOLD),
            info: theme.info.add_modifier(bmux_tui::style::Modifier::BOLD),
            success: theme.success.add_modifier(bmux_tui::style::Modifier::BOLD),
            warning: theme.warning.add_modifier(bmux_tui::style::Modifier::BOLD),
            error: theme.error.add_modifier(bmux_tui::style::Modifier::BOLD),
            muted: theme.muted,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Rect, Size};
    use bmux_tui::paint::PaintCx;

    use super::{Badge, BadgeComponent, BadgePolicy, BadgeSeverity, BadgeStyles};

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

    #[test]
    fn canonical_component_measures_and_projects_metadata() {
        let badge = BadgeComponent::new("health", "ok").severity(BadgeSeverity::Success);
        let mut layout_cx = LayoutCx::new();
        let layout = badge.layout(Constraints::loose(Size::new(20, 4)), &mut layout_cx);
        assert_eq!(layout.size.width, 6);
        assert_eq!(layout.size.height, 1);
        assert_eq!(layout.metadata.semantics, ["status"]);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
        let mut frame = Frame::new(&mut buffer);
        badge.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("[ ok ]              ")
        );
        assert_eq!(frame.semantics().regions()[0].id, "health");
        assert_eq!(frame.semantics().regions()[0].role, "status");
        assert_eq!(
            frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .retained_regions(),
            &[Rect::new(0, 0, 6, 1)]
        );
    }

    #[test]
    fn canonical_component_separates_layout_and_paint_revisions() {
        let initial = BadgeComponent::new("health", "ok").revision();
        let severity = BadgeComponent::new("health", "ok")
            .severity(BadgeSeverity::Error)
            .revision();
        assert_eq!(initial.layout, severity.layout);
        assert_ne!(initial.paint, severity.paint);

        let styles = BadgeComponent::new("health", "ok")
            .styles(BadgeStyles::default())
            .revision();
        assert_ne!(initial.paint, styles.paint);
        assert_ne!(
            initial.layout,
            BadgeComponent::new("health", "healthy").revision().layout
        );
        assert_ne!(
            initial.layout,
            BadgeComponent::new("health", "ok")
                .policy(BadgePolicy::bare())
                .revision()
                .layout
        );
    }
}
