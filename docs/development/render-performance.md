# Render path performance: reproducible metrics

Wall time on a developer machine moves with load more than the differences worth measuring, so
the render path is measured by numbers that do not: **allocations and allocated bytes** per frame
and phase, and **bytes written** to the host terminal per frame. They repeat exactly between runs
of the same code, so CI fails on any increase. **Instructions retired** are reported too, for A/B
comparisons on one machine; they differ between machines and never gate. Latency on a real
terminal (`bmux-bench replay`) is the last check, not the gate.

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
process-wide live and peak bytes), `instructions` (`perf_event_open` on Linux, `proc_pid_rusage`
on macOS) and `metrics` (per-phase recording, the JSON report, comparison against a baseline).

## Running

From the repository root, in the Nix shell:

```sh
scripts/perf-render.py check            # gate against docs/perf-baselines/render/<bench>-<os>.json
scripts/perf-render.py check --soak     # and the memory soaks (what CI runs)
scripts/perf-render.py update           # rewrite this OS's baselines after an improvement
scripts/perf-render.py compare master   # A/B: master against HEAD, every metric and its change
```

A check prints `improved (the baseline can be lowered)` when a number drops well below its
baseline; commit the updated baseline with the change so the gain cannot be lost. A tolerance of
1% (or one allocation) absorbs small differences between platforms' standard libraries. Baselines
are per OS; CI uploads its reports, and a missing Linux baseline is taken from that artifact.

To find where allocations come from, profile a scenario with
[dhat](https://docs.rs/dhat) or Instruments; `pipeline_bench_loop` replays one corpus for
external profilers.

## Allocation work

Each change lands as its own commit with its before/after numbers.

1. Escape-sequence parameters (SGR and CSI) are parsed without allocating.
2. Rows are sized once and their storage reused.
3. The changed columns of each row, the frame output buffer and the compositor's per-frame plans
   reuse their storage across frames.
4. The image path keys and compares pane images without copying or rescanning their bytes, and
   reuses its per-frame vectors.
