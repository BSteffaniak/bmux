# Pane output performance plan

Status: plan. Nothing here is implemented yet.

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

## Order

1. Phase 0: the corpus and `bmux-bench pipeline`; then `replay` with end-to-end markers.
2. Phase 1 and the first step of phase 2: the held-back frames and round trips are what is felt.
3. Phase 3: region scrolls and the single server emulator.
4. Phase 4: scroll passthrough, cell diffs and the writer thread.
5. Phase 2's push and credit-based flow control.
6. Phase 4's single renderer.

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
