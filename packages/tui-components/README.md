# bmux_tui_components

Reusable, opt-in terminal UI components built from BMUX low-level primitives.

The crate keeps state separate from policy. Low-level `bmux_tui` widgets remain
available for applications that want raw rendering and event handling.

## Cargo features

The crate has no default component set. Enable only the controls an application uses:

```toml
bmux_tui_components = { version = "0.0.1-alpha.1", default-features = false, features = [
    "dialog",
    "selectable-list",
    "text-input-box",
] }
```

Each public component has an additive feature of the same kebab-case name. A composed component
activates its implementation prerequisites, so enabling `dialog` also enables its modal frame,
action row, and button. Consumers do not need to repeat those internal dependencies.

Optional bundles provide ergonomic groups: `forms`, `navigation`, `data-display`, `overlays`, and
`text-editing`. The `all` feature enables every component for galleries, documentation, and broad
validation; it is intentionally not a default production choice.

Cargo unifies features across the dependency graph. Features therefore only add APIs and optional
dependencies; runtime policies control whether an enabled component accepts keyboard or mouse
input. Display-only components do not activate BMUX text editing or Unicode grapheme editing.
The repository verifies the no-feature, individual-feature, bundle, and all-feature matrix with
`scripts/check-tui-component-features.sh`.

The component theme separates application canvas from normal, raised, and overlay surfaces. Semantic
role styles are deterministically patched over the selected surface: an explicit role foreground,
background, or modifier wins, while omitted values inherit from that surface. Overlay themes may
also carry a scrim. `terminal_default()` preserves `Color::Default` at every depth; `opaque_dark()`
and `opaque_light()` provide neutral gallery/reference palettes rather than product theme policy.

The feature-gated `compact` helpers provide grapheme-safe width truncation, rich metadata header
wrapping, and compact byte formatting. The feature-gated `terminal-viewer` owns bounded generic
terminal-grid decoding and ANSI preservation; applications remain responsible for process, shell,
artifact, and recording semantics.

## Component conventions

Interactive components should follow the same shape as the existing text input
control:

- `*State` stores runtime UI state such as focus, hover, pressed, drag, cursor,
  selection, and scroll offsets.
- `*Policy` declares behavior such as keyboard handling, mouse support,
  activation, dragging, resizing, and bounds rules.
- `*Styles` or `*Theme` stores rendering configuration.
- `*Outcome` reports whether an event was ignored, handled, needs redraw, or
  produced a semantic action.

Mouse capability is first-class. Components that handle pointer input should
make that behavior explicit in policy rather than hiding it in ad hoc event
branches.

Components in this crate must remain domain-neutral. Generic UI panes, buttons,
forms, lists, dialogs, and surfaces are in scope; product-specific BMUX behavior
belongs in applications or plugins.

## Application integration boundary

Applications should compose these controls at their renderer boundary and request exact production
features. Product-specific recipes and workflow semantics remain application/plugin-owned; do not
move them into this crate to eliminate a thin adapter. Direct `bmux_tui` primitives remain
appropriate for full-canvas underpaint, clipping/scratch-frame adaptation, terminal media placement,
domain-specific drawing, and component implementation internals. Reusable controls, chrome, and
interaction policy belong here rather than in application-local raw rendering.

`bmux_tui` intentionally carries keyboard and text-editing in its baseline primitive API. Public
event, focus, list, viewport, picker, palette, history, and text-input modules directly expose those
types. Component-owned optional dependencies—including terminal-grid and Unicode helpers—remain
isolated behind component features and the repository feature-matrix guard.
