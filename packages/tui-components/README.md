# bmux_tui_components

Reusable, opt-in terminal UI components built on BMUX's measurable component,
scoped-paint, and interaction primitives.

The crate keeps caller-owned state separate from reusable policy. Components
resolve an authoritative `LayoutNode` through `Component::layout`, paint that
exact layout through `PaintCx`, and route events through `EventCx`. Applications
should not assign raw frame areas to reusable controls or reconstruct component
geometry after painting.

## Cargo features

The crate has no default component set. Enable only the controls an application
uses:

```toml
bmux_tui_components = { version = "0.0.1-alpha.1", default-features = false, features = [
    "dialog",
    "selectable-list",
    "text-input-box",
] }
```

Each public component has an additive feature of the same kebab-case name. A
composed component
activates its implementation prerequisites, so enabling `dialog` also enables
its modal frame,
action row, and button. Consumers do not need to repeat those internal
dependencies.

Optional bundles provide ergonomic groups: `forms`, `navigation`,
`data-display`, `overlays`, and
`text-editing`. The `all` feature enables every component for galleries,
documentation, and broad
validation; it is intentionally not a default production choice.

Cargo unifies features across the dependency graph. Features therefore only add
APIs and optional
dependencies; runtime policies control whether an enabled component accepts
keyboard or mouse
input. Display-only components do not activate BMUX text editing or Unicode
grapheme editing.
The repository verifies the no-feature, individual-feature, bundle, and
all-feature matrix with
`scripts/check-tui-component-features.sh`.

The component theme separates application canvas from normal, raised, and
overlay surfaces. Semantic
role styles are deterministically patched over the selected surface: an explicit
role foreground,
background, or modifier wins, while omitted values inherit from that surface.
Overlay themes may
also carry a scrim. `terminal_default()` preserves `Color::Default` at every
depth; `opaque_dark()`
and `opaque_light()` provide neutral gallery/reference palettes rather than
product theme policy.

## Bordered panels

Use `pane::PaneComponent` with a `pane::Pane` configuration and a content child
such as `bmux_tui::composition::TextBlock`. Do not hand-paint borders or write
titles over full-width rows: row replacement clears unused cells and can erase
chrome. The pane owns title clipping, border/padding geometry, and the child's
clipped content area. Low-level `PaintCx` calls belong inside custom components.

```rust
use std::cell::Cell;
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::Rect;
use bmux_tui::text::TextWrap;
use bmux_tui_components::pane::{Pane, PaneComponent, PaneState};

let state = Cell::new(PaneState::new(Rect::new(0, 0, 40, 10)));
let logs = PaneComponent::new(
    "logs", // Stable application-owned identity, not a list index.
    Pane::new().title("Logs"),
    &state,
    TextBlock::new("Log content").wrap(TextWrap::None),
);
```

Enable the `pane` feature. Keep interactive pane state in the caller across
frames, and lay out, paint, and route events through the same component tree.
Applications remain responsible for sanitizing untrusted text and choosing
content wrapping/scrolling policies.

## Canonical composition

Use `bmux_tui::composition` for child ownership, measurement, placement, and
rectangular styling. A `Surface` owns its background, border, padding, and
child;
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

`selection` provides opt-in `ComponentSelectionPolicy`,
`ComponentSelectionState`, styles, outcomes,
and bounded autoscroll cadence. Components register caller-owned logical
identities and source
boundaries; they never derive copied text from terminal cells. Pane content
captures locally while
chrome delegates by default, PanelGroup supplies deterministic sibling ordering
without consuming
divider handles, and TextViewComponent maps wrapped/scrolled visible graphemes
to original UTF-8 offsets
through the scoped paint context. SourceViewer, DiffViewer, and TerminalViewer
selection registrars
accept the same `PaintCx` so their fragments translate and clip with the
surrounding component tree.
SourceViewer and unified DiffViewer map visible rows to canonical source bytes;
DiffViewer declines
ambiguous side-by-side projections. TerminalViewer deliberately selects its
decoded terminal-grid
text document rather than claiming ANSI/control bytes as source provenance.
Editable TextInput
remains isolated from outer selection unless explicitly configured to delegate.
Consumers retain
the BMUX selection controller and own viewport mutation, source resolution, and
clipboard behavior.

The feature-gated `compact` helpers provide grapheme-safe width truncation, rich
metadata header
wrapping, and compact byte formatting. The feature-gated `terminal-viewer` owns
bounded generic
terminal-grid decoding and ANSI preservation; applications remain responsible
for process, shell,
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

Applications should compose these controls at their renderer boundary and
request exact production
features. Product-specific recipes and workflow semantics remain
application/plugin-owned; do not
move them into this crate to eliminate a thin adapter. Direct `bmux_tui`
primitives remain
appropriate for full-canvas underpaint, terminal media placement,
domain-specific drawing, and
component implementation internals. Reusable controls, chrome, composition,
scrolling, and
interaction policy belong here rather than in application-local raw rendering.
Ordinary reusable
components paint through `PaintCx`; scratch-frame adaptation and unrestricted
`Frame`/buffer access
are backend/runtime concerns, not an application integration model.

`bmux_tui` intentionally carries keyboard events and foundational text-edit
painting in its
baseline primitive API. Applications should use `TextInputComponent` or
`TextInputBoxComponent`
from this crate; the foundational `bmux_tui::input::TextInput` leaf is an
implementation detail for
component authors and is not prelude-exported. `TextInputState` owns the edit
buffer, cursor,
selection, pointer gesture, and viewport offset; `TextInputPolicy` owns
keyboard, mouse, edge, and
outer-selection behavior. Component-owned optional dependencies—including
terminal-grid and Unicode
helpers—remain isolated behind component features and the repository
feature-matrix guard.

## Text-input undo/redo bindings

With the `text-input` feature, `TextInputControl` delegates history to its
`bmux_text_edit::TextEditBuffer`; it does not maintain a separate undo engine.
Default history bindings are ctrl-Z/super-Z for undo and ctrl-shift-Z,
super-shift-Z or ctrl-Y for redo. These operate only when the terminal reports
those exact key strokes. A recognized action with no history returns
`TextInputOutcome::Redraw` without modifying text or falling through to another
shortcut. `policy.keyboard.enabled = false` disables keyboard handling,
including
history actions supplied by an adapter.

`TextInputHistoryBindings` contains borrowed `undo` and `redo` key lists. Supply
resolved application configuration through `with_history_bindings` on either
`TextInputControl` or `TextInputComponent`. Explicit lists replace defaults;
empty lists independently disable an action, and `disabled()` disables both.
A stroke appearing in both lists resolves to no history action. Ordinary text
editing continues through the configured text keymap.

```rust
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::TextEditBuffer;
use bmux_tui_components::text_input::{
    TextInputControl, TextInputHistoryBindings, TextInputOutcome,
    TextInputPolicy, TextInputState,
};

let policy = TextInputPolicy::chat_composer();
let undo_keys = [KeyStroke::simple(KeyCode::F(7))];
let control = TextInputControl::new(&policy).with_history_bindings(
    TextInputHistoryBindings { undo: &undo_keys, redo: &[] },
);
let mut state = TextInputState::new(TextEditBuffer::from_text("before"));
state.buffer_mut().insert_str(" after");
assert_eq!(control.handle_key(&mut state, undo_keys[0]), TextInputOutcome::Edited);
assert_eq!(state.buffer().text(), "before");
// Redo remains available in the editor, but has no keyboard binding here.
assert!(state.buffer().can_redo());
```

Applications with an existing semantic keymap can instead call
`handle_key_with_history_action(state, stroke, action)`, where `action` is
`Option<TextInputHistoryAction>` (`Undo` or `Redo`). This result is
authoritative:
`None` bypasses all history bindings, including defaults, rather than falling
back to a hardcoded ctrl-Z handler. Other editing still uses the ordinary text
keymap; `None` is not a blanket instruction to ignore the key.

```rust
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_text_edit::TextEditBuffer;
use bmux_tui_components::text_input::{
    TextInputControl, TextInputHistoryAction, TextInputOutcome,
    TextInputPolicy, TextInputState,
};

let policy = TextInputPolicy::chat_composer();
let control = TextInputControl::new(&policy);
let mut state = TextInputState::new(TextEditBuffer::from_text("before"));
state.buffer_mut().insert_str(" after");
let stroke = KeyStroke::with_modifiers(
    KeyCode::Char('z'), Modifiers { ctrl: true, ..Modifiers::NONE },
);
// A disabled/unmapped semantic result does not trigger default undo.
assert_eq!(
    control.handle_key_with_history_action(&mut state, stroke, None),
    TextInputOutcome::Ignored,
);
assert_eq!(state.buffer().text(), "before after");
assert_eq!(
    control.handle_key_with_history_action(
        &mut state, stroke, Some(TextInputHistoryAction::Undo),
    ),
    TextInputOutcome::Edited,
);
```

Keep binding slices alive as long as the control/component borrows them. Resolve
remapping, explicit disablement and application shortcut precedence before
passing the semantic action; do not call the default history handler afterward.
Undo storage bounds, oversized-edit clearing and serde omission are documented
in the `bmux_text_edit` README.

### Ephemeral control rebuild snapshots

Interactive native states expose `snapshot() -> serde_json::Value` and
`restore_snapshot(&Value) -> Result<(), String>`. These JSON values are opaque,
in-memory rebuild state, not stable wire formats or durable schemas. A snapshot
retains selection and private cursor/hover/press state; dropdown snapshots also
retain the open list and its pending option independently of the committed
value.
Applications keep these snapshots per view, restore them into caller-owned
state,
and then reapply canonical values and current keyboard-focus flags. Restore does
not clamp indices against item collections; callers still own item
reconciliation.
Malformed snapshots leave state unchanged. Restore is bounded to 32 nesting
levels,
16,384 JSON nodes and 256 KiB of string/key data. A larger native state can be
captured but cannot be restored through this bounded API.

Text-input snapshots retain text, caret, selection, viewport/scroll state and
active
mouse selection. Derived wrap layouts and editor undo/redo history are not
included.
Multi-click timing is restored from a bounded relative age, not a serialized
clock
identity. Snapshot APIs are available with each control's existing feature;
there is
no additional feature a caller must enable. `CheckboxState::interaction()` and
`set_focused(bool)` expose focus without making its private state fields public.
