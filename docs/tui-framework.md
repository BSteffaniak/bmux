# BMUX TUI framework

BMUX will provide a native terminal UI framework for building responsive, high-performance terminal interfaces in BMUX, Bcode, and future plugin/application surfaces.

The framework is not a product-specific UI layer. It owns terminal UI primitives: geometry, layout, styled text, render buffers, widgets, events, focus, overlays, virtualization, and terminal backends. Product behavior such as sessions, windows, panes, clients, contexts, permissions, model turns, tools, and chat state remains outside the framework.

## Goals

- Provide a BMUX-native replacement for the terminal UI library currently used by Bcode.
- Make terminal UI construction more ergonomic than direct cell painting or ad hoc crossterm rendering.
- Support deterministic responsive layout across terminal sizes and resize events.
- Support reusable widgets for text input, lists, overlays, modals, command palettes, transcripts, and diff/file views.
- Preserve BMUX's existing strengths in terminal protocol correctness, rendering performance, and plugin-oriented architecture.
- Make rendering testable without a real terminal.

## Non-goals

- Do not put BMUX domain behavior into the framework. Sessions, windows, panes, clients, contexts, and permissions remain plugin/application concerns.
- Do not mechanically clone ratatui. Ratatui is a practical migration checklist, not the design ceiling.
- Do not rewrite the attach pipeline wholesale as an early step.
- Do not create speculative helper crates before implementation pressure proves they are needed.

## Initial crate boundary

The initial crate is `packages/tui`, published in the workspace as `bmux_tui`.

The first crate owns neutral primitives:

- geometry: `Point`, `Size`, `Rect`, `Insets`
- style: `Color`, `Modifier`, `Style`
- text: `Span`, `Line`, `Text`
- buffer: `Cell`, `Buffer`
- layout: constraints and deterministic split helpers

Bespoke product-adjacent functionality should be opt-in. Diff/file views are useful for coding-agent and developer-tool surfaces, but they are not required by general terminal UI consumers, so they live behind the `diff` crate feature and must not be required by the core primitives.

Later modules should be added only when needed:

- widgets
- events
- focus
- terminal backends
- virtualization
- diff/file views
- test support

If the crate becomes too broad, split only by real capability pressure, for example `tui-render` or `tui-testing`. Avoid vague shared/common crates.

## Relationship to existing BMUX crates

### `bmux_terminal_grid`

`bmux_terminal_grid` models terminal-emulator state and PTY output. `bmux_tui` models application UI output. The two may share concepts, but the TUI render buffer should remain an application rendering target rather than becoming PTY state.

### `bmux_attach_pipeline`

The attach pipeline already contains rendering, cursor, compositor, mouse, and scene integration code. Generic helpers can be extracted or reused, but attach-specific pane/session behavior should stay in attach/plugin layers.

### `bmux_attach_layout_protocol`

Attach layout protocol DTOs describe attached surfaces and content rectangles. `bmux_tui` should use its own neutral geometry and may provide explicit adapter functions later.

### `bmux_scene_protocol` and `bmux_scene_protocol_render`

Scene protocol is useful for plugin paint/decorations and may become an adapter target. `bmux_tui` should first define a neutral render buffer. Scene-protocol integration should be adapter-oriented unless a stronger dependency is justified.

### `bmux_text_edit`

Text input widgets should build on `bmux_text_edit` for Unicode-aware text storage, grapheme movement, deletion, word navigation, wrapping, and cursor projection.

### `bmux_keyboard`

TUI event handling should use `bmux_keyboard` key types instead of backend-specific key events. Crossterm conversion belongs in backend/adapters.

## Bcode replacement milestone

A major milestone is that Bcode's TUI can remove its current ratatui dependency and render with BMUX TUI primitives.

The replacement surface should cover the practical roles Bcode currently gets from ratatui:

| Current role                 | BMUX TUI target                    |
| ---------------------------- | ---------------------------------- |
| terminal runtime             | terminal/backend abstraction       |
| frame render pass            | frame/render context               |
| cell buffer                  | `Buffer` and `Cell`                |
| rectangle and position types | `Rect`, `Point`, `Size`, `Insets`  |
| constraints and splits       | `layout` primitives                |
| colors and style modifiers   | `Color`, `Modifier`, `Style`       |
| styled text                  | `Span`, `Line`, `Text`             |
| widgets                      | BMUX-native widget model           |
| block/borders                | panel/block widget                 |
| paragraph/wrapping           | text block widget                  |
| lists and list state         | virtualized list widget            |
| overlays/clear               | overlay stack and modal primitives |
| cursor placement             | frame cursor API                   |
| crossterm backend            | backend adapter                    |

This document deliberately does not perform Bcode migration work. Bcode should migrate only after BMUX primitives are implemented and tested.

## Responsive layout

The layout system should be deterministic and testable. Widgets should not read terminal size directly; the frame/layout context provides bounds.

Initial requirements:

- fixed, percentage, ratio/fill, minimum, and maximum constraints
- horizontal and vertical splitting
- safe behavior for zero-size and tiny terminal regions
- nested layout determinism
- breakpoint helpers for compact, medium, and wide views

Future responsive widgets should be able to switch presentation by available width, for example:

- wide diff view: side-by-side
- medium diff view: unified
- narrow diff view: compact stacked

## Rendering model

The framework should render into a neutral cell buffer first. Backends then flush the buffer to crossterm/ANSI or other targets.

Important properties:

- clipping must be explicit and reliable
- wide/unicode text behavior must be deliberate
- style patching should be cheap and predictable
- render output should be easy to assert in tests
- later backend flushing should support damage/incremental updates

## Performance direction

The framework should be designed for high-churn terminal applications, large transcripts, and large diffs.

Required performance direction:

- virtualized lists/transcripts/diffs
- wrapping caches keyed by content revision and width
- dirty-region or damage tracking where practical
- incremental buffer diffing before backend flush
- avoid full transcript/diff re-render work every frame
- performance tests for resize churn, large transcripts, and large diffs

## Diff and file views

Diff/file views are first-class product targets, especially for coding-agent file edit tools.

The framework should eventually provide reusable primitives for:

- unified diffs
- side-by-side diffs
- responsive mode switching
- line-number gutters
- added/deleted/changed-line styling
- inline changed-region highlighting
- folded unchanged ranges
- file list navigation
- hunk navigation
- virtualization for large files

## Phased implementation plan

### Phase 1: foundation

- Create `bmux_tui` crate.
- Add geometry, style, text, buffer, and basic layout primitives.
- Add focused unit tests.

### Phase 2: render and layout depth

- Add richer constraints and responsive helpers.
- Add clipping and styled text rendering helpers.
- Add backend-neutral frame/render context.

### Phase 3: text input widgets

- Build single-line and multiline inputs on `bmux_text_edit`.
- Support selection, cursor projection, wrapping, paste, and viewport scrolling.

### Phase 4: widgets and overlays

- Add text block, panel/block, list, virtualized viewport, overlay stack, modal, completion, and command palette primitives.

### Phase 5: terminal backend

- Add crossterm/ANSI backend adapter.
- Add cursor positioning and incremental flush support.

### Phase 6: rich views

- Add transcript/list virtualization depth.
- Add diff/file view primitives.

### Phase 7: adoption

- Integrate into bounded BMUX UI surfaces first.
- Later migrate Bcode component-by-component until ratatui can be removed.

## Validation expectations

For implementation changes, run focused validation first:

```sh
cargo fmt --check
cargo check -p bmux_tui
cargo clippy -p bmux_tui --all-targets -- -D warnings
cargo test -p bmux_tui
```

Before broader integration, run workspace checks according to repository policy.
