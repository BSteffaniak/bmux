# bmux_tui_components

Reusable, opt-in terminal UI components built on BMUX's measurable component,
scoped-paint, and interaction primitives.

The crate keeps caller-owned state separate from reusable policy. Components
resolve an authoritative `LayoutNode` through `Component::layout`, paint that
exact layout through `PaintCx`, and route events through `EventCx`. Applications
should not assign raw frame areas to reusable controls or reconstruct component
geometry after painting.

## Pointer presentation

Controls publish hit roles through their ordinary scoped paint path. The shared
`bmux_tui::pointer` resolver maps those roles to automatic pointer intent using
the committed scene: buttons use `HitRole::Action`, while editable text uses
`HitRole::TextInput`. Keep disabled state in the control's caller-owned state;
disabled regions must not advertise an actionable override or expose lower
controls through their geometry.

Custom controls may set `bmux_tui::hit::HitRegion::pointer_intent` when a role
default is insufficient. Use `PointerIntent::Auto` to return to role-derived
behavior rather
than maintaining a separate hover-to-shape table. Explicit shape or hidden
intent
remains explicit during capture. Use the existing interaction router's capture
and modal scope, not independent control-local terminal state. Modal
presentation
must publish the appropriate scoped hit geometry and exclude background input.

Components do not write OSC 22, hide the text caret, or own terminal cleanup.
The runtime's [pointer controls example](../tui-runtime/examples/pointer_controls.rs)
combines real `ButtonComponent`, `TextInputComponent`, and `ModalFrameComponent`
controls with a custom drag handle through the standard presenters and a single
committed interaction router. Its [run instructions](../tui-runtime/README.md#interactive-pointer-controls)
cover overrides, modal exclusion, capture, and explicit restoration.

`Hidden` is a host-dependent pointer intent, not a portable OSC 22 value. Pass
resolved intent through the terminal backend only when its output API supports
it; component availability alone makes no claim about the host terminal.

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

## Split buttons

`split_button::SplitButton` (feature `split-button`) pairs a primary action
with a `▾` toggle that opens an anchored menu of alternatives, such as a
"Copy as CSV" action with other formats in its menu. It is immediate-mode like
`Button` and `Menu`. The caller owns `SplitButtonState`, routes events through
`handle_event(area, bounds, …)`, paints the buttons with `paint`, and paints
the
popup last with `paint_menu`. The popup occludes what lies beneath it and opens
below the control, or above when there is no room, right-aligned and kept within
`bounds`. Outcomes are `Primary`, `Selected { index, id }`, `Opened`, `Closed`,
or redraw states. A click outside or Escape closes the menu, and `alt+↓` opens
it from a focused button. `split_button::choice(id, label, checked)` builds
check-marked items for mutually exclusive choices.

## Date pickers

`date_picker::DatePicker` (feature `date-picker`) edits a `chrono` date,
date-time, or time (`DatePickerMode::{Date, DateTime, Time}`), with optional
seconds and a Monday or Sunday week start. It renders a `‹ November 2026 ›`
header, a weekday row, a six-week grid (outside days dimmed, today marked, the
selection highlighted), and an `HH : MM : SS` row. The caller owns
`DatePickerState`, including the "today" reference passed to
`DatePickerState::with_today`; painting never reads the clock.
`DatePicker::preferred_size` returns the natural size for anchoring the picker
as a popup with `Rect::popup`.

Arrows move by day/week, PageUp/PageDown by month (with Shift, by year),
Home/End jump to the month's ends, and `t` jumps to today. Tab/Shift+Tab move
between the calendar and the time fields, where Up/Down wrap and digits type
values. Clicking a day selects it, double-clicking submits, the arrows and the
wheel over the calendar change the viewed month, and the wheel over a time
field changes it. Outcomes are `Changed(value)`, `Submitted(value)` (Enter or
double-click), `Cancelled` (Escape), `Redraw`, or `Ignored`.

`DatePickerPolicy::interactive()` is that compact picker.
`DatePickerPolicy::full()` adds a text field above the calendar that parses
as you type (`2026-11-04 14:30`, `2026-11-04T14:30:05`, `14:30`, and `now`,
`today`, `yesterday` or `tomorrow`, optionally followed by a time) and moves
the calendar and time with it. A line under the field shows the value readably
("We 4 Nov 2026, 14:30:05"), or why the text is not a value yet. The full
picker also adds `«` / `»` year arrows, a month grid (click the title or press
`m`), ▲ / ▼ buttons on each time field, and Now, Today, Cancel and OK
buttons.
Each policy flag can also be enabled on its own. The full picker fills the
width it is given (`DatePicker::size_for_width`); from `SIDE_BY_SIDE_WIDTH`
columns a date-time picker puts the time, Now and Today beside the calendar
rather than under it. With `offsets`, the field
accepts a UTC offset (`+02:00`, `Z`) and `DatePickerState::offset` returns it.
Relative words and the Now and Today buttons use the clock the caller passes
to `DatePickerState::set_now`, so hosts choose the time zone. While typed text
is not a value, `DatePickerState::text_pending` is true, and the picker does
not submit it. `DatePicker::hints` lists the focused part's keys.

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

Title-row buttons (collapse, maximize, close, …) are `PaneTitleAction`s. They
are right-aligned on the top border, and each registers a hit region
`<pane id>.action.<action id>`. When the row is too narrow, the leftmost actions
are dropped first, then the title is clipped; widths are measured in cells.
`Pane::title_hit(true)` also registers the whole title row as `<pane id>.title`,
for focus or double-click handling (use `bmux_text_edit::pointer::ClickTracker`
to count clicks). `Pane::title_layout` returns the same geometry the pane
paints.

```rust
use bmux_tui_components::pane::{Pane, PaneTitleAction};

let chrome = Pane::new()
    .title("Files")
    .title_hit(true)
    .action(PaneTitleAction::new("collapse", "▾"))
    .action(PaneTitleAction::new("close", "✕"));
```

`Pane::footer` writes text right-aligned on the bottom border, such as a count
or total that would compete with tabs and actions in the title row. It is
truncated with an ellipsis when the border is too short, and its geometry is
`Pane::footer_area`.

A title can also be a component: `PaneComponent::title_content` takes, for
example, a `Row` holding a label and a `tab_bar::TabBarComponent`. The pane
measures it and places it where a text title would go (after the actions take
their room, clipped to what remains), paints it there, and routes events to it,
so each tab's own hit region (`<tab bar id>.<tab id>`) works inside the title
row
and tabs clipped away register nothing. The whole-row `<pane id>.title` region
sits beneath the tabs.

A pane can carry the scrollbar of a viewport inside its content on its right
border: `PaneComponent::border_scrollbar(viewport, source)` takes the
viewport's layout id (`selectable_list::list_viewport_id` for a list) and a
`scroll_view::ScrollSource` holding its scroll state (a `Cell` of
`ScrollViewState` or of `SelectableListState`). It takes no content width,
appears only while the content overflows, and can be dragged or clicked through
its `<pane id>.scrollbar` region. Its geometry is
`Pane::border_scrollbar_track`, and `scroll_view::TrackScrollbar` paints and
drives the same kind of scrollbar on any other track.

## Resizable panel groups

`panel_group::PanelGroupComponent` lays out children along one axis with fixed
(`PanelSize::fixed`) or weighted (`PanelSize::flex`) sizes, per-panel min/max
constraints, and draggable dividers. State (`PanelGroupState`) is caller-owned:
keep it across frames and persist `sizes()` if layouts should be remembered.

* **Divider modes.** `PanelDividerMode::Gap` reserves a one-cell divider line
  between panels. `PanelDividerMode::Overlay { before, after }` takes no space:
  the handle covers the adjacent panels' own edge cells (typically their
  borders) and is highlighted in place, keeping their symbols, while hovered or
  dragged. For stacked bordered panes, `Overlay { before: 1, after: 0 }` keeps
  the lower pane's title row free for its own clicks; side by side,
  `Overlay { before: 0, after: 1 }` keeps the left pane's right border free for
  a border scrollbar.
* **Flexible resizing.** With `PanelGroupResizePolicy::preserving_flex(true)`,
  resizing rebases flex weights to the current cell lengths, so the resized
  proportions survive later area changes; otherwise resized panels become fixed.
* **Programmatic resizing.** `PanelGroup::resize_divider` moves a divider by a
  number of cells (for keyboard resizing) with the same constraint clamping as
  dragging.
* **Collapsing.** `PanelGroupState::collapse(index, cells)` shrinks a panel to a
  size that ignores its constraints and remembers the previous size;
  `expand(index)` restores it, and `set_size` on a collapsed panel changes what it
  restores to. Dividers next to a collapsed panel neither resize nor react to the
  pointer.

Applications that route pointer input themselves can call
`PanelGroup::handle_event` with the group's area from the presented layout
(`EventCx::find_visible_rect` on the group's layout id).

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
selection autoscroll, and bottom follow.
`ScrollbarAxisLayoutMode::OverlayOnOverflow`
draws an integrated scrollbar over the content only while it overflows, so
content width never depends on overflow. `ScrollExtent` captures a laid-out
viewport's content and visible heights for callers that scroll it outside a
component event pass. For large variable-height collections,
use `virtual-list`: item keys and revisions retain exact current-width layouts,
only viewport-intersecting items paint/register metadata, and keyed top or
bottom anchors survive insertion, removal, reorder, append, and reflow.
For long lists of one-row items, use `uniform-list`: rows are painted on
demand by index, so nothing is built or measured for rows outside the viewport.
Callers own `UniformListState` (selection plus a `ScrollViewState`) and may
drive it with their own key bindings or route events through
`UniformList::handle_event` for arrows, paging, wheel, click-to-select, and
scrollbar dragging.

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

## Live terminal output

The `terminal-view` feature shows a program's output as a terminal shows it,
live.
`terminal_view::TerminalState` is caller-owned: feed it what the program writes
(`feed`), report its end (`finish`), and keep it across frames. It parses
incrementally into a bounded `bmux_terminal_grid` terminal, so painting never
re-parses output. `TerminalViewComponent` paints it in one of two modes:

* `TerminalViewMode::Screen` shows the screen as the program drew it, with
  history above it, including the alternate screen and the cursor. Resize the
  terminal and the program to `TerminalState::requested_size`.
* `TerminalViewMode::Reflow` shows the output as a log, rewrapped to the view's
  width; `max_rows` shows only its end.

```rust
use bmux_tui_components::terminal_view::{TerminalState, TerminalViewComponent, TerminalViewMode};

let mut terminal = TerminalState::new(120, 30, 5_000);
terminal.feed(&bytes); // as output arrives, for example from `bmux_pty`
let view = TerminalViewComponent::new("output", &terminal).mode(TerminalViewMode::Reflow);
```

The view follows new output while scrolled to the bottom. `TerminalState`
implements `scroll_view::ScrollSource` and the view lays out its viewport as
`terminal_viewport_id`, so it works with a pane's border scrollbar and with
`ScrollExtent` wheel handling; `scrollbar` draws one inside the view instead.
Running the program (for example with `bmux_pty`) is the caller's concern.

`ScrollViewComponent::fill` makes a scroll view take whatever area layout gives
it, measuring its content at that width, for scrolled content whose size is not
known when the view is built.

## Activity spinners

The `spinner` feature's `spinner::Spinner` maps elapsed time to a frame, so a
spinner turns at the same speed however often the application redraws. Measure
elapsed time from one shared epoch to keep every spinner on screen in step.
Presets are `braille`, `dot` and `ascii` (also by name with `Spinner::named`);
`Spinner::new` takes custom frames and `Spinner::fixed` one frame for when
motion is off. `width` is the widest frame in cells, for reserving space, and
`next_change` says when to redraw next (`None` for a fixed spinner).

```rust
use bmux_tui_components::spinner::Spinner;

let spinner = Spinner::braille();
let span = spinner.span(epoch.elapsed(), style);
let redraw_in = spinner.next_change(epoch.elapsed());
```

## Trailing list segments

`SelectableListItem::trailing` attaches a segment, such as a count or status,
after an item's first line. The list places it at the width each row is
painted (after the highlight prefix and any scrollbar gutter) with
`bmux_tui::text::place_trailing`, so callers never measure rows themselves.
`TrailingPlacement` chooses `TrailingAlign::AfterContent` or
`TrailingAlign::Right`, the gap, and `keep_visible`: when set, long content is
shortened with an ellipsis instead of pushing the segment out of view.

```rust
use bmux_tui::prelude::{TrailingAlign, TrailingPlacement};
use bmux_tui_components::selectable_list::SelectableListItem;

let item = SelectableListItem::rich("app", "src/app.rs").trailing(
    "+12 −3",
    TrailingPlacement::new(TrailingAlign::Right).keep_visible(true),
);
```

`SelectableListItem::background` paints one row on a color across its whole
width, in place of the background its state style would give it (span
backgrounds still apply), for row-level highlights such as a flash that fades
out by blending with `Color::mix` each frame.

## Diff row layout

The `diff-viewer` feature exposes `diff_viewer::layout_diff_row`, which lays out
one logical diff item at a given width so a virtualized caller (for example a
`VirtualList` item) builds only the rows it shows. An item is either a unified
line (`DiffRowItem::Unified`: kind, old/new line numbers, content) or a
side-by-side pair (`DiffRowItem::SideBySide`: optional old and new
`DiffRowCell`s, each with its own line number). Content is a `DiffRowText`:
the line text, sorted intra-line `ChangedRange`s, and sorted caller-supplied
`DiffSyntaxRange` byte ranges (for example syntax tokens).

```rust,ignore
use bmux_tui_components::diff_viewer::{
    DiffGutterShape, DiffLineKind, DiffRowGutter, DiffRowItem, DiffRowOptions,
    DiffRowOverflow, DiffRowText, DiffViewerStyle, intraline_changed_ranges,
    layout_diff_row,
};

// Once per document: every row shares this gutter, so all rows align.
let gutter = DiffRowGutter::for_max_line_number(max_line, DiffGutterShape::LineNumbersThenMarker);
let options = DiffRowOptions::new(gutter).with_overflow(DiffRowOverflow::Wrap);

// Once per changed line pair (cache it with the item).
let changes = intraline_changed_ranges(old_text, new_text).unwrap_or_default();

let layout = layout_diff_row(
    DiffRowItem::Unified {
        kind: DiffLineKind::Added,
        old_line: None,
        new_line: Some(42),
        text: DiffRowText::plain(new_text).with_changed_ranges(&changes.new),
    },
    width,
    options,
    &DiffViewerStyle::default(),
);
let rows = layout.rows(); // each exactly `width` cells
```

Geometry and mapping:

* `DiffRowGutter` is derived once per document from the largest line number.
  `unified_width` and `side_width` report its cells. A number with more digits
  than the gutter shows a right-aligned `…`, never a partial number.
  `DiffGutterShape::LineNumbersThenMarker` shows both numbers then the marker;
  `MarkerThenLineNumber` is the compact single-number shape used by the
  whole-document card.
* Overflow is explicit: `Wrap` (grapheme boundaries; a grapheme wider than the
  whole content area gets its own unpainted row), `Truncate` (one row, `…` in
  the last cell when clipped), or `Scroll { offset }` (one row from a display
  column; graphemes cut by an edge leave blank cells).
* Narrow columns degrade explicitly (`DiffGutterMode`): the full gutter when it
  fits (content may then have no cells), otherwise only the marker cell from two
  cells, content only at one cell. Width zero yields one empty row with no
  segments. Side-by-side rows put a one-cell divider between the columns from
  three cells; `DiffSplitColumns` chooses an even split or a fixed old width.
* `DiffRowLayout::segments` maps every row of every present column to its
  `DiffSourceSide`, line number, content cells, UTF-8 byte range, grapheme
  range, and the column of its first painted grapheme. Rows added below the
  shorter side of a pair map to the end of that side's line.
  `DiffRowLayout::selection_cells(segment)` returns the row's selection units
  (cells and source bytes of every painted grapheme, at item columns), to
  register as a `selection::SelectionTextRun` with its origin at column zero of
  the segment's row.
* Tabs expand to tab stops: `DiffRowOptions::tab_width` (default
  `DEFAULT_DIFF_TAB_WIDTH`, eight; `with_tab_width`) sets the interval, counted
  in display columns of the whole line as if unwrapped. A tab paints as spaces,
  stays one unit mapped to its source byte (hits, `source_cells`, and selection
  units cover its whole expansion), moves to the next row whole when wrapping,
  and never occupies more cells than the content area.
* `diff_row_height(item, width, options)` returns the height `layout_diff_row`
  would produce, from the same wrap geometry, without building rows: one pass
  over the graphemes and no allocation. Use it to measure items that are not
  painted, for example as a virtual-list `measure_hint`.
* `DiffRowLayout::hit` resolves a cell to side, line, gutter or content, and the
  byte/grapheme under it; `source_cells` returns the painted cells covering a
  byte range. Graphemes are placed with `text_width::painted_grapheme_width`, so
  hit regions, selection geometry, and painted cells agree for CJK, emoji,
  combining marks, zero-width characters, tabs, and control characters.

`intraline_changed_ranges` compares a removed line with the added line that
replaced it by token (word runs, whitespace runs, other graphemes) using the
same LCS as line diffs, returning disjoint grapheme-aligned ranges per side and
merging ranges separated only by whitespace. Lines without a shared
non-whitespace token, or comparisons beyond the LCS budget, produce one range
per side; lines over 2,000 graphemes return `None`.

The whole-document APIs (`diff_viewer_rows*`, `diff_viewer_document_*`) render
their card rows through `layout_diff_row` with unchanged output.

## Rich and large tree views

The `tree-view` feature's `TreeViewItem` carries a plain `label` or a styled
`rich_label` (`TreeViewItem::rich`, `.rich_label(line)`), plus right-aligned
`trailing` decorations such as counts or badges (`.trailing(lines)`,
`.trailing_decoration(line)`). Span styles are patched over the row's state
style; disabled rows patch the disabled style over them.

```rust,ignore
use bmux_tui::prelude::{Line, Span};
use bmux_tui_components::tree_view::{
    TreeViewComponent, TreeViewItem, TreeViewLabelOverflow, TreeViewTrailingLayout,
};

let item = TreeViewItem::rich("src/lib.rs", Line::from_spans([Span::styled("lib.rs", file)]), 1)
    .trailing_decoration(Line::from_spans([Span::styled("M", modified)]))
    .trailing_decoration(Line::from_spans([Span::styled("+12", added)]));

let tree = TreeViewComponent::new("files", &items, &state)
    .items_revision(items_generation)
    .trailing_layout(
        TreeViewTrailingLayout::new()
            .min_label_width(6)
            .label_overflow(TreeViewLabelOverflow::Ellipsis),
    );
```

Row geometry, in order: indentation, disclosure marker, one separator cell,
then the room holding the label and decorations. `TreeViewTrailingLayout`
makes narrow rows explicit:

* Decorations sit against the row's right edge, `gap` cells after the label
  and `spacing` cells apart, measured by display width.
* They are shown only while `min(label width, min_label_width) + gap +
  decorations` fits the room. Otherwise whole decorations are hidden starting
  with the first (leftmost), so put the most important decoration last. A
  decoration is never cut and never overlaps the label.
* The label takes the remaining room and is shortened by `label_overflow`:
  `Clip` (the default; a wide grapheme that would straddle the edge is
  omitted) or `Ellipsis`. A spare cell becomes padding, so decorations stay
  flush right.
* An indentation wider than the row hides the marker, label, and decorations
  while the row keeps its width and hit region.
* The intrinsic width includes the gap and every decoration.

A click selects a row; with `.toggle_on_click(true)` a click on an expandable
row also toggles its expansion and reports `TreeViewOutcome::Toggled`, as file
trees usually do. Leaf rows still only select.

Scale: the tree is one leaf whose height is its visible row count, placed in a
`ScrollViewComponent` (or another clipping parent). Painting, row hit regions,
and semantic regions cover only rows inside the paint clip, and a pointer
position maps to its logical row through the event transform, so hits stay
correct at any scroll offset. Pass `items_revision` (changed whenever any item
changes) to retain the visible-row projection and intrinsic width in
`TreeViewState` across frames until the expansion changes; the revision then
hashes the token instead of every item. A tight layout width never measures
labels. `visible_count`, `visible_source`, `visible_row_of`, and `row_rect`
read the same projection (for example to ensure the selected row is visible).

State is keyed: expansion is a set of item ids (`set_expanded`,
`set_expanded_ids`), and keyboard and pointer selection record the selected
item's id, so the selection follows its item when rows above it appear or
disappear. `set_selected_id` selects by id; while that item is hidden or
absent no row is selected and the id is kept. `set_selected_visible` selects a
row position and clears the id. `TreeViewComponent::selected_visible` resolves
the keyed row against the current items.

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

### Rebinding text-input editing keys

`with_text_bindings(&bindings)` on `TextInputControl` or `TextInputComponent`
routes every non-insert editing key through a `bmux_text_edit` `TextBindings`
table, including undo/redo (the table then supersedes the history bindings in
`handle_key`). Unbound printable strokes are still inserted, Enter keeps its
`KeyboardPolicy` behavior, `selection_keys` still gates selection actions, and
vertical moves keep edge signals and soft-wrap-aware movement.
`policy.keyboard.keymap.bindings()` reproduces the behavior without a table.

### Masked (secret) text input

`TextInputState::with_mask(TextInputMask)` (or `set_mask`) turns any text input,
including one inside `TextInputBoxComponent` and its form-field chrome, into a
secret field for API keys and passwords:

- `TextInputMask::bullets()` / `Mask(char)` paints one glyph per grapheme
  (default `•`); wrapping, layout height, cursor and click mapping use the glyph
  count, never the secret's own display width. Newlines are masked too.
- `TextInputMask::Hidden` paints nothing; a focused field shows only its cursor.
- Selection is disabled (shift keys, select-all bindings, drag and multi-click),
  so nothing can be copied out. Word and hard-line motions and deletions span
  the whole secret so the cursor does not reveal its structure.
- `Debug` prints `<redacted>` and `snapshot()` records no text or cursor.
- `wipe()` and dropping a masked state zeroize the text and its undo snapshots.

`TextInputPolicy::secret_field()` is a single-row preset where Enter submits and
only click-to-cursor is enabled. Read the value with `state.buffer().text()`,
then call `wipe()`.

```rust
use std::cell::RefCell;

use bmux_text_edit::TextEditBuffer;
use bmux_tui_components::text_input::{TextInputMask, TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{TextInputBoxComponent, TextInputBoxPolicy};

let state = RefCell::new(
    TextInputState::new(TextEditBuffer::new()).with_mask(TextInputMask::bullets()),
);
let field = TextInputBoxComponent::new("api-key", TextInputPolicy::secret_field(), &state)
    .label("API key")
    .policy(TextInputBoxPolicy::labeled_field().focused(true));
```

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
