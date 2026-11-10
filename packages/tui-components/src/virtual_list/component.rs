//! The canonical-lifecycle virtual-list component: viewport, integrated
//! scrollbar, and input mapping over a lazy [`VirtualItemSource`].

use std::cell::RefCell;
use std::hash::{DefaultHasher, Hash, Hasher};

use bmux_keyboard::KeyCode;
use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutEnvironment,
    LayoutId, LayoutMetadata, LayoutNode, LogicalRect, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::pointer::PointerIntent;
use bmux_tui::selection::SelectionAutoScrollRequest;
use bmux_tui::semantic::SemanticRegion;

use super::{
    ActionDispatch, Edge, NoSourceMap, PageStep, PlacedItem, PositionEstimate, ResolvedPosition,
    RoutedEvent, ScrollInputSource, SourceMap, VirtualItemSource, VirtualListAction,
    VirtualListRenderStats, VirtualListState, VirtualSourceList, VisibleItemRange, clip_rows,
    find_item_layout, item_layout_id,
};
use crate::common::{local_area_of, local_control_event};
use crate::scroll_view::{logical_offset_from_scrollbar, route_vertical_track, scrollbar_state};
use crate::scrollbar::{
    Scrollbar, ScrollbarOutcome, ScrollbarPolicy, ScrollbarState, ScrollbarStyles,
};
use crate::scrollbar_layout::{ScrollbarAxisLayoutMode, ScrollbarLayoutPolicy, scrollbar_layout};

/// Scroll chrome and input policy for a [`VirtualListComponent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent input switches mirror ScrollViewPolicy"
)]
pub struct VirtualListChrome {
    /// Integrated vertical scrollbar layout. `Gutter` always reserves one
    /// column, so content width never depends on overflow.
    pub scrollbar: ScrollbarAxisLayoutMode,
    /// Scrollbar glyphs, minimum thumb, and drag policy. The orientation is
    /// always vertical.
    pub scrollbar_policy: ScrollbarPolicy,
    /// Scrollbar styles.
    pub scrollbar_styles: ScrollbarStyles,
    /// Map Up/Down, PageUp/PageDown, and Home/End to actions while the state's
    /// `scroll.interaction.focused` is set.
    pub keyboard: bool,
    /// Map unhandled wheel events to actions.
    pub mouse_wheel: bool,
    /// Rows per wheel event.
    pub wheel_rows: u16,
    /// Rows per page request.
    pub page: PageStep,
}

impl VirtualListChrome {
    /// Gutter scrollbar, keyboard and wheel input, three rows per wheel event,
    /// and one viewport per page.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            scrollbar: ScrollbarAxisLayoutMode::Gutter,
            scrollbar_policy: ScrollbarPolicy::vertical(),
            scrollbar_styles: ScrollbarStyles::new(),
            keyboard: true,
            mouse_wheel: true,
            wheel_rows: 3,
            page: PageStep::Viewport,
        }
    }

    /// No scrollbar and no keyboard or wheel input. Items still receive
    /// events and clicks still report activation.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            scrollbar: ScrollbarAxisLayoutMode::Hidden,
            keyboard: false,
            mouse_wheel: false,
            ..Self::interactive()
        }
    }

    /// Return this chrome with a different scrollbar layout mode.
    #[must_use]
    pub const fn scrollbar(mut self, mode: ScrollbarAxisLayoutMode) -> Self {
        self.scrollbar = mode;
        self
    }

    /// Return this chrome with different scrollbar glyphs and drag policy.
    #[must_use]
    pub const fn scrollbar_policy(mut self, policy: ScrollbarPolicy) -> Self {
        self.scrollbar_policy = policy;
        self
    }

    /// Return this chrome with different scrollbar styles.
    #[must_use]
    pub const fn scrollbar_styles(mut self, styles: ScrollbarStyles) -> Self {
        self.scrollbar_styles = styles;
        self
    }

    /// Return this chrome with keyboard navigation enabled or disabled.
    #[must_use]
    pub const fn keyboard(mut self, enabled: bool) -> Self {
        self.keyboard = enabled;
        self
    }

    /// Return this chrome with wheel navigation enabled or disabled.
    #[must_use]
    pub const fn mouse_wheel(mut self, enabled: bool) -> Self {
        self.mouse_wheel = enabled;
        self
    }

    /// Return this chrome with a different wheel step.
    #[must_use]
    pub const fn wheel_rows(mut self, rows: u16) -> Self {
        self.wheel_rows = rows;
        self
    }

    /// Return this chrome with a different page step.
    #[must_use]
    pub const fn page(mut self, page: PageStep) -> Self {
        self.page = page;
        self
    }

    const fn layout_policy(self) -> ScrollbarLayoutPolicy {
        ScrollbarLayoutPolicy::new(self.scrollbar, ScrollbarAxisLayoutMode::Hidden)
    }

    const fn vertical_policy(self) -> ScrollbarPolicy {
        let mut policy = self.scrollbar_policy;
        policy.orientation = crate::scrollbar::ScrollbarOrientation::Vertical;
        policy
    }
}

impl Default for VirtualListChrome {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Geometry recorded by the most recent component layout. Paint and event
/// use it only for the layout node it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LayoutSnapshot {
    /// Root (viewport) size.
    size: LogicalSize,
    /// Content viewport in root-local cells.
    content: Rect,
    /// Integrated scrollbar track in root-local cells.
    scrollbar: Option<Rect>,
    /// Position resolved for the content viewport.
    resolved: ResolvedPosition,
    /// Items placed in the content node, in order.
    visible: VisibleItemRange,
    /// Index measurement generation the placement was computed from.
    measurement_generation: u64,
}

impl<K: Clone + Ord, S: Clone> VirtualListState<K, S> {
    /// Route a vertical edge-autoscroll request from the selection controller
    /// as a [`VirtualListAction::SelectionAutoScroll`] of the request's
    /// intensity (queued in controlled mode, applied in uncontrolled mode).
    ///
    /// Returns whether the request was queued or moved the list; horizontal
    /// requests and disabled lists return `false`.
    pub fn handle_selection_auto_scroll(&mut self, request: &SelectionAutoScrollRequest) -> bool {
        if self.scroll.interaction.disabled {
            return false;
        }
        let Some(action) = VirtualListAction::selection_auto_scroll(request) else {
            return false;
        };
        matches!(
            self.dispatch_action(action, &NoSourceMap),
            ActionDispatch::Queued | ActionDispatch::Applied
        )
    }

    /// Scrollbar thumb model for a resolved position: the position estimate
    /// when one is set and its resident rows equal the exact total, otherwise
    /// the exact total. Overscroll pins the thumb at the bottom. Drag
    /// fractions invert this same model.
    fn scrollbar_model(&self, resolved: ResolvedPosition) -> ScrollbarState {
        let top = resolved.top_offset.min(resolved.max_content_top);
        match self.estimate {
            Some(estimate) if estimate.resident == resolved.total_height => {
                estimate.scrollbar_state(top, resolved.viewport_height)
            }
            _ => scrollbar_state(resolved.total_height, resolved.viewport_height, top),
        }
    }

    /// Scroll fraction for a drag to scrollbar cell `offset` of `model`
    /// (the model drawn for `resolved`). The cell is inverted to a logical
    /// row with the same integer mapping scroll views use, and the fraction
    /// is that row over the model's logical maximum, so applying it recovers
    /// the row exactly.
    #[allow(
        clippy::cast_precision_loss,
        reason = "logical rows below 2^52 round-trip exactly through the fraction"
    )]
    fn drag_fraction(&self, resolved: ResolvedPosition, model: ScrollbarState, offset: u16) -> f64 {
        let logical_maximum = match self.estimate {
            Some(estimate) if estimate.resident == resolved.total_height => {
                estimate.total().saturating_sub(resolved.viewport_height)
            }
            _ => resolved.max_content_top,
        };
        if logical_maximum == 0 {
            return 0.0;
        }
        let row = logical_offset_from_scrollbar(
            u64::from(offset),
            u64::from(model.max_offset()),
            logical_maximum,
        );
        row as f64 / logical_maximum as f64
    }

    /// The recorded snapshot when it describes `layout` and the index has not
    /// changed since.
    fn snapshot_for(&self, layout: &LayoutNode) -> Option<LayoutSnapshot> {
        let snapshot = self.snapshot?;
        let content = &layout.children.first()?.node;
        let window = content.children.last()?;
        let placed = snapshot.visible.end - snapshot.visible.start;
        (layout.size == snapshot.size
            && content.size.height == snapshot.resolved.total_height
            && window.y == snapshot.resolved.top_offset
            && content.children.len() == placed + 1
            && self.generation == snapshot.measurement_generation)
            .then_some(snapshot)
    }

    fn release_pointer(&mut self) {
        self.scroll.release_scrollbar();
        self.pressed = None;
    }
}

/// A virtualized list component over a lazy [`VirtualItemSource`] with
/// caller-owned [`VirtualListState`].
///
/// Layout reconciles the state against the source, resolves the keyed
/// position, and returns a viewport node whose first child is a content node
/// in collection coordinates holding only the visible items (each wrapped in a
/// node identified by the item's stable id) plus a trailing window node whose
/// row is the resolved top offset; a gutter or overlay scrollbar is the second
/// child. Paint and event consume exactly that node.
///
/// See the "`VirtualListComponent`" section of `docs/tui-framework.md`.
pub struct VirtualListComponent<'a, K, Src: ?Sized, S = ()> {
    id: LayoutId,
    source: &'a Src,
    state: &'a RefCell<VirtualListState<K, S>>,
    chrome: VirtualListChrome,
    sources: &'a dyn SourceMap<K, S>,
    estimate: Option<PositionEstimate>,
    environment: LayoutEnvironment,
    pointer_intent: PointerIntent,
}

impl<'a, K, Src, S> VirtualListComponent<'a, K, Src, S>
where
    K: Clone + Ord + ToString,
    Src: VirtualItemSource<K> + ?Sized,
    S: Clone,
{
    /// Create a list with a stable identity, an item source, and caller-owned
    /// state. Item identities are `"{id}.item.{key}"`.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        source: &'a Src,
        state: &'a RefCell<VirtualListState<K, S>>,
    ) -> Self {
        Self {
            id: id.into(),
            source,
            state,
            chrome: VirtualListChrome::interactive(),
            sources: &NoSourceMap,
            estimate: None,
            environment: LayoutEnvironment::default(),
            pointer_intent: PointerIntent::Auto,
        }
    }

    /// Set scroll chrome and input policy.
    #[must_use]
    pub const fn chrome(mut self, chrome: VirtualListChrome) -> Self {
        self.chrome = chrome;
        self
    }

    /// Resolve [`super::AnchorOffset::Source`] anchors through `sources`.
    #[must_use]
    pub const fn sources(mut self, sources: &'a dyn SourceMap<K, S>) -> Self {
        self.sources = sources;
        self
    }

    /// Draw the scrollbar from a presentation-only position estimate for
    /// content beyond a resident window. Ignored unless its `resident` rows
    /// equal the exact measured total.
    #[must_use]
    pub const fn position_estimate(mut self, estimate: PositionEstimate) -> Self {
        self.estimate = Some(estimate);
        self
    }

    /// Set geometry-affecting environment inputs for item measurement.
    #[must_use]
    pub const fn environment(mut self, environment: LayoutEnvironment) -> Self {
        self.environment = environment;
        self
    }

    /// Override item pointer presentation without changing input routing.
    #[must_use]
    pub const fn pointer_intent(mut self, intent: PointerIntent) -> Self {
        self.pointer_intent = intent;
        self
    }

    /// Stable identity of the content node.
    #[must_use]
    pub fn content_id(&self) -> LayoutId {
        LayoutId::new(format!("{}.content", self.id.as_str()))
    }

    /// Stable identity of the window node, a child of the content node whose
    /// logical rectangle is the visible region in collection rows.
    #[must_use]
    pub fn window_id(&self) -> LayoutId {
        LayoutId::new(format!("{}.window", self.id.as_str()))
    }

    /// Stable identity of the scrollbar node.
    #[must_use]
    pub fn scrollbar_id(&self) -> LayoutId {
        LayoutId::new(format!("{}.scrollbar", self.id.as_str()))
    }

    /// Stable identity of one item's wrapper node (also its hit, focus, and
    /// semantic identifier).
    #[must_use]
    pub fn item_id(&self, key: &K) -> LayoutId {
        item_layout_id(self.id.as_str(), key)
    }

    fn list(&self) -> VirtualSourceList<'_, Src> {
        VirtualSourceList::new(self.id.as_str(), self.source).pointer_intent(self.pointer_intent)
    }

    const fn configure(&self, state: &mut VirtualListState<K, S>) {
        state.set_page_step(self.chrome.page);
        state.set_position_estimate(self.estimate);
    }

    /// Visible items placed by `layout` (validated against `snapshot`),
    /// restricted to index rows in `positions`. Items the source no longer
    /// holds unchanged at their (splice-adjusted) position are skipped.
    fn placed(
        &self,
        state: &VirtualListState<K, S>,
        layout: &LayoutNode,
        snapshot: &LayoutSnapshot,
        positions: std::ops::Range<usize>,
    ) -> Vec<PlacedItem> {
        let start = snapshot.visible.start;
        let placed = snapshot.visible.end - start;
        layout
            .children
            .first()
            .map(|content| &content.node.children[..placed])
            .unwrap_or_default()
            .iter()
            .enumerate()
            .filter_map(|(offset, child)| {
                let index = start + offset;
                if !positions.contains(&index) {
                    return None;
                }
                let position = state.placed_source_position(self.source, index)?;
                let measured = state.index.item(index)?;
                Some(PlacedItem {
                    index,
                    position,
                    start: child.y,
                    height: child.node.size.height,
                    layout_revision: measured.layout_revision,
                    id: child.node.id.clone(),
                })
            })
            .collect()
    }

    /// The retained layout of a placed item at `width`, borrowed from the
    /// source or the state's layout cache.
    fn item_layout<'s>(
        &'s self,
        state: &'s VirtualListState<K, S>,
        item: &PlacedItem,
        width: u64,
    ) -> Option<&'s LayoutNode> {
        let environment = state
            .synced
            .map_or(self.environment, |synced| synced.environment);
        find_item_layout(
            self.source,
            &state.layouts,
            item.position,
            &item.id,
            item.layout_revision,
            width,
            environment,
        )
    }

    /// Leaf wrapper nodes for the visible items `range` at their exact
    /// collection rows, each identified by the item's stable id, and the range
    /// they cover. Retained item layouts stay in the source or layout cache and
    /// are borrowed at paint (cloned only for items an event is offered to),
    /// so layout cost does not scale with visible items' layout size.
    fn place_items(
        &self,
        state: &VirtualListState<K, S>,
        range: VisibleItemRange,
        content_width: u64,
    ) -> (Vec<ChildLayout>, VisibleItemRange) {
        let mut children = Vec::with_capacity(range.end - range.start + 1);
        for position in range.start..range.end {
            // Placement stays contiguous: an index shorter than the range
            // (never expected after layout) ends it.
            let Some(measured) = state.index.item(position) else {
                break;
            };
            let start = state.index.item_offset(position).unwrap_or(0);
            children.push(ChildLayout::new(
                0,
                start,
                LayoutNode::leaf(
                    item_layout_id(self.id.as_str(), &measured.key),
                    LogicalSize::new(content_width, measured.height),
                ),
            ));
        }
        let visible = VisibleItemRange {
            start: range.start,
            end: range.start + children.len(),
            first_item_offset: range.first_item_offset,
        };
        (children, visible)
    }

    /// Route an event through the placed items without holding the state
    /// borrow while items handle it, so their handlers may consult caller
    /// state; only the layouts of items actually offered the event are cloned.
    fn route(
        &self,
        snapshot: &LayoutSnapshot,
        layout: &LayoutNode,
        event: &Event,
        cx: &mut EventCx<'_>,
    ) -> RoutedEvent {
        let items = {
            let state = self.state.borrow();
            self.placed(&state, layout, snapshot, 0..usize::MAX)
        };
        let width = u64::from(snapshot.content.width);
        let routed = self.list().route_items(
            snapshot.content,
            snapshot.resolved.top_offset,
            items,
            event,
            cx,
            |item| {
                let state = self.state.borrow();
                self.item_layout(&state, item, width)
                    .cloned()
                    .map(std::borrow::Cow::Owned)
            },
        );
        self.state
            .borrow()
            .record(|stats| stats.built_items = stats.built_items.saturating_add(routed.built));
        routed
    }

    fn dispatch(
        &self,
        state: &mut VirtualListState<K, S>,
        action: VirtualListAction<K>,
    ) -> ActionDispatch {
        self.configure(state);
        state.dispatch_action(action, self.sources)
    }

    /// Map a dispatched navigation action to an outcome. Unchanged
    /// navigation is ignored so an enclosing scroller can consume it.
    const fn navigation_outcome(dispatch: ActionDispatch) -> EventOutcome {
        match dispatch {
            ActionDispatch::Queued => EventOutcome::Handled,
            ActionDispatch::Applied => EventOutcome::Redraw,
            ActionDispatch::Unchanged => EventOutcome::Ignored,
        }
    }

    /// Route integrated scrollbar input. Returns `None` when the scrollbar
    /// does not consume the event.
    fn scrollbar_event(
        &self,
        state: &mut VirtualListState<K, S>,
        snapshot: &LayoutSnapshot,
        layout: &LayoutNode,
        event: &Event,
        cx: &EventCx<'_>,
    ) -> Option<EventOutcome> {
        let dragging = state.scroll.dragging_scrollbar();
        let local = local_control_event(event, cx, layout, dragging)?;
        let Some(track) = snapshot.scrollbar else {
            state.scroll.release_scrollbar();
            return None;
        };
        let model = state.scrollbar_model(snapshot.resolved);
        if !self.chrome.scrollbar.shown_without_overflow() && model.max_offset() == 0 {
            state.scroll.release_scrollbar();
            return None;
        }
        match route_vertical_track(
            self.chrome.vertical_policy(),
            track,
            model,
            &mut state.scroll,
            &local,
        ) {
            ScrollbarOutcome::Changed { offset } => {
                state.pressed = None;
                let fraction = state.drag_fraction(snapshot.resolved, model, offset);
                Some(
                    match self.dispatch(state, VirtualListAction::ScrollbarDrag(fraction)) {
                        ActionDispatch::Applied => EventOutcome::Redraw,
                        ActionDispatch::Queued | ActionDispatch::Unchanged => EventOutcome::Handled,
                    },
                )
            }
            ScrollbarOutcome::Redraw => Some(EventOutcome::Redraw),
            ScrollbarOutcome::Ignored => {
                (dragging || state.scroll.dragging_scrollbar()).then_some(EventOutcome::Handled)
            }
        }
    }

    fn mouse_event(
        &self,
        snapshot: &LayoutSnapshot,
        layout: &LayoutNode,
        mouse: MouseEvent,
        event: &Event,
        cx: &mut EventCx<'_>,
    ) -> EventOutcome {
        {
            let mut state = self.state.borrow_mut();
            if let Some(outcome) = self.scrollbar_event(&mut state, snapshot, layout, event, cx) {
                return outcome;
            }
        }
        let routed = self.route(snapshot, layout, event, cx);
        let mut state = self.state.borrow_mut();
        let target_key = routed
            .target
            .and_then(|position| state.index.item(position))
            .map(|item| item.key.clone());
        let handled = routed.outcome.is_handled();
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                state.pressed = if handled { None } else { target_key };
            }
            MouseEventKind::Drag(_) => state.pressed = None,
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = state.pressed.take();
                if !handled
                    && let Some(key) = target_key
                    && pressed.as_ref() == Some(&key)
                {
                    self.dispatch(&mut state, VirtualListAction::Activated(key));
                    return EventOutcome::Handled;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if !handled
                    && self.chrome.mouse_wheel
                    && cx
                        .visible_rect(LogicalRect::new(
                            0,
                            0,
                            layout.size.width,
                            layout.size.height,
                        ))
                        .contains(mouse.position) =>
            {
                let rows = i64::from(self.chrome.wheel_rows);
                let rows = if mouse.kind == MouseEventKind::ScrollUp {
                    -rows
                } else {
                    rows
                };
                return Self::navigation_outcome(self.dispatch(
                    &mut state,
                    VirtualListAction::ScrollRequested {
                        rows,
                        source: ScrollInputSource::Wheel,
                    },
                ));
            }
            _ => {}
        }
        routed.outcome
    }

    fn key_event(
        &self,
        snapshot: &LayoutSnapshot,
        layout: &LayoutNode,
        key: KeyCode,
        event: &Event,
        cx: &mut EventCx<'_>,
    ) -> EventOutcome {
        let routed = self.route(snapshot, layout, event, cx).outcome;
        if routed.is_handled() {
            return routed;
        }
        let mut state = self.state.borrow_mut();
        if !self.chrome.keyboard || !state.scroll.interaction.focused {
            return EventOutcome::Ignored;
        }
        let action = match key {
            KeyCode::Up => VirtualListAction::ScrollRequested {
                rows: -1,
                source: ScrollInputSource::Key,
            },
            KeyCode::Down => VirtualListAction::ScrollRequested {
                rows: 1,
                source: ScrollInputSource::Key,
            },
            KeyCode::PageUp => VirtualListAction::PageRequested(-1),
            KeyCode::PageDown => VirtualListAction::PageRequested(1),
            KeyCode::Home => VirtualListAction::EdgeRequested(Edge::Top),
            KeyCode::End => VirtualListAction::EdgeRequested(Edge::Bottom),
            _ => return EventOutcome::Ignored,
        };
        Self::navigation_outcome(self.dispatch(&mut state, action))
    }
}

impl<K, Src, S> Component for VirtualListComponent<'_, K, Src, S>
where
    K: Clone + Ord + ToString,
    Src: VirtualItemSource<K> + ?Sized,
    S: Clone,
{
    fn revision(&self) -> ComponentRevision {
        let state = self.state.borrow();
        let len = self.source.len();
        let mut layout = DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.chrome.scrollbar.hash(&mut layout);
        self.environment.hash(&mut layout);
        state.generation.hash(&mut layout);
        state.overscroll.hash(&mut layout);
        state.pending.regions.hash(&mut layout);
        state.pending.invalidated.len().hash(&mut layout);
        state.pending.full.hash(&mut layout);
        len.hash(&mut layout);
        let viewport = state.snapshot.map_or_else(
            || state.last_viewport(),
            |snapshot| snapshot.resolved.viewport_height,
        );
        state
            .resolve(viewport, self.sources)
            .map(|resolved| resolved.top_offset)
            .hash(&mut layout);
        state.position().is_follow_bottom().hash(&mut layout);
        self.estimate.hash(&mut layout);
        // Eager sources do not note changes, so their whole membership is part
        // of the revision; lazy sources contribute only visible items.
        let keyed = if self.source.always_reconcile() {
            0..len
        } else {
            state.snapshot.map_or(0..0, |snapshot| {
                snapshot.visible.start..snapshot.visible.end.min(len)
            })
        };
        for position in keyed {
            self.source.key(position).to_string().hash(&mut layout);
            self.source.layout_revision(position).hash(&mut layout);
        }

        let mut paint = DefaultHasher::new();
        format!("{:?}", self.pointer_intent).hash(&mut paint);
        format!("{:?}", self.chrome).hash(&mut paint);
        state.scroll.hash(&mut paint);
        for position in state.snapshot.map_or(0..0, |snapshot| {
            snapshot.visible.start..snapshot.visible.end.min(len)
        }) {
            self.source.paint_revision(position).hash(&mut paint);
        }
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let mut state = self.state.borrow_mut();
        self.configure(&mut state);
        let list = self.list();
        let width = constraints.max_width();
        let terminal_width = u16::try_from(width).unwrap_or(u16::MAX);
        let content_width = u64::from(
            scrollbar_layout(
                Rect::new(0, 0, terminal_width, 1),
                self.chrome.layout_policy(),
            )
            .content
            .width,
        );
        let height = if let Some(height) = constraints.max_height() {
            height
        } else {
            list.sync_with_environment(content_width, self.environment, &mut state, cx);
            state.index.total_height()
        };
        let size = constraints.constrain(LogicalSize::new(width, height));
        let chrome = scrollbar_layout(local_area_of(size), self.chrome.layout_policy());
        let content_width = u64::from(chrome.content.width);
        let viewport_height = size.height;
        let placed = list.layout(
            content_width,
            self.environment,
            viewport_height,
            self.sources,
            &mut state,
            cx,
        );
        let (mut children, visible) = self.place_items(&state, placed.visible, content_width);
        children.push(ChildLayout::new(
            0,
            placed.resolved.top_offset,
            LayoutNode::leaf(
                self.window_id(),
                LogicalSize::new(content_width, viewport_height),
            ),
        ));
        let content = LayoutNode::with_children(
            self.content_id(),
            LogicalSize::new(content_width, placed.resolved.total_height),
            children,
        );
        let mut root = vec![ChildLayout::new(0, 0, content)];
        if let Some(track) = chrome.vertical_scrollbar {
            root.push(ChildLayout::new(
                track.x.into(),
                track.y.into(),
                LayoutNode::leaf(
                    self.scrollbar_id(),
                    LogicalSize::new(track.width.into(), track.height.into()),
                ),
            ));
        }
        state.snapshot = Some(LayoutSnapshot {
            size,
            content: chrome.content,
            scrollbar: chrome.vertical_scrollbar,
            resolved: placed.resolved,
            visible,
            measurement_generation: state.generation,
        });
        LayoutNode::with_children(self.id.clone(), size, root)
            .with_metadata(LayoutMetadata::new().semantic("list"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let (snapshot, positions, disabled) = {
            let state = self.state.borrow();
            let Some(snapshot) = state.snapshot_for(layout) else {
                return;
            };
            let positions = clip_rows(snapshot.content, cx).map(|rows| {
                let range = state.index.visible_range(
                    snapshot.resolved.top_offset.saturating_add(rows.start),
                    rows.end - rows.start,
                );
                range.start..range.end
            });
            (snapshot, positions, state.scroll.interaction.disabled)
        };
        let area = local_area_of(layout.size);
        if area.is_empty() {
            return;
        }
        if self.chrome.keyboard
            || self.chrome.mouse_wheel
            || !matches!(self.pointer_intent, PointerIntent::Auto)
        {
            cx.push_hit(
                HitRegion::new(self.id.as_str(), snapshot.content)
                    .role(HitRole::Scroll)
                    .pointer_intent(self.pointer_intent)
                    .pointer_events(self.chrome.mouse_wheel)
                    .focusable(self.chrome.keyboard)
                    .enabled(!disabled),
            );
        }
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            snapshot.content,
            "list",
        ));
        // Items paint while the state is borrowed immutably, so their retained
        // layouts are borrowed rather than copied.
        let state = self.state.borrow();
        let report = positions.map_or_else(VirtualListRenderStats::default, |positions| {
            let content = snapshot.content;
            let width = u64::from(content.width);
            let items = self.placed(&state, layout, &snapshot, positions);
            let items = items.into_iter().filter_map(|item| {
                let node = self.item_layout(&state, &item, width)?;
                debug_assert_eq!(node.size.height, item.height);
                Some((item, node))
            });
            let mut report = VirtualListRenderStats::default();
            cx.with_child(
                i32::from(content.x),
                i64::from(content.y),
                LocalRect::new(0, 0, content.width, content.height),
                |cx| {
                    report = self.list().paint_items(
                        Rect::new(0, 0, content.width, content.height),
                        snapshot.resolved.top_offset,
                        items,
                        cx,
                    );
                },
            );
            report
        });
        state.record(|stats| {
            stats.painted_items = report.painted_items;
            stats.registered_items = report.registered_items;
            stats.built_items = stats.built_items.saturating_add(report.built_items);
        });
        if let Some(track) = snapshot.scrollbar {
            let model = state.scrollbar_model(snapshot.resolved);
            if self.chrome.scrollbar.shown_without_overflow() || model.max_offset() > 0 {
                Scrollbar::new()
                    .policy(self.chrome.vertical_policy())
                    .styles(self.chrome.scrollbar_styles)
                    .paint(track, &model, cx);
            }
        }
        cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let snapshot = {
            let mut state = self.state.borrow_mut();
            let disabled = state.scroll.interaction.disabled;
            match state.snapshot_for(layout) {
                Some(snapshot) if !disabled => snapshot,
                _ => {
                    // A stale or disabled list consumes nothing, but a release
                    // still ends any capture it started.
                    let releasing = matches!(
                        event,
                        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Up(_))
                    );
                    if disabled || releasing {
                        state.release_pointer();
                    }
                    return EventOutcome::Ignored;
                }
            }
        };
        match event {
            Event::Mouse(mouse) => self.mouse_event(&snapshot, layout, *mouse, event, cx),
            Event::Key(stroke) => self.key_event(&snapshot, layout, stroke.key, event, cx),
            _ => self.route(&snapshot, layout, event, cx).outcome,
        }
    }
}

#[cfg(test)]
mod tests;
