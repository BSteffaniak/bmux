//! Generic empty/no-results placeholder component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::prelude::{Alignment, Line, Text, TextBlock, TextWrap};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::widget::Widget;

/// Vertical placement for [`EmptyState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
                TextWrap::Character
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
        Self {
            icon: theme.muted,
            title: theme.base.add_modifier(bmux_tui::style::Modifier::BOLD),
            body: theme.muted,
            action: theme.info,
            background: theme.background,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Insets, Rect};
    use bmux_tui::prelude::{Alignment, Line};

    use super::{EmptyState, EmptyStatePlacement, EmptyStatePolicy};

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
    fn tiny_area_does_not_panic() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        EmptyState::new("Empty").render(Rect::new(0, 0, 0, 0), &mut frame);
    }
}
