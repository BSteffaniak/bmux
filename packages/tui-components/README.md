# bmux_tui_components

Reusable, opt-in terminal UI components built from BMUX low-level primitives.

The crate keeps state separate from policy. Low-level `bmux_tui` widgets remain
available for applications that want raw rendering and event handling.

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
