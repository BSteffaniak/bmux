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
