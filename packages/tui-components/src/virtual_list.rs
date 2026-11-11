//! Visible-only rendering for keyed variable-height component collections.
//!
//! [`VirtualListState`] stores the scroll position as a keyed
//! [`ListPosition`] (an item key plus a row or caller source position inside
//! it) and derives collection rows from the incremental [`MeasuredListIndex`]
//! whenever they are needed, so positions survive insertion, removal, reorder,
//! and width reflow. Items come from a [`VirtualItemSource`], which constructs
//! components lazily: only items that need measurement and visible items are
//! built. The eager [`VirtualList`] builder is one such source.
//!
//! See the "Virtualized scrolling" section of `docs/tui-framework.md` for the
//! complete contract.

mod action;
mod component;
mod focus;
mod marks;
mod motion;
mod position;
mod selection;
mod source;
mod sticky;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, Element, EventCx, LayoutCache, LayoutCx,
    LayoutEnvironment, LayoutId, LayoutNode, LogicalRect,
};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::pointer::PointerIntent;
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::Easing;

pub use action::{ActionDispatch, PageStep, ScrollInputSource, VirtualListAction};
pub use bmux_tui::measured_list::{
    MeasuredListError, MeasuredListIndex, MeasuredListItem, VisibleItemRange,
};
pub use component::{VirtualListChrome, VirtualListComponent};
pub use focus::{
    FocusTraversal, ItemFocusTarget, reveal_for_focus, reveal_top_offset, reveal_top_offset_below,
};
pub use marks::{ScrollbarMark, ScrollbarMarkCells, ScrollbarMarkPosition};
pub use motion::{MotionSample, ScrollMotion, ScrollTarget};
pub use position::{
    AnchorOffset, AnchorPolicy, Edge, ListAnchor, ListGeometry, ListPosition, NoSourceMap,
    Overscroll, ResolvedPosition, SourceMap, anchor_at_offset, anchor_for, anchor_for_below,
    position_at_offset, resolve_position, target_top_offset, target_top_offset_below,
};
pub use selection::{
    ItemOrder, KeyOrder, LogicalSelection, LogicalSelectionEndpoint, LogicalSelectionFragment,
};
pub use source::VirtualItemSource;
pub use sticky::StickyHeaders;

pub use crate::resident_window::{EdgeRows, FractionTarget, PositionEstimate, WindowSplice};
use crate::scroll_view::{ScrollViewState, scrollbar_state};

/// Structural work counters for virtual-list layout and rendering.
///
/// [`VirtualListState::render_stats`] reports the counters accumulated since
/// the most recent synchronization: `measured_items` and `built_items` from
/// that synchronization plus construction during later paints and events,
/// and `painted_items` / `registered_items` from the most recent paint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VirtualListRenderStats {
    /// Items whose components were painted.
    pub painted_items: usize,
    /// Visible item interaction regions registered.
    pub registered_items: usize,
    /// Item components constructed through [`VirtualItemSource::build`].
    pub built_items: usize,
    /// Items whose exact height was (re)determined, through either a built
    /// component or a source-retained layout.
    pub measured_items: usize,
}

/// Who owns a [`VirtualListState`]'s scroll position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PositionControl {
    /// The list applies its own scroll actions (the default).
    #[default]
    Uncontrolled,
    /// The caller owns the position; input only queues actions.
    Controlled,
}

/// Result of [`VirtualSourceList::layout`]: the resolved position and the
/// items intersecting the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualListLayout {
    /// Resolved collection rows.
    pub resolved: ResolvedPosition,
    /// Items intersecting the viewport.
    pub visible: VisibleItemRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SyncKey {
    width: u64,
    environment: LayoutEnvironment,
}

/// One composed region of noted splices: index rows `old_start..old_end`
/// correspond to source rows `new_start..new_end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DirtyRegion {
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
}

impl DirtyRegion {
    const fn old_len(self) -> usize {
        self.old_end - self.old_start
    }

    const fn new_len(self) -> usize {
        self.new_end - self.new_start
    }

    const fn is_identity(self) -> bool {
        self.old_start == self.old_end && self.new_start == self.new_end
    }
}

/// Disjoint composed regions retained before they are merged into one span,
/// bounding note and lookup cost.
const MAX_DIRTY_REGIONS: usize = 16;

/// Noted changes not yet reconciled against the source.
///
/// `regions` are sorted, disjoint, and non-adjacent in both coordinate
/// spaces; rows outside every region correspond one-to-one, shifted by the
/// size change of the regions before them. Keeping separate regions means a
/// prepend and an append noted in the same frame cost `O(k)`, not `O(n)`.
#[derive(Debug)]
struct PendingChanges<K> {
    regions: Vec<DirtyRegion>,
    invalidated: BTreeSet<K>,
    full: bool,
}

impl<K> PendingChanges<K> {
    const fn new() -> Self {
        Self {
            regions: Vec::new(),
            invalidated: BTreeSet::new(),
            full: false,
        }
    }

    /// Net change in length, as `(inserted rows, removed rows)`.
    fn length_change(&self) -> (usize, usize) {
        self.regions.iter().fold((0, 0), |(new, old), region| {
            (new + region.new_len(), old + region.old_len())
        })
    }

    /// Compose a splice of source rows `removed` (in coordinates after all
    /// previously noted splices) replaced by `inserted` rows.
    fn note(&mut self, removed: &Range<usize>, inserted: usize) {
        let regions = &mut self.regions;
        // Regions touching or overlapping `removed` (in source coordinates)
        // merge with it; earlier regions stay; later regions shift.
        let first = regions.partition_point(|region| region.new_end < removed.start);
        let last =
            first + regions[first..].partition_point(|region| region.new_start <= removed.end);
        let (before_new, before_old) =
            regions[..first].iter().fold((0, 0), |(new, old), region| {
                (new + region.new_len(), old + region.old_len())
            });
        let (merged_new, merged_old) = regions[first..last]
            .iter()
            .fold((before_new, before_old), |(new, old), region| {
                (new + region.new_len(), old + region.old_len())
            });
        let merging = &regions[first..last];
        let new_start = merging
            .first()
            .map_or(removed.start, |region| region.new_start.min(removed.start));
        let old_start = match merging.first() {
            Some(region) if region.new_start <= removed.start => region.old_start,
            _ => removed.start + before_old - before_new,
        };
        let span_end = merging
            .last()
            .map_or(removed.end, |region| region.new_end.max(removed.end));
        let old_end = match merging.last() {
            Some(region) if region.new_end >= removed.end => region.old_end,
            _ => removed.end + merged_old - merged_new,
        };
        let merged = DirtyRegion {
            old_start,
            old_end,
            new_start,
            new_end: span_end - removed.len() + inserted,
        };
        for region in &mut regions[last..] {
            region.new_start = region.new_start + inserted - removed.len();
            region.new_end = region.new_end + inserted - removed.len();
        }
        regions.splice(first..last, (!merged.is_identity()).then_some(merged));
        if regions.len() > MAX_DIRTY_REGIONS {
            let (first, last) = (regions[0], regions[regions.len() - 1]);
            *regions = vec![DirtyRegion {
                old_start: first.old_start,
                old_end: last.old_end,
                new_start: first.new_start,
                new_end: last.new_end,
            }];
        }
    }

    /// Source row expected to hold index row `index` after the noted splices,
    /// or `None` when a noted splice replaced that row. Callers still verify
    /// the key there, since unnoted changes (which schedule a full
    /// reconciliation) are not reflected.
    fn source_row(&self, index: usize) -> Option<usize> {
        let (mut new, mut old) = (0, 0);
        for region in &self.regions {
            if index < region.old_start {
                break;
            }
            if index < region.old_end {
                return None;
            }
            new += region.new_len();
            old += region.old_len();
        }
        Some(index + new - old)
    }
}

/// Interior-mutable work counters updated by `&self` paint and event paths.
/// Atomics (rather than `Cell`) keep the state `Sync`.
#[derive(Debug, Default)]
struct StatsCell {
    painted: AtomicUsize,
    registered: AtomicUsize,
    built: AtomicUsize,
    measured: AtomicUsize,
}

impl StatsCell {
    fn get(&self) -> VirtualListRenderStats {
        VirtualListRenderStats {
            painted_items: self.painted.load(Ordering::Relaxed),
            registered_items: self.registered.load(Ordering::Relaxed),
            built_items: self.built.load(Ordering::Relaxed),
            measured_items: self.measured.load(Ordering::Relaxed),
        }
    }

    fn set(&self, stats: VirtualListRenderStats) {
        self.painted.store(stats.painted_items, Ordering::Relaxed);
        self.registered
            .store(stats.registered_items, Ordering::Relaxed);
        self.built.store(stats.built_items, Ordering::Relaxed);
        self.measured.store(stats.measured_items, Ordering::Relaxed);
    }
}

/// Removed keys tolerated before stale retained layouts are swept.
const LAYOUT_SWEEP_MIN: usize = 64;

/// Retained state for keyed variable-height collection layout.
///
/// The scroll position is a keyed [`ListPosition`]. In uncontrolled mode (the
/// default, [`Self::new`]) the list applies its own scroll actions and the
/// legacy offset API (`scroll`, [`Self::scroll_by`], [`Self::restore_anchor`],
/// ...) stays available: offsets written through it are adopted as a keyed
/// position at the next position query. In controlled mode
/// ([`Self::controlled`]) the caller owns the position and input only queues
/// [`VirtualListAction`]s.
#[derive(Debug)]
pub struct VirtualListState<K, S = ()> {
    index: MeasuredListIndex<K>,
    layouts: LayoutCache,
    anchor: Option<(K, usize, u64)>,
    /// Shared logical scroll state. After each layout its vertical offset
    /// mirrors the resolved top offset and its follow flag mirrors
    /// [`ListPosition::FollowBottom`].
    pub scroll: ScrollViewState,
    control: PositionControl,
    position: ListPosition<K, S>,
    /// Scroll offset and follow flag last written by the position model, used
    /// to detect legacy offset writes.
    mirror: (u64, bool),
    overscroll: Overscroll,
    page_step: PageStep,
    estimate: Option<PositionEstimate>,
    resolved: Option<ResolvedPosition>,
    resolved_valid: bool,
    anchor_index: Option<usize>,
    anchor_missing: bool,
    actions: Vec<VirtualListAction<K>>,
    pending: PendingChanges<K>,
    synced: Option<SyncKey>,
    generation: u64,
    removed_layouts: usize,
    stats: StatsCell,
    motion: Option<ScrollMotion<K, S>>,
    snapshot: Option<component::LayoutSnapshot>,
    pressed: Option<K>,
}

impl<K> VirtualListState<K>
where
    K: Clone + Ord,
{
    /// Create empty uncontrolled state with one logical inter-item gap.
    #[must_use]
    pub fn new(gap: u64) -> Self {
        Self::with_control(gap, PositionControl::Uncontrolled)
    }

    /// Create empty controlled state with one logical inter-item gap.
    #[must_use]
    pub fn controlled(gap: u64) -> Self {
        Self::with_control(gap, PositionControl::Controlled)
    }
}

impl<K> Default for VirtualListState<K>
where
    K: Clone + Ord,
{
    fn default() -> Self {
        Self::new(0)
    }
}

impl<K, S> VirtualListState<K, S>
where
    K: Clone + Ord,
    S: Clone,
{
    /// Create empty state with an explicit position owner and source-position
    /// type `S`.
    #[must_use]
    pub fn with_control(gap: u64, control: PositionControl) -> Self {
        let scroll = ScrollViewState::new();
        Self {
            index: MeasuredListIndex::new(gap),
            layouts: LayoutCache::new(),
            anchor: None,
            mirror: (scroll.vertical_offset(), scroll.follows_bottom()),
            scroll,
            control,
            position: ListPosition::Top,
            overscroll: Overscroll::None,
            page_step: PageStep::Viewport,
            estimate: None,
            resolved: None,
            resolved_valid: false,
            anchor_index: None,
            anchor_missing: false,
            actions: Vec::new(),
            pending: PendingChanges::new(),
            synced: None,
            generation: 0,
            removed_layouts: 0,
            stats: StatsCell::default(),
            motion: None,
            snapshot: None,
            pressed: None,
        }
    }

    /// Whether the caller owns the position.
    #[must_use]
    pub const fn is_controlled(&self) -> bool {
        matches!(self.control, PositionControl::Controlled)
    }

    /// Who owns the position.
    #[must_use]
    pub const fn control(&self) -> PositionControl {
        self.control
    }

    /// The exact measured index.
    #[must_use]
    pub const fn index(&self) -> &MeasuredListIndex<K> {
        &self.index
    }

    /// Counter that changes whenever measured geometry changes.
    #[must_use]
    pub const fn measurement_generation(&self) -> u64 {
        self.generation
    }

    /// Set the blank tail policy below content.
    pub fn set_overscroll(&mut self, overscroll: Overscroll) {
        if self.overscroll != overscroll {
            self.adopt_scroll();
            self.overscroll = overscroll;
            self.resolved_valid = false;
        }
    }

    /// The blank tail policy below content.
    #[must_use]
    pub const fn overscroll(&self) -> Overscroll {
        self.overscroll
    }

    /// Set the rows moved by [`VirtualListAction::PageRequested`].
    pub const fn set_page_step(&mut self, page_step: PageStep) {
        self.page_step = page_step;
    }

    /// Rows moved by [`VirtualListAction::PageRequested`].
    #[must_use]
    pub const fn page_step(&self) -> PageStep {
        self.page_step
    }

    /// Set the presentation-only scrollbar estimate that
    /// [`VirtualListAction::ScrollbarDrag`] fractions are relative to. An
    /// estimate whose `resident` rows differ from the exact total is ignored.
    pub const fn set_position_estimate(&mut self, estimate: Option<PositionEstimate>) {
        self.estimate = estimate;
    }

    /// The scrollbar estimate set with [`Self::set_position_estimate`].
    #[must_use]
    pub const fn position_estimate(&self) -> Option<PositionEstimate> {
        self.estimate
    }

    const fn geometry(&self, viewport_height: u64) -> ListGeometry {
        ListGeometry::new(viewport_height).overscroll(self.overscroll)
    }

    fn last_viewport(&self) -> u64 {
        self.resolved.map_or(0, |resolved| resolved.viewport_height)
    }

    fn scroll_written_externally(&self) -> bool {
        (self.scroll.vertical_offset(), self.scroll.follows_bottom()) != self.mirror
    }

    fn effective_position(&self) -> ListPosition<K, S> {
        if self.is_controlled() || !self.scroll_written_externally() {
            return self.position.clone();
        }
        if self.scroll.follows_bottom() {
            ListPosition::FollowBottom
        } else {
            position_at_offset(
                &self.index,
                self.scroll.vertical_offset(),
                self.geometry(self.last_viewport()),
                &NoSourceMap,
            )
        }
    }

    /// Adopt offsets written through the legacy scroll API as the keyed
    /// position (uncontrolled mode only).
    fn adopt_scroll(&mut self) {
        if !self.is_controlled() && self.scroll_written_externally() {
            // An explicit legacy write interrupts a running motion.
            self.motion = None;
            self.position = self.effective_position();
            self.anchor_index = self
                .position
                .anchor()
                .and_then(|anchor| self.index.index_of(&anchor.key));
            self.mirror = (self.scroll.vertical_offset(), self.scroll.follows_bottom());
            self.resolved_valid = false;
        }
    }

    /// Store a position and, in uncontrolled mode, mirror it into `scroll`.
    fn commit_position(&mut self, position: ListPosition<K, S>, top: Option<u64>) {
        self.anchor_index = position
            .anchor()
            .and_then(|anchor| self.index.index_of(&anchor.key))
            .or(self.anchor_index);
        self.position = position;
        if !self.is_controlled() {
            if let Some(top) = top {
                self.scroll.set_vertical_offset(top);
            }
            self.scroll
                .set_follow_bottom(self.position.is_follow_bottom());
            self.mirror = (self.scroll.vertical_offset(), self.scroll.follows_bottom());
        }
    }

    /// Replace the keyed position. Controlled lists never change it
    /// themselves; uncontrolled lists continue from it, and a running motion
    /// is cancelled.
    pub fn set_position(&mut self, position: ListPosition<K, S>) {
        self.motion = None;
        let top = self.resolved.and_then(|resolved| {
            resolve_position(
                &self.index,
                &position,
                self.geometry(resolved.viewport_height),
                &NoSourceMap,
            )
            .map(|resolved| resolved.top_offset)
        });
        self.commit_position(position, top);
        self.resolved_valid = false;
    }

    /// The current keyed position. In uncontrolled mode this includes offsets
    /// written through the legacy scroll API since the last layout.
    #[must_use]
    pub fn position(&self) -> ListPosition<K, S> {
        self.effective_position()
    }

    /// Replace [`ListPosition::FollowBottom`] with a keyed position at the
    /// current top offset. Returns whether the position was following.
    pub fn release_follow(&mut self) -> bool {
        self.adopt_scroll();
        if !self.position.is_follow_bottom() {
            return false;
        }
        let viewport = self.last_viewport();
        let geometry = self.geometry(viewport);
        let top = geometry.max_content_top(self.index.total_height());
        let position = position_at_offset(&self.index, top, geometry, &NoSourceMap);
        self.commit_position(position, Some(top));
        self.resolved_valid = false;
        true
    }

    /// Resolve the current position for a viewport without mutating anything.
    /// Returns `None` when the anchor key is not indexed; layout then applies
    /// the missing-anchor fallback described on [`Self::resolve_layout`].
    #[must_use]
    pub fn resolve(
        &self,
        viewport_height: u64,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<ResolvedPosition> {
        resolve_position(
            &self.index,
            &self.effective_position(),
            self.geometry(viewport_height),
            sources,
        )
    }

    /// The position resolved by the most recent layout.
    #[must_use]
    pub const fn resolved(&self) -> Option<ResolvedPosition> {
        self.resolved
    }

    /// Whether the most recent layout could not resolve a controlled anchor.
    #[must_use]
    pub const fn anchor_missing(&self) -> bool {
        self.anchor_missing
    }

    /// Keyed position that places `key` according to `policy`, relative to
    /// the current position. Returns `None` when `key` is not indexed.
    #[must_use]
    pub fn anchor_for(
        &self,
        key: &K,
        policy: AnchorPolicy,
        viewport_height: u64,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<ListPosition<K, S>> {
        let current_top = self
            .resolve(viewport_height, sources)
            .map_or(0, |resolved| resolved.top_offset);
        anchor_for(
            &self.index,
            key,
            policy,
            current_top,
            self.geometry(viewport_height),
            sources,
        )
    }

    /// Resolve the position for layout, apply the missing-anchor fallback,
    /// and mirror the result into `scroll`.
    ///
    /// A missing anchor key is handled per mode:
    ///
    /// * controlled: the previous top offset is reused, clamped to the new
    ///   geometry, the position is left unchanged, and
    ///   [`Self::anchor_missing`] becomes true;
    /// * uncontrolled: the anchor moves to the item now at its last resolved
    ///   index (adjusted for noted splices before it; the last item when the
    ///   collection shrank past it), keeping its row clamped to that item, or
    ///   to [`ListPosition::Top`] when the collection is empty.
    pub fn resolve_layout(
        &mut self,
        viewport_height: u64,
        sources: &dyn SourceMap<K, S>,
    ) -> ResolvedPosition {
        self.adopt_scroll();
        let geometry = self.geometry(viewport_height);
        let resolved = if let Some(resolved) =
            resolve_position(&self.index, &self.position, geometry, sources)
        {
            self.anchor_missing = false;
            resolved
        } else if self.is_controlled() {
            self.anchor_missing = true;
            let total_height = self.index.total_height();
            let max_content_top = geometry.max_content_top(total_height);
            let max_overscroll_top = geometry.max_overscroll_top(total_height);
            let limit = if matches!(self.position, ListPosition::Overscrolled(_)) {
                max_overscroll_top
            } else {
                max_content_top
            };
            ResolvedPosition {
                top_offset: self
                    .resolved
                    .map_or(0, |resolved| resolved.top_offset)
                    .min(limit),
                viewport_height,
                total_height,
                max_content_top,
                max_overscroll_top,
            }
        } else {
            self.anchor_missing = false;
            self.position = self.remapped_anchor();
            resolve_position(&self.index, &self.position, geometry, sources).unwrap_or_else(|| {
                let total_height = self.index.total_height();
                ResolvedPosition {
                    top_offset: 0,
                    viewport_height,
                    total_height,
                    max_content_top: geometry.max_content_top(total_height),
                    max_overscroll_top: geometry.max_overscroll_top(total_height),
                }
            })
        };
        if let Some(anchor) = self.position.anchor()
            && let Some(position) = self.index.index_of(&anchor.key)
        {
            self.anchor_index = Some(position);
        }
        self.resolved = Some(resolved);
        self.resolved_valid = true;
        self.scroll.set_vertical_offset(resolved.top_offset);
        self.scroll
            .set_follow_bottom(self.position.is_follow_bottom());
        self.mirror = (self.scroll.vertical_offset(), self.scroll.follows_bottom());
        resolved
    }

    /// Uncontrolled fallback for an anchor whose key was removed.
    fn remapped_anchor(&self) -> ListPosition<K, S> {
        let Some(anchor) = self.position.anchor() else {
            return self.position.clone();
        };
        let Some(last) = self.index.len().checked_sub(1) else {
            return ListPosition::Top;
        };
        let position = self.anchor_index.unwrap_or(0).min(last);
        let Some(item) = self.index.item(position) else {
            return ListPosition::Top;
        };
        let row = match anchor.offset {
            AnchorOffset::Row(row) | AnchorOffset::Source { row_delta: row, .. } => row,
        };
        let extent = position::item_extent(&self.index, position);
        let row = if extent == 0 { 0 } else { row.min(extent - 1) };
        let anchor = ListAnchor::row(item.key.clone(), row);
        if matches!(self.position, ListPosition::Overscrolled(_)) {
            ListPosition::Overscrolled(anchor)
        } else {
            ListPosition::Anchored(anchor)
        }
    }

    /// Current resolution for applying an action: the last layout when still
    /// valid, otherwise a fresh resolution at the last viewport height.
    fn current_resolution(&mut self, sources: &dyn SourceMap<K, S>) -> ResolvedPosition {
        self.adopt_scroll();
        match self.resolved {
            Some(resolved) if self.resolved_valid => resolved,
            _ => self.resolve_layout(self.last_viewport(), sources),
        }
    }

    fn move_to(
        &mut self,
        current: ResolvedPosition,
        top: u64,
        follow: bool,
        sources: &dyn SourceMap<K, S>,
    ) -> bool {
        let geometry = self.geometry(current.viewport_height);
        let position = if follow {
            ListPosition::FollowBottom
        } else if top == 0 && self.index.is_empty() {
            ListPosition::Top
        } else {
            position_at_offset(&self.index, top, geometry, sources)
        };
        // Equivalent positions at the same row (`Top` and an anchor on row 0)
        // are not a change, nor is starting to follow without moving (so an
        // enclosing scroller can take the input); releasing a follow is.
        let changed = top != current.top_offset
            || (self.position.is_follow_bottom() && !position.is_follow_bottom());
        self.commit_position(position, Some(top));
        self.resolved = Some(ResolvedPosition {
            top_offset: top,
            ..current
        });
        self.resolved_valid = true;
        changed
    }

    /// The position estimate when it describes the resolved content (its
    /// resident rows equal the exact total).
    fn active_estimate(&self, current: ResolvedPosition) -> Option<PositionEstimate> {
        self.estimate
            .filter(|estimate| estimate.resident == current.total_height)
    }

    /// Whether the active estimate reports content beyond `edge` of the
    /// resident rows. Following the bottom is entered only when no content
    /// follows, because the resident bottom is then not the true tail.
    fn more_beyond(&self, current: ResolvedPosition, edge: Edge) -> bool {
        self.active_estimate(current)
            .is_some_and(|estimate| match edge {
                Edge::Top => estimate.before > 0,
                Edge::Bottom => estimate.after > 0,
            })
    }

    fn scroll_rows(&mut self, rows: i64, sources: &dyn SourceMap<K, S>) -> bool {
        if rows == 0 {
            return false;
        }
        let current = self.current_resolution(sources);
        let top = current
            .top_offset
            .saturating_add_signed(rows)
            .min(current.max_overscroll_top);
        let follow =
            rows > 0 && top == current.max_content_top && !self.more_beyond(current, Edge::Bottom);
        self.move_to(current, top, follow, sources)
    }

    /// Move to a scrollbar fraction. Returns whether the position changed
    /// and whether the target lies outside the resident rows described by the
    /// position estimate.
    fn scroll_to_fraction(&mut self, fraction: f64, sources: &dyn SourceMap<K, S>) -> (bool, bool) {
        let current = self.current_resolution(sources);
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let viewport = current.viewport_height;
        let (top, outside) = self.active_estimate(current).map_or_else(
            || (fraction_of(current.max_content_top, fraction), false),
            |estimate| match estimate.target_for_fraction(fraction, viewport) {
                FractionTarget::Before { .. } => (0, true),
                FractionTarget::Resident { offset } => (offset, false),
                FractionTarget::After { .. } => (current.max_content_top, true),
            },
        );
        let top = top.min(current.max_overscroll_top);
        let follow = current.max_content_top > 0
            && top == current.max_content_top
            && !self.more_beyond(current, Edge::Bottom);
        (self.move_to(current, top, follow, sources), outside)
    }

    /// Apply an action to an uncontrolled list's own position, resolving
    /// source anchors without a [`SourceMap`]. See [`Self::apply_action_with`].
    pub fn apply_action(&mut self, action: &VirtualListAction<K>) -> bool {
        self.apply_action_with(action, &NoSourceMap)
    }

    /// Apply an action to an uncontrolled list's own position and return
    /// whether the position changed. Controlled lists return `false` without
    /// changing anything; `Activated` never changes the position.
    ///
    /// Actions move relative to the most recent layout (re-resolved at the
    /// last viewport height when content or the position changed since):
    ///
    /// * a downward move ending exactly at `max_content_top` enters
    ///   [`ListPosition::FollowBottom`]; ending beyond it enters
    ///   [`ListPosition::Overscrolled`];
    /// * when an active position estimate reports rows after the resident
    ///   content, reaching `max_content_top` anchors there instead of
    ///   following, because the resident bottom is not the true tail;
    /// * any upward request releases `FollowBottom`;
    /// * `EdgeRequested(Top)` moves to [`ListPosition::Top`] and
    ///   `EdgeRequested(Bottom)` to `FollowBottom`;
    /// * pages move by [`Self::page_step`] rows per page;
    /// * scrollbar fractions are relative to the scrollbar estimate when one is
    ///   set and matches the exact total, otherwise to `max_content_top`;
    /// * a scrollbar drag whose target lies before or after the resident rows,
    ///   or an edge request toward an edge with estimated rows beyond it,
    ///   moves to the nearest resident edge and is also queued (see
    ///   [`Self::drain_actions`]) so the caller can load or replace the
    ///   window;
    /// * every action except `Activated` cancels a running motion, keeping the
    ///   position of its last committed frame.
    pub fn apply_action_with(
        &mut self,
        action: &VirtualListAction<K>,
        sources: &dyn SourceMap<K, S>,
    ) -> bool {
        if self.is_controlled() {
            return false;
        }
        if !matches!(action, VirtualListAction::Activated(_)) {
            self.motion = None;
        }
        match action {
            VirtualListAction::ScrollRequested { rows, .. }
            | VirtualListAction::SelectionAutoScroll(rows) => self.scroll_rows(*rows, sources),
            VirtualListAction::PageRequested(pages) => {
                let viewport = self.current_resolution(sources).viewport_height;
                let step = i64::try_from(self.page_step.rows(viewport)).unwrap_or(i64::MAX);
                self.scroll_rows(step.saturating_mul(i64::from(*pages)), sources)
            }
            VirtualListAction::EdgeRequested(edge) => {
                let current = self.current_resolution(sources);
                if self.more_beyond(current, *edge) {
                    self.actions.push(action.clone());
                }
                match edge {
                    Edge::Top => {
                        let changed = current.top_offset != 0 || self.position.is_follow_bottom();
                        self.commit_position(ListPosition::Top, Some(0));
                        self.resolved = Some(ResolvedPosition {
                            top_offset: 0,
                            ..current
                        });
                        changed
                    }
                    Edge::Bottom => {
                        let follow = !self.more_beyond(current, Edge::Bottom);
                        self.move_to(current, current.max_content_top, follow, sources)
                    }
                }
            }
            VirtualListAction::ScrollbarDrag(fraction) => {
                let (changed, outside) = self.scroll_to_fraction(*fraction, sources);
                if outside {
                    self.actions.push(action.clone());
                }
                changed
            }
            VirtualListAction::Activated(_) => false,
        }
    }

    /// Dispatch an input action: queue it in controlled mode, apply it in
    /// uncontrolled mode (`Activated` is always queued, and actions targeting
    /// rows outside the resident window are also queued; see
    /// [`Self::apply_action_with`]).
    pub fn dispatch_action(
        &mut self,
        action: VirtualListAction<K>,
        sources: &dyn SourceMap<K, S>,
    ) -> ActionDispatch {
        if self.is_controlled() || matches!(action, VirtualListAction::Activated(_)) {
            self.actions.push(action);
            return ActionDispatch::Queued;
        }
        if self.apply_action_with(&action, sources) {
            ActionDispatch::Applied
        } else {
            ActionDispatch::Unchanged
        }
    }

    /// Queued actions, oldest first.
    #[must_use]
    pub fn pending_actions(&self) -> &[VirtualListAction<K>] {
        &self.actions
    }

    /// Take queued actions, oldest first.
    pub fn drain_actions(&mut self) -> std::vec::Drain<'_, VirtualListAction<K>> {
        self.actions.drain(..)
    }

    fn expected_source_len(&self) -> usize {
        let (inserted, removed) = self.pending.length_change();
        self.index.len() + inserted - removed
    }

    fn adjust_anchor_index(&mut self, removed: &Range<usize>, inserted: usize) {
        if let Some(position) = self.anchor_index.as_mut() {
            if *position >= removed.end {
                *position = *position - removed.len() + inserted;
            } else if *position >= removed.start {
                *position = removed.start;
            }
        }
    }

    /// Note that source rows `removed` were replaced by `inserted` new rows.
    /// The next synchronization reads keys and revisions only for the noted
    /// rows and measures only new or changed items. Ranges are in source
    /// coordinates after all previously noted splices; an out-of-bounds range
    /// schedules a full reconciliation.
    pub fn note_splice(&mut self, removed: Range<usize>, inserted: usize) {
        let current_len = self.expected_source_len();
        if removed.start > removed.end || removed.end > current_len {
            self.pending.full = true;
            return;
        }
        self.adjust_anchor_index(&removed, inserted);
        self.pending.note(&removed, inserted);
    }

    /// Note a resident-window splice (see [`Self::note_splice`]).
    pub fn note_window_splice(&mut self, splice: &WindowSplice<K>) {
        self.note_splice(splice.removed_range(), splice.inserted.len());
    }

    /// Note that one item's layout revision may have changed. The next
    /// synchronization re-reads its revision and re-measures it when stale.
    pub fn invalidate(&mut self, key: &K) {
        self.pending.invalidated.insert(key.clone());
    }

    /// Schedule a full reconciliation: every key and revision is compared at
    /// the next synchronization (only stale items are re-measured).
    pub const fn invalidate_all(&mut self) {
        self.pending.full = true;
    }

    /// Replace index rows `range` with caller-measured items now, without
    /// reading the source. Items should carry the synchronized width and
    /// capability revision; stale items are re-measured when visible or at the
    /// next full reconciliation. Mixing this with pending noted splices
    /// schedules one full reconciliation.
    ///
    /// # Errors
    ///
    /// Returns [`MeasuredListError::OutOfBounds`] when `range` exceeds the
    /// index and [`MeasuredListError::DuplicateKey`] when an inserted key is
    /// already indexed outside `range` or repeated; the index is unchanged.
    pub fn splice_measured(
        &mut self,
        range: Range<usize>,
        items: impl IntoIterator<Item = MeasuredListItem<K>>,
    ) -> Result<Vec<MeasuredListItem<K>>, MeasuredListError> {
        self.adopt_scroll();
        let items = items.into_iter().collect::<Vec<_>>();
        let inserted = items.len();
        let removed = self.index.splice(range.clone(), items)?;
        if !self.pending.regions.is_empty() {
            self.pending.full = true;
        }
        self.adjust_anchor_index(&range, inserted);
        self.removed_layouts = self.removed_layouts.saturating_add(removed.len());
        self.geometry_changed();
        Ok(removed)
    }

    /// Replace one item's caller-measured geometry. Returns whether the key
    /// is indexed.
    pub fn update_measured(&mut self, item: MeasuredListItem<K>) -> bool {
        self.adopt_scroll();
        let updated = self.index.update_item(item);
        if updated {
            self.geometry_changed();
        }
        updated
    }

    const fn geometry_changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.resolved_valid = false;
    }

    /// Work counters since the most recent synchronization.
    #[must_use]
    pub fn render_stats(&self) -> VirtualListRenderStats {
        self.stats.get()
    }

    fn record(&self, update: impl FnOnce(&mut VirtualListRenderStats)) {
        let mut stats = self.stats.get();
        update(&mut stats);
        self.stats.set(stats);
    }

    /// Start an eased motion from the current keyed position toward `target`
    /// (uncontrolled lists only; controlled lists ignore it and own their
    /// [`ScrollMotion`] directly).
    ///
    /// The start is the position committed by the most recent frame, so a
    /// motion started while another runs continues from where that one is.
    /// A zero `duration` (reduce-motion) commits the target immediately.
    /// Otherwise drive the motion with [`Self::advance_motion`] before each
    /// layout, using the caller's time source (for example
    /// `RuntimeHandle::now`).
    pub fn start_motion(
        &mut self,
        target: ScrollTarget<K, S>,
        easing: Easing,
        duration: Duration,
        now: Instant,
    ) {
        if self.is_controlled() {
            return;
        }
        self.adopt_scroll();
        self.motion = Some(ScrollMotion::new(
            self.position.clone(),
            target,
            easing,
            duration,
            now,
        ));
        if duration.is_zero() {
            self.advance_motion(now, Duration::ZERO, &NoSourceMap);
        }
    }

    /// Commit the running motion's frame at `now` as the keyed position and
    /// return when the next frame is due (`None` once the motion completed or
    /// when none is running).
    ///
    /// Both endpoints are re-resolved against the current index at the most
    /// recent layout's viewport height, so content inserted or reflowed during
    /// the motion never strands it. Completion commits the target's exact
    /// keyed position ([`ScrollTarget::final_position`]); a target whose key
    /// disappeared cancels the motion where it is. Schedule the returned
    /// instant with a keyed runtime timer (`RuntimeHandle::schedule_timer`).
    pub fn advance_motion(
        &mut self,
        now: Instant,
        frame_interval: Duration,
        sources: &dyn SourceMap<K, S>,
    ) -> Option<Instant> {
        self.adopt_scroll();
        let motion = self.motion.take()?;
        if self.is_controlled() {
            return None;
        }
        let geometry = self.geometry(self.last_viewport());
        let position = motion.position_at(now, &self.index, geometry, sources)?;
        let top = resolve_position(&self.index, &position, geometry, sources)
            .map(|resolved| resolved.top_offset);
        self.commit_position(position, top);
        self.resolved_valid = false;
        let next = motion.next_frame(now, frame_interval);
        if next.is_some() {
            self.motion = Some(motion);
        }
        next
    }

    /// Stop the running motion at its last committed frame. Returns whether a
    /// motion was running.
    pub fn cancel_motion(&mut self) -> bool {
        self.motion.take().is_some()
    }

    /// The running motion, if any.
    #[must_use]
    pub const fn motion(&self) -> Option<&ScrollMotion<K, S>> {
        self.motion.as_ref()
    }

    /// Capture the current top item and row within it as a stable mutation and
    /// reflow anchor.
    pub fn capture_anchor(&mut self) {
        let offset = self.scroll.vertical_offset();
        self.anchor = self.index.item_at_offset(offset).and_then(|index| {
            let item = self.index.item(index)?;
            let start = self.index.item_offset(index)?;
            Some((item.key.clone(), index, offset.saturating_sub(start)))
        });
    }

    /// Restore the captured stable-key anchor after synchronization.
    pub fn restore_anchor(&mut self, viewport_height: u64) {
        if self.scroll.follows_bottom() {
            let maximum = self.index.total_height().saturating_sub(viewport_height);
            self.scroll.set_vertical_offset(maximum);
            self.scroll.set_follow_bottom(true);
            return;
        }
        let Some((key, former_index, row)) = self.anchor.as_ref() else {
            self.clamp_scroll(viewport_height);
            return;
        };
        let index = self.index.index_of(key).or_else(|| {
            (!self.index.is_empty()).then(|| (*former_index).min(self.index.len() - 1))
        });
        let Some(index) = index else {
            self.scroll.set_vertical_offset(0);
            self.anchor = None;
            return;
        };
        let start = self.index.item_offset(index).unwrap_or(0);
        let height = self.index.item(index).map_or(0, |item| item.height);
        let row = (*row).min(height.saturating_sub(1));
        let maximum = self.max_scroll_top(viewport_height);
        self.scroll
            .set_vertical_offset(start.saturating_add(row).min(maximum));
        self.capture_anchor();
    }

    /// Largest legacy scroll offset: `max_overscroll_top`, which equals
    /// `total_height - viewport_height` without overscroll.
    fn max_scroll_top(&self, viewport_height: u64) -> u64 {
        self.geometry(viewport_height)
            .max_overscroll_top(self.index.total_height())
    }

    /// Scroll so one stable item begins at the viewport top.
    pub fn scroll_to_key(&mut self, key: &K, viewport_height: u64) -> bool {
        let Some(index) = self.index.index_of(key) else {
            return false;
        };
        let start = self.index.item_offset(index).unwrap_or(0);
        let maximum = self.max_scroll_top(viewport_height);
        self.scroll.set_vertical_offset(start.min(maximum));
        true
    }

    /// Ensure one complete stable item is visible with minimum movement.
    pub fn ensure_key_visible(&mut self, key: &K, viewport_height: u64) -> bool {
        let Some(index) = self.index.index_of(key) else {
            return false;
        };
        let Some(item) = self.index.item(index) else {
            return false;
        };
        let start = self.index.item_offset(index).unwrap_or(0);
        let old = self.scroll.vertical_offset();
        let offset = crate::scroll_view::reveal_offset(old, viewport_height, start, item.height);
        let maximum = self.max_scroll_top(viewport_height);
        let next = offset.min(maximum);
        if next == old {
            return false;
        }
        self.scroll.set_vertical_offset(next);
        true
    }

    /// Exact logical content extent at the synchronized width.
    #[must_use]
    pub fn total_height(&self) -> u64 {
        self.index.total_height()
    }

    /// Convert exact virtual-list geometry into terminal scrollbar state.
    #[must_use]
    pub fn scrollbar_state(&self, viewport_height: u64) -> crate::scrollbar::ScrollbarState {
        scrollbar_state(
            self.index.total_height(),
            viewport_height,
            self.scroll.vertical_offset(),
        )
    }

    /// Scroll to the first logical collection row and stop following appends.
    pub const fn scroll_to_top(&mut self) {
        self.scroll.set_vertical_offset(0);
    }

    /// Move by a signed logical row delta and clamp to the collection extent
    /// (including overscroll).
    pub fn scroll_by(&mut self, rows: i64, viewport_height: u64) -> bool {
        let old = self.scroll.vertical_offset();
        let next = old.saturating_add_signed(rows);
        let maximum = self.max_scroll_top(viewport_height);
        self.scroll.set_vertical_offset(next.min(maximum));
        self.scroll.vertical_offset() != old
    }

    /// Scroll to the final logical collection row and follow subsequent appends.
    pub fn scroll_to_bottom(&mut self, viewport_height: u64) {
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll.set_vertical_offset(maximum);
        self.scroll.set_follow_bottom(true);
    }

    /// Clamp logical scrolling to the current collection extent (including
    /// overscroll) without changing whether subsequent appends are followed.
    pub fn clamp_scroll(&mut self, viewport_height: u64) {
        let maximum = self.max_scroll_top(viewport_height);
        let follow_bottom = self.scroll.follows_bottom();
        self.scroll
            .set_vertical_offset(self.scroll.vertical_offset().min(maximum));
        self.scroll.set_follow_bottom(follow_bottom);
    }

    /// Logical start row for a stable key.
    #[must_use]
    pub fn item_offset(&self, key: &K) -> Option<u64> {
        self.index.key_offset(key)
    }

    /// Measured logical height for a stable key at the synchronized width.
    #[must_use]
    pub fn item_height(&self, key: &K) -> Option<u64> {
        self.index.item_by_key(key).map(|item| item.height)
    }

    /// Stable key containing one logical collection row.
    #[must_use]
    pub fn key_at_offset(&self, offset: u64) -> Option<&K> {
        self.index
            .item_at_offset(offset)
            .and_then(|index| self.index.item(index))
            .map(|item| &item.key)
    }

    /// Retained layout cache diagnostics.
    #[must_use]
    pub const fn layout_cache(&self) -> &LayoutCache {
        &self.layouts
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "fraction is clamped to 0..=1, so the rounded product lies in 0..=maximum"
)]
fn fraction_of(maximum: u64, fraction: f64) -> u64 {
    ((maximum as f64) * fraction).round().min(maximum as f64) as u64
}

/// Borrowed measurement inputs shared by synchronization paths.
struct Measure<'a, 'b, Src: ?Sized> {
    list_id: &'a str,
    source: &'a Src,
    width: u64,
    environment: LayoutEnvironment,
    layouts: &'b mut LayoutCache,
    stats: &'b StatsCell,
}

impl<Src: ?Sized> Measure<'_, '_, Src> {
    /// Exact height of source item `position` at the synchronized width.
    fn height<K>(&mut self, position: usize, key: &K, revision: u64, cx: &mut LayoutCx) -> u64
    where
        K: ToString,
        Src: VirtualItemSource<K>,
    {
        let mut stats = self.stats.get();
        stats.measured_items = stats.measured_items.saturating_add(1);
        let height = if let Some(node) =
            self.source
                .retained_layout(position, self.width, self.environment)
        {
            node.size.height
        } else {
            stats.built_items = stats.built_items.saturating_add(1);
            let element = self.source.build(position);
            self.layouts
                .layout_with_revision_and_environment(
                    item_layout_id(self.list_id, key),
                    element.as_component(),
                    revision,
                    Constraints::for_width(self.width),
                    self.environment,
                    cx,
                )
                .size
                .height
        };
        self.stats.set(stats);
        height
    }
}

impl<K, S> VirtualListState<K, S>
where
    K: Clone + Ord + ToString,
    S: Clone,
{
    /// Reconcile measurements against a source. Full `O(n)` reconciliation
    /// runs only after [`Self::invalidate_all`], on a width or environment
    /// change, for [`VirtualItemSource::always_reconcile`] sources, or when
    /// the source length disagrees with the index plus noted splices
    /// (debug-asserted, then reconciled). Otherwise only noted splices and
    /// invalidated keys are read.
    fn reconcile<Src: VirtualItemSource<K> + ?Sized>(
        &mut self,
        list_id: &str,
        source: &Src,
        width: u64,
        environment: LayoutEnvironment,
        cx: &mut LayoutCx,
    ) {
        // Interpret legacy offset writes against the geometry they were made in.
        self.adopt_scroll();
        self.stats.set(VirtualListRenderStats::default());
        let key = SyncKey { width, environment };
        let mut full = self.pending.full || self.synced != Some(key) || source.always_reconcile();
        if !full {
            let expected = self.expected_source_len();
            debug_assert_eq!(
                expected,
                source.len(),
                "virtual list source length changed without noted splices"
            );
            full = expected != source.len();
        }
        if full || !self.reconcile_incremental(list_id, source, width, environment, cx) {
            self.reconcile_full(list_id, source, width, environment, cx);
        }
        self.pending = PendingChanges::new();
        self.synced = Some(key);
        self.sweep_layouts(list_id, false);
    }

    fn reconcile_full<Src: VirtualItemSource<K> + ?Sized>(
        &mut self,
        list_id: &str,
        source: &Src,
        width: u64,
        environment: LayoutEnvironment,
        cx: &mut LayoutCx,
    ) {
        let entries = (0..source.len())
            .map(|position| (source.key(position), source.layout_revision(position)))
            .collect::<Vec<_>>();
        let capability = environment.capability_revision;
        let unchanged = entries.len() == self.index.len()
            && entries
                .iter()
                .zip(self.index.iter())
                .all(|((key, revision), item)| {
                    item.key == *key && item.is_valid_for(*revision, width, capability)
                });
        if unchanged {
            return;
        }
        let Self {
            index,
            layouts,
            stats,
            ..
        } = self;
        let mut measure = Measure {
            list_id,
            source,
            width,
            environment,
            layouts,
            stats,
        };
        let mut lookup: Option<BTreeMap<K, (usize, u64)>> = None;
        index.sync(entries.iter().cloned(), width, capability, |key| {
            let lookup = lookup.get_or_insert_with(|| {
                entries
                    .iter()
                    .enumerate()
                    .map(|(position, (key, revision))| (key.clone(), (position, *revision)))
                    .collect()
            });
            let (position, revision) = lookup[key];
            measure.height(position, key, revision, cx)
        });
        self.geometry_changed();
        self.sweep_layouts(list_id, true);
    }

    /// Apply noted splices and invalidations. Returns `false` when the source
    /// disagrees with the noted changes and a full reconciliation is needed.
    fn reconcile_incremental<Src: VirtualItemSource<K> + ?Sized>(
        &mut self,
        list_id: &str,
        source: &Src,
        width: u64,
        environment: LayoutEnvironment,
        cx: &mut LayoutCx,
    ) -> bool {
        let capability = environment.capability_revision;
        let regions = std::mem::take(&mut self.pending.regions);
        let invalidated = std::mem::take(&mut self.pending.invalidated);
        let Self {
            index,
            layouts,
            stats,
            removed_layouts,
            generation,
            resolved_valid,
            ..
        } = self;
        let mut measure = Measure {
            list_id,
            source,
            width,
            environment,
            layouts,
            stats,
        };
        // Detach every replaced range (back to front, so earlier index rows
        // keep their positions), then insert each region's source rows front
        // to back: after the removals plus earlier insertions, index row
        // `new_start` is exactly source row `new_start`. Measurements of
        // removed keys reinserted anywhere are reused.
        let mut removed = BTreeMap::new();
        for region in regions.iter().rev() {
            let Ok(items) = index.remove_range(region.old_start..region.old_end) else {
                return false;
            };
            removed.extend(items.into_iter().map(|item| (item.key.clone(), item)));
        }
        let mut reinserted = 0;
        for region in &regions {
            let mut items = Vec::with_capacity(region.new_len());
            for position in region.new_start..region.new_end {
                let key = source.key(position);
                let revision = source.layout_revision(position);
                let height = match removed.get(&key) {
                    Some(item) if item.is_valid_for(revision, width, capability) => item.height,
                    _ => measure.height(position, &key, revision, cx),
                };
                if removed.contains_key(&key) {
                    reinserted += 1;
                }
                items.push(MeasuredListItem::new(
                    key, revision, width, capability, height,
                ));
            }
            if index.insert(region.new_start, items).is_err() {
                return false;
            }
        }
        *removed_layouts = removed_layouts.saturating_add(removed.len().saturating_sub(reinserted));
        let mut changed = !regions.is_empty();
        for key in invalidated {
            let Some(position) = index.index_of(&key) else {
                continue;
            };
            if regions
                .iter()
                .any(|region| (region.new_start..region.new_end).contains(&position))
            {
                continue;
            }
            if source.key(position) != key {
                return false;
            }
            let revision = source.layout_revision(position);
            let valid = index
                .item(position)
                .is_some_and(|item| item.is_valid_for(revision, width, capability));
            if !valid {
                let height = measure.height(position, &key, revision, cx);
                index.update_item(MeasuredListItem::new(
                    key, revision, width, capability, height,
                ));
                changed = true;
            }
        }
        if changed {
            *generation = generation.wrapping_add(1);
            *resolved_valid = false;
        }
        true
    }

    /// Release retained layouts of removed keys once enough accumulate, so
    /// retained memory tracks the current collection with amortized cost.
    fn sweep_layouts(&mut self, list_id: &str, force: bool) {
        if !force && self.removed_layouts <= LAYOUT_SWEEP_MIN.max(self.index.len()) {
            return;
        }
        let active = self
            .index
            .keys()
            .map(|key| item_layout_id(list_id, key))
            .collect::<BTreeSet<_>>();
        self.layouts.retain_ids(&active);
        self.removed_layouts = 0;
    }

    /// Make sure every visible item has an exact retained layout and current
    /// revision. Positions in `measured` were already measured during this
    /// layout and are skipped; newly measured positions are added. Returns
    /// whether any measured height changed.
    fn prepare_visible<Src: VirtualItemSource<K> + ?Sized>(
        &mut self,
        list_id: &str,
        source: &Src,
        range: Range<usize>,
        measured: &mut BTreeSet<usize>,
        cx: &mut LayoutCx,
    ) -> bool {
        let Some(SyncKey { width, environment }) = self.synced else {
            return false;
        };
        let capability = environment.capability_revision;
        let mut changed = false;
        for position in range.start..range.end.min(source.len()) {
            if measured.contains(&position) {
                continue;
            }
            let Some(item) = self.index.item(position) else {
                continue;
            };
            let key = source.key(position);
            if key != item.key {
                // The source disagrees with the index; the caller reconciles.
                self.pending.full = true;
                continue;
            }
            let revision = source.layout_revision(position);
            let retained = source
                .retained_layout(position, width, environment)
                .map(|node| node.size.height);
            let current = item.is_valid_for(revision, width, capability)
                && (retained.is_some()
                    || self
                        .layouts
                        .get_with_environment(
                            &item_layout_id(list_id, &key),
                            revision,
                            Constraints::for_width(width),
                            environment,
                        )
                        .is_some());
            let previous = item.height;
            if current && retained.is_none_or(|height| height == previous) {
                continue;
            }
            let height = Measure {
                list_id,
                source,
                width,
                environment,
                layouts: &mut self.layouts,
                stats: &self.stats,
            }
            .height(position, &key, revision, cx);
            measured.insert(position);
            self.index.update_item(MeasuredListItem::new(
                key, revision, width, capability, height,
            ));
            if height != previous {
                changed = true;
                self.geometry_changed();
            }
        }
        changed
    }

    /// Source position of the item at index row `index` when the source still
    /// holds that same item (same key and layout revision) there, accounting
    /// for splices noted since the last synchronization. Paint and event use
    /// only such items, so a node is never painted or routed with another
    /// item's retained layout.
    fn placed_source_position<Src: VirtualItemSource<K> + ?Sized>(
        &self,
        source: &Src,
        index: usize,
    ) -> Option<usize> {
        let item = self.index.item(index)?;
        let position = self.pending.source_row(index)?;
        (position < source.len()
            && source.key(position) == item.key
            && source.layout_revision(position) == item.layout_revision)
            .then_some(position)
    }
}

/// Erases a borrowed component so eager items can be lent as [`Element`]s.
struct BorrowedComponent<'a>(&'a dyn Component);

impl Component for BorrowedComponent<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.0.event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.0.revision()
    }
}

/// A virtualized list over a lazy [`VirtualItemSource`].
///
/// This is the shared measurement, painting, and event implementation; the
/// eager [`VirtualList`] builder delegates to it.
pub struct VirtualSourceList<'a, Src: ?Sized> {
    id: Cow<'a, str>,
    source: &'a Src,
    pointer_intent: PointerIntent,
}

impl<'a, Src: ?Sized> VirtualSourceList<'a, Src> {
    /// Create a list over `source` with a stable semantic identifier prefix.
    #[must_use]
    pub fn new(id: impl Into<Cow<'a, str>>, source: &'a Src) -> Self {
        Self {
            id: id.into(),
            source,
            pointer_intent: PointerIntent::Auto,
        }
    }

    /// Override pointer presentation without changing input routing.
    #[must_use]
    pub const fn pointer_intent(mut self, intent: PointerIntent) -> Self {
        self.pointer_intent = intent;
        self
    }

    /// Reconcile exact measurements at `width` in the default environment.
    /// See [`Self::sync_with_environment`].
    pub fn sync<K, S>(&self, width: u64, state: &mut VirtualListState<K, S>, cx: &mut LayoutCx)
    where
        K: Clone + Ord + ToString,
        S: Clone,
        Src: VirtualItemSource<K>,
    {
        self.sync_with_environment(width, LayoutEnvironment::default(), state, cx);
    }

    /// Reconcile exact measurements at `width` and `environment`.
    ///
    /// Only noted splices ([`VirtualListState::note_splice`]) and invalidated
    /// keys are read, and only new or changed items are built for measurement.
    /// A full `O(n)` comparison (still measuring only stale items) runs after
    /// [`VirtualListState::invalidate_all`], on a width or environment change,
    /// for [`VirtualItemSource::always_reconcile`] sources, or when the source
    /// length disagrees with the noted changes.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when the source length changed without noted
    /// splices (release builds reconcile fully instead).
    pub fn sync_with_environment<K, S>(
        &self,
        width: u64,
        environment: LayoutEnvironment,
        state: &mut VirtualListState<K, S>,
        cx: &mut LayoutCx,
    ) where
        K: Clone + Ord + ToString,
        S: Clone,
        Src: VirtualItemSource<K>,
    {
        state.reconcile(&self.id, self.source, width, environment, cx);
    }

    /// Synchronize, resolve the position for a viewport, and make sure every
    /// visible item has an exact retained layout.
    ///
    /// Visible items' keys and revisions are re-read, so a visible item whose
    /// revision changed without [`VirtualListState::invalidate`] is still
    /// re-measured. When that changes a height, the position is re-resolved
    /// and the newly visible range prepared again until it is stable; each
    /// item is measured at most once per layout, so this terminates after at
    /// most one pass per newly measured item. A visible key that disagrees
    /// with the index triggers one full reconciliation within the same
    /// layout.
    ///
    /// # Panics
    ///
    /// See [`Self::sync_with_environment`].
    pub fn layout<K, S>(
        &self,
        width: u64,
        environment: LayoutEnvironment,
        viewport_height: u64,
        sources: &dyn SourceMap<K, S>,
        state: &mut VirtualListState<K, S>,
        cx: &mut LayoutCx,
    ) -> VirtualListLayout
    where
        K: Clone + Ord + ToString,
        S: Clone,
        Src: VirtualItemSource<K>,
    {
        self.layout_pinned(
            width,
            environment,
            viewport_height,
            sources,
            StickyHeaders::disabled(),
            state,
            cx,
        )
        .0
    }

    /// [`Self::layout`] that also determines the section headers pinned
    /// under `sticky` and gives each an exact retained layout. Pinned headers
    /// can start far above the viewport, so they are prepared like visible
    /// items, and a height change re-resolves the position.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the public layout signature plus the sticky policy"
    )]
    fn layout_pinned<K, S>(
        &self,
        width: u64,
        environment: LayoutEnvironment,
        viewport_height: u64,
        sources: &dyn SourceMap<K, S>,
        sticky: StickyHeaders,
        state: &mut VirtualListState<K, S>,
        cx: &mut LayoutCx,
    ) -> (VirtualListLayout, Vec<sticky::PinnedHeader>)
    where
        K: Clone + Ord + ToString,
        S: Clone,
        Src: VirtualItemSource<K>,
    {
        self.sync_with_environment(width, environment, state, cx);
        let mut resolved = state.resolve_layout(viewport_height, sources);
        let mut measured = BTreeSet::new();
        let mut reconciled = false;
        let pinned = loop {
            let visible = state
                .index
                .visible_range(resolved.top_offset, viewport_height);
            let mut changed = state.prepare_visible(
                &self.id,
                self.source,
                visible.start..visible.end,
                &mut measured,
                cx,
            );
            let mut pinned = Vec::new();
            if !changed && !state.pending.full {
                pinned = sticky::pinned_headers(
                    &state.index,
                    self.source,
                    resolved.top_offset,
                    viewport_height,
                    sticky,
                );
                for header in &pinned {
                    changed |= state.prepare_visible(
                        &self.id,
                        self.source,
                        header.index..header.index + 1,
                        &mut measured,
                        cx,
                    );
                }
            }
            if state.pending.full && !reconciled {
                // A visible or pinned key disagreed with the index: reconcile
                // now rather than placing a mismatched item for this frame.
                reconciled = true;
                measured.clear();
                self.sync_with_environment(width, environment, state, cx);
            } else if !changed {
                break pinned;
            }
            resolved = state.resolve_layout(viewport_height, sources);
        };
        (
            VirtualListLayout {
                resolved,
                visible: state
                    .index
                    .visible_range(resolved.top_offset, viewport_height),
            },
            pinned,
        )
    }

    /// Paint only visible items at the state's current top offset and
    /// register their visible hit, focus, semantic, and damage regions.
    ///
    /// Only items the source still holds unchanged (same key and layout
    /// revision, after noted splices) and that have a retained layout at
    /// `area.width` are painted; synchronize at that width first.
    pub fn paint<K, S>(
        &self,
        area: Rect,
        state: &VirtualListState<K, S>,
        cx: &mut PaintCx<'_, '_>,
    ) -> VirtualListRenderStats
    where
        K: Clone + Ord + ToString,
        S: Clone,
        Src: VirtualItemSource<K>,
    {
        let offset = state.scroll.vertical_offset();
        let Some(rows) = clip_rows(area, cx) else {
            state.record(|stats| {
                stats.painted_items = 0;
                stats.registered_items = 0;
            });
            return VirtualListRenderStats::default();
        };
        let range = state
            .index
            .visible_range(offset.saturating_add(rows.start), rows.end - rows.start);
        let environment = state.synced.map(|key| key.environment).unwrap_or_default();
        let base = Rect::new(0, 0, area.width, area.height);
        let items = (range.start..range.end).filter_map(|index| {
            let item = self.placed_item(state, index)?;
            let layout = find_item_layout(
                self.source,
                &state.layouts,
                item.position,
                &item.id,
                item.layout_revision,
                area.width.into(),
                environment,
            )?;
            debug_assert_eq!(layout.size.height, item.height);
            Some((ItemSlot::scrolled(item, base, offset, 0), layout))
        });
        let report = self.paint_items(area.width, items, cx);
        state.record(|stats| {
            stats.painted_items = report.painted_items;
            stats.registered_items = report.registered_items;
            stats.built_items = stats.built_items.saturating_add(report.built_items);
        });
        report
    }

    /// The item at index row `index` placed at its measured row, when the
    /// source still holds it unchanged (see
    /// [`VirtualListState::placed_source_position`]).
    fn placed_item<K, S>(&self, state: &VirtualListState<K, S>, index: usize) -> Option<PlacedItem>
    where
        K: Clone + Ord + ToString,
        S: Clone,
        Src: VirtualItemSource<K>,
    {
        let position = state.placed_source_position(self.source, index)?;
        let measured = state.index.item(index)?;
        Some(PlacedItem {
            index,
            position,
            start: state.index.item_offset(index)?,
            height: measured.height,
            layout_revision: measured.layout_revision,
            id: item_layout_id(&self.id, &measured.key),
        })
    }

    /// Paint placed item slots (placed in an area `width` cells wide at the
    /// paint origin), each clipped to its visible rows, registering each
    /// item's visible hit, focus, semantic, and damage regions. Returns
    /// painted, registered, and built counts.
    fn paint_items<'n, K>(
        &self,
        width: u16,
        items: impl IntoIterator<Item = (ItemSlot, &'n LayoutNode)>,
        cx: &mut PaintCx<'_, '_>,
    ) -> VirtualListRenderStats
    where
        Src: VirtualItemSource<K>,
    {
        let mut report = VirtualListRenderStats::default();
        for (
            ItemSlot {
                item,
                local_y,
                visible,
            },
            layout,
        ) in items
        {
            let element = self.source.build(item.position);
            report.built_items = report.built_items.saturating_add(1);
            cx.with_child(
                0,
                local_y,
                LocalRect::new(
                    0,
                    i64::from(visible.y).saturating_sub(local_y),
                    width,
                    visible.height,
                ),
                |cx| cx.with_child_size(0, 0, layout.size, |cx| element.paint(layout, cx)),
            );
            let semantic_id = item.id.as_str().to_owned();
            cx.push_hit(
                HitRegion::new(semantic_id.clone(), visible)
                    .role(HitRole::ListItem)
                    .pointer_intent(self.pointer_intent),
            );
            let visible_local = LocalRect::new(
                i32::from(visible.x),
                i64::from(visible.y),
                visible.width,
                visible.height,
            );
            cx.push_focus(semantic_id.clone(), visible_local);
            cx.push_semantic(SemanticRegion::new(semantic_id, visible, "list-item"));
            cx.push_damage(visible_local);
            report.painted_items = report.painted_items.saturating_add(1);
            report.registered_items = report.registered_items.saturating_add(1);
        }
        report
    }

    /// Route an event only through viewport-intersecting items, using the same
    /// retained layout, translation, clipping, and topmost-first order as paint.
    ///
    /// Mouse events are offered only to the visible item under the pointer.
    /// Non-positional events traverse visible items from bottom to top until
    /// handled. Items changed or replaced since the last synchronization
    /// (including through splices noted since) are skipped, so an event never
    /// reaches a different item than the one laid out at its position.
    pub fn event<K, S>(
        &self,
        area: Rect,
        state: &VirtualListState<K, S>,
        event: &Event,
        cx: &mut EventCx<'_>,
    ) -> EventOutcome
    where
        K: Clone + Ord + ToString,
        S: Clone,
        Src: VirtualItemSource<K>,
    {
        let offset = state.scroll.vertical_offset();
        let range = state.index.visible_range(offset, u64::from(area.height));
        let environment = state.synced.map(|key| key.environment).unwrap_or_default();
        let items = (range.start..range.end)
            .filter_map(|index| {
                let item = self.placed_item(state, index)?;
                Some(ItemSlot::scrolled(item, area, offset, 0))
            })
            .collect::<Vec<_>>();
        let width = u64::from(area.width);
        let routed = self.route_items(area, items, event, cx, |item| {
            find_item_layout(
                self.source,
                &state.layouts,
                item.position,
                &item.id,
                item.layout_revision,
                width,
                environment,
            )
            .map(Cow::Borrowed)
        });
        state.record(|stats| {
            stats.built_items = stats.built_items.saturating_add(routed.built);
        });
        routed.outcome
    }

    /// Route an event through placed item slots of `area` (last slot first).
    /// `layout_of` supplies an item's retained layout, and is called only for
    /// items the event is actually offered to.
    fn route_items<'n, K>(
        &self,
        area: Rect,
        items: impl IntoIterator<Item = ItemSlot, IntoIter: DoubleEndedIterator>,
        event: &Event,
        cx: &mut EventCx<'_>,
        mut layout_of: impl FnMut(&PlacedItem) -> Option<Cow<'n, LayoutNode>>,
    ) -> RoutedEvent
    where
        Src: VirtualItemSource<K>,
    {
        let mut routed = RoutedEvent {
            outcome: EventOutcome::Ignored,
            target: None,
            built: 0,
        };
        let viewport = cx.visible_rect(logical_rect(area));
        if viewport.is_empty() {
            return routed;
        }
        let pointer = match event {
            Event::Mouse(mouse) if !viewport.contains(mouse.position) => return routed,
            Event::Mouse(mouse) => Some(mouse.position),
            _ => None,
        };
        for ItemSlot {
            item,
            local_y,
            visible,
        } in items.into_iter().rev()
        {
            let item_area = cx.visible_rect(logical_rect(visible));
            if item_area.is_empty() || pointer.is_some_and(|point| !item_area.contains(point)) {
                continue;
            }
            let Some(layout) = layout_of(&item) else {
                continue;
            };
            let layout = layout.as_ref();
            let element = self.source.build(item.position);
            routed.built = routed.built.saturating_add(1);
            let outcome = cx.with_transform(
                0,
                item.start,
                i32::from(area.x),
                i64::from(area.y).saturating_add(local_y),
                item_area,
                |cx| cx.with_root(layout, |cx| element.event(event, layout, cx)),
            );
            if outcome.is_handled() || pointer.is_some() {
                routed.outcome = outcome;
                routed.target = pointer.map(|_| item.index);
                return routed;
            }
        }
        routed
    }
}

/// A placed item with its viewport placement and the rows it may use.
struct ItemSlot {
    item: PlacedItem,
    /// Viewport row of the item's first row (negative above the viewport).
    local_y: i64,
    /// Visible part of the item, in the coordinates of the area it is placed
    /// in. Painting, interaction regions, and routing use exactly this.
    visible: Rect,
}

impl ItemSlot {
    /// An item scrolled with the viewport's first row (the top of `area`) at
    /// collection row `top`, restricted to rows of `area` at or below its
    /// first `occluded` rows.
    fn scrolled(item: PlacedItem, area: Rect, top: u64, occluded: u16) -> Self {
        let local_y = signed_row(item.start).saturating_sub(signed_row(top));
        let visible = translated_item_area(area, local_y, item.height);
        let first = visible.y.max(area.y.saturating_add(occluded));
        let bottom = visible.bottom().max(first);
        Self {
            item,
            local_y,
            visible: Rect::new(visible.x, first, visible.width, bottom - first),
        }
    }

    /// A header pinned in `area` at its sticky placement.
    fn pinned(item: PlacedItem, area: Rect, header: &sticky::PinnedHeader) -> Self {
        let row = |row: u64| {
            u16::try_from(row)
                .unwrap_or(u16::MAX)
                .min(area.height)
                .saturating_add(area.y)
        };
        let (first, bottom) = (row(header.visible.start), row(header.visible.end));
        Self {
            item,
            local_y: header.local_y,
            visible: Rect::new(area.x, first, area.width, bottom.saturating_sub(first)),
        }
    }
}

/// One visible item placed for painting or event routing.
struct PlacedItem {
    /// Index row (as of the last synchronization).
    index: usize,
    /// Current source position of the same item.
    position: usize,
    /// Collection row of the item's first row.
    start: u64,
    /// Exact measured height.
    height: u64,
    /// Layout revision the retained layout was measured at.
    layout_revision: u64,
    /// Stable item identity, also its semantic and hit identifier.
    id: LayoutId,
}

/// Result of routing one event through placed items.
struct RoutedEvent {
    outcome: EventOutcome,
    /// Index row of the item under the pointer, for mouse events that
    /// reached an item.
    target: Option<usize>,
    /// Items constructed to route the event.
    built: usize,
}

/// Viewport rows of `area` visible through the paint clip.
fn clip_rows(area: Rect, cx: &PaintCx<'_, '_>) -> Option<Range<u64>> {
    let clip = cx.area();
    let first = u64::try_from(clip.y.max(0))
        .unwrap_or(u64::MAX)
        .min(u64::from(area.height));
    let end = u64::try_from(clip.y.saturating_add(i64::from(clip.height)).max(0))
        .unwrap_or(u64::MAX)
        .min(u64::from(area.height));
    let hidden = end <= first
        || clip.width == 0
        || clip.x >= i32::from(area.width)
        || clip.x.saturating_add(i32::from(clip.width)) <= 0;
    (!hidden).then_some(first..end)
}

fn signed_row(row: u64) -> i64 {
    i64::try_from(row).unwrap_or(i64::MAX)
}

fn logical_rect(area: Rect) -> LogicalRect {
    LogicalRect::new(
        area.x.into(),
        u64::from(area.y),
        u64::from(area.width),
        u64::from(area.height),
    )
}

fn find_item_layout<'s, K, Src: VirtualItemSource<K> + ?Sized>(
    source: &'s Src,
    layouts: &'s LayoutCache,
    position: usize,
    layout_id: &LayoutId,
    layout_revision: u64,
    width: u64,
    environment: LayoutEnvironment,
) -> Option<&'s LayoutNode> {
    source
        .retained_layout(position, width, environment)
        .or_else(|| {
            layouts.get_with_environment(
                layout_id,
                layout_revision,
                Constraints::for_width(width),
                environment,
            )
        })
}

/// A keyed item supplied to [`VirtualList`].
pub struct VirtualListItem<'a, K> {
    key: K,
    layout_revision: u64,
    component: Element<'a>,
    header_level: Option<u16>,
    section_parent: Option<usize>,
}

impl<'a, K> VirtualListItem<'a, K> {
    /// Create one keyed item component.
    #[must_use]
    pub fn new(key: K, layout_revision: u64, component: impl Component + 'a) -> Self {
        Self {
            key,
            layout_revision,
            component: Element::new(component),
            header_level: None,
            section_parent: None,
        }
    }
}

/// A variable-height collection that measures by stable key and paints only
/// viewport-intersecting items.
///
/// This eager builder holds every item component and implements
/// [`VirtualItemSource`]; each synchronization compares every key and
/// revision. Prefer a lazy [`VirtualItemSource`] with [`VirtualSourceList`]
/// for large collections.
pub struct VirtualList<'a, K> {
    pointer_intent: PointerIntent,
    id: String,
    items: Vec<VirtualListItem<'a, K>>,
    /// Headers whose sections are still open, outermost first, as
    /// `(position, level)`.
    open_sections: Vec<(usize, u16)>,
}

impl<K: Clone> VirtualItemSource<K> for VirtualList<'_, K> {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn key(&self, index: usize) -> K {
        self.items[index].key.clone()
    }

    fn layout_revision(&self, index: usize) -> u64 {
        self.items[index].layout_revision
    }

    fn paint_revision(&self, index: usize) -> u64 {
        self.items[index].component.revision().paint
    }

    fn build(&self, index: usize) -> Element<'_> {
        Element::new(BorrowedComponent(
            self.items[index].component.as_component(),
        ))
    }

    fn always_reconcile(&self) -> bool {
        true
    }

    fn section_header_level(&self, index: usize) -> Option<u16> {
        self.items[index].header_level
    }

    fn section_parent(&self, index: usize) -> Option<usize> {
        self.items[index].section_parent
    }
}

impl<'a, K> VirtualList<'a, K>
where
    K: Clone + Ord + ToString,
{
    /// Override pointer presentation without changing input routing or mouse policy.
    #[must_use]
    pub const fn pointer_intent(mut self, intent: PointerIntent) -> Self {
        self.pointer_intent = intent;
        self
    }

    /// Create an empty collection with a stable semantic identifier prefix.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            pointer_intent: PointerIntent::Auto,
            items: Vec::new(),
            open_sections: Vec::new(),
        }
    }

    /// Append one keyed item using the component's declared layout revision.
    #[must_use]
    pub fn component(self, key: K, component: impl Component + 'a) -> Self {
        let revision = component.revision();
        self.push(VirtualListItem::new(key, revision.layout, component))
    }

    /// Append one keyed item with an externally managed layout revision.
    #[must_use]
    pub fn item(self, key: K, layout_revision: u64, component: impl Component + 'a) -> Self {
        self.push(VirtualListItem::new(key, layout_revision, component))
    }

    /// Append one keyed sticky section header at nesting `level` with an
    /// externally managed layout revision.
    ///
    /// The header's section holds every following item up to the next header
    /// whose level is at most `level`; lower levels enclose higher ones. A
    /// `VirtualListComponent` over this list pins the header at the viewport
    /// top while any item of its section is visible (see
    /// [`VirtualItemSource::section_header_level`]).
    #[must_use]
    pub fn section_header(
        mut self,
        key: K,
        level: u16,
        layout_revision: u64,
        component: impl Component + 'a,
    ) -> Self {
        let mut item = VirtualListItem::new(key, layout_revision, component);
        item.header_level = Some(level);
        self.open_sections
            .retain(|&(_, open_level)| open_level < level);
        self = self.push(item);
        let position = self.items.len() - 1;
        self.open_sections.push((position, level));
        self
    }

    /// Append one keyed sticky section header at nesting `level` using the
    /// component's declared layout revision. See [`Self::section_header`].
    #[must_use]
    pub fn component_section_header(
        self,
        key: K,
        level: u16,
        component: impl Component + 'a,
    ) -> Self {
        let revision = component.revision();
        self.section_header(key, level, revision.layout, component)
    }

    fn push(mut self, mut item: VirtualListItem<'a, K>) -> Self {
        item.section_parent = self.open_sections.last().map(|&(position, _)| position);
        self.items.push(item);
        self
    }

    fn as_source(&self) -> VirtualSourceList<'_, Self> {
        VirtualSourceList::new(self.id.as_str(), self).pointer_intent(self.pointer_intent)
    }

    /// Synchronize exact current-width measurements, retaining unchanged keyed
    /// layouts across reorder. This checks the collection's keys and revisions;
    /// reuse retained state for pure scrolling without calling this again. Sync
    /// before painting after item, layout revision, width, or environment changes.
    ///
    /// # Panics
    ///
    /// Panics if stable item keys or their string representations are duplicated.
    pub fn sync<S: Clone>(
        &self,
        width: u64,
        state: &mut VirtualListState<K, S>,
        cx: &mut LayoutCx,
    ) {
        self.sync_with_environment(width, LayoutEnvironment::default(), state, cx);
    }

    /// Synchronize exact measurements with geometry-affecting terminal
    /// capability inputs included in both retained cache layers.
    ///
    /// # Panics
    ///
    /// Panics if stable item keys or their string representations are duplicated.
    pub fn sync_with_environment<S: Clone>(
        &self,
        width: u64,
        environment: LayoutEnvironment,
        state: &mut VirtualListState<K, S>,
        cx: &mut LayoutCx,
    ) {
        self.assert_unique_ids();
        self.as_source()
            .sync_with_environment(width, environment, state, cx);
    }

    fn assert_unique_ids(&self) {
        let active = self
            .items
            .iter()
            .map(|item| item_layout_id(&self.id, &item.key))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            active.len(),
            self.items.len(),
            "virtual list keys must have unique string representations"
        );
    }

    /// Synchronize, resolve the keyed position for a viewport, and make sure
    /// visible items have retained layouts. See [`VirtualSourceList::layout`].
    ///
    /// # Panics
    ///
    /// Panics if stable item keys or their string representations are duplicated.
    pub fn layout<S: Clone>(
        &self,
        width: u64,
        environment: LayoutEnvironment,
        viewport_height: u64,
        sources: &dyn SourceMap<K, S>,
        state: &mut VirtualListState<K, S>,
        cx: &mut LayoutCx,
    ) -> VirtualListLayout {
        self.assert_unique_ids();
        self.as_source()
            .layout(width, environment, viewport_height, sources, state, cx)
    }

    /// Paint only visible items and register full visible item hit rectangles.
    ///
    /// Synchronize the list at `area.width` first; items without a retained
    /// layout at that width are skipped.
    pub fn paint<S: Clone>(
        &self,
        area: Rect,
        state: &VirtualListState<K, S>,
        cx: &mut PaintCx<'_, '_>,
    ) -> VirtualListRenderStats {
        self.as_source().paint(area, state, cx)
    }

    /// Route an event only through viewport-intersecting items, using the same
    /// retained layout, translation, clipping, and topmost-first order as paint.
    ///
    /// Mouse events are offered only to the visible item under the pointer.
    /// Non-positional events traverse visible items from bottom to top until
    /// handled. Items without a retained layout at `area.width` are skipped.
    pub fn event<S: Clone>(
        &self,
        area: Rect,
        state: &VirtualListState<K, S>,
        event: &Event,
        cx: &mut EventCx<'_>,
    ) -> EventOutcome {
        self.as_source().event(area, state, event, cx)
    }

    /// Scroll so the keyed item is visible with the minimum movement.
    ///
    /// Returns whether the key exists, even if its viewport offset is unchanged.
    pub fn ensure_item_visible<S: Clone>(
        &self,
        state: &mut VirtualListState<K, S>,
        key: &K,
        viewport_height: u64,
    ) -> bool {
        if state.index.index_of(key).is_none() {
            return false;
        }
        state.ensure_key_visible(key, viewport_height);
        true
    }

    /// Logical start row for a stable key.
    #[must_use]
    pub fn item_offset<S: Clone>(&self, state: &VirtualListState<K, S>, key: &K) -> Option<u64> {
        state.item_offset(key)
    }
}

fn item_layout_id<K: ToString>(list_id: &str, key: &K) -> LayoutId {
    LayoutId::new(format!("{list_id}.item.{}", key.to_string()))
}

fn translated_item_area(area: Rect, local_y: i64, item_height: u64) -> Rect {
    let top = i64::from(area.y).saturating_add(local_y);
    let bottom = top.saturating_add(i64::try_from(item_height).unwrap_or(i64::MAX));
    let visible_top = top.clamp(i64::from(area.y), i64::from(area.bottom()));
    let visible_bottom = bottom.clamp(visible_top, i64::from(area.bottom()));
    Rect::new(
        area.x,
        u16::try_from(visible_top).unwrap_or(area.y),
        area.width,
        u16::try_from(visible_bottom.saturating_sub(visible_top)).unwrap_or(u16::MAX),
    )
}

#[cfg(test)]
mod state_tests;

#[cfg(test)]
mod tests {
    use super::{VirtualList, VirtualListState};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{
        Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutEnvironment, LayoutId,
        LayoutNode, LogicalSize,
    };
    use bmux_tui::composition::TextBlock;
    use bmux_tui::damage::{Damage, DamagePolicy};
    use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::HitId;
    use bmux_tui::image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement,
    };
    use bmux_tui::interaction::InteractionRouter;
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::selection::{SelectionFragment, SelectionScope};
    use bmux_tui::style::Style;
    use bmux_tui::text::{Line, Text};

    #[test]
    fn retained_card_scrolling_matches_full_render_without_resync() {
        use bmux_tui::composition::{Column, Surface};
        use bmux_tui::geometry::Insets;
        use bmux_tui::style::Color;

        let list = (0..8).fold(VirtualList::new("cards"), |list, index| {
            let style = Style::new().bg(Color::Blue);
            list.component(
                index,
                Surface::new(
                    Column::new()
                        .child(TextBlock::new(format!("Author {index}")))
                        .child(TextBlock::new("界 wrapped body ".repeat(index % 3 + 1))),
                )
                .padding(Insets::all(1))
                .background(style)
                .content_style(style),
            )
        });
        let mut state = VirtualListState::new(1);
        list.sync(12, &mut state, &mut LayoutCx::new());
        let height = u16::try_from(state.total_height()).unwrap();
        let mut full = Buffer::empty(Rect::new(0, 0, 12, height));
        list.paint(
            Rect::new(0, 0, 12, height),
            &state,
            &mut PaintCx::new(&mut Frame::new(&mut full)),
        );
        let viewport = Rect::new(0, 0, 12, 4);
        loop {
            let mut clipped = Buffer::empty(viewport);
            list.paint(
                viewport,
                &state,
                &mut PaintCx::new(&mut Frame::new(&mut clipped)),
            );
            let offset = u16::try_from(state.scroll.vertical_offset()).unwrap();
            for y in 0..viewport.height {
                for x in 0..viewport.width {
                    assert_eq!(
                        clipped.get(Point::new(x, y)),
                        full.get(Point::new(x, y + offset)),
                        "cell ({x}, {y}) at offset {offset}"
                    );
                }
            }
            let mut nested = Buffer::empty(Rect::new(0, 0, 16, 8));
            let mut frame = Frame::new(&mut nested);
            PaintCx::new(&mut frame).with_child(2, 2, LocalRect::new(0, 1, 12, 2), |cx| {
                list.paint(viewport, &state, cx);
            });
            let parent_clip = Rect::new(2, 3, 12, 2);
            let expected_areas = clipped_card_areas(&state, offset);
            assert_eq!(
                frame
                    .semantics()
                    .regions()
                    .iter()
                    .map(|region| region.area)
                    .collect::<Vec<_>>(),
                expected_areas,
                "semantic coverage at offset {offset}"
            );
            for region in frame.hits().regions() {
                assert!(!region.area.is_empty());
                assert_eq!(region.area.intersection(parent_clip), region.area);
            }
            assert_eq!(
                frame
                    .hits()
                    .regions()
                    .iter()
                    .filter(|region| region.focusable)
                    .map(|region| region.area)
                    .collect::<Vec<_>>(),
                expected_areas,
                "focus coverage at offset {offset}"
            );
            let untouched = Buffer::empty(Rect::new(0, 0, 16, 8));
            for y in 0..8 {
                for x in 0..16 {
                    let expected = if (2..14).contains(&x) && (3..5).contains(&y) {
                        full.get(Point::new(x - 2, y - 2 + offset))
                    } else {
                        untouched.get(Point::new(x, y))
                    };
                    assert_eq!(
                        nested.get(Point::new(x, y)),
                        expected,
                        "nested cell ({x}, {y}) at offset {offset}"
                    );
                }
            }
            if !state.scroll_by(1, u64::from(viewport.height)) {
                break;
            }
        }
    }

    fn clipped_card_areas(state: &VirtualListState<usize>, offset: u16) -> Vec<Rect> {
        (0..8)
            .filter_map(|index| {
                let start = state.item_offset(&index).unwrap();
                let height = state.index.item(index).unwrap().height;
                let visible_start = start.max(u64::from(offset) + 1);
                let visible_end = (start + height).min(u64::from(offset) + 3);
                (visible_start < visible_end).then(|| {
                    Rect::new(
                        2,
                        u16::try_from(visible_start - u64::from(offset) + 2).unwrap(),
                        12,
                        u16::try_from(visible_end - visible_start).unwrap(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn rejects_colliding_key_strings_before_changing_retained_state() {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
        struct Key(u8);

        impl std::fmt::Display for Key {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("same")
            }
        }

        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();
        VirtualList::new("keys")
            .item(Key(0), 0, TextBlock::new("original"))
            .sync(8, &mut state, &mut cx);
        let measurements = cx.measured_nodes();
        let height = state.total_height();
        for second in [0, 1] {
            let invalid = VirtualList::new("keys")
                .item(Key(0), 1, TextBlock::new("changed text wraps"))
                .item(Key(second), 0, TextBlock::new("collision"));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                invalid.sync(8, &mut state, &mut cx);
            }));
            assert!(result.is_err());
            assert_eq!(state.total_height(), height);
            assert_eq!(cx.measured_nodes(), measurements);
            assert_eq!(state.item_offset(&Key(0)), Some(0));
            assert_eq!(state.item_offset(&Key(1)), None);
        }
    }

    struct ExternallyRevisedItem;

    impl Component for ExternallyRevisedItem {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new("external-item"),
                constraints.constrain(LogicalSize::new(constraints.max_width(), 1)),
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

        fn revision(&self) -> ComponentRevision {
            ComponentRevision::default()
        }
    }

    struct RevisedHeightItem {
        revision: u64,
        height: u64,
    }

    impl Component for RevisedHeightItem {
        fn revision(&self) -> ComponentRevision {
            ComponentRevision::new(self.revision, 0)
        }

        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new("revised-height"),
                constraints.constrain(LogicalSize::new(constraints.max_width(), self.height)),
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}
    }

    struct MetadataItem {
        id: &'static str,
        height: u64,
        cursor_row: Option<u16>,
    }

    impl Component for MetadataItem {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new(self.id),
                constraints.constrain(LogicalSize::new(constraints.max_width(), self.height)),
            )
        }

        fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
            let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
            cx.fill(
                LocalRect::new(
                    0,
                    0,
                    layout.size.width.try_into().unwrap_or(u16::MAX),
                    height,
                ),
                self.id,
                Style::new(),
            );
            cx.push_selection_scope(SelectionScope::new(
                format!("scope:{}", self.id),
                Rect::new(
                    0,
                    0,
                    layout.size.width.try_into().unwrap_or(u16::MAX),
                    height,
                ),
            ));
            cx.push_selection_fragment(SelectionFragment::new(
                format!("scope:{}", self.id),
                format!("content:{}", self.id),
                Rect::new(0, 1, 1, 1),
                0,
                0..1,
            ));
            cx.push_image(ImageContribution::Present(ImagePlacement {
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
                key: ImageKey::new(format!("image:{}", self.id)),
                payload: ImagePayload::Png {
                    bytes: vec![1],
                    width: 1,
                    height: 1,
                },
                destination: Rect::new(1, 0, 2, height),
                clip: Rect::new(
                    0,
                    0,
                    layout.size.width.try_into().unwrap_or(u16::MAX),
                    height,
                ),
                lifecycle: ImageLifecycle::Frame,
            }));
            if let Some(row) = self.cursor_row {
                cx.set_cursor(Point::new(2, row), true);
            }
            cx.push_damage(LocalRect::new(
                0,
                0,
                layout.size.width.try_into().unwrap_or(u16::MAX),
                height,
            ));
        }

        fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
            if matches!(event, Event::Mouse(_))
                && cx
                    .find_visible_rect(&layout.id)
                    .is_some_and(|area| !area.is_empty())
            {
                EventOutcome::Handled
            } else {
                EventOutcome::Ignored
            }
        }
    }

    #[test]
    fn tall_item_metadata_clips_before_terminal_conversion() {
        let list = VirtualList::new("tall").item(
            "item",
            0,
            MetadataItem {
                id: "tall-child",
                height: 100_000,
                cursor_row: None,
            },
        );
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(90_000);
        let viewport = Rect::new(4, 5, 8, 4);
        let area = Rect::new(0, 0, 8, 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 12));
        let mut frame = Frame::new(&mut buffer);
        let mut report = super::VirtualListRenderStats::default();
        PaintCx::new(&mut frame).with_child(
            i32::from(viewport.x),
            i64::from(viewport.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| report = list.paint(area, &state, cx),
        );
        assert_eq!(report.registered_items, 1);
        assert_eq!(frame.hits().regions().len(), 2);
        for region in frame.hits().regions() {
            assert_eq!(region.area, viewport);
        }
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].area, viewport);
        assert_eq!(frame.semantics().regions()[0].id, "tall.item.item");
        let root = LayoutNode::leaf(LayoutId::new("root"), LogicalSize::new(20, 12));
        let mut event_cx = EventCx::with_clip(&root, viewport);
        for (point, expected) in [
            (Point::new(4, 5), EventOutcome::Handled),
            (Point::new(11, 8), EventOutcome::Handled),
            (Point::new(3, 5), EventOutcome::Ignored),
            (Point::new(4, 9), EventOutcome::Ignored),
        ] {
            let event = Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                point,
            ));
            assert_eq!(
                list.event(viewport, &state, &event, &mut event_cx),
                expected
            );
        }
        assert_eq!(super::translated_item_area(area, -90_000, 100_000), area);
        assert_eq!(
            super::translated_item_area(area, -100_000, 100_000).height,
            0
        );
        assert_eq!(super::translated_item_area(area, 10, 100_000).height, 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn virtualized_metadata_uses_exact_boundary_clipping() {
        let list = VirtualList::new("messages")
            .item(
                "a",
                0,
                MetadataItem {
                    id: "a",
                    height: 3,
                    cursor_row: Some(0),
                },
            )
            .item(
                "b",
                0,
                MetadataItem {
                    id: "b",
                    height: 3,
                    cursor_row: Some(1),
                },
            )
            .item(
                "c",
                0,
                MetadataItem {
                    id: "c",
                    height: 3,
                    cursor_row: None,
                },
            );
        let mut state = VirtualListState::new(0);
        list.sync(6, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 4));
        let mut frame = Frame::new(&mut buffer);
        let report = list.paint(Rect::new(0, 0, 6, 4), &state, &mut PaintCx::new(&mut frame));

        assert_eq!(report.painted_items, 2);
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("a")
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 1))
                .map(|cell| cell.symbol.as_str()),
            Some("b")
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 3))
                .map(|cell| cell.symbol.as_str()),
            Some("b")
        );
        assert_eq!(report.registered_items, 2);
        let pointer_hits = frame
            .hits()
            .regions()
            .iter()
            .filter(|region| region.pointer_events)
            .collect::<Vec<_>>();
        assert_eq!(pointer_hits.len(), 2);
        assert_eq!(pointer_hits[0].area, Rect::new(0, 0, 6, 1));
        assert_eq!(pointer_hits[1].area, Rect::new(0, 1, 6, 3));
        assert_eq!(frame.semantics().regions().len(), 2);
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 6, 1));
        assert_eq!(frame.semantics().regions()[1].area, Rect::new(0, 1, 6, 3));
        assert_eq!(frame.selection().scopes().len(), 2);
        assert_eq!(frame.selection().scopes()[0].area, Rect::new(0, 0, 6, 1));
        assert_eq!(frame.selection().scopes()[1].area, Rect::new(0, 1, 6, 3));
        assert_eq!(frame.selection().fragments().len(), 1);
        assert_eq!(frame.selection().fragments()[0].area, Rect::new(0, 2, 1, 1));
        assert_eq!(frame.images().len(), 2);
        let ImageContribution::Present(first_image) = &frame.images()[0] else {
            panic!("expected image placement");
        };
        assert_eq!(first_image.key, ImageKey::new("image:a"));
        assert_eq!(first_image.destination, Rect::new(1, 0, 2, 1));
        assert_eq!(first_image.clip, Rect::new(0, 0, 6, 1));
        assert_eq!(first_image.lifecycle, ImageLifecycle::Frame);
        let ImageContribution::Present(second_image) = &frame.images()[1] else {
            panic!("expected image placement");
        };
        assert_eq!(second_image.key, ImageKey::new("image:b"));
        assert_eq!(second_image.destination, Rect::new(1, 1, 2, 3));
        assert_eq!(second_image.clip, Rect::new(0, 1, 6, 3));
        assert_eq!(second_image.lifecycle, ImageLifecycle::Frame);
        assert_eq!(
            frame.cursor(),
            Some(bmux_tui::frame::Cursor::visible(Point::new(2, 2)))
        );
        assert_eq!(
            frame.damage(bmux_tui::damage::DamagePolicy {
                max_regions: 64,
                max_area_percent: 101,
            }),
            bmux_tui::damage::Damage::Regions(vec![Rect::new(0, 0, 6, 4)])
        );

        let root = LayoutNode::leaf(LayoutId::new("root"), LogicalSize::new(6, 4));
        let mut event_cx = EventCx::with_clip(&root, Rect::new(0, 0, 6, 4));
        let clipped_item = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(2, 0),
        ));
        assert_eq!(
            list.event(Rect::new(0, 0, 6, 4), &state, &clipped_item, &mut event_cx),
            EventOutcome::Handled
        );
        let visible_item = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(2, 2),
        ));
        assert_eq!(
            list.event(Rect::new(0, 0, 6, 4), &state, &visible_item, &mut event_cx),
            EventOutcome::Handled
        );
    }

    struct EventItem {
        id: &'static str,
        height: u64,
        outcome: EventOutcome,
    }

    impl Component for EventItem {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new(self.id),
                constraints.constrain(LogicalSize::new(constraints.max_width(), self.height)),
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

        fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
            if matches!(event, Event::Mouse(_))
                && cx
                    .find_visible_rect(&layout.id)
                    .is_some_and(|area| !area.is_empty())
            {
                self.outcome
            } else {
                EventOutcome::Ignored
            }
        }
    }

    #[test]
    fn translated_pointer_routing_respects_parent_clip_and_item_gaps() {
        let list = VirtualList::new("events")
            .component(
                0,
                EventItem {
                    id: "first",
                    height: 3,
                    outcome: EventOutcome::Handled,
                },
            )
            .component(
                1,
                EventItem {
                    id: "second",
                    height: 2,
                    outcome: EventOutcome::Redraw,
                },
            );
        let mut state = VirtualListState::new(1);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(1);
        let area = Rect::new(0, 0, 8, 5);
        let root = LayoutNode::leaf("root".into(), LogicalSize::new(8, 5));
        let clip = Rect::new(4, 7, 8, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 12));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(4, 6, LocalRect::new(0, 1, 8, 3), |cx| {
            list.paint(area, &state, cx);
        });
        assert_eq!(
            frame
                .semantics()
                .regions()
                .iter()
                .map(|region| region.area)
                .collect::<Vec<_>>(),
            vec![Rect::new(4, 7, 8, 1), Rect::new(4, 9, 8, 1)]
        );
        EventCx::new(&root).with_transform(0, 0, 4, 6, clip, |cx| {
            for (point, expected) in [
                (Point::new(4, 6), EventOutcome::Ignored),
                (Point::new(4, 7), EventOutcome::Handled),
                (Point::new(4, 8), EventOutcome::Ignored),
                (Point::new(4, 9), EventOutcome::Redraw),
                (Point::new(4, 10), EventOutcome::Ignored),
                (Point::new(3, 7), EventOutcome::Ignored),
                (Point::new(12, 9), EventOutcome::Ignored),
            ] {
                let event = Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    point,
                ));
                assert_eq!(
                    list.event(area, &state, &event, cx),
                    expected,
                    "pointer {point:?}"
                );
            }
        });
    }

    #[test]
    fn nonpositional_events_skip_parent_clipped_items() {
        struct Handler(EventOutcome);
        impl Component for Handler {
            fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf(
                    "handler".into(),
                    constraints.constrain(LogicalSize::new(8, 2)),
                )
            }
            fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}
            fn event(
                &self,
                _event: &Event,
                _layout: &LayoutNode,
                _cx: &mut EventCx<'_>,
            ) -> EventOutcome {
                self.0
            }
        }
        let list = VirtualList::new("list")
            .item("visible", 0, Handler(EventOutcome::Handled))
            .item("hidden", 0, Handler(EventOutcome::Redraw));
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        let root = LayoutNode::leaf("root".into(), LogicalSize::new(8, 4));
        let area = Rect::new(0, 0, 8, 4);
        let event = Event::Paste("input".into());
        let mut cx = EventCx::with_clip(&root, Rect::new(0, 0, 8, 2));
        assert_eq!(
            list.event(area, &state, &event, &mut cx),
            EventOutcome::Handled
        );
        let mut cx = EventCx::with_clip(&root, Rect::new(10, 10, 2, 2));
        assert_eq!(
            list.event(area, &state, &event, &mut cx),
            EventOutcome::Ignored
        );
        let mut cx = EventCx::with_clip(&root, area);
        assert_eq!(
            list.event(area, &state, &event, &mut cx),
            EventOutcome::Redraw
        );
    }

    #[test]
    fn intrinsic_narrow_item_uses_full_width_list_constraints() {
        struct NarrowItem;
        impl Component for NarrowItem {
            fn layout(&self, constraints: Constraints, _: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf(
                    "narrow".into(),
                    constraints.constrain(LogicalSize::new(3, 2)),
                )
            }

            fn paint(&self, _: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
                cx.fill(cx.area(), "x", Style::new());
            }

            fn event(&self, _: &Event, _: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
                assert_eq!(cx.clip(), Some(Rect::new(12, 11, 8, 2)));
                EventOutcome::Handled
            }
        }

        let list = VirtualList::new("list").item("narrow", 0, NarrowItem);
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        let root = LayoutNode::leaf("root".into(), LogicalSize::new(30, 30));
        let area = Rect::new(2, 1, 8, 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 30));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(12, 11, LocalRect::new(0, 0, 8, 4), |cx| {
            list.paint(Rect::new(0, 0, 8, 4), &state, cx);
        });
        for y in 10..14 {
            for x in 11..21 {
                let expected = if (12..20).contains(&x) && (11..13).contains(&y) {
                    "x"
                } else {
                    " "
                };
                assert_eq!(buffer.get(Point::new(x, y)).unwrap().symbol, expected);
            }
        }
        let mut cx = EventCx::new(&root);
        cx.with_transform(0, 0, 10, 10, Rect::new(0, 0, 30, 30), |cx| {
            for (x, expected) in [
                (12, EventOutcome::Handled),
                (14, EventOutcome::Handled),
                (15, EventOutcome::Handled),
                (19, EventOutcome::Handled),
                (20, EventOutcome::Ignored),
            ] {
                let event = Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(x, 11),
                ));
                assert_eq!(list.event(area, &state, &event, cx), expected);
            }
            assert_eq!(
                list.event(area, &state, &Event::Paste("input".into()), cx),
                EventOutcome::Handled
            );
        });
        // A parent exposing only rows below the measured child hides it entirely,
        // including from non-positional handlers that unconditionally accept input.
        let mut cx = EventCx::with_clip(&root, Rect::new(2, 3, 8, 2));
        assert_eq!(
            list.event(area, &state, &Event::Paste("input".into()), &mut cx),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn translated_list_routes_pointer_using_parent_clip() {
        let list = VirtualList::new("messages")
            .item(
                "a",
                0,
                EventItem {
                    id: "a",
                    height: 3,
                    outcome: EventOutcome::Handled,
                },
            )
            .item(
                "b",
                0,
                EventItem {
                    id: "b",
                    height: 3,
                    outcome: EventOutcome::Redraw,
                },
            );
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);
        let root = LayoutNode::leaf("root".into(), LogicalSize::new(30, 30));
        let mut cx = EventCx::with_clip(&root, Rect::new(0, 0, 30, 30));
        // Local area (2, 1) is presented at (12, 11), with its final row clipped.
        let area = Rect::new(2, 1, 8, 4);
        cx.with_transform(0, 0, 10, 10, Rect::new(12, 11, 8, 3), |cx| {
            for (x, y, expected) in [
                (13, 11, EventOutcome::Handled),
                (13, 12, EventOutcome::Redraw),
                (13, 13, EventOutcome::Redraw),
                (13, 14, EventOutcome::Ignored),
                (11, 11, EventOutcome::Ignored),
                (3, 1, EventOutcome::Ignored),
            ] {
                let event = Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(x, y),
                ));
                assert_eq!(list.event(area, &state, &event, cx), expected);
            }
        });
    }

    #[test]
    fn pointer_events_route_only_to_visible_intersecting_items() {
        let list = VirtualList::new("messages")
            .item(
                "a",
                0,
                EventItem {
                    id: "a",
                    height: 3,
                    outcome: EventOutcome::Handled,
                },
            )
            .item(
                "b",
                0,
                EventItem {
                    id: "b",
                    height: 3,
                    outcome: EventOutcome::Redraw,
                },
            )
            .item(
                "c",
                0,
                EventItem {
                    id: "c",
                    height: 3,
                    outcome: EventOutcome::Handled,
                },
            );
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);
        let root = LayoutNode::leaf(LayoutId::new("root"), LogicalSize::new(8, 4));
        let area = Rect::new(4, 5, 8, 4);
        let mut cx = EventCx::with_clip(&root, area);

        let first = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(5, 5),
        ));
        assert_eq!(
            list.event(area, &state, &first, &mut cx),
            EventOutcome::Handled
        );

        let lower_pointer = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(5, 8),
        ));
        assert_eq!(
            list.event(area, &state, &lower_pointer, &mut cx),
            EventOutcome::Redraw
        );

        let outside = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(3, 5),
        ));
        assert_eq!(
            list.event(area, &state, &outside, &mut cx),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn semantic_ids_remain_keyed_across_reorder() {
        let first = VirtualList::new("messages")
            .item(
                "a",
                0,
                MetadataItem {
                    id: "a",
                    height: 1,
                    cursor_row: None,
                },
            )
            .item(
                "b",
                0,
                MetadataItem {
                    id: "b",
                    height: 1,
                    cursor_row: None,
                },
            );
        let reordered = VirtualList::new("messages")
            .item(
                "b",
                0,
                MetadataItem {
                    id: "b",
                    height: 1,
                    cursor_row: None,
                },
            )
            .item(
                "a",
                0,
                MetadataItem {
                    id: "a",
                    height: 1,
                    cursor_row: None,
                },
            );
        let mut state = VirtualListState::new(0);
        let area = Rect::new(0, 0, 8, 2);

        first.sync(8, &mut state, &mut LayoutCx::new());
        let first_ids = paint_semantic_ids(&first, &state, area);
        reordered.sync(8, &mut state, &mut LayoutCx::new());
        let reordered_ids = paint_semantic_ids(&reordered, &state, area);

        assert_eq!(first_ids, ["messages.item.a", "messages.item.b"]);
        assert_eq!(reordered_ids, ["messages.item.b", "messages.item.a"]);
    }

    fn paint_semantic_ids<K>(
        list: &VirtualList<'_, K>,
        state: &VirtualListState<K>,
        area: Rect,
    ) -> Vec<String>
    where
        K: Clone + Ord + ToString,
    {
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        list.paint(area, state, &mut PaintCx::new(&mut frame));
        frame
            .semantics()
            .regions()
            .iter()
            .map(|region| region.id.as_str().to_owned())
            .collect()
    }

    #[test]
    fn stable_key_scroll_and_ensure_visible_use_exact_item_geometry() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b line that wraps"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        list.sync(6, &mut state, &mut LayoutCx::new());

        assert!(state.scroll_to_key(&"b", 3));
        assert_eq!(state.scroll.vertical_offset(), 2);
        assert!(!state.ensure_key_visible(&"b", 3));
        assert!(state.ensure_key_visible(&"c", 3));
        assert_eq!(state.scroll.vertical_offset(), 4);
        assert!(!state.scroll_to_key(&"missing", 3));
        assert!(!state.ensure_key_visible(&"missing", 3));
        assert_eq!(state.scroll.vertical_offset(), 4);
    }

    #[test]
    fn focused_key_is_ensured_visible_and_restored_after_modal_scope() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"))
            .item("d", 0, TextBlock::new("d"));
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());

        let focused_key = "d";
        assert!(state.ensure_key_visible(&focused_key, 2));
        assert_eq!(state.scroll.vertical_offset(), 2);

        let area = Rect::new(0, 0, 8, 2);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        list.paint(area, &state, &mut PaintCx::new(&mut frame));
        let focused_id = HitId::new("messages.item.d");
        let mut router = InteractionRouter::new();
        router.commit_scene(frame.hits().clone(), None);
        assert!(router.set_focused(&focused_id));

        let modal = bmux_tui::hit::HitRegion::new("modal.close", area)
            .focusable(true)
            .focus_scope("modal");
        let modal_scene = frame.hits().clone().with_region(modal);
        router.commit_scene(modal_scene, Some(HitId::new("modal")));
        assert_eq!(router.focused(), Some(&HitId::new("modal.close")));
        router.commit_scene(frame.hits().clone(), None);
        assert_eq!(router.focused(), Some(&focused_id));
    }

    #[test]
    fn capability_revision_invalidates_item_measurements_and_layouts() {
        let list =
            VirtualList::new("messages").item("a", 0, TextBlock::new("a message that wraps"));
        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();

        for revision in [1, 1, 2] {
            list.sync_with_environment(8, LayoutEnvironment::new(revision), &mut state, &mut cx);
        }

        assert_eq!(cx.measured_nodes(), 2);
        assert_eq!(state.key_at_offset(0), Some(&"a"));
        assert_eq!(state.item_offset(&"a"), Some(0));
    }

    #[test]
    fn logical_navigation_clamps_and_disables_bottom_follow() {
        fn rows(lines: &[&str]) -> TextBlock {
            TextBlock::new(Text::from_lines(
                lines
                    .iter()
                    .map(|line| Line::raw(*line))
                    .collect::<Vec<_>>(),
            ))
        }

        let mut state = VirtualListState::new(0);
        VirtualList::new("messages")
            .item("a", 0, rows(&["one", "two", "three", "four", "five"]))
            .sync(8, &mut state, &mut LayoutCx::new());

        state.scroll_to_bottom(2);
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(state.scroll.follows_bottom());
        assert!(state.scroll_by(-2, 2));
        assert_eq!(state.scroll.vertical_offset(), 1);
        assert!(!state.scroll.follows_bottom());
        assert!(state.scroll_by(i64::MAX, 2));
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(!state.scroll_by(1, 2));
        state.scroll_to_top();
        assert_eq!(state.scroll.vertical_offset(), 0);
        assert!(!state.scroll.follows_bottom());
    }

    #[test]
    fn scroll_to_bottom_sets_exact_offset_and_follows_appends() {
        fn rows(lines: &[&str]) -> TextBlock {
            TextBlock::new(Text::from_lines(
                lines
                    .iter()
                    .map(|line| Line::raw(*line))
                    .collect::<Vec<_>>(),
            ))
        }

        let mut state = VirtualListState::new(0);
        VirtualList::new("messages")
            .item("a", 0, rows(&["one", "two", "three"]))
            .sync(8, &mut state, &mut LayoutCx::new());

        state.scroll_to_bottom(2);
        assert_eq!(state.scroll.vertical_offset(), 1);
        assert!(state.scroll.follows_bottom());

        state.capture_anchor();
        VirtualList::new("messages")
            .item("a", 0, rows(&["one", "two", "three"]))
            .item("b", 0, rows(&["four", "five"]))
            .sync(8, &mut state, &mut LayoutCx::new());
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(state.scroll.follows_bottom());
    }

    #[test]
    fn state_geometry_queries_hide_index_implementation() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b wraps here"));
        let mut state = VirtualListState::new(1);
        list.sync(6, &mut state, &mut LayoutCx::new());

        assert_eq!(state.total_height(), 5);
        assert_eq!(state.item_offset(&"a"), Some(0));
        assert_eq!(state.item_offset(&"b"), Some(2));
        assert_eq!(state.key_at_offset(0), Some(&"a"));
        assert_eq!(state.key_at_offset(1), Some(&"a"));
        assert_eq!(state.key_at_offset(2), Some(&"b"));
        assert_eq!(state.key_at_offset(4), Some(&"b"));
        assert_eq!(state.key_at_offset(5), None);
    }

    #[test]
    fn viewport_height_does_not_remeasure_but_width_and_removed_keys_do() {
        let list = VirtualList::new("messages")
            .item("a", 1, TextBlock::new("a message that wraps"))
            .item("b", 1, TextBlock::new("b"));
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        list.sync(8, &mut state, &mut cx);
        let initial_measurements = cx.measured_nodes();

        for viewport_height in [1, 3, 20] {
            state.capture_anchor();
            state.restore_anchor(viewport_height);
            list.sync(8, &mut state, &mut cx);
        }
        assert_eq!(cx.measured_nodes(), initial_measurements);

        let reordered = VirtualList::new("messages")
            .item("b", 1, TextBlock::new("b"))
            .item("a", 1, TextBlock::new("a message that wraps"));
        reordered.sync(8, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), initial_measurements);

        reordered.sync(6, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), initial_measurements + 2);
        let retained_before_removal = state.layout_cache().len();

        VirtualList::new("messages")
            .item("a", 1, TextBlock::new("a message that wraps"))
            .sync(6, &mut state, &mut cx);
        assert!(state.layout_cache().len() < retained_before_removal);
        assert!(state.layout_cache().stats().released > 0);
    }

    #[test]
    fn component_item_repaints_without_remeasuring_for_paint_only_revision() {
        let area = Rect::new(0, 0, 8, 1);
        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();
        let initial = VirtualList::new("messages").component(
            "a",
            TextBlock::new("message").style(Style::new().fg(bmux_tui::style::Color::Red)),
        );
        initial.sync(u64::from(area.width), &mut state, &mut cx);
        let measured = cx.measured_nodes();

        let changed = VirtualList::new("messages").component(
            "a",
            TextBlock::new("message").style(Style::new().fg(bmux_tui::style::Color::Blue)),
        );
        changed.sync(u64::from(area.width), &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), measured);

        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        changed.paint(area, &state, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).unwrap().style.fg,
            Some(bmux_tui::style::Color::Blue)
        );
    }

    #[test]
    fn component_item_uses_declared_layout_revision() {
        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();
        VirtualList::new("messages")
            .component(
                "a",
                RevisedHeightItem {
                    revision: 1,
                    height: 1,
                },
            )
            .sync(8, &mut state, &mut cx);
        assert_eq!(state.total_height(), 1);

        VirtualList::new("messages")
            .component(
                "a",
                RevisedHeightItem {
                    revision: 2,
                    height: 3,
                },
            )
            .sync(8, &mut state, &mut cx);
        assert_eq!(state.total_height(), 3);
        assert_eq!(cx.measured_nodes(), 2);
    }

    #[test]
    fn external_item_revision_invalidates_retained_layout() {
        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();
        VirtualList::new("messages")
            .item("a", 1, ExternallyRevisedItem)
            .sync(8, &mut state, &mut cx);
        VirtualList::new("messages")
            .item("a", 2, ExternallyRevisedItem)
            .sync(8, &mut state, &mut cx);

        assert_eq!(cx.measured_nodes(), 2);
    }

    #[test]
    fn item_paint_scope_cannot_fill_gaps_or_neighbors() {
        struct FillItem;
        impl Component for FillItem {
            fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf("fill".into(), constraints.constrain(LogicalSize::new(4, 1)))
            }
            fn paint(&self, _layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
                cx.fill(cx.area(), "x", bmux_tui::style::Style::default());
            }
        }
        let list = VirtualList::new("list")
            .item("first", 0, TextBlock::new("a"))
            .item("fill", 0, FillItem)
            .item("last", 0, TextBlock::new("b"));
        let mut state = VirtualListState::new(1);
        list.sync(4, &mut state, &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 5));
        let mut frame = Frame::new(&mut buffer);
        list.paint(Rect::new(0, 0, 4, 5), &state, &mut PaintCx::new(&mut frame));
        for (row, expected) in ["a   ", "    ", "xxxx", "    ", "b   "].iter().enumerate() {
            assert_eq!(
                frame
                    .buffer()
                    .row_symbols(u16::try_from(row).unwrap())
                    .as_deref(),
                Some(*expected)
            );
        }
    }

    #[test]
    fn parent_clip_limits_virtual_list_paint_work() {
        let list = VirtualList::new("list")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"))
            .item("d", 0, TextBlock::new("d"));
        let mut state = VirtualListState::new(0);
        list.sync(4, &mut state, &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(0, -2, LocalRect::new(0, 0, 4, 4), |cx| {
            let report = list.paint(Rect::new(0, 0, 4, 4), &state, cx);
            assert_eq!(report.painted_items, 1);
            assert_eq!(report.registered_items, 1);
        });
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("c   "));
        assert_eq!(frame.semantics().regions().len(), 1);
        PaintCx::new(&mut frame).with_child(10, 0, LocalRect::new(0, 0, 4, 4), |cx| {
            assert_eq!(
                list.paint(Rect::new(0, 0, 4, 4), &state, cx).painted_items,
                0
            );
        });
    }

    #[test]
    fn paints_and_registers_only_intersecting_variable_height_items() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b line that wraps"))
            .item("c", 0, TextBlock::new("c"))
            .item("d", 0, TextBlock::new("d"));
        let mut state = VirtualListState::new(1);
        list.sync(6, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 3));
        let mut frame = Frame::new(&mut buffer);
        let report = list.paint(Rect::new(0, 0, 6, 3), &state, &mut PaintCx::new(&mut frame));

        assert_eq!(report.painted_items, 1);
        assert_eq!(frame.hits().regions().len(), 2);
        assert_eq!(frame.hits().regions()[0].id.as_str(), "messages.item.b");
        assert!(frame.hits().regions()[0].pointer_events);
        assert_eq!(frame.hits().regions()[1].id.as_str(), "messages.item.b");
        assert!(!frame.hits().regions()[1].pointer_events);
        assert!(frame.hits().regions()[1].focusable);
        assert_eq!(frame.hits().regions()[1].area, Rect::new(0, 0, 6, 3));
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].id, "messages.item.b");
        assert_eq!(frame.semantics().regions()[0].role, "list-item");
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 6, 3));
        assert_eq!(
            frame.damage(bmux_tui::damage::DamagePolicy {
                max_regions: 64,
                max_area_percent: 101,
            }),
            bmux_tui::damage::Damage::Regions(vec![Rect::new(0, 0, 6, 3)])
        );
    }

    #[test]
    fn clips_partial_first_and_last_items_to_exact_viewport_rows() {
        let list = VirtualList::new("messages")
            .item(
                "a",
                0,
                MetadataItem {
                    id: "a",
                    height: 3,
                    cursor_row: None,
                },
            )
            .item(
                "b",
                0,
                MetadataItem {
                    id: "b",
                    height: 3,
                    cursor_row: None,
                },
            )
            .item(
                "c",
                0,
                MetadataItem {
                    id: "c",
                    height: 3,
                    cursor_row: None,
                },
            );
        let mut state = VirtualListState::new(0);
        list.sync(6, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(1);
        let area = Rect::new(0, 0, 6, 4);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);

        let report = list.paint(area, &state, &mut PaintCx::new(&mut frame));
        let pointer_hits = frame
            .hits()
            .regions()
            .iter()
            .filter(|region| region.pointer_events)
            .collect::<Vec<_>>();

        assert_eq!(report.painted_items, 2);
        assert_eq!(pointer_hits.len(), 2);
        assert_eq!(pointer_hits[0].id.as_str(), "messages.item.a");
        assert_eq!(pointer_hits[0].area, Rect::new(0, 0, 6, 2));
        assert_eq!(pointer_hits[1].id.as_str(), "messages.item.b");
        assert_eq!(pointer_hits[1].area, Rect::new(0, 2, 6, 2));
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 6, 2));
        assert_eq!(frame.semantics().regions()[1].area, Rect::new(0, 2, 6, 2));
        assert!(
            frame
                .hits()
                .regions()
                .iter()
                .all(|region| { region.id.as_str() != "messages.item.c" })
        );
    }

    #[test]
    fn boundary_items_are_the_only_registered_interactions() {
        let list = (0..100usize).fold(VirtualList::new("messages"), |list, key| {
            list.item(
                key,
                0,
                MetadataItem {
                    id: "item",
                    height: 3,
                    cursor_row: None,
                },
            )
        });
        let mut state = VirtualListState::new(1);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);
        let area = Rect::new(0, 0, 8, 5);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);

        let report = list.paint(area, &state, &mut PaintCx::new(&mut frame));
        let pointer_ids = frame
            .hits()
            .regions()
            .iter()
            .filter(|region| region.pointer_events)
            .map(|region| region.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(report.painted_items, 2);
        assert_eq!(report.registered_items, 2);
        assert_eq!(pointer_ids, ["messages.item.0", "messages.item.1"]);
        assert_eq!(frame.semantics().regions().len(), 2);
        assert_eq!(frame.selection().fragments().len(), 1);
        assert_eq!(frame.images().len(), 2);
        assert!(matches!(
            frame.damage(DamagePolicy {
                max_regions: usize::MAX,
                max_area_percent: 101,
            }),
            Damage::Regions(ref regions)
                if !regions.is_empty()
                    && regions.len() <= 2
                    && regions.iter().all(|region| area.intersection(*region) == *region)
        ));
    }

    #[test]
    fn large_collection_paint_work_is_bounded_by_viewport() {
        let mut list = VirtualList::new("messages");
        for key in 0..10_000usize {
            list = list.item(key, 0, TextBlock::new("x"));
        }
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(7_500);

        let area = Rect::new(0, 0, 8, 5);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        let report = list.paint(area, &state, &mut PaintCx::new(&mut frame));

        assert_eq!(report.painted_items, 5);
        assert_eq!(report.registered_items, 5);
        assert_eq!(
            frame
                .hits()
                .regions()
                .iter()
                .filter(|region| region.pointer_events)
                .count(),
            5
        );
        assert_eq!(frame.semantics().regions().len(), 5);
    }

    #[test]
    fn removed_anchor_falls_forward_then_back_and_empty_resets() {
        let initial = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        initial.sync(8, &mut state, &mut LayoutCx::new());
        state
            .scroll
            .set_vertical_offset(initial.item_offset(&state, &"b").unwrap());
        state.capture_anchor();

        let removed = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("c", 0, TextBlock::new("c"));
        removed.sync(8, &mut state, &mut LayoutCx::new());
        state.restore_anchor(1);
        assert_eq!(state.scroll.vertical_offset(), 2);

        let empty: VirtualList<'_, &str> = VirtualList::new("messages");
        empty.sync(8, &mut state, &mut LayoutCx::new());
        state.restore_anchor(1);
        assert_eq!(state.scroll.vertical_offset(), 0);
    }

    #[test]
    fn stable_top_anchor_survives_insertion_and_width_reflow() {
        let first = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("short"))
            .item("b", 0, TextBlock::new("b message wraps here"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        first.sync(8, &mut state, &mut LayoutCx::new());
        let b_start = first.item_offset(&state, &"b").unwrap();
        state.scroll.set_vertical_offset(b_start.saturating_add(1));
        state.capture_anchor();

        let changed = VirtualList::new("messages")
            .item("new", 0, TextBlock::new("inserted"))
            .item("a", 0, TextBlock::new("short"))
            .item("b", 0, TextBlock::new("b message wraps here"))
            .item("c", 0, TextBlock::new("c"));
        changed.sync(6, &mut state, &mut LayoutCx::new());
        state.restore_anchor(3);

        assert_eq!(
            state.scroll.vertical_offset(),
            changed.item_offset(&state, &"b").unwrap().saturating_add(1)
        );
    }

    #[test]
    fn stable_viewport_survives_complete_keyed_mutation_sequence() {
        let initial = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("d", 0, TextBlock::new("d"));
        let mut state = VirtualListState::new(1);
        initial.sync(8, &mut state, &mut LayoutCx::new());
        state
            .scroll
            .set_vertical_offset(state.item_offset(&"c").unwrap().saturating_add(1));
        state.capture_anchor();

        let appended = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("d", 0, TextBlock::new("d"))
            .item("e", 0, TextBlock::new("e"));
        assert_anchor_after_sync(&appended, 8, &mut state, "c", 1);

        let inserted = VirtualList::new("messages")
            .item("new", 0, TextBlock::new("inserted above"))
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("d", 0, TextBlock::new("d"))
            .item("e", 0, TextBlock::new("e"));
        assert_anchor_after_sync(&inserted, 8, &mut state, "c", 1);

        let reordered = VirtualList::new("messages")
            .item("e", 0, TextBlock::new("e"))
            .item("d", 0, TextBlock::new("d"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("a", 0, TextBlock::new("a"))
            .item("new", 0, TextBlock::new("inserted above"));
        assert_anchor_after_sync(&reordered, 8, &mut state, "c", 1);
        assert_anchor_after_sync(&reordered, 6, &mut state, "c", 1);

        let removed_above = VirtualList::new("messages")
            .item("e", 0, TextBlock::new("e"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("a", 0, TextBlock::new("a"))
            .item("new", 0, TextBlock::new("inserted above"));
        assert_anchor_after_sync(&removed_above, 6, &mut state, "c", 1);
    }

    fn assert_anchor_after_sync<'a>(
        list: &VirtualList<'_, &'a str>,
        width: u16,
        state: &mut VirtualListState<&'a str>,
        key: &str,
        row: u64,
    ) {
        list.sync(u64::from(width), state, &mut LayoutCx::new());
        state.restore_anchor(3);
        assert_eq!(
            state.scroll.vertical_offset(),
            state.item_offset(&key).unwrap().saturating_add(row)
        );
    }

    #[test]
    fn bottom_follow_and_ensure_visible_use_keyed_collection_geometry() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_follow_bottom(true);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert_eq!(state.total_height(), 5);
        let scrollbar = state.scrollbar_state(2);
        assert_eq!(scrollbar.content_len, 5);
        assert_eq!(scrollbar.viewport_len, 2);
        assert_eq!(scrollbar.offset, 3);
        assert_eq!(scrollbar.max_offset(), 3);
        assert!(list.ensure_item_visible(&mut state, &"a", 2));
        assert_eq!(state.scroll.vertical_offset(), 0);
    }

    #[test]
    fn list_and_state_visibility_share_offsets_and_follow_policy() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("first"))
            .item("b", 0, TextBlock::new("several wrapped words"))
            .item("c", 0, TextBlock::new("last"));
        for viewport in [0, 1, 3, 100] {
            for offset in [0, 2, u64::MAX] {
                for key in ["a", "b", "c", "missing"] {
                    let mut through_list = VirtualListState::new(1);
                    let mut through_state = VirtualListState::new(1);
                    for state in [&mut through_list, &mut through_state] {
                        list.sync(5, state, &mut LayoutCx::new());
                        state.scroll.set_vertical_offset(offset);
                        state.scroll.set_follow_bottom(true);
                    }
                    let found = list.ensure_item_visible(&mut through_list, &key, viewport);
                    let changed = through_state.ensure_key_visible(&key, viewport);
                    assert_eq!(found, key != "missing");
                    assert_eq!(changed, through_state.scroll.vertical_offset() != offset);
                    assert_eq!(
                        through_list.scroll.vertical_offset(),
                        through_state.scroll.vertical_offset()
                    );
                    assert_eq!(
                        through_list.scroll.follows_bottom(),
                        through_state.scroll.follows_bottom()
                    );
                    if !found {
                        assert_eq!(through_list.scroll.vertical_offset(), offset);
                        assert!(through_list.scroll.follows_bottom());
                    }
                }
            }
        }
    }

    #[test]
    fn shrinking_anchor_item_keeps_the_surviving_key_visible() {
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        let initial = VirtualList::new("reflow")
            .item("anchor", 0, TextBlock::new("a\nb\nc\nd\ne"))
            .item("following", 0, TextBlock::new("1\n2\n3\n4\n5\n6"));
        initial.sync(8, &mut state, &mut cx);
        state.scroll.set_vertical_offset(4);
        state.capture_anchor();
        let shorter = VirtualList::new("reflow")
            .item("anchor", 1, TextBlock::new("a\nb"))
            .item("following", 0, TextBlock::new("1\n2\n3\n4\n5\n6"));
        shorter.sync(8, &mut state, &mut cx);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 1);
        assert_eq!(state.key_at_offset(1), Some(&"anchor"));
        assert!(!state.scroll.follows_bottom());
        // Subsequent reflow retains the clamped row, not the stale original row.
        initial.sync(8, &mut state, &mut cx);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 1);
    }

    #[test]
    fn clamping_preserves_follow_policy_across_append_and_viewport_changes() {
        let initial = VirtualList::new("list")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"));
        let appended = VirtualList::new("list")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"));
        for follow in [false, true] {
            let mut state = VirtualListState::new(0);
            initial.sync(8, &mut state, &mut LayoutCx::new());
            state.scroll.set_vertical_offset(100);
            state.scroll.set_follow_bottom(follow);
            state.clamp_scroll(1);
            assert_eq!(state.scroll.vertical_offset(), 1);
            assert_eq!(state.scroll.follows_bottom(), follow);
            state.clamp_scroll(3);
            assert_eq!(state.scroll.vertical_offset(), 0);
            assert_eq!(state.scroll.follows_bottom(), follow);
            appended.sync(8, &mut state, &mut LayoutCx::new());
            state.restore_anchor(1);
            assert_eq!(state.scroll.vertical_offset(), if follow { 2 } else { 0 });
            assert_eq!(state.scroll.follows_bottom(), follow);
        }
    }

    #[test]
    fn bottom_follow_survives_append_without_jump() {
        let initial = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        initial.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_follow_bottom(true);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 3);

        let appended = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"))
            .item("d", 0, TextBlock::new("d message wraps"));
        appended.sync(8, &mut state, &mut LayoutCx::new());
        state.restore_anchor(2);

        assert!(state.scroll.follows_bottom());
        assert_eq!(
            state.scroll.vertical_offset(),
            state.total_height().saturating_sub(2)
        );
    }

    #[test]
    fn generic_component_measurements_retain_by_key_not_rendered_rows() {
        let first = VirtualList::new("surfaces")
            .item(
                "panel",
                7,
                MetadataItem {
                    id: "panel",
                    height: 3,
                    cursor_row: None,
                },
            )
            .item(
                "toolbar",
                11,
                MetadataItem {
                    id: "toolbar",
                    height: 2,
                    cursor_row: None,
                },
            );
        let mut state = VirtualListState::new(2);
        let mut cx = LayoutCx::new();
        first.sync(20, &mut state, &mut cx);
        let measured = cx.measured_nodes();

        let reordered = VirtualList::new("surfaces")
            .item(
                "toolbar",
                11,
                MetadataItem {
                    id: "toolbar",
                    height: 2,
                    cursor_row: None,
                },
            )
            .item(
                "panel",
                7,
                MetadataItem {
                    id: "panel",
                    height: 3,
                    cursor_row: None,
                },
            );
        reordered.sync(20, &mut state, &mut cx);

        assert_eq!(cx.measured_nodes(), measured);
        assert_eq!(state.total_height(), 7);
        assert_eq!(state.item_offset(&"toolbar"), Some(0));
        assert_eq!(state.item_offset(&"panel"), Some(4));
        assert_eq!(state.key_at_offset(3), Some(&"toolbar"));
        assert_eq!(state.key_at_offset(4), Some(&"panel"));
    }

    #[test]
    fn ensuring_visible_item_preserves_bottom_follow_without_movement() {
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        let list = VirtualList::new("reveal")
            .item("a", 0, TextBlock::new("a\na"))
            .item("b", 0, TextBlock::new("b\nb"));
        list.sync(10, &mut state, &mut cx);
        state.scroll_to_bottom(2);
        assert!(!state.ensure_key_visible(&"b", 2));
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(state.scroll.follows_bottom());
        assert!(list.ensure_item_visible(&mut state, &"b", 2));
        assert!(!list.ensure_item_visible(&mut state, &"missing", 2));
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(state.scroll.follows_bottom());
        assert!(state.ensure_key_visible(&"a", 2));
        assert_eq!(state.scroll.vertical_offset(), 0);
        assert!(!state.scroll.follows_bottom());
    }

    #[test]
    fn scrolling_up_stops_following_appended_content() {
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        let populated = VirtualList::new("follow")
            .item("a", 0, TextBlock::new("a\na\na"))
            .item("b", 0, TextBlock::new("b\nb\nb"));
        populated.sync(10, &mut state, &mut cx);
        state.scroll_to_bottom(2);
        assert!(state.scroll_by(-1, 2));
        assert!(!state.scroll.follows_bottom());
        state.capture_anchor();

        let appended = populated.item("c", 0, TextBlock::new("c\nc\nc"));
        appended.sync(10, &mut state, &mut cx);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 4);
        assert!(!state.scroll.follows_bottom());
        state.scroll_to_bottom(2);
        assert_eq!(state.scroll.vertical_offset(), 9);
        assert!(state.scroll.follows_bottom());
    }

    #[test]
    fn manual_anchor_resets_when_collection_is_cleared() {
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        let populated = VirtualList::new("manual")
            .item("a", 0, TextBlock::new("a\na\na"))
            .item("b", 0, TextBlock::new("b\nb\nb"));
        populated.sync(10, &mut state, &mut cx);
        assert!(state.scroll_to_key(&"b", 2));
        state.capture_anchor();
        assert_eq!(state.scroll.vertical_offset(), 4);
        assert!(!state.scroll.follows_bottom());

        VirtualList::<&str>::new("manual").sync(10, &mut state, &mut cx);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 0);
        assert!(state.anchor.is_none());

        populated.sync(10, &mut state, &mut cx);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 0);
        assert!(!state.scroll.follows_bottom());
    }

    #[test]
    fn bottom_follow_survives_empty_collection_and_repopulation() {
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        let populated = VirtualList::new("follow")
            .item("a", 0, TextBlock::new("a\na\na"))
            .item("b", 0, TextBlock::new("b\nb\nb"));
        populated.sync(10, &mut state, &mut cx);
        state.scroll_to_bottom(2);
        assert_eq!(state.scroll.vertical_offset(), 5);
        state.capture_anchor();

        VirtualList::<&str>::new("follow").sync(10, &mut state, &mut cx);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 0);
        assert!(state.scroll.follows_bottom());

        let repopulated = VirtualList::new("follow").item("c", 0, TextBlock::new("c\nc\nc\nc"));
        repopulated.sync(10, &mut state, &mut cx);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 2);
        assert!(state.scroll.follows_bottom());

        state.restore_anchor(10);
        assert_eq!(state.scroll.vertical_offset(), 0);
        assert!(state.scroll.follows_bottom());
        state.restore_anchor(1);
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(state.scroll.follows_bottom());
    }

    #[test]
    fn owned_keys_reuse_measurements_across_rebuild_and_revision_change() {
        let first = VirtualList::new("owned")
            .item(String::from("a"), 0, TextBlock::new("a"))
            .item(String::from("b"), 0, TextBlock::new("b"));
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        first.sync(10, &mut state, &mut cx);
        let measured = cx.measured_nodes();
        drop(first);

        let rebuilt = VirtualList::new("owned")
            .item(String::from("b"), 0, TextBlock::new("b"))
            .item(String::from("a"), 1, TextBlock::new("a\na"));
        rebuilt.sync(10, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), measured + 1);
        assert_eq!(state.total_height(), 4);
        assert_eq!(state.item_offset(&String::from("b")), Some(0));
        assert_eq!(state.item_offset(&String::from("a")), Some(2));
        rebuilt.sync(10, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), measured + 1);

        let removed = VirtualList::new("owned").item(String::from("b"), 0, TextBlock::new("b"));
        removed.sync(10, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), measured + 1);
        assert_eq!(state.item_offset(&String::from("a")), None);
        assert_eq!(state.total_height(), 1);

        let reinserted = VirtualList::new("owned")
            .item(String::from("b"), 0, TextBlock::new("b"))
            .item(String::from("a"), 1, TextBlock::new("a\na\na"));
        reinserted.sync(10, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), measured + 2);
        assert_eq!(state.total_height(), 5);
        assert_eq!(state.item_offset(&String::from("a")), Some(2));

        VirtualList::<String>::new("owned").sync(10, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), measured + 2);
        assert_eq!(state.total_height(), 0);
        assert_eq!(state.key_at_offset(0), None);
        assert_eq!(state.item_offset(&String::from("b")), None);

        let repopulated =
            VirtualList::new("owned").item(String::from("b"), 0, TextBlock::new("b\nb"));
        repopulated.sync(10, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), measured + 3);
        assert_eq!(state.total_height(), 2);
        assert_eq!(state.item_offset(&String::from("b")), Some(0));
    }

    #[test]
    fn sync_retains_measurements_across_reorder_and_exposes_key_offsets() {
        let first = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"));
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        first.sync(8, &mut state, &mut cx);
        let measured = cx.measured_nodes();

        let reordered = VirtualList::new("messages")
            .item("b", 0, TextBlock::new("b"))
            .item("a", 0, TextBlock::new("a"));
        reordered.sync(8, &mut state, &mut cx);

        assert_eq!(cx.measured_nodes(), measured);
        assert_eq!(reordered.item_offset(&state, &"a"), Some(2));
    }
}
