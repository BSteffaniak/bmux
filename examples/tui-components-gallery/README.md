# TUI Components Gallery

Render and interaction-conformance examples for `bmux_tui_components` buttons,
panes, modal frames, form-field wrappers, details, and dialogs. Every control is
measured through `Component::layout` and paints the resulting `LayoutNode`
through a translated, clipped `PaintCx`; the `Frame` in the example is only the
terminal scene staging boundary. The interactive binary routes Tab/Shift-Tab,
focus, and pointer hover through the last successfully committed BMUX
interaction scene; press `q`, Escape, or Ctrl-C to exit.

Run interactively:

```sh
cargo run -p bmux_tui_components_gallery
```
