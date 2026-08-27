//! Generic empty/no-results placeholder component.

use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata, LayoutNode,
    LogicalSize,
};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Alignment, Line, Text, TextBlock, TextWrap, TextWrapGeometry};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::widget::Widget;

/// Vertical placement for [`EmptyState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum EmptyStatePlacement {
    /// Place content at the top of the area.
    #[default]
    Top,
    /// Center content vertically.
    Center,
}

/// Behavior/layout policy for [`EmptyState`].
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

/// Visual styles for [`EmptyState`].
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

/// Computed empty-state layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyStateLayout {
    /// Area used for content after padding and placement.
    pub content: Rect,
    /// Lines rendered by the component.
    pub lines: Vec<Line>,
}

/// Canonical component-lifecycle empty-state placeholder.
pub struct EmptyStateComponent<'a> {
    id: LayoutId,
    empty_state: EmptyState<'a>,
}

impl<'a> EmptyStateComponent<'a> {
    /// Create an empty-state component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, title: &'a str) -> Self {
        Self {
            id: id.into(),
            empty_state: EmptyState::new(title),
        }
    }

    /// Set optional icon.
    #[must_use]
    pub const fn icon(mut self, icon: &'a str) -> Self {
        self.empty_state.icon = Some(icon);
        self
    }

    /// Set body lines.
    #[must_use]
    pub const fn body(mut self, body: &'a [Line]) -> Self {
        self.empty_state.body = body;
        self
    }

    /// Set action hint lines.
    #[must_use]
    pub const fn actions(mut self, actions: &'a [Line]) -> Self {
        self.empty_state.actions = actions;
        self
    }

    /// Set layout policy.
    #[must_use]
    pub const fn policy(mut self, policy: EmptyStatePolicy) -> Self {
        self.empty_state.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: EmptyStateStyles) -> Self {
        self.empty_state.styles = styles;
        self
    }
}

impl Component for EmptyStateComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.empty_state.icon.hash(&mut layout);
        self.empty_state.title.hash(&mut layout);
        for line in self.empty_state.body {
            format!("{line:?}").hash(&mut layout);
        }
        for line in self.empty_state.actions {
            format!("{line:?}").hash(&mut layout);
        }
        format!("{:?}", self.empty_state.policy.alignment).hash(&mut layout);
        self.empty_state.policy.placement.hash(&mut layout);
        self.empty_state.policy.padding.top.hash(&mut layout);
        self.empty_state.policy.padding.right.hash(&mut layout);
        self.empty_state.policy.padding.bottom.hash(&mut layout);
        self.empty_state.policy.padding.left.hash(&mut layout);
        self.empty_state.policy.wrap.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.empty_state.policy.background.hash(&mut paint);
        self.empty_state.styles.icon.hash(&mut paint);
        self.empty_state.styles.title.hash(&mut paint);
        self.empty_state.styles.body.hash(&mut paint);
        self.empty_state.styles.action.hash(&mut paint);
        self.empty_state.styles.background.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let lines = self.empty_state.lines();
        let intrinsic_width = lines
            .iter()
            .map(Line::width)
            .max()
            .unwrap_or_default()
            .saturating_add(usize::from(self.empty_state.policy.padding.horizontal()));
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            u16::try_from(intrinsic_width)
                .unwrap_or(u16::MAX)
                .clamp(constraints.min_width(), constraints.max_width())
        };
        let content_width = width.saturating_sub(self.empty_state.policy.padding.horizontal());
        let content_height =
            wrapped_lines(&lines, content_width, self.empty_state.policy.wrap).len();
        let height =
            content_height.saturating_add(usize::from(self.empty_state.policy.padding.vertical()));
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, height)),
        )
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
        if self.empty_state.policy.background {
            cx.fill(full, " ", self.empty_state.styles.background);
        }

        let horizontal_padding = self.empty_state.policy.padding.horizontal();
        let content_width = layout.size.width.saturating_sub(horizontal_padding);
        let rows = wrapped_lines(
            &self.empty_state.lines(),
            content_width,
            self.empty_state.policy.wrap,
        );
        let available_height = layout
            .size
            .height
            .saturating_sub(usize::from(self.empty_state.policy.padding.vertical()));
        let visible_rows = rows.len().min(available_height);
        let content_y = usize::from(self.empty_state.policy.padding.top)
            + match self.empty_state.policy.placement {
                EmptyStatePlacement::Top => 0,
                EmptyStatePlacement::Center => available_height.saturating_sub(visible_rows) / 2,
            };
        for (index, line) in rows.iter().take(visible_rows).enumerate() {
            let line_width = u16::try_from(line.width()).unwrap_or(u16::MAX);
            let alignment_offset = match self.empty_state.policy.alignment {
                Alignment::Left => 0,
                Alignment::Center => content_width.saturating_sub(line_width) / 2,
                Alignment::Right => content_width.saturating_sub(line_width),
            };
            cx.write_line(
                LocalRect::new(
                    i32::from(self.empty_state.policy.padding.left) + i32::from(alignment_offset),
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

/// Generic empty/no-results placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyState<'a> {
    icon: Option<&'a str>,
    title: &'a str,
    body: &'a [Line],
    actions: &'a [Line],
    policy: EmptyStatePolicy,
    styles: EmptyStateStyles,
}

impl<'a> EmptyState<'a> {
    /// Create an empty-state placeholder with a title.
    #[must_use]
    pub const fn new(title: &'a str) -> Self {
        Self {
            icon: None,
            title,
            body: &[],
            actions: &[],
            policy: EmptyStatePolicy {
                alignment: Alignment::Center,
                placement: EmptyStatePlacement::Center,
                padding: Insets::new(0, 0, 0, 0),
                wrap: true,
                background: false,
            },
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

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: EmptyStateStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Compute layout and render lines.
    #[must_use]
    pub fn layout(&self, area: Rect) -> EmptyStateLayout {
        let inner = apply_insets(area, self.policy.padding);
        let lines = self.lines();
        let height = u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .min(inner.height);
        let y = match self.policy.placement {
            EmptyStatePlacement::Top => inner.y,
            EmptyStatePlacement::Center => inner
                .y
                .saturating_add(inner.height.saturating_sub(height) / 2),
        };
        EmptyStateLayout {
            content: Rect::new(inner.x, y, inner.width, height),
            lines,
        }
    }

    /// Render the empty-state placeholder.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if self.policy.background {
            frame.fill(area, " ", self.styles.background);
        }
        let layout = self.layout(area);
        if layout.content.is_empty() {
            return;
        }
        let text = Text::from_lines(layout.lines);
        TextBlock::new(text)
            .alignment(self.policy.alignment)
            .wrap(if self.policy.wrap {
                TextWrap::Word
            } else {
                TextWrap::None
            })
            .render(layout.content, frame);
    }

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

const fn apply_insets(area: Rect, insets: Insets) -> Rect {
    Rect::new(
        area.x.saturating_add(insets.left),
        area.y.saturating_add(insets.top),
        area.width.saturating_sub(insets.horizontal()),
        area.height.saturating_sub(insets.vertical()),
    )
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
    use bmux_tui::geometry::{Insets, Rect};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::prelude::{Alignment, Line};

    use super::{EmptyState, EmptyStateComponent, EmptyStatePlacement, EmptyStatePolicy};

    #[test]
    fn renders_title_only() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut frame = Frame::new(&mut buffer);

        EmptyState::new("No rows")
            .policy(EmptyStatePolicy::bare())
            .render(Rect::new(0, 0, 12, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("No rows     ")
        );
    }

    #[test]
    fn renders_full_content() {
        let body = [Line::from("Try a search")];
        let actions = [Line::from("Press / to filter")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
        let mut frame = Frame::new(&mut buffer);

        EmptyState::new("Nothing found")
            .icon("∅")
            .body(&body)
            .actions(&actions)
            .policy(EmptyStatePolicy::bare())
            .render(Rect::new(0, 0, 20, 4), &mut frame);

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
    fn centers_vertically() {
        let layout = EmptyState::new("Empty").layout(Rect::new(0, 0, 10, 5));

        assert_eq!(layout.content.y, 2);
    }

    #[test]
    fn applies_padding() {
        let layout = EmptyState::new("Empty")
            .policy(EmptyStatePolicy::centered().padding(Insets::all(1)))
            .layout(Rect::new(0, 0, 10, 5));

        assert_eq!(layout.content.x, 1);
        assert_eq!(layout.content.width, 8);
    }

    #[test]
    fn top_alignment_starts_at_top() {
        let layout = EmptyState::new("Empty")
            .policy(EmptyStatePolicy {
                placement: EmptyStatePlacement::Top,
                alignment: Alignment::Left,
                ..EmptyStatePolicy::bare()
            })
            .layout(Rect::new(0, 0, 10, 5));

        assert_eq!(layout.content.y, 0);
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
    fn component_style_only_changes_do_not_invalidate_layout() {
        let initial = EmptyStateComponent::new("empty", "Empty").revision();
        let styled = EmptyStateComponent::new("empty", "Empty")
            .styles(super::EmptyStateStyles::default())
            .revision();
        assert_eq!(initial.layout, styled.layout);
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        EmptyState::new("Empty").render(Rect::new(0, 0, 0, 0), &mut frame);
    }
}
