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
| Pane reader | `runtime::ingest_bench::ingest_metrics` (`plugins/pane-runtime-plugin`) | `images` (image interceptor, registry and protocol replies), `metadata`, `modes`, `parse` (grid parse and output push) |

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
instructions allow 0.5% plus 1,000 per frame; units of work allow 0.2%. Baselines are per OS; CI
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

`scripts/perf-render.py compare e06ff6ab HEAD`: the first revision that measured (master plus
the metrics, nothing trimmed) against the allocation work, per frame at 420×90 on an Apple
M-series machine. Bytes written to the host are identical in every scenario: no behavior changed.

| Scenario | Phase | Allocations | Instructions |
| --- | --- | --- | --- |
| Full repaint | client apply | 42,274 → 0 | 65.0M → 53.2M (−18%) |
| Full repaint | client render | 120 → 25 | 22.8M → 22.7M |
| Full repaint, two panes | client render | 212 → 27 | 22.7M → 22.7M |
| Streaming text | client apply | 1,409 → 3 | 2.24M → 1.89M (−15%) |
| Streaming text | client render | 33 → 27 | 1.37M → 1.36M |
| Moving sixel image | client render | 6,552 → 47 | 22.5M → 6.7M (−70%) |
| Kitty image beside streaming | client render | 56 → 37 | 5.07M → 5.06M |
| Full repaint | pane reader, all stages | 101,285 → 33 | 136.7M → 104.9M (−23%) |
| Streaming text | pane reader, all stages | 3,379 → 14 | 4.76M → 3.79M (−20%) |
| Text with Kitty images | pane reader, image stage | 9,750 → 31 | 6.47M → 3.62M (−44%) |

What changed, each its own commit with its numbers: escape-sequence parameters parse into a stack
buffer; grid rows are erased and scrolled in place instead of rebuilt; the parser prefix, the
reader's per-chunk buffers, the frame's changed-column runs and the frame output buffer keep their
storage; metadata-free chunks pass the shell metadata parser uncopied; the sixel encoder writes
straight into its output; the image content cache survives the screen clear that repairs raster
images; image keys are shared, not copied; display capture copies images only while recording.

## Known costs left

- **The pane reader parses all output twice.** The cursor tracker keeps a second full terminal
  grid (with its own 10,000-row history) to answer cursor queries and to place and evict images;
  the main grid already holds the same state. Merging them would roughly halve the reader's parse
  CPU and its memory per pane.
- **A raster image repaints the whole screen when it moves.** Raster protocols cannot delete a
  placement, so the presenter clears the screen and repaints every cell (about 104 KB per frame for
  a moving sixel image); repainting only the old footprint would cut that to its cells.
- **The render rebuilds a compositor plan every frame.** `render_attach_scene` builds a
  `RetainedCompositor` from the scene to find the surfaces to repaint (about 15 small allocations
  per frame), though the attach client keeps one across frames.
- **Pane image payloads are compared by content every frame**, a scan of each image's bytes,
  because a pane's image may change without a new id; a revision from the server would avoid it.
- **History is held at full width**: a 420-column pane's 10,000 rows take about 70 MB.
