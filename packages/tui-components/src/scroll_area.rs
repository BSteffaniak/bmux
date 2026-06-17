//! Reusable scroll-area behavior and renderer.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Style};

use crate::common::InteractionState;
use crate::scrollbar::{Scrollbar, ScrollbarOutcome, ScrollbarPolicy, ScrollbarState};

/// Runtime scroll-area state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollAreaState {
    /// Common scroll-area interaction flags.
    pub interaction: InteractionState,
    vertical_offset: u16,
}

impl ScrollAreaState {
    /// Create enabled scroll-area state at the top of the content.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            interaction: InteractionState::new(),
            vertical_offset: 0,
        }
    }

    /// Return the current vertical content offset.
    #[must_use]
    pub const fn vertical_offset(self) -> u16 {
        self.vertical_offset
    }

    /// Set the vertical content offset.
    pub const fn set_vertical_offset(&mut self, offset: u16) {
        self.vertical_offset = offset;
    }

    /// Set disabled state for the scroll area.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
    }
}

impl Default for ScrollAreaState {
    fn default() -> Self {
        Self::new()
    }
}

/// Scrollbar layout mode for scroll areas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAreaScrollbarMode {
    /// Do not render an integrated scrollbar.
    Hidden,
    /// Render scrollbar over the right edge of the content area.
    Overlay,
    /// Reserve a one-cell gutter on the right for the scrollbar.
    Gutter,
}

/// Configurable scroll-area behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ScrollAreaPolicy {
    /// Whether keyboard input can scroll the area.
    pub keyboard: bool,
    /// Whether mouse-wheel input can scroll the area.
    pub mouse_wheel: bool,
    /// Whether Up and Down scroll by one line.
    pub arrows_scroll: bool,
    /// Whether `PageUp` and `PageDown` scroll by one page.
    pub page_keys_scroll: bool,
    /// Whether Home and End scroll to the beginning/end.
    pub home_end_scroll: bool,
    /// Integrated scrollbar layout mode.
    pub scrollbar: ScrollAreaScrollbarMode,
    /// Scrollbar policy used when integrated scrollbar rendering is enabled.
    pub scrollbar_policy: ScrollbarPolicy,
    /// Number of lines moved for each mouse-wheel event.
    pub wheel_lines: u16,
}

impl ScrollAreaPolicy {
    /// Common interactive scroll-area behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            keyboard: true,
            mouse_wheel: true,
            arrows_scroll: true,
            page_keys_scroll: true,
            home_end_scroll: true,
            scrollbar: ScrollAreaScrollbarMode::Hidden,
            scrollbar_policy: ScrollbarPolicy::vertical(),
            wheel_lines: 3,
        }
    }

    /// Disable all scrolling input.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            keyboard: false,
            mouse_wheel: false,
            arrows_scroll: false,
            page_keys_scroll: false,
            home_end_scroll: false,
            scrollbar: ScrollAreaScrollbarMode::Hidden,
            scrollbar_policy: ScrollbarPolicy::bare(),
            wheel_lines: 0,
        }
    }
    /// Return this policy with integrated scrollbar mode changed.
    #[must_use]
    pub const fn scrollbar(mut self, scrollbar: ScrollAreaScrollbarMode) -> Self {
        self.scrollbar = scrollbar;
        self
    }

    /// Return this policy with integrated scrollbar policy changed.
    #[must_use]
    pub const fn scrollbar_policy(mut self, scrollbar_policy: ScrollbarPolicy) -> Self {
        self.scrollbar_policy = scrollbar_policy;
        self
    }
}

impl Default for ScrollAreaPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Outcome from scroll-area input handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAreaOutcome {
    /// Event was not handled.
    Ignored,
    /// Event was handled without changing the visible offset.
    Handled,
    /// The visible offset changed.
    Scrolled { vertical_offset: u16 },
}

impl ScrollAreaOutcome {
    /// Return true when the event was handled.
    #[must_use]
    pub const fn is_handled(self) -> bool {
        matches!(self, Self::Handled | Self::Scrolled { .. })
    }

    /// Return true when rendering should be refreshed.
    #[must_use]
    pub const fn needs_redraw(self) -> bool {
        matches!(self, Self::Scrolled { .. })
    }
}

/// Vertical scroll area over caller-owned line content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollArea<'a> {
    lines: &'a [Line],
    policy: ScrollAreaPolicy,
}

impl<'a> ScrollArea<'a> {
    /// Create a scroll area over caller-owned lines.
    #[must_use]
    pub const fn new(lines: &'a [Line]) -> Self {
        Self {
            lines,
            policy: ScrollAreaPolicy {
                keyboard: true,
                mouse_wheel: true,
                arrows_scroll: true,
                page_keys_scroll: true,
                home_end_scroll: true,
                scrollbar: ScrollAreaScrollbarMode::Hidden,
                scrollbar_policy: ScrollbarPolicy::vertical(),
                wheel_lines: 3,
            },
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: ScrollAreaPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Return the number of content lines.
    #[must_use]
    pub fn content_height(&self) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }

    /// Return the maximum valid vertical offset for `area`.
    #[must_use]
    pub fn max_vertical_offset(&self, area: Rect) -> u16 {
        self.content_height().saturating_sub(area.height)
    }

    /// Return content area after integrated scrollbar reservation.
    #[must_use]
    pub const fn content_area(&self, area: Rect) -> Rect {
        if matches!(self.policy.scrollbar, ScrollAreaScrollbarMode::Gutter) && area.width > 0 {
            Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height)
        } else {
            area
        }
    }

    /// Return integrated scrollbar area when enabled.
    #[must_use]
    pub const fn scrollbar_area(&self, area: Rect) -> Option<Rect> {
        if matches!(self.policy.scrollbar, ScrollAreaScrollbarMode::Hidden) || area.width == 0 {
            return None;
        }
        Some(Rect::new(
            area.right().saturating_sub(1),
            area.y,
            1,
            area.height,
        ))
    }

    /// Return the visible content line range for `area` and `state`.
    #[must_use]
    pub fn visible_range(&self, area: Rect, state: &ScrollAreaState) -> std::ops::Range<usize> {
        let start = usize::from(state.vertical_offset.min(self.max_vertical_offset(area)));
        let end = start
            .saturating_add(usize::from(area.height))
            .min(self.lines.len());
        start..end
    }

    /// Render visible lines.
    pub fn render(&self, area: Rect, state: &ScrollAreaState, frame: &mut Frame<'_>) {
        self.render_with_fallback_style(area, state, frame, Style::new());
    }

    /// Render visible lines with a fallback style filling each rendered row.
    pub fn render_with_fallback_style(
        &self,
        area: Rect,
        state: &ScrollAreaState,
        frame: &mut Frame<'_>,
        fallback: Style,
    ) {
        let content_area = self.content_area(area);
        let range = self.visible_range(content_area, state);
        for (row, line) in self.lines[range].iter().enumerate() {
            let y = area
                .y
                .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
            frame.write_line_with_fallback_style(
                Rect::new(content_area.x, y, content_area.width, 1),
                line,
                fallback,
            );
        }
        if let Some(scrollbar_area) = self.scrollbar_area(area) {
            let scrollbar_state = ScrollbarState::new(self.content_height(), content_area.height)
                .offset(state.vertical_offset);
            Scrollbar::new()
                .policy(self.policy.scrollbar_policy)
                .render(scrollbar_area, &scrollbar_state, frame);
        }
    }

    /// Handle one input event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut ScrollAreaState,
        event: &Event,
    ) -> ScrollAreaOutcome {
        self.normalize_state(self.content_area(area), state);
        if state.interaction.disabled {
            return ScrollAreaOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) if self.policy.keyboard => self.handle_key(area, state, *stroke),
            Event::Mouse(mouse) if self.policy.mouse_wheel => {
                self.handle_mouse(area, state, *mouse)
            }
            Event::Key(_)
            | Event::Mouse(_)
            | Event::Resize(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => ScrollAreaOutcome::Ignored,
        }
    }

    fn handle_key(
        &self,
        area: Rect,
        state: &mut ScrollAreaState,
        stroke: KeyStroke,
    ) -> ScrollAreaOutcome {
        if !stroke.modifiers.is_empty() {
            return ScrollAreaOutcome::Ignored;
        }
        match stroke.key {
            KeyCode::Up if self.policy.arrows_scroll => {
                self.scroll_by(self.content_area(area), state, -1)
            }
            KeyCode::Down if self.policy.arrows_scroll => {
                self.scroll_by(self.content_area(area), state, 1)
            }
            KeyCode::PageUp if self.policy.page_keys_scroll => self.scroll_by(
                self.content_area(area),
                state,
                -i32::from(area.height.max(1)),
            ),
            KeyCode::PageDown if self.policy.page_keys_scroll => self.scroll_by(
                self.content_area(area),
                state,
                i32::from(area.height.max(1)),
            ),
            KeyCode::Home if self.policy.home_end_scroll => {
                self.scroll_to(self.content_area(area), state, 0)
            }
            KeyCode::End if self.policy.home_end_scroll => self.scroll_to(
                self.content_area(area),
                state,
                self.max_vertical_offset(self.content_area(area)),
            ),
            KeyCode::Char(_)
            | KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Escape
            | KeyCode::Space
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Insert
            | KeyCode::F(_) => ScrollAreaOutcome::Ignored,
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut ScrollAreaState,
        mouse: MouseEvent,
    ) -> ScrollAreaOutcome {
        if let Some(scrollbar_area) = self.scrollbar_area(area) {
            let mut scrollbar_state =
                ScrollbarState::new(self.content_height(), self.content_area(area).height)
                    .offset(state.vertical_offset);
            scrollbar_state.dragging = state.interaction.pressed;
            match Scrollbar::new()
                .policy(self.policy.scrollbar_policy)
                .handle_event(scrollbar_area, &mut scrollbar_state, &Event::Mouse(mouse))
            {
                ScrollbarOutcome::Changed { offset } => {
                    state.vertical_offset = offset;
                    state.interaction.pressed = scrollbar_state.dragging;
                    return ScrollAreaOutcome::Scrolled {
                        vertical_offset: offset,
                    };
                }
                ScrollbarOutcome::Redraw => {
                    state.interaction.pressed = scrollbar_state.dragging;
                    return ScrollAreaOutcome::Handled;
                }
                ScrollbarOutcome::Ignored => {
                    state.interaction.pressed = scrollbar_state.dragging;
                    if state.interaction.pressed {
                        return ScrollAreaOutcome::Handled;
                    }
                }
            }
        }
        if !area.contains(mouse.position) {
            return ScrollAreaOutcome::Ignored;
        }
        let content_area = self.content_area(area);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_by(content_area, state, -i32::from(self.policy.wheel_lines))
            }
            MouseEventKind::ScrollDown => {
                self.scroll_by(content_area, state, i32::from(self.policy.wheel_lines))
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => ScrollAreaOutcome::Ignored,
        }
    }

    fn scroll_by(&self, area: Rect, state: &mut ScrollAreaState, delta: i32) -> ScrollAreaOutcome {
        let next = offset_u16(state.vertical_offset, delta);
        self.scroll_to(area, state, next)
    }

    fn scroll_to(&self, area: Rect, state: &mut ScrollAreaState, offset: u16) -> ScrollAreaOutcome {
        let next = offset.min(self.max_vertical_offset(area));
        if state.vertical_offset == next {
            ScrollAreaOutcome::Handled
        } else {
            state.vertical_offset = next;
            ScrollAreaOutcome::Scrolled {
                vertical_offset: next,
            }
        }
    }

    fn normalize_state(&self, area: Rect, state: &mut ScrollAreaState) {
        state.vertical_offset = state.vertical_offset.min(self.max_vertical_offset(area));
    }
}

fn offset_u16(value: u16, delta: i32) -> u16 {
    u16::try_from((i32::from(value) + delta).max(0)).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::prelude::Line;

    use super::{ScrollArea, ScrollAreaOutcome, ScrollAreaScrollbarMode, ScrollAreaState};

    #[test]
    fn renders_visible_lines_from_offset() {
        let lines = lines(&["zero", "one", "two", "three"]);
        let area = ScrollArea::new(&lines);
        let mut state = ScrollAreaState::new();
        state.set_vertical_offset(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut frame = Frame::new(&mut buffer);

        area.render(Rect::new(0, 0, 8, 2), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("one     "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("two     "));
    }

    #[test]
    fn renders_overlay_and_gutter_scrollbars_when_enabled() {
        let lines = lines(&["zero", "one", "two", "three"]);
        let mut state = ScrollAreaState::new();
        state.set_vertical_offset(1);

        let overlay = ScrollArea::new(&lines).policy(
            super::ScrollAreaPolicy::interactive().scrollbar(ScrollAreaScrollbarMode::Overlay),
        );
        let mut overlay_buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut overlay_frame = Frame::new(&mut overlay_buffer);
        overlay.render(Rect::new(0, 0, 8, 2), &state, &mut overlay_frame);
        assert_eq!(
            overlay_frame.buffer().row_symbols(0).as_deref(),
            Some("one    █")
        );

        let gutter = ScrollArea::new(&lines).policy(
            super::ScrollAreaPolicy::interactive().scrollbar(ScrollAreaScrollbarMode::Gutter),
        );
        let mut gutter_buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut gutter_frame = Frame::new(&mut gutter_buffer);
        gutter.render(Rect::new(0, 0, 8, 2), &state, &mut gutter_frame);
        assert_eq!(
            gutter_frame.buffer().row_symbols(0).as_deref(),
            Some("one    █")
        );
    }

    #[test]
    fn scrollbar_drag_handoff_scrolls_area() {
        let lines = lines(&["zero", "one", "two", "three", "four"]);
        let area = ScrollArea::new(&lines).policy(
            super::ScrollAreaPolicy::interactive().scrollbar(ScrollAreaScrollbarMode::Overlay),
        );
        let mut state = ScrollAreaState::new();

        let outcome = area.handle_event(
            Rect::new(0, 0, 8, 2),
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(7, 1),
            )),
        );

        assert!(outcome.is_handled());
        assert!(state.vertical_offset() > 0);
    }

    #[test]
    fn down_key_scrolls_one_line() {
        let lines = lines(&["zero", "one", "two", "three"]);
        let area = ScrollArea::new(&lines);
        let mut state = ScrollAreaState::new();

        let outcome = area.handle_event(
            Rect::new(0, 0, 8, 2),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );

        assert_eq!(outcome, ScrollAreaOutcome::Scrolled { vertical_offset: 1 });
        assert_eq!(state.vertical_offset(), 1);
    }

    #[test]
    fn page_down_clamps_to_max_offset() {
        let lines = lines(&["zero", "one", "two", "three"]);
        let area = ScrollArea::new(&lines);
        let mut state = ScrollAreaState::new();

        let outcome = area.handle_event(
            Rect::new(0, 0, 8, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::PageDown)),
        );

        assert_eq!(outcome, ScrollAreaOutcome::Scrolled { vertical_offset: 1 });
        assert_eq!(state.vertical_offset(), 1);
    }

    #[test]
    fn mouse_wheel_scrolls_inside_area() {
        let lines = lines(&["zero", "one", "two", "three", "four"]);
        let area = ScrollArea::new(&lines);
        let mut state = ScrollAreaState::new();

        let outcome = area.handle_event(
            Rect::new(0, 0, 8, 2),
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::ScrollDown,
                Point::new(1, 1),
            )),
        );

        assert_eq!(outcome, ScrollAreaOutcome::Scrolled { vertical_offset: 3 });
        assert_eq!(state.vertical_offset(), 3);
    }

    #[test]
    fn disabled_scroll_area_ignores_events() {
        let lines = lines(&["zero", "one", "two"]);
        let area = ScrollArea::new(&lines);
        let mut state = ScrollAreaState::new();
        state.set_disabled(true);

        let outcome = area.handle_event(
            Rect::new(0, 0, 8, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );

        assert_eq!(outcome, ScrollAreaOutcome::Ignored);
        assert_eq!(state.vertical_offset(), 0);
    }

    fn lines(values: &[&str]) -> Vec<Line> {
        values.iter().map(|value| Line::from(*value)).collect()
    }
}
