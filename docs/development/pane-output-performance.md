# Pane output performance plan

Status: phases 0 through 5 are implemented; phase 4's single renderer landed as
[Phase 5](#phase-5-one-renderer). Phase 6 is implemented except its image step (see
[Progress](#progress)).

A full-screen application in a large pane (420×90, 37,800 cells) feels laggy inside bmux and
smooth in the same terminal without it. The application is not the cost: a code review scrolled
at that size paints and encodes a frame in about 1.5 ms and writes 20–50 KB per frame, every frame
wrapped in a synchronized update (DEC mode 2026), half of them scrolling with `SU`. While it
scrolls inside bmux, the server and client are mostly idle. The time is lost to how pane output
is framed, delivered and redrawn, not to raw CPU.

The goal is that bmux adds no visible latency to a terminal application at any pane size: a frame
the application finishes reaches the host terminal within one host frame, and the host terminal
receives about as many bytes as the application wrote.

## Budgets

Measured on the replay corpus (below) at 420×90, 60 frames per second, release build:

| Measure | Budget |
|---|---|
| Completed application frames held back by bmux | 0 |
| End to end, PTY read to host-terminal write, p95 | ≤ 4 ms |
| Server ingest per 50 KB frame (all parsing) | ≤ 1 ms |
| Client apply and render per frame | ≤ 2 ms |
| Bytes written to the host terminal / bytes the application wrote, scrolling frames | ≤ 1.5 |
| Requests between client and server per frame | ≤ 1 (0 with push) |

## What is wrong now

Each is cited so it can be checked; the first two are what is felt.

1. **Completed frames are held back.** The client asks whether a pane is inside a synchronized
   update from the server's *current* flag (`AttachPaneChunk::sync_update_active`, read from
   `pane.sync_update_in_progress` in `attach_pane_chunk_for_client`,
   `plugins/pane-runtime-plugin/src/runtime.rs`), not the state at the end of the bytes it was
   given. Once the application has begun its next frame, a frame the client already holds in full
   is deferred (`render.rs`, `sync_deferred`) until a later fetch happens to land between frames.
2. **Many round trips per frame.** The client pulls output in 8 KiB requests, at most 8 per
   drain (`ATTACH_OUTPUT_BATCH_MAX_BYTES`, `ATTACH_OUTPUT_DRAIN_MAX_ROUNDS` in
   `packages/cli/src/runtime/attach/runtime.rs`), plus an empty request to see the end; leftovers
   wait for the next wake or the 20 ms producer-barrier tick. The server allows a response of
   about 8 MiB. While a synchronized update is open and a round returns nothing, the client
   re-requests for up to 4 ms.
3. **Scrolls are repainted in full.** The client's renderer turns every row of the pane into an
   ANSI string each frame and re-emits any row that changed (`queue_pane_content_for_surface`,
   `packages/attach_pipeline/src/render.rs`). It never emits a scroll region, so an application's
   one-line `SU` becomes all 90 rows of 420 cells with their colors, several times what the
   application wrote, written with one blocking `write_all` on the client's event loop
   (`write_frame`, `packages/attach_pipeline/src/terminal.rs`).
4. **The server parses everything twice, one copy byte by byte.** Besides the main grid, the
   pane's cursor tracker feeds a second full `TerminalGridStream` one byte at a time, allocating
   per byte, to answer cursor and pointer queries (`PaneCursorTracker::process`,
   `image_terminal_bytes`, `protocol_reply_for_chunk` in `runtime.rs`). The client parses the
   same bytes a third time.
5. **Region scrolls cost rows × lines.** `scroll_region_up`/`scroll_region_down`
   (`packages/terminal-grid/src/model.rs`) shift the region once per scrolled line, cloning every
   row each time; IL/DL and an alternate-screen linefeed at the bottom take the same path.
6. **Smaller costs on the hot path:** the parse holds the output-buffer lock the client's read
   needs (`process_pane_output_with_pointer_queries`); every chunk is copied for recording with
   no recorder; each output event is JSON-encoded; the client makes an inline request every
   20 ms.

## Phase 0: measure

Nothing below lands without before and after numbers from these.

* **A replay corpus.** Captured frame streams, each a sequence of records (8-byte little-endian
  length, then the frame's bytes) starting from a full repaint: a code review scrolled at 420×90
  in unified and side-by-side layouts (captured with Bcode's review benchmark,
  `BCODE_BENCH_CAPTURE`), plus generated ones checked into the repository at modest sizes: a
  full-screen scroller using `SU`, one using linefeeds in a region, `cat` of a large file, a
  status-heavy redraw without scrolling, and one that never closes a synchronized update.
* **`bmux-bench pipeline <frames>`, in process.** Feeds a corpus through the real stages in one
  process: server ingest (output buffer, mode tracker, main grid, query answering), delivery
  (chunking as the client would receive it), client apply (reconcile, client grid) and the attach
  renderer, writing to memory. Reports per stage and per frame p50/p95/max, bytes to the host,
  the amplification ratio, and frames deferred. No cell cap (the existing internal workload stops
  at 32,768 cells). Runs in CI with relative regression thresholds.
* **`bmux-bench replay <frames> --fps N`, live.** Plays a corpus into a real pane at a fixed rate.
  Each frame carries a marker (a private OSC with the frame number) that the server notes with a
  timestamp when it reads it and the client notes when the frame containing it is written to the
  host; `bmux perf` records both, so end-to-end latency per frame is measured with shared ids,
  which no tool does today. Also reports drain rounds, deferred frames, render time and frame
  bytes from the existing attach telemetry.

## Phase 1: frames show when they are complete

* **The client tracks synchronized updates itself.** It already parses every byte it receives
  (`append_pane_output`). Its protocol tracker records `?2026h`/`?2026l` as it parses, so after
  each chunk the pane's state is exact at that point in the stream. Rendering uses that state;
  the server's flag stays only for consumers that do not parse (snapshots, status).
* **The server's flag becomes exact too.** The output buffer records the stream offsets where a
  synchronized update opens and closes (as it already records escape-sequence spans), and a read
  reports the state at its `stream_end`, not the reader thread's latest.
* **A finished frame is drawn at once.** When a chunk closes a synchronized update, the client
  renders without waiting for another wake. The 4 ms re-request loop goes away.
* **A stuck update cannot freeze a pane.** An update left open longer than a bound (around 150 ms,
  as terminals do) is drawn anyway, and counted.

## Phase 2: delivery

* **First, one request per frame.** Raise the per-request cap to what the IPC frame allows (about
  1 MiB per pane per request, well under the 8 MiB budget) and drain until empty without a round
  cap. Small, safe, and most of the round-trip cost.
* **Then push.** The server sends a pane's new bytes to each attached client when they arrive,
  on the client's existing push channel, one message per wake per pane carrying everything
  available (the unused `read_pane_output_for_push` is the start of this). No request per frame.
* **Flow control by credit.** The client grants a byte credit and renews it as it applies chunks;
  the server stops pushing to a client that has none. A client that falls far behind is not sent
  every intermediate byte: the server sends the pane's current grid (the existing snapshot
  resynchronization) and resumes the stream from there, so a slow consumer skips to the latest
  screen instead of replaying history.
* The inline 20 ms barrier request leaves the hot path, and output events use the binary codec
  instead of JSON.

## Phase 3: server ingest

* **One emulator on the server.** Cursor-position, pointer and other query replies come from the
  main grid's parser at the point in the stream where the query is parsed (a callback from the
  parser with the cursor state there). The cursor tracker's second grid and its per-byte loop are
  removed. Until then, it is fed the spans between queries in one call each.
* **Region scrolls in one move.** Scrolling a region by `n` rotates its rows once and resets `n`
  rows in place (reusing their allocations): O(rows) pointer moves, no cell clones. IL/DL and the
  alternate-screen linefeed use it.
* **Parsing does not block reads.** Bytes enter the output buffer under a short lock; the grid is
  parsed under the grid lock only, so a client read never waits on a parse.
* **No copies nobody needs.** The recording copy is made only while a recorder listens; chunk
  buffers are pooled and shared (reference-counted) between the output buffer and readers.

## Phase 4: client rendering and the host terminal

* **Scrolls stay scrolls.** The client's grid records the scrolls it applies (region and rows)
  since the last render. When a pane scrolled, the renderer moves the host terminal's copy of
  those rows with a scroll region and `SU`/`SD`, shifts its retained rows to match, and then diffs
  what is left. A pane narrower than the terminal uses left/right margins (DECSLRM) where the host
  supports them; otherwise full rows are scrolled when the columns outside the pane repeat on
  every scrolled row (as bmux_tui's terminal already decides), else it falls back to repainting.
  Never while anything overlaps the pane (floating panes, popups, images).
* **Cells, not rows.** The renderer diffs cells and writes only changed runs within a row, against
  rows it marks dirty while parsing, instead of formatting every row as a string each frame.
* **The host write never blocks the loop.** A writer thread owns the host terminal; the event loop
  hands it whole frames, and when the host is slower than the application, only the newest frame
  is kept.
* **Target: one renderer.** The attach renderer and bmux_tui's `Terminal` both diff cells and
  emit terminal output; the end state is panes drawn into a bmux_tui buffer and presented by its
  terminal, which already has cell diffs, scroll-region acceleration, synchronized output and
  frame pacing, so improvements land once.

## Phase 5: one renderer

The attach client draws its frame into a bmux_tui buffer and presents it through bmux_tui's
`Terminal`, replacing the attach renderer's own row diff, SGR encoder, scroll emission and
cursor handling (`packages/attach_pipeline/src/render.rs`, about 6,200 lines without tests).
bcode's TUI and the attach client then share one renderer, and an improvement to either is an
improvement to both.

### Where the two renderers stand

`Terminal` already has what the attach renderer had to grow by hand: a retained buffer and cell
diff, scroll-region acceleration with DECSLRM, synchronized output, image scenes with Kitty,
Sixel and iTerm2, pointer shapes, and a staged commit for asynchronous delivery. The attach
client already paints parts of its frame into bmux_tui buffers (help and prompt overlays, and the
observer's `PreparedPresentation` for browser delivery), only to turn them back into render ops.

At 420×90 `Terminal` is slower than the attach renderer now is, so moving onto it first would
undo phase 4:

* **Every frame touches every cell, several times.** Preparing a frame blanks or clones the
  spare buffer, partial damage copies retained cells back with a per-cell region test
  (`Buffer::restore_outside`), and the diff compares all cells. Damage limits only what is
  painted and written.
* **One scroll per frame, none with images.** `Frame` keeps only the largest scroll hint, and any
  image placement in the old or new scene turns acceleration off. The attach renderer scrolls
  each pane on its own and keeps scrolling while an image is elsewhere on screen.
* **Cells cost an allocation and a width lookup.** `Cell::symbol` is a `String`, and each
  `set_cell` recomputes the grapheme's width, which can disagree with the pane's emulator (an
  emoji with VS16, ambiguous widths). There is no bulk copy and no way to give a width.
* **Every style change resets.** The encoder writes `ESC[0;...m` and all attributes where the
  attach renderer writes only what changed.

### Steps

Each step lands on its own, with numbers from `pipeline_bench`, bcode's screen benchmark and
`bmux-bench replay`, and the pipeline benchmark's host-screen comparison after every frame.

1. **Damage-limited frames in `Terminal`.** A regional frame paints into the retained buffer in
   place (the damaged regions reset first) and diffs only the damaged rows and columns, so a
   frame costs its damage, not the screen. Full frames are unchanged.
2. **A scroll per region.** `Frame` keeps every non-overlapping scroll hint, and `Terminal`
   emits one scroll per hint that still saves cells. Images turn acceleration off only where
   they intersect the scrolled region.
3. **Cheaper cells.** Graphemes are stored inline up to a small size (as the grid's `CellText`
   is), and a `PaintCx` row write takes cells with their width, for callers that already know it.
4. **Incremental SGR.** The encoder writes the difference between the current and next style.
5. **Pane content through `Terminal`.** The attach client paints each pane's visible rows into
   the frame (with under-cells, content effects and selection) as damage from the rows its grid
   marked dirty, and gives the pane's viewport scroll as a scroll hint. A pane inside an open
   synchronized update is not painted or damaged, so its previous cells stay on screen.
   `ShownRow`, `scroll_host_rows` and the row output plans go away.
6. **Everything else through the buffer.** Borders, plugin surfaces, extension layers and the
   damage overlay are painted as render ops into the frame (`projection::prepare_content_overlays`
   already does this); help and prompt overlays paint directly instead of round-tripping through
   render ops. The retained compositor stays as the source of damage, occlusion and hit-testing.
7. **Images, cursor and pointer.** Pane images and Kitty overlays become image placements or the
   output hooks; the cursor and pointer shape come from the frame. The host writer stays the
   writer, with render deferral while it is behind.
8. **One frame for the terminal and the observer.** The observer takes its presentation from the
   same frame instead of capturing the renderer's output, and the capture machinery goes away.
   Render statistics and traces map onto the new stages, so `bmux perf` keeps its fields.

Steps 1 to 4 change only bmux_tui and are measured in bcode first. Steps 5 to 8 change the
attach client; the old renderer stayed reachable until step 8 and is deleted.

**The presenter replaces the renderer's bytes only when it beats them everywhere:** on every
corpus of the pipeline benchmark and in `bmux-bench replay`, it must take less client time and
write no more bytes than the renderer's own output, as well as show the same screen.

### How steps 5 to 8 landed

`Terminal` keeps a model of the screen, so pane content cannot move onto it while borders,
extensions and overlays still write their own bytes beside it. Instead of a new compositor, the
attach client presents the renderer's *capture*, the same paint operations the observer already
paints into a bmux_tui presentation:

* `Terminal::draw_over` presents a frame painted over the last one; its damage is the cells
  painted, so its cost is what changed.
* A pane's scroll is captured as a cell move (`PaintCx::move_cells`), which `Terminal` emits as a
  scroll region (one per pane, inside margins where the host has them) and diffs only the cells
  painted besides; pane rows are captured as runs of cells with the widths their grid measured.
* The attach client presents every host frame this way, an observed client's too. A frame
  holding what a terminal frame cannot show (a capture over its budget, a rejected local zoom)
  is not presented; the next frame repaints in full.

Since then:

* Frames painted over the last one are painted in place in the committed cells, which are
  marked where they change (the marks move with cell moves) and written without being compared
  to what they replaced: no spare copy, no model of the scrolled screen.
* Cell symbols are stored inline, the encoder writes styles and cursor moves without the
  formatting machinery, and buffer rows move in bulk.
* Grid rows carry a changed flag, so the renderer resolves only the rows the grid changed since
  they were drawn, in both paths.
* Pane images (Kitty, Sixel, iTerm2) and extension graphic overlays are placed through the
  terminal's output hooks. Imperative render extensions, which wrote their own bytes, are
  gone; nothing falls back to the renderer's own bytes.
* The host's terminal is the only record of what the host shows. A frame painted in place
  marks only the cells a write changes, so the renderer keeps no copy of the rows it drew
  (`ShownRow`, `prev_rows` and the row output plans are gone): it captures every row the grid
  changed whole, straight from the grid, and a pane remembers only whether its rows are drawn.
  A grid rebuilt from a snapshot (the scene pipeline, playbook traces) keeps only its changes
  since the grid drawn before it.
* An observed client's observer keeps the same frame: it applies the change the host's terminal
  made (its moves, then its changed cells copied from the terminal's cells) to its own copy,
  instead of painting the captured operations again. Its copy shares cells with the frame it
  publishes, and starts from a spare grid brought up to date rather than a copy of the screen.
  Both paint captured operations the same way where they do paint them (a headless client).

Instructions per frame at 420x90 (`pipeline_bench_loop` under `/usr/bin/time -l`, which other
load does not change; `BMUX_BENCH_MODE=observed` adds an observer), and bytes out per byte the
application wrote:

| Corpus | Renderer's bytes | Presenter, at the switch | Now | Now, observed |
| --- | --- | --- | --- | --- |
| SU scroll | 2.0M, x1.31 | 1.6M, x1.00 | 1.27M (0.63), x1.00 | 2.63M |
| Plain stream | 1.9M, x1.41 | 1.6M, x1.10 | 1.35M (0.71), x1.10 | 2.64M |
| Full repaint | 54M, x1.31 | 30M, x0.99 | 22.9M (0.42), x0.99 | 26.3M |
| Review, unified | 15.9M, x1.01 | 14.8M, x0.68 | 12.71M (0.80), x0.68 | 14.45M |
| Review, side by side | 16.6M, x0.96 | 14.9M, x0.61 | 12.99M (0.78), x0.61 | 14.76M |

An observer used to add 4.8M a frame on a scroll and 6.1M on a review (two copies of the screen
and a second paint); it now adds 1.3M to 1.7M (3.5M on a full repaint).

Live with `bmux-bench replay` (render start to host p50), the presenter matches the renderer's
own bytes on a review (1.05 to 1.23 ms against 1.07 to 1.56 ms) and on a scroll (0.39 ms
against 0.30 to 0.46 ms, within run-to-run noise), writing 17 to 21% fewer bytes.

## Phase 6: one paint path, no intermediate copies

Phase 5 left the renderer *recording* what it draws (a thread-local list of paint operations,
cloned once more for the observer) and a presenter replaying it into the terminal; pane rows
are copied into owned strings first, and extension output passes through a terminal-command
plan and a second conversion. Every client already presents through `Terminal`, so the
intermediate forms are only copies:

1. **One paint path.** The renderer paints into a `bmux_tui` frame through a `Canvas` (the
   frame's `PaintCx`, the frame's graphics protocol output and its rejection) instead of
   recording operations. `FramePresenter` runs the paint inside the terminal's in-place frame
   and abandons the frame when the render rejects it. Every client presents through a
   presenter; a headless client's terminal tracks changes without encoding them. Observers
   mirror the terminal's change, so there is one observer path. The thread-local capture,
   `staged_operations` and the per-frame operation list go.
2. **Zero-copy pane rows.** Pane rows are painted straight from the grid: cells borrow the
   grid's text, and each grid style converts to a `bmux_tui` style once (selection and content
   effects applied there), with no per-row `String` or `Vec`.
3. **Extensions paint directly.** Plugin render operations paint into the canvas; the
   terminal-command plan and its conversion go.
4. **One image pipeline.** Pane images and extension graphic overlays are `bmux_tui` image
   placements, encoded by the terminal for host and headless clients alike, instead of the
   attach renderer's own Kitty encoder.
5. **Cleanup and numbers.** Leftovers removed; `pipeline_bench`, observed and not, compared with
   phase 5.

Steps 1, 2, 3 and 5 are implemented. Step 4 replaces the attach client's three image paths
(pane images through its own `KittyHostState`, extension overlays through a Kitty-only cache and
writer, observer images through a separate conversion) with the one bcode uses: images are frame
contributions, the terminal reconciles them into an image scene and its change, and
`bmux_image::tui::TuiImageCompositor` alone encodes that change through the terminal's output
hooks (its pre-frame hook's repaint answer carries Sixel and iTerm2 repair). Every pane image can
take part: Sixel is decoded to pixels, iTerm2 carries a PNG/JPEG/GIF, and the registry keeps
completed Kitty transfers as PNG; the attach client already decodes all of them. In order:

1. `bmux_tui`: image payload bytes are shared (`Arc<[u8]>`), so contributing images every frame
   copies nothing; frames painted in place carry image contributions, and the terminal commits
   their scene and change with the cells.
2. `FramePresenter` owns the compositor and encodes the scene's change through the hooks;
   `Canvas::image` always contributes to the frame.
3. Extension graphic overlays are image placements; the attach graphics cache and Kitty overlay
   writer go (overlays gain Sixel and iTerm2).
4. Pane images are image placements with stable keys, converted once per image; the attach
   client's own Kitty host state and pane image path go.
5. The CLI's raster repair (forced full frames and ED2), the observer's own image conversion and
   the unused `images.decode_mode` setting go; observers mirror the terminal's image scene.

Known limitation, unchanged: the Kitty intake accepts direct (inline) transmissions only; file,
temporary-file and shared-memory media, zlib compression, Unicode placeholders and animation
frames never reach the registry.

## Order

1. Phase 0: the corpus and `bmux-bench pipeline`; then `replay` with end-to-end markers.
2. Phase 1 and the first step of phase 2: the held-back frames and round trips are what is felt.
3. Phase 3: region scrolls and the single server emulator.
4. Phase 4: scroll passthrough, cell diffs and the writer thread.
5. Phase 2's push and credit-based flow control.
6. Phase 4's single renderer, as phase 5.

Each step is its own change with its benchmark numbers, and the budgets above are checked at
the end of each phase.

## Risks

* **Protocol changes.** Push delivery, credits and exact sync state change the attach protocol;
  they are versioned so a client and server of different builds still work (falling back to pull).
* **Scroll passthrough correctness.** Overlapping surfaces, images, the cursor, and hosts that
  misread margins can corrupt the screen. It is gated per frame on the conditions above, and the
  pipeline benchmark compares the host screen it produces with a full repaint after every frame.
* **Recording and replay** read the same stream; their formats must not change underneath them.
* **Query answering from the main grid** must give the same replies at the same stream positions
  as the cursor tracker does now; the replay corpus includes queries, and both are compared
  before the tracker is removed.

## Progress

Implemented, each with its numbers in its commit:

* Phase 0: frame corpora, `pipeline_bench` (client) and `ingest_bench` (server) in process, and
  `bmux-bench replay` live: frames carry a private OSC marker, and with `BMUX_REPLAY_LOG` the
  server logs when it reads each marker and the attach client when it applies it, starts rendering
  it and hands the frame to the host, with the frame's bytes and rows moved.
* Phase 1: synchronized updates tracked by the client and exact on the server, the 150 ms bound,
  and finished frames drawn at once.
* Phase 2: one request per frame, then push (`attach-pane-output-push`): the server reads a
  client's output when it can queue it a message, so a slow client gets fewer, larger batches
  instead of a queue. Flow control is that read-at-send, not an explicit credit; a client past the
  buffer's retention resynchronizes from a snapshot as before. Input producer barriers are
  announced the same way instead of polled every 20 ms.
* Phase 3: queries answered from spans, region scrolls in one rotation, parsing outside the output
  lock, recording copies only while recording.
* Phase 4: scroll passthrough (inside left/right margins for a narrower pane), cell diffs, and the
  host writer thread with render deferral while the host is behind. A pane whose own content
  changed no longer gets a repair rect over all of it from the retained compositor.
* Phase 5 (steps 5 to 8): every attach frame is captured and presented through bmux_tui's
  `Terminal` (`FramePresenter`), including observed clients, the recording and mobile scene
  pipelines and playbook traces; graphics go through its output hooks. Imperative render
  extensions and the renderer's byte encoding (SGR and cursor writes, row strings, host scroll
  emission, the command optimizer) are deleted, and so is its copy of the rows it drew: the
  host's terminal is the only record of what the host shows, and an observer keeps the frame
  the terminal presented by applying the change it made. Instructions per frame at 420x90: SU
  scroll 1.29M, plain stream 1.39M, full repaint 23.8M, review 13.36M and 13.66M.

At 420x90, SU scroll, the attach client renders in about 1 ms and writes 12 KB a frame (from
2.9 ms and 169 KB); a review scroll writes about what the application wrote (x1.0, from x4.2).

* Phase 6 (steps 1, 2, 3 and 5): the renderer paints straight into the terminal's frame through
  a `Canvas` (no recorded operations, no per-frame copy of them); every client presents, a
  headless one through a terminal that encodes nothing, and observers always mirror the
  terminal's change; pane rows are painted from the grid without per-row allocations, and
  render operations are lowered straight into the frame. Instructions per frame at 420x90: SU
  scroll 1.27M, plain stream 1.35M, full repaint 22.9M, review 12.71M and 12.99M.

`bmux perf` keeps its fields except `pane_row_segments_emitted`: rows are drawn whole and the
host's terminal decides which cells to write, so `pane_rows_emitted` and `pane_cells_emitted`
count the rows and cells drawn.

Not done:

* Output events (`OutputAvailable`) are still JSON-encoded and broadcast; clients that have their
  output pushed ignore them.
* The end-to-end budget (p95 4 ms) was measured only on a loaded machine; quiet-machine numbers
  are still to be taken with `bmux-bench replay`.
