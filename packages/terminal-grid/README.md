# bmux_terminal_grid

Neutral structured terminal grid primitives for bmux. The crate stores parsed terminal rows, style runs, cursor state, alternate-screen state, and soft-wrap metadata so retained scrollback can be reflowed on resize without replaying raw PTY bytes.

## Embedding a transcript

Keep the `TerminalGridStream` at the execution dimensions. Feed recorded output and
execution resize events in order. **Do not call `resize` for presentation-only
changes**: it changes parser geometry, retention, and alternate-screen cells.

```rust
use bmux_terminal_grid::{ContentBudget, GridLimits, TerminalGridStream};

let mut stream = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
stream.process(b"a long command output line\r\n");
let budget = ContentBudget { cells: 100_000, bytes: 8 * 1024 * 1024 };
// Choose an identity unique to this source/content capture.
let mut content = stream.grid().capture_content(42, 100, budget).unwrap();
content.prepare(20, budget).unwrap();
let narrow = content.tail(28, budget.bytes).unwrap();
let anchor = narrow.anchors.first().copied();
content.prepare(60, budget).unwrap();
if let Some(anchor) = anchor {
    let row = content.resolve(anchor).unwrap();
    let restored = content.window(row..row + 28, budget.bytes).unwrap();
    assert!(!restored.rows.is_empty());
}
```

`capture_content` copies only the requested logical tail, joins soft continuations
across history and the live screen, and excludes unused trailing rectangle rows.
It preserves styles (IDs refer to the source palette), explicit hard breaks, and
wide-cell boundaries. It does not reconstruct text that the application omitted.

A caller owns and reuses each capture. A width index costs O(captured cells) work
and O(projected rows) metadata on a width change. Only one index is cached; a
same-width preparation is O(1). Window projection touches selected cells rather
than scanning the prefix of a long logical line. Capturing and indexing have
explicit cell-work and byte allowances; window allocations are also charged.
Exhaustion returns `HistorySliceError::BudgetExhausted`, not partial success.
A giant logical line that exceeds capture allowances requires an explicit larger
budget or an unavailable presentation; this API does not silently clip a logical
prefix. The older revision-fenced history-slice API remains available for streaming
export; it is not required for repeated transcript projection.

Anchors identify logical columns **within one capture**, not durable terminal
identities. Reuse them across width changes. On live edits, eviction, reset, or
recording replacement, create a new capture identity; resolving an old identity
fails. `revision()` exposes the captured content revision for caller invalidation.
`has_more_above`, `has_more_below`, and `history_truncated` distinguish a selected
window from missing source history. This is not a persistent snapshot format.

For live previews, use `grid.capture_viewport(identity, budget)` instead of
`capture_content`. It captures only the main viewport, joins its soft-wrapped
rows, and retains the blank cursor row needed for live sizing. It never copies
pending history, even when a long logical line crosses the viewport boundary.
`viewport_prefix_continues()` identifies that case: the visible fragment starts
at capture-local logical `(line: 0, column: 0)`, and widening cannot reveal its
hidden prefix. Anchors and width preparation work identically to content
captures. Alternate mode returns `Unavailable`; choose positioned cropping there.

For positioned output, use `screen_window(columns, rows, budget)` instead. It
crops the active screen without reflow, accepts horizontal offsets, and blanks
clipped wide-glyph fragments. Narrowing then widening does not destroy source
cells. Applications own overflow indicators, horizontal navigation, and the
choice between main-history projection and active-screen presentation.

`ContentRows.sources` parallels `rows` and `anchors` with half-open logical
source ranges. Their end columns count source cells, not display width: a wide
glyph projected at width one still consumes two logical columns. Empty lines
have equal start/end anchors but represent a visible row; `continues` distinguishes
soft continuation (including outside the capture) from a hard end. An exclusive
end is range metadata, not necessarily a resolvable content anchor. Consumers
must not reconstruct these ranges from painted text. Range metadata is included
in the projection allocation budget and does not scan unselected line prefixes.

For positioned selection, `screen_window_with_sources` adds one source-column
range per row, the content revision, and flags for blanked left/right wide-glyph
fragments. Display columns map by adding the source range start, including
implicit blanks; blanked fragments must not be copied as partial source glyphs.
The existing `screen_window` remains available without metadata overhead.

The generic `bmux_tui_components::terminal_viewer` uses these primitives for its
bounded compatibility renderer. Performance-sensitive live consumers should
retain the stream and capture themselves instead of passing cumulative bytes to
that stateless convenience component on every draw.
