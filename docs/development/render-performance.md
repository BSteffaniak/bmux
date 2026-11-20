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
scripts/perf-render.py profile pipeline --scenario full-repaint # where its time goes, sampled
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

Units of work and instruction totals do not say what each function costs: a call into a hasher
counts as one unit like a getter, an inlined body counts toward its caller, and library code such
as `memcpy` is not instrumented. `profile` ranks functions by sampled self time instead: it builds
a bench with symbols, replays one scenario in a loop (`pipeline_bench_loop`,
`ingest_bench_loop`) and samples it with the system profiler (`sample` on macOS, `perf` on Linux):

```sh
scripts/perf-render.py profile ingest --scenario full-repaint --top 30
```

Samples vary from run to run; use them to find what to change, and the gated numbers to prove the
change. To find where allocations come from, use [dhat](https://docs.rs/dhat).

## Results

The first revision that measured (`e06ff6ab`: master plus the metrics, nothing changed) against the
work since, per frame at 420×90 on an Apple M-series machine, from the committed baselines. The
client now keeps its panes current with frames (see
[terminal-architecture.md](terminal-architecture.md)): `apply` is decoding and applying them, and
`frame` is the server building them, which replaces the client parsing pane output.

| Scenario | Phase | Allocations | Instructions | Bytes written |
| --- | --- | --- | --- | --- |
| Full repaint | client apply | 42,274 → 0 | 65.1M → 4.9M (−93%) | 128,475 → 128,475 |
| Full repaint | server frame build | — → 4 | — → 3.2M | |
| Full repaint | client render | 120 → 24 | 22.8M → 17.5M (−23%) | |
| Streaming text | client apply | 1,409 → 3 | 2.24M → 0.18M (−92%) | 4,769 → 4,769 |
| Streaming text | server frame build | — → 3 | — → 0.13M | |
| Streaming text | client render | — | 1.35M → 1.11M (−18%) | |
| Moving sixel image | client apply + render | 7,270 → 52 | 23.6M → 5.0M (−79%) | 104,180 → 85,639 |
| Full repaint | pane reader | 101,285 → 2.8 | 136.9M → 14.4M (−89%) | |
| Streaming text | pane reader | 3,380 → 6 | 4.75M → 0.70M (−85%) | |
| Scrolling (SU) | pane reader | — → 0.02 | 0.91M → 0.49M (−46%) | |
| Text with Kitty images | pane reader | 11,715 → 29 | 8.99M → 0.85M (−91%) | |

Bytes written change only where intended (a moving raster image repairs its footprint, not the
screen). Memory: the pane reader keeps one grid instead of two, history lines are stored compactly
(a full 8 MiB history takes about 1 MB), and a client no longer keeps a parser per pane.

What changed, each its own commit with its numbers:

- Allocation: escape-sequence parameters parse into a stack buffer; grid rows are erased and
  scrolled in place; per-chunk buffers keep their storage; the sixel encoder writes straight into
  its output; image keys and pane image bytes are shared; frames are written and applied as bytes,
  each row decoded into the storage of the row it replaces.
- Parser CPU: styles are interned through a hash index; runs of printable ASCII are written a row
  segment at a time; CSI and DCS parameters are taken in runs.
- One pass on the server: input modes come from the grid, and terminal queries are answered by
  an observer of the grid's own parse, instead of separate scanners.
- Frames: clients (desktop and mobile) apply damage frames instead of parsing pane output, so
  their cost follows screen size and frame rate, not how much a program writes.
- Images: the content cache survives raster repairs; a moved raster image repaints only its old
  footprint.
- History lines are stored compactly (this one trades about 8% streaming CPU for memory).
- Found with `profile` (sampled time, which the counts above do not rank): style interning hashed
  two packed words with foldhash instead of SipHash field by field; one-byte cell text and frame
  text written without copy calls; frame lines of ASCII decoded a byte per cell; runs of one-byte
  cells painted and encoded at once; the output buffer's phase scan skips sequence bodies; chunks
  without an image start skip the image interceptor; escapes are found with memchr; control
  sequence parameters reuse one buffer in one pass; no per-chunk allocation or per-sequence
  memset in the reader; image positions follow each grid operation in the one parse, not a byte
  at a time.

## Known costs left

- **Client render** is still the client's largest cost (17.5M instructions for a full repaint,
  about 460 a cell), most of it encoding: the SGR changes a colourful screen needs, written cell
  run by cell run.
- **The server still runs separate scanners for images and shell metadata**, and the raw output
  fan-out for its remaining consumers (cluster replication, playbook, recording), now with
  memchr and a pass-through check, but still a scan each.
- **The render rebuilds a compositor plan every frame** (about 15 small allocations).
- **Cluster replication** still moves pane output between nodes as bytes.
