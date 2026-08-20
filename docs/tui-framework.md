# BMUX TUI framework

BMUX will provide a native terminal UI framework for building responsive, high-performance terminal interfaces in BMUX, Bcode, and future plugin/application surfaces.

The framework is not a product-specific UI layer. It owns terminal UI primitives: geometry, layout, styled text, render buffers, widgets, events, focus, overlays, virtualization, and terminal backends. Product behavior such as sessions, windows, panes, clients, contexts, permissions, model turns, tools, and chat state remains outside the framework.

## Goals

- Provide a BMUX-native replacement for the terminal UI library currently used by Bcode.
- Make terminal UI construction more ergonomic than direct cell painting or ad hoc crossterm rendering.
- Support deterministic responsive layout across terminal sizes and resize events.
- Support reusable widgets for text input, lists, overlays, modals, command palettes, and transcripts.
- Support opt-in developer-tool widgets such as diff/file views without making them part of the default TUI core.
- Preserve BMUX's existing strengths in terminal protocol correctness, rendering performance, and plugin-oriented architecture.
- Make rendering testable without a real terminal.

## Non-goals

- Do not put BMUX domain behavior into the framework. Sessions, windows, panes, clients, contexts, and permissions remain plugin/application concerns.
- Do not mechanically clone ratatui. Ratatui is a practical migration checklist, not the design ceiling.
- Do not rewrite the attach pipeline wholesale as an early step.
- Do not create speculative helper crates before implementation pressure proves they are needed.

## Image presentation boundary

`bmux_tui` frames may contribute protocol-neutral image payloads, stable keys,
cell destinations, clips, and frame or persistent lifecycles. The terminal
retains and reconciles this image scene alongside its cell buffer and
interaction metadata. `draw_with_overlay` and `draw_damage_with_overlay` let a
backend emit protocol overlays before the shared flush, so failed image output
does not commit speculative frame state.

Terminal protocol selection and encoding are intentionally outside
`bmux_tui`. The feature-gated `bmux_tui_runtime::ImageTerminalPresenter` uses
`bmux_image` for Kitty, Sixel, and iTerm2 output and owns the normal runtime
integration. Applications retain semantic image identity, content, sizing, and
text fallback policy.



BMUX's TUI framework has three domain-neutral layers:

- `packages/tui` (`bmux_tui`) owns low-level terminal primitives, frames, events, and backends.
- `packages/tui-components` (`bmux_tui_components`) owns reusable controls and component-local interaction state.
- `packages/tui-runtime` (`bmux_tui_runtime`) owns bounded event admission, fair scheduling, commands, timers, redraw coalescing, render cadence, terminal-input lifecycle, shutdown, and neutral runtime statistics.

The runtime depends on `bmux_tui` but not `bmux_tui_components`. Applications may use all three while retaining product state and behavior. See [`tui-runtime.md`](tui-runtime.md) for the runtime contract.

## Initial primitive crate boundary

The primitive crate is `packages/tui`, published in the workspace as `bmux_tui`.

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

## Feature boundaries and dependency direction

`bmux_tui` has a small default feature set. The default build is the general-purpose terminal UI toolkit and must stay useful for applications that have no developer-tool or coding-agent needs.

Default `bmux_tui` may contain:

- geometry, layout, style, text, buffer, frame, and backend primitives
- general widgets such as text blocks, panels, modals, text inputs, lists, and pickers
- generic event/focus/viewport primitives when they are added
- adapters to neutral BMUX primitives when those adapters do not pull in product behavior

Default `bmux_tui` must not contain hard dependencies on:

- BMUX sessions, windows, panes, clients, contexts, permissions, or plugin runtime behavior
- Bcode chat/model/tool concepts
- coding-agent-only visualizations
- file-edit or VCS-specific models

Feature-gated modules may provide more specialized UI surfaces. These features must depend inward on the neutral TUI primitives; neutral primitives must never depend outward on feature-gated modules.

Current specialized features:

| Feature | Purpose                                                   | Boundary rule                                                     |
| ------- | --------------------------------------------------------- | ----------------------------------------------------------------- |
| `diff`  | Developer-tool/coding-agent diff and file view primitives | Opt-in only; no default exports; no core dependency on diff types |

If a feature begins to need substantial domain models or behavior, prefer a separate domain crate or plugin-owned UI module instead of expanding default `bmux_tui`.

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

The framework renders into a neutral cell buffer first. Backends then flush the buffer to crossterm/ANSI or other targets.

Important properties:

- clipping is explicit and reliable
- wide/unicode text behavior is deliberate
- style patching is cheap and predictable
- render output is easy to assert in tests
- complete frames use retained-buffer incremental ANSI diffing
- `Damage::Regions` supports process-local partial presentation: regions are clipped, deterministically coalesced, bounded by count and area, and promoted to `Damage::Full` when excessive
- partial renders begin with an empty staging buffer; only declared damaged cells survive, while cells and hit/image metadata outside damage are restored from the last committed presentation
- resize, reset, first presentation, and unknown damage use a complete presentation
- terminal output and metadata commit atomically after a successful flush; output failure preserves committed metadata, discards the uncertain retained buffer, and forces the next successful draw to repaint fully

This retained state is process-local terminal presentation state. It does not define transport retention, acknowledgment, replay, conflict handling, reconnect safety, or durable resume.

## Logical content selection

`bmux_tui::selection` models selection as logical document interaction rather than framebuffer text
extraction. Renderers register hierarchical scopes and visible fragments that map terminal geometry
to caller-owned UTF-8 source boundaries. A caller-owned controller locks the deepest eligible scope
at pointer-down, supports parent delegation and cross-descendant ordering, and exposes snapshots of
logical source slices plus current visible highlights.

Selection metadata follows the same transactional frame boundary as hits and images. Applications
paint a current snapshot with `Frame::paint_selection` after ordinary content rendering, then retain
the controller independently of frame geometry. Applications and components own viewport mutation
for generic autoscroll requests; product consumers own canonical source resolution, copy formatting,
and clipboard effects. Core selection types remain domain-neutral and do not interpret panes,
transcripts, Markdown, tools, sessions, or plugins.

## Performance direction

The framework should be designed for high-churn terminal applications, large transcripts, and large diffs.

Required performance direction:

- virtualized lists/transcripts/diffs
- wrapping caches keyed by content revision and width
- bounded dirty-region damage with safe full-frame fallback
- incremental retained-buffer diffing before backend flush
- avoid full transcript/diff re-render work every frame
- performance tests for resize churn, large transcripts, and large diffs

## Diff and file views

Diff/file views are important for developer-tool and coding-agent consumers, especially for file edit tools, but they are not part of the general-purpose TUI core. They live behind the `diff` feature and should remain optional.

Boundary requirements for diff/file view work:

- The default `bmux_tui` build must not export diff types.
- Core modules such as geometry, layout, style, text, buffer, frame, widgets, and ANSI backends must not depend on diff modules.
- Diff modules may depend on neutral TUI primitives.
- Diff modules must avoid Bcode-specific model/tool/session types.
- If diff support grows into a substantial developer-tool UI domain, consider extracting it into a dedicated crate such as `packages/tui-diff` instead of expanding default `bmux_tui`.

The optional diff feature can eventually provide reusable primitives for:

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

### Phase 6: optional developer-tool views

- Keep diff/file view primitives behind the `diff` feature.
- Add transcript/list virtualization depth to the default core only when the primitives remain general.
- Add optional diff/file view primitives without introducing default dependencies on coding-agent or VCS concepts.

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
