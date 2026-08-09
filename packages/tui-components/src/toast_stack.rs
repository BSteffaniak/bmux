//! Generic toast / notification stack component.

use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::truncate_to_display_width;

/// Generic toast severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

    /// Render toast stack.
    pub fn render(&self, area: Rect, _state: &ToastStackState, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        for (visible, toast) in self.toasts.iter().take(self.visible_count()).enumerate() {
            let Some(rect) = self.toast_area(area, visible, toast) else {
                continue;
            };
            self.render_toast(rect, toast, frame);
        }
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

    fn render_toast(&self, area: Rect, toast: &ToastItem<'_>, frame: &mut Frame<'_>) {
        let width = usize::from(area.width);
        let close = if self.policy.close_button { " ×" } else { "" };
        let title_width = width.saturating_sub(close.len());
        let title = truncate_to_display_width(toast.title, title_width);
        let title_line = Line::from_spans([
            Span::styled(title, self.title_style(toast.severity)),
            Span::styled(close, self.styles.close),
        ]);
        frame.write_line(area, &title_line);
        if let Some(body) = toast.body
            && area.height > 1
        {
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
                &Line::from_spans([Span::styled(
                    truncate_to_display_width(body, width),
                    self.styles.body,
                )]),
            );
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

    fn toast_area(&self, area: Rect, index: usize, toast: &ToastItem<'_>) -> Option<Rect> {
        let height = u16::from(toast.body.is_some()).saturating_add(1);
        let step = height.saturating_add(self.policy.spacing);
        let width = self.policy.width.min(area.width);
        let x = area.x.saturating_add(area.width.saturating_sub(width));
        let index_offset = u16::try_from(index).ok()?.saturating_mul(step);
        let y = match self.policy.placement {
            ToastPlacement::Top => area.y.saturating_add(index_offset),
            ToastPlacement::Bottom => area.y.saturating_add(
                area.height
                    .saturating_sub(height)
                    .saturating_sub(index_offset),
            ),
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
        Self {
            default: theme.base.add_modifier(bmux_tui::style::Modifier::BOLD),
            info: theme.info.add_modifier(bmux_tui::style::Modifier::BOLD),
            success: theme.success.add_modifier(bmux_tui::style::Modifier::BOLD),
            warning: theme.warning.add_modifier(bmux_tui::style::Modifier::BOLD),
            error: theme.error.add_modifier(bmux_tui::style::Modifier::BOLD),
            body: theme.base,
            close: theme.muted,
            border: theme.border,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{
        ToastItem, ToastSeverity, ToastStack, ToastStackOutcome, ToastStackPolicy, ToastStackState,
    };

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
