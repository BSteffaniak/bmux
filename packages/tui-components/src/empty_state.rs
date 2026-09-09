//! Generic empty/no-results placeholder component.

use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Alignment, Line, TextWrap, TextWrapGeometry};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier, Style};

/// Vertical placement for [`EmptyStateComponent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum EmptyStatePlacement {
    /// Place content at the top of the area.
    #[default]
    Top,
    /// Center content vertically.
    Center,
}

/// Behavior/layout policy for [`EmptyStateComponent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyStatePolicy {
    /// Horizontal alignment.
    pub alignment: Alignment,
    /// Vertical placement.
    pub placement: EmptyStatePlacement,
    /// Inner padding.
    pub padding: Insets,
    /// Wrap long body/action lines.
    pub wrap: bool,
    /// Fill background before rendering.
    pub background: bool,
}

impl EmptyStatePolicy {
    /// Bare top-aligned placeholder.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            alignment: Alignment::Left,
            placement: EmptyStatePlacement::Top,
            padding: Insets::new(0, 0, 0, 0),
            wrap: false,
            background: false,
        }
    }

    /// Centered placeholder suitable for empty panes.
    #[must_use]
    pub const fn centered() -> Self {
        Self {
            alignment: Alignment::Center,
            placement: EmptyStatePlacement::Center,
            padding: Insets::new(0, 0, 0, 0),
            wrap: true,
            background: false,
        }
    }

    /// Return this policy with padding changed.
    #[must_use]
    pub const fn padding(mut self, padding: Insets) -> Self {
        self.padding = padding;
        self
    }

    /// Return this policy with background fill changed.
    #[must_use]
    pub const fn background(mut self, background: bool) -> Self {
        self.background = background;
        self
    }
}

impl Default for EmptyStatePolicy {
    fn default() -> Self {
        Self::centered()
    }
}

/// Visual styles for [`EmptyStateComponent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyStateStyles {
    /// Icon style.
    pub icon: Style,
    /// Title style.
    pub title: Style,
    /// Body style.
    pub body: Style,
    /// Action hint style.
    pub action: Style,
    /// Background fill style.
    pub background: Style,
}

impl Default for EmptyStateStyles {
    fn default() -> Self {
        Self {
            icon: Style::new().fg(Color::BrightBlack),
            title: Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
            body: Style::new().fg(Color::BrightBlack),
            action: Style::new().fg(Color::Cyan),
            background: Style::new(),
        }
    }
}

/// Canonical component-lifecycle empty-state placeholder.
pub struct EmptyStateComponent<'a> {
    id: LayoutId,
    icon: Option<&'a str>,
    title: &'a str,
    body: &'a [Line],
    actions: &'a [Line],
    policy: EmptyStatePolicy,
    styles: EmptyStateStyles,
}

impl<'a> EmptyStateComponent<'a> {
    /// Create an empty-state component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, title: &'a str) -> Self {
        Self {
            id: id.into(),
            icon: None,
            title,
            body: &[],
            actions: &[],
            policy: EmptyStatePolicy::centered(),
            styles: EmptyStateStyles {
                icon: Style::new(),
                title: Style::new(),
                body: Style::new(),
                action: Style::new(),
                background: Style::new(),
            },
        }
    }

    /// Set optional icon.
    #[must_use]
    pub const fn icon(mut self, icon: &'a str) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Set body lines.
    #[must_use]
    pub const fn body(mut self, body: &'a [Line]) -> Self {
        self.body = body;
        self
    }

    /// Set action hint lines.
    #[must_use]
    pub const fn actions(mut self, actions: &'a [Line]) -> Self {
        self.actions = actions;
        self
    }

    /// Set layout policy.
    #[must_use]
    pub const fn policy(mut self, policy: EmptyStatePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: EmptyStateStyles) -> Self {
        self.styles = styles;
        self
    }
}

impl Component for EmptyStateComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.icon.hash(&mut layout);
        self.title.hash(&mut layout);
        for lines in [self.body, self.actions] {
            lines.len().hash(&mut layout);
            for line in lines {
                line.spans.len().hash(&mut layout);
                for span in &line.spans {
                    span.content.hash(&mut layout);
                }
            }
        }
        self.policy.alignment.hash(&mut layout);
        self.policy.placement.hash(&mut layout);
        self.policy.padding.top.hash(&mut layout);
        self.policy.padding.right.hash(&mut layout);
        self.policy.padding.bottom.hash(&mut layout);
        self.policy.padding.left.hash(&mut layout);
        self.policy.wrap.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for line in self.body.iter().chain(self.actions) {
            for span in &line.spans {
                span.style.hash(&mut paint);
            }
        }
        self.policy.background.hash(&mut layout);
        self.policy.background.hash(&mut paint);
        self.styles.icon.hash(&mut paint);
        self.styles.title.hash(&mut paint);
        self.styles.body.hash(&mut paint);
        self.styles.action.hash(&mut paint);
        self.styles.background.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let lines = self.lines();
        let intrinsic_width = lines
            .iter()
            .map(Line::width)
            .max()
            .unwrap_or_default()
            .saturating_add(usize::from(self.policy.padding.horizontal()));
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            u16::try_from(intrinsic_width)
                .unwrap_or(u16::MAX)
                .clamp(constraints.min_width(), constraints.max_width())
        };
        let content_width = width.saturating_sub(self.policy.padding.horizontal());
        let content_height = wrapped_lines(&lines, content_width, self.policy.wrap).len();
        let height = content_height.saturating_add(usize::from(self.policy.padding.vertical()));
        let size = constraints.constrain(LogicalSize::new(width, height));
        let surface = bmux_tui::composition::Surface::new(bmux_tui::composition::Stack::new())
            .background(self.styles.background);
        let children = if self.policy.background {
            vec![ChildLayout::new(
                0,
                0,
                surface.layout(
                    Constraints::new(size.width, size.width, size.height, Some(size.height)),
                    cx,
                ),
            )]
        } else {
            Vec::new()
        };
        LayoutNode::with_children(self.id.clone(), size, children)
            .with_metadata(LayoutMetadata::new().semantic("status"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let full = LocalRect::new(
            0,
            0,
            layout.size.width,
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        if let Some(surface) = layout.children.first() {
            bmux_tui::composition::Surface::new(bmux_tui::composition::Stack::new())
                .background(self.styles.background)
                .paint(&surface.node, cx);
        }

        let horizontal_padding = self.policy.padding.horizontal();
        let content_width = layout.size.width.saturating_sub(horizontal_padding);
        let rows = wrapped_lines(&self.lines(), content_width, self.policy.wrap);
        let available_height = layout
            .size
            .height
            .saturating_sub(usize::from(self.policy.padding.vertical()));
        let visible_rows = rows.len().min(available_height);
        let content_y = usize::from(self.policy.padding.top)
            + match self.policy.placement {
                EmptyStatePlacement::Top => 0,
                EmptyStatePlacement::Center => available_height.saturating_sub(visible_rows) / 2,
            };
        for (index, line) in rows.iter().take(visible_rows).enumerate() {
            let line_width = u16::try_from(line.width()).unwrap_or(u16::MAX);
            let alignment_offset = match self.policy.alignment {
                Alignment::Left => 0,
                Alignment::Center => content_width.saturating_sub(line_width) / 2,
                Alignment::Right => content_width.saturating_sub(line_width),
            };
            cx.write_line(
                LocalRect::new(
                    i32::from(self.policy.padding.left) + i32::from(alignment_offset),
                    i64::try_from(content_y.saturating_add(index)).unwrap_or(i64::MAX),
                    content_width.saturating_sub(alignment_offset),
                    1,
                ),
                line,
            );
        }
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(
                0,
                0,
                layout.size.width,
                u16::try_from(layout.size.height).unwrap_or(u16::MAX),
            ),
            "status",
        ));
        cx.push_damage(full);
    }
}

fn wrapped_lines(lines: &[Line], width: u16, wrap: bool) -> Vec<Line> {
    if width == 0 {
        return Vec::new();
    }
    let policy = if wrap { TextWrap::Word } else { TextWrap::None };
    lines
        .iter()
        .flat_map(|line| line.wrap(TextWrapGeometry::uniform(usize::from(width)), policy))
        .collect()
}

impl EmptyStateComponent<'_> {
    fn lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        if let Some(icon) = self.icon {
            lines.push(Line::from_spans([bmux_tui::prelude::Span::styled(
                icon,
                self.styles.icon,
            )]));
        }
        lines.push(Line::from_spans([bmux_tui::prelude::Span::styled(
            self.title,
            self.styles.title,
        )]));
        lines.extend(
            self.body
                .iter()
                .map(|line| line.with_fallback_style(self.styles.body)),
        );
        lines.extend(
            self.actions
                .iter()
                .map(|line| line.with_fallback_style(self.styles.action)),
        );
        lines
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`EmptyStateStyles`].
    #[must_use]
    pub fn empty_state_styles(self) -> EmptyStateStyles {
        EmptyStateStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for EmptyStateStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            icon: theme.muted,
            title: theme.text.add_modifier(bmux_tui::style::Modifier::BOLD),
            body: theme.muted,
            action: theme.info,
            background: theme.surfaces.normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Insets, Rect, Size};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::prelude::{Alignment, Line};

    use super::{EmptyStateComponent, EmptyStatePlacement, EmptyStatePolicy};

    #[test]
    fn rich_text_content_and_line_boundaries_invalidate_measurement() {
        let original = [Line::from("abcd")];
        let longer = [Line::from("abcdefgh")];
        let split = [Line::from("ab"), Line::from("cd")];
        for actions in [false, true] {
            let mut cache = bmux_tui::component::LayoutCache::new();
            let mut cx = LayoutCx::new();
            for (lines, height) in [
                (original.as_slice(), 2),
                (longer.as_slice(), 3),
                (split.as_slice(), 3),
                (original.as_slice(), 2),
            ] {
                let component =
                    EmptyStateComponent::new("empty", "Title").policy(EmptyStatePolicy {
                        wrap: true,
                        ..EmptyStatePolicy::bare()
                    });
                let component = if actions {
                    component.actions(lines)
                } else {
                    component.body(lines)
                };
                let layout = cache.layout(
                    "empty".into(),
                    &component,
                    Constraints::for_width(5),
                    &mut cx,
                );
                assert_eq!(layout.size.height, height);
            }
            assert_eq!(cx.measured_nodes(), 3);
            assert_eq!(cache.stats().hits, 1);
        }
    }

    #[test]
    fn rich_text_style_changes_reuse_measurement() {
        use bmux_tui::prelude::{Color, Span, Style};

        let original = [Line::from_spans([Span::styled(
            "body",
            Style::new().fg(Color::Red),
        )])];
        let restyled = [Line::from_spans([Span::styled(
            "body",
            Style::new().fg(Color::Blue),
        )])];
        for actions in [false, true] {
            let before =
                EmptyStateComponent::new("empty", "title").policy(EmptyStatePolicy::bare());
            let after = EmptyStateComponent::new("empty", "title").policy(EmptyStatePolicy::bare());
            let (before, after) = if actions {
                (before.actions(&original), after.actions(&restyled))
            } else {
                (before.body(&original), after.body(&restyled))
            };
            assert_ne!(before.revision(), after.revision());
            let mut cache = bmux_tui::component::LayoutCache::new();
            let mut cx = LayoutCx::new();
            let constraints = Constraints::for_width(10);
            let first = cache.layout("empty".into(), &before, constraints, &mut cx);
            let second = cache.layout("empty".into(), &after, constraints, &mut cx);
            assert_eq!(first.size, second.size);
            assert_eq!(cx.measured_nodes(), 1);
            assert_eq!(cache.stats().hits, 1);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
            let mut frame = Frame::new(&mut buffer);
            for (component, layout, color) in [
                (&before, &first, Color::Red),
                (&after, &second, Color::Blue),
            ] {
                component.paint(layout, &mut PaintCx::new(&mut frame));
                assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("body      "));
                for x in 0..4 {
                    let cell = frame
                        .buffer()
                        .get(bmux_tui::geometry::Point::new(x, 1))
                        .unwrap();
                    assert_eq!(cell.style.fg, Some(color));
                }
            }
        }
    }

    #[test]
    fn width_reflow_reuses_prior_layout_without_stale_wrapping() {
        let component = EmptyStateComponent::new("empty", "abcd efgh").policy(EmptyStatePolicy {
            wrap: true,
            ..EmptyStatePolicy::bare()
        });
        let mut cache = bmux_tui::component::LayoutCache::new();
        let mut cx = LayoutCx::new();
        for (width, height, first_row) in [(9, 1, "abcd efgh"), (4, 2, "abcd"), (9, 1, "abcd efgh")]
        {
            let layout = cache.layout(
                "empty".into(),
                &component,
                Constraints::for_width(width),
                &mut cx,
            );
            assert_eq!(layout.size.height, usize::from(height));
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(first_row));
            if height == 2 {
                assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("efgh"));
            }
        }
        assert_eq!(cx.measured_nodes(), 2);
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn component_renders_full_content() {
        let body = [Line::from("Try a search")];
        let actions = [Line::from("Press / to filter")];
        let component = EmptyStateComponent::new("empty", "Nothing found")
            .icon("∅")
            .body(&body)
            .actions(&actions)
            .policy(EmptyStatePolicy::bare());
        let layout = component.layout(Constraints::tight(Size::new(20, 4)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("∅                   ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("Nothing found       ")
        );
        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("Press / to filter   ")
        );
    }

    #[test]
    fn component_measures_wrapped_content_and_paints_scoped_background() {
        let body = [Line::from("a body that wraps")];
        let component = EmptyStateComponent::new("empty", "Nothing")
            .body(&body)
            .policy(EmptyStatePolicy {
                wrap: true,
                background: true,
                ..EmptyStatePolicy::bare()
            });
        let layout = component.layout(Constraints::for_width(8), &mut LayoutCx::new());
        assert_eq!(layout.size.width, 8);
        assert_eq!(layout.size.height, 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("Nothing "));
        assert_eq!(frame.semantics().regions().len(), 1);
        assert!(
            !frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .is_none()
        );
    }

    #[test]
    fn component_centers_content_inside_tight_height() {
        let component = EmptyStateComponent::new("empty", "Empty");
        let layout = component.layout(Constraints::new(10, 10, 5, Some(5)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 5));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("  Empty   "));
    }

    #[test]
    fn canonical_layout_applies_padding_and_top_alignment() {
        let component = EmptyStateComponent::new("empty", "Empty").policy(EmptyStatePolicy {
            placement: EmptyStatePlacement::Top,
            alignment: Alignment::Left,
            padding: Insets::all(1),
            ..EmptyStatePolicy::bare()
        });
        let layout = component.layout(Constraints::tight(Size::new(10, 5)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 5));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some(" Empty    "));
    }

    #[test]
    fn component_style_only_changes_do_not_invalidate_layout() {
        let initial = EmptyStateComponent::new("empty", "Empty").revision();
        let styled = EmptyStateComponent::new("empty", "Empty")
            .styles(super::EmptyStateStyles::default())
            .revision();
        assert_eq!(initial.layout, styled.layout);
    }

    #[test]
    fn zero_width_layout_and_paint_do_not_panic() {
        let component = EmptyStateComponent::new("empty", "Empty");
        let layout = component.layout(Constraints::tight(Size::new(0, 0)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
    }
}
