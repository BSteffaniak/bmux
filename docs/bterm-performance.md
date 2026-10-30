# Bterm native rendering performance

## Fractional zoom acceptance and measurement

Whole-window zoom changes logical typography, then rerasterizes at the current
monitor scale; it must not stretch previously rasterized glyph textures. Font
size, line height, letter spacing, and DPI must produce one geometry for paint
and pointer inversion. A requested target is not a committed frame, and a
renderer geometry setter is not evidence of successful presentation.

The public-API acceptance suite is
`packages/bterm/tests/zoom_acceptance.rs`. It uses captured explicit
configuration, private temporary preference roots, deterministic bundled fonts,
and caller-supplied animation times. It requires neither a native window nor a
running companion and does not read host font installations for raster tests.
Run it with the repository toolchain when build capacity is available:

```sh
nix develop --command cargo test -p bterm --test zoom_acceptance
```

The tests cover fractional config resolution, unchanged companion identity,
exact-size/reset/step actions, animation retargeting and completion, reduced
motion, remembered targets and runtime isolation, corrupt preference handling,
keyboard press/repeat/release ownership, normalized wheel input, window-local
state, and size-distinct raster-cache entries. They do not measure frame time,
input-to-photon latency, visible fallback quality, or live application wiring.
A passing matcher test proves returned ownership, not that every application
caller obeys it. Native acceptance must establish those separate boundaries.

For a finite, opt-in check of the real native application/GPU path, use the
standalone example (it never starts a daemon or attaches to a user session):

```sh
nix develop --command cargo run -p bterm --example zoom_window_acceptance -- --window
```

`--window` is required. The example opens one owned window, wraps the real
`Application<AttachWorker>`, and injects synthetic input directly into its event
path. A separate producer prepares canonical full retained deliveries, delays
40 ms per attempt, and calls synchronous `NativeOutput::present`. Its mailbox
retains one coalesced pending viewport, not a growing frame queue. Fifteen
stages
check zoom in/out (including repeats), reset from a non-default size,
modifier-wheel fractions, exact sizes, rapid final-target retargeting, and
native
reload at non-unit zoom. Reload changes base size and letter spacing, must
advance committed geometry generation, and must preserve the zoom ratio. An
invalid reload must retain the last usable geometry and bindings. A single
wrong-viewport delivery (generation `u64::MAX`, never a neighboring generation
that animation could make current) then exercises rejection without a commit and
application-scheduled retry to another successfully committed frame. Final
stages
reload a `0.01` minimum, request exact scale `0.01`, require actual new
committed
tiny geometry with at most 100,000 cells, observe convergence without idle
commit churn, and reset to verify normal-size recovery.
Successful application `presented` callbacks provide committed viewport/cell
metrics; the worker must receive only the deliberately unmatched control key,
never the zoom shortcuts. Nonzero letter spacing checks proportional scaling.
The example reports counters and exits on success, fails after a 60-second
execution budget, and has a 65-second process watchdog for stalled
callbacks/shutdown. Exit before completing every stage is failure.

This is actual GPU/application output evidence only when run successfully. It is
not a screenshot, OS event-injection test, display-completion measurement, or
claim about visible glyph quality. Empty damage after a successful commit must
produce no preparation, and `present_prepared(None, ...)` must return `Ok(None)`
without invoking its output callback. Rejected deliveries are counted and must
recover;
the harness injects provenance failure, not GPU/device failure, and does not
suspend the real OS window or exercise persistence. Its reload handoff tests
application consumption, not file-source discovery or attach command transport.
Those remain separate checks below.
Keep the test window on one display without resizing it during the run; DPI
changes fail the controlled comparison.

### Reported release fractional-zoom observation

The coordinator measured a finite 60-sample release `fractional-zoom` workload
at
1920×1280 physical pixels and DPI scale 2:

| CPU wall-time field |       Median | p95 (sorted index 57) |
| ------------------- | -----------: | --------------------: |
| `total_ns`          | 16.283021 ms |          21.461916 ms |
| GPU wait            |  1.501333 ms |           9.802958 ms |

These are CPU-side durations, including time spent waiting for the GPU, not GPU
execution timestamps, display latency, refresh completion, or input-to-photon
measurements. They are not a baseline comparison or evidence of a speedup.
Artifact identity and full run metadata remain coordinator-owned evidence.

The coordinator reported the fifteen-stage native application harness passing
with 24 committed outputs and two rejections. Tiny zoom committed 97,580 cells,
settled without commit churn, and recovered after reset. An earlier debug run
exceeded the original 15-second budget at reset; the budget is now 60 seconds
with unchanged behavioral assertions, allowing bounded 100,000-cell debug work.

For an isolated full-product native acceptance run:

1. Select an owned runtime and private config/state roots. Record artifact hash,
   effective typography, zoom settings, fonts, display DPI, and renderer backend.
   Do not attach to or inject input into a user's existing terminal.
2. Render a fixed terminal workload containing narrow/wide cells, combining
   marks, fallback glyphs, a cursor, selection, images, and pointer targets. Use
   exact sizes such as `15.25`, `16.0`, and `17.75`; repeat at non-integer DPI.
   Check that glyphs rerasterize and all presentation elements remain aligned.
   Include nonzero letter spacing to verify proportional whole-window scaling,
   including when the effective font size reaches a DPI-dependent cap.
3. Exercise primary-plus/minus/reset, repeats, modifier-before-key release,
   ordinary unmodified input, and fractional modifier-wheel input. Confirm zoom
   shortcuts do not also type or scroll the terminal and unmatched input follows
   its ordinary route. Verify focus loss and window isolation. Pinch and
   pane-local zoom are out of scope.
4. Rapidly reverse an animation mid-transition; check continuity, final exact
   size, and no animation-only wakeups after settling. Repeat with reduced
   motion and animation disabled. Check that pending/failed output does not
   install a different hit-test geometry from the last successful paint.
5. Close and reopen the owned window after persistence settles. Confirm the
   final target is restored, reset persists `1.0`, `remember = false` ignores
   saved targets, and another runtime/config selection does not borrow it.
   Use unchanged configuration for ordinary reopen/restore checks. There is no
   file watcher: exercise the explicit existing attach reload path to apply
   native typography/zoom edits while retaining the selected source locators.
   Verify BMUX/runtime, companion, and window-creation changes are refused; frozen
   snapshots must remain frozen. Confirm malformed preferences are reported and
   saving is disabled for that window without overwriting the corrupt file.
6. Exercise the configured minimum and maximum at each tested DPI. The current
   window integration caps effective font size to keep logical and physical raster
   sizes within `1..256`, while retaining the requested target. Verify capped
   behavior and restoration on a lower-DPI display separately from exact sizes.
   Treat clipped/misaligned output, stale hit targets, or input duplication as
   failures, not benchmark samples.
7. Delay output across multiple animation samples and inject presentation failure;
   verify eventual successful output after settling, no stale pointer delivery,
   coherent accessibility/IME geometry, and recovery without exhausting retries
   on superseded frames. Test suspend/resume and zero-sized windows mid-animation.
8. Contend the preference lock and inject save failure, both during debounce and
   close. Saves use a bounded worker and up to three retries; close waits at most
   200 ms and reports an unretained target on timeout. Check these bounds and
   latest-target ordering separately from the store's isolated round-trip tests.

For timing comparisons, keep fonts, profile, DPI, telemetry, workload and input
sequence fixed. Separate cold font selection/cache fill, steady exact-size
rendering, animated transitions, rapid retargeting, and settled idle. Fractional
animation can generate many distinct raster sizes: inspect bounded cache usage,
CPU rasterization time, upload bytes and GPU atlas churn rather than assuming a
cache hit at every sample. Repeated use of one exact size should be measured
separately from a sweep through unique sizes. Preference file IO must remain off
the rendering/input hot path and be debounced rather than run per animation
sample. Telemetry stage timings do not establish actual display completion.
Report distributions and failures with the evidence requirements below; do not
claim a zoom performance improvement from headless correctness tests.

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

## Shared pipeline diagnostics and measurement boundaries

With `BTERM_RENDER_TELEMETRY=1`, Bterm also reports identity-correlated producer
contention, UI queue residence, output callback time, input admission/residence,
composition, and output acceptance. Output histories retain 128 scalar samples;
input histories retain 256. Disabled input metrics avoid diagnostic clocks,
locks, and sample storage. No terminal text, image payload, or key contents are
recorded by these summaries. Logical payload counters are not allocator or RSS
measurements.

Surface acquisition, encode/submit, GPU wait/validation, and present-call time
are separate stages. Present-call return and nominal monitor refresh do not
prove physical visibility or input-to-photon latency. Failed output never gains
interaction authority; neither enqueue nor telemetry acknowledges output.
Wake events service new output without replaying an unchanged retained frame;
actual exposure redraws and viewport fencing remain intact.

Display-free measurement entry points:

```sh
nix develop --command cargo run --release -p bmux_terminal_grid \
  --example replay_prefix_benchmark
nix develop --command cargo test -p bmux_cli --test native_echo_integration
nix develop --command cargo test -p bmux_cli --test native_echo_integration \
  -- --ignored --nocapture
python3 scripts/test-perf-owned-process.py
```

The echo fixture launches an isolated real daemon and native key input, then
matches complete retained-output acceptance. Its timer starts before input
admission. It does not exercise native GPU output or the display. A debug-build
96-sample run measured p50 16.92 ms, p95 21.47 ms, p99 21.82 ms; without an
identical baseline echo fixture this establishes a diagnostic baseline, not a
speedup.

For owned process-tree observations, use an explicit executable and isolated
configuration/state (never attach the sampler to an existing runtime):

```sh
python3 scripts/perf-owned-process.py --seconds 30 --interval 0.2 \
  --output /tmp/owned-resource-run.json -- /absolute/path/to/test-executable
```

The sampler records executable SHA-256 and bounded-duration numeric samples,
discards workload stdin/stdout/stderr, and cleans up its owned process group.
Children escaping that group are not contained. Summed RSS can double-count
shared pages, excludes GPU memory, and is not physical footprint; `ps` CPU is a
gauge, not CPU time or energy. Compare identical sampling configurations.

### Preliminary shared-path comparison

Against baseline `f9aebbe8`, an optimized 200-iteration/20-warmup
`sample-tui-runtime-render` pair measured p50 8.02 → 4.48 ms and p99
9.57 → 4.60 ms with identical structural counts and no runtime faults.
This synthetic shared TUI workload is not full-product responsiveness evidence.

Interleaved parser runs retained protocol prefix-buffer reuse but dropped the
grid double-buffer optimization after larger-chunk grid regressions. The
smaller isolation measured plain one-byte protocol parsing 3.25 → 1.20 ms,
32-byte protocol parsing 0.359 → 0.292 ms, and 8192-byte protocol parsing
0.226 → 0.226 ms. Grid timings remained mixed (one-byte 19.51 → 20.54 ms;
32-byte 15.03 → 14.83 ms; 8192-byte 14.89 → 15.01 ms). These are local
microbenchmark observations, not proof of universal improvement. No accepted
ordinary-bmux/Bterm end-to-end, idle CPU, or process-tree memory improvement is
established by these results; further optimization remains measurement-gated.

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
