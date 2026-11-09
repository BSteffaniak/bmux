# Component examples

## Selectable lists

For selection-oriented controls such as pickers, use
`bmux_tui_components::selectable_list::SelectableListComponent` with the
`selectable-list` feature. Supply a stable component ID, a borrowed slice of
`SelectableListItem`, and a caller-owned `Cell<SelectableListState>`. Keep item
IDs stable across updates; visible item hit and semantic IDs are namespaced by
the component ID.

Configure behavior with `policy`, visuals with `styles`, and an optional row
fill override with `fallback_style`. Place the component in the ordinary
`Component` layout tree and paint its resolved layout through `PaintCx` rather
than assigning a terminal rectangle to the legacy list renderer. The component
uses shared `ScrollView` geometry for its viewport and content.

Route input using the resolved layout and `EventCx`. Use `handle_event` when the
application needs `SelectableListOutcome::Focused` or `Selected` to perform a
product action. The generic `Component::event` entry point maps these outcomes
to `EventOutcome::Redraw`; it does not deliver the selected item to application
logic. Both routes update the supplied state cell, so retain that cell across
frames or copy its value back to the application's authoritative state after
dispatch. Do not dispatch the same event through both routes.

This control is not a replacement for large variable-height collection
virtualization. Use `VirtualList` and keyed child components for that case, as
demonstrated below. Also verify the resolved dependency revision contains
`SelectableListComponent` before migrating a downstream consumer; the API in
this workspace may be newer than its lockfile.

## Variable-height virtual list benchmark

This executable is both the structural performance baseline and the public
large-collection example for `bmux_tui_components::VirtualList`.

It builds stable-key item components with mixed exact heights and runs the
canonical lifecycle:

1. Build a keyed `VirtualList` whose children implement `Component`.
2. Call `sync` at the current width so `VirtualListState` retains authoritative
   exact layouts by key, layout revision, width, and environment.
3. Keep logical scrolling and anchor state in the caller-owned
   `VirtualListState`. Use `scroll_by` for row/page movement, `scroll_to_top`
   and `scroll_to_bottom` for Home/End behavior, and capture/sync/restore around
   keyed mutations so top or bottom anchoring reconciles against exact geometry.
4. Paint through a clipped `PaintCx`; only viewport-intersecting items may paint
   or register hits, focus, semantics, selection, images, and damage.

The benchmark exercises 100, 1,000, and 10,000 mixed-height items and reports
latency together with measured-node, painted-item, allocation, metadata,
damage, frame-byte, and layout-cache counters. Its assertions demonstrate:

- unchanged layout performs zero remeasurement;
- one-row and one-page scrolling paint only visible items;
- appending while bottom-following preserves the bottom anchor;
- insertion, removal, and reorder preserve the stable top key and intra-item
  row;
- paint-only revisions do not invalidate geometry;
- width reflow remeasures exact width-dependent layouts and restores the stable
  semantic anchor.

A composed-card scenario at the same collection sizes uses padded `Surface`
children containing a `Column` with an author and wrapped Unicode message body.
It asserts unchanged-layout reuse, viewport-bounded painting and registration,
exact offset restoration after 64 downward and 64 upward one-row steps, and
no movement beyond the top or bottom boundary. It also verifies stable-key
anchor restoration after narrowing from 40 to 24 columns. It reports paint
latency and allocations plus width-reflow latency and measurement counts
separately from the bare-text baseline. The `cards_scroll` line aggregates paint
latency, allocation requests, and requested bytes over all 128 steps, with
`max_painted` reporting the largest visible-item count across those steps.
The `cards` line separately reports initial, single-row, and resized-paint
allocations. Allocation counters include successful ordinary, zeroed, and
reallocation requests during painting; requested bytes are cumulative allocation
traffic, not retained or peak memory. Buffer setup and ANSI encoding are outside
the counted interval.

One-row scrolling uses `scroll_by` and
paints retained geometry before any new synchronization. Callers must
synchronize
after item, layout revision, width, or environment changes, not merely because
the scroll offset changed. For comparison, `row_sync_us`
measures the unchanged-collection synchronization, `row_paint_us` measures only
painting, and `row_sync_and_paint_us` sums those two operations. The sum
excludes
buffer setup, metadata inspection, and ANSI encoding in the benchmark harness;
it is not an end-to-end frame latency. Zero remeasurement does not imply that
synchronization is constant-time or sublinear. These fixed-viewport samples
provide structural evidence for the exercised scenarios, not a completed
comparison against historical baselines or proof of downstream adoption.

The additional 100,000-item lookup probe measures prefix/visible-range lookup
without presenting the collection. Estimated heights and application-owned row
caches are deliberately absent.

Run the example in release mode:

```sh
cargo run --release -p bmux_tui_components --example virtual_list_benchmark \
  --features virtual-list
```

The `component` lines measure `VirtualListComponent` over a lazy item source
at 1,000, 10,000, and 100,000 items: the first frame (which measures every
item), a steady layout-plus-paint frame (which constructs only visible items),
routing one wheel event, a frame after 16 streamed appends while following the
bottom, and the average frame of an eased motion to the top. Steady, append,
and motion frame costs stay flat as the collection grows.

The `lazy_source` lines include `prepend16_append16_us`: a history prepend and
a live append noted in the same frame, which stay separate dirty regions so the
cost tracks the 32 noted rows rather than the collection length. The
`component_large_layouts` lines lay out and paint items whose retained layouts
have 10 to 10,000 child nodes; steady layout cost stays flat because the layout
tree holds leaf item wrappers and paint borrows the retained layouts.

## Virtual list chat demo

`virtual_list_chat` is an interactive chat log built on
`VirtualListComponent` and the BMUX runtime. It demonstrates:

- streamed appends noted with `VirtualListState::note_splice`, followed while
  the position is `FollowBottom`;
- history pages prepended (after a simulated fetch delay) when
  `ResolvedPosition::edge_rows` reports fewer than one viewport of rows above
  the viewport; the keyed position keeps the view still;
- keyed scroll motion (`g` / `G`) advanced with `RuntimeHandle::now` and a
  keyed runtime timer, with a reduced-motion toggle (`m`) that uses a zero
  duration;
- `Overscroll::ViewportMinus(1)` (toggle with `o`), the gutter scrollbar,
  keyboard and wheel navigation, and click activation that selects a message.

Run it in a terminal:

```sh
cargo run -p bmux_tui_components --example virtual_list_chat --features virtual-list
```
