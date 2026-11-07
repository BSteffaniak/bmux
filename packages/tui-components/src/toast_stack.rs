//! Generic toast / notification stack component.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::{display_width, truncate_to_display_width, wrap_text_with_continuation};

/// Generic toast severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum ToastSeverity {
    /// Default toast.
    #[default]
    Default,
    /// Informational toast.
    Info,
    /// Success toast.
    Success,
    /// Warning toast.
    Warning,
    /// Error toast.
    Error,
}

/// One caller-owned toast item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastItem<'a> {
    /// Stable toast id.
    pub id: &'a str,
    /// Toast title.
    pub title: &'a str,
    /// Optional body text.
    pub body: Option<&'a str>,
    /// Toast severity.
    pub severity: ToastSeverity,
}

impl<'a> ToastItem<'a> {
    /// Create a toast item.
    #[must_use]
    pub const fn new(id: &'a str, title: &'a str) -> Self {
        Self {
            id,
            title,
            body: None,
            severity: ToastSeverity::Default,
        }
    }

    /// Return this toast with body text.
    #[must_use]
    pub const fn body(mut self, body: &'a str) -> Self {
        self.body = Some(body);
        self
    }

    /// Return this toast with severity.
    #[must_use]
    pub const fn severity(mut self, severity: ToastSeverity) -> Self {
        self.severity = severity;
        self
    }
}

/// Toast stack placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum ToastPlacement {
    /// Stack from the top edge downward.
    #[default]
    Top,
    /// Stack from the bottom edge upward.
    Bottom,
}

/// Runtime toast stack state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToastStackState {
    hovered_close: Option<usize>,
    pressed_close: Option<usize>,
}

impl ToastStackState {
    /// Hovered close button index.
    #[must_use]
    pub const fn hovered_close(&self) -> Option<usize> {
        self.hovered_close
    }
}

/// Toast stack behavior/layout policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastStackPolicy {
    /// Stack placement.
    pub placement: ToastPlacement,
    /// Toast width.
    pub width: u16,
    /// Maximum visible toasts.
    pub max_visible: usize,
    /// Spacing rows between toasts.
    pub spacing: u16,
    /// Render close button.
    pub close_button: bool,
    /// Enable mouse close handling.
    pub mouse: bool,
    /// Most body rows per toast. Longer bodies wrap at word boundaries and
    /// end in `…` on the last row ([`ToastStack::is_truncated`] tells).
    pub max_body_lines: u16,
    /// Blank columns inside each toast's left and right edges.
    pub padding: u16,
    /// Draw a severity-colored bar down each toast's left edge.
    pub accent_bar: bool,
}

impl ToastStackPolicy {
    /// Compact top-right style stack.
    #[must_use]
    pub const fn compact() -> Self {
        Self {
            placement: ToastPlacement::Top,
            width: 28,
            max_visible: 3,
            spacing: 1,
            close_button: true,
            mouse: true,
            max_body_lines: 1,
            padding: 0,
            accent_bar: false,
        }
    }

    /// Render-only stack.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            placement: ToastPlacement::Top,
            width: 28,
            max_visible: 3,
            spacing: 1,
            close_button: false,
            mouse: false,
            max_body_lines: 1,
            padding: 0,
            accent_bar: false,
        }
    }

    /// Card-style stack: filled toasts (with [`ToastStackStyles::surface`])
    /// with a severity bar, padding, and bodies wrapped over up to three
    /// rows.
    #[must_use]
    pub const fn cards() -> Self {
        Self {
            placement: ToastPlacement::Top,
            width: 44,
            max_visible: 3,
            spacing: 1,
            close_button: false,
            mouse: false,
            max_body_lines: 3,
            padding: 1,
            accent_bar: true,
        }
    }
}

impl Default for ToastStackPolicy {
    fn default() -> Self {
        Self::compact()
    }
}

/// Toast stack styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastStackStyles {
    /// Default toast style.
    pub default: Style,
    /// Info toast style.
    pub info: Style,
    /// Success toast style.
    pub success: Style,
    /// Warning toast style.
    pub warning: Style,
    /// Error toast style.
    pub error: Style,
    /// Body style.
    pub body: Style,
    /// Close button style.
    pub close: Style,
    /// Border/chrome style.
    pub border: Style,
    /// Fill behind each toast. The default style leaves what is underneath
    /// showing between the text.
    pub surface: Style,
}

impl Default for ToastStackStyles {
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
            body: Style::new().fg(Color::BrightBlack),
            close: Style::new().fg(Color::BrightBlack),
            border: Style::new().fg(Color::BrightBlack),
            surface: Style::new(),
        }
    }
}

/// Toast stack outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastStackOutcome<'a> {
    /// Event ignored.
    Ignored,
    /// Visual state changed.
    Redraw,
    /// Close was requested. Caller owns lifecycle/removal.
    CloseRequested { index: usize, id: &'a str },
}

/// Canonical component-lifecycle toast stack.
pub struct ToastStackComponent<'a, 'state> {
    pointer_intent: bmux_tui::pointer::PointerIntent,
    id: LayoutId,
    stack: ToastStack<'a>,
    state: &'state Cell<ToastStackState>,
}

impl<'a, 'state> ToastStackComponent<'a, 'state> {
    /// Override pointer presentation without changing input routing or mouse policy.
    #[must_use]
    pub const fn pointer_intent(mut self, intent: bmux_tui::pointer::PointerIntent) -> Self {
        self.pointer_intent = intent;
        self
    }

    /// Create a toast stack with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        toasts: &'a [ToastItem<'a>],
        state: &'state Cell<ToastStackState>,
    ) -> Self {
        Self {
            id: id.into(),
            pointer_intent: bmux_tui::pointer::PointerIntent::Auto,
            stack: ToastStack::new(toasts),
            state,
        }
    }

    /// Set layout and interaction policy.
    #[must_use]
    pub const fn policy(mut self, policy: ToastStackPolicy) -> Self {
        self.stack.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: ToastStackStyles) -> Self {
        self.stack.styles = styles;
        self
    }
}

impl ToastStackComponent<'_, '_> {
    fn paint_toast(&self, cx: &mut PaintCx<'_, '_>, rect: Rect, toast: &ToastItem<'_>) {
        let stack = &self.stack;
        let local = |x: u16, y: u16, width: u16, height: u16| {
            LocalRect::new(i32::from(x), i64::from(y), width, height)
        };
        if stack.styles.surface != Style::new() {
            cx.fill(
                local(rect.x, rect.y, rect.width, rect.height),
                " ",
                stack.styles.surface,
            );
        }
        if stack.policy.accent_bar {
            let bar = stack.styles.surface.patch(
                Style::new().fg(stack.title_style(toast.severity).fg.unwrap_or(Color::White)),
            );
            cx.fill(local(rect.x, rect.y, 1, rect.height), "▌", bar);
        }
        if stack.policy.close_button && stack.policy.mouse {
            cx.push_hit(
                HitRegion::new(
                    format!("{}.{}.close", self.id.as_str(), toast.id),
                    Rect::new(rect.right().saturating_sub(1), rect.y, 1, 1),
                )
                .role(HitRole::Action)
                .pointer_intent(self.pointer_intent)
                .hoverable(true)
                .focusable(true),
            );
        }
        let close = if stack.policy.close_button { " ×" } else { "" };
        let x = rect.x.saturating_add(stack.content_offset());
        let width = stack.content_width(rect.width);
        let title =
            truncate_to_display_width(toast.title, usize::from(width).saturating_sub(close.len()));
        let title_width = u16::try_from(display_width(&title))
            .unwrap_or(u16::MAX)
            .saturating_add(u16::try_from(close.len()).unwrap_or(u16::MAX))
            .min(width);
        let surface = stack.styles.surface;
        cx.write_line(
            local(x, rect.y, title_width, 1),
            &Line::from_spans([
                Span::styled(title, surface.patch(stack.title_style(toast.severity))),
                Span::styled(close, surface.patch(stack.styles.close)),
            ]),
        );
        let (lines, _) = stack.body_lines(toast, width);
        for (row, line) in (1..rect.height).zip(lines) {
            let line_width = u16::try_from(display_width(&line))
                .unwrap_or(u16::MAX)
                .min(width);
            cx.write_line(
                local(x, rect.y.saturating_add(row), line_width, 1),
                &Line::from_spans([Span::styled(line, surface.patch(stack.styles.body))]),
            );
        }
    }
}

impl Component for ToastStackComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.stack.policy.width.hash(&mut layout);
        self.stack.policy.max_visible.hash(&mut layout);
        self.stack.policy.spacing.hash(&mut layout);
        self.stack.policy.placement.hash(&mut layout);
        self.stack.policy.max_body_lines.hash(&mut layout);
        self.stack.policy.padding.hash(&mut layout);
        self.stack.policy.accent_bar.hash(&mut layout);
        for toast in self.stack.toasts {
            toast.id.hash(&mut layout);
            toast.title.hash(&mut layout);
            toast.body.hash(&mut layout);
        }

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.pointer_intent).hash(&mut paint);
        self.stack.policy.close_button.hash(&mut paint);
        self.stack.policy.mouse.hash(&mut paint);
        for toast in self.stack.toasts {
            toast.severity.hash(&mut paint);
        }
        format!("{:?}", self.stack.styles).hash(&mut paint);
        format!("{:?}", self.state.get()).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let visible = self.stack.visible_count();
        let content_width = self
            .stack
            .toasts
            .iter()
            .take(visible)
            .flat_map(|toast| std::iter::once(toast.title).chain(toast.body))
            .map(display_width)
            .max()
            .unwrap_or_default()
            .saturating_add(usize::from(self.stack.policy.close_button) * 2)
            .saturating_add(usize::from(self.stack.chrome_width()));
        let width = self
            .stack
            .policy
            .width
            .min(u16::try_from(content_width).unwrap_or(u16::MAX))
            .clamp(
                constraints.min_width().try_into().unwrap_or(u16::MAX),
                constraints.max_width().try_into().unwrap_or(u16::MAX),
            );
        let content_height = self
            .stack
            .toasts
            .iter()
            .take(visible)
            .map(|toast| usize::from(self.stack.toast_height(toast, width)))
            .sum::<usize>()
            .saturating_add(
                usize::from(self.stack.policy.spacing).saturating_mul(visible.saturating_sub(1)),
            );
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                width.into(),
                content_height.try_into().unwrap_or(u64::MAX),
            )),
        )
        .with_metadata(LayoutMetadata::new().semantic("notifications"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let area = Rect::new(
            0,
            0,
            layout.size.width.try_into().unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        for (index, toast) in self
            .stack
            .toasts
            .iter()
            .take(self.stack.visible_count())
            .enumerate()
        {
            let Some(rect) = self.stack.toast_area(area, index, toast) else {
                continue;
            };
            self.paint_toast(cx, rect, toast);
        }
        let local = LocalRect::new(0, 0, area.width, area.height);
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "notifications"));
        cx.push_damage(local);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.get();
        let outcome = self.stack.handle_event(area, &mut state, event);
        self.state.set(state);
        match outcome {
            ToastStackOutcome::Ignored => EventOutcome::Ignored,
            ToastStackOutcome::Redraw | ToastStackOutcome::CloseRequested { .. } => {
                EventOutcome::Redraw
            }
        }
    }
}

/// Generic toast / notification stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastStack<'a> {
    toasts: &'a [ToastItem<'a>],
    policy: ToastStackPolicy,
    styles: ToastStackStyles,
}

impl<'a> ToastStack<'a> {
    /// Create a toast stack over caller-owned toasts.
    #[must_use]
    pub const fn new(toasts: &'a [ToastItem<'a>]) -> Self {
        Self {
            toasts,
            policy: ToastStackPolicy {
                placement: ToastPlacement::Top,
                width: 28,
                max_visible: 3,
                spacing: 1,
                close_button: true,
                mouse: true,
                max_body_lines: 1,
                padding: 0,
                accent_bar: false,
            },
            styles: ToastStackStyles {
                default: Style::new(),
                info: Style::new(),
                success: Style::new(),
                warning: Style::new(),
                error: Style::new(),
                body: Style::new(),
                close: Style::new(),
                border: Style::new(),
                surface: Style::new(),
            },
        }
    }

    /// Set policy.
    #[must_use]
    pub const fn policy(mut self, policy: ToastStackPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: ToastStackStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return visible toast count.
    #[must_use]
    pub fn visible_count(&self) -> usize {
        self.toasts.len().min(self.policy.max_visible)
    }

    /// Handle activation routed from committed semantic interaction metadata.
    pub fn handle_event_for_target(
        &self,
        area: Rect,
        state: &mut ToastStackState,
        event: &Event,
        semantic_target: Option<&str>,
        id_prefix: &str,
    ) -> ToastStackOutcome<'a> {
        if matches!(
            event,
            Event::Key(bmux_keyboard::KeyStroke {
                key: bmux_keyboard::KeyCode::Enter
                    | bmux_keyboard::KeyCode::Space
                    | bmux_keyboard::KeyCode::Char(' '),
                ..
            }) | Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..
            })
        ) && let Some(target) = semantic_target
            && let Some(id) = target
                .strip_prefix(id_prefix)
                .and_then(|suffix| suffix.strip_prefix('.'))
                .and_then(|suffix| suffix.strip_suffix(".close"))
            && let Some((index, toast)) = self
                .toasts
                .iter()
                .take(self.visible_count())
                .enumerate()
                .find(|(_, toast)| toast.id == id)
        {
            return ToastStackOutcome::CloseRequested {
                index,
                id: toast.id,
            };
        }
        self.handle_event(area, state, event)
    }

    /// Handle mouse close interaction.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut ToastStackState,
        event: &Event,
    ) -> ToastStackOutcome<'a> {
        if !self.policy.mouse || !self.policy.close_button {
            return ToastStackOutcome::Ignored;
        }
        let Event::Mouse(mouse) = event else {
            return ToastStackOutcome::Ignored;
        };
        self.handle_mouse(area, state, *mouse)
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut ToastStackState,
        mouse: MouseEvent,
    ) -> ToastStackOutcome<'a> {
        match mouse.kind {
            MouseEventKind::Move => {
                let hovered = self.close_at(area, mouse.position);
                if hovered == state.hovered_close {
                    ToastStackOutcome::Ignored
                } else {
                    state.hovered_close = hovered;
                    ToastStackOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                state.pressed_close = self.close_at(area, mouse.position);
                ToastStackOutcome::Redraw
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let released = self.close_at(area, mouse.position);
                let pressed = state.pressed_close.take();
                if released == pressed
                    && let Some(index) = released
                    && let Some(toast) = self.toasts.get(index)
                {
                    return ToastStackOutcome::CloseRequested {
                        index,
                        id: toast.id,
                    };
                }
                ToastStackOutcome::Redraw
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => ToastStackOutcome::Ignored,
        }
    }

    fn close_at(&self, area: Rect, position: Point) -> Option<usize> {
        self.toasts
            .iter()
            .take(self.visible_count())
            .enumerate()
            .find_map(|(index, toast)| {
                let rect = self.toast_area(area, index, toast)?;
                let close_x = rect.x.saturating_add(rect.width.saturating_sub(1));
                (position.y == rect.y && position.x == close_x).then_some(index)
            })
    }

    /// Columns taken by the accent bar and padding.
    const fn chrome_width(&self) -> u16 {
        (self.policy.accent_bar as u16).saturating_add(self.policy.padding.saturating_mul(2))
    }

    /// Column offset of text inside a toast.
    const fn content_offset(&self) -> u16 {
        (self.policy.accent_bar as u16).saturating_add(self.policy.padding)
    }

    /// Text width inside a toast `width` columns wide.
    const fn content_width(&self, width: u16) -> u16 {
        width.saturating_sub(self.chrome_width())
    }

    /// Body rows of `toast` at text width `width`, and whether text was cut.
    fn body_lines(&self, toast: &ToastItem<'_>, width: u16) -> (Vec<String>, bool) {
        let Some(body) = toast.body else {
            return (Vec::new(), false);
        };
        let width = usize::from(width.max(1));
        let max = usize::from(self.policy.max_body_lines.max(1));
        let mut lines: Vec<String> = body
            .lines()
            .flat_map(|line| {
                let wrapped = wrap_text_with_continuation(line, width, width);
                if wrapped.is_empty() {
                    vec![String::new()]
                } else {
                    wrapped
                }
            })
            .collect();
        if lines.len() <= max {
            return (lines, false);
        }
        lines.truncate(max);
        if let Some(last) = lines.last_mut() {
            // Room for the ellipsis on the last row.
            let mut cut = truncate_to_display_width(last, width.saturating_sub(1));
            if cut.ends_with('…') {
                cut.pop();
            }
            *last = format!("{}…", cut.trim_end());
        }
        (lines, true)
    }

    /// Rows `toast` takes in a stack `width` columns wide.
    fn toast_height(&self, toast: &ToastItem<'_>, width: u16) -> u16 {
        let width = self.policy.width.min(width);
        let rows = self.body_lines(toast, self.content_width(width)).0.len();
        u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(1)
    }

    /// Whether the body of the toast at `index` was cut short in `area` (so
    /// callers can offer the full text, for example on click).
    #[must_use]
    pub fn is_truncated(&self, area: Rect, index: usize) -> bool {
        self.toasts.get(index).is_some_and(|toast| {
            let width = self.policy.width.min(area.width);
            self.body_lines(toast, self.content_width(width)).1
        })
    }

    /// Rectangles of the visible toasts in `area`, by index (`None` for
    /// toasts that do not fit).
    #[must_use]
    pub fn toast_rects(&self, area: Rect) -> Vec<Option<Rect>> {
        (0..self.visible_count())
            .map(|index| {
                self.toasts
                    .get(index)
                    .and_then(|toast| self.toast_area(area, index, toast))
            })
            .collect()
    }

    fn toast_area(&self, area: Rect, index: usize, toast: &ToastItem<'_>) -> Option<Rect> {
        let height = self.toast_height(toast, area.width);
        let offset = self
            .toasts
            .iter()
            .take(index)
            .map(|earlier| {
                self.toast_height(earlier, area.width)
                    .saturating_add(self.policy.spacing)
            })
            .fold(0u16, u16::saturating_add);
        let width = self.policy.width.min(area.width);
        let x = area.x.saturating_add(area.width.saturating_sub(width));
        let y = match self.policy.placement {
            ToastPlacement::Top => area.y.saturating_add(offset),
            ToastPlacement::Bottom => area
                .y
                .saturating_add(area.height.saturating_sub(height).saturating_sub(offset)),
        };
        (y >= area.y && y.saturating_add(height) <= area.y.saturating_add(area.height))
            .then_some(Rect::new(x, y, width, height))
    }

    const fn title_style(&self, severity: ToastSeverity) -> Style {
        match severity {
            ToastSeverity::Default => self.styles.default,
            ToastSeverity::Info => self.styles.info,
            ToastSeverity::Success => self.styles.success,
            ToastSeverity::Warning => self.styles.warning,
            ToastSeverity::Error => self.styles.error,
        }
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`ToastStackStyles`].
    #[must_use]
    pub fn toast_stack_styles(self) -> ToastStackStyles {
        ToastStackStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for ToastStackStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Raised);
        Self {
            default: theme.text.add_modifier(bmux_tui::style::Modifier::BOLD),
            info: theme
                .info
                .add_modifier(bmux_tui::style::Modifier::BOLD)
                .patch(theme.state_surfaces.info),
            success: theme
                .success
                .add_modifier(bmux_tui::style::Modifier::BOLD)
                .patch(theme.state_surfaces.success),
            warning: theme
                .warning
                .add_modifier(bmux_tui::style::Modifier::BOLD)
                .patch(theme.state_surfaces.warning),
            error: theme
                .error
                .add_modifier(bmux_tui::style::Modifier::BOLD)
                .patch(theme.state_surfaces.error),
            body: theme.text,
            close: theme.muted,
            border: theme.border,
            surface: theme
                .surfaces
                .resolve(crate::theme::ComponentSurfaceDepth::Raised),
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx, LogicalSize};
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::paint::{LocalRect, PaintCx};

    use super::{
        ToastItem, ToastSeverity, ToastStack, ToastStackComponent, ToastStackOutcome,
        ToastStackPolicy, ToastStackState,
    };

    trait ToastStackTestRender {
        fn render(&self, area: Rect, state: &ToastStackState, frame: &mut Frame<'_>);
        fn render_with_id_prefix(
            &self,
            area: Rect,
            state: &ToastStackState,
            frame: &mut Frame<'_>,
            id_prefix: &'static str,
        );
    }

    impl ToastStackTestRender for ToastStack<'_> {
        fn render(&self, area: Rect, state: &ToastStackState, frame: &mut Frame<'_>) {
            self.render_with_id_prefix(area, state, frame, "test.toasts");
        }

        fn render_with_id_prefix(
            &self,
            area: Rect,
            state: &ToastStackState,
            frame: &mut Frame<'_>,
            id_prefix: &'static str,
        ) {
            let state = std::cell::Cell::new(*state);
            let component = ToastStackComponent {
                pointer_intent: bmux_tui::pointer::PointerIntent::Auto,
                id: id_prefix.into(),
                stack: *self,
                state: &state,
            };
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            PaintCx::new(frame).with_child(
                i32::from(area.x),
                i64::from(area.y),
                LocalRect::new(0, 0, area.width, area.height),
                |cx| component.paint(&layout, cx),
            );
        }
    }

    #[test]
    fn pointer_override_invalidates_paint_without_remeasuring_layout() {
        use bmux_tui::pointer::{PointerIntent, PointerShape};

        let toasts = [ToastItem::new("one", "Saved").body("Changes persisted")];
        let state = std::cell::Cell::new(ToastStackState::default());
        let component = ToastStackComponent::new("toasts", &toasts, &state);
        let original = component.revision();
        let component = component.pointer_intent(PointerIntent::Hidden);
        assert_eq!(component.pointer_intent, PointerIntent::Hidden);
        let hidden = component.revision();
        assert_eq!(original.layout, hidden.layout);
        assert_ne!(original.paint, hidden.paint);
        let component = component.pointer_intent(PointerIntent::Shape(PointerShape::Crosshair));
        assert_eq!(
            component.pointer_intent,
            PointerIntent::Shape(PointerShape::Crosshair)
        );
        assert_ne!(hidden.paint, component.revision().paint);
    }

    #[test]
    fn component_measures_paints_and_registers_close_controls() {
        let toasts = [ToastItem::new("one", "Saved").body("Changes persisted")];
        let state = std::cell::Cell::new(ToastStackState::default());
        let component = ToastStackComponent::new("toasts", &toasts, &state);
        let layout = component.layout(Constraints::new(0, 24, 0, None), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(19, 2));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 19, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("Changes persisted  ")
        );
        assert_eq!(frame.hits().regions().len(), 1);
        assert_eq!(frame.semantics().regions().len(), 1);
    }

    #[test]
    fn component_respects_nonzero_local_toast_placement() {
        let toasts = [ToastItem::new("one", "Saved")];
        let state = std::cell::Cell::new(ToastStackState::default());
        let component =
            ToastStackComponent::new("toasts", &toasts, &state).policy(ToastStackPolicy::compact());
        let layout = component.layout(Constraints::tight(Size::new(24, 2)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 2));
        let mut frame = Frame::new(&mut buffer);

        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Saved ×                 ")
        );
    }

    #[test]
    fn component_measurement_respects_visible_limit_and_spacing() {
        let toasts = [
            ToastItem::new("one", "One"),
            ToastItem::new("two", "Two").body("Body"),
            ToastItem::new("three", "Three"),
        ];
        let state = std::cell::Cell::new(ToastStackState::default());
        let component =
            ToastStackComponent::new("toasts", &toasts, &state).policy(ToastStackPolicy {
                max_visible: 2,
                spacing: 1,
                ..ToastStackPolicy::compact()
            });
        let layout = component.layout(Constraints::new(0, 24, 0, None), &mut LayoutCx::new());
        assert_eq!(layout.size.height, 4);
    }

    #[test]
    fn renders_toast_title_and_body() {
        let toasts = [ToastItem::new("one", "Saved").body("Changes persisted")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 2));
        let mut frame = Frame::new(&mut buffer);

        ToastStack::new(&toasts).render(
            Rect::new(0, 0, 24, 2),
            &ToastStackState::default(),
            &mut frame,
        );

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Saved ×                 ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("Changes persisted       ")
        );
    }

    #[test]
    fn card_bodies_wrap_and_stack_by_their_height() {
        let toasts = [
            ToastItem::new("one", "Scan failed")
                .body("the network has 2046 hosts, more than discovery.max-hosts (256) allows")
                .severity(ToastSeverity::Error),
            ToastItem::new("two", "Saved").body("ok"),
        ];
        let policy = ToastStackPolicy {
            width: 24,
            max_body_lines: 2,
            ..ToastStackPolicy::cards()
        };
        let area = Rect::new(0, 0, 24, 8);
        let stack = ToastStack::new(&toasts).policy(policy);
        assert_eq!(
            stack.toast_rects(area),
            [Some(Rect::new(0, 0, 24, 3)), Some(Rect::new(0, 4, 24, 2))]
        );
        assert!(stack.is_truncated(area, 0));
        assert!(!stack.is_truncated(area, 1));

        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        stack.render(area, &ToastStackState::default(), &mut frame);
        let rows: Vec<String> = (0..6)
            .map(|y| frame.buffer().row_symbols(y).unwrap_or_default())
            .collect();
        assert_eq!(
            rows,
            [
                "▌ Scan failed           ",
                "▌ the network has 2046  ",
                "▌ hosts, more than…     ",
                "                        ",
                "▌ Saved                 ",
                "▌ ok                    ",
            ]
        );
    }

    #[test]
    fn respects_max_visible() {
        let toasts = [
            ToastItem::new("one", "One"),
            ToastItem::new("two", "Two"),
            ToastItem::new("three", "Three"),
        ];

        assert_eq!(
            ToastStack::new(&toasts)
                .policy(ToastStackPolicy {
                    max_visible: 2,
                    ..ToastStackPolicy::compact()
                })
                .visible_count(),
            2
        );
    }

    #[test]
    fn mouse_close_requests_close() {
        let toasts = [ToastItem::new("one", "Saved")];
        let stack = ToastStack::new(&toasts);
        let mut state = ToastStackState::default();
        let area = Rect::new(0, 0, 24, 2);

        let _ = stack.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(23, 0),
            )),
        );
        assert_eq!(
            stack.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(23, 0)
                )),
            ),
            ToastStackOutcome::CloseRequested {
                index: 0,
                id: "one"
            }
        );
    }

    #[test]
    fn committed_semantic_close_target_uses_stable_toast_id() {
        let toasts = [ToastItem::new("one", "Saved")];
        let stack = ToastStack::new(&toasts);
        let mut state = ToastStackState::default();
        let area = Rect::new(8, 3, 24, 2);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        stack.render_with_id_prefix(area, &state, &mut frame, "notice");

        assert_eq!(frame.hits().regions()[0].id.as_str(), "notice.one.close");
        assert_eq!(frame.hits().regions()[0].area, Rect::new(31, 3, 1, 1));
        assert_eq!(
            stack.handle_event_for_target(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(99, 99),
                )),
                Some("notice.one.close"),
                "notice",
            ),
            ToastStackOutcome::CloseRequested {
                index: 0,
                id: "one"
            }
        );
    }

    #[test]
    fn bare_policy_ignores_events() {
        let toasts = [ToastItem::new("one", "Saved")];
        let mut state = ToastStackState::default();

        assert_eq!(
            ToastStack::new(&toasts)
                .policy(ToastStackPolicy::bare())
                .handle_event(
                    Rect::new(0, 0, 24, 2),
                    &mut state,
                    &Event::Mouse(MouseEvent::new(MouseEventKind::Move, Point::new(23, 0))),
                ),
            ToastStackOutcome::Ignored
        );
    }

    #[test]
    fn supports_severity_constructors() {
        let toasts = [
            ToastItem::new("info", "Info").severity(ToastSeverity::Info),
            ToastItem::new("success", "Success").severity(ToastSeverity::Success),
            ToastItem::new("warning", "Warning").severity(ToastSeverity::Warning),
            ToastItem::new("error", "Error").severity(ToastSeverity::Error),
        ];

        assert_eq!(ToastStack::new(&toasts).visible_count(), 3);
    }
}
