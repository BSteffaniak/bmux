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
- `TextBlock` provides measurable rich text and source projection;
- `ScrollViewport` applies caller-owned logical offsets through scoped
  translation and clipping.

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
during scene construction. `Frame::paint_selection` is the deterministic visual
overlay stage after ordinary content painting. Copying reads the logical
selection snapshot rather than scraping the terminal buffer.

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
layout and one caller-owned `ScrollViewState`.

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
- caller actions use `scroll_by`, `scroll_to_top`, and `scroll_to_bottom` rather
  than mutating or recomputing row offsets outside the collection state;
- a retained layout is reused for painting instead of being remeasured there.

Exact measurement is the correctness model. Estimated heights are not part of
the public contract unless measured evidence establishes that exact retained
measurement cannot satisfy the supported scale.

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

`Buffer` and `Frame` are backend-facing staging types. Ordinary components
paint through `PaintCx`; unrestricted buffer mutation is not a component API.
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
sublinear, and steady scrolling paints/registers only visible content. Width or
layout revision changes invalidate exactly the geometry they affect; a
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
