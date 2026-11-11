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

use super::marks::{ScrollbarMark, ScrollbarMarkCells, mark_cells};
use super::sticky::{PinnedHeader, StickyHeaders, inset_for};
use super::{
    ActionDispatch, AnchorPolicy, Edge, ItemFocusTarget, ItemSlot, ListPosition, NoSourceMap,
    PageStep, PlacedItem, PositionEstimate, ResolvedPosition, RoutedEvent, ScrollInputSource,
    SourceMap, VirtualItemSource, VirtualListAction, VirtualListRenderStats, VirtualListState,
    VirtualSourceList, VisibleItemRange, anchor_for_below, clip_rows, find_item_layout,
    item_layout_id, position_at_offset, reveal_top_offset_below,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Section headers pinned at the viewport top, outermost first.
    pinned: Vec<PinnedHeader>,
    /// Content viewport rows covered by the pinned headers; scrolled items
    /// neither paint nor register interactions there.
    sticky_rows: u64,
}

impl LayoutSnapshot {
    /// Rows of the content viewport hidden from scrolled items.
    fn occluded(&self) -> u16 {
        u16::try_from(self.sticky_rows).unwrap_or(u16::MAX)
    }
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
        let snapshot = self.snapshot.as_ref()?;
        let content = &layout.children.first()?.node;
        let window = content.children.last()?;
        let placed = snapshot.visible.end - snapshot.visible.start;
        let pinned_matches = if snapshot.pinned.is_empty() {
            layout.children.len() == 1 + usize::from(snapshot.scrollbar.is_some())
        } else {
            layout.children.len() == 2 + usize::from(snapshot.scrollbar.is_some())
                && layout.children.last().is_some_and(|sticky| {
                    sticky.node.children.len() == snapshot.pinned.len()
                        && sticky.node.size.height == snapshot.sticky_rows
                })
        };
        (layout.size == snapshot.size
            && content.size.height == snapshot.resolved.total_height
            && window.y == snapshot.resolved.top_offset
            && content.children.len() == placed + 1
            && pinned_matches
            && self.generation == snapshot.measurement_generation)
            .then(|| snapshot.clone())
    }

    /// Keys of the section headers pinned by the most recent
    /// `VirtualListComponent` layout, outermost first.
    #[must_use]
    pub fn pinned_header_keys(&self) -> Vec<K> {
        self.snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
            snapshot
                .pinned
                .iter()
                .filter_map(|header| self.index.item(header.index))
                .map(|item| item.key.clone())
                .collect()
        })
    }

    /// Content viewport rows covered by pinned section headers at the most
    /// recent `VirtualListComponent` layout (zero when none are pinned).
    #[must_use]
    pub fn sticky_header_rows(&self) -> u64 {
        self.snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.sticky_rows)
    }

    /// Viewport rows that the section headers enclosing `key` (excluding the
    /// item itself) cover when the item is revealed directly below them, in
    /// a viewport of `viewport_height` rows under `sticky`.
    ///
    /// Placing the viewport top `inset` rows above the item's first row shows
    /// the item on the first row not covered by pinned headers.
    ///
    /// Returns `None` when `key` is not indexed or when membership changes
    /// noted since the last synchronization are pending (the index and the
    /// source disagree until the next layout).
    pub fn sticky_inset<Src: VirtualItemSource<K> + ?Sized>(
        &self,
        source: &Src,
        key: &K,
        viewport_height: u64,
        sticky: StickyHeaders,
    ) -> Option<u64> {
        if !self.pending.regions.is_empty() || self.pending.full {
            return None;
        }
        let position = self.index.index_of(key)?;
        Some(inset_for(
            &self.index,
            source,
            position,
            viewport_height,
            sticky,
        ))
    }

    /// A position showing item `key` on the first viewport row below the
    /// section headers that pin above it (see [`Self::sticky_inset`]),
    /// clamped like any other position at that offset. Equivalent to
    /// [`Self::sticky_anchor_for`] with [`AnchorPolicy::ItemTopAtTop`].
    ///
    /// Returns `None` under the same conditions as [`Self::sticky_inset`].
    pub fn sticky_reveal_position<Src: VirtualItemSource<K> + ?Sized>(
        &self,
        source: &Src,
        key: &K,
        viewport_height: u64,
        sticky: StickyHeaders,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<ListPosition<K, S>> {
        self.sticky_anchor_for(
            source,
            key,
            AnchorPolicy::ItemTopAtTop,
            viewport_height,
            sticky,
            sources,
        )
    }

    /// [`Self::anchor_for`] that keeps item `key` clear of the section
    /// headers that pin above it: the policy places the item in the viewport
    /// rows below [`Self::sticky_inset`] rows, relative to the current
    /// position at `viewport_height` (see [`target_top_offset_below`](super::target_top_offset_below)).
    ///
    /// `VirtualListComponent::anchor_for` calls this with the component's
    /// source, sticky policy, and source map.
    ///
    /// Returns `None` under the same conditions as [`Self::sticky_inset`].
    pub fn sticky_anchor_for<Src: VirtualItemSource<K> + ?Sized>(
        &self,
        source: &Src,
        key: &K,
        policy: AnchorPolicy,
        viewport_height: u64,
        sticky: StickyHeaders,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<ListPosition<K, S>> {
        let inset = self.sticky_inset(source, key, viewport_height, sticky)?;
        let current_top = self
            .resolve(viewport_height, sources)
            .map_or(0, |resolved| resolved.top_offset);
        anchor_for_below(
            &self.index,
            key,
            policy,
            current_top,
            self.geometry(viewport_height),
            inset,
            sources,
        )
    }

    /// [`Self::reveal_for_focus`] that keeps `target` clear of the section
    /// headers that pin above its item: rows under those headers
    /// ([`Self::sticky_inset`] of the target's item) do not count as
    /// visible, and a target revealed from [`Edge::Top`] lands on the first
    /// row below them (see [`reveal_top_offset_below`]).
    ///
    /// Returns the current position unchanged when the target is already
    /// visible below the headers. `VirtualListComponent::reveal_for_focus`
    /// calls this with the component's source, sticky policy, and source map.
    ///
    /// Returns `None` under the same conditions as [`Self::sticky_inset`]
    /// for the target's item.
    pub fn sticky_reveal_for_focus<Src: VirtualItemSource<K> + ?Sized, I>(
        &self,
        source: &Src,
        target: &ItemFocusTarget<K, I>,
        edge: Edge,
        viewport_height: u64,
        sticky: StickyHeaders,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<ListPosition<K, S>> {
        let inset = self.sticky_inset(source, &target.key, viewport_height, sticky)?;
        let geometry = self.geometry(viewport_height);
        let current = self
            .resolve(viewport_height, sources)
            .map_or(0, |resolved| resolved.top_offset);
        let top = reveal_top_offset_below(&self.index, target, edge, current, geometry, inset)?;
        Some(if top == current {
            self.position.clone()
        } else {
            position_at_offset(&self.index, top, geometry, sources)
        })
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
    sticky: StickyHeaders,
    marks: &'a [ScrollbarMark<K>],
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
            sticky: StickyHeaders::enabled(),
            marks: &[],
        }
    }

    /// Set the sticky section-header policy (enabled by default; it has no
    /// effect unless the source reports section headers through
    /// [`VirtualItemSource::section_header_level`]).
    ///
    /// While any item of a section is visible, its header item paints pinned
    /// at the top of the content viewport, below the pinned headers of
    /// enclosing sections, and the next section's header pushes it up as it
    /// arrives. Pinned headers own their hit, focus, and semantic regions
    /// (under the item's usual identity), and scrolled items neither paint
    /// nor register interactions under them.
    #[must_use]
    pub const fn sticky_headers(mut self, sticky: StickyHeaders) -> Self {
        self.sticky = sticky;
        self
    }

    /// Draw `marks` on the integrated scrollbar track, scaled to the whole
    /// collection (including an active position estimate's rows before and
    /// after the resident content). Marks draw only while the scrollbar does.
    #[must_use]
    pub const fn scrollbar_marks(mut self, marks: &'a [ScrollbarMark<K>]) -> Self {
        self.marks = marks;
        self
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

    /// Keyed position that places `key` according to `policy`, relative to
    /// the current position, in a content viewport of `viewport_height`
    /// rows. Rows covered by the section headers that pin above `key` under
    /// this component's sticky policy do not count as viewport rows, so a
    /// jump never leaves the item under them (see
    /// [`VirtualListState::sticky_anchor_for`]); with sticky headers
    /// disabled or a source without sections this equals
    /// [`VirtualListState::anchor_for`].
    ///
    /// The list does not move; apply the result with
    /// [`VirtualListState::set_position`] or controlled navigation.
    ///
    /// Returns `None` when `key` is not indexed or when membership changes
    /// noted since the last synchronization are pending.
    pub fn anchor_for(
        &self,
        key: &K,
        policy: AnchorPolicy,
        viewport_height: u64,
    ) -> Option<ListPosition<K, S>> {
        self.state.borrow().sticky_anchor_for(
            self.source,
            key,
            policy,
            viewport_height,
            self.sticky,
            self.sources,
        )
    }

    /// Position that reveals `target` approaching from `edge`, relative to
    /// the current position, in a content viewport of `viewport_height`
    /// rows, keeping it clear of the section headers that pin above its item
    /// under this component's sticky policy (see
    /// [`VirtualListState::sticky_reveal_for_focus`]); with sticky headers
    /// disabled or a source without sections this equals
    /// [`VirtualListState::reveal_for_focus`].
    ///
    /// Returns the current position unchanged when the target is already
    /// visible. The list does not move.
    ///
    /// Returns `None` when the target's item is not indexed or when
    /// membership changes noted since the last synchronization are pending.
    pub fn reveal_for_focus<I>(
        &self,
        target: &ItemFocusTarget<K, I>,
        edge: Edge,
        viewport_height: u64,
    ) -> Option<ListPosition<K, S>> {
        self.state.borrow().sticky_reveal_for_focus(
            self.source,
            target,
            edge,
            viewport_height,
            self.sticky,
            self.sources,
        )
    }

    /// Stable identity of one item's wrapper node (also its hit, focus, and
    /// semantic identifier, including while the item is a pinned header).
    #[must_use]
    pub fn item_id(&self, key: &K) -> LayoutId {
        item_layout_id(self.id.as_str(), key)
    }

    /// Stable identity of the sticky node: a child of the root, over the top
    /// of the content viewport, present while any section header is pinned.
    #[must_use]
    pub fn sticky_id(&self) -> LayoutId {
        LayoutId::new(format!("{}.sticky", self.id.as_str()))
    }

    /// Stable identity of a pinned header's node inside the sticky node. Its
    /// rectangle is the part of the content viewport the pinned header
    /// covers; hit, focus, and semantic regions still use [`Self::item_id`].
    #[must_use]
    pub fn sticky_item_id(&self, key: &K) -> LayoutId {
        LayoutId::new(format!("{}.sticky.{}", self.id.as_str(), key.to_string()))
    }

    /// Track cells covered by each scrollbar mark for `layout` (the node the
    /// most recent layout returned), in root-local coordinates. Empty when
    /// the node is stale, no scrollbar is drawn, or no mark resolves. Paint
    /// draws exactly these cells.
    #[must_use]
    pub fn scrollbar_mark_cells(&self, layout: &LayoutNode) -> Vec<ScrollbarMarkCells> {
        let state = self.state.borrow();
        state
            .snapshot_for(layout)
            .map_or_else(Vec::new, |snapshot| self.mark_cells(&state, &snapshot))
    }

    fn mark_cells(
        &self,
        state: &VirtualListState<K, S>,
        snapshot: &LayoutSnapshot,
    ) -> Vec<ScrollbarMarkCells> {
        let Some(track) = snapshot.scrollbar else {
            return Vec::new();
        };
        if self.marks.is_empty() || !self.scrollbar_drawn(state, snapshot) {
            return Vec::new();
        }
        mark_cells(&state.index, self.estimate, self.marks, track)
    }

    fn scrollbar_drawn(&self, state: &VirtualListState<K, S>, snapshot: &LayoutSnapshot) -> bool {
        snapshot.scrollbar.is_some()
            && (self.chrome.scrollbar.shown_without_overflow()
                || state.scrollbar_model(snapshot.resolved).max_offset() > 0)
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

    /// Pinned headers of `snapshot` the source still holds unchanged, with
    /// their placement.
    fn placed_pinned<'p>(
        &self,
        state: &VirtualListState<K, S>,
        snapshot: &'p LayoutSnapshot,
    ) -> Vec<(&'p PinnedHeader, PlacedItem)> {
        snapshot
            .pinned
            .iter()
            .filter_map(|header| {
                let position = state.placed_source_position(self.source, header.index)?;
                let measured = state.index.item(header.index)?;
                (measured.height == header.height).then(|| {
                    (
                        header,
                        PlacedItem {
                            index: header.index,
                            position,
                            start: header.start,
                            height: header.height,
                            layout_revision: measured.layout_revision,
                            id: item_layout_id(self.id.as_str(), &measured.key),
                        },
                    )
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
        let area = snapshot.content;
        let items = {
            let state = self.state.borrow();
            // Pinned headers come first so that routing (last slot first)
            // offers non-positional events to scrolled items bottom to top
            // and then to the pinned headers above them. Visible rows of
            // pinned headers and scrolled items never overlap.
            let pinned = self
                .placed_pinned(&state, snapshot)
                .into_iter()
                .map(|(header, item)| ItemSlot::pinned(item, area, header));
            let scrolled = self
                .placed(&state, layout, snapshot, 0..usize::MAX)
                .into_iter()
                .map(|item| {
                    ItemSlot::scrolled(
                        item,
                        area,
                        snapshot.resolved.top_offset,
                        snapshot.occluded(),
                    )
                });
            pinned.chain(scrolled).collect::<Vec<_>>()
        };
        let width = u64::from(snapshot.content.width);
        let routed = self.list().route_items(area, items, event, cx, |item| {
            let state = self.state.borrow();
            self.item_layout(&state, item, width)
                .cloned()
                .map(std::borrow::Cow::Owned)
        });
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

    /// Paint the integrated scrollbar, when drawn, and its marks over it.
    fn paint_scrollbar(
        &self,
        state: &VirtualListState<K, S>,
        snapshot: &LayoutSnapshot,
        cx: &mut PaintCx<'_, '_>,
    ) {
        let Some(track) = snapshot.scrollbar else {
            return;
        };
        if !self.scrollbar_drawn(state, snapshot) {
            return;
        }
        let model = state.scrollbar_model(snapshot.resolved);
        let scrollbar = Scrollbar::new()
            .policy(self.chrome.vertical_policy())
            .styles(self.chrome.scrollbar_styles);
        scrollbar.paint(track, &model, cx);
        let geometry = scrollbar.layout(track, &model);
        for cells in self.mark_cells(state, snapshot) {
            let mark = &self.marks[cells.mark];
            for y in cells.area.y..cells.area.bottom() {
                let position = y - track.y;
                let on_thumb = position >= geometry.thumb_start
                    && position < geometry.thumb_start.saturating_add(geometry.thumb_len);
                if on_thumb && !mark.over_thumb {
                    continue;
                }
                let (symbol, _) = scrollbar.cell_style(position, track.height, geometry);
                cx.set_cell(
                    i32::from(track.x),
                    i64::from(y),
                    mark.cell_symbol().unwrap_or(symbol),
                    mark.style,
                );
            }
        }
    }

    /// Hash a scrollbar mark's resolved rows and presentation.
    fn hash_mark(
        state: &VirtualListState<K, S>,
        mark: &ScrollbarMark<K>,
        hasher: &mut impl Hasher,
    ) {
        use super::ScrollbarMarkPosition;
        let row = |key: &K| state.index.key_offset(key);
        match &mark.position {
            ScrollbarMarkPosition::Row(row) => (0_u8, *row).hash(hasher),
            ScrollbarMarkPosition::Rows(rows) => (1_u8, rows.start, rows.end).hash(hasher),
            ScrollbarMarkPosition::Item(key) => (2_u8, row(key)).hash(hasher),
            ScrollbarMarkPosition::Items { first, last } => {
                (3_u8, row(first), row(last)).hash(hasher);
            }
        }
        mark.style.hash(hasher);
        mark.symbol.hash(hasher);
        mark.over_thumb.hash(hasher);
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
        self.sticky.hash(&mut layout);
        state.generation.hash(&mut layout);
        state.overscroll.hash(&mut layout);
        state.pending.regions.hash(&mut layout);
        state.pending.invalidated.len().hash(&mut layout);
        state.pending.full.hash(&mut layout);
        len.hash(&mut layout);
        let viewport = state.snapshot.as_ref().map_or_else(
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
            state.snapshot.as_ref().map_or(0..0, |snapshot| {
                snapshot.visible.start..snapshot.visible.end.min(len)
            })
        };
        for position in keyed {
            self.source.key(position).to_string().hash(&mut layout);
            self.source.layout_revision(position).hash(&mut layout);
        }
        let pinned = state
            .snapshot
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.pinned.as_slice());
        for header in pinned.iter().filter(|header| header.index < len) {
            self.source.key(header.index).to_string().hash(&mut layout);
            self.source.layout_revision(header.index).hash(&mut layout);
        }

        let mut paint = DefaultHasher::new();
        format!("{:?}", self.pointer_intent).hash(&mut paint);
        format!("{:?}", self.chrome).hash(&mut paint);
        state.scroll.hash(&mut paint);
        for position in state.snapshot.as_ref().map_or(0..0, |snapshot| {
            snapshot.visible.start..snapshot.visible.end.min(len)
        }) {
            self.source.paint_revision(position).hash(&mut paint);
        }
        for header in pinned.iter().filter(|header| header.index < len) {
            self.source.paint_revision(header.index).hash(&mut paint);
        }
        for mark in self.marks {
            Self::hash_mark(&state, mark, &mut paint);
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
        let (placed, pinned) = list.layout_pinned(
            content_width,
            self.environment,
            viewport_height,
            self.sources,
            self.sticky,
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
        let sticky_rows = pinned.last().map_or(0, |header| header.visible.end);
        if !pinned.is_empty() {
            let headers = pinned
                .iter()
                .filter_map(|header| {
                    let key = &state.index.item(header.index)?.key;
                    Some(ChildLayout::new(
                        0,
                        header.visible.start,
                        LayoutNode::leaf(
                            self.sticky_item_id(key),
                            LogicalSize::new(
                                content_width,
                                header.visible.end - header.visible.start,
                            ),
                        ),
                    ))
                })
                .collect();
            root.push(ChildLayout::new(
                chrome.content.x.into(),
                chrome.content.y.into(),
                LayoutNode::with_children(
                    self.sticky_id(),
                    LogicalSize::new(content_width, sticky_rows),
                    headers,
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
            pinned,
            sticky_rows,
        });
        LayoutNode::with_children(self.id.clone(), size, root)
            .with_metadata(LayoutMetadata::new().semantic("list"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let (snapshot, clip, positions, disabled) = {
            let state = self.state.borrow();
            let Some(snapshot) = state.snapshot_for(layout) else {
                return;
            };
            let clip = clip_rows(snapshot.content, cx);
            // Scrolled items under the pinned headers are neither painted nor
            // registered, so only rows below them select items.
            let positions = clip
                .clone()
                .map(|rows| rows.start.max(snapshot.sticky_rows)..rows.end)
                .filter(|rows| !rows.is_empty())
                .map(|rows| {
                    let range = state.index.visible_range(
                        snapshot.resolved.top_offset.saturating_add(rows.start),
                        rows.end - rows.start,
                    );
                    range.start..range.end
                });
            (snapshot, clip, positions, state.scroll.interaction.disabled)
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
        let content = snapshot.content;
        let base = Rect::new(0, 0, content.width, content.height);
        let width = u64::from(content.width);
        let scrolled = positions.map_or_else(Vec::new, |positions| {
            self.placed(&state, layout, &snapshot, positions)
        });
        let scrolled = scrolled.into_iter().map(|item| {
            ItemSlot::scrolled(
                item,
                base,
                snapshot.resolved.top_offset,
                snapshot.occluded(),
            )
        });
        // Pinned headers paint after (over) scrolled items; only those
        // intersecting the paint clip are built.
        let pinned = clip.map_or_else(Vec::new, |rows| {
            self.placed_pinned(&state, &snapshot)
                .into_iter()
                .filter(|(header, _)| {
                    header.visible.start < rows.end && rows.start < header.visible.end
                })
                .map(|(header, item)| ItemSlot::pinned(item, base, header))
                .collect()
        });
        let items = scrolled.chain(pinned).filter_map(|slot| {
            let node = self.item_layout(&state, &slot.item, width)?;
            debug_assert_eq!(node.size.height, slot.item.height);
            Some((slot, node))
        });
        let mut report = VirtualListRenderStats::default();
        cx.with_child(
            i32::from(content.x),
            i64::from(content.y),
            LocalRect::new(0, 0, content.width, content.height),
            |cx| {
                report = self.list().paint_items(content.width, items, cx);
            },
        );
        state.record(|stats| {
            stats.painted_items = report.painted_items;
            stats.registered_items = report.registered_items;
            stats.built_items = stats.built_items.saturating_add(report.built_items);
        });
        self.paint_scrollbar(&state, &snapshot, cx);
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
