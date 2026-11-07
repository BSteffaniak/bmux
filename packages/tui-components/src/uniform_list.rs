//! Uniform-height virtual list for large collections.
//!
//! Every row is exactly one terminal row tall and is painted on demand by
//! index, so a list of any length costs only its visible rows per frame and
//! nothing is measured or built up front. Use it for long homogeneous lists
//! (search results, inventories, logs); use `selectable-list` for short lists
//! of prebuilt items and `virtual-list` for keyed variable-height items.
//!
//! The caller owns [`UniformListState`]: the selected index plus a
//! [`ScrollViewState`] whose vertical offset is the first visible row. Apps
//! with their own key bindings drive the state's movement methods directly;
//! [`UniformList::handle_event`] provides the default arrow/page/home/end keys,
//! wheel scrolling, click-to-select, and scrollbar dragging.

use std::ops::Range;

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::PaintCx;
use bmux_tui::semantic::SemanticRegion;

use crate::scroll_view::{ScrollExtent, ScrollViewOutcome, ScrollViewState, TrackScrollbar};

/// Caller-owned selection and scroll state of a [`UniformList`].
///
/// Every method takes the row count `len` and the `viewport` height in rows,
/// so the state never goes stale when either changes between frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct UniformListState {
    /// Selected row index.
    pub selected: usize,
    /// Scroll state; its vertical offset is the first visible row.
    pub scroll: ScrollViewState,
}

impl UniformListState {
    /// State with the first row selected, scrolled to the top.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            selected: 0,
            scroll: ScrollViewState::new(),
        }
    }

    /// First visible row index.
    #[must_use]
    pub fn offset(&self) -> usize {
        usize::try_from(self.scroll.vertical_offset()).unwrap_or(usize::MAX)
    }

    fn set_offset(&mut self, offset: usize) {
        self.scroll
            .set_vertical_offset(u64::try_from(offset).unwrap_or(u64::MAX));
    }

    /// Clamps the selection to `len` rows and scrolls the minimum amount to
    /// keep it visible in `viewport` rows.
    pub fn clamp(&mut self, len: usize, viewport: usize) {
        if len == 0 {
            self.selected = 0;
            self.set_offset(0);
            return;
        }
        self.selected = self.selected.min(len - 1);
        let viewport = viewport.max(1);
        let mut offset = self.offset();
        if self.selected < offset {
            offset = self.selected;
        } else if self.selected >= offset.saturating_add(viewport) {
            offset = self.selected + 1 - viewport;
        }
        self.set_offset(offset.min(len.saturating_sub(viewport)));
    }

    /// Selects `index` (clamped) and scrolls it into view.
    pub fn select(&mut self, index: usize, len: usize, viewport: usize) {
        self.selected = index;
        self.clamp(len, viewport);
    }

    /// Moves the selection by `delta` rows, saturating at both ends.
    pub fn move_by(&mut self, delta: isize, len: usize, viewport: usize) {
        self.select(self.selected.saturating_add_signed(delta), len, viewport);
    }

    /// Moves the selection one page (the viewport minus one row of overlap)
    /// down when `forward`, otherwise up.
    pub fn page(&mut self, forward: bool, len: usize, viewport: usize) {
        let step = isize::try_from(viewport.saturating_sub(1).max(1)).unwrap_or(isize::MAX);
        self.move_by(if forward { step } else { -step }, len, viewport);
    }

    /// Selects the first row.
    pub fn top(&mut self, len: usize, viewport: usize) {
        self.select(0, len, viewport);
    }

    /// Selects the last row.
    pub fn bottom(&mut self, len: usize, viewport: usize) {
        self.select(len.saturating_sub(1), len, viewport);
    }

    /// Scrolls by `delta` rows without letting the selection leave the view
    /// (mouse-wheel behavior): the selection is dragged along at the edges.
    pub fn scroll_by(&mut self, delta: isize, len: usize, viewport: usize) {
        let max_offset = len.saturating_sub(viewport.max(1));
        self.set_offset(self.offset().saturating_add_signed(delta).min(max_offset));
        self.keep_selection_visible(len, viewport);
    }

    /// Moves the selection to the nearest visible row after the offset changed.
    fn keep_selection_visible(&mut self, len: usize, viewport: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        let offset = self.offset().min(len - 1);
        let last = offset.saturating_add(viewport.max(1) - 1).min(len - 1);
        self.selected = self.selected.clamp(offset, last);
    }

    /// Indices of the rows visible in `viewport` rows.
    #[must_use]
    pub fn visible(&self, len: usize, viewport: usize) -> Range<usize> {
        let start = self.offset().min(len);
        start..start.saturating_add(viewport).min(len)
    }
}

/// Input behavior of a [`UniformList`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniformListPolicy {
    /// Arrow, page, home/end, and enter keys are handled.
    pub keyboard: bool,
    /// Wheel, click, and scrollbar input are handled.
    pub mouse: bool,
    /// Rows scrolled per wheel notch.
    pub wheel_rows: u16,
}

impl UniformListPolicy {
    /// Keyboard and mouse enabled, three rows per wheel notch.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            keyboard: true,
            mouse: true,
            wheel_rows: 3,
        }
    }
}

impl Default for UniformListPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of routing one event through [`UniformList::handle_event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniformListOutcome {
    /// The event was not handled.
    Ignored,
    /// Visual state changed (e.g. a scrollbar thumb was grabbed) without
    /// moving the view or selection.
    Redraw,
    /// The view scrolled; the selection may have been dragged along.
    Scrolled,
    /// The selection moved to, or a click landed on, the contained row.
    Selected(usize),
    /// Enter was pressed on the contained row.
    Activated(usize),
}

/// Where a [`UniformList`] places its rows and scrollbar inside its area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniformListLayout {
    /// Area the rows are painted in.
    pub rows: Rect,
    /// Scrollbar track in the rightmost column, while the rows overflow.
    pub scrollbar: Option<Rect>,
}

/// A virtual list of `len` one-row items painted on demand.
///
/// The list is rebuilt every frame from the current row count; it holds no
/// item data. Painting clamps the caller's state, so selection and offset stay
/// valid as rows are added or removed.
#[derive(Debug, Clone)]
pub struct UniformList {
    id: String,
    len: usize,
    policy: UniformListPolicy,
    scrollbar: TrackScrollbar,
}

impl UniformList {
    /// A list with stable identity `id` over `len` rows.
    #[must_use]
    pub fn new(id: impl Into<String>, len: usize) -> Self {
        Self {
            id: id.into(),
            len,
            policy: UniformListPolicy::new(),
            scrollbar: TrackScrollbar::new(),
        }
    }

    /// Replace the input policy.
    #[must_use]
    pub const fn policy(mut self, policy: UniformListPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replace the scrollbar's symbols, drag policy, and styles.
    #[must_use]
    pub const fn scrollbar(mut self, scrollbar: TrackScrollbar) -> Self {
        self.scrollbar = scrollbar;
        self
    }

    /// Row count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the list has no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn extent(&self, area: Rect) -> ScrollExtent {
        ScrollExtent {
            content: u64::try_from(self.len).unwrap_or(u64::MAX),
            viewport: u64::from(area.height),
        }
    }

    /// Splits `area` into the rows and, while the rows overflow, a one-column
    /// scrollbar track on the right.
    #[must_use]
    pub fn layout(&self, area: Rect) -> UniformListLayout {
        if self.len > usize::from(area.height) && area.width > 1 {
            UniformListLayout {
                rows: Rect::new(area.x, area.y, area.width - 1, area.height),
                scrollbar: Some(Rect::new(area.right() - 1, area.y, 1, area.height)),
            }
        } else {
            UniformListLayout {
                rows: area,
                scrollbar: None,
            }
        }
    }

    /// Clamps `state` to `area` and paints the visible rows and scrollbar.
    ///
    /// `area` is in the paint context's local coordinates. `paint_row(index,
    /// row_area, selected, cx)` is called once per visible row with a
    /// one-row-tall rectangle; each row also registers a list-item hit region.
    pub fn paint(
        &self,
        area: Rect,
        state: &mut UniformListState,
        cx: &mut PaintCx<'_, '_>,
        mut paint_row: impl FnMut(usize, Rect, bool, &mut PaintCx<'_, '_>),
    ) {
        if area.is_empty() {
            return;
        }
        let viewport = usize::from(area.height);
        state.clamp(self.len, viewport);
        let layout = self.layout(area);
        for (row, index) in state.visible(self.len, viewport).enumerate() {
            let y = layout
                .rows
                .y
                .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
            let row_area = Rect::new(layout.rows.x, y, layout.rows.width, 1);
            paint_row(index, row_area, index == state.selected, cx);
            cx.push_hit(
                HitRegion::new(format!("{}.row.{index}", self.id), row_area)
                    .role(HitRole::ListItem)
                    .pointer_events(self.policy.mouse)
                    .enabled(!state.scroll.interaction.disabled),
            );
        }
        if let Some(track) = layout.scrollbar {
            self.scrollbar
                .paint(track, self.extent(area), &state.scroll, cx);
        }
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "list"));
    }

    /// Handles one input event against the terminal rectangle the list was
    /// painted into.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut UniformListState,
        event: &Event,
    ) -> UniformListOutcome {
        if state.scroll.interaction.disabled || area.is_empty() {
            return UniformListOutcome::Ignored;
        }
        let viewport = usize::from(area.height);
        match event {
            Event::Key(stroke) if self.policy.keyboard => self.handle_key(state, *stroke, viewport),
            Event::Mouse(mouse) if self.policy.mouse => {
                self.handle_mouse(area, state, *mouse, viewport)
            }
            _ => UniformListOutcome::Ignored,
        }
    }

    fn handle_key(
        &self,
        state: &mut UniformListState,
        stroke: KeyStroke,
        viewport: usize,
    ) -> UniformListOutcome {
        if !stroke.modifiers.is_empty() || self.len == 0 {
            return UniformListOutcome::Ignored;
        }
        let before = state.selected;
        match stroke.key {
            KeyCode::Up => state.move_by(-1, self.len, viewport),
            KeyCode::Down => state.move_by(1, self.len, viewport),
            KeyCode::PageUp => state.page(false, self.len, viewport),
            KeyCode::PageDown => state.page(true, self.len, viewport),
            KeyCode::Home => state.top(self.len, viewport),
            KeyCode::End => state.bottom(self.len, viewport),
            KeyCode::Enter => {
                state.clamp(self.len, viewport);
                return UniformListOutcome::Activated(state.selected);
            }
            _ => return UniformListOutcome::Ignored,
        }
        if state.selected == before {
            UniformListOutcome::Ignored
        } else {
            UniformListOutcome::Selected(state.selected)
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut UniformListState,
        mouse: MouseEvent,
        viewport: usize,
    ) -> UniformListOutcome {
        let layout = self.layout(area);
        if let Some(track) = layout.scrollbar {
            let grabbed = state.scroll.dragging_scrollbar();
            let outcome = self.scrollbar.handle_event(
                track,
                self.extent(area),
                &mut state.scroll,
                &Event::Mouse(mouse),
            );
            if matches!(outcome, ScrollViewOutcome::Scrolled { .. }) {
                state.keep_selection_visible(self.len, viewport);
                return UniformListOutcome::Scrolled;
            }
            if grabbed || state.scroll.dragging_scrollbar() {
                return UniformListOutcome::Redraw;
            }
        }
        if !area.contains(mouse.position) {
            return UniformListOutcome::Ignored;
        }
        let wheel = isize::try_from(self.policy.wheel_rows).unwrap_or(isize::MAX);
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let before = state.offset();
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -wheel
                } else {
                    wheel
                };
                state.scroll_by(delta, self.len, viewport);
                if state.offset() == before {
                    UniformListOutcome::Ignored
                } else {
                    UniformListOutcome::Scrolled
                }
            }
            MouseEventKind::Down(MouseButton::Left) if layout.rows.contains(mouse.position) => {
                let index = state
                    .offset()
                    .saturating_add(usize::from(mouse.position.y - layout.rows.y));
                if index < self.len {
                    state.select(index, self.len, viewport);
                    UniformListOutcome::Selected(index)
                } else {
                    UniformListOutcome::Ignored
                }
            }
            _ => UniformListOutcome::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::text::Line;

    use super::{UniformList, UniformListOutcome, UniformListState};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
    }

    fn render(list: &UniformList, state: &mut UniformListState, area: Rect) -> Vec<String> {
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        list.paint(
            area,
            state,
            &mut PaintCx::new(&mut frame),
            |index, row, selected, cx| {
                let marker = if selected { '>' } else { ' ' };
                cx.write_line(
                    LocalRect::terminal(row),
                    &Line::from(format!("{marker}{index}")),
                );
            },
        );
        (area.y..area.bottom())
            .map(|y| frame.buffer().row_symbols(y).unwrap_or_default())
            .collect()
    }

    #[test]
    fn state_keeps_selection_visible_while_moving() {
        let mut s = UniformListState::new();
        s.move_by(15, 20, 5);
        assert_eq!((s.selected, s.offset()), (15, 11));
        s.move_by(-100, 20, 5);
        assert_eq!((s.selected, s.offset()), (0, 0));
        s.page(true, 20, 5);
        assert_eq!(s.selected, 4);
        s.bottom(20, 5);
        assert_eq!((s.selected, s.offset()), (19, 15));
        s.top(20, 5);
        assert_eq!((s.selected, s.offset()), (0, 0));
        s.select(9, 3, 5);
        assert_eq!((s.selected, s.offset()), (2, 0));
        s.clamp(0, 5);
        assert_eq!((s.selected, s.offset()), (0, 0));
    }

    #[test]
    fn wheel_scroll_drags_the_selection_along() {
        let mut s = UniformListState::new();
        s.scroll_by(10, 20, 5);
        assert_eq!((s.selected, s.offset()), (10, 10));
        s.scroll_by(100, 20, 5);
        assert_eq!((s.selected, s.offset()), (15, 15));
        s.move_by(4, 20, 5);
        s.scroll_by(-3, 20, 5);
        assert_eq!((s.selected, s.offset()), (16, 12));
        assert_eq!(s.visible(20, 5), 12..17);
    }

    #[test]
    fn paints_only_visible_rows_with_a_scrollbar_when_overflowing() {
        let list = UniformList::new("list", 100);
        let mut state = UniformListState::new();
        state.select(50, 100, 3);
        let mut painted = Vec::new();
        let area = Rect::new(0, 0, 6, 3);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        list.paint(
            area,
            &mut state,
            &mut PaintCx::new(&mut frame),
            |index, row, selected, _| painted.push((index, row.width, selected)),
        );
        assert_eq!(painted, [(48, 5, false), (49, 5, false), (50, 5, true)]);
        assert_eq!(frame.hits().regions().len(), 3);

        let short = UniformList::new("list", 2);
        let rows = render(&short, &mut UniformListState::new(), area);
        assert_eq!(rows, [">0    ", " 1    ", "      "]);
    }

    #[test]
    fn keys_move_the_selection_and_enter_activates() {
        let list = UniformList::new("list", 10);
        let area = Rect::new(0, 0, 10, 4);
        let mut s = UniformListState::new();
        assert_eq!(
            list.handle_event(area, &mut s, &key(KeyCode::Up)),
            UniformListOutcome::Ignored
        );
        assert_eq!(
            list.handle_event(area, &mut s, &key(KeyCode::Down)),
            UniformListOutcome::Selected(1)
        );
        assert_eq!(
            list.handle_event(area, &mut s, &key(KeyCode::PageDown)),
            UniformListOutcome::Selected(4)
        );
        assert_eq!(
            list.handle_event(area, &mut s, &key(KeyCode::End)),
            UniformListOutcome::Selected(9)
        );
        assert_eq!(
            list.handle_event(area, &mut s, &key(KeyCode::Enter)),
            UniformListOutcome::Activated(9)
        );
        assert_eq!(
            list.handle_event(
                area,
                &mut s,
                &Event::Key(KeyStroke::with_modifiers(
                    KeyCode::Home,
                    Modifiers {
                        ctrl: true,
                        ..Modifiers::NONE
                    }
                ))
            ),
            UniformListOutcome::Ignored
        );
    }

    #[test]
    fn mouse_wheel_scrolls_and_click_selects() {
        let list = UniformList::new("list", 20);
        let area = Rect::new(2, 1, 10, 4);
        let mut s = UniformListState::new();
        assert_eq!(
            list.handle_event(area, &mut s, &mouse(MouseEventKind::ScrollDown, 3, 2)),
            UniformListOutcome::Scrolled
        );
        assert_eq!((s.selected, s.offset()), (3, 3));
        assert_eq!(
            list.handle_event(
                area,
                &mut s,
                &mouse(MouseEventKind::Down(MouseButton::Left), 3, 3)
            ),
            UniformListOutcome::Selected(5)
        );
        // Outside the list.
        assert_eq!(
            list.handle_event(area, &mut s, &mouse(MouseEventKind::ScrollDown, 0, 0)),
            UniformListOutcome::Ignored
        );
    }

    #[test]
    fn scrollbar_track_click_scrolls_and_keeps_selection_visible() {
        let list = UniformList::new("list", 100);
        let area = Rect::new(0, 0, 10, 5);
        let mut s = UniformListState::new();
        let outcome = list.handle_event(
            area,
            &mut s,
            &mouse(MouseEventKind::Down(MouseButton::Left), 9, 4),
        );
        assert_eq!(outcome, UniformListOutcome::Scrolled);
        assert!(s.offset() > 0);
        assert!(s.visible(100, 5).contains(&s.selected));
    }
}
