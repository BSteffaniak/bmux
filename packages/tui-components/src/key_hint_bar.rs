//! Compact key/action hint bar component.

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
use bmux_tui::text_width::display_width;

/// One key/action hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHint<'a> {
    /// Key or chord label.
    pub key: &'a str,
    /// Action label.
    pub label: &'a str,
    /// Whether this hint should render disabled.
    pub disabled: bool,
}

impl<'a> KeyHint<'a> {
    /// Create an enabled key hint.
    #[must_use]
    pub const fn new(key: &'a str, label: &'a str) -> Self {
        Self {
            key,
            label,
            disabled: false,
        }
    }

    /// Return this hint with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Overflow behavior for [`KeyHintBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyHintOverflow {
    /// Truncate to fit the available row.
    Truncate,
    /// Hide the whole bar if it does not fit.
    Hide,
}

/// Behavior policy for [`KeyHintBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHintBarPolicy<'a> {
    /// Separator between hints.
    pub separator: &'a str,
    /// Overflow behavior.
    pub overflow: KeyHintOverflow,
    /// Fill row background before rendering.
    pub background: bool,
}

impl<'a> KeyHintBarPolicy<'a> {
    /// Create a compact default policy.
    #[must_use]
    pub const fn compact() -> Self {
        Self {
            separator: " · ",
            overflow: KeyHintOverflow::Truncate,
            background: false,
        }
    }

    /// Return this policy with a custom separator.
    #[must_use]
    pub const fn separator(mut self, separator: &'a str) -> Self {
        self.separator = separator;
        self
    }

    /// Return this policy with overflow behavior changed.
    #[must_use]
    pub const fn overflow(mut self, overflow: KeyHintOverflow) -> Self {
        self.overflow = overflow;
        self
    }

    /// Return this policy with background fill changed.
    #[must_use]
    pub const fn background(mut self, background: bool) -> Self {
        self.background = background;
        self
    }
}

impl Default for KeyHintBarPolicy<'_> {
    fn default() -> Self {
        Self::compact()
    }
}

/// Visual styles for [`KeyHintBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHintBarStyles {
    /// Key style.
    pub key: Style,
    /// Action label style.
    pub label: Style,
    /// Separator style.
    pub separator: Style,
    /// Disabled hint style.
    pub disabled: Style,
    /// Background fill style.
    pub background: Style,
}

impl Default for KeyHintBarStyles {
    fn default() -> Self {
        Self {
            key: Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
            label: Style::new().fg(Color::BrightBlack),
            separator: Style::new().fg(Color::BrightBlack),
            disabled: Style::new().fg(Color::BrightBlack),
            background: Style::new(),
        }
    }
}

/// Compact key/action hint renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHintBar<'a> {
    hints: &'a [KeyHint<'a>],
    policy: KeyHintBarPolicy<'a>,
    styles: KeyHintBarStyles,
}

/// Canonical component-lifecycle key hint bar.
pub struct KeyHintBarComponent<'a> {
    id: LayoutId,
    bar: KeyHintBar<'a>,
}

impl<'a> KeyHintBarComponent<'a> {
    /// Create a hint bar with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, hints: &'a [KeyHint<'a>]) -> Self {
        Self {
            id: id.into(),
            bar: KeyHintBar::new(hints),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: KeyHintBarPolicy<'a>) -> Self {
        self.bar.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: KeyHintBarStyles) -> Self {
        self.bar.styles = styles;
        self
    }
}

impl Component for KeyHintBarComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        format!("{:?}", self.bar.hints).hash(&mut layout);
        self.bar.policy.separator.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.bar.policy).hash(&mut paint);
        format!("{:?}", self.bar.styles).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = u16::try_from(display_width(&self.bar.text())).unwrap_or(u16::MAX);
        let height = usize::from(!self.bar.hints.is_empty());
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, height)),
        )
        .with_metadata(LayoutMetadata::new().semantic("hint"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 || self.bar.hints.is_empty() {
            return;
        }
        let text = self.bar.text();
        if matches!(self.bar.policy.overflow, KeyHintOverflow::Hide)
            && display_width(&text) > usize::from(layout.size.width)
        {
            return;
        }
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        if self.bar.policy.background {
            cx.fill(area, " ", self.bar.styles.background);
        }
        let line = if display_width(&text) > usize::from(layout.size.width) {
            self.bar
                .styled_line()
                .truncate(usize::from(layout.size.width))
        } else {
            self.bar.styled_line()
        };
        cx.write_line_with_fallback_style(area, &line, self.bar.styles.background);
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "hint",
        ));
        cx.push_damage(area);
    }
}

impl<'a> KeyHintBar<'a> {
    /// Create a key hint bar over caller-owned hints.
    #[must_use]
    pub const fn new(hints: &'a [KeyHint<'a>]) -> Self {
        Self {
            hints,
            policy: KeyHintBarPolicy {
                separator: " · ",
                overflow: KeyHintOverflow::Truncate,
                background: false,
            },
            styles: KeyHintBarStyles {
                key: Style::new(),
                label: Style::new(),
                separator: Style::new(),
                disabled: Style::new(),
                background: Style::new(),
            },
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: KeyHintBarPolicy<'a>) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: KeyHintBarStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Build the untruncated display text.
    #[must_use]
    pub fn text(&self) -> String {
        hint_text(self.hints, self.policy.separator)
    }

    /// Render hints into one row.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() || self.hints.is_empty() {
            return;
        }
        let text = self.text();
        if matches!(self.policy.overflow, KeyHintOverflow::Hide)
            && display_width(&text) > usize::from(area.width)
        {
            return;
        }
        if self.policy.background {
            frame.fill(area, " ", self.styles.background);
        }
        let line = if display_width(&text) > usize::from(area.width) {
            self.styled_line().truncate(usize::from(area.width))
        } else {
            self.styled_line()
        };
        frame.write_line_with_fallback_style(area, &line, self.styles.background);
    }

    fn styled_line(&self) -> Line {
        let mut spans = Vec::new();
        for (index, hint) in self.hints.iter().copied().enumerate() {
            if index > 0 {
                spans.push(Span::styled(self.policy.separator, self.styles.separator));
            }
            let style = if hint.disabled {
                self.styles.disabled
            } else {
                self.styles.key
            };
            spans.push(Span::styled(hint.key, style));
            spans.push(Span::styled(" ", self.styles.label));
            spans.push(Span::styled(
                hint.label,
                if hint.disabled {
                    self.styles.disabled
                } else {
                    self.styles.label
                },
            ));
        }
        Line::from_spans(spans)
    }
}

fn hint_text(hints: &[KeyHint<'_>], separator: &str) -> String {
    hints
        .iter()
        .map(|hint| format!("{} {}", hint.key, hint.label))
        .collect::<Vec<_>>()
        .join(separator)
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`KeyHintBarStyles`].
    #[must_use]
    pub fn key_hint_bar_styles(self) -> KeyHintBarStyles {
        KeyHintBarStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for KeyHintBarStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            key: theme.text.add_modifier(bmux_tui::style::Modifier::BOLD),
            label: theme.muted,
            separator: theme.border,
            disabled: theme.disabled,
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

    use super::{KeyHint, KeyHintBar, KeyHintBarComponent, KeyHintBarPolicy, KeyHintOverflow};

    #[test]
    fn builds_compact_hint_text() {
        let hints = [KeyHint::new("q", "quit"), KeyHint::new("enter", "select")];

        assert_eq!(KeyHintBar::new(&hints).text(), "q quit · enter select");
    }

    #[test]
    fn renders_key_hints() {
        let hints = [KeyHint::new("q", "quit"), KeyHint::new("tab", "focus")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 1));
        let mut frame = Frame::new(&mut buffer);

        KeyHintBar::new(&hints).render(Rect::new(0, 0, 24, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("q quit · tab focus      ")
        );
    }

    #[test]
    fn supports_custom_separator() {
        let hints = [KeyHint::new("q", "quit"), KeyHint::new("esc", "cancel")];
        let bar = KeyHintBar::new(&hints).policy(KeyHintBarPolicy::compact().separator(" | "));

        assert_eq!(bar.text(), "q quit | esc cancel");
    }

    #[test]
    fn truncates_when_too_small() {
        let hints = [KeyHint::new("enter", "select")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        KeyHintBar::new(&hints).render(Rect::new(0, 0, 6, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("enter…"));
    }

    #[test]
    fn hides_when_too_small_and_policy_requests_hide() {
        let hints = [KeyHint::new("enter", "select")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        KeyHintBar::new(&hints)
            .policy(KeyHintBarPolicy::compact().overflow(KeyHintOverflow::Hide))
            .render(Rect::new(0, 0, 6, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("      "));
    }

    #[test]
    fn empty_hints_render_nothing() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        KeyHintBar::new(&[]).render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("        "));
    }

    #[test]
    fn disabled_hints_are_included_in_text() {
        let hints = [KeyHint::new("x", "disabled").disabled(true)];

        assert_eq!(KeyHintBar::new(&hints).text(), "x disabled");
    }

    #[test]
    fn canonical_component_uses_one_layout_for_all_channels() {
        let hints = [KeyHint::new("q", "quit"), KeyHint::new("tab", "focus")];
        let bar = KeyHintBarComponent::new("hints", &hints);
        let mut layout_cx = LayoutCx::new();
        let layout = bar.layout(Constraints::loose(Size::new(24, 2)), &mut layout_cx);
        assert_eq!(layout.size, bmux_tui::component::LogicalSize::new(18, 1));
        assert_eq!(layout.metadata.semantics, ["hint"]);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 2));
        let mut frame = Frame::new(&mut buffer);
        bar.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 18, 1));
        assert_eq!(
            frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .retained_regions(),
            &[Rect::new(0, 0, 18, 1)]
        );
    }

    #[test]
    fn canonical_component_revision_separates_geometry_and_paint() {
        let hints = [KeyHint::new("q", "quit")];
        let initial = KeyHintBarComponent::new("hints", &hints).revision();
        let background = KeyHintBarComponent::new("hints", &hints)
            .policy(KeyHintBarPolicy::compact().background(true))
            .revision();
        assert_eq!(initial.layout, background.layout);
        assert_ne!(initial.paint, background.paint);

        let separator = KeyHintBarComponent::new("hints", &hints)
            .policy(KeyHintBarPolicy::compact().separator(" | "))
            .revision();
        assert_ne!(initial.layout, separator.layout);
    }
}
