//! Explicit scrollbar primitive component.

use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Style};

use crate::hit_test::HitRegion;

/// Scrollbar orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarOrientation {
    /// Vertical scrollbar.
    #[default]
    Vertical,
    /// Horizontal scrollbar.
    Horizontal,
}

/// Runtime scrollbar state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollbarState {
    /// Current content offset.
    pub offset: u16,
    /// Total content length in cells/items.
    pub content_len: u16,
    /// Visible viewport length in cells/items.
    pub viewport_len: u16,
    pub(crate) dragging: bool,
}

impl ScrollbarState {
    /// Create scrollbar state.
    #[must_use]
    pub const fn new(content_len: u16, viewport_len: u16) -> Self {
        Self {
            offset: 0,
            content_len,
            viewport_len,
            dragging: false,
        }
    }

    /// Set offset.
    #[must_use]
    pub const fn offset(mut self, offset: u16) -> Self {
        self.offset = offset;
        self
    }

    /// Return maximum offset.
    #[must_use]
    pub const fn max_offset(self) -> u16 {
        self.content_len.saturating_sub(self.viewport_len)
    }

    /// Clamp offset to content bounds.
    pub fn normalize(&mut self) {
        self.offset = self.offset.min(self.max_offset());
    }
}

/// Scrollbar behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarPolicy {
    /// Orientation.
    pub orientation: ScrollbarOrientation,
    /// Begin/end cap symbol.
    pub begin: &'static str,
    /// Track symbol.
    pub track: &'static str,
    /// Thumb symbol.
    pub thumb: &'static str,
    /// End cap symbol.
    pub end: &'static str,
    /// Minimum thumb size.
    pub min_thumb: u16,
    /// Mouse dragging enabled.
    pub mouse_drag: bool,
}

impl ScrollbarPolicy {
    /// Vertical scrollbar policy.
    #[must_use]
    pub const fn vertical() -> Self {
        Self {
            orientation: ScrollbarOrientation::Vertical,
            begin: "│",
            track: "│",
            thumb: "█",
            end: "│",
            min_thumb: 1,
            mouse_drag: true,
        }
    }

    /// Horizontal scrollbar policy.
    #[must_use]
    pub const fn horizontal() -> Self {
        Self {
            orientation: ScrollbarOrientation::Horizontal,
            begin: "─",
            track: "─",
            thumb: "█",
            end: "─",
            min_thumb: 1,
            mouse_drag: true,
        }
    }

    /// Render-only policy.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            orientation: ScrollbarOrientation::Vertical,
            begin: "│",
            track: "│",
            thumb: "█",
            end: "│",
            min_thumb: 1,
            mouse_drag: false,
        }
    }
    /// Return policy with symbols changed.
    #[must_use]
    pub const fn symbols(
        mut self,
        begin: &'static str,
        track: &'static str,
        thumb: &'static str,
        end: &'static str,
    ) -> Self {
        self.begin = begin;
        self.track = track;
        self.thumb = thumb;
        self.end = end;
        self
    }
}

impl Default for ScrollbarPolicy {
    fn default() -> Self {
        Self::vertical()
    }
}

/// Scrollbar styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarStyles {
    /// Begin cap style.
    pub begin: Style,
    /// Track style.
    pub track: Style,
    /// Thumb style.
    pub thumb: Style,
    /// End cap style.
    pub end: Style,
}

impl Default for ScrollbarStyles {
    fn default() -> Self {
        Self {
            begin: Style::new().fg(Color::BrightBlack),
            track: Style::new().fg(Color::BrightBlack),
            thumb: Style::new().fg(Color::BrightCyan),
            end: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Pure scrollbar geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarLayout {
    /// Track length in cells.
    pub track_len: u16,
    /// Thumb start offset along track.
    pub thumb_start: u16,
    /// Thumb length in cells.
    pub thumb_len: u16,
}

/// Scrollbar input outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarOutcome {
    /// Event ignored.
    Ignored,
    /// Visual state changed.
    Redraw,
    /// Offset changed.
    Changed { offset: u16 },
}

/// Explicit scrollbar primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrollbar {
    policy: ScrollbarPolicy,
    styles: ScrollbarStyles,
}

impl Scrollbar {
    /// Create a scrollbar.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            policy: ScrollbarPolicy {
                orientation: ScrollbarOrientation::Vertical,
                begin: "│",
                track: "│",
                thumb: "█",
                end: "│",
                min_thumb: 1,
                mouse_drag: true,
            },
            styles: ScrollbarStyles {
                begin: Style::new(),
                track: Style::new(),
                thumb: Style::new(),
                end: Style::new(),
            },
        }
    }

    /// Set policy.
    #[must_use]
    pub const fn policy(mut self, policy: ScrollbarPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: ScrollbarStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Compute layout for area/state.
    #[must_use]
    pub fn layout(&self, area: Rect, state: &ScrollbarState) -> ScrollbarLayout {
        let track_len = match self.policy.orientation {
            ScrollbarOrientation::Vertical => area.height,
            ScrollbarOrientation::Horizontal => area.width,
        };
        if track_len == 0 || state.content_len == 0 || state.viewport_len >= state.content_len {
            return ScrollbarLayout {
                track_len,
                thumb_start: 0,
                thumb_len: track_len,
            };
        }
        let thumb_len = u16::try_from(
            (u32::from(state.viewport_len) * u32::from(track_len)) / u32::from(state.content_len),
        )
        .unwrap_or(track_len)
        .max(self.policy.min_thumb)
        .min(track_len);
        let travel = track_len.saturating_sub(thumb_len);
        let max_offset = state.max_offset();
        let thumb_start = if max_offset == 0 {
            0
        } else {
            u16::try_from(
                (u32::from(state.offset.min(max_offset)) * u32::from(travel))
                    / u32::from(max_offset),
            )
            .unwrap_or(travel)
        };
        ScrollbarLayout {
            track_len,
            thumb_start,
            thumb_len,
        }
    }

    /// Render scrollbar.
    pub fn render(&self, area: Rect, state: &ScrollbarState, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        let layout = self.layout(area, state);
        match self.policy.orientation {
            ScrollbarOrientation::Vertical => {
                for y in 0..area.height {
                    let (symbol, style) = if y >= layout.thumb_start
                        && y < layout.thumb_start.saturating_add(layout.thumb_len)
                    {
                        (self.policy.thumb, self.styles.thumb)
                    } else if y == 0 {
                        (self.policy.begin, self.styles.begin)
                    } else if y == area.height.saturating_sub(1) {
                        (self.policy.end, self.styles.end)
                    } else {
                        (self.policy.track, self.styles.track)
                    };
                    frame.write_line(
                        Rect::new(area.x, area.y.saturating_add(y), area.width, 1),
                        &Line::from_spans([Span::styled(symbol, style)]),
                    );
                }
            }
            ScrollbarOrientation::Horizontal => {
                let mut spans = Vec::new();
                for x in 0..area.width {
                    let (symbol, style) = if x >= layout.thumb_start
                        && x < layout.thumb_start.saturating_add(layout.thumb_len)
                    {
                        (self.policy.thumb, self.styles.thumb)
                    } else if x == 0 {
                        (self.policy.begin, self.styles.begin)
                    } else if x == area.width.saturating_sub(1) {
                        (self.policy.end, self.styles.end)
                    } else {
                        (self.policy.track, self.styles.track)
                    };
                    spans.push(Span::styled(symbol, style));
                }
                frame.write_line(area, &Line::from_spans(spans));
            }
        }
    }

    /// Handle mouse dragging.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut ScrollbarState,
        event: &Event,
    ) -> ScrollbarOutcome {
        if !self.policy.mouse_drag || area.is_empty() {
            return ScrollbarOutcome::Ignored;
        }
        let Event::Mouse(mouse) = event else {
            return ScrollbarOutcome::Ignored;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if HitRegion::new((), area).contains(mouse.position) =>
            {
                state.dragging = true;
                self.set_from_position(area, state, mouse.position)
            }
            MouseEventKind::Drag(MouseButton::Left) if state.dragging => {
                self.set_from_position(area, state, mouse.position)
            }
            MouseEventKind::Up(MouseButton::Left) if state.dragging => {
                state.dragging = false;
                ScrollbarOutcome::Redraw
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => ScrollbarOutcome::Ignored,
        }
    }

    fn set_from_position(
        &self,
        area: Rect,
        state: &mut ScrollbarState,
        position: Point,
    ) -> ScrollbarOutcome {
        let layout = self.layout(area, state);
        let track_pos = match self.policy.orientation {
            ScrollbarOrientation::Vertical => position.y.saturating_sub(area.y),
            ScrollbarOrientation::Horizontal => position.x.saturating_sub(area.x),
        }
        .min(layout.track_len.saturating_sub(1));
        let travel = layout.track_len.saturating_sub(layout.thumb_len);
        let next = if travel == 0 {
            0
        } else {
            u16::try_from(
                (u32::from(track_pos) * u32::from(state.max_offset())) / u32::from(travel),
            )
            .unwrap_or_else(|_| state.max_offset())
            .min(state.max_offset())
        };
        if next == state.offset {
            ScrollbarOutcome::Ignored
        } else {
            state.offset = next;
            ScrollbarOutcome::Changed { offset: next }
        }
    }
}

impl Default for Scrollbar {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`ScrollbarStyles`].
    #[must_use]
    pub fn scrollbar_styles(self) -> ScrollbarStyles {
        ScrollbarStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for ScrollbarStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        Self {
            begin: theme.border,
            track: theme.border,
            thumb: theme.info,
            end: theme.border,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use bmux_tui::style::{Color, Style};

    use super::{Scrollbar, ScrollbarOutcome, ScrollbarPolicy, ScrollbarState, ScrollbarStyles};

    #[test]
    fn computes_thumb_size_and_position() {
        let state = ScrollbarState::new(100, 20).offset(40);
        let layout = Scrollbar::new().layout(Rect::new(0, 0, 1, 10), &state);

        assert_eq!(layout.thumb_len, 2);
        assert_eq!(layout.thumb_start, 4);
    }

    #[test]
    fn renders_vertical_scrollbar() {
        let state = ScrollbarState::new(100, 20).offset(40);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 5));
        let mut frame = Frame::new(&mut buffer);

        Scrollbar::new().render(Rect::new(0, 0, 1, 5), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("█"));
    }

    #[test]
    fn customizes_begin_end_track_thumb_symbols_and_styles() {
        let state = ScrollbarState::new(100, 20).offset(50);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 5));
        let mut frame = Frame::new(&mut buffer);
        let styles = ScrollbarStyles {
            begin: Style::new().fg(Color::Blue),
            track: Style::new().fg(Color::Green),
            thumb: Style::new().fg(Color::Red),
            end: Style::new().fg(Color::Yellow),
        };

        Scrollbar::new()
            .policy(ScrollbarPolicy::vertical().symbols("^", "|", "#", "v"))
            .styles(styles)
            .render(Rect::new(0, 0, 1, 5), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("^"));
        assert_eq!(frame.buffer().row_symbols(4).as_deref(), Some("v"));
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 0))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Blue))
        );
    }

    #[test]
    fn renders_horizontal_scrollbar() {
        let state = ScrollbarState::new(100, 20).offset(0);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let mut frame = Frame::new(&mut buffer);

        Scrollbar::new()
            .policy(ScrollbarPolicy::horizontal())
            .render(Rect::new(0, 0, 5, 1), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("█────"));
    }

    #[test]
    fn dragging_changes_offset() {
        let mut state = ScrollbarState::new(100, 20);

        assert_eq!(
            Scrollbar::new().handle_event(
                Rect::new(0, 0, 1, 10),
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(0, 9)
                )),
            ),
            ScrollbarOutcome::Changed { offset: 80 }
        );
    }

    #[test]
    fn bare_policy_ignores_drag() {
        let mut state = ScrollbarState::new(100, 20);

        assert_eq!(
            Scrollbar::new()
                .policy(ScrollbarPolicy::bare())
                .handle_event(
                    Rect::new(0, 0, 1, 10),
                    &mut state,
                    &Event::Mouse(MouseEvent::new(
                        MouseEventKind::Down(MouseButton::Left),
                        Point::new(0, 9)
                    )),
                ),
            ScrollbarOutcome::Ignored
        );
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let state = ScrollbarState::new(100, 20);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        Scrollbar::new().render(Rect::new(0, 0, 0, 0), &state, &mut frame);
    }
}
