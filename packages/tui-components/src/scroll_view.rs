//! Interactive state and policy for arbitrary measured scroll content.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{
    ChildLayout, Component, Constraints, Element, EventCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::selection::{
    SelectionAutoScrollRequest, SelectionScrollAxis, SelectionScrollDirection,
};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Style};

use crate::common::InteractionState;
use crate::scrollbar::{
    Scrollbar, ScrollbarOutcome, ScrollbarPolicy, ScrollbarState, ScrollbarStyles,
};

/// Stable layout identity and viewport-relative row used across relayout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollAnchor {
    id: LayoutId,
    viewport_row: i64,
}

impl ScrollAnchor {
    /// Stable identity retained by this anchor.
    #[must_use]
    pub const fn id(&self) -> &LayoutId {
        &self.id
    }

    /// Signed target row relative to the viewport top.
    #[must_use]
    pub const fn viewport_row(&self) -> i64 {
        self.viewport_row
    }
}

/// Caller-owned logical scroll state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollViewState {
    /// Common enabled and focus state.
    pub interaction: InteractionState,
    vertical_offset: usize,
    horizontal_offset: usize,
    follow_bottom: bool,
}

impl ScrollViewState {
    /// Create enabled scroll state at the top.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            interaction: InteractionState::new(),
            vertical_offset: 0,
            horizontal_offset: 0,
            follow_bottom: false,
        }
    }

    /// Current logical vertical offset.
    #[must_use]
    pub const fn vertical_offset(self) -> usize {
        self.vertical_offset
    }

    /// Set a logical vertical offset before clamping against layout.
    pub const fn set_vertical_offset(&mut self, offset: usize) {
        self.vertical_offset = offset;
        self.follow_bottom = false;
    }

    /// Current logical horizontal offset.
    #[must_use]
    pub const fn horizontal_offset(self) -> usize {
        self.horizontal_offset
    }

    /// Set a logical horizontal offset before clamping against layout.
    pub const fn set_horizontal_offset(&mut self, offset: usize) {
        self.horizontal_offset = offset;
    }

    /// Select whether layout changes remain anchored at the bottom.
    pub const fn set_follow_bottom(&mut self, follow: bool) {
        self.follow_bottom = follow;
    }

    /// Whether layout changes remain bottom anchored.
    #[must_use]
    pub const fn follows_bottom(self) -> bool {
        self.follow_bottom
    }
}

impl Default for ScrollViewState {
    fn default() -> Self {
        Self::new()
    }
}

/// Generic vertical scroll behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollViewPolicy {
    /// Keyboard navigation enabled.
    pub keyboard: bool,
    /// Mouse-wheel navigation enabled.
    pub mouse_wheel: bool,
    /// Render and route an integrated vertical scrollbar gutter.
    pub scrollbar: bool,
    /// Logical rows per wheel event.
    pub wheel_rows: usize,
}

impl ScrollViewPolicy {
    /// Common keyboard and mouse behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            keyboard: true,
            mouse_wheel: true,
            scrollbar: true,
            wheel_rows: 3,
        }
    }
}

impl Default for ScrollViewPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Result of handling one scroll event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollViewOutcome {
    /// Event was ignored.
    Ignored,
    /// Logical offset changed.
    Scrolled { vertical_offset: usize },
}

/// Result of routing one event through nested vertical scroll views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestedScrollOutcome {
    /// Neither viewport consumed the event.
    Ignored,
    /// The innermost viewport consumed the event.
    Inner(ScrollViewOutcome),
    /// The enclosing viewport consumed an event handed off at the inner edge.
    Outer(ScrollViewOutcome),
}

/// A generic scroll viewport over one arbitrary component subtree.
pub struct ScrollViewComponent<'a> {
    id: LayoutId,
    viewport: LogicalSize,
    offset_x: usize,
    offset_y: usize,
    child: Element<'a>,
}

impl<'a> ScrollViewComponent<'a> {
    /// Create a viewport with caller-owned logical offsets.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        viewport: LogicalSize,
        state: ScrollViewState,
        child: impl Component + 'a,
    ) -> Self {
        Self {
            id: id.into(),
            viewport,
            offset_x: state.horizontal_offset(),
            offset_y: state.vertical_offset(),
            child: Element::new(child),
        }
    }
}

impl Component for ScrollViewComponent<'_> {
    fn layout(
        &self,
        constraints: Constraints,
        cx: &mut bmux_tui::component::LayoutCx,
    ) -> LayoutNode {
        let child = self
            .child
            .layout(Constraints::for_width(self.viewport.width), cx);
        let size = constraints.constrain(self.viewport);
        LayoutNode::with_children(self.id.clone(), size, vec![ChildLayout::new(0, 0, child)])
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let Some(child) = layout.children.first() else {
            return;
        };
        let viewport_height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        cx.with_child(
            -i32::try_from(self.offset_x).unwrap_or(i32::MAX),
            -i64::try_from(self.offset_y).unwrap_or(i64::MAX),
            LocalRect::new(
                i32::try_from(self.offset_x).unwrap_or(i32::MAX),
                i64::try_from(self.offset_y).unwrap_or(i64::MAX),
                layout.size.width,
                viewport_height,
            ),
            |cx| self.child.paint(&child.node, cx),
        );
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(child) = layout.children.first() else {
            return EventOutcome::Ignored;
        };
        let clip = cx.clip().unwrap_or_else(|| {
            Rect::new(
                0,
                0,
                layout.size.width,
                u16::try_from(layout.size.height).unwrap_or(u16::MAX),
            )
        });
        cx.with_transform(
            u16::try_from(self.offset_x).unwrap_or(u16::MAX),
            self.offset_y,
            -i32::try_from(self.offset_x).unwrap_or(i32::MAX),
            -i64::try_from(self.offset_y).unwrap_or(i64::MAX),
            clip,
            |cx| self.child.event(event, &child.node, cx),
        )
    }
}

/// Generic interaction controller for one measured scroll viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollView {
    policy: ScrollViewPolicy,
    scrollbar_styles: ScrollbarStyles,
}

impl ScrollView {
    /// Create an interactive scroll controller.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            policy: ScrollViewPolicy::interactive(),
            scrollbar_styles: ScrollbarStyles {
                begin: Style::new().fg(Color::BrightBlack),
                track: Style::new().fg(Color::BrightBlack),
                thumb: Style::new().fg(Color::BrightCyan),
                end: Style::new().fg(Color::BrightBlack),
            },
        }
    }

    /// Set scroll behavior.
    #[must_use]
    pub const fn policy(mut self, policy: ScrollViewPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set scrollbar styles.
    #[must_use]
    pub const fn scrollbar_styles(mut self, styles: ScrollbarStyles) -> Self {
        self.scrollbar_styles = styles;
        self
    }

    /// Content viewport after reserving an integrated scrollbar gutter.
    #[must_use]
    pub const fn content_area(&self, area: Rect) -> Rect {
        if self.policy.scrollbar && area.width > 0 {
            Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height)
        } else {
            area
        }
    }

    /// Integrated vertical scrollbar area.
    #[must_use]
    pub const fn scrollbar_area(&self, area: Rect) -> Option<Rect> {
        if self.policy.scrollbar && area.width > 0 {
            Some(Rect::new(
                area.right().saturating_sub(1),
                area.y,
                1,
                area.height,
            ))
        } else {
            None
        }
    }

    /// Register the scroll viewport and paint an integrated scrollbar.
    pub fn paint_chrome(
        &self,
        id: impl Into<String>,
        area: Rect,
        layout: &LayoutNode,
        state: &ScrollViewState,
        cx: &mut PaintCx<'_, '_>,
    ) {
        let content = self.content_area(area);
        let id = id.into();
        cx.push_hit(
            HitRegion::new(id.clone(), Rect::new(0, 0, content.width, content.height))
                .role(HitRole::Scroll)
                .pointer_events(self.policy.mouse_wheel)
                .focusable(self.policy.keyboard)
                .enabled(!state.interaction.disabled),
        );
        cx.push_semantic(SemanticRegion::new(
            id,
            Rect::new(0, 0, content.width, content.height),
            "scroll",
        ));
        let Some(scrollbar_area) = self.scrollbar_area(area) else {
            return;
        };
        let total = layout
            .children
            .first()
            .map_or(0, |child| child.node.size.height);
        let scrollbar = scrollbar_state(total, layout.size.height, state.vertical_offset);
        Scrollbar::new()
            .policy(ScrollbarPolicy::vertical())
            .styles(self.scrollbar_styles)
            .paint(
                Rect::new(scrollbar_area.x.saturating_sub(area.x), 0, 1, area.height),
                &scrollbar,
                cx,
            );
    }

    /// Route integrated scrollbar dragging into logical scroll state.
    pub fn handle_scrollbar_event(
        &self,
        area: Rect,
        layout: &LayoutNode,
        state: &mut ScrollViewState,
        event: &Event,
    ) -> ScrollViewOutcome {
        let Some(area) = self.scrollbar_area(area) else {
            return ScrollViewOutcome::Ignored;
        };
        let total = layout
            .children
            .first()
            .map_or(0, |child| child.node.size.height);
        let mut scrollbar = scrollbar_state(total, layout.size.height, state.vertical_offset);
        let result = Scrollbar::new()
            .policy(ScrollbarPolicy::vertical())
            .handle_event(area, &mut scrollbar, event);
        match result {
            ScrollbarOutcome::Changed { offset } => {
                let old = state.vertical_offset;
                state.vertical_offset = logical_offset_from_scrollbar(
                    usize::from(offset),
                    usize::from(scrollbar.max_offset()),
                    Self::max_vertical_offset(layout),
                );
                state.follow_bottom = state.vertical_offset == Self::max_vertical_offset(layout);
                outcome(old, state.vertical_offset)
            }
            ScrollbarOutcome::Ignored | ScrollbarOutcome::Redraw => ScrollViewOutcome::Ignored,
        }
    }

    /// Return the maximum logical offset from an authoritative viewport layout.
    #[must_use]
    pub fn max_vertical_offset(layout: &LayoutNode) -> usize {
        layout.children.first().map_or(0, |child| {
            child.node.size.height.saturating_sub(layout.size.height)
        })
    }

    /// Return the maximum logical horizontal offset from an authoritative viewport layout.
    #[must_use]
    pub fn max_horizontal_offset(layout: &LayoutNode) -> usize {
        layout.children.first().map_or(0, |child| {
            usize::from(child.node.size.width.saturating_sub(layout.size.width))
        })
    }

    /// Reconcile state after content or viewport layout changes.
    pub fn reconcile(&self, layout: &LayoutNode, state: &mut ScrollViewState) {
        let maximum = Self::max_vertical_offset(layout);
        state.vertical_offset = if state.follow_bottom {
            maximum
        } else {
            state.vertical_offset.min(maximum)
        };
        state.horizontal_offset = state
            .horizontal_offset
            .min(Self::max_horizontal_offset(layout));
    }

    /// Apply a selection edge-autoscroll request owned by this viewport.
    pub fn handle_selection_auto_scroll(
        &self,
        layout: &LayoutNode,
        state: &mut ScrollViewState,
        request: &SelectionAutoScrollRequest,
    ) -> ScrollViewOutcome {
        if request.axis != SelectionScrollAxis::Vertical || state.interaction.disabled {
            return ScrollViewOutcome::Ignored;
        }
        let old = state.vertical_offset;
        let amount = usize::from(request.intensity.max(1));
        match request.direction {
            SelectionScrollDirection::Backward => {
                state.vertical_offset = state.vertical_offset.saturating_sub(amount);
                state.follow_bottom = false;
            }
            SelectionScrollDirection::Forward => {
                let maximum = Self::max_vertical_offset(layout);
                state.vertical_offset = state.vertical_offset.saturating_add(amount).min(maximum);
                state.follow_bottom = state.vertical_offset == maximum;
            }
        }
        outcome(old, state.vertical_offset)
    }

    /// Return whether this viewport can consume one vertical scroll direction.
    #[must_use]
    pub fn can_scroll(
        layout: &LayoutNode,
        state: &ScrollViewState,
        direction: SelectionScrollDirection,
    ) -> bool {
        match direction {
            SelectionScrollDirection::Backward => state.vertical_offset > 0,
            SelectionScrollDirection::Forward => {
                state.vertical_offset < Self::max_vertical_offset(layout)
            }
        }
    }

    /// Ensure one logical content row range is visible.
    pub fn ensure_visible(
        &self,
        layout: &LayoutNode,
        state: &mut ScrollViewState,
        start: usize,
        height: usize,
    ) -> ScrollViewOutcome {
        let old = state.vertical_offset;
        let viewport = layout.size.height;
        let end = start.saturating_add(height);
        if start < state.vertical_offset {
            state.vertical_offset = start;
        } else if end > state.vertical_offset.saturating_add(viewport) {
            state.vertical_offset = end.saturating_sub(viewport);
        }
        state.follow_bottom = false;
        self.reconcile(layout, state);
        outcome(old, state.vertical_offset)
    }

    /// Scroll to the first logical content row.
    pub const fn scroll_to_top(&self, state: &mut ScrollViewState) -> ScrollViewOutcome {
        let old = state.vertical_offset;
        state.vertical_offset = 0;
        state.follow_bottom = false;
        outcome(old, state.vertical_offset)
    }

    /// Scroll to the final logical content row and follow subsequent appends.
    pub fn scroll_to_bottom(
        &self,
        layout: &LayoutNode,
        state: &mut ScrollViewState,
    ) -> ScrollViewOutcome {
        let old = state.vertical_offset;
        state.vertical_offset = Self::max_vertical_offset(layout);
        state.follow_bottom = true;
        outcome(old, state.vertical_offset)
    }

    /// Place one authoritative descendant's top at the viewport top.
    pub fn scroll_to_layout(
        &self,
        layout: &LayoutNode,
        state: &mut ScrollViewState,
        id: &LayoutId,
    ) -> ScrollViewOutcome {
        let Some(rect) = layout.find_logical_rect(id) else {
            return ScrollViewOutcome::Ignored;
        };
        let old = state.vertical_offset;
        state.vertical_offset = rect.y.min(Self::max_vertical_offset(layout));
        state.follow_bottom = state.vertical_offset == Self::max_vertical_offset(layout);
        outcome(old, state.vertical_offset)
    }

    /// Ensure one authoritative descendant layout is visible by stable identity.
    pub fn ensure_layout_visible(
        &self,
        layout: &LayoutNode,
        state: &mut ScrollViewState,
        id: &LayoutId,
    ) -> ScrollViewOutcome {
        layout
            .find_logical_rect(id)
            .map_or(ScrollViewOutcome::Ignored, |rect| {
                self.ensure_visible(layout, state, rect.y, rect.height)
            })
    }

    /// Capture one stable descendant's signed viewport-relative row.
    #[must_use]
    pub fn capture_anchor(
        layout: &LayoutNode,
        state: &ScrollViewState,
        id: &LayoutId,
    ) -> Option<ScrollAnchor> {
        let rect = layout.find_logical_rect(id)?;
        Some(ScrollAnchor {
            id: id.clone(),
            viewport_row: signed_difference(rect.y, state.vertical_offset),
        })
    }

    /// Restore a captured stable descendant after relayout.
    pub fn restore_anchor(
        &self,
        layout: &LayoutNode,
        state: &mut ScrollViewState,
        anchor: &ScrollAnchor,
    ) -> ScrollViewOutcome {
        let Some(rect) = layout.find_logical_rect(&anchor.id) else {
            return ScrollViewOutcome::Ignored;
        };
        let old = state.vertical_offset;
        state.vertical_offset = offset_for_viewport_row(rect.y, anchor.viewport_row)
            .min(Self::max_vertical_offset(layout));
        state.follow_bottom = state.vertical_offset == Self::max_vertical_offset(layout);
        outcome(old, state.vertical_offset)
    }

    /// Route one event to the innermost viewport first, handing an unconsumed
    /// edge event to the enclosing viewport.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_nested_event(
        inner: &Self,
        inner_area: Rect,
        inner_layout: &LayoutNode,
        inner_state: &mut ScrollViewState,
        outer: &Self,
        outer_area: Rect,
        outer_layout: &LayoutNode,
        outer_state: &mut ScrollViewState,
        event: &Event,
    ) -> NestedScrollOutcome {
        let inner_outcome = inner.handle_event(inner_area, inner_layout, inner_state, event);
        if inner_outcome != ScrollViewOutcome::Ignored {
            return NestedScrollOutcome::Inner(inner_outcome);
        }
        let outer_outcome = outer.handle_event(outer_area, outer_layout, outer_state, event);
        if outer_outcome == ScrollViewOutcome::Ignored {
            NestedScrollOutcome::Ignored
        } else {
            NestedScrollOutcome::Outer(outer_outcome)
        }
    }

    /// Handle one event, returning whether this viewport consumed it.
    ///
    /// A wheel event at a vertical edge is intentionally ignored so an
    /// enclosing scroll view can consume it deterministically.
    pub fn handle_event(
        &self,
        area: Rect,
        layout: &LayoutNode,
        state: &mut ScrollViewState,
        event: &Event,
    ) -> ScrollViewOutcome {
        if state.interaction.disabled {
            return ScrollViewOutcome::Ignored;
        }
        let maximum = Self::max_vertical_offset(layout);
        let old = state.vertical_offset;
        match event {
            Event::Key(stroke) if self.policy.keyboard && state.interaction.focused => {
                Self::handle_key(*stroke, layout.size.height, maximum, state);
            }
            Event::Mouse(mouse) if self.policy.mouse_wheel && area.contains(mouse.position) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        state.vertical_offset =
                            state.vertical_offset.saturating_sub(self.policy.wheel_rows);
                        state.follow_bottom = false;
                    }
                    MouseEventKind::ScrollDown => {
                        state.vertical_offset = state
                            .vertical_offset
                            .saturating_add(self.policy.wheel_rows)
                            .min(maximum);
                        state.follow_bottom = state.vertical_offset == maximum;
                    }
                    _ => return ScrollViewOutcome::Ignored,
                }
            }
            _ => return ScrollViewOutcome::Ignored,
        }
        self.reconcile(layout, state);
        outcome(old, state.vertical_offset)
    }

    fn handle_key(
        stroke: KeyStroke,
        viewport_height: usize,
        maximum: usize,
        state: &mut ScrollViewState,
    ) {
        match stroke.key {
            KeyCode::Up => {
                state.vertical_offset = state.vertical_offset.saturating_sub(1);
                state.follow_bottom = false;
            }
            KeyCode::Down => {
                state.vertical_offset = state.vertical_offset.saturating_add(1).min(maximum);
                state.follow_bottom = state.vertical_offset == maximum;
            }
            KeyCode::PageUp => {
                state.vertical_offset = state.vertical_offset.saturating_sub(viewport_height);
                state.follow_bottom = false;
            }
            KeyCode::PageDown => {
                state.vertical_offset = state
                    .vertical_offset
                    .saturating_add(viewport_height)
                    .min(maximum);
                state.follow_bottom = state.vertical_offset == maximum;
            }
            KeyCode::Home => {
                state.vertical_offset = 0;
                state.follow_bottom = false;
            }
            KeyCode::End => {
                state.vertical_offset = maximum;
                state.follow_bottom = true;
            }
            _ => {}
        }
    }
}

impl Default for ScrollView {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn scrollbar_state(total: usize, viewport: usize, offset: usize) -> ScrollbarState {
    let maximum = total.saturating_sub(viewport);
    let scaled_total = u16::try_from(total).unwrap_or(u16::MAX);
    let scaled_viewport = if maximum == 0 {
        scaled_total
    } else {
        u16::try_from(
            (u128::from(scaled_total) * u128::try_from(viewport).unwrap_or(u128::MAX))
                / u128::try_from(total.max(1)).unwrap_or(u128::MAX),
        )
        .unwrap_or(u16::MAX)
        .max(1)
    };
    let scrollbar_maximum = scaled_total.saturating_sub(scaled_viewport);
    let scaled_offset = if maximum == 0 {
        0
    } else {
        u16::try_from(
            (u128::from(scrollbar_maximum) * u128::try_from(offset).unwrap_or(u128::MAX))
                / u128::try_from(maximum).unwrap_or(u128::MAX),
        )
        .unwrap_or(scrollbar_maximum)
    };
    ScrollbarState::new(scaled_total, scaled_viewport).offset(scaled_offset)
}

fn logical_offset_from_scrollbar(
    offset: usize,
    scrollbar_maximum: usize,
    logical_maximum: usize,
) -> usize {
    if logical_maximum == 0 || scrollbar_maximum == 0 {
        return 0;
    }
    usize::try_from(
        (u128::try_from(offset).unwrap_or(u128::MAX)
            * u128::try_from(logical_maximum).unwrap_or(u128::MAX))
            / u128::try_from(scrollbar_maximum).unwrap_or(u128::MAX),
    )
    .unwrap_or(logical_maximum)
    .min(logical_maximum)
}

fn signed_difference(value: usize, base: usize) -> i64 {
    if value >= base {
        i64::try_from(value - base).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(base - value).unwrap_or(i64::MAX)
    }
}

fn offset_for_viewport_row(row: usize, viewport_row: i64) -> usize {
    if viewport_row >= 0 {
        row.saturating_sub(usize::try_from(viewport_row).unwrap_or(usize::MAX))
    } else {
        row.saturating_add(usize::try_from(viewport_row.unsigned_abs()).unwrap_or(usize::MAX))
    }
}

const fn outcome(old: usize, current: usize) -> ScrollViewOutcome {
    if old == current {
        ScrollViewOutcome::Ignored
    } else {
        ScrollViewOutcome::Scrolled {
            vertical_offset: current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NestedScrollOutcome, ScrollView, ScrollViewComponent, ScrollViewOutcome, ScrollViewState,
        logical_offset_from_scrollbar, scrollbar_state,
    };
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{
        ChildLayout, Component, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize,
    };
    use bmux_tui::composition::TextContent;
    use bmux_tui::event::{Event, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::hit::HitRole;
    use bmux_tui::paint::PaintCx;
    use bmux_tui::selection::{
        SelectionAutoScrollRequest, SelectionScopeId, SelectionScrollAxis, SelectionScrollDirection,
    };

    #[test]
    fn arbitrary_component_subtree_paints_through_translation_and_clip() {
        let mut state = ScrollViewState::new();
        state.set_vertical_offset(1);
        let component = ScrollViewComponent::new(
            "scroll",
            LogicalSize::new(6, 2),
            state,
            TextContent::new("first\nsecond\nthird"),
        );
        let mut layout_cx = LayoutCx::new();
        let layout = component.layout(Constraints::tight(Size::new(6, 2)), &mut layout_cx);
        let area = Rect::new(0, 0, 6, 2);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);

        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(frame.buffer().get(Point::new(0, 0)).unwrap().symbol, "s");
        assert_eq!(frame.buffer().get(Point::new(0, 1)).unwrap().symbol, "t");
        assert_eq!(layout.children.len(), 1);
        assert_eq!(layout.children[0].node.size.height, 3);
    }

    #[test]
    fn reconcile_clamps_both_logical_axes_without_terminal_saturation() {
        let content = LayoutNode::leaf(
            LayoutId::new("content"),
            LogicalSize::new(u16::MAX, 100_000),
        );
        let layout = LayoutNode::with_children(
            LayoutId::new("scroll"),
            LogicalSize::new(80, 24),
            vec![ChildLayout::new(0, 0, content)],
        );
        let mut state = ScrollViewState::new();
        state.set_vertical_offset(usize::MAX);
        state.set_horizontal_offset(usize::MAX);

        ScrollView::new().reconcile(&layout, &mut state);

        assert_eq!(state.vertical_offset(), 99_976);
        assert_eq!(state.horizontal_offset(), usize::from(u16::MAX) - 80);
    }

    #[test]
    fn huge_logical_extent_round_trips_through_terminal_scrollbar_scale() {
        let total = usize::MAX / 4;
        let viewport = 37usize;
        let maximum = total - viewport;
        for offset in [0, maximum / 4, maximum / 2, maximum * 3 / 4, maximum] {
            let bar = scrollbar_state(total, viewport, offset);
            assert_eq!(bar.content_len, u16::MAX);
            assert!(bar.viewport_len >= 1);
            assert!(bar.offset <= bar.max_offset());
            let restored = logical_offset_from_scrollbar(
                usize::from(bar.offset),
                usize::from(bar.max_offset()),
                maximum,
            );
            let tolerance = maximum / usize::from(bar.max_offset().max(1)) + 1;
            assert!(restored.abs_diff(offset) <= tolerance);
        }
    }

    fn layout(content_height: usize, viewport_height: usize) -> LayoutNode {
        LayoutNode::with_children(
            LayoutId::new("viewport"),
            LogicalSize::new(10, viewport_height),
            vec![ChildLayout::new(
                0,
                0,
                LayoutNode::leaf(
                    LayoutId::new("content"),
                    LogicalSize::new(10, content_height),
                ),
            )],
        )
    }

    #[test]
    fn nested_keyboard_routing_respects_focus_and_edge_handoff() {
        let inner = ScrollView::new();
        let outer = ScrollView::new();
        let inner_layout = layout(20, 5);
        let outer_layout = layout(40, 10);
        let area = Rect::new(0, 0, 10, 10);
        let event = Event::Key(KeyStroke::simple(KeyCode::PageDown));
        let mut inner_state = ScrollViewState::new();
        let mut outer_state = ScrollViewState::new();
        inner_state.interaction.focused = true;
        outer_state.interaction.focused = true;

        assert_eq!(
            ScrollView::handle_nested_event(
                &inner,
                area,
                &inner_layout,
                &mut inner_state,
                &outer,
                area,
                &outer_layout,
                &mut outer_state,
                &event,
            ),
            NestedScrollOutcome::Inner(ScrollViewOutcome::Scrolled { vertical_offset: 5 })
        );
        assert_eq!(outer_state.vertical_offset(), 0);

        inner_state.set_vertical_offset(ScrollView::max_vertical_offset(&inner_layout));
        assert_eq!(
            ScrollView::handle_nested_event(
                &inner,
                area,
                &inner_layout,
                &mut inner_state,
                &outer,
                area,
                &outer_layout,
                &mut outer_state,
                &event,
            ),
            NestedScrollOutcome::Outer(ScrollViewOutcome::Scrolled {
                vertical_offset: 10
            })
        );

        inner_state.interaction.focused = false;
        outer_state.interaction.focused = false;
        assert_eq!(
            ScrollView::handle_nested_event(
                &inner,
                area,
                &inner_layout,
                &mut inner_state,
                &outer,
                area,
                &outer_layout,
                &mut outer_state,
                &event,
            ),
            NestedScrollOutcome::Ignored
        );
    }

    #[test]
    fn nested_wheel_routing_hands_off_only_at_inner_edges() {
        let inner = ScrollView::new();
        let outer = ScrollView::new();
        let inner_layout = layout(20, 5);
        let outer_layout = layout(40, 10);
        let inner_area = Rect::new(2, 2, 5, 5);
        let outer_area = Rect::new(0, 0, 10, 10);
        let event = Event::Mouse(MouseEvent::new(
            MouseEventKind::ScrollDown,
            Point::new(3, 3),
        ));
        let mut inner_state = ScrollViewState::new();
        let mut outer_state = ScrollViewState::new();

        assert_eq!(
            ScrollView::handle_nested_event(
                &inner,
                inner_area,
                &inner_layout,
                &mut inner_state,
                &outer,
                outer_area,
                &outer_layout,
                &mut outer_state,
                &event,
            ),
            NestedScrollOutcome::Inner(ScrollViewOutcome::Scrolled { vertical_offset: 3 })
        );
        assert_eq!(outer_state.vertical_offset(), 0);

        inner_state.set_vertical_offset(ScrollView::max_vertical_offset(&inner_layout));
        assert_eq!(
            ScrollView::handle_nested_event(
                &inner,
                inner_area,
                &inner_layout,
                &mut inner_state,
                &outer,
                outer_area,
                &outer_layout,
                &mut outer_state,
                &event,
            ),
            NestedScrollOutcome::Outer(ScrollViewOutcome::Scrolled { vertical_offset: 3 })
        );
    }

    #[test]
    fn explicit_top_bottom_and_layout_operations_share_canonical_offset() {
        let target = LayoutNode::leaf(LayoutId::new("target"), LogicalSize::new(10, 2));
        let content = LayoutNode::with_children(
            LayoutId::new("content"),
            LogicalSize::new(10, 80_000),
            vec![ChildLayout::new(0, 70_000, target)],
        );
        let layout = LayoutNode::with_children(
            LayoutId::new("viewport"),
            LogicalSize::new(10, 5),
            vec![ChildLayout::new(0, 0, content)],
        );
        let view = ScrollView::new();
        let mut state = ScrollViewState::new();

        assert_eq!(
            view.scroll_to_layout(&layout, &mut state, &LayoutId::new("target")),
            ScrollViewOutcome::Scrolled {
                vertical_offset: 70_000
            }
        );
        assert!(!state.follows_bottom());
        assert_eq!(
            view.scroll_to_bottom(&layout, &mut state),
            ScrollViewOutcome::Scrolled {
                vertical_offset: 79_995
            }
        );
        assert!(state.follows_bottom());
        assert_eq!(
            view.scroll_to_top(&mut state),
            ScrollViewOutcome::Scrolled { vertical_offset: 0 }
        );
        assert!(!state.follows_bottom());
        assert_eq!(
            view.scroll_to_layout(&layout, &mut state, &LayoutId::new("missing")),
            ScrollViewOutcome::Ignored
        );
    }

    #[test]
    fn stable_anchor_restores_relative_row_after_relayout() {
        fn anchored_layout(target_row: usize) -> LayoutNode {
            let target = LayoutNode::leaf(LayoutId::new("target"), LogicalSize::new(10, 2));
            let content = LayoutNode::with_children(
                LayoutId::new("content"),
                LogicalSize::new(10, 100_000),
                vec![ChildLayout::new(0, target_row, target)],
            );
            LayoutNode::with_children(
                LayoutId::new("viewport"),
                LogicalSize::new(10, 10),
                vec![ChildLayout::new(0, 0, content)],
            )
        }

        let view = ScrollView::new();
        let before = anchored_layout(70_000);
        let mut state = ScrollViewState::new();
        state.set_vertical_offset(69_997);
        let anchor = ScrollView::capture_anchor(&before, &state, &LayoutId::new("target")).unwrap();
        assert_eq!(anchor.viewport_row(), 3);

        let after = anchored_layout(75_000);
        assert_eq!(
            view.restore_anchor(&after, &mut state, &anchor),
            ScrollViewOutcome::Scrolled {
                vertical_offset: 74_997
            }
        );
        assert_eq!(state.vertical_offset(), 74_997);

        let missing = layout(100_000, 10);
        assert_eq!(
            view.restore_anchor(&missing, &mut state, &anchor),
            ScrollViewOutcome::Ignored
        );
        assert_eq!(state.vertical_offset(), 74_997);
    }

    #[test]
    fn ensure_layout_visible_uses_unsaturated_authoritative_geometry() {
        let target = LayoutNode::leaf(LayoutId::new("target"), LogicalSize::new(10, 2));
        let content = LayoutNode::with_children(
            LayoutId::new("content"),
            LogicalSize::new(10, 80_000),
            vec![ChildLayout::new(0, 70_000, target)],
        );
        let layout = LayoutNode::with_children(
            LayoutId::new("viewport"),
            LogicalSize::new(10, 5),
            vec![ChildLayout::new(0, 0, content)],
        );
        let view = ScrollView::new();
        let mut state = ScrollViewState::new();

        assert_eq!(
            view.ensure_layout_visible(&layout, &mut state, &LayoutId::new("target")),
            ScrollViewOutcome::Scrolled {
                vertical_offset: 69_997
            }
        );
        assert_eq!(state.vertical_offset(), 69_997);
        assert_eq!(
            view.ensure_layout_visible(&layout, &mut state, &LayoutId::new("missing")),
            ScrollViewOutcome::Ignored
        );
    }

    #[test]
    fn keyboard_navigation_clamps_and_tracks_bottom_anchor() {
        let view = ScrollView::new();
        let initial_layout = layout(20, 5);
        let mut state = ScrollViewState::new();
        state.interaction.focused = true;

        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 10, 5),
                &initial_layout,
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::End)),
            ),
            ScrollViewOutcome::Scrolled {
                vertical_offset: 15
            }
        );
        assert!(state.follows_bottom());
        view.reconcile(&layout(25, 5), &mut state);
        assert_eq!(state.vertical_offset(), 20);
    }

    #[test]
    fn selection_auto_scroll_and_edge_handoff_share_scroll_bounds() {
        let view = ScrollView::new();
        let layout = layout(20, 5);
        let mut state = ScrollViewState::new();
        let request = SelectionAutoScrollRequest {
            scope_id: SelectionScopeId::new("content"),
            axis: SelectionScrollAxis::Vertical,
            direction: SelectionScrollDirection::Forward,
            intensity: 2,
        };

        assert_eq!(
            view.handle_selection_auto_scroll(&layout, &mut state, &request),
            ScrollViewOutcome::Scrolled { vertical_offset: 2 }
        );
        assert!(ScrollView::can_scroll(
            &layout,
            &state,
            SelectionScrollDirection::Forward
        ));
        state.set_vertical_offset(ScrollView::max_vertical_offset(&layout));
        assert!(!ScrollView::can_scroll(
            &layout,
            &state,
            SelectionScrollDirection::Forward
        ));
    }

    #[test]
    fn focus_visibility_uses_same_minimum_scroll_projection() {
        let view = ScrollView::new();
        let layout = layout(20, 5);
        let mut state = ScrollViewState::new();

        assert_eq!(
            view.ensure_visible(&layout, &mut state, 9, 1),
            ScrollViewOutcome::Scrolled { vertical_offset: 5 }
        );
    }

    #[test]
    fn scrollbar_drag_routes_to_logical_offset() {
        let view = ScrollView::new();
        let layout = layout(100, 5);
        let mut state = ScrollViewState::new();
        let area = Rect::new(0, 0, 10, 5);
        let event = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            Point::new(9, 4),
        ));

        assert!(matches!(
            view.handle_scrollbar_event(area, &layout, &mut state, &event),
            ScrollViewOutcome::Scrolled { .. }
        ));
        assert!(state.vertical_offset() > 0);
    }

    #[test]
    fn paints_integrated_scrollbar_and_registers_scroll_region() {
        let view = ScrollView::new();
        let layout = layout(20, 5);
        let state = ScrollViewState::new();
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 10, 5));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        view.paint_chrome(
            "view",
            Rect::new(0, 0, 10, 5),
            &layout,
            &state,
            &mut PaintCx::new(&mut frame),
        );

        assert_eq!(view.content_area(Rect::new(0, 0, 10, 5)).width, 9);
        assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 0, 9, 5));
        assert_eq!(frame.hits().regions()[0].role, HitRole::Scroll);
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].id.as_str(), "view");
        assert_eq!(frame.semantics().regions()[0].role, "scroll");
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 9, 5));
        assert_ne!(
            frame
                .buffer()
                .get(Point::new(9, 0))
                .map(|cell| cell.symbol.as_str()),
            Some(" ")
        );
    }

    #[test]
    fn ensure_visible_moves_minimum_distance() {
        let view = ScrollView::new();
        let layout = layout(20, 5);
        let mut state = ScrollViewState::new();

        view.ensure_visible(&layout, &mut state, 7, 2);
        assert_eq!(state.vertical_offset(), 4);
        view.ensure_visible(&layout, &mut state, 2, 1);
        assert_eq!(state.vertical_offset(), 2);
    }

    #[test]
    fn wheel_only_routes_inside_viewport() {
        let view = ScrollView::new();
        let layout = layout(20, 5);
        let mut state = ScrollViewState::new();
        let outside = Event::Mouse(MouseEvent::new(
            MouseEventKind::ScrollDown,
            Point::new(20, 20),
        ));
        assert_eq!(
            view.handle_event(Rect::new(0, 0, 10, 5), &layout, &mut state, &outside),
            ScrollViewOutcome::Ignored
        );
    }
}
