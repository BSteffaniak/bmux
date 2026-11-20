# Terminal pipeline architecture

The design the terminal pipeline is moving to: pane output parsed once, clients kept current by
structured damage instead of re-parsing bytes, rendering driven by what changed, and one plugin
model over all of it. This document is the plan and the record of why; each phase lands as its
own measured commits (see [render-performance.md](render-performance.md) for the gates).

## Why: where the time goes today

Profiles of a full repaint at 420×90 (37,800 cells), from `scripts/perf-render.py work`:

- **The server scans each PTY byte about seven times.** The grid's parser, the image interceptor,
  the shell metadata filter, the protocol reply engine, the mode tracker, and the fan-out
  buffer's escape-phase and synchronized-update scanners each run their own byte loop. Only the
  grid's parse is necessary; the rest re-detect sequences it already recognizes.
- **The client parses everything again, twice.** The server forwards (filtered) bytes; the
  client parses them into its own grid and through a protocol tracker that duplicates that
  grid's state. Client cost therefore grows with how much a program writes, not with screen size:
  every byte of `cat bigfile` is parsed on the client, even for frames never shown.
- **Rendering converts every cell through layers.** Each frame takes grid cells, converts style
  per cell (`cell_style_to_tui`, `Style::patch`, `Color::quantized`), re-validates symbols,
  writes a second buffer, diffs it and re-emits escape sequences: about ten helper calls a cell
  for work that depends only on the style, of which a screen has a handful.
- **Structured deltas exist but nothing produces them.** `GridDeltaBatch` is built only by
  snapshot diffing in tests; the server's delta log is filled only in tests. Mobile, which relies
  on deltas, falls back to a full grid snapshot on every output event.
- **Plugins are polled and re-serialized.** Render extensions are called every frame, the
  compositor plan is rebuilt every frame, and plugin state such as a decoration scene is relayed
  as full JSON on every change, including animation ticks.

tmux and zellij parse once on the server and send each client only the resulting screen changes;
Alacritty, WezTerm and Ghostty parse once and draw from the grid. That is the shape to reach,
without giving up client-side composition and plugins.

## Invariants

These hold at every step; a step that cannot keep one does not land.

- **Behavior parity.** What a user sees and what a program in a pane observes are unchanged:
  screen content, cursor, scrollback, selection, images, mouse and input modes, query replies,
  synchronized-update gating, reattach and gap recovery. Checked by the differential test below,
  the attach compatibility matrix and the existing suites.
- **Every plugin keeps working.** Plugin contracts are replaced in place: a change to a contract
  migrates every in-repo plugin that uses it in the same change, so the tree never carries two
  versions of one API. Plugins built outside this repository rebuild against the new contracts.
- **Measured.** Each step reports allocations, units of work, instructions and memory against
  the baselines, and adds scenarios for any path it creates.
- **Fail closed on desync.** A client that cannot prove its replica matches a server revision
  resynchronizes from a snapshot; it never renders guessed state.

## Target architecture

```text
PTY ─▶ pane engine (server, one pass) ─▶ grid + damage log ─▶ frame producer (per client)
            │                                                     │
            └─▶ observers: images, shell metadata,                ▼
                query replies, plugin interests           replica (client) ─▶ render from damage
```

### 1. Pane engine: one pass per byte

One parser per pane drives the grid and a set of observers. The grid's parser already
recognizes every sequence; observers are dispatched from it by interest, so a pane pays only for
what is registered.

- **Core transforms and observers**, statically dispatched (the same unit model plugins use,
  below): image protocols (DCS/APC/OSC strings handed to the
  image registry instead of the grid, placed at the grid's cursor as of that byte), shell
  metadata (its private OSC), query replies (DA, DSR, XTVERSION and the rest, answered in order
  with the grid's cursor as of the query), pointer shape (already in the grid).
- **Modes come from the grid.** The grid already tracks every mouse and input mode the server's
  mode tracker follows; the tracker is deleted. The grid gains DECSTR (`CSI ! p`), which the
  tracker handled and the grid does not.
- **Images move with the grid in order.** Today the reader re-feeds a chunk byte by byte whenever
  an image exists so placements follow erases and scrolls; with images as observers of the same
  pass, they see those events in order without a second loop.
- **The raw byte stream becomes opt-in.** It is published only while something subscribes
  (recording takes the pre-filter PTY bytes as now). The fan-out buffer's escape-phase and
  synchronized-update scanners run only for raw subscribers; the engine knows synchronized-update
  state already.

### 2. Damage and frames: replicate state, not bytes

- **The grid records damage as it writes**: which rows changed since a revision, and scrolls as
  operations (a scroll is one operation plus the new rows, not every row). This replaces snapshot
  diffing.
- **One message for snapshot, delta and recovery.** A frame carries, for a client's acknowledged
  revision: scroll operations, changed rows (text and style runs, styles by palette id, with any
  palette entries the client lacks), cursor, modes, and the state below. A client whose revision
  fell out of the damage log gets the same message with every row. Gap recovery, reattach and
  attach are one path.
- **Coalesced by construction.** Frames are built when the client is ready, from the current
  grid, so intermediate states are never sent: client cost is bounded by screen size times frame
  rate, not by output volume.
- **Synchronized updates move to the server.** Frames are not built while an update is open
  (bounded by the current 150 ms timeout), preserving today's client-side gating.
- **State the client used to derive from bytes is in the frame**: mouse and input modes,
  alternate screen, pointer shape, and new state the pipeline drops today (title, working
  directory, hyperlinks; see the plugin model).

### 3. Client replica

A replica crate holds a pane's rows, palette and terminal state, applies frames by copying rows,
and exposes damage to the renderer. It has no parser. The CLI attach client, mobile, the cluster
client and playbook use it; the client's protocol tracker is deleted.

### 4. Rendering from damage

- Only damaged rows are rendered; a full repaint is the case where every row is damaged.
- Styles are converted once per palette id to the host's escape sequence (per color depth), not
  per cell. Theme content effects become a palette mapping (style id to effected style id),
  cached until the theme changes.
- Symbol safety is decided when a cell is written, stored as a cell flag.
- The compositor plan is retained across frames and rebuilt only when layers change.

### 5. Compact cells

A 16-byte cell (code point or grapheme-table index, style id, width and flags) halves memory
traffic and makes writing a cell one store. Last, because every earlier phase touches cells.

## Plugin model

One model, pay for what you use, typed events instead of bytes, retained output instead of
polling. It is built on the same pass and the same damage as the core.

- **Pane observation by interest.** A plugin declares what it wants from a pane: specific OSC
  numbers, prompt marks (OSC 133), bell, title, working directory (OSC 7), hyperlinks (OSC 8),
  notifications (OSC 9/777), clipboard writes (OSC 52), mode changes, images, or the raw byte
  stream. It receives typed events with the grid revision and position they occurred at. Events
  are queued off the reader thread (in-process: the existing `Arc<dyn Any>` event bus, no
  serialization; dylib and process plugins: the codec), so a slow observer never stalls a pane.
- **Transforms on the same pass.** A plugin can also filter or rewrite what it declared interest
  in, without a pass of its own and without cost to panes that have no transform:
  - The parser already splits output into units (a text run, one control, one complete escape
    sequence or string). A transform registers for unit kinds and keys (text runs, OSC 52, a CSI
    final byte, DCS) and is called with only the matching units, already delimited and decoded.
    The core observers (images, shell metadata) are transforms of this kind.
  - It returns keep, drop or replace. Replacement bytes are fed back into the same parser, past
    the transform that produced them (and any earlier in the chain), so a transform cannot loop.
  - Transforms run inline on the reader thread, so they are native (bundled or dylib) and
    synchronous; each is measured by the ingest benchmarks with a transform installed. Process
    plugins, a pipe round trip away, observe.
  - Interest is compiled into the parser's dispatch (a bitset per unit kind and a small table of
    keys), so the check for a pane with no transforms is the same branch an observer check is.
- **Sequences dropped today become core state and events.** Title and working directory live in
  the replica (tabs and status can show them); hyperlinks are stored per cell as link ids; bell
  and notifications are events. Clipboard writes are gated by the permissions plugin with a
  configurable policy: ask (the default: the first write from a pane prompts, and the answer can
  be remembered for that pane or session), allow, or deny.
- **Retained layers.** Plugin rendering is one vocabulary of layers with ids, z-order and
  revisions. A plugin publishes a layer when it changes; unchanged layers cost nothing per frame.
  The `AttachRenderExtension` compatibility methods collapse to this, and scene-protocol paint
  commands and `RenderOp` become one vocabulary. Plugin state relays send changes by revision,
  not the full state on every change.
- **Input with pane context.** Input hooks receive the pane's modes (mouse, keys, bracketed
  paste, alternate screen) from the replica. `AttachInputHook` and the presentation input
  registry become one routing table, hit-tested by the retained compositor.
- **One status-item model**, replacing the extension point and the tab-bar service side by side.
- **Visual adapters read the replica's damaged rows**, instead of a full projection on every
  change.

## Phases

Each phase is independently shippable, measured, and keeps every invariant.

0. **Guardrails.** Benchmark scenarios for the paths this creates: frame production and apply,
   many panes, reattach, bytes on the wire. A differential test that feeds the conformance corpus
   through today's path (bytes into a client grid) and the new one (frames into a replica) and
   requires identical screens, cursor, modes and rendered host output.
1. **Single pass on the server.** Observers for images, shell metadata and query replies; mode
   tracker deleted (DECSTR added to the grid); fan-out scanners only for raw subscribers.
2. **Damage and frames.** Damage recorded by the grid; the frame producer; one message for
   attach, delta and recovery; the reattach resume-offset ordering fixed on that path.
3. **Replica clients.** CLI attach on frames behind a setting, differential test and matrix
   green, then default; mobile, playbook, and cluster replication between nodes; the raw attach
   path and the client protocol tracker removed (the raw stream remains for recording).
4. **Render from damage.** Palette-keyed style cache, content effects as a palette mapping,
   symbol flags, retained compositor.
5. **Compact cells.**
6. **Plugin model.** Interest-based pane events and transforms, and the new core state
   (clipboard policy included); pane modes in input context; retained layers and revisioned
   relays; the duplicated APIs replaced in place, with every in-repo plugin migrated in the same
   change.

## Cluster replication: frames, pushed

Cluster panes used to replicate as raw bytes polled every 25 ms per pane (each poll a
linearizable read plus peer-authentication round trips), repaired with whole-grid JSON
snapshots, and parsed twice on the viewing node; the layout was polled every 100 ms. The cluster
has no users yet, so its protocol is replaced, not kept alongside:

- **Frames end to end.** Attach providers hand the renderer frames (`AttachProviderChange`
  frame changes replace byte appends and repaint repairs), applied with `apply_pane_frame`, so a
  remote pane is parsed once, on the node that runs it.
- **Pushed, not polled.** The CLI holds one push-capable connection per worker node (admitted
  once), and subscribes, as a frame observer, to the panes it shows: the pane runtime's existing
  frame push (wake, then build frames from the grid as it is when the connection can take a
  message: never a backlog). Each subscription is authorized by a lease the ingress signs and the
  worker verifies, and fenced to the execution's generation.
- **Replicas follow their source.** The controller keeps a replica grid per remote pane (from
  frames: size, content and input modes), forwards frames from it to its renderer, and only
  resizes the worker's PTY; it no longer parses, reflows or tracks protocol state itself.
- **Layout pushed too.** The ingress wakes subscribed clients when the cluster's control state
  advances (the consensus metrics watch); clients then read the layout once.
- Removed with it: the raw cursor reads, JSON terminal snapshots, gap repair, the worker's unused
  output buffers, and both polls.

## Status

Done (each a measured commit; see [render-performance.md](render-performance.md) for numbers):

- Phase 1, in part: input modes come from the grid (the mode tracker is gone, DECSTR added) and
  terminal queries are answered through a `SequenceObserver` on the grid's parse, statically
  dispatched so a grid observing nothing costs what it did.
- Phase 2: grids record damage for any number of replicas; frames are written into a caller's
  buffer and applied from it without allocating. A randomized differential test requires a
  replica's whole snapshot, revisions included, to equal its source after every frame.
- Phase 3, for the desktop attach client and mobile: frames are pushed (or read) per client and
  applied to replicas; the client no longer parses pane output, and the unused delta format is
  gone. Frames carry the source's revisions, synchronized-update state and benchmark replay
  markers; a grid resize wakes its clients.
- Phase 6, in part:
  - Pane terminal events (title, working directory, notifications, bell, prompt marks) are
    published on the event bus (`pane-runtime-terminal-events`) from the reader's one pass.
  - Input hooks receive the pane's input modes (mouse tracking and encoding, cursor and keypad
    modes, bracketed paste, alternate screen) from the client's replica.
  - OSC 52 clipboard writes travel only in the frames of clients viewing the pane, which apply
    `clipboard.osc52` (`ask` per pane while attached, the default; `allow`; `deny`). Queries stay
    unanswered. The grid collects whole payloads an observer asks for (`wants_osc`).
  - Output filters: native plugins register a `SequenceFilter` in the shared
    `SequenceFilters` (plugin state) with a declared `FilterInterest` (CSI and ESC final bytes, OSC
    numbers). The reader asks only interested filters, on the grid's own parse, before the grid
    applies the sequence; a dropped sequence is neither applied nor answered. With no filter
    registered the observer type has no filter check at all.
- Images, in part: image positions follow the grid's operations in its one pass
  (`SequenceObserver::operation`, called after each print run, control, sequence and DCS
  string), where a pane with an image on screen used to be fed to its grid a byte at a time. A
  chunk with no image start skips the interceptor entirely (`ImageInterceptor::passes_through`).
- Cluster replication on frames, pushed (see "Cluster replication: frames, pushed" above): attach
  providers hand the renderer frames; workers push frames to observing connections; control
  changes are pushed from the consensus metrics; nothing in the cluster path polls. A process
  test runs a one-node cluster end to end (launch, frames, input, resize, a new generation).
- The raw output buffer works only while someone reads raw bytes: otherwise a chunk only
  advances its offsets, and a returning reader resynchronizes from a snapshot. Frames take their
  synchronized-update state from the grid.

Changed from the plan: the image and shell-metadata scanners are folded in after every client
reads frames, since until then their only other job is producing the filtered byte stream clients
parsed. Rendering from damage comes before cluster replication, since rendering is now the
client's largest cost.

Output filters change what panes display (grids, and so frames), not the raw output stream:
recording records what the program wrote, and the playbook, which still reads raw bytes for its
display track and `pane_output` events, sees what was written. Filters keep or drop whole
sequences; rewriting sequences and filtering text runs come later, with the same interest model.

The playbook stays on raw bytes: its display track and `pane_output` events are defined as what
the program wrote, and with the buffer retaining only for readers, it costs nothing unless a
playbook runs.

Remaining: extracting images and shell metadata on the grid's parse instead of scans ahead of it;
compact cells (phase 5); the rest of the plugin model (rewriting filters and text filters,
retained layers with revisioned relays, unified render/input/status APIs).
