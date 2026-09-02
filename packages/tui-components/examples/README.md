# Variable-height virtual list benchmark

This executable is both the structural performance baseline and the public
large-collection example for `bmux_tui_components::VirtualList`.

It builds stable-key item components with mixed exact heights and runs the
canonical lifecycle:

1. Build a keyed `VirtualList` whose children implement `Component`.
2. Call `sync` at the current width so `VirtualListState` retains authoritative
   exact layouts by key, layout revision, width, and environment.
3. Keep logical scrolling and anchor state in the caller-owned
   `VirtualListState`.
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

The additional 100,000-item lookup probe measures prefix/visible-range lookup
without presenting the collection. Estimated heights and application-owned row
caches are deliberately absent.

Run the example in release mode:

```sh
cargo run --release -p bmux_tui_components --example virtual_list_benchmark \
  --features virtual-list
```
