# BMUX TUI framework

`bmux_tui` is BMUX's domain-neutral foundation for measurable terminal user
interfaces. It owns logical geometry, constraint-based layout, scoped painting,
styled text, terminal buffers, interaction metadata, selection, images, damage,
and ANSI presentation primitives. Reusable controls live in
`bmux_tui_components`; scheduling and terminal presentation live in
`bmux_tui_runtime`.

Product behavior such as windows, sessions, panes, clients, contexts,
permissions, model turns, tools, and chat state remains in plugins and
applications.

## Crate boundaries

```text
applications and plugins
        |
        +-- bmux_tui_components  reusable controls and control state
        +-- bmux_tui_runtime     scheduling, events, commands, presentation
                    |
                 bmux_tui        geometry, layout, paint, scenes, terminal I/O
```

- `bmux_tui` does not depend on either higher layer.
- `bmux_tui_runtime` depends on `bmux_tui`, not on `bmux_tui_components`.
- Application and control state is caller-owned.
- The framework may retain only derived data that can be reconstructed from
  caller state, stable identities, revisions, constraints, and environment.
- Core layers must not interpret a plugin or application domain.

## Canonical component lifecycle

A terminal component implements `Component`:

```rust,ignore
pub trait Component {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode;
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>);
    fn event(
        &self,
        event: &Event,
        layout: &LayoutNode,
        cx: &mut EventCx<'_>,
    ) -> EventOutcome;
    fn revision(&self) -> ComponentRevision;
}
```

Layout resolves explicit constraints into an authoritative `LayoutNode` tree.
Painting and event routing consume that exact tree; they do not independently
recompute placement. A parent assigns each child's local placement, and scoped
contexts translate it to terminal coordinates.

`ComponentRevision` has independent layout and paint channels. Callers advance
the layout revision when measurement or child placement can change and the
paint revision when visual output changes without changing geometry. Stable
`LayoutId` values identify retained dynamic children. Vector positions and
whole-tree hashes are not substitutes for stable identity.

`LayoutCache` keys derived geometry by stable identity, layout revision,
constraints, and layout environment. Cache entries are disposable and never
become application state.

## Logical geometry

Terminal width is represented in cells (`u16`), while vertical document
positions and extents use logical rows (`usize`). This lets a scrollable or
virtualized document exceed terminal coordinate limits. Conversion to terminal
rectangles happens only where content intersects a visible boundary.

Nested components use local coordinates. `PaintCx` and `EventCx` carry the
current translation and effective clip. Every projected channel follows the
same transform and clip:

- cells and wide glyph continuations;
- cursor ownership and visibility;
- pointer hit regions and focus geometry;
- semantic regions;
- UTF-8 selection fragments and scopes;
- image placements;
- damage regions.

Offscreen descendants contribute no visible or interactive metadata.

## Composition primitives

`composition` contains orthogonal containers and wrappers rather than
product-specific controls:

- `Surface` owns a child plus background, border, padding, and style scope;
- `Column` and `Row` own linear placement, gaps, intrinsic/fixed/flex sizing,
  cross-axis alignment, and deterministic constrained overflow;
- `Padding`, `SizeBox`, `Fill`, `Align`, `Flex`, `Clip`, `StyleScope`,
  `Visibility`, `Stack`, and `Keyed` modify one composition concern;
- `TextBlock` provides measurable rich text and source projection.

Translated/clipped scrolling over arbitrary content is owned by
`bmux_tui_components::ScrollViewComponent` and its `ScrollView` controller; the
core crate provides only the coordinate/clip primitives it builds on.

A rectangular style belongs to the component that measures that rectangle.
Consumers must not extend backgrounds, reconstruct wrapped rows, or mutate the
buffer after rendering to compensate for missing geometry.

Editable controls belong to `bmux_tui_components`: use `TextInputComponent` or
`TextInputBoxComponent` with caller-owned `TextInputState`. The core
`bmux_tui::input::TextInput` leaf exists for component implementation and is not
an application-facing/prelude API.

New child-owning surfaces should prefer `Surface`; duplicate panel, modal,
clear, overlay, or content-layout engines must not be introduced.

## Text and selection

Rich text preserves span styles, Unicode display width, wrapping, alignment,
and logical source projection. Selection uses stable content and scope
identities plus logical UTF-8 byte offsets. Rendering projects those endpoints
to visible cells; resize, wrapping, clipping, and scrolling do not rewrite the
logical selection.

`SelectionController` is caller-owned. Components register scopes and fragments
during scene construction. `PaintCx::paint_selection` is the deterministic
visual
overlay stage after ordinary content painting. Copying reads the logical
selection snapshot rather than scraping the terminal buffer.

## Text projection cost

`TextBlock` measures wrapped height without constructing rendered text.
Character
measurement does not allocate output rows; word measurement still retains
segment
metadata but tracks row boundaries numerically without allocating row storage.
Both measurement and rendering use the same
wrapping rules, including distinct first/continuation widths in the underlying
text helpers.

Painting and selection construct output lazily. They skip complete offscreen
source lines before rendering and stop after the visible row range. Locating the
first visible row counts only enough rows to determine which source line
contains
it, and does not measure the retained line at an exact source-line boundary.
Selection preserves original UTF-8 source offsets when starting at a later line.

These properties do **not** make arbitrary text projection viewport-bounded:

- locating a deep scroll position still measures preceding source lines;
- reaching a wrapped row inside the retained source line still traverses preceding
  wrapped rows;
- word segmentation reads an entire whitespace or non-whitespace segment before
  wrapping it, so a single very long word can require substantial lookahead;
- word selection projection searches source bytes for rendered row content;
- the public `projection(width)` method explicitly collects every projected row.

Use `VirtualList` for independent variable-height items rather than flattening a
large document into one text block. This bounds item construction and painting,
not the work inside an individual oversized item. Applications must not
compensate
with a second wrapping or source-coordinate engine; further projection indexing
belongs in the framework and must retain the same authoritative wrapping rules.

## Interaction and committed scenes

Components register hit regions, focus geometry, semantics, cursors, selection
fragments, images, and damage from their authoritative layouts. Stable IDs,
not coordinates, preserve focus and interaction identity across reflow.

Presentation is transactional. Runtime presentation stages a complete scene,
flushes terminal output, and publishes interaction metadata only after a
successful presentation. Failed presentation does not expose geometry that the
user cannot see. Regional updates replace only the affected metadata while
preserving valid state outside the damaged region.

## Scrolling

Reusable arbitrary-content scrolling belongs to
`bmux_tui_components::ScrollView`. Its caller-owned `ScrollViewState` stores
logical horizontal and vertical offsets, bottom-follow state, and interaction
state. Reconciliation clamps offsets against authoritative content and viewport
layout.

The shared scroll path owns:

- keyboard, page, home/end, and wheel navigation;
- viewport-routed nested scrolling and edge propagation;
- horizontal and vertical logical offsets;
- integrated vertical and horizontal gutter scrollbars, their hit regions, and
  drag mapping through `ScrollbarAxisLayoutMode`;
- ensure-visible and focus visibility;
- selection edge autoscroll;
- bottom-follow restoration.

Controls must not add independent line-oriented scroll engines when their
content can use this model. `TextViewComponent` is the reference consumer: it
composes a measured `TextBlock` inside the shared viewport, so wrapping, exact
height, clipping, scrollbars, selection geometry, and events all derive from one
layout and one caller-owned `ScrollViewState`. `SelectableListComponent` and
`TableComponent` (and the menu/dropdown controls built on the list) embed a
`ScrollViewState` in their caller-owned state and resolve a
`ScrollViewComponent::viewport_layout` node inside their own layout, so paging,
wheel, gutter scrollbars, clamping, and selected-item auto-scroll route through
the same controller rather than a per-control offset. Sticky table columns are
a per-line horizontal projection over that viewport, not a second engine.

Keyed virtualized collections are the item-granular counterpart: `VirtualList`
positions are keyed anchors (see [Virtualized scrolling](#virtualized-scrolling))
rather than stored row offsets, and they reuse the shared scrollbar geometry and
selection edge-autoscroll requests instead of adding parallel ones.

`TextInputState` keeps its wrapped-row offset because that offset is derived
from the caller-owned edit buffer's cursor and wrap layout, which the shared
viewport does not own; it is cursor-following text-edit state, not content
scrolling.

## Variable-height virtualization

`bmux_tui_components::VirtualList` composes arbitrary keyed item components.
`MeasuredListIndex` retains exact current-width heights and prefix geometry
behind an implementation-independent API.

The virtual-list contract is:

- item keys are stable across insertion, removal, reorder, and reflow;
- cache validity includes key, layout revision, width, and relevant environment;
- total height, prefix offsets, and offset-to-item lookup remain exact;
- only viewport-intersecting boundary and visible items paint or register
  interaction metadata;
- top-item and bottom-follow anchors survive mutation and width reflow;
- scroll-to-key and ensure-visible use keyed geometry;
- scrolling is expressed as a keyed `ListPosition` and `VirtualListAction`s
  (`set_position`, `apply_action`, `dispatch_action`; see "Virtualized
  scrolling") rather than by mutating or recomputing row offsets outside the
  collection state. The offset methods (`scroll_by`, `scroll_to_top`,
  `scroll_to_bottom`) remain as a legacy uncontrolled API and are ignored by
  controlled lists;
- a retained layout is reused for painting instead of being remeasured there.

Exact measurement is the correctness model. Estimated heights are not part of
the public contract unless measured evidence establishes that exact retained
measurement cannot satisfy the supported scale. The single exception is
`resident_window::PositionEstimate` (see [Resident windows](#resident-windows)):
a separately typed, presentation-only hint for rows that are not resident at
all. It is never an item height and never enters `MeasuredListIndex`.

## Virtualized scrolling

This section fixes the public API for keyed, anchor-based scrolling over
virtualized collections: the incremental measured index, the anchor position
model, overscroll, scroll motion, lazy item construction, the
`VirtualListComponent`, and resident windows over longer sequences. BMUX owns
the mechanics; follow, hold, reveal, paging, and eviction *policy* remain with
the caller. All types are generic over a caller key `K` and, where source
anchoring applies, an opaque caller source position `S` that BMUX never
interprets.

### Placement and features

| Capability | Crate and module | Feature |
| --- | --- | --- |
| Incremental measured index | `bmux_tui::measured_list` | always available |
| Position model, overscroll, anchor policies | `bmux_tui_components::virtual_list` | `virtual-list` |
| Lazy item source | `bmux_tui_components::virtual_list` | `virtual-list` |
| Scroll motion | `bmux_tui_components::virtual_list` | `virtual-list` |
| `VirtualListComponent`, chrome, actions | `bmux_tui_components::virtual_list` | `virtual-list` |
| Resident windows, `PositionEstimate` | `bmux_tui_components::resident_window` | `resident-window` |

`virtual-list` enables `scroll-view` (and through it `scrollbar` and
`keyboard-input`) and `resident-window`, so the list can accept a
`PositionEstimate` and report `EdgeRows`. `resident-window` has no
dependencies and may be enabled alone. Nothing here reaches `bmux_tui_runtime`;
animation is driven by caller-supplied time (see [Scroll motion](#scroll-motion)).

Every type in the tables below is re-exported from
`bmux_tui_components::virtual_list`; submodules are an implementation detail.

### Geometry conventions

- Rows are logical `u64` collection rows. Row 0 is the first row of the first
  item. Each item occupies its exact height plus the index gap, except the final
  item, whose trailing gap is excluded from `total_height`.
- A *top offset* is the collection row shown on the viewport's first row.
  Positions are never stored as top offsets in controlled mode; offsets are
  derived from a keyed position and the current geometry each time they are
  needed.
- An item's *extent* is its height plus the gap (final item: height only). A
  `Row(r)` anchor offset resolves to `item_start + min(r, extent - 1)`, or
  `item_start` for a zero-extent item, so gap rows are addressable and a shrunken
  item clamps instead of shifting its neighbors.

### Incremental measured index

`bmux_tui::measured_list::MeasuredListIndex<K>` stores exact heights in an
order-statistic tree with parent links. Every query and keyed mutation is
logarithmic in the collection length `n`; inserting or removing `k` items costs
`O(k + log n)` tree work plus `O(k log n)` key bookkeeping. Key-to-index lookup
stays correct across every splice without rebuilding. Gaps are applied
arithmetically, so `set_gap` is constant time.

```rust,ignore
pub struct MeasuredListItem<K> {
    pub key: K,
    pub layout_revision: u64,
    pub width: u64,
    pub capability_revision: u64,
    pub height: u64,
}
impl<K> MeasuredListItem<K> {
    pub const fn new(key: K, layout_revision: u64, width: u64, capability_revision: u64, height: u64) -> Self;
    pub const fn is_valid_for(&self, layout_revision: u64, width: u64, capability_revision: u64) -> bool;
}

pub enum MeasuredListError {
    DuplicateKey { position: usize },             // offending position in the supplied batch
    OutOfBounds { start: usize, end: usize, len: usize },
}

impl<K: Clone + Ord> MeasuredListIndex<K> {
    pub const fn new(gap: u64) -> Self;
    pub fn from_items(gap: u64, items: impl IntoIterator<Item = MeasuredListItem<K>>)
        -> Result<Self, MeasuredListError>;

    // Membership. A rejected mutation leaves the index unchanged.
    pub fn insert(&mut self, at: usize, items: impl IntoIterator<Item = MeasuredListItem<K>>)
        -> Result<(), MeasuredListError>;
    pub fn prepend(&mut self, items: impl IntoIterator<Item = MeasuredListItem<K>>)
        -> Result<(), MeasuredListError>;
    pub fn append(&mut self, items: impl IntoIterator<Item = MeasuredListItem<K>>)
        -> Result<(), MeasuredListError>;
    pub fn remove_range(&mut self, range: Range<usize>)
        -> Result<Vec<MeasuredListItem<K>>, MeasuredListError>;
    pub fn remove(&mut self, key: &K) -> Option<MeasuredListItem<K>>;
    pub fn splice(&mut self, range: Range<usize>, items: impl IntoIterator<Item = MeasuredListItem<K>>)
        -> Result<Vec<MeasuredListItem<K>>, MeasuredListError>;   // removed keys may be reinserted
    pub fn replace_range(&mut self, range: Range<usize>, items: impl IntoIterator<Item = MeasuredListItem<K>>)
        -> Result<Vec<MeasuredListItem<K>>, MeasuredListError>;   // alias of splice

    // Measurement updates.
    pub fn update_height(&mut self, key: &K, height: u64) -> bool;
    pub fn update_item(&mut self, item: MeasuredListItem<K>) -> bool;
    pub fn remeasure(&mut self, width: u64, capability_revision: u64, measure: impl FnMut(&K) -> u64) -> usize;
    pub fn sync(&mut self, entries: impl IntoIterator<Item = (K, u64)>, width: u64,
                capability_revision: u64, measure: impl FnMut(&K) -> u64);
    pub const fn set_gap(&mut self, gap: u64);

    // Queries.
    pub const fn is_empty(&self) -> bool;
    pub const fn gap(&self) -> u64;
    pub fn item(&self, index: usize) -> Option<&MeasuredListItem<K>>;
    pub fn item_by_key(&self, key: &K) -> Option<&MeasuredListItem<K>>;
    pub fn contains_key(&self, key: &K) -> bool;
    pub fn index_of(&self, key: &K) -> Option<usize>;
    pub fn key_offset(&self, key: &K) -> Option<u64>;
    pub fn item_offset(&self, index: usize) -> Option<u64>;
    pub fn total_height(&self) -> u64;
    pub fn item_at_offset(&self, offset: u64) -> Option<usize>;
    pub fn visible_range(&self, offset: u64, viewport_height: u64) -> VisibleItemRange;
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &K> + '_;
}
impl<K> MeasuredListIndex<K> {
    pub fn len(&self) -> usize;
    pub fn iter(&self) -> Iter<'_, K>;                     // in order
    pub fn iter_from(&self, index: usize) -> Iter<'_, K>;  // O(log n) start, amortized O(1) steps
}
```

`sync` remains the full-reconciliation entry point. It retains the unchanged
common prefix and suffix in place and splices only the differing middle,
measuring only new or invalidated items, so a prepend or append through `sync`
does not rebuild the index; comparing the supplied entries is still `O(n)`.
`remeasure` is the width and capability invalidation path and is inherently
`O(n)`. Equality compares gap and items in order, not tree shape. A
`resident_window::WindowSplice` maps directly onto `splice(splice.removed_range(),
measured inserted items)`.

`packages/tui/examples/measured_list_benchmark.rs` reports rebuild versus
incremental costs from 1,000 to 1,000,000 items.

### Position model

```rust,ignore
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AnchorOffset<S = ()> {
    /// Row within the anchored item (clamped to its extent on resolution).
    Row(u64),
    /// Caller source position inside the item plus rows below the row where
    /// it begins. Resolved through a `SourceMap`; unmapped falls back to
    /// `Row(row_delta)`.
    Source { position: S, row_delta: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListAnchor<K, S = ()> {
    pub key: K,
    pub offset: AnchorOffset<S>,
}
impl<K, S> ListAnchor<K, S> {
    pub const fn new(key: K, offset: AnchorOffset<S>) -> Self;
    pub const fn top(key: K) -> Self;              // Row(0)
    pub const fn row(key: K, row: u64) -> Self;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum ListPosition<K, S = ()> {
    /// First collection row on the viewport top row.
    #[default]
    Top,
    /// Last content row on the viewport bottom row (top row 0 when the content
    /// fits). Survives growth; released only by an explicit caller or user
    /// action, never by a content change.
    FollowBottom,
    /// Anchor row on the viewport top row, clamped to `0..=max_content_top`
    /// (no blank rows below content).
    Anchored(ListAnchor<K, S>),
    /// Anchor row on the viewport top row, clamped to `0..=max_overscroll_top`
    /// (blank tail rows below content permitted).
    Overscrolled(ListAnchor<K, S>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge { Top, Bottom }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AnchorPolicy {
    ItemTopAtTop,
    /// Item bottom on the viewport bottom; an item taller than the viewport is
    /// placed top-at-top instead, so its top is never pushed off.
    ItemBottomAtBottom,
    /// Item centered (floor); taller items are placed top-at-top.
    Center,
    /// No movement when the item is fully visible, or when a taller item covers
    /// the whole viewport. Otherwise `Edge::Top` places it like `ItemTopAtTop`
    /// and `Edge::Bottom` like `ItemBottomAtBottom`.
    EnsureVisibleFrom(Edge),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Overscroll {
    #[default]
    None,
    /// At most this many blank rows below the last content row.
    Rows(u64),
    /// At most `viewport_height - n` blank rows (`ViewportMinus(1)` lets the
    /// last content row reach the viewport top).
    ViewportMinus(u64),
}
impl Overscroll {
    pub const fn max_below(self, viewport_height: u64) -> u64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListGeometry {
    pub viewport_height: u64,
    pub overscroll: Overscroll,
}
impl ListGeometry {
    pub const fn new(viewport_height: u64) -> Self;                    // Overscroll::None
    pub const fn overscroll(self, overscroll: Overscroll) -> Self;
    /// `total_height - viewport_height`, saturating.
    pub const fn max_content_top(self, total_height: u64) -> u64;
    /// `max(max_content_top, min(total + max_below - viewport_height, total - 1))`,
    /// saturating, so at least one content row stays visible.
    pub const fn max_overscroll_top(self, total_height: u64) -> u64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResolvedPosition {
    pub top_offset: u64,
    pub viewport_height: u64,
    pub total_height: u64,
    pub max_content_top: u64,
    pub max_overscroll_top: u64,
}
impl ResolvedPosition {
    pub const fn rows(self) -> Range<u64>;                // top..top + viewport_height
    pub const fn blank_rows_below(self) -> u64;           // min(vh, top + vh - total), saturating
    pub const fn is_overscrolled(self) -> bool;           // top_offset > max_content_top
    pub const fn is_at_content_bottom(self) -> bool;      // top_offset >= max_content_top
    pub const fn edge_rows(self) -> resident_window::EdgeRows;
}

pub trait SourceMap<K, S> {
    /// Row inside item `key`, at the current width, where `position` begins.
    fn row_for_source(&self, key: &K, position: &S) -> Option<u64>;
    /// Source position covering `row` of item `key`, with the row where that
    /// position begins (at most `row`).
    fn source_for_row(&self, key: &K, row: u64) -> Option<(S, u64)>;
}
pub struct NoSourceMap;
impl<K, S> SourceMap<K, S> for NoSourceMap { /* both return None */ }

pub fn resolve_position<K: Clone + Ord, S>(
    index: &MeasuredListIndex<K>,
    position: &ListPosition<K, S>,
    geometry: ListGeometry,
    sources: &dyn SourceMap<K, S>,
) -> Option<ResolvedPosition>;           // None only when an anchor key is not indexed

pub fn anchor_at_offset<K: Clone + Ord, S>(
    index: &MeasuredListIndex<K>,
    top_offset: u64,
    sources: &dyn SourceMap<K, S>,
) -> Option<ListAnchor<K, S>>;           // None when offset >= total_height

pub fn position_at_offset<K: Clone + Ord, S>(
    index: &MeasuredListIndex<K>,
    top_offset: u64,
    geometry: ListGeometry,
    sources: &dyn SourceMap<K, S>,
) -> ListPosition<K, S>;                 // Top when empty; Overscrolled past max_content_top; never FollowBottom

pub fn target_top_offset<K: Clone + Ord>(
    index: &MeasuredListIndex<K>,
    key: &K,
    policy: AnchorPolicy,
    current_top: u64,
    geometry: ListGeometry,
) -> Option<u64>;                        // clamped to 0..=max_overscroll_top

pub fn anchor_for<K: Clone + Ord, S>(
    index: &MeasuredListIndex<K>,
    key: &K,
    policy: AnchorPolicy,
    current_top: u64,
    geometry: ListGeometry,
    sources: &dyn SourceMap<K, S>,
) -> Option<ListPosition<K, S>>;         // position_at_offset(target_top_offset(..))
```

Resolution is a pure function of the index, the position, the geometry, and the
source map: `Top` is row 0; `FollowBottom` is `max_content_top`; `Anchored` and
`Overscrolled` resolve their anchor row and clamp to `max_content_top` and
`max_overscroll_top` respectively. `anchor_at_offset` prefers a `Source` offset
when `source_for_row` maps the row and otherwise produces `Row`. These free
functions let a caller that owns its position (for example a navigation
reducer) derive geometry from the list's index without going through
`VirtualListState`.

### Controlled and uncontrolled state

`VirtualListState<K, S = ()>` gains the position model while keeping the
existing offset-based API working.

```rust,ignore
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PositionControl { #[default] Uncontrolled, Controlled }

impl<K: Clone + Ord> VirtualListState<K> {          // S = (), so `new` infers S
    pub fn new(gap: u64) -> Self;            // uncontrolled (existing behavior)
    pub fn controlled(gap: u64) -> Self;     // controlled mode
}
impl<K: Clone + Ord> Default for VirtualListState<K> { /* new(0) */ }

impl<K: Clone + Ord, S: Clone> VirtualListState<K, S> {
    pub fn with_control(gap: u64, control: PositionControl) -> Self;   // any source-position type S
    pub const fn is_controlled(&self) -> bool;
    pub const fn control(&self) -> PositionControl;

    pub const fn index(&self) -> &MeasuredListIndex<K>;
    pub const fn measurement_generation(&self) -> u64;     // changes whenever measured geometry changes
    pub fn set_overscroll(&mut self, overscroll: Overscroll);
    pub const fn overscroll(&self) -> Overscroll;
    pub const fn set_page_step(&mut self, page_step: PageStep);       // rows per PageRequested
    pub const fn page_step(&self) -> PageStep;
    pub const fn set_position_estimate(&mut self, estimate: Option<resident_window::PositionEstimate>);
    pub const fn position_estimate(&self) -> Option<resident_window::PositionEstimate>;

    pub fn set_position(&mut self, position: ListPosition<K, S>);
    pub fn position(&self) -> ListPosition<K, S>;
    pub fn release_follow(&mut self) -> bool;              // FollowBottom -> keyed position at the current top
    pub fn resolve(&self, viewport_height: u64, sources: &dyn SourceMap<K, S>) -> Option<ResolvedPosition>;
    pub fn resolve_layout(&mut self, viewport_height: u64, sources: &dyn SourceMap<K, S>) -> ResolvedPosition;
    pub const fn resolved(&self) -> Option<ResolvedPosition>;   // from the last layout
    pub const fn anchor_missing(&self) -> bool;                  // last layout could not resolve the anchor
    pub fn anchor_for(&self, key: &K, policy: AnchorPolicy, viewport_height: u64,
                      sources: &dyn SourceMap<K, S>) -> Option<ListPosition<K, S>>;

    pub fn apply_action(&mut self, action: &VirtualListAction<K>) -> bool;   // uncontrolled only
    pub fn apply_action_with(&mut self, action: &VirtualListAction<K>,
                             sources: &dyn SourceMap<K, S>) -> bool;
    pub fn dispatch_action(&mut self, action: VirtualListAction<K>,
                           sources: &dyn SourceMap<K, S>) -> ActionDispatch;  // Queued | Applied | Unchanged
    pub fn pending_actions(&self) -> &[VirtualListAction<K>];
    pub fn drain_actions(&mut self) -> std::vec::Drain<'_, VirtualListAction<K>>;

    // Membership changes reconciled by the next synchronization (measured via the source).
    pub fn note_splice(&mut self, removed: Range<usize>, inserted: usize);
    pub fn note_window_splice(&mut self, splice: &resident_window::WindowSplice<K>);
    pub fn invalidate(&mut self, key: &K);
    pub const fn invalidate_all(&mut self);

    // Membership changes with caller-measured exact heights, applied now.
    pub fn splice_measured(&mut self, range: Range<usize>, items: impl IntoIterator<Item = MeasuredListItem<K>>)
        -> Result<Vec<MeasuredListItem<K>>, MeasuredListError>;
    pub fn update_measured(&mut self, item: MeasuredListItem<K>) -> bool;

    pub fn render_stats(&self) -> VirtualListRenderStats;
}
```

`new` and `controlled` live on `VirtualListState<K>` (`S = ()`) so existing
`VirtualListState::new(gap)` call sites keep inferring their type; lists with a
source-position type use `with_control`. `render_stats` is not `const` because
its counters are atomics, which keep the state `Sync` while `&self` paint and
event paths update them.

- **Controlled.** The caller owns the position. `set_position` stores it; the
  list never changes it. Wheel, page, key, scrollbar, and selection-autoscroll
  input only queue `VirtualListAction`s (`dispatch_action` returns `Queued`),
  drained with `drain_actions`; `apply_action` does nothing. Membership and
  height changes never move a controlled position; its rows are re-derived at
  the next layout. `resolve`, `anchor_for`, and the free functions compute
  geometry without mutating anything. If the anchor key is not indexed at
  layout, the list reuses the previous top offset clamped to the new geometry
  and sets `anchor_missing`; correcting the position is caller policy.
- **Uncontrolled.** The list applies its own actions through `apply_action`
  (`dispatch_action` applies everything except `Activated`, which is queued).
  Actions move relative to the most recent layout, re-resolved at the last
  viewport height when content or the position changed since. A downward move
  ending exactly at `max_content_top` enters `FollowBottom`; ending beyond it
  enters `Overscrolled`; any upward request releases `FollowBottom`.
  `EdgeRequested(Top)` moves to `Top`, `EdgeRequested(Bottom)` to
  `FollowBottom`. `ScrollbarDrag` fractions are relative to the position
  estimate when one is set and its `resident` equals the exact total
  (non-resident targets clamp to the resident edges), otherwise to
  `max_content_top`. When that active estimate reports rows after the resident
  content, reaching `max_content_top` (by scrolling, dragging, or
  `EdgeRequested(Bottom)`) anchors there instead of entering `FollowBottom`,
  because the resident bottom is not the true tail. A drag whose target lies
  before or after the resident rows, or an edge request toward an edge with
  estimated rows beyond it, is applied (clamped to the resident edge) and also
  queued, so the caller can load pages or replace the window. A removed anchor remaps to the item now at the anchor's
  last resolved index, adjusted for noted splices before it (the next
  survivor, or the last item when the collection shrank past it), keeping the
  row clamped to that item; an empty collection resolves to `Top`.
- **Legacy offset API.** The `scroll` field, `capture_anchor`,
  `restore_anchor`, `scroll_to_key`, `ensure_key_visible`, `scroll_by`,
  `scroll_to_top`, `scroll_to_bottom`, and `clamp_scroll` keep their behavior.
  In uncontrolled mode an offset or follow flag written through them is adopted
  as the keyed position (`position_at_offset` at the geometry it was written
  in, or `FollowBottom` for a set follow flag) before the next synchronization,
  position query, or action, so the two APIs stay consistent. The legacy
  maximum offset is `max_overscroll_top`, which equals the previous
  `total_height - viewport_height` with the default `Overscroll::None`.
  Controlled lists ignore legacy offset writes.
- `resolve_layout` resolves the position for a viewport, applies the
  missing-anchor fallback above, records `resolved`, and mirrors the top offset
  and follow flag into `scroll`. In both modes `scroll.vertical_offset()`
  mirrors the last resolved top offset after each layout, for read-only legacy
  consumers.
- `VirtualListRenderStats` gains `built_items` and `measured_items` next to
  `painted_items` and `registered_items`. `render_stats` reports counters since
  the most recent synchronization: measurement work from that synchronization,
  construction during later paints and events, and the most recent paint's
  painted and registered items.

### Lazy item sources

```rust,ignore
pub trait VirtualItemSource<K> {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool { self.len() == 0 }
    fn key(&self, index: usize) -> K;
    fn layout_revision(&self, index: usize) -> u64;
    fn paint_revision(&self, index: usize) -> u64 { 0 }
    fn build(&self, index: usize) -> Element<'_>;
    /// A caller-retained exact layout for this item at `width` and
    /// `environment`. When `Some`, the list uses it for measurement, painting,
    /// and events, and never calls `build` to measure this item.
    fn retained_layout(&self, index: usize, width: u64, environment: LayoutEnvironment)
        -> Option<&LayoutNode> { None }
    /// Whether every synchronization must compare all keys and revisions
    /// (sources rebuilt per frame that do not note changes). Default false.
    fn always_reconcile(&self) -> bool { false }
}
impl<K, T: VirtualItemSource<K> + ?Sized> VirtualItemSource<K> for &T { /* delegates */ }

pub struct VirtualSourceList<'a, Src: ?Sized> { /* private */ }
impl<'a, Src: ?Sized> VirtualSourceList<'a, Src> {
    pub fn new(id: impl Into<Cow<'a, str>>, source: &'a Src) -> Self;
    pub const fn pointer_intent(self, intent: PointerIntent) -> Self;
    // Each method below requires K: Clone + Ord + ToString, S: Clone, Src: VirtualItemSource<K>.
    pub fn sync<K, S>(&self, width: u64, state: &mut VirtualListState<K, S>, cx: &mut LayoutCx);
    pub fn sync_with_environment<K, S>(&self, width: u64, environment: LayoutEnvironment,
                                       state: &mut VirtualListState<K, S>, cx: &mut LayoutCx);
    pub fn layout<K, S>(&self, width: u64, environment: LayoutEnvironment, viewport_height: u64,
                        sources: &dyn SourceMap<K, S>, state: &mut VirtualListState<K, S>,
                        cx: &mut LayoutCx) -> VirtualListLayout;   // { resolved, visible }
    pub fn paint<K, S>(&self, area: Rect, state: &VirtualListState<K, S>,
                       cx: &mut PaintCx<'_, '_>) -> VirtualListRenderStats;
    pub fn event<K, S>(&self, area: Rect, state: &VirtualListState<K, S>, event: &Event,
                       cx: &mut EventCx<'_>) -> EventOutcome;
}
```

Source order equals index order. The construction contract is:

- `build(i)` is called only to measure an item that is new, has a changed
  layout revision, or is stale for the current width or environment and has no
  `retained_layout`; to give a visible item that has no retained layout one
  (for example after `splice_measured`); and once per paint or event dispatch
  for each visible item (construction only, the retained layout is reused). It
  is never called for other items. There is no construction overscan: every
  item is measured exactly, and only visible items are built for painting.
- `key(i)` and `layout_revision(i)` are read for items covered by noted splices
  and invalidations, for visible items, and during a full reconciliation.
  `paint_revision(i)` is read only for visible items and feeds the component's
  paint revision.
- `invalidate(key)` means "re-read this item's revision": the item is
  re-measured only when its revision, width, or environment is stale.
  `invalidate_all` schedules a full comparison that likewise measures only
  stale items.
- Noted splices are composed into sorted, disjoint dirty regions (up to 16;
  beyond that they merge into one span), so a prepend and an append noted in
  the same frame, or an eviction trimming both ends, read only the noted rows.
  Regions are applied as `O(k + log n)` index removals and insertions; removed
  keys that reappear in any region with a valid measurement are reused, not
  rebuilt.
- Full `O(n)` reconciliation happens only after `invalidate_all`, after a width
  or environment change (every resident item is re-measured), for
  `always_reconcile` sources, when noted changes disagree with the source, or
  when the source length disagrees with the index without noted changes
  (debug-asserted, then reconciled). Mixing `splice_measured` with pending
  noted splices also schedules one full reconciliation. Steady scrolling
  performs no reconciliation.
- `layout` re-reads visible items' keys and revisions, re-measures a stale
  visible item even without `invalidate`, and re-resolves the position when
  that changes a height, repeating until the visible range is stable. Each item
  is measured at most once per layout, which bounds the passes, so every item
  in the returned visible range has an exact retained layout. A visible key
  that disagrees with the index triggers one full reconciliation within the
  same layout. Exact measurement wins over caller-measured heights for visible
  items.
- Paint and event use an item only when the source still holds the same key
  and layout revision at its position (adjusted for splices noted since the
  last synchronization) and its retained layout exists. Between a noted splice
  and the next layout, events therefore reach the item that was laid out, and
  a changed or replaced item is skipped rather than painted or routed with
  another item's layout.
- Retained layouts of removed keys are released in amortized batches once
  their number exceeds the collection length (minimum 64), so retained memory
  tracks the collection.

`VirtualSourceList` is the shared measurement, painting, and event
implementation. The eager `VirtualList` builder implements `VirtualItemSource`
(with `always_reconcile`) and delegates to it, so the builder and lazy paths
share one implementation; `VirtualList::layout` mirrors
`VirtualSourceList::layout`. `bmux_tui_components::virtual_list` also
re-exports `MeasuredListIndex`, `MeasuredListItem`, `MeasuredListError`,
`VisibleItemRange`, and the `resident_window` types its signatures use
(`EdgeRows`, `PositionEstimate`, `FractionTarget`, `WindowSplice`).

### VirtualListComponent

```rust,ignore
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualListChrome {
    pub scrollbar: ScrollbarAxisLayoutMode,   // Hidden | Overlay | Gutter | OverlayOnOverflow
    pub scrollbar_policy: ScrollbarPolicy,    // glyphs, minimum thumb, drag; always vertical
    pub scrollbar_styles: ScrollbarStyles,
    pub keyboard: bool,
    pub mouse_wheel: bool,
    pub wheel_rows: u16,
    pub page: PageStep,
}
impl VirtualListChrome {
    pub const fn interactive() -> Self;   // Gutter, keyboard, wheel 3, PageStep::Viewport (Default)
    pub const fn bare() -> Self;          // Hidden, no keyboard, no wheel
    pub const fn scrollbar(self, mode: ScrollbarAxisLayoutMode) -> Self;
    pub const fn scrollbar_policy(self, policy: ScrollbarPolicy) -> Self;
    pub const fn scrollbar_styles(self, styles: ScrollbarStyles) -> Self;
    pub const fn keyboard(self, enabled: bool) -> Self;
    pub const fn mouse_wheel(self, enabled: bool) -> Self;
    pub const fn wheel_rows(self, rows: u16) -> Self;
    pub const fn page(self, page: PageStep) -> Self;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PageStep { #[default] Viewport, ViewportMinus(u64), Rows(u64) }
impl PageStep { pub const fn rows(self, viewport_height: u64) -> u64; } // at least 1

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollInputSource { Wheel, Key }

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum VirtualListAction<K> {
    ScrollRequested { rows: i64, source: ScrollInputSource },  // negative = toward row 0
    PageRequested(i32),                                         // negative = toward row 0
    EdgeRequested(Edge),                                        // Home / End
    ScrollbarDrag(f64),          // 0.0..=1.0 of the drawn scrollbar model
    SelectionAutoScroll(i64),    // signed rows from a vertical SelectionAutoScrollRequest
    Activated(K),                // click on an item that did not handle it
}

pub struct VirtualListComponent<'a, K, Src: ?Sized, S = ()> { /* private */ }
impl<'a, K, Src, S> VirtualListComponent<'a, K, Src, S>
where
    K: Clone + Ord + ToString,
    Src: VirtualItemSource<K> + ?Sized,
    S: Clone,
{
    pub fn new(id: impl Into<LayoutId>, source: &'a Src,
               state: &'a RefCell<VirtualListState<K, S>>) -> Self;
    pub const fn chrome(self, chrome: VirtualListChrome) -> Self;
    pub const fn sources(self, sources: &'a dyn SourceMap<K, S>) -> Self;
    pub const fn position_estimate(self, estimate: resident_window::PositionEstimate) -> Self;
    pub const fn environment(self, environment: LayoutEnvironment) -> Self;
    pub const fn pointer_intent(self, intent: PointerIntent) -> Self;
    pub fn content_id(&self) -> LayoutId;      // "{id}.content"
    pub fn window_id(&self) -> LayoutId;       // "{id}.window"
    pub fn scrollbar_id(&self) -> LayoutId;    // "{id}.scrollbar"
    pub fn item_id(&self, key: &K) -> LayoutId; // "{id}.item.{key}"
}
impl<K, Src, S> Component for VirtualListComponent<'_, K, Src, S> { /* layout, paint, event, revision */ }

impl<K: Clone + Ord, S: Clone> VirtualListState<K, S> {
    /// Route a vertical edge-autoscroll request from the selection controller
    /// as `SelectionAutoScroll`. Returns false for horizontal requests and
    /// disabled lists.
    pub fn handle_selection_auto_scroll(&mut self, request: &SelectionAutoScrollRequest) -> bool;
}
```

`PageStep`, `ScrollInputSource`, `VirtualListAction`, and `ActionDispatch` are
defined with the state, because `apply_action` and `dispatch_action` consume
them. The component dispatches every action through
`VirtualListState::dispatch_action`, and before dispatching it keeps
`set_page_step(chrome.page)` and `set_position_estimate(..)` current, so
uncontrolled pages and drags use the same model that is drawn. Keyboard input
is accepted while the caller sets `state.scroll.interaction.focused`;
`state.scroll.interaction.disabled` ignores all input and releases capture.

- **Layout.** The viewport takes the constraint size (the content height when
  height is unbounded). A `Gutter` scrollbar always reserves one column, so the
  content width never depends on overflow. Layout reconciles the state against
  the source (noted changes, invalidations, width and environment) through
  `VirtualSourceList::layout`, resolves the position, and returns this tree:
  - the viewport node (the component id, semantic `"list"`), whose first child
    is the content node `"{id}.content"` at the origin, width × `total_height`
    in collection rows, as for `ScrollViewComponent::viewport_layout`;
  - inside the content node, one leaf wrapper per visible item, identified
    `"{id}.item.{key}"`, at the item's exact collection row and with its exact
    measured size, followed by the window node `"{id}.window"`
    whose row is the resolved top offset and whose height is the viewport, so
    the visible region is part of the authoritative geometry;
  - the scrollbar node `"{id}.scrollbar"` at its track rectangle, when enabled.

  Item layouts are not copied into the tree: the retained layout stays in the
  source (`retained_layout`) or the state's layout cache, is borrowed at paint,
  and is cloned only for an item an event is actually offered to, so layout
  cost does not grow with the size of visible items' layouts. To locate a node
  nested inside an item, add the wrapper's rectangle to the item's retained
  layout geometry.

  Layout also records a snapshot (resolved position, placed range, content and
  scrollbar rectangles, measurement generation) in the state. In controlled
  mode it never changes the position.
- **Paint and event** consume the node they are given. A node that does not
  match the recorded snapshot (size, window row, total height, placed items) or
  whose index changed since layout is stale: it paints nothing and ignores
  events, except that a release still ends drag capture. Placed items are
  painted and routed with their exact retained layouts, through the same
  per-item routine and source-identity check as `VirtualSourceList`, so only
  placed items within the paint clip
  are built, painted, and register hit, focus, semantic, selection, image, and
  damage contributions. Item handlers run without the state borrowed. Blank
  overscroll rows paint nothing (the parent background shows) and register
  nothing. The container registers a `HitRole::Scroll` region (focusable with
  `keyboard`, pointer events with `mouse_wheel`) and a `"list"` semantic region
  under the items.
- **Event order.** A press on the scrollbar track or thumb starts a drag that
  reports `ScrollbarDrag`; a captured drag continues outside the track until
  release. Other mouse events go to the visible item under the pointer first,
  and an unhandled wheel anywhere in the viewport becomes `ScrollRequested`
  with `wheel_rows`. Keys and other non-positional events are offered to
  visible items bottom to top; then, with `keyboard` and focus, Up/Down map to
  `ScrollRequested { ±1, Key }`, PageUp/PageDown to `PageRequested(∓1/±1)`, and
  Home/End to `EdgeRequested`. A left press and release on the same item with
  no intervening drag, unhandled by the item, reports `Activated(key)` (queued
  in both modes). Outcomes: `Queued` returns `Handled`; `Applied` returns
  `Redraw`; navigation that changes nothing returns `Ignored` so an enclosing
  scroller can take it (scrollbar drags stay `Handled`).
- **Scrollbar** geometry uses `PositionEstimate::scrollbar_state(top, viewport)`
  when an estimate is supplied and its `resident` equals the index total, and
  the exact total otherwise; overscroll pins the thumb at the content bottom.
  Both models scale into the scrollbar's `u16` cells with the same mapping
  `ScrollView` uses, and a drag cell is inverted to a logical row with
  `ScrollView`'s integer inverse; the reported `ScrollbarDrag` fraction is that
  row over the model's logical maximum, so applying it recovers the row
  exactly and the same drag lands on the same row in both components. Uncontrolled lists map a fraction with
  `PositionEstimate::target_for_fraction` and clamp non-resident targets to the
  resident edges. Thumb drawing, track clicks, and capture reuse the shared
  `Scrollbar` primitive and the vertical-track routine used by `ScrollView` and
  `TrackScrollbar`.
- **Selection autoscroll** reuses `ScrollView`'s request-to-rows mapping
  (intensity, at least one row, toward the request direction).
- **Revision.** The layout channel combines the id, scrollbar mode,
  environment, measurement generation, pending changes, source length,
  overscroll, resolved top offset and follow state, estimate, and visible
  items' keys and layout revisions (every item for `always_reconcile`
  sources); the paint channel combines pointer intent, chrome, scroll and
  interaction state, and visible items' paint revisions.

A lazy list in a frame, with uncontrolled state the list scrolls itself:

```rust,ignore
struct Log<'a>(&'a [Entry]);
impl VirtualItemSource<u64> for Log<'_> {
    fn len(&self) -> usize { self.0.len() }
    fn key(&self, index: usize) -> u64 { self.0[index].id }
    fn layout_revision(&self, index: usize) -> u64 { self.0[index].revision }
    fn build(&self, index: usize) -> Element<'_> { Element::new(EntryView(&self.0[index])) }
}

// Program state: entries plus `state: RefCell<VirtualListState<u64>>`.
// On append, note the change instead of re-reading the whole source:
state.get_mut().note_splice(len..len, appended);

// Each frame:
let source = Log(&entries);
let list = VirtualListComponent::new("log", &source, &state)
    .chrome(VirtualListChrome::interactive().page(PageStep::ViewportMinus(1)));
let layout = list.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
list.paint(&layout, cx);
// Route later input against that committed layout:
let outcome = list.event(&event, &layout, &mut EventCx::new(&layout));
```

A controlled list keeps the position in caller state; input only queues
actions, which the caller turns into positions by its own policy:

```rust,ignore
let state = RefCell::new(VirtualListState::controlled(0));
// ... event routed as above returns Handled ...
for action in state.borrow_mut().drain_actions() {
    match action {
        VirtualListAction::EdgeRequested(Edge::Bottom) => model.follow = true,
        VirtualListAction::Activated(key) => model.open(key),
        other => model.pending_scroll.push(other),
    }
}
state.borrow_mut().set_position(model.position());   // before the next layout
```

`packages/tui-components/examples/virtual_list_chat.rs` is a runnable chat log
on the BMUX runtime: streaming appends that stay followed, history pages
prepended as `ResolvedPosition::edge_rows` reports the top edge approaching
(the viewport stays still because positions are keyed), keyed motion driven
by runtime timers, overscroll, and activation.

### Scroll motion

```rust,ignore
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScrollTarget<K, S = ()> {
    Key(K, AnchorPolicy),        // completes at anchor_for(..)
    Top,                         // completes at ListPosition::Top
    Bottom,                      // completes at ListPosition::FollowBottom
    Anchor(ListAnchor<K, S>),    // anchor row on the viewport top; may land in overscroll
}
impl<K: Clone + Ord, S: Clone> ScrollTarget<K, S> {
    pub fn resolve_top(&self, index: &MeasuredListIndex<K>, current_top: u64,
                       geometry: ListGeometry, sources: &dyn SourceMap<K, S>) -> Option<u64>;
    pub fn final_position(&self, index: &MeasuredListIndex<K>, current_top: u64,
                          geometry: ListGeometry, sources: &dyn SourceMap<K, S>) -> Option<ListPosition<K, S>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollMotion<K, S = ()> {
    pub from: ListPosition<K, S>,     // keyed visual start position
    pub target: ScrollTarget<K, S>,
    pub easing: Easing,               // bmux_tui::style::Easing
    pub duration: Duration,
    pub started: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MotionSample { pub top_offset: u64, pub done: bool }

impl<K: Clone + Ord, S: Clone> ScrollMotion<K, S> {
    pub const fn new(from: ListPosition<K, S>, target: ScrollTarget<K, S>, easing: Easing,
                     duration: Duration, started: Instant) -> Self;
    pub fn sample(&self, now: Instant, index: &MeasuredListIndex<K>, geometry: ListGeometry,
                  sources: &dyn SourceMap<K, S>) -> Option<MotionSample>;
    pub fn sample_elapsed(&self, elapsed: Duration, index: &MeasuredListIndex<K>,
                          geometry: ListGeometry, sources: &dyn SourceMap<K, S>) -> Option<MotionSample>;
    pub fn position_at(&self, now: Instant, index: &MeasuredListIndex<K>, geometry: ListGeometry,
                       sources: &dyn SourceMap<K, S>) -> Option<ListPosition<K, S>>;
    pub fn is_done(&self, now: Instant) -> bool;
    pub fn deadline(&self) -> Instant;
    pub fn next_frame(&self, now: Instant, frame_interval: Duration) -> Option<Instant>;
}

impl<K: Clone + Ord, S: Clone> VirtualListState<K, S> {   // uncontrolled only
    pub fn start_motion(&mut self, target: ScrollTarget<K, S>, easing: Easing, duration: Duration, now: Instant);
    pub fn advance_motion(&mut self, now: Instant, frame_interval: Duration,
                          sources: &dyn SourceMap<K, S>) -> Option<Instant>;   // next frame, None when idle
    pub fn cancel_motion(&mut self) -> bool;
    pub const fn motion(&self) -> Option<&ScrollMotion<K, S>>;
}
```

- Both endpoints are keyed and re-resolved against the current index and
  geometry at every sample, so content inserted, removed, or reflowed during a
  motion retargets it instead of stranding it at stale rows.
  `EnsureVisibleFrom` targets resolve relative to the sampled start. A start
  anchor whose key disappeared starts at the target; a target key that
  disappeared makes `sample` and `position_at` return `None`.
- `top = from + (target − from) × easing.progress(elapsed, duration)`, computed
  in integer arithmetic with progress quantized to 1/65536 and rounded toward
  the start, monotonic toward the target. At or after the deadline the sample
  is exactly the target and `done` is true; a zero duration is done
  immediately (reduce-motion). An unrepresentable deadline saturates.
- `position_at` materializes the visual position as a keyed position
  (`position_at_offset` before completion, `final_position` after), for
  committing a frame or interrupting a motion where it is. Completion commits
  the target's exact keyed position, which layout then resolves.
- `next_frame` is one `frame_interval` after `now`, capped at the deadline,
  and `None` once done; a zero interval schedules only the deadline.
- No BMUX code reads a wall clock. `Instant` is `std::time::Instant` on native
  targets and `web_time::Instant` on `wasm32`. Callers pass runtime time
  (`RuntimeHandle::now`) and schedule the next frame with a keyed timer while
  `advance_motion` returns `Some`, so the deterministic harness's `ManualClock`
  controls every sample.
- State helpers serve uncontrolled lists (controlled lists ignore them).
  `start_motion` starts from the position committed by the most recent frame,
  so a motion started during another continues from where that one is; a zero
  duration commits the target immediately. `advance_motion` commits the frame
  for `now` at the latest layout's viewport height and returns the next frame
  time. Every applied action except `Activated`, `set_position`, and legacy
  offset writes cancel a running motion at its last committed frame.
- Controlled callers own their `ScrollMotion`: they sample it and call
  `set_position(motion.position_at(now, ..))` before layout, and cancel it on
  queued input by their own policy.

Driving a motion from a runtime `Program` with a keyed timer:

```rust,ignore
const MOTION: &str = "log.motion";

fn drive(&mut self) {
    let timer = TimerId::new(MOTION);
    let now = self.handle.now();
    match self.state.get_mut().advance_motion(now, Duration::from_millis(16), &NoSourceMap) {
        Some(next) => self.handle.schedule_timer(timer, next),
        None => { let _ = self.handle.cancel_timer(&timer); }
    }
}

// update():
RuntimeEvent::Message(Message::Jump(key)) => {
    let now = self.handle.now();
    let duration = if self.reduce_motion { Duration::ZERO } else { Duration::from_millis(200) };
    self.state.get_mut().start_motion(
        ScrollTarget::Key(key, AnchorPolicy::Center), Easing::EaseOut, duration, now);
    self.drive();
    Ok(Update::redraw())
}
RuntimeEvent::Timer(id) if id.as_str() == MOTION => { self.drive(); Ok(Update::redraw()) }
```

`packages/tui-components/tests/virtual_list_motion.rs` drives such a program
through `bmux_tui_runtime::testing::ProgramHarness` and asserts the committed
frame at each manual-clock step, including retargeting after streamed
appends, cancellation by wheel input, and zero-duration motion.

### Resident windows

`bmux_tui_components::resident_window` (feature `resident-window`) is pure
bookkeeping for a list whose keys are a contiguous, page-loaded window over a
longer ordered sequence. It performs no I/O and owns no timers: the caller
executes the page and replacement requests it reports and feeds results back.

- Boundary demands come from exact measured rows beyond each viewport edge
  and caller prefetch distances, with at most one in-flight page per side and
  one replacement at a time.
- Every request carries a `RequestToken`. Cancelled, superseded, replayed, or
  edge-mismatched results are rejected without changing the window.
- Page, replacement, and eviction results are keyed `WindowSplice`s that the
  caller applies to its list and `MeasuredListIndex`, so keyed anchors survive.
- Eviction never removes the anchor, caller-pinned keys, or the protected band
  around the viewport, and cuts only at caller-declared window starts.

```rust,ignore
pub enum WindowSide { Before, After }
pub enum EdgeAvailability { MoreAvailable, Exhausted }
pub enum BoundaryStatus { MoreAvailable, Loading, Exhausted }
pub struct ResidentItem<K> { pub key: K, pub weight: u64 }
pub struct EdgeRows { pub before: u64, pub after: u64 }
pub struct PrefetchPolicy { pub before_rows: u64, pub after_rows: u64,
                            pub ceiling: Option<WeightCeiling>, pub reversal: ReversalPolicy }
pub struct WindowSplice<K> { pub at: usize, pub removed: Vec<K>, pub inserted: Vec<K> }
pub enum JumpTarget<P> { Around(P), Head, Tail }
pub struct ViewportPlacement<'a, K> { pub anchor: &'a K, pub top_from_anchor: i64, pub height: u64 }
pub struct ProtectedBand { pub before_rows: u64, pub after_rows: u64,
                           pub max_items: usize, pub max_weight: Option<u64> }
pub struct EvictionBudget { pub trigger_weight: u64, pub target_weight: u64, pub sides: EvictionSides }
pub struct PositionEstimate { pub before: u64, pub resident: u64, pub after: u64 }

impl<K: Clone + Ord> ResidentWindow<K> {
    pub fn boundary_signals(&self, rows: EdgeRows, policy: &PrefetchPolicy) -> BoundarySignals;
    pub fn poll_demands(&mut self, rows: EdgeRows, policy: &PrefetchPolicy) -> BoundaryDemands<K>;
    pub fn apply_page(&mut self, token: RequestToken, result: PageResult<K>)
        -> Result<PageApplied<K>, StaleResult>;
    pub fn request_replace<P>(&mut self, target: JumpTarget<P>) -> ReplaceRequest<P>;
    pub fn apply_replace(&mut self, token: RequestToken, result: ReplaceResult<K>)
        -> Result<ReplaceApplied<K>, StaleResult>;
    pub fn plan_eviction<R: ItemRows<K> + ?Sized>(&self, input: &EvictionInput<'_, K, R>)
        -> Result<EvictionPlan, EvictionError>;
    pub fn apply_eviction(&mut self, plan: &EvictionPlan) -> Result<Vec<WindowSplice<K>>, EvictionError>;
    pub fn position_estimate(&self, resident_rows: u64, rows_per_weight: f64) -> PositionEstimate;
}
```

The module's rustdoc is authoritative for the complete API. The list connects
to it at three points: `ResolvedPosition::edge_rows` supplies `EdgeRows`,
`MeasuredListIndex::key_offset` of the anchor supplies
`ViewportPlacement::top_from_anchor`, and each `WindowSplice` is applied with
`VirtualListState::note_window_splice` (or measured by the caller and applied
with `splice_measured`) in the same update that received the result.

Exact measurement still governs every resident item. `PositionEstimate` is the
one exception to the estimate rule above, and only for content that is not
resident at all: it extrapolates rows before and after the window from
caller-supplied `PositionHints` and a rows-per-weight density. A window capped
for boundedness over an unbounded sequence cannot measure rows it has not
loaded. The estimate is not an estimated item height. It is presentation-only
scrollbar and fraction-jump geometry and must not drive navigation, anchoring,
or hit testing.

### Typical compositions

The mechanics above combine into common list shapes without product-specific
code in BMUX:

| Use case | Composition |
| --- | --- |
| Chat or log view that follows new output | Uncontrolled `VirtualListComponent`, `ListPosition::FollowBottom`, appends noted with `note_splice` |
| Caller-owned navigation policy | Controlled state, `drain_actions`, `anchor_for` / `resolve_position`, `set_position` |
| Short content that can scroll its last row to the top | `Overscroll::ViewportMinus(1)` and `ListPosition::Overscrolled` |
| Animated reveal or jump | `ScrollMotion` / `ScrollTarget` driven by runtime timers |
| Very long or mutable collections | `MeasuredListIndex` splices, `VirtualItemSource` with `retained_layout` or `splice_measured` |
| History larger than memory | `resident_window` paging and eviction replayed through `note_window_splice`, with `PositionEstimate` for the scrollbar |

## Images and damage

Images are scene contributions with stable keys, logical placement, payload,
and lifecycle. They use the same transforms and clips as cells. Presentation
diffs committed image scenes so moved, replaced, hidden, and removed images are
handled consistently with terminal output.

Damage is registered through scoped paint contexts. Full and regional damage
must agree with visual output and all metadata channels. Wide glyphs and image
placements are clipped atomically at visible boundaries.

## Reusable controls

Reusable interactive behavior belongs in `bmux_tui_components`, including
buttons, inputs, panes, dialogs, menus, selectable collections, scrollbars,
scroll views, virtual lists, viewers, and image-adjacent controls. Controls own
no application domain and receive caller-owned state and policy.

Feature-gated controls must preserve dependency isolation. Developer-tool
source and diff views belong in the component crate rather than introducing
VCS or coding-agent concepts into `bmux_tui`.

## Terminal presentation

`Buffer` and `Frame` are backend-facing staging types. `Frame` exposes only
read accessors publicly; every cell, cursor, hit, focus-scope, image,
semantic, selection, and damage contribution is written through `PaintCx`,
including from the root `Terminal::draw` and presenter callbacks, so
translation and clipping are never bypassed.
ANSI output performs retained cell diffing, cursor projection, and terminal
writes. Runtime code owns terminal setup/restoration and presentation timing,
not component layout.

## Performance contract

Performance is demonstrated structurally as well as with elapsed time:

- retained layout cache hits and misses;
- measured nodes and items;
- painted nodes and items;
- interaction registrations;
- damaged cells or regions;
- allocations and emitted frame bytes.

Steady repaint reuses valid measurement. Virtual-list visible-range lookup is
sublinear, and steady scrolling paints/registers only visible content.
Measured-collection membership changes are logarithmic per changed item; only
width or environment invalidation and explicit full reconciliation are linear.
Width or layout revision changes invalidate exactly the geometry they affect; a
paint-only revision must not force measurement.

## Validation

Focused framework work should run the affected crate tests first. Repository
completion follows `AGENTS.md`, including formatting, warning-free clippy,
nextest, dependency hygiene, CLI checks/tests, and relevant PTY/runtime smoke
commands. Changes to terminal protocol, query/reply, TERM, or profile behavior
also require the compatibility matrix. Plugin source changes require rebuilding
workspace plugins.

Architecture checks must continue to prove that core TUI and runtime layers are
domain-neutral and that deleted area-assigned rendering APIs, rendered-row
caches, and unrestricted component buffer access are not reintroduced.
