# Bterm native rendering performance

## Build selection and optimized baseline

`cargo xtask bterm build/run` defaults to the development profile. Select an
optimized complete immutable bundle explicitly:

```sh
nix develop --command cargo xtask bterm run --profile release
# Optimized with symbols for a separate profiling experiment:
nix develop --command cargo xtask bterm build --profile profiling
```

The selected profile applies to the frontend, companion, and declared helpers.
`dev` maps to Cargo's `debug` output directory; other profiles retain their
names. Managed runtime identity does not change with the build profile.
Changing only the outer `cargo run --release` optimizes xtask, not its product.
Use the same profile throughout a timing experiment. The comparison below uses
`release` (opt-level=3, one codegen unit, no LTO), not the development profile.

Renderer benchmark build, from the workspace root (do not override the Nix
target directory):

```sh
nix develop --command cargo build --locked --release \
  -p bterm_native_renderer --example native_render_workloads
```

`BTERM_RENDER_TELEMETRY=1` enables bounded renderer summaries on product stderr
every 120 presentation attempts (disabled by default). Summaries include stage
timings, resource counters, and p50/p95/p99 for the last 128 attempts; they
contain no terminal text or image contents. Errors still fail output; enabling
diagnostics never acknowledges frames.

## Isolated native workload

`packages/bterm-native-renderer/examples/native_render_workloads.rs` is an
ordinary Cargo auto-discovered example using existing dependencies; no manifest,
renderer source, or `lib.rs` wiring is required. It creates its own main-thread
native window and renderer. It neither starts a companion nor connects to any
BMUX runtime, launches a PTY, or reads/writes managed runtime state. It requires
explicit `--window CASE` opt-in; do not run it during concurrent native testing.
Use a disposable desktop/login session if window/focus interference matters.

Run **one case per process**, then repeat the sequence at least five times on
each revision, alternating baseline/candidate order. For example:

```sh
nix develop --command cargo run --locked --release \
  -p bterm_native_renderer --example native_render_workloads -- --window warm-text
```

Cases:

| Case             | Work and interpretation                                                                                                                                                       |
| ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cold-text`      | Eight frames, fresh renderer/font caches/device per frame; initialization reported separately. OS font/driver caches are not flushed.                                         |
| `warm-text`      | Five warmup frames, 60 repeated full retained text frames. Exercises raster-cache reuse and GPU upload/submission.                                                            |
| `changing-text`  | Same bounds, revision text changes each frame.                                                                                                                                |
| `scroll`         | Every row's source line advances each frame; scrolling-like repaint, **not** a PTY scroll or scroll-blit benchmark.                                                           |
| `resize`         | Alternates full and three-quarter surface dimensions, recomputes bounded presentation geometry. Synthetic renderer reconfiguration, **not** an OS resize-latency measurement. |
| `scale`          | Alternates current OS scale and 1.5 times that scale, recomputes cell geometry. Not an actual monitor transition.                                                             |
| `static-png`     | Stable PNG payload encoded once before timed painting; warm samples must decode zero times.                                                                                   |
| `static-image`   | Stable caller-owned image key/content: one 128×128 RGBA image over text.                                                                                                      |
| `changing-image` | Same key, alternates two image payloads. Measures replacement, not unbounded accumulation.                                                                                    |
| `idle`           | Initial commit followed by `Damage::None` transactions: no paint/output callbacks or GPU submissions. Measures idle transaction overhead, **not** event-loop idle CPU/power.  |

Non-cold cases exclude five warmups and record 60 samples. The harness retains
only one previous/staged presentation and at most 60 timing records. Viewports
are capped at 80×24 cells, scene admission at 128 entries/2 MiB, drawable axes
at
4096 pixels, and reported OS scale at 0.5–4. A 120-second deadline is checked
at
frame entry; it cannot interrupt a blocked driver/event-loop call. The
coordinator should impose an external process deadline for unattended runs.
Closing, suspension, invalid geometry, or renderer failure aborts the run,
except
for the explicitly bounded initial occlusion handling described below. No
successful CSV is emitted until all requested samples succeed. No target is
silently substituted.

### Native startup and rejected acquisitions

The platform creates a hidden window and only calls `set_visible(true)`
**after**
the application's `Resumed` callback returns. It forwards redraw/focus/resize
notifications, but does not expose window visibility or `WindowEvent::Occluded`
notifications to this example. A redraw request from inside `Resumed` therefore
does not prove that the window has reached the compositor.

The example returns from `Resumed` without presenting, waits 50 ms on a finite
helper thread, and uses the platform wake proxy to request a later event-loop
redraw. Early redraws are ignored while this deferral is pending. This delay is
only scheduling/backoff, **never** evidence of visibility or successful output.
A successful renderer surface output is the drawable evidence. Focus or nonzero
size alone is not used as proof of successful presentation.

Before the first successful frame only, the exact renderer error
`surface acquisition: Occluded` is logged and deferred through another wake.
There are at most 40 startup deferrals, including the initial one; exhaustion
fails the run. Every rejected transaction passes through `present_prepared`'s
failure path and does not advance iteration, warmup, timing samples, or
committed
interaction metadata. Cold cases recreate their renderer on the next attempt;
warm cases retain caches warmed by rejected attempts, and the rejection count
must be retained with evidence. Once any frame succeeds, further occlusion is a
hard failure, not silently dropped sampling. Other errors never enter this retry
path. Stderr records each rejection and the deferral/rejection counts at the
first successful surface output. Persistent occlusion must not produce CSV.

Text/image fixtures are synthetic producer input to the existing authoritative
`PresentationState::prepare_bounded` path, not a replacement scene schema. The
caller invokes renderer output through `present_prepared`; errors propagate
before metadata commit. No application interactions are routed by the harness.
Display-free example unit tests cover real fixture changes, scene bounds,
failed-output retention, idle suppression, and strict startup-error
classification.

## Evidence and timing limitations

Stdout is CSV emitted only after completion:

```text
case,sample,setup_ns,configure_ns,prepare_ns,output_commit_ns,submitted,text_cpu_ns,image_cpu_ns,gpu_wait_ns,gpu_validation_ns,total_ns,atlas_upload_count,atlas_upload_bytes,cpu_vertex_allocations,cpu_vertex_allocation_bytes,vertex_count,vertex_bytes,gpu_vertex_allocations,gpu_vertex_bytes
```

Stderr identifies case, physical drawable size, OS scale, font size, bounds,
warmups, and sample count. Keep it with CSV. `setup_ns` is renderer construction
(cold samples); `configure_ns` is synthetic resize/scale plus geometry work;
`prepare_ns` includes fixture construction and bounded retained preparation;
`output_commit_ns` includes renderer output and caller commit. Idle measured
samples must all have `submitted=false`; all other cases must have `true`.
These are CPU wall-clock durations, potentially including swapchain blocking,
**not GPU completion times, input-to-photon latency, or real terminal
throughput**.
No artificial GPU wait is inserted by the harness. Do not sum percentiles across
columns. Renderer telemetry is explicitly enabled on every renderer, including
cold recreations. Successful `PresentReport::telemetry` provides each frame's
measurements directly, with no cumulative subtraction or percentile polling.
Idle frames have zero telemetry fields because no renderer call occurs; they do
not repeat the initial frame's measurements. Missing telemetry on a submitted
frame aborts the run rather than silently emitting zeros.

`text_cpu_ns` and `image_cpu_ns` include projection and upload enqueue work;
`gpu_wait_ns` measures renderer device polling, `gpu_validation_ns` subsequent
error-scope resolution, and `total_ns` the renderer attempt, not caller commit.
These timings overlap `output_commit_ns` and are not additive independent
stages.
Atlas upload count/bytes include the four-byte solid texel reset at frame start.
CPU/GPU vertex allocation counters describe requested vertex allocations, not
all renderer allocations or resident GPU memory; allocation bytes sum requested
capacities, including reallocations. `vertex_count`/`vertex_bytes` describe the
logical frame geometry. The API does not expose a separate GPU vertex-upload
byte counter. Keep telemetry enabled identically in baseline and candidate;
its instrumentation overhead is part of these measurements.

Capture exact Git revision and dirty diff (including concurrent lanes), Nix
lock/toolchain identity, profile, OS/hardware/GPU, adapter/backend if available,
display resolution/refresh/scale, power mode, font environment, executable
SHA-256, command, exit status, stderr and CSV. Save evidence outside the
checkout.
Use an explicit built executable path and hash for repeat runs after locating
the Nix-selected Cargo target directory; never substitute an installed Bterm.
Report per-process medians/p95s and cross-process variation, not a single best
sample. Do not claim a speedup until both revisions have complete successful
runs under comparable conditions.

Real input latency needs a separate isolated full-product test: timestamp an
owned injected input at ingress, correlate its authoritative retained delivery
and successful output, and separately measure display completion externally if
claiming input-to-photon. This harness deliberately does not fabricate such a
metric. Likewise, true event-loop idle CPU/wakeup evidence needs a
fixed-duration
no-input isolated process observation; the `idle` case is only a transaction
micro-workload.

## Validation commands

Run the required Nix clippy before full nextest, then machete. Also explicitly
run the example's display-free tests
because examples are not necessarily included by the default nextest target set:

```sh
nix develop --command cargo clippy --all-targets -- -D warnings
nix develop --command cargo nextest run --no-fail-fast
nix develop --command cargo machete --with-metadata
nix develop --command cargo test -p bterm_native_renderer --example native_render_workloads
```

Then build/run the isolated optimized workload cases above, serially. Additional
CLI/runtime or plugin checks are the coordinator's responsibility if other lanes
change those areas.

## Initial measured evidence (not a latency claim)

Apple M4, macOS 27.0.1, 1920×1280 drawable, scale 2, 80×24 cell ceiling;
optimized finite harness, 60 retained warm samples per process. Initial baseline
and candidate runs used the same non-PNG cases. A candidate-only PNG case was
added afterward; it has no comparable baseline. Values below are medians from
one baseline/candidate process pair, not a statistically established speedup.

| Metric                                          | Baseline | Candidate |
| ----------------------------------------------- | -------: | --------: |
| Warm text CPU                                   |   306 µs |    247 µs |
| Warm text atlas upload bytes/frame              |   41,040 |         0 |
| Warm text GPU vertex allocations/frame          |        1 |         0 |
| Warm text synchronous GPU wait                  |  1.29 ms |   0.94 ms |
| Warm text output transaction                    | 16.35 ms |  16.28 ms |
| Static RGBA image CPU                           |    18 µs |     29 µs |
| Static RGBA upload bytes/frame (including text) |  106,576 |         0 |
| Static PNG CPU (candidate only)                 |        — |     16 µs |
| Cold-text output transaction                    |  8.91 ms |  10.12 ms |

All ten candidate cases completed successfully. Warm static PNG and RGBA
samples reported one decoded-cache hit and zero decodes per frame. Warm uploads
and GPU vertex allocations were zero. Raw-image content hashing/exact comparison
adds CPU cost; cold construction also regressed in this pair. These costs are
explicit tradeoffs, not hidden by the upload reduction. Output remained display
paced near 16.4 ms. No input-to-display, visual-equivalence, or real idle CPU
result has been established. Repeat independent runs and larger/cache-pressure
workloads before further optimization or a user-perceived speedup claim.

Keep full redraw and synchronous completion/validation. This evidence does not
justify changing CPU reconstruction architecture or acknowledgement timing.
