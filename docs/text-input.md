# Text input

BMUX prompt text fields, searchable selects, and editable text/number form fields use shared Unicode-aware text editing primitives from `bmux_text_edit`.

## Prompt input bindings

Prompt text inputs support standard terminal editor behavior where the terminal reports the corresponding key event:

| Binding                    | Behavior                                |
| -------------------------- | --------------------------------------- |
| `left` / `right`           | Move one grapheme left or right         |
| `alt+left` / `ctrl+left`   | Move one word left                      |
| `alt+right` / `ctrl+right` | Move one word right                     |
| `home` / `end`             | Move to start or end of the text field  |
| `ctrl+a` / `ctrl+e`        | Move to start or end of the text field  |
| `backspace` / `delete`     | Delete one grapheme backward or forward |
| `ctrl+u`                   | Clear search-select query text          |

The reusable `bmux_text_edit` crate owns text storage, cursor movement, deletion, word boundaries, Unicode grapheme handling, and terminal-width projection. Rendering and prompt submission remain owned by BMUX attach UI.

## Notes

- Word movement uses the current `bmux_text_edit` word-boundary policy.
- Modifier reporting varies by terminal; if a terminal does not send distinct `alt`/`ctrl` arrow events, BMUX cannot apply the matching word movement binding.
- Direct OS clipboard access is intentionally outside `bmux_text_edit`; future selection/cut/copy/paste support should expose editor primitives for app-owned clipboard integration.
