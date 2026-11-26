# Plan: determinism as a guarantee

Phase 20 of the [bmux_pty simulation plan](pty-deterministic-simulation-plan.md), after
[simulations that find races](race-hunting-simulation-plan.md) (phase 19).

## Goal

Every simulated run is a pure function of its inputs:

- the seed;
- the simulator's environment variables;
- the build;
- for guided or explored runs, the recorded profile or schedule.

Today that holds by convention: code is meant to use switchy's primitives, and a regex scan of
some crates checks that it does. Phase 20 turns it into a guarantee, in three ways:

- **Unreachable:** sources of non-determinism cannot be reached from a simulated run.
- **Loud:** a source that is reached anyway fails the run, with a pointer to the fix.
- **Checked:** every run's determinism is verified against everything the run observed, not
  only its schedule.

## Principles

- **Nothing in application code.** bmux code keeps using switchy's facades (`switchy_time`,
  `switchy_random`, `switchy_thread`, `switchy_fs`, `switchy_env`, `switchy_async`). They are
  std or tokio natively, at no cost, and that is the only requirement on it.
  - There are no simulation hooks, `record()` calls or simulation-only `cfg` branches in
    product logic.
  - Exemptions live in manifests (`[package.metadata.switchy]`), not in code.
- **On by default.** A crate that runs under simvar with switchy's `simulator` features gets
  every check below without opting in:
  - the interception;
  - the tripwires;
  - the full-observation verification;
  - the dependency-graph check.
- **Harness-owned.** The machinery lives in MoosicBox (switchy and simvar), so every simvar user
  gets it. bmux contributes only its simulations and its manifests' metadata.
- **Explainable failures.** Every failure names the source, where it was reached (backtrace or
  file and line), and the switchy replacement.

## Where non-determinism can come from today

The audit (2026-11-22) found these routes, none of them checked:

| Route | Today | Example |
| --- | --- | --- |
| Unscanned bmux crates on the simulated path | 12 of 50 linked bmux crates scanned | `bmux_plugin_sdk`: `Uuid::new_v4` (`host.rs:440`, `callback_execution.rs:567`), real `Instant::now`, process-wide counters; `bmux_ipc`, `bmux_config`, `bmux_perf_telemetry`: real clocks, process-wide statics |
| Third-party crates | Not scanned at all | `getrandom` 0.2, 0.3 and 0.4, `rand` and `uuid` (OS randomness); `hashbrown` and `indexmap` hashers; `once_cell` statics; `mio` and `nix` (OS calls) |
| std's `HashMap` and `HashSet` | Banned only in scanned crates | Iteration order follows per-process random keys |
| tokio primitives re-exported by the simulator | Deterministic only because one thread runs at a time | `Mutex`, `RwLock`, `Semaphore`, `Notify`, `oneshot`, `broadcast`, `CancellationToken` |
| Process-wide state shared by parallel runs | Unchecked (switchy's own counters are harmless: they preserve relative order and are not hashed) | Caches, registries and buffers in statics |
| The real environment | Read freely | `std::env::var`; `switchy_pty`'s `vars_os` spawn path |
| Verification | In-process replay, comparing the schedule, steps, result and hand-recorded values | It misses: different data that does not change the schedule; interference between parallel runs; dependence on addresses or the per-process hash seed |

## 20.1 Verification against everything a run observed (simvar, switchy)

Measure first: this tells us what leaks today, and catches whatever the other steps miss.

- **The run's digest.** Each switchy simulator contributes to a per-run digest automatically,
  through a registration in `switchy_simulation` (no application calls):
  - `switchy_thread`: the schedule, the trace hash already kept.
  - `switchy_pty`: processes spawned, with argv, exits and signals, and each terminal's output
    bytes.
  - `switchy_fs`: at the run's end, the file tree with content hashes and metadata.
  - `switchy_random`: draws per named stream, count and value.
  - `switchy_time`: the final simulated clock.
  - `switchy_tcp`: connections and bytes.
  - simvar: the run's result, error text and violations.

  The digest is a stream of per-decision checkpoints, not one hash, so a mismatch can be
  located.
- **Replay in a fresh process.** `SIMULATOR_VERIFY_DETERMINISM=process` re-executes the
  simulation binary for one run (seed, epoch offset, profile and schedule passed as today's
  replay variables), alone. The fresh process runs perturbed on purpose:
  - a different ASLR layout and std hash seed (inherent to a new process);
  - process-wide id counters started at a random offset;
  - no other runs beside it.

  It compares the digests. `=1` keeps today's in-process replay, which is cheaper.
- **Locating a divergence.** On a mismatch, report:
  - the first checkpoint where the two runs part;
  - each side's operation and thread;
  - backtraces at that decision (both processes re-run to it with `BACKTRACE_AT`).
- **CI.** Per-change runs keep in-process verification for every run, and add process
  verification for a sample (say 20 runs per simulation). The nightly verifies every run in a
  fresh process.

## 20.2 One check over the whole simulated dependency graph (switchy tooling)

Replace the per-crate `tests/primitives.rs` scans with one command that covers whatever a
simulation links, automatically.

- **`cargo sim <name> --check`** (backed by a `switchy_cargo` library, which grows out of
  `switchy_async_cargo`):
  1. It resolves the simulation binary's dependency graph (`cargo metadata`).
  2. For every local crate in the graph, it runs a `syn` scan with real path resolution
     (`use` groups, aliases, re-exports, macro bodies where parseable). It bans:
     - `std::time::{Instant, SystemTime}::now`;
     - `std::thread::{spawn, sleep, Builder}`;
     - std's `Mutex`, `RwLock`, `Condvar` and `mpsc`, and std atomics, in shared state;
     - `HashMap` and `HashSet` with `RandomState`;
     - `Uuid::new_v4` and `rand::thread_rng`;
     - `std::env::var` and `std::env::vars`;
     - `std::fs` and `std::net`;
     - `static` items holding data, and `thread_local!`;
     - tokio's runtime, `spawn`, `select!` and `time` used directly.
  3. **Exemptions** are declared once in the crate's manifest, with a reason each:

     ```toml
     [package.metadata.switchy]
     native-only = ["src/platform/unix.rs"]
     allow = [{ path = "src/clock.rs", item = "std::time::Instant::now", reason = "..." }]
     ```

     `cfg(not(feature = "simulator"))` and `#[cfg(test)]` code is skipped by understanding the
     `cfg`, not by name patterns.
  4. **Third-party crates** must be on an audited list kept in the simulation workspace
     (`simulation/determinism-audit.toml`). Each entry gives the crate and version range, a
     verdict (`pure`, `intercepted` or `exempt`) and a reason. A new dependency or version in
     the graph fails the check until it is audited, as `cargo vet` does. Audits of common
     crates (`uuid`, `bytes`, `serde`, `regex`, …) ship with switchy as a shared baseline.
- **Running it.** The pane-runtime simulation's `--regression` mode runs the check first, and
  CI runs it on every change. The existing `bmux_sim_primitives_check` and per-crate tests are
  deleted once the check covers them.

## 20.3 Interception in simulation builds (switchy, simulation workspace)

Make the common sources deterministic outright, below application code.

- **Randomness.** `switchy_random`'s `simulator` feature registers custom backends for
  `getrandom` 0.2 (the `custom` feature with `register_custom_getrandom!`) and for 0.3 and 0.4
  (`getrandom_backend="custom"`). They draw from the current run's named stream
  (`getrandom`), and panic outside a run.
  - The `cfg` is set in `simulation/.cargo/config.toml` only, so real builds never see it.
  - This covers `uuid`, `rand` and every hasher seeded through `getrandom`.
  - Verify per hasher in the graph (`ahash`, `foldhash`, `hashbrown`'s default) where its seed
    comes from. A hasher seeded otherwise (addresses, std's keys) goes on the audit list as
    `exempt` only if it is never iterated, or is replaced.
- **std's `HashMap` keys** cannot be intercepted (std draws them itself). They are banned by
  20.2 on the simulated path, and 20.1 catches any slip.
- **The environment.** The simulator seals it: `switchy_env`'s simulator serves a per-run
  environment from the simulation's configuration. Real variables reach a run only when listed
  (`SIMULATOR_*`, and any the simulation passes through). `switchy_pty`'s spawn uses the
  simulated environment.
- **Process-wide state.** switchy's own counters become per-run state:
  - lock and channel keys;
  - task and runtime ids;
  - the yields configuration.

  bmux's statics on the simulated path go through `run_shared`, or are reported by 20.2.

## 20.4 Tripwires (switchy, simulator builds)

Fail loudly where a source is reached anyway.

- **A thread outside the scheduler.** Simulated code running on an OS thread the scheduler
  does not know (a `std::thread::spawn` that slipped through) panics at its first switchy call.
  Today it can block on the baton or run unscheduled.
- **Switchy facades outside a run** (a real clock read, a real file opened) in a simulation
  build panic with the call site, instead of silently using the host.
- **Real clocks.** A read of std's clock cannot be intercepted portably. On Linux, a
  simulation-build-only check can still catch it, by counting `clock_gettime` calls from inside
  a run (seccomp-free, using an `LD_PRELOAD`-style test shim in CI). It is optional, and it
  backs up 20.2's ban.

## 20.5 switchy versions of tokio's primitives (switchy_async)

Replace the simulator's re-exports of tokio's `Mutex`, `RwLock`, `Semaphore`, `Notify`,
`oneshot`, `broadcast` and tokio-util's `CancellationToken` with switchy implementations, as
`watch`, `mpsc`, `mpmc` and `Barrier` already are:

- They are deterministic by construction (wake order defined, no shards).
- They are switch points, so 19.5's coverage and 19.4's exploration see them.
- They are subject to the spurious-`Pending` fault (19.3).

Contract tests run each against tokio's to keep the semantics identical.

## Order

1. **20.1: verification against full observation, in a fresh process.** It tells us what
   leaks now. Run it over the existing fleets and fix what it finds.
2. **20.2: the graph-wide check, with the audit list.** It covers what is linked today; the
   per-crate scans are retired.
3. **20.3: interception** of randomness, the environment and process-wide state.
4. **20.4: tripwires.**
5. **20.5: switchy versions of tokio's primitives.**

Each step lands with tests in MoosicBox, adoption in bmux's simulations, and a page for simvar
users: "making a crate simulatable", with what the checks require and how to state an
exemption.

## Open questions

- Is the digest's cost acceptable on every run? Content hashes of the final file tree are
  cheap, and terminal output is hashed incrementally. If needed, process verification can
  cover only a sample.
- How do we handle macro-generated code `syn` cannot see? Proc macros the graph uses go on the
  audit list. `macro_rules!` bodies are scanned as token streams.
- **Decided: scope.** 20.2 checks the simulated dependency graph only. All of bmux follows
  "switchy everywhere" at the end of the simulator migration, but not before. Until then, a
  crate is held to the rules once a simulation links it.

## Implementation notes

### 20.1 (MoosicBox `b6a8dbf14`, `93aba9019`)

- **The digest.** `switchy_simulation::digest` keeps, per run, a hash per source, a running
  total, and the first 100,000 observations as checkpoints with their text (up to 64 KiB
  each). Simulators observe as things happen (`observe`, `observe_debug`), and through
  end-of-run observers registered once per process (`on_run_end`). Each observer checks
  `Context::existing_state` and stays silent for runs that never used its simulator.
  - `switchy_pty`: each spawn and exit; at the end, the signal log, every byte each
    terminal and pipe was written, and the processes left.
  - `switchy_fs`: the whole file system at the end.
  - Every named random stream's final state, which also encodes its number of draws.
  - simvar: the schedule's trace hash, the steps, the simulated time and wall clock, and
    `record()`ed values.

  `switchy_tcp` does not observe yet; no bmux simulation uses it.
- **Verification.** simvar compares whole digests. `SIMULATOR_VERIFY_DETERMINISM=process`
  re-executes the simulation for one run with its replay variables and `SIMULATOR_PERTURB`.
  Perturbing starts switchy's process-wide counters (context, task, runtime, async thread
  and wait-key ids) at a random offset. The child reports its digest, trace and schedule
  back through `SIMULATOR_DIGEST_OUT`.
- **Locating a divergence.** A failure lists:
  - each source that differs;
  - the first observation that differs, with the text around the first differing
    character on both sides;
  - where the thread schedules part.

  `SIMULATOR_DIGEST_BACKTRACE_AT=N` adds a backtrace to observation N. Re-running both
  processes up to a decision (the plan's `BACKTRACE_AT`) was not needed: the text located
  the only divergence found.
- **Cost.** A fresh process per run costs milliseconds: 1000 pane runtime runs verified
  this way take 7 seconds. So CI, the nightly job, AGENTS.md's checks and the regression
  runner all use `process`, not the sample the plan proposed. `=1` stays available.
- **Found.** The pane runtime's durable-state writer named its staging file
  `<name>.tmp-<pid>` with the host's process id. A run's final file system depended on
  which process ran it (2 of 100 runs verified in a fresh process).
  - The staging name now comes from `ids::new_uuid()`.
  - The plugin's own process id (readiness) comes from the platform layer's `files`: the
    host's natively, a stand-in (`1`) simulated.
  - `bmux_sim_primitives_check` bans `process::id()`.

  Nothing else differed in 1000 pane runtime runs (guided ones too), and 300 each of pty
  and local terminal.

### 20.2 (MoosicBox `f28ef693f`, `af3931b78`)

- **The check.** `switchy_cargo` resolves a simulation's graph with `cargo metadata`:
  normal dependencies for the host, not entering proc macros. `cargo sim NAME --check` runs
  it through `simulation/check` (`sim-check`), a separate member, so the check's own
  dependencies stay out of the simulations' graphs. CI runs it for the three simulations.
- **Local crates** are scanned by the rules bmux's per-crate scan had grown, now shared in
  `switchy_cargo`. Two changes:
  - Files loaded only by `#[cfg(test)] mod` declarations are skipped.
  - Lines several patterns match are reported once.

  Rules moved from the twelve `tests/primitives.rs` files into the crates' manifests, and
  `bmux_sim_primitives_check` is gone. Rules naming missing files are errors.
- **What was found.** Run over the whole graph, the rules reported about 700 lines in 18
  crates never scanned before. An instrumented build of the pane runtime simulation (800
  runs, standard and guided) settled which of them run: in 16 of the crates not one line ran.
  - Eight run only around runs (the dashboard and TUI stack, the client, terminal I/O,
    benchmarks, install manifests).
  - Six are linked by code the simulations run, but not exercised by it: configuration,
    IPC models, the plugin SDK, performance telemetry, presentation facts. Each says to drop
    its `runs = false` before a simulation runs it.
  - `bmux_color`'s one finding is a constant table (marked safe). `bmux_cell_geometry`'s
    are in a test-only module, which the scanner fix above now skips.
  - The simulations' own crates (the workload, `sim_support`) state what of them is
    harness. The pane runtime simulation's host switches go through one annotated helper.
- **External crates.** `simulation/determinism.toml` lists all 256 the simulations link.
  - 26 `controlled`: switchy and simvar.
  - 19 `restricted`: `uuid`, `rand`, `getrandom`, `tokio`, the hashers, `tempfile` and
    others, whose non-deterministic entry points the rules forbid.
  - 110 `pure`: 59 read, and 51 of the baseline that `--scan-external` found clean (below).
  - 97 `unaudited`: a baseline of 2026-11-22, pinned to exact versions.
- **The parsing scan** (`af3931b78`). The line scan gave way to one that parses each crate
  (`syn`) from its roots:
  - It follows `mod` declarations as rustc does, reading only the files the crate compiles.
    `#[path]` is followed, and so are modules declared inside a macro. The pane runtime's
    `inject_yields_mod!` modules had never been scanned before.
  - `cfg`s are evaluated for a simulation build: `test` is false, features come from the
    resolved graph, and an OS predicate is scanned on both sides.
  - Paths are resolved before they are judged (`use` groups, aliases, globs,
    `self`/`super`/`crate`, the crate's own re-exports, 2018 uniform paths). A planted
    `use std::sync::Mutex as Lock;` and an `env::var` inside `dbg!` are both caught.
  - Imports are judged first; an imported primitive is reported once, at its `use`.
  - `thread_local!` counts as process-wide state.

  The three simulations scan with 0 findings. `SWITCHY_CARGO_SHOW_UNREACHED=1` lists the
  source files no `mod` reaches: tests, other platforms, feature-gated code, and one dead
  file (`plugins/pane-runtime-plugin/src/runtime/terminal_session.rs`, declared nowhere).
- **Evidence for audits.** `switchy_cargo --scan-external` scans every external crate by the
  shared rules, with the simulations' features (252 crates in about 11 s). 55 baseline crates
  scanned clean. 51 of them are now `pure`, with that as their reason. Four stay
  `unaudited`, with a note: `hostname` (reads the host name through libc) and
  `libsqlite3-sys` (C code), which the scan cannot see into; `lazy_static` and `scoped-tls`,
  whose macros put state in the calling crate. The first external scan hung on `dispatch2`:
  objc2's generated module and the crate root glob-import each other, and the resolver
  expanded globs through globs exponentially. It now expands a glob once per path.
- **Not yet** (all since done; see "Follow-ups" below).
  - Foreign functions and `libc` calls were invisible to the scan.
  - Macros are scanned as tokens, not expanded: a macro from another crate that reaches a
    primitive (`lazy_static!`, `scoped_thread_local!`) is caught through its audit's `forbid`.
  - `runs = false` claims are now checked by a nightly coverage run (20.4 below).
  - The 97 baseline crates were not audited.

### 20.3 (MoosicBox `db9119472`, `974999fa7`, `fade23050`)

- **Randomness.** `simulation/.cargo/config.toml` builds simulations with
  `--cfg getrandom_backend="custom"`; `cargo sim` runs cargo in `simulation/` so it applies,
  and real builds never see it. `switchy_random` (simulator feature) defines
  `__getrandom_v03_custom`, the one symbol `getrandom` 0.3 and 0.4 both call: bytes come from
  the run's `getrandom` stream, which the digest covers, and a call outside a run panics.
  `getrandom` 0.3/0.4 are now `controlled` on the audit list.
  - `getrandom` 0.2 cannot be redirected: its `custom` feature is a fallback after every OS
    backend. Its callers (`rand` 0.8's `OsRng`, `thread_rng`) stay forbidden.
  - The hashers in the graph do not seed through `getrandom`: `ahash` (no `runtime-rng` in
    this build) seeds from addresses, `foldhash` from addresses and the clock. They stay
    `restricted`: `HashMap`/`HashSet`, where iteration would show the seed, are forbidden in
    scanned crates, and 20.1's fresh-process replay (a new address layout) catches a slip.
  - The `Uuid::new_v4` ban stays, though `new_v4` is now a function of the seed: identifiers
    still come from switchy_uuid or a crate's `ids` module, which say where they come from.
- **The environment.** Inside a run, `switchy_env` serves a sealed environment per run: the
  host's `SIMULATOR_*` variables, those passed through (`pass_through`), and the defaults,
  unless the run `isolate`s its own. Outside a run nothing changed. `switchy_pty`'s simulated
  spawns already start from a fixed environment.
- **"Inside a run".** `switchy_simulation` now knows which contexts are runs' (`begin_run`,
  `in_run`): simvar marks each run's, `switchy_thread::simulator::run` marks its own.
- **Process-wide counters.** Task, runtime and lock-key ids come from
  `switchy_simulation::next_id::<Kind>()`: counted per run from 1 inside a run, and from a
  separate process-wide range outside one. Runtimes compare by identity instead of id. The
  yields configuration was already per run.

### 20.4 (MoosicBox `fade23050`, `7086c88f0`)

- **Threads outside the scheduler.** `switchy_simulation::require_contexts`: once simvar
  starts, a thread reaching a switchy simulator without a context panics at that call, naming
  the problem, instead of getting a fresh clock, seed and schedule of its own. simvar gives its
  main thread, its workers and each run explicit contexts; simulated threads inherit their
  run's. All five simulations run clean under it.
- **Randomness outside a run** panics (20.3).
- **`runs = false`, checked.** `cargo sim NAME --coverage` builds NAME with
  `-C instrument-coverage` in `target/sim-coverage`, runs it, merges the profiles with LLVM's
  tools, and `sim-check NAME --coverage EXPORT` (`switchy_cargo`'s `run_claims`) fails if a file
  of a `runs = false` crate ran. The nightly workflow runs it for the three pane/pty
  simulations, guided, 300 runs each.
- **Found on the way.** With simvar's `fs` feature (which `consensus-cluster` enables, so any
  simulation built beside it), a run reset its filesystem before its clock, stamping the root
  with an epoch offset the run then replaced; every process-verified run failed. The clock is
  now reset first (MoosicBox `e8b38c4db`, with `Found-by`).
- **Not done: counting real clock reads.** The optional `LD_PRELOAD` shim counting
  `clock_gettime` inside runs was left out: 20.2 forbids std's clocks on the simulated path,
  20.1's replay catches a read that changes what a run observes, and the thread tripwire
  catches the usual way a real clock is reached (an unscheduled thread).

### 20.5 (MoosicBox `239d3dda2`, `b4f712a52`, `a0b0aa529`, `3185c5e7b`, `aa1828257`)

- The simulator's `Mutex`, `RwLock`, `Semaphore`, `Notify`, `oneshot`, `broadcast` and
  `CancellationToken` are switchy's own, replacing tokio's re-exports (`watch`, `mpsc`, `mpmc`
  and `Barrier` already were). Waiters are woken oldest first from one ordered list (the one
  `watch` had, now shared); no shards, no thread-local generator. Semaphore, `RwLock` and
  `broadcast` mirror tokio's designs (fair permits, the reader-permit trick, the slot ring), so
  capacity, lag and fairness behave the same.
- Every leaf future is subject to the spurious-`Pending` fault (19.3). `blocking_*` calls from
  a simulated thread wait on `switchy_thread`'s scheduler, so its exploration and coverage see
  them; otherwise, like `watch` and `mpsc`, they are switch points only through the fault.
- `tests/sync_tokio_contract.rs` runs 17 scenarios against both implementations with
  recording wakers: results and wake order must match. `tests/sync_determinism.rs` checks
  first-in-first-out order, replay per seed, and the fault.
- **Found on the way.** `Runtime::block_on` lost a wake its future sent itself while polled
  (`yield_now`) when other simulated threads were alive, and waited on the scheduler for
  another; it now polls again (`aa1828257`).
- duckdb's gate and `moosicbox_scan` named tokio's lock types beside switchy's locks, which
  compiled only while switchy re-exported tokio's (`3185c5e7b`).
- **API differences** from tokio, none used by bmux or MoosicBox: `Semaphore`'s `Debug` text;
  mapped guards accept unsized targets; `Notified` and the cancellation futures are `Unpin`
  (and the latter `FusedFuture`); `Mutex::new` and `RwLock::new` are `const`.

### Follow-ups (2026-11-25; MoosicBox `23c66f735`..`16c560e99`)

- **The audit list has no `unaudited` entry left.** Coverage of 300 guided runs of each
  simulation, on Linux and on macOS, showed which crates run at all: of the 97, ten.
  - `futures-core` and `futures-channel` are `pure`.
  - The other eight are `restricted`, with their uncontrolled entry points now in `forbid`:
    `futures`' unseeded `select!`, simvar's Ctrl-C handler (`ctrlc`, and the `nix` and
    `dispatch2` it reaches), `rustix`, `scoped_thread_local!`, and `log`'s and `tracing`'s
    enabled-level checks.
  - The rest are `not-run`, a new verdict: like a local `runs = false`, a claim
    `cargo sim NAME --coverage` checks nightly. FFI bindings (`libc` and three `-sys` crates)
    are `restricted` instead, since coverage cannot see calls into C.
  - Four entries were stale (`crossbeam-*`, `symlink`, `tracing-appender`) and are gone.
- **Foreign functions.** switchy_cargo reports `libc` paths and `extern` blocks, so an external
  crate's scan now shows them too.
- **Macros.** Covered by the audits: a crate whose macro reaches a primitive has that macro in
  `forbid` (`scoped_thread_local`, `log_enabled`).
- **Floating point.** `f64::powf` and the like call the platform's math library, which rounds
  differently between systems. `switchy_random::gen_range_dist` used it, so a seed drew
  different values on Linux and macOS (`historical_seeded_replay` had failed on macOS since it
  was recorded on Linux); it now uses `libm`. bmux's OKLab and sRGB conversions and Lanczos
  kernel moved to `libm` too, and switchy_cargo now forbids those methods.
- **`--coverage`, run end to end**, including a false claim (`bmux_terminal_grid` marked
  `runs = false`), which it caught.
- **The dashboard** now starts a verify job (simvar's `control().verify`) when a failure's
  confirming replay passes. The job runs the seed recording what it observes, replays it in a
  fresh process, and the failure's page shows where the two part (or that they agree, which
  points at state left by earlier runs in the fleet's process).
- **Found on the way.** `bmux_terminal_protocol`'s new `notification_output.rs` read the
  environment, which made every simulation's `--check` fail on master; it is client side and
  now exempt, as `pointer_output.rs` is.

### Follow-ups, continued (2026-11-26)

- **Windows in the pty simulations.** `bmux_pty`'s Unix and simulated module follows
  `switchy_pty::behavior()` instead of the operating system, so `cargo sim pty` and
  `cargo sim local-terminal` now draw Linux, macOS or Windows. Under Windows a process runs in
  a kill-on-close job (`switchy_pty::PtyJob`), its console keeps its own cursor and asks for
  no win32 input mode (`PtyOptions`), and with output captured or input fed it runs on pipes
  alone (`Behavior::console_on_all_streams_or_none`, `PtyChild::take_stderr`). The pty
  workload counts a process handle's drop as signalling its session there (it closes the
  job). Native Windows runs the same module: `switchy_pty`'s Windows backend gained jobs,
  console options and console-less processes on pipes (non-blocking reads by peeking the
  pipe), validated on Windows in CI before landing, and `conpty.rs` is gone.
- **Real clock reads, counted.** `cargo sim NAME --clock-reads` runs NAME with
  `switchy_clock_probe` loaded (`LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`); simvar lists every real
  clock read made inside a run by its caller, and the nightly job fails on any not allowed.
  The simulators' own reads (simvar timing a run, `switchy_thread` waiting for the OS to
  spare a thread) and `foldhash` seeding its hashers (simulated maps are lookup-only) are
  allowed. Nothing else read the clock in 60 runs of each simulation on macOS.
