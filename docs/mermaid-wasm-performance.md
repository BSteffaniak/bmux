# Historical Mermaid native-process / embedded-WASM measurements

The process renderer has been removed. Commands below describe historical
artifacts and cannot be used with the current checkout. Current benchmarking
uses WASM modes only; see `packages/mermaid-render/README.md`.

**Status: v4 prototype measurements recorded; migration NOT accepted.**
Coordinator granted the exclusive benchmark CPU lane for the runs below. Font/
layout parity, visual correctness, unloaded actual-stop and deployment gates
remain unresolved. Earlier source-wave validation history is retained below.

## V4 measured handoff (2026-10-14)

Evidence root: `/hdd/bmux-mermaid-evidence/v4-20261014-1650-7e1ae36d`.
Each phase contains `metadata.json`, `schedule.json`, `raw.jsonl`, `report.json`
and hashed generated corpus. Correctness also retains SVGs. Root contains build
record, supplemental source/font/governor provenance, paired block-bootstrap
analysis and a SHA-256 evidence inventory. No production deployment was changed.

### Artifact/source scope and coordinator disposition

Coordinator disposition: requested prototype measured; **compatibility failed**;
production process backend remains unchanged. Benchmark CPU capacity is released.
No further lane builds or timings are authorized in this docs-only handoff.

These measurements predate the coordinator's **build-only all-architecture AOT
addition**. Coordinator reports unchanged runtime machine behavior, but the final
rebuilt artifact has a distinct hash and was **not measured by this wave**.
Do not attribute these numbers to that later binary or claim cross-target
verification from this x86-64 run. Preserve the verified compare/worker hashes
above when citing the evidence. Dirty checkout HEAD at measurement was
`8c5b518cff4c305ba76d93e409a503870cc35c5b`; HEAD alone is not full source identity.

Main timing source/input SHA-256 identities:

- compare source: `7f369df1b0b851f45633c212f6e9ba0143450598b336ee4315e7fc7bc4c250db`
- runner after case-filter refinement: `43ce94aed2ff9f90b5bc2fc14b5f7c84ed06fef9f800ca8e5ce6b373a247354c`
- corpus manifest: `f9b7e39113d970426aefa09629439c27ae1b03b5a1678570f075e98e7c66c0c9`
- Cargo.lock: `a9edcddc52d8f097c584d79f4d5fbcf68449e9a93ed016c9f81797740bf0341d`
- flake.lock: `279d8c78e730c6b30e497f20ce8140ffcf6f8ec8285fd9162e1fd0b5b475052e`

Per-phase metadata retains runner/corpus/artifact identities; correctness used
the pre-filter runner version. Supplemental provenance hashes runtime/build/
guest/font sources as observed after measurements, not an independently sealed
build-time snapshot. The final artifact hash belongs in the coordinator's rebuild
record, not substituted into these historical measurements.

**Cold JIT is not steady-state:** three renderer reconstructions per case,
including approximately 2.2 seconds runtime compilation each; neither its render-
call-only component nor reused JIT results may be labeled total cold-JIT latency.
**Cancellation is scoped:** 5 ms delayed token, typical+dense, one independent
host, epoch/AOT/fuel modes listed above, API-return only. No JIT-cold cancellation,
no shared-engine async queue/drop test, no unloaded actual-stop proof. Successful
requests completed before cancellation are not cancellation-pass samples.

### Provenance and exact run parameters

Verified coordinator release hashes before execution:

- compare: `10ab3e0cb48c423637a53d95fc14ec6ab0c16e5b48affa4b74bcf5bf7fcd37d1`
- worker: `29711e7430031e3d1c2209db17117687b6205686e79265374fc75451b5900fd7`

Release target root:
`/hdd/GitHub/bmux/target/nix/ln8ngqgs4kl86x7mcv4yax5z696cp42a-rust-default-1.96.0`.
Combined prototype host uses `--features wasm`, both baseline and WASM paths,
not two matched production products. Coordinator-reported release build elapsed
146 seconds; **not a clean workspace build**; build CPU/peak memory unknown.
Measurements ran via `nix develop --command python3 ...`; rustc 1.96.0
`ac68faa20c58cbccd01ee7208bf3b6e93a7d7f96`, LLVM 22.1.2, Cargo 1.96.0
`30a34c682`. Linux 6.12.74, glibc 2.40, AMD Ryzen 9 7945HX, affinity CPUs 0..31.
OS pages warm/uncontrolled; no SVG cache; governor `powersave`; no affinity,
frequency, page-cache or font configuration changes. Although the coordinator
reserved the lane, background load was uncontrolled (post-run load averages
12.28/11.84/12.00). **Do not call these unloaded measurements.**

All phases use seed `20261014`, `--fuel 1000000000`, `--timeout 60`,
`--sample-ms 5`, explicit binary/worker/build-record paths and five artifact
inventory arguments. Commands' exact parsed arguments are in each metadata file.
A Python-only runner refinement added `--cases` (native remains restricted to
trusted intersection); driver/release artifacts were not edited or rebuilt.

| Evidence directory | Phase / modes | Trials × rounds | Cases / concurrency |
| --- | --- | --- | --- |
| correctness | correctness, all eight modes | 1 × 1 | full typical + stress; native six trusted only; 1 replica |
| timing | timing; current, worker, native, AOT cold/warm, JIT warm, fuel | 10 × 5 | eight typical; native 10 × 1 six trusted; 1 replica |
| jit-cold | timing; current, JIT cold | 3 × 1 | eight typical; 1 replica |
| cancellation | cancellation; current, worker, AOT cold/warm, JIT warm, fuel | 10 × 5 | typical + dense; cancel delay 5 ms; 1 replica |
| concurrent | timing; current, worker, AOT warm, JIT warm, fuel | 10 × 5 | eight typical; 2 independent replicas |

Each trial/mode block is interleaved/randomized; jobs are shuffled within blocks.
Main timing has 50 observations per typical case/mode, except native (10). JIT
cold has only three observations per case; its p95 is essentially the maximum,
not a stable tail estimate. Concurrent phases have 100 observations per case/mode.

### Correctness: blocking, not normalized away

All eight typical cases succeeded in every WASM mode. Worker exactly matched
current SVG bytes/dimensions/fonts on 8/8; native diagnostic matched 6/6 trusted
cases. Every WASM mode matched current exact bytes on **0/8**, and dimensions/
font declarations on only **1/8** (sequence). Example flowchart current dimensions
`638.48236 × 156.67928` versus WASM `649.0045 × 158.4134`. Declared SVG font stacks
were identical while metrics/layout differed, so font attribute equality is not
font-input parity. Guest JetBrains inputs versus native system fonts remain a
known confounder; first-byte and bounded diffs are retained, not dismissed.

Malformed inputs differ in error text (process adds `invalid Mermaid diagram:`).
Dense current/worker reject output dimensions; WASM AOT/JIT epoch paths time out,
and fuel must be inspected for its recorded typed outcome rather than assumed
identical. Oversize and directive host-validation errors match. Near-limit and
deep cases succeed but SVG parity differs. No rasterizer (`rsvg-convert`, `resvg`,
`convert`) was available in the measurement shell; **visual assessment was not
performed**. Font resolution, Unicode glyph coverage/shaping, clipping and pixel
parity remain blocking migration checks.

### Typical request latency (median / p95, milliseconds)

Total request time includes cold constructor/destruction where applicable;
source read/output hash excluded. These comparisons are **font-confounded
prototype diagnostics**, not matched-backend migration claims.

| Case | Current | Acquired worker | AOT cold | AOT warm | JIT warm | AOT fuel |
| --- | --- | --- | --- | --- | --- | --- |
| Flowchart | 32.94 / 42.23 | 14.02 / 19.45 | 26.14 / 30.67 | 16.42 / 17.91 | 16.72 / 20.46 | 19.88 / 25.03 |
| Sequence | 27.29 / 31.69 | 8.86 / 9.39 | 11.14 / 13.13 | 2.30 / 2.89 | 2.26 / 3.41 | 2.33 / 3.39 |
| Class | 27.20 / 29.88 | 8.87 / 9.16 | 11.01 / 13.19 | 1.88 / 2.41 | 1.98 / 2.62 | 2.09 / 3.42 |
| State | 27.53 / 30.88 | 8.85 / 9.35 | 12.91 / 15.29 | 3.88 / 4.55 | 3.97 / 5.74 | 4.39 / 5.31 |
| ER | 27.25 / 31.32 | 8.90 / 9.63 | 13.93 / 17.67 | 4.71 / 6.57 | 4.73 / 5.70 | 5.48 / 8.78 |
| Pie | 27.48 / 32.98 | 8.89 / 9.17 | 11.01 / 12.83 | 1.78 / 2.25 | 1.84 / 2.21 | 1.85 / 2.76 |
| Unicode | 27.59 / 31.79 | 8.83 / 9.31 | 13.85 / 15.66 | 4.59 / 5.36 | 4.74 / 5.83 | 5.11 / 5.53 |
| Nesting | 37.29 / 44.65 | 19.00 / 19.45 | 27.07 / 32.20 | 17.63 / 18.91 | 17.74 / 19.48 | 20.68 / 23.96 |

Native diagnostic median/p95 ms: flowchart 7.47/11.89, sequence 0.38/1.57,
class 0.24/1.59, state 1.41/3.60, ER 1.60/2.87, pie 0.15/1.59. It excludes
stress and cancellation and has a different job count; do not compare aggregate
CPU/throughput with typical-inclusive mode totals. All typical timing observations
succeeded; no timing errors were silently folded into successful latencies.

Warm AOT/current p95 ratios range 0.068..0.424; exploratory paired trial-block
bootstrap (2,000 replicates, seed 20261014, five repeated requests kept together)
95% upper bounds range 0.087..0.513. This numerically satisfies the proposed 1.20
threshold **for this confounded run only**, not acceptance. Against acquired
worker, warm AOT p95 ratios range 0.245..0.972; nesting's 95% interval is
0.941..1.089. Full ratios/CIs reside in `paired-p95-analysis.json`.

### Startup, constructor, CPU, memory and throughput

| Mode | Launch-to-READY median / p95 ms | Median CPU seconds / trial, host + waited child | Max sampled tree RSS MiB | Serial request-roundtrip successes/s |
| --- | --- | --- | --- | --- |
| Current | 4.67 / 4.94 | 1.0028 | 13.36 | 33.1 |
| Worker | 14.10 / 19.70 | 0.3636 | 13.34 | 88.7 |
| Native diagnostic | 1.23 / 1.37 | 0.0147 | 9.85 | 411.5 |
| AOT cold | 1.15 / 1.35 | 0.6464 | 38.14 | 60.4 |
| AOT warm | 8.47 / 8.97 | 0.2826 | 38.00 | 141.2 |
| JIT warm | 2245.60 / 2330.51 | 2.5301 | 106.54 | 137.4 |
| AOT fuel | 9.76 / 10.54 | 0.3277 | 42.02 | 120.9 |

Launch-to-READY includes process startup and driver setup, distinct from render.
Raw trial records preserve `driver_setup_ns` independently. AOT cold median
constructor is 6.08..6.25 ms across cases; render-call medians 3.37..19.35 ms.
JIT cold total medians 2224.4..2265.1 ms, constructor medians 2216.8..2258.1 ms,
render-call medians 3.3..21.7 ms (three observations/case). Thus cold JIT costs
are predominantly runtime compilation. These are warm-page prototype launches,
not first-install/disk-cold startup or clean-build measurements.

CPU is wait4 host+waited descendant trial total (includes initialization, output
hashing and teardown). Peak tree RSS sampled every 5 ms may miss short children
and double count shared pages; no cgroup/unique-physical-memory claim. Retained
post-request RSS ranges: current 4.43..4.46 MiB, worker 4.43..4.44, AOT cold
19.62..19.65, AOT warm 32.48..32.49, JIT warm 101.03, fuel 36.50..36.51.

Two-replica batch throughput successes/s: current 60.4, worker 171.2, AOT warm
257.8, JIT warm 259.6, fuel 225.4; all 800 requests/mode succeeded. This is
independent host concurrency, **not** shared-engine `render_async` contention.
Concurrent flowchart p95 ms: current 42.315, worker 19.138, AOT warm 20.759,
JIT warm 18.311, fuel 23.559. AOT warm therefore slightly exceeds worker p95
for that case under replica load; raw evidence preserves other cases too.

### Cancellation and retained resources

5 ms delayed token, typical + dense, 450 requests/mode. Cancellation-return p95
among typed `Cancelled` observations: current 19.753 ms (450), worker 4.337
(450), AOT cold 3.032 (450), AOT warm 4.717 (166), JIT warm 4.779 (163), fuel
4.707 (180). Remaining warm/JIT/fuel requests completed successfully before
cancellation; they are not zero-latency cancellation samples. Dense specifically
has 50 cancellations/mode, p95 return: current 17.965 ms, worker 4.495, AOT cold
3.319, AOT warm 4.838, JIT warm 4.805, fuel 4.697.

Actual-stop remains **null/unproved**; these are API-return observations and not
unloaded measurements. JIT cold cancellation was omitted because its compile
startup is expensive and uncancellable; shared-engine queue/drop cancellation
and synthetic no-progress guest probes were not measured. No acceptance of the
actual-stop <50 ms gate follows from return latency.

Cancellation post-request FDs/threads were constant: current/worker 4/1, AOT cold
3/1, AOT warm/JIT warm/fuel 4/2. Retained RSS ranges MiB: current 4.30..4.31,
worker 4.37..4.39, AOT cold 19.54..19.56, AOT warm 32.55..32.58, JIT warm 101.11,
fuel 36.57..36.60. No runaway growth is evident in these sampled bounded trials,
but small high-water changes, temporary-file lifetime, task teardown and long-run
resource leakage are not fully proven. Post-run `ps` found no compare/worker
processes; this single check does not establish each request's stop timestamp.

### Footprint and unresolved gates

Measured files (bytes): compare 48,809,992; native worker 5,400,872; guest WASM
5,414,087; ordinary AOT 13,670,768; fuel AOT 15,784,560. Compare includes runtime,
JIT support and embedded guest/AOT variants; **do not add embedded files again
and call the sum a deployed bundle**. It is not a production host baseline pair,
so incremental runtime deployment overhead is unresolved. Build elapsed 146 s
is coordinator provenance only; clean/no-op/cross-target verification and build
CPU/memory remain coordinator follow-ups after CPU release.

Migration blocked by: mismatched font inputs and SVG/layout parity; missing
visual/glyph correctness; background-load confounder; actual-stop instrumentation;
precise resource/leak verification; complete matched production bundle/build
footprint; shared-engine async/cancellation behavior. No default backend or
production deployment changed. Keep the native process baseline selectable.

V4 validation: all five benchmark phases completed with evidence, Python-only
`--cases` refinement does not alter compiled artifacts. Focused AOT clippy/package
tests and release build were coordinator-reported PASS before this wave, not
rerun during exclusive timings. Workspace clippy/nextest/machete and any broader
product validations remain coordinator obligations. This measured wave does not
claim clean build, cross-target validity, migration acceptance or visual PASS.

## Coordinator validation checkpoint

All commands ran through `nix develop --command` with the locked Nix toolchain.

- `cargo clippy --all-targets -- -D warnings`: PASS.
- `cargo nextest run --no-fail-fast`: FAIL initially (broadcast pending-state
  expiry assertion and TLS voter-restart integration); both passed isolated.
- `cargo nextest run --no-fail-fast --test-threads 2`: PASS, 5,728 tests,
  16 skipped. Default-concurrency sensitivity remains disclosed, not repaired.
- `cargo machete --with-metadata`: PASS, zero unused dependencies.
- `cargo clippy -p bmux_mermaid_render --all-targets --features wasm -- -D warnings`:
  PASS.
- `cargo nextest run -p bmux_mermaid_render --features wasm --no-fail-fast`:
  PASS, 38 tests including AOT/JIT parity, mismatch rejection, async drop and
  execution interruption.
- `cargo test -p bmux_mermaid_render --features wasm`: PASS before the added
  cross-target rejection tests; the final feature-enabled nextest covers them.
- `cargo fmt --all -- --check`, workspace dependency checker and diff check: PASS.
- Ordinary feature-enabled build: PASS; identical second invocation finished in
  0.30 seconds without compilation. This is an incremental no-op, not a clean
  build measurement.

Cross-architecture AOT is explicitly rejected without a destination compiler
backend. Attempting Wasmtime 38.0.4 `all-arch` failed dependency resolution for
unpublished `winch-codegen =38.0.4`; that feature was removed. Actual execution
on other platforms remains unvalidated. Production CLI/plugin files were not
changed, so their extra runtime/rebuild commands were not required.

The prototype is not migration-ready. In particular, bounded-concurrency suite
success does not erase the initial unrelated concurrency-sensitive test failures,
and synthetic cancellation tests do not substitute for unloaded actual-stop
performance measurements. No production backend replacement or deployment occurred.

## Original protocol and source-wave history

## Sources and API

- `packages/mermaid-render/examples/compare.rs`: dedicated line-protocol driver.
- `packages/mermaid-render/bench-corpus`: immutable small cases and deterministic
  stress recipe manifest; runner materializes generated cases outside checkout.
- `scripts/benchmark-mermaid.py`: Python standard library, Linux `/proc` + `wait4`.
- WASM feature/API: `WasmMermaidRenderer::new() -> Result<Self,
  MermaidRenderError>` and `render(&request, &token)` returning the existing
  `MermaidRendered`/`MermaidRenderError`. Internal guest ABI is not a benchmark API.
  The coordinator has supplied feature `wasm`, Wasmtime 38 and automatic guest/
  AOT generation in `OUT_DIR`; no separate guest build command is required.
  V3 `new()`/`new_with_fuel()` load AOT; `new_jit()`/`new_jit_with_fuel()`
  compile at runtime. This driver exposes warm AOT fuel only, not JIT fuel.
- Existing `sha2` is reused by the example; no benchmark dependency or manifest
  change is requested. Retain native worker/process baseline during migration;
  if a later default removes it, coordinator should retain it behind a feature.

No core/runtime domain conveniences or production behavior are changed here.

## Measurement boundaries

| Mode | Timed render work | Setup outside render timer |
| --- | --- | --- |
| `current` | captured-worker acquire, second pin (presenter double snapshot), digest verification, process spawn/IPC/render/wait, snapshot destruction | capture/open/hash installation once |
| `worker` | acquired snapshot digest verification, process spawn/IPC/render/wait | capture and acquire one retained snapshot |
| `native` | public native render including its backend thread/channel | none |
| `wasm-cold` | independent `new()` AOT construction, render, renderer destruction | host executable launch only |
| `wasm-warm` | render using one retained AOT renderer | `new()` AOT construction once |
| `wasm-jit-cold` | independent `new_jit()` runtime compilation, render, renderer destruction | host executable launch only |
| `wasm-jit-warm` | render using one retained JIT renderer | `new_jit()` compilation once |
| `wasm-fuel` | render using one retained fuel-instrumented AOT renderer | `new_with_fuel(budget)` AOT construction once |

`current` reproduces the Mermaid portion of the presenter path, **not** entire
Markdown loading, deck preparation, plugin dispatch, presentation, or output
cache behavior. Native worker remains a **cold process per render** in both
process modes even when its snapshot and OS pages are warm. WASM warm means an
initialized module, not a cached SVG or necessarily a reused guest instance.
WASM cold is renderer-cold (AOT load or JIT compile according to mode), **not**
a flushed OS page/font/disk cache. Startup is
recorded separately as both driver setup and launch-to-READY roundtrip.
Per-request `constructor_ns` isolates cold AOT/JIT construction;
`render_call_ns` isolates the render call; legacy `render_ns` remains total
end-to-end work including constructor and destruction. The residual
`teardown_and_timer_overhead_ns` is a timer-derived difference, not a precise
standalone teardown measurement. Warm construction occurs in driver setup, not
per-request constructor time (which is zero). For process modes render-call time
includes their existing snapshot/spawn/IPC/wait work. Cold cancellation latency
can include an uncancellable constructor; constructor failure is not cancellation.
Report cold constructor, render-call, total and process-startup distributions
separately, retaining unsuccessful constructor outcomes in raw status evidence.
The first request remains visible as sequence 0; inspect first-render effects
separately rather than silently discarding them. There is no warm-up exclusion.

Render timer excludes source-file reading, request construction, output hashing,
result formatting and correctness output-file writes. Trial CPU/wall time
includes these operations, driver initialization/teardown and request handling.
The runner writes SVG only in the separate correctness phase. No renderer output
cache is used. `800x600`, default 64 KiB source/4 MiB output, 5-second deadline,
concurrency bound 2 are identical; execution is serial/unloaded unless the
recorded environment says otherwise. Config and release/backend equivalence
must be reviewed from build provenance, not assumed from artifact names.

## Safe corpus and separate correctness

Typical families: flowchart, sequence, class, state, ER, pie, Unicode and nested
subgraphs. Stress: 32-node dense DAG, 24 nested subgraphs, valid comment padding
at exactly 65,535 UTF-8 bytes, oversized 65,537-byte source, malformed syntax and
forbidden directive. Near-limit padding stresses transport/validation, **not**
large layout complexity. These are representative examples, not exhaustive
family, maximum-depth, output-limit or Unicode-shaping coverage. Family support
is established by observed results rather than assumed.

Native diagnostic accepts only exact compile-time bytes of six small checked-in
families; it refuses arbitrary/stress/Unicode/nested cases and cancellation.
It exits immediately after any render error, because public native timeout can
leave an unjoined backend thread. Never add cancellation storms, stress inputs
or repeated timeout probes to that mode. The runner also places each trial in
an owned process group and kills it on its external deadline. External timeout
aborts the run (partial raw evidence remains), not a successful benchmark sample.

Run correctness first, separately from timing. For each case compare current
process baseline to worker/native/WASM results: exact SVG SHA-256 and error
status, root width/height/viewBox and declared font attributes/styles. Report
bounded unified diffs plus the first differing byte and contextual excerpts for
SVG mismatches. Error reports are preserved, not counted as zero-duration
successes. Unsupported family or unexpected error requires investigation.
Declared fonts/dimensions are structural checks only: this does not prove font
availability, glyph shaping, pixel equivalence, clipping, safe SVG content or
visual correctness. **Q1 font disclosure:** the guest is being patched to bundle
JetBrains font inputs while the current native backend uses system-font inputs.
This is a known configuration mismatch, not a matched-backend comparison. Record
both resolved font sets and their hashes in build provenance. Exact SVG mismatch
remains evidence requiring attribution (including font metrics/layout); neither
byte equality nor an explained font difference establishes migration acceptance.
Do not apply the fair-performance gate until fonts/configurations are matched,
or explicitly label results as a confounded prototype diagnostic.
Raster comparison and family-specific semantic checks are
still coordinator follow-ups. Exact nondeterministic-ID differences are a
mismatch until explained; do not normalize them away in the timing runner.

## Coordinator execution recipe (not executed)

After guest verification and an exclusive capacity grant, use the pinned
`nix develop` / direnv toolchain. Build **both** process and WASM paths in release
with the same lockfile, renderer version/options, font inputs and target/native
codegen assumptions. The combined example needs feature `wasm`:

```sh
nix develop --command cargo build --release -p bmux_mermaid_render --features wasm \
  --example compare --bin bmux-mermaid-worker
```

Use the actual Nix-selected target paths (do not override target directory).
For deployment overhead also build matched host products with/without embedded
runtime under the same release flags; the combined comparison binary alone
cannot quantify deployment overhead. Preserve build commands, tool versions,
source/lock/config hashes, clean/incremental state, elapsed/user/system CPU,
peak memory, artifact inventory and build failures in a JSON build record.
Unknown fields must be `null` with explanations. Observe cold/clean and warm
incremental builds separately. Do not delete shared build state to simulate cold.

Example invocation, after resolving binary/worker paths and creating a build
record outside checkout:

```sh
python3 scripts/benchmark-mermaid.py \
  --binary /absolute/nix-target/release/examples/compare \
  --worker /absolute/nix-target/release/bmux-mermaid-worker \
  --output /absolute/evidence/mermaid-correctness-01 \
  --phase correctness --trials 1 --rounds 1 \
  --build-record /absolute/evidence/build-record.json \
  --artifact /absolute/host-product --artifact /absolute/guest.wasm \
  --cache-settings 'page cache warm; no SVG cache; font inputs pinned; affinity/governor/load recorded'
```

Use distinct new output directories for `--phase timing --trials 20 --rounds 10`
and `--phase cancellation --trials 20 --rounds 10 --cancel-ms 5`. Cancellation
omits native. Retain and vary random seeds between independent repetitions;
each block randomizes/interleaves mode order, each trial launches a fresh host,
and request order is independently shuffled. Warm module spans one trial; it
is not shared across mode trials. Case counts differ for native diagnostics,
so native aggregate throughput/CPU must not be compared to stress-inclusive
mode totals. Correctness runs need both `current` and candidate modes.
Do not treat repeated requests within one trial as independent experiments.

The runner never builds. It records seed/schedule identities, corpus/artifact/
lockfile/driver hashes, toolchain commands, CPU/OS/affinity and user-supplied
cache/load settings. The build record must identify artifacts at build time;
recording checkout hashes at run time does not prove artifact/source equality.
Do not execute on a concurrently loaded shared checkout and call it unloaded.

## Statistics and resource evidence

Raw JSONL is authoritative; summaries report successful-render median, nearest-
rank p95, failures and statuses per case/mode, successful render-only throughput
and trial-wall throughput including launch/setup/teardown. Trial CPU median/p95
includes **host plus waited descendants** via Linux `wait4`, not host-only WASM
CPU versus omitted native child CPU. This is trial-total CPU, not per-request
CPU. Confirm child accounting on the target kernel (including renderer teardown)
before publishing comparisons; the process adapters wait for their children.
Report per-trial dispersion/confidence intervals and paired typical-case ratios
from raw data before applying gates. The current runner does not compute CIs.

Memory evidence includes Linux wait4 max RSS (a high-water metric, **not** sum of
concurrent host+child RSS), sampled process-tree RSS sum and retained host/tree
RSS, FD and thread counts after each request. Sampling defaults to 5 ms and can
miss short-lived children; RSS sums double-count shared pages. Memory peaks are
approximate and must be labeled. For acceptance-grade peak/retained resource
measurements add an isolated cgroup or equivalent observer with attribution to
both host and descendants, and monitor temporary snapshot files/handles. FD/
thread/RSS trends are evidence, not an automatic leak/no-leak verdict. Runtime
sampling overhead is shared but can distort very short cases; repeat with a
larger sample interval and report the sensitivity.

Deployment footprint includes native worker, guest bytes, serialized AOT artifacts
(including the fuel variant where deployed), host executable,
Wasmtime/runtime code and all dynamically deployed files. Supply every deployed
artifact with `--artifact` and compare matched product bundles; report summed
bundle size, shared/dynamic assumptions and incremental host overhead versus
process-only baseline. Never present guest bytes alone as deployment footprint.
Build record owns elapsed/CPU/peak-memory footprint; runner only records supplied
build evidence and artifact size/hash inventory. It does not infer missing values.

## Cancellation and proposed gates

Cancellation timing records actual token-request timestamp to render return,
including poll/IPC/teardown work. Requests completed before cancellation are not
cancellation observations. Prevalidated errors are not successful cancellation.
**`actual_stop_ns` is explicitly null:** the public return value alone does not
prove guest execution stopped, children were reaped or resources released. The
native diagnostic is intentionally excluded. The process API documents hard
termination before return; WASM runtime should document/verify corresponding
execution-stop semantics. Coordinator needs runtime instrumentation or isolated
OS observer proof (child lifetime and reaping, guest task/epoch/fuel teardown),
then record actual-stop p95 independently. Do not relabel return latency as
actual-stop latency.

Proposed acceptance gates, not approved migration conclusions:

1. Correctness passes separately; every supported typical family accounted for.
2. Warm WASM p95 for each typical case <= 1.20x current end-to-end process p95;
   also report single-snapshot worker comparison and initialized-module overhead.
3. Unloaded actual-stop cancellation p95 < 50 ms, with explicit proof.
4. No monotonic leaked resources across repeated cancellation/render cycles;
   plateau/GC behavior distinguished from leaks and all children/snapshots gone.
5. Cold latency, CPU, memory and total deployment/build footprint reviewed, even
   if the warm latency gate passes.

Insufficient successful samples, unverified resource stops, missing footprint
artifacts or unexpected errors block a performance conclusion. Keep the process
baseline selectable throughout prototype/migration work.

## Q2 source wave: fuel and bounded concurrent hosts

Q2 fixes SHA-256 hexadecimal encoding for sha2 0.11 by writing each byte;
no digest `LowerHex` implementation is assumed. Mode `wasm-fuel` initializes
`WasmMermaidRenderer::new_with_fuel(u64)` once (AOT in v3) and renders repeatedly like
`wasm-warm`, but with fuel instrumentation and a per-render budget. Compare
`wasm-warm` (epoch) versus `wasm-fuel` under identical inputs and build settings.
Default `--fuel 1000000000` is a finite conservative starting budget, **not a
measured/calibrated guarantee**; record errors/exhaustion separately from successful
latency and sweep the budget before any acceptance conclusion. The driver accepts
`compare MODE ABSOLUTE_WORKER_PATH [FUEL_BUDGET=1000000000]`.

Optional runner `--concurrency 2` (bounded to 1..4, default 1) submits one request
to each independent initialized host replica before waiting. It excludes native
diagnostics and correctness (select explicit non-native modes). It records
replica identity, per-request latency/resources, batch wall time/successes and
group wall time. Concurrent batch throughput uses the batch wall denominator;
render-only throughput and sum-of-trial-wall throughput are **not aggregate
concurrent throughput**. Process resources/CPU are per replica, not a synchronized
group memory peak. This probes concurrent hosts, **not** shared-module admission,
one process's permit contention, or `render_async` scheduling/drop behavior.
The new public async API is not exercised by this source wave. Synthetic
cancellation/admission tests remain deferred pending stable test fixtures.
V3 adds AOT/JIT mode separation, but no artifact compatibility, AOT load/build
footprint or performance result is yet available.

Exact extra runner flags for future exclusive-capacity experiments:

```sh
# Append required binary/worker/output/cache/build-record/artifact arguments.
python3 scripts/benchmark-mermaid.py --phase timing \
  --modes wasm-warm wasm-fuel --fuel 1000000000 --concurrency 1 \
  --trials 20 --rounds 10
python3 scripts/benchmark-mermaid.py --phase timing \
  --modes current worker wasm-warm wasm-fuel --fuel 1000000000 \
  --concurrency 2 --trials 20 --rounds 10
```

Q2 authorized focused validation, through `nix develop --command`:

- `cargo clippy -p bmux_mermaid_render --all-targets --features wasm -- -D warnings`: PASS.
- `cargo test -p bmux_mermaid_render --features wasm`: PASS.

Q2 used development-profile validation, not release performance results or a
build-footprint measurement. Clippy's reported incremental 0.84 s is **not** a
clean-build timing. No complete matched artifact inventory, static footprint,
release build measurement, benchmark or migration gate result is available.

## Validation handoff and omissions

Q2 validation predates v3 AOT/JIT driver edits and does not validate this source
wave. V3 is source-only: cargo/build/tests/benchmarks are prohibited until AOT
lanes settle and coordinator grants capacity. Benchmark and broader build capacity
remain coordinator-owned.
Required workspace `cargo clippy --all-targets -- -D warnings`, then
`cargo nextest run --no-fail-fast`, and `cargo machete --with-metadata` remain
coordinator obligations; include feature-enabled example validation and the
crate's WASM correctness tests. No CLI/plugin runtime files are changed by this
lane. Formatting/Python syntax checks do not establish Rust compilation,
correctness, performance, guest verification or migration acceptance. Known
omissions above (actual-stop instrumentation, exact memory accounting, raster
correctness, CIs and complete build/deployment provenance) must be filled or
reported explicitly rather than replaced by optimistic claims.
