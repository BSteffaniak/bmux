# Render path performance: reproducible metrics

Wall time on a developer machine moves with load more than the differences worth measuring, so
the render path is measured by numbers that do not:

- **Allocations and allocated bytes** per frame and phase, and **bytes written** to the host
  terminal per frame: exact, run to run and machine to machine.
- **Units of work** per frame: every function entry and counted region a scenario executes, from
  a coverage-instrumented build. Exact on any machine and OS, with a per-function profile that
  says which code does the work and how often it runs.
- **Instructions retired** per frame and phase: each scenario runs three times and each frame
  keeps its fewest instructions, which strips interrupts and other noise (they only ever add).
  Run to run they differ by about 0.01% (0.07% at worst); they are tied to the architecture and
  toolchain, so baselines are per OS.

All three gate: CI fails on any increase. Latency on a real terminal (`bmux-bench replay`) is the
last check, not the gate. None of it touches application code: the counting allocator, probes and
instrumentation live in the benchmarks, `bmux_bench` and `scripts/perf-render.py`.

## What is measured

| Benchmark | Where | Phases per frame |
| --- | --- | --- |
| Client pipeline | `pipeline_bench::pipeline_metrics` (`packages/attach_pipeline`) | `apply` (pane output into the client's grids), `render` (painting and presenting), `observe` (preparing and publishing for an observer) |
| Pane reader | `runtime::ingest_bench::ingest_metrics` (`plugins/pane-runtime-plugin`) | `read`: one PTY read as the reader handles it (`follow_chunk`: images intercepted, shell metadata filtered, queries answered and the pane's grid parsed in one pass; then mode tracking and the output push) |

Scenarios: every generated corpus (`bmux_bench::corpus`) at 420×90; the busiest without scroll
passthrough, with an observer, and across two panes; scattered edits; Kitty and sixel images beside
streaming output and moving each frame; and, for the reader, text with a Kitty image every 30
frames. Each scenario runs 300 frames and measures the 270 after a 30-frame warm-up.

`pipeline_soak` and `ingest_soak` replay workloads ten times over and fail when live bytes keep
growing through the second half: history, caches, retained images and the image registry must be
bounded.

The primitives are in `bmux_bench`: `allocations` (a counting global allocator, per-thread counts,
process-wide live and peak bytes), `instructions` (`perf_event_open` on Linux, counting this
thread's user-space instructions; `proc_pid_rusage` on macOS, which counts kernel instructions too,
so the minimum across repeats matters there), `metrics` (per-phase recording, repeats, the JSON
report, comparison against a baseline) and `work` (reading coverage profiles into units of work).

## Running

From the repository root, in the Nix shell:

```sh
scripts/perf-render.py check                  # gate against docs/perf-baselines/render/<bench>-<os>.json
scripts/perf-render.py check --soak --work    # and the memory soaks and units of work (what CI runs)
scripts/perf-render.py update [--work]        # rewrite this OS's baselines after an improvement
scripts/perf-render.py compare master         # A/B: master against HEAD, every metric and its change
scripts/perf-render.py work --scenario full-repaint --top 20   # where a scenario's work goes
```

A check prints `improved (the baseline can be lowered)` when a number drops well below its
baseline; commit the updated baseline with the change so the gain cannot be lost. Allocations and
bytes allow 1% (or one allocation) for differences between platforms' standard libraries;
instructions allow 2% plus 1,000 per frame, since code generation moves them by about a percent (generic code is compiled into each crate using it, and with one codegen unit a change elsewhere in that crate shifts inlining); units of work, which code generation cannot move, allow 0.2%. Baselines are per OS; CI
uploads its reports, and a missing Linux baseline is taken from that artifact. A machine whose
instruction counts legitimately differ from the baseline's runs with `BMUX_BENCH_GATE_INSTRUCTIONS=0`.

Units of work come from a build with `-C instrument-coverage` in its own target directory
(`$CARGO_TARGET_DIR-coverage`), one process per scenario (`BMUX_BENCH_ONLY`, one repeat), read with
`llvm-profdata` of the same toolchain: `LLVM_PROFDATA`, the toolchain's `llvm-tools-preview`
component, or the flake's `llvm-tools` package (kept out of the dev shell so its toolchain, and the
target directory named after it, stay as they are). The benchmark library and modules are left out
of the counts. Units count executions, not instructions: they say what ran and how often, and
instructions say what it cost.

To find where time goes beyond counts (cache misses, memory waits), sample a scenario with
[samply](https://github.com/mstange/samply) or Instruments; to find where allocations come from,
use [dhat](https://docs.rs/dhat). `pipeline_bench_loop` replays one corpus (or, with
`BMUX_BENCH_IMAGE`, an image scenario) for external profilers.

## Results

The first revision that measured (`e06ff6ab`: master plus the metrics, nothing changed) against the
work since, per frame at 420×90 on an Apple M-series machine, from the committed baselines.

| Scenario | Phase | Allocations | Instructions | Bytes written |
| --- | --- | --- | --- | --- |
| Full repaint | client apply | 42,274 → 0 | 65.1M → 25.4M (−61%) | 128,475 → 128,475 |
| Full repaint | client render | 120 → 25 | 22.8M → 22.7M | 128,475 → 128,475 |
| Full repaint, two panes | client render | 212 → 27 | 22.8M → 22.7M | 128,226 → 128,226 |
| Streaming text | client apply | 1,409 → 3 | 2.24M → 1.03M (−54%) | 4,769 → 4,769 |
| Streaming text | client render | 33 → 27 | 1.37M → 1.36M | 4,769 → 4,769 |
| Moving sixel image | client render | 6,552 → 49 | 22.5M → 5.15M (−77%) | 104,180 → 85,639 |
| Kitty image beside streaming | client render | 56 → 37 | 5.09M → 5.06M | 64,010 → 64,010 |
| Full repaint | pane reader | 101,285 → 33 | 136.9M → 33.4M (−76%) | |
| Streaming text | pane reader | 3,380 → 8 | 4.75M → 1.33M (−72%) | |
| Text with Kitty images | pane reader | 11,715 → 31 | 8.99M → 3.25M (−64%) | |

Bytes written change only where intended (a moving raster image repairs its footprint, not the
screen). Memory: the pane reader keeps one grid instead of two, and history lines are stored
compactly, so a full 8 MiB history (the cap, charged by its cells' logical size) takes about 1 MB.

What changed, each its own commit with its numbers:

- Allocation: escape-sequence parameters parse into a stack buffer; grid rows are erased and
  scrolled in place; the parser prefix, the reader's per-chunk buffers, the frame's changed-column
  runs and the frame output buffer keep their storage; metadata-free chunks pass the shell
  metadata parser uncopied; the sixel encoder writes straight into its output; image keys and pane
  image bytes are shared, not copied; display capture copies images only while recording.
- Parser CPU: styles are interned through a hash index; runs of printable ASCII are written a
  row segment at a time; CSI and DCS parameters are taken in runs; the client's protocol tracker
  skips text and plain CSI sequences, which carry nothing it follows.
- The pane reader follows output through the pane's one grid instead of a second cursor-tracking
  grid.
- Images: the content cache survives the screen clear that repairs raster images; a raster image
  that moves is repaired by repainting its old footprint's cells, not by clearing the screen; the
  attach client matches pane images by their shared bytes, not by rescanning them.
- History lines are stored compactly. This one costs CPU for memory: each scrolled line is encoded
  once, about 8% more instructions per streaming frame.

## Known costs left

- **The client still parses pane output twice**, into its grid and through its protocol tracker.
  The tracker now skips text and plain CSI sequences, so it costs little; folding what it follows
  into the grid's parser would remove it.
- **The render rebuilds a compositor plan every frame.** `render_attach_scene` builds a
  `RetainedCompositor` from the scene to find the surfaces to repaint (about 15 small allocations
  per frame), though the attach client keeps one across frames.
- **Client render per cell.** Rendering a full repaint costs about as much as applying it (22.7M
  instructions); its per-cell work (style conversion, symbol checks, changed-run tracking) is spread
  across small helpers.
- **Raster images are re-sent when they move.** The repair is now the footprint, but a moved sixel
  image is written again in full (about 20 KB per frame here); terminals offer no move.
