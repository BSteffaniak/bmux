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
