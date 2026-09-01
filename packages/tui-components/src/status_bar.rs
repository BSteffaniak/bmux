//! Generic status and message bar renderers.

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

/// Canonical component-lifecycle status bar.
pub struct StatusBarComponent<'a> {
    id: LayoutId,
    bar: StatusBar<'a>,
}

impl<'a> StatusBarComponent<'a> {
    /// Create an empty status bar with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>) -> Self {
        Self {
            id: id.into(),
            bar: StatusBar::new(),
        }
    }

    /// Set left segments.
    #[must_use]
    pub const fn left(mut self, segments: &'a [StatusSegment<'a>]) -> Self {
        self.bar.left = segments;
        self
    }

    /// Set center segments.
    #[must_use]
    pub const fn center(mut self, segments: &'a [StatusSegment<'a>]) -> Self {
        self.bar.center = segments;
        self
    }

    /// Set right segments.
    #[must_use]
    pub const fn right(mut self, segments: &'a [StatusSegment<'a>]) -> Self {
        self.bar.right = segments;
        self
    }

    /// Set rendering policy.
    #[must_use]
    pub const fn policy(mut self, policy: StatusBarPolicy<'a>) -> Self {
        self.bar.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: StatusBarStyles) -> Self {
        self.bar.styles = styles;
        self
    }
}

impl Component for StatusBarComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        format!("{:?}", (self.bar.left, self.bar.center, self.bar.right)).hash(&mut layout);
        self.bar.policy.separator.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.bar.policy).hash(&mut paint);
        format!("{:?}", self.bar.styles).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = [self.bar.left, self.bar.center, self.bar.right]
            .into_iter()
            .map(|segments| {
                u16_saturating(display_width(&segments_text(
                    segments,
                    self.bar.policy.separator,
                )))
            })
            .max()
            .unwrap_or(0);
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
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        if self.bar.policy.background {
            cx.fill(area, " ", self.bar.styles.background);
        }
        self.paint_group(cx, layout.size.width, self.bar.left, BarAlign::Left);
        self.paint_group(cx, layout.size.width, self.bar.center, BarAlign::Center);
        self.paint_group(cx, layout.size.width, self.bar.right, BarAlign::Right);
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "status",
        ));
        cx.push_damage(area);
    }
}

impl StatusBarComponent<'_> {
    fn paint_group(
        &self,
        cx: &mut PaintCx<'_, '_>,
        available: u16,
        segments: &[StatusSegment<'_>],
        align: BarAlign,
    ) {
        if segments.is_empty() {
            return;
        }
        let full_width = display_width(&segments_text(segments, self.bar.policy.separator));
        let width = u16_saturating(full_width.min(usize::from(available)));
        let x = match align {
            BarAlign::Left => 0,
            BarAlign::Center => available.saturating_sub(width) / 2,
            BarAlign::Right => available.saturating_sub(width),
        };
        let line = self.bar.line(segments);
        let line = if self.bar.policy.truncate && full_width > usize::from(width) {
            line.truncate(usize::from(width))
        } else {
            line
        };
        cx.write_line_with_fallback_style(
            LocalRect::new(i32::from(x), 0, width, 1),
            &line,
            self.bar.styles.default,
        );
    }
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

/// Canonical component-lifecycle single message bar.
pub struct MessageBarComponent<'a> {
    id: LayoutId,
    bar: MessageBar<'a>,
}

impl<'a> MessageBarComponent<'a> {
    /// Create a message bar with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, message: &'a str) -> Self {
        Self {
            id: id.into(),
            bar: MessageBar::new(message),
        }
    }

    /// Set message severity.
    #[must_use]
    pub const fn severity(mut self, severity: StatusSeverity) -> Self {
        self.bar.message.severity = severity;
        self
    }

    /// Set alignment.
    #[must_use]
    pub const fn align(mut self, align: BarAlign) -> Self {
        self.bar.align = align;
        self
    }

    /// Set rendering policy.
    #[must_use]
    pub const fn policy(mut self, policy: StatusBarPolicy<'a>) -> Self {
        self.bar.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: StatusBarStyles) -> Self {
        self.bar.styles = styles;
        self
    }
}

impl Component for MessageBarComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.bar.message.text.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.bar).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                u16_saturating(display_width(self.bar.message.text)),
                1,
            )),
        )
        .with_metadata(LayoutMetadata::new().semantic("status"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        if self.bar.policy.background {
            cx.fill(area, " ", self.bar.styles.background);
        }
        StatusBarComponent::new(self.id.clone())
            .policy(self.bar.policy)
            .styles(self.bar.styles)
            .paint_group(cx, layout.size.width, &[self.bar.message], self.bar.align);
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "status",
        ));
        cx.push_damage(area);
    }
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

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`StatusBarStyles`].
    #[must_use]
    pub fn status_bar_styles(self) -> StatusBarStyles {
        StatusBarStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for StatusBarStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            default: theme.text,
            muted: theme.muted,
            info: theme.info,
            success: theme.success,
            warning: theme.warning.add_modifier(bmux_tui::style::Modifier::BOLD),
            error: theme.error.add_modifier(bmux_tui::style::Modifier::BOLD),
            separator: theme.border,
            background: theme.surfaces.normal,
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

    use super::{
        BarAlign, MessageBarComponent, StatusBarComponent, StatusBarPolicy, StatusSegment,
        StatusSeverity,
    };

    #[test]
    fn renders_left_center_and_right_segments() {
        let left = [StatusSegment::new("NORMAL")];
        let center = [StatusSegment::new("CENTER")];
        let right = [StatusSegment::new("RIGHT")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 1));
        let mut frame = Frame::new(&mut buffer);

        let bar = StatusBarComponent::new("status")
            .left(&left)
            .center(&center)
            .right(&right);
        let layout = bar.layout(Constraints::tight(Size::new(30, 1)), &mut LayoutCx::new());
        bar.paint(&layout, &mut PaintCx::new(&mut frame));

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

        let bar = StatusBarComponent::new("status")
            .left(&left)
            .policy(StatusBarPolicy::compact().separator(" | "));
        let layout = bar.layout(Constraints::tight(Size::new(12, 1)), &mut LayoutCx::new());
        bar.paint(&layout, &mut PaintCx::new(&mut frame));

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

        let bar = StatusBarComponent::new("status").left(&left);
        let layout = bar.layout(Constraints::tight(Size::new(8, 1)), &mut LayoutCx::new());
        bar.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("very lo…"));
    }

    #[test]
    fn message_bar_aligns_center_and_right() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);

        let centered = MessageBarComponent::new("centered", "ok").align(BarAlign::Center);
        let centered_layout =
            centered.layout(Constraints::tight(Size::new(12, 1)), &mut LayoutCx::new());
        centered.paint(&centered_layout, &mut PaintCx::new(&mut frame));
        let right = MessageBarComponent::new("right", "ok").align(BarAlign::Right);
        let right_layout = right.layout(Constraints::tight(Size::new(12, 1)), &mut LayoutCx::new());
        PaintCx::new(&mut frame).with_child(
            0,
            1,
            bmux_tui::paint::LocalRect::new(0, 0, 12, 1),
            |cx| right.paint(&right_layout, cx),
        );

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

        let bar = StatusBarComponent::new("status")
            .left(&left)
            .policy(StatusBarPolicy::compact().background(true));
        let layout = bar.layout(Constraints::tight(Size::new(4, 1)), &mut LayoutCx::new());
        bar.paint(&layout, &mut PaintCx::new(&mut frame));

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

        let bar = StatusBarComponent::new("status").left(&left);
        let layout = bar.layout(Constraints::tight(Size::new(20, 1)), &mut LayoutCx::new());
        bar.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("info · warn · err   ")
        );
    }

    #[test]
    fn canonical_message_component_preserves_alignment_and_channels() {
        let bar = MessageBarComponent::new("message", "ok").align(BarAlign::Right);
        let layout = bar.layout(Constraints::tight(Size::new(12, 1)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);
        bar.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("          ok")
        );
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 12, 1));
        assert_eq!(
            frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .retained_regions(),
            &[Rect::new(0, 0, 12, 1)]
        );
    }

    #[test]
    fn canonical_message_component_revision_tracks_geometry_and_paint() {
        let initial = MessageBarComponent::new("message", "ok").revision();
        let severity = MessageBarComponent::new("message", "ok")
            .severity(StatusSeverity::Warning)
            .revision();
        assert_eq!(initial.layout, severity.layout);
        assert_ne!(initial.paint, severity.paint);
        assert_ne!(
            initial.layout,
            MessageBarComponent::new("message", "healthy")
                .revision()
                .layout
        );
    }

    #[test]
    fn canonical_component_uses_one_layout_for_all_channels() {
        let left = [StatusSegment::new("NORMAL")];
        let right = [StatusSegment::new("RIGHT")];
        let bar = StatusBarComponent::new("footer").left(&left).right(&right);
        let mut layout_cx = LayoutCx::new();
        let layout = bar.layout(Constraints::loose(Size::new(20, 2)), &mut layout_cx);
        assert_eq!(layout.size, bmux_tui::component::LogicalSize::new(6, 1));
        assert_eq!(layout.metadata.semantics, ["status"]);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
        let mut frame = Frame::new(&mut buffer);
        bar.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 6, 1));
        assert_eq!(
            frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .retained_regions(),
            &[Rect::new(0, 0, 6, 1)]
        );
    }

    #[test]
    fn canonical_component_revision_tracks_geometry_and_paint() {
        let left = [StatusSegment::new("ok")];
        let initial = StatusBarComponent::new("footer").left(&left).revision();
        let policy = StatusBarComponent::new("footer")
            .left(&left)
            .policy(StatusBarPolicy::compact().background(true))
            .revision();
        assert_eq!(initial.layout, policy.layout);
        assert_ne!(initial.paint, policy.paint);

        let longer = [StatusSegment::new("healthy")];
        assert_ne!(
            initial.layout,
            StatusBarComponent::new("footer")
                .left(&longer)
                .revision()
                .layout
        );
    }
}
