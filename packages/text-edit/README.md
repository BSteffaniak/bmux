# bmux_text_edit

Unicode-aware terminal text editing primitives for BMUX.

This crate owns reusable editor state for terminal text fields and composers. It
is intentionally UI-framework agnostic: renderers such as the BMUX attach prompt
UI or a ratatui application own painting, focus, validation, and submission.

## Scope

- Editable UTF-8 text plus cursor state
- Grapheme-boundary insertion, deletion, and movement
- Word movement for readline-style bindings
- Terminal column width and single-line viewport projection helpers
- Soft wrapping and visual cursor projection helpers

The crate does not depend on crossterm, ratatui, BMUX runtime state, plugins, or
prompt request/response types.
