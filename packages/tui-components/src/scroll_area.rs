//! Reusable scroll-area behavior and renderer.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::text_width::display_width;

use crate::common::InteractionState;
use crate::scrollbar::{Scrollbar, ScrollbarOutcome, ScrollbarPolicy, ScrollbarState};

/// Runtime scroll-area state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollAreaState {
    /// Common scroll-area interaction flags.
    pub interaction: InteractionState,
    vertical_offset: u16,
    horizontal_offset: u16,
}

impl ScrollAreaState {
    /// Create enabled scroll-area state at the top of the content.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            interaction: InteractionState::new(),
            vertical_offset: 0,
            horizontal_offset: 0,
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

    /// Return the current horizontal content offset in terminal cells.
    #[must_use]
    pub const fn horizontal_offset(self) -> u16 {
        self.horizontal_offset
    }

    /// Set the horizontal content offset in terminal cells.
    pub const fn set_horizontal_offset(&mut self, offset: u16) {
        self.horizontal_offset = offset;
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
    /// Integrated vertical scrollbar layout mode.
    pub scrollbar: ScrollAreaScrollbarMode,
    /// Vertical scrollbar policy used when integrated scrollbar rendering is enabled.
    pub scrollbar_policy: ScrollbarPolicy,
    /// Integrated horizontal scrollbar layout mode.
    pub horizontal_scrollbar: ScrollAreaScrollbarMode,
    /// Horizontal scrollbar policy used when integrated scrollbar rendering is enabled.
    pub horizontal_scrollbar_policy: ScrollbarPolicy,
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
            horizontal_scrollbar: ScrollAreaScrollbarMode::Hidden,
            horizontal_scrollbar_policy: ScrollbarPolicy::horizontal(),
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
            horizontal_scrollbar: ScrollAreaScrollbarMode::Hidden,
            horizontal_scrollbar_policy: ScrollbarPolicy::bare(),
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

    /// Return this policy with integrated horizontal scrollbar mode changed.
    #[must_use]
    pub const fn horizontal_scrollbar(
        mut self,
        horizontal_scrollbar: ScrollAreaScrollbarMode,
    ) -> Self {
        self.horizontal_scrollbar = horizontal_scrollbar;
        self
    }

    /// Return this policy with integrated horizontal scrollbar policy changed.
    #[must_use]
    pub const fn horizontal_scrollbar_policy(
        mut self,
        horizontal_scrollbar_policy: ScrollbarPolicy,
    ) -> Self {
        self.horizontal_scrollbar_policy = horizontal_scrollbar_policy;
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
    /// The visible vertical offset changed.
    Scrolled { vertical_offset: u16 },
    /// The visible horizontal offset changed.
    HorizontalScrolled { horizontal_offset: u16 },
}

impl ScrollAreaOutcome {
    /// Return true when the event was handled.
    #[must_use]
    pub const fn is_handled(self) -> bool {
        matches!(
            self,
            Self::Handled | Self::Scrolled { .. } | Self::HorizontalScrolled { .. }
        )
    }

    /// Return true when rendering should be refreshed.
    #[must_use]
    pub const fn needs_redraw(self) -> bool {
        matches!(
            self,
            Self::Scrolled { .. } | Self::HorizontalScrolled { .. }
        )
    }
}

/// Scroll area over caller-owned line content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollArea<'a> {
    lines: &'a [Line],
    policy: ScrollAreaPolicy,
}

/// Resolved scroll-area layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollAreaLayout {
    /// Content viewport area.
    pub content: Rect,
    /// Vertical scrollbar area, when enabled.
    pub vertical_scrollbar: Option<Rect>,
    /// Horizontal scrollbar area, when enabled.
    pub horizontal_scrollbar: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollAxis {
    Vertical,
    Horizontal,
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
                horizontal_scrollbar: ScrollAreaScrollbarMode::Hidden,
                horizontal_scrollbar_policy: ScrollbarPolicy::horizontal(),
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

    /// Return the maximum line width in terminal cells.
    #[must_use]
    pub fn content_width(&self) -> u16 {
        self.lines
            .iter()
            .map(Line::width)
            .max()
            .and_then(|width| u16::try_from(width).ok())
            .unwrap_or(u16::MAX)
    }

    /// Return the maximum valid horizontal offset for `area`.
    #[must_use]
    pub fn max_horizontal_offset(&self, area: Rect) -> u16 {
        self.content_width().saturating_sub(area.width)
    }

    /// Return resolved content and scrollbar layout for `area`.
    #[must_use]
    pub const fn layout(&self, area: Rect) -> ScrollAreaLayout {
        let reserve_vertical =
            matches!(self.policy.scrollbar, ScrollAreaScrollbarMode::Gutter) && area.width > 0;
        let reserve_horizontal = matches!(
            self.policy.horizontal_scrollbar,
            ScrollAreaScrollbarMode::Gutter
        ) && area.height > 0;
        let content = Rect::new(
            area.x,
            area.y,
            if reserve_vertical {
                area.width.saturating_sub(1)
            } else {
                area.width
            },
            if reserve_horizontal {
                area.height.saturating_sub(1)
            } else {
                area.height
            },
        );
        let vertical_scrollbar = if matches!(self.policy.scrollbar, ScrollAreaScrollbarMode::Hidden)
            || area.width == 0
        {
            None
        } else {
            Some(Rect::new(
                area.right().saturating_sub(1),
                area.y,
                1,
                content.height,
            ))
        };
        let horizontal_scrollbar = if matches!(
            self.policy.horizontal_scrollbar,
            ScrollAreaScrollbarMode::Hidden
        ) || area.height == 0
        {
            None
        } else {
            Some(Rect::new(
                area.x,
                area.bottom().saturating_sub(1),
                content.width,
                1,
            ))
        };
        ScrollAreaLayout {
            content,
            vertical_scrollbar,
            horizontal_scrollbar,
        }
    }

    /// Return content area after integrated scrollbar reservation.
    #[must_use]
    pub const fn content_area(&self, area: Rect) -> Rect {
        self.layout(area).content
    }

    /// Return integrated vertical scrollbar area when enabled.
    #[must_use]
    pub const fn scrollbar_area(&self, area: Rect) -> Option<Rect> {
        self.layout(area).vertical_scrollbar
    }

    /// Return integrated horizontal scrollbar area when enabled.
    #[must_use]
    pub const fn horizontal_scrollbar_area(&self, area: Rect) -> Option<Rect> {
        self.layout(area).horizontal_scrollbar
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
        let layout = self.layout(area);
        let range = self.visible_range(layout.content, state);
        for (row, line) in self.lines[range].iter().enumerate() {
            let y = layout
                .content
                .y
                .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
            let visible_line = line_viewport(line, state.horizontal_offset, layout.content.width);
            frame.write_line_with_fallback_style(
                Rect::new(layout.content.x, y, layout.content.width, 1),
                &visible_line,
                fallback,
            );
        }
        if let Some(scrollbar_area) = layout.vertical_scrollbar {
            let scrollbar_state = ScrollbarState::new(self.content_height(), layout.content.height)
                .offset(state.vertical_offset);
            Scrollbar::new()
                .policy(self.policy.scrollbar_policy)
                .render(scrollbar_area, &scrollbar_state, frame);
        }
        if let Some(scrollbar_area) = layout.horizontal_scrollbar {
            let scrollbar_state = ScrollbarState::new(self.content_width(), layout.content.width)
                .offset(state.horizontal_offset);
            Scrollbar::new()
                .policy(self.policy.horizontal_scrollbar_policy)
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
                self.scroll_by_axis(ScrollAxis::Vertical, self.content_area(area), state, -1)
            }
            KeyCode::Down if self.policy.arrows_scroll => {
                self.scroll_by_axis(ScrollAxis::Vertical, self.content_area(area), state, 1)
            }
            KeyCode::Left if self.policy.arrows_scroll => {
                self.scroll_by_axis(ScrollAxis::Horizontal, self.content_area(area), state, -1)
            }
            KeyCode::Right if self.policy.arrows_scroll => {
                self.scroll_by_axis(ScrollAxis::Horizontal, self.content_area(area), state, 1)
            }
            KeyCode::PageUp if self.policy.page_keys_scroll => self.scroll_by_axis(
                ScrollAxis::Vertical,
                self.content_area(area),
                state,
                -i32::from(area.height.max(1)),
            ),
            KeyCode::PageDown if self.policy.page_keys_scroll => self.scroll_by_axis(
                ScrollAxis::Vertical,
                self.content_area(area),
                state,
                i32::from(area.height.max(1)),
            ),
            KeyCode::Home if self.policy.home_end_scroll => {
                self.scroll_to_axis(ScrollAxis::Vertical, self.content_area(area), state, 0)
            }
            KeyCode::End if self.policy.home_end_scroll => self.scroll_to_axis(
                ScrollAxis::Vertical,
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
        if let Some(outcome) = self.handle_scrollbar_axis(ScrollAxis::Vertical, area, state, mouse)
        {
            return outcome;
        }
        if let Some(outcome) =
            self.handle_scrollbar_axis(ScrollAxis::Horizontal, area, state, mouse)
        {
            return outcome;
        }
        if !area.contains(mouse.position) {
            return ScrollAreaOutcome::Ignored;
        }
        let content_area = self.content_area(area);
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by_axis(
                ScrollAxis::Vertical,
                content_area,
                state,
                -i32::from(self.policy.wheel_lines),
            ),
            MouseEventKind::ScrollDown => self.scroll_by_axis(
                ScrollAxis::Vertical,
                content_area,
                state,
                i32::from(self.policy.wheel_lines),
            ),
            MouseEventKind::ScrollLeft => self.scroll_by_axis(
                ScrollAxis::Horizontal,
                content_area,
                state,
                -i32::from(self.policy.wheel_lines),
            ),
            MouseEventKind::ScrollRight => self.scroll_by_axis(
                ScrollAxis::Horizontal,
                content_area,
                state,
                i32::from(self.policy.wheel_lines),
            ),
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move => ScrollAreaOutcome::Ignored,
        }
    }

    fn handle_scrollbar_axis(
        &self,
        axis: ScrollAxis,
        area: Rect,
        state: &mut ScrollAreaState,
        mouse: MouseEvent,
    ) -> Option<ScrollAreaOutcome> {
        let layout = self.layout(area);
        let scrollbar_area = match axis {
            ScrollAxis::Vertical => layout.vertical_scrollbar,
            ScrollAxis::Horizontal => layout.horizontal_scrollbar,
        }?;
        let mut scrollbar_state =
            ScrollbarState::new(self.content_len(axis), axis.viewport_len(layout.content))
                .offset(axis.offset(*state));
        scrollbar_state.dragging = state.interaction.pressed;
        let policy = match axis {
            ScrollAxis::Vertical => self.policy.scrollbar_policy,
            ScrollAxis::Horizontal => self.policy.horizontal_scrollbar_policy,
        };
        match Scrollbar::new().policy(policy).handle_event(
            scrollbar_area,
            &mut scrollbar_state,
            &Event::Mouse(mouse),
        ) {
            ScrollbarOutcome::Changed { offset } => {
                axis.set_offset(state, offset);
                state.interaction.pressed = scrollbar_state.dragging;
                Some(axis.scrolled(offset))
            }
            ScrollbarOutcome::Redraw => {
                state.interaction.pressed = scrollbar_state.dragging;
                Some(ScrollAreaOutcome::Handled)
            }
            ScrollbarOutcome::Ignored => {
                state.interaction.pressed = scrollbar_state.dragging;
                state
                    .interaction
                    .pressed
                    .then_some(ScrollAreaOutcome::Handled)
            }
        }
    }

    fn content_len(&self, axis: ScrollAxis) -> u16 {
        match axis {
            ScrollAxis::Vertical => self.content_height(),
            ScrollAxis::Horizontal => self.content_width(),
        }
    }

    fn max_offset(&self, axis: ScrollAxis, area: Rect) -> u16 {
        self.content_len(axis)
            .saturating_sub(axis.viewport_len(area))
    }

    fn scroll_by_axis(
        &self,
        axis: ScrollAxis,
        area: Rect,
        state: &mut ScrollAreaState,
        delta: i32,
    ) -> ScrollAreaOutcome {
        let next = offset_u16(axis.offset(*state), delta);
        self.scroll_to_axis(axis, area, state, next)
    }

    fn scroll_to_axis(
        &self,
        axis: ScrollAxis,
        area: Rect,
        state: &mut ScrollAreaState,
        offset: u16,
    ) -> ScrollAreaOutcome {
        let next = offset.min(self.max_offset(axis, area));
        if axis.offset(*state) == next {
            ScrollAreaOutcome::Handled
        } else {
            axis.set_offset(state, next);
            axis.scrolled(next)
        }
    }

    fn normalize_state(&self, area: Rect, state: &mut ScrollAreaState) {
        state.vertical_offset = state
            .vertical_offset
            .min(self.max_offset(ScrollAxis::Vertical, area));
        state.horizontal_offset = state
            .horizontal_offset
            .min(self.max_offset(ScrollAxis::Horizontal, area));
    }
}

impl ScrollAxis {
    const fn viewport_len(self, area: Rect) -> u16 {
        match self {
            Self::Vertical => area.height,
            Self::Horizontal => area.width,
        }
    }

    const fn offset(self, state: ScrollAreaState) -> u16 {
        match self {
            Self::Vertical => state.vertical_offset,
            Self::Horizontal => state.horizontal_offset,
        }
    }

    const fn set_offset(self, state: &mut ScrollAreaState, offset: u16) {
        match self {
            Self::Vertical => state.vertical_offset = offset,
            Self::Horizontal => state.horizontal_offset = offset,
        }
    }

    const fn scrolled(self, offset: u16) -> ScrollAreaOutcome {
        match self {
            Self::Vertical => ScrollAreaOutcome::Scrolled {
                vertical_offset: offset,
            },
            Self::Horizontal => ScrollAreaOutcome::HorizontalScrolled {
                horizontal_offset: offset,
            },
        }
    }
}

fn line_viewport(line: &Line, horizontal_offset: u16, width: u16) -> Line {
    if horizontal_offset == 0 {
        return line.clone();
    }
    let start = usize::from(horizontal_offset);
    let end = start.saturating_add(usize::from(width));
    let mut cursor = 0usize;
    let mut spans = Vec::new();
    for span in &line.spans {
        let mut content = String::new();
        for ch in span.content.chars() {
            let grapheme = ch.to_string();
            let grapheme_width = display_width(&grapheme);
            if grapheme_width == 0 {
                continue;
            }
            let next = cursor.saturating_add(grapheme_width);
            if next <= start {
                cursor = next;
                continue;
            }
            if cursor >= end || next > end {
                break;
            }
            content.push(ch);
            cursor = next;
        }
        if !content.is_empty() {
            spans.push(Span::styled(content, span.style));
        }
        if cursor >= end {
            break;
        }
    }
    Line::from_spans(spans)
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
    fn line_viewport_slices_text() {
        let line = Line::from("abcdef");
        assert_eq!(super::line_viewport(&line, 2, 3).plain_text(), "cde");
    }

    #[test]
    fn renders_horizontal_viewport_from_offset() {
        let lines = lines(&["abcdef"]);
        let area = ScrollArea::new(&lines);
        let mut state = ScrollAreaState::new();
        state.set_horizontal_offset(2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        area.render(Rect::new(0, 0, 3, 1), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("cde"));
    }

    #[test]
    fn right_key_scrolls_one_cell() {
        let lines = lines(&["abcdef"]);
        let area = ScrollArea::new(&lines);
        let mut state = ScrollAreaState::new();

        let outcome = area.handle_event(
            Rect::new(0, 0, 3, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Right)),
        );

        assert_eq!(
            outcome,
            ScrollAreaOutcome::HorizontalScrolled {
                horizontal_offset: 1
            }
        );
        assert_eq!(state.horizontal_offset(), 1);
    }

    #[test]
    fn mouse_horizontal_wheel_scrolls_inside_area() {
        let lines = lines(&["abcdefghi"]);
        let area = ScrollArea::new(&lines);
        let mut state = ScrollAreaState::new();

        let outcome = area.handle_event(
            Rect::new(0, 0, 3, 1),
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::ScrollRight,
                Point::new(1, 0),
            )),
        );

        assert_eq!(
            outcome,
            ScrollAreaOutcome::HorizontalScrolled {
                horizontal_offset: 3
            }
        );
        assert_eq!(state.horizontal_offset(), 3);
    }

    #[test]
    fn renders_horizontal_overlay_and_gutter_scrollbars_when_enabled() {
        let lines = lines(&["abcdef"]);
        let mut state = ScrollAreaState::new();
        state.set_horizontal_offset(1);

        let overlay = ScrollArea::new(&lines).policy(
            super::ScrollAreaPolicy::interactive()
                .horizontal_scrollbar(ScrollAreaScrollbarMode::Overlay),
        );
        let mut overlay_buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut overlay_frame = Frame::new(&mut overlay_buffer);
        overlay.render(Rect::new(0, 0, 3, 1), &state, &mut overlay_frame);
        assert_eq!(
            overlay_frame.buffer().row_symbols(0).as_deref(),
            Some("█──")
        );

        let gutter = ScrollArea::new(&lines).policy(
            super::ScrollAreaPolicy::interactive()
                .horizontal_scrollbar(ScrollAreaScrollbarMode::Gutter),
        );
        let mut gutter_buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        let mut gutter_frame = Frame::new(&mut gutter_buffer);
        gutter.render(Rect::new(0, 0, 3, 2), &state, &mut gutter_frame);
        assert_eq!(gutter_frame.buffer().row_symbols(0).as_deref(), Some("bcd"));
        assert_eq!(gutter_frame.buffer().row_symbols(1).as_deref(), Some("█──"));
    }

    #[test]
    fn horizontal_scrollbar_drag_handoff_scrolls_area() {
        let lines = lines(&["abcdefghi"]);
        let area = ScrollArea::new(&lines).policy(
            super::ScrollAreaPolicy::interactive()
                .horizontal_scrollbar(ScrollAreaScrollbarMode::Overlay),
        );
        let mut state = ScrollAreaState::new();

        let outcome = area.handle_event(
            Rect::new(0, 0, 3, 1),
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(2, 0),
            )),
        );

        assert!(outcome.is_handled());
        assert!(state.horizontal_offset() > 0);
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
