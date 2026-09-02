# TUI Components Layout Playground

Geometry and layout examples for `bmux_tui_components`, including pane bounds,
modal placement, hit regions, and resize bounds. Components resolve explicit
constraints to authoritative `LayoutNode` trees; painting and interaction use
the same local placements, translations, and clips instead of recomputing
terminal geometry.

Run interactively:

```sh
cargo run -p bmux_tui_components_layout_playground
```
