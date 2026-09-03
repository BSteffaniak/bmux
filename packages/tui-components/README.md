# bmux_tui_components

Reusable, opt-in terminal UI components built on BMUX's measurable component,
scoped-paint, and interaction primitives.

The crate keeps caller-owned state separate from reusable policy. Components
resolve an authoritative `LayoutNode` through `Component::layout`, paint that
exact layout through `PaintCx`, and route events through `EventCx`. Applications
should not assign raw frame areas to reusable controls or reconstruct component
geometry after painting.

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

## Canonical composition

Use `bmux_tui::composition` for child ownership, measurement, placement, and
rectangular styling. A `Surface` owns its background, border, padding, and child;
`Row` and `Column` own linear placement; wrappers such as `Padding`, `SizeBox`,
`Align`, `Flex`, `Clip`, `StyleScope`, `Visibility`, `Stack`, and `Keyed` each
modify one concern. A component that measures a rectangle is responsible for
painting that complete rectangle. Application-side background extension,
wrapped-row caches, and parallel hit geometry are not supported integration
patterns.

For arbitrary scrolling, use the `scroll-view` feature and caller-owned
`ScrollViewState`. It provides logical offsets, nested wheel routing,
keyboard/page/home/end navigation, scrollbars and dragging, ensure-visible,
selection autoscroll, and bottom follow. For large variable-height collections,
use `virtual-list`: item keys and revisions retain exact current-width layouts,
only viewport-intersecting items paint/register metadata, and keyed top or
bottom anchors survive insertion, removal, reorder, append, and reflow.

## Logical content selection

`selection` provides opt-in `ComponentSelectionPolicy`, `ComponentSelectionState`, styles, outcomes,
and bounded autoscroll cadence. Components register caller-owned logical identities and source
boundaries; they never derive copied text from terminal cells. Pane content captures locally while
chrome delegates by default, PanelGroup supplies deterministic sibling ordering without consuming
divider handles, and TextViewComponent maps wrapped/scrolled visible graphemes to original UTF-8 offsets
through the scoped paint context. SourceViewer, DiffViewer, and TerminalViewer selection registrars
accept the same `PaintCx` so their fragments translate and clip with the surrounding component tree.
SourceViewer and unified DiffViewer map visible rows to canonical source bytes; DiffViewer declines
ambiguous side-by-side projections. TerminalViewer deliberately selects its decoded terminal-grid
text document rather than claiming ANSI/control bytes as source provenance. Editable TextInput
remains isolated from outer selection unless explicitly configured to delegate. Consumers retain
the BMUX selection controller and own viewport mutation, source resolution, and clipboard behavior.

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
appropriate for full-canvas underpaint, terminal media placement, domain-specific drawing, and
component implementation internals. Reusable controls, chrome, composition, scrolling, and
interaction policy belong here rather than in application-local raw rendering. Ordinary reusable
components paint through `PaintCx`; scratch-frame adaptation and unrestricted `Frame`/buffer access
are backend/runtime concerns, not an application integration model.

`bmux_tui` intentionally carries keyboard events and foundational text-edit painting in its
baseline primitive API. Applications should use `TextInputComponent` or `TextInputBoxComponent`
from this crate; the foundational `bmux_tui::input::TextInput` leaf is an implementation detail for
component authors and is not prelude-exported. `TextInputState` owns the edit buffer, cursor,
selection, pointer gesture, and viewport offset; `TextInputPolicy` owns keyboard, mouse, edge, and
outer-selection behavior. Component-owned optional dependencies—including terminal-grid and Unicode
helpers—remain isolated behind component features and the repository feature-matrix guard.
