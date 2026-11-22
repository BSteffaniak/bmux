# Plan: simulations that find races

Phase 19 of the [bmux_pty simulation plan](pty-deterministic-simulation-plan.md), after the
[bug-hunting simulations](bug-hunting-simulation-plan.md) (phase 18).

## Goal

Phase 18 made the simulated world hostile. Phase 19 makes the code's own concurrency
hostile: interleavings that real servers produce but the simulation cannot yet reach, and a
systematic search for the races that need one or two badly timed switches. Every run stays a
pure function of its seed (and, for guided runs, its recorded profile).

Simulating the whole server (bmux server, clients and plugins over in-memory IPC) is still
out of scope.

## Where things stand

- **Threads.** Simulated threads (`switchy_thread`) switch at every lock, atomic, channel,
  sleep, kernel and file operation. One runs at a time, handed the baton by a seeded
  scheduler (random or PCT, weak memory in one run in four).
- **Async tasks.** The `switchy_async` simulator runtime polls every task on one thread. The
  next ready task is drawn from the run's generator. A task runs until an `.await` returns
  `Pending`, so everything it does between two such awaits is atomic with respect to every
  other task. `worker_threads` is ignored.
- **`inject_yields`.** It adds a `yield_now` after every `.await`, but only inside `async fn`
  bodies and `async` methods. It does not reach:
  - `async` blocks inside ordinary functions (the pane runtime has 77, including most spawned
    tasks);
  - closures;
  - awaits inside macro calls such as `select!`.

  Nothing in bmux applies it.
- **Faults.** Phase 18 covers the kernel, files, crashes, power and programs. No fault reaches
  thread spawn failures, wall-clock jumps or slow wakeups.
- **Search.** Random, PCT and guided (outcome-novelty) runs. Nothing is exhaustive.
- **Determinism checks.** `tests/primitives.rs` scans the pane runtime plugin only, and
  `SIMULATOR_VERIFY_DETERMINISM` replays runs in CI.

## Determinism rules (unchanged)

Everything below runs on simulated threads under the seeded scheduler, or draws from the run's
named streams. A worker thread, an injected yield, a fault, and an explored schedule are all
decided by the seed (and, for exploration, by the schedule being replayed). Determinism
verification stays on in every CI run.

## 19.1 Several workers in the simulator runtime (switchy_async)

Real servers run tokio's multi-thread runtime: two tasks run at once, and the synchronous code
between their awaits overlaps at every lock and atomic. One executor thread never overlaps
them, so these failures cannot occur in simulation today:

- deadlocks from two tasks taking locks in opposite orders;
- races in read-modify-write sequences across tasks;
- check-then-act across tasks without an await in between.

The design:

- The simulator runtime gains workers: `Builder::worker_threads(n)` (and a per-run default)
  starts `n` simulated threads in the runtime's thread group. Each takes the next task from the
  shared queue. The choice of task, and of which idle worker takes it, comes from the run's
  generator.
- Workers are ordinary simulated threads. They switch only at switch points and only when the
  seeded scheduler says so, which keeps every interleaving deterministic, as `spawn_blocking`
  threads already are.
- A task is polled by one worker at a time (the existing `polling` flag), and is woken by any
  thread.
- `block_on` keeps driving its own future on the calling thread. With workers, it lets them run
  the other tasks. `tick` and `wait` work as now.
- `shutdown_now` kills the workers with the rest of the group.
- `SIMULATOR_ASYNC_WORKERS` fixes the worker count, or `swarm` draws it per run: 1 (today's
  behaviour) in one run in four, otherwise 2 to 4.
- Tests:
  - an opposite-order lock deadlock between two tasks that one worker never finds and two do,
    by seed;
  - determinism across replays;
  - shutdown;
  - `block_on` with workers.

In bmux, the pane-runtime facade builds its server's runtime with the run's worker count, so
every server operation and spawned task runs on several workers. The pump task becomes the
workers' supervisor: it stops them, and no longer polls tasks itself.

## 19.2 Yields at every await (switchy_async macros, bmux)

`inject_yields` lets another task run after any `.await`, even one that was ready at once. It
needs three changes before it can cover the pane runtime:

- **Reach every `.await`:** inside `async` blocks and `async` closures in any function, not
  only in `async fn` bodies.
- **Awaits inside `select!`:** `switchy_async::select!` (the simulator's own) yields after the
  branch it completes, which covers what the macro cannot see inside it.
- **Seeded, not always:** the injected point yields with a probability drawn per run (0, ¼, ½
  or 1), from the run's generator. Runs differ in how often tasks give way. A probability of 0
  reproduces runs without yields.

Applying it in bmux:

- Apply it to the pane runtime plugin's modules (`inject_yields_mod!`, or the attribute on each
  impl block and function). It expands to nothing natively.
- Enforce it: the primitives scan (or switchy's `async-cargo` lint) fails on an `async fn`,
  `async` block or `async` closure the transformation does not cover.

## 19.3 Faults the code's own failure paths need (switchy)

Seeded, per-run rates, and off unless a simulation configures them. In the pane runtime they
are drawn by the profile.

| Fault | Crate | What it exercises |
|---|---|---|
| Thread spawns fail (`thread::Builder::spawn` errors) | switchy_thread | The runtime's `.ok()` paths ("failed spawning pane process waiter thread", reader threads), never run today |
| Late wakeups: a woken task is queued after a random delay | switchy_async | A busy server's executor; races between a wakeup and a timeout |
| Spurious `Pending`: a ready leaf future (switchy's channels, locks, sleeps) returns `Pending` once, waking at once | switchy_async | Awaits that `inject_yields` cannot reach (tokio's own futures excepted) |
| The wall clock jumps (forward, or backward) | switchy_time | Code that mixes `SystemTime` with intervals: timestamps, expiry, records' start identities |

## 19.4 Exhaustive exploration of short scenarios (switchy_thread, simulation)

Random and PCT schedules find a race eventually. A bounded exhaustive search finds every race
reachable within a few preemptions, for certain. Most real races need only one or two.

- **`switchy_thread`.** Add `Strategy::Explore { preemptions }`: preemption-bounded depth-first
  enumeration of schedules (as CHESS does), built on what exists:
  - choice points;
  - `Schedule` replay;
  - the trace of runnable threads at each decision.

  `simulator::explore(seed, bound, run)` drives it. It replays each prefix, branches at every
  choice point with an untried thread while the preemption budget lasts, and reports the first
  failing schedule. That schedule replays with `SIMULATOR_SCHEDULE` and shrinks as now.

  To keep the space small, it uses sleep-set pruning: it skips schedules that only reorder
  independent operations on different objects, using the scheduler's knowledge of which
  object each operation touches.
- **Scenarios.** A `pane-runtime --explore <scenario> [--preemptions N]` mode runs short fixed
  scenarios: a few operations on one or two panes, two drivers, a fixed profile with no faults.
  1. restart during close;
  2. crash during open;
  3. resize during restart;
  4. close of one split pane during the other's restart;
  5. exit notice during restart;
  6. interrupt during a program's startup.

  Each runs every schedule up to 2 preemptions (3 for the smaller ones) under 1 and 2 workers
  (19.1), checking every invariant.
- **CI.** The nightly runs every scenario to its bound. Per-change CI runs them to 1
  preemption.

## 19.5 Guidance by interleaving coverage (switchy_thread, simulation)

Outcome novelty (phase 18) steers toward new actions, not new interleavings.

- **The coverage.** The scheduler records which ordered pairs of conflicting operations
  occurred: two threads touching the same lock, atomic location, channel or kernel object,
  identified by the code location of each operation (its caller, from `#[track_caller]` on the
  switch points). The recorded set is the run's interleaving coverage.
- **Guided runs** keep profiles whose runs added coverage pairs, not only new outcomes. The
  dashboard shows coverage growth over time.
- **Code coverage.** Coverage from LLVM's instrumentation (`llvm-cov` sanitizer counters) is an
  optional later signal, not part of this phase.

## 19.6 Determinism beyond the plugin (bmux)

- **Extend the scan.** Generalise `tests/primitives.rs` into a shared checker. Run it over
  every crate on the simulated path:
  - `bmux_pty`, `bmux_terminal_grid` and `bmux_terminal_protocol`;
  - `bmux_session_models`;
  - the state crates the runtime calls into;
  - `bmux_plugin` parts used at runtime.

  Each crate states its exemptions.
- **Unordered collections.** `HashMap` and `HashSet` with `RandomState` are forbidden on the
  simulated path, as in the plugin.
- **Verification everywhere.** Every CI simulation run has determinism verification on (it
  does). The nightly also verifies guided and explored runs.

## 19.7 Carried over from phase 18

- **The open Windows close hang with `sweep_on_exit` off** (see phase 18's notes). Profiles keep
  sweeping on by default until it is fixed.
- **The wider workload:**
  - attach and detach with grid snapshots;
  - program executions with exit codes;
  - snapshots and resurrection;
  - managed scopes and reservations.
- **Grid consistency.** What a pane's grid shows matches what its program wrote.
- **Metamorphic runs.** One workload under the three personalities.

These follow 19.1–19.6, and their new races are found under the new workers and yields.

## Order

1. **19.1: workers.** It changes what is reachable at all. The pane runtime adopts it, and
   phase 18's fleets and regression seeds rerun under workers. Fix what they find.
2. **19.2 and 19.3: yields and faults.** Cheap, and they widen every run.
3. **19.4: exploration**, now that workers make the cross-task races reachable.
4. **19.6: determinism hardening**, before the new mechanisms reach code outside the plugin.
5. **19.5: coverage guidance.**
6. **19.7: phase 18's leftovers.**

Each step lands with tests in its crate, the simulation's checks (`clippy`, `--regression`,
fleets and determinism-verified runs), regression seeds for what it finds, and notes below.

## Implementation notes

### 19.6 (scan beyond the plugin)

- **The checker.** `packages/sim-primitives-check` (`bmux_sim_primitives_check`, dev-only)
  holds the plugin's scan. Each crate calls `Scan::new(env!("CARGO_MANIFEST_DIR"))` from its
  own `tests/primitives.rs`, stating its exemptions:
  - `native` prefixes, skipped whole;
  - `exempt` files;
  - `os_named_in` (where `cfg(unix)` and the like may appear);
  - extra `forbid` patterns;
  - `only`, for crates a simulation runs only parts of.

  `// native-only: begin/end` regions, `#[cfg(test)]` items, `tests.rs` and `*_tests.rs` are
  skipped. Two holes are closed:
  - `use` statements are expanded into the paths they import, so
    `use std::sync::{Arc, Mutex};` and multi-line groups no longer hide a forbidden import;
  - statics are caught whatever their visibility.
- **Crates scanned.**
  - The plugin itself.
  - `bmux_pty` (its old scan, with its stricter patterns kept).
  - `bmux_terminal_grid`.
  - `bmux_terminal_protocol` (minus client-side `pointer_output.rs`).
  - `bmux_session_models`.
  - `bmux_session_state` and `bmux_context_state` (their no-op handles run).
  - `bmux_pane_runtime_state`.
  - `bmux_snapshot_runtime` (the dirty flag).
  - `bmux_plugin`: only `plugin_state.rs`, since a simulated runtime's host is a bare state
    registry.
  - `bmux_plugin_runtime`: only `shutdown_progress.rs`.
  - `bmux_local_terminal`.

  Not scanned, because the simulation runs none of their code beyond plain data types and
  constants:
  - `bmux_presentation_state`, `bmux_recording_runtime`, `bmux_ipc` and `bmux_plugin_sdk`;
  - the attach protocols and plugin API crates.
- **Fixes.**
  - `Session::new` drew random ids from inside the plugin's request handlers. The new
    `Session::with_id` takes the plugin's seeded `ids` instead, and the plugin's scan forbids
    the models' random constructors.
  - `SnapshotDirtyFlag` (marked on every pane change) read the host clock and used std atomics;
    it now uses `switchy_time` and `switchy_thread`'s atomics.
  - The protocol engine's trace timestamps read `switchy_time`.
- **Marked safe.** These are std locks held around one operation and never across a switch
  point, or maps that are looked up but never iterated:
  - the state registry's lock and `HashMap`;
  - the style interner's `HashMap`;
  - the grid's line memo;
  - the trace buffer, the shutdown token and shutdown progress's lock.

  Also marked safe: activation registrations and shutdown progress publication, which never
  run in a simulated pane runtime.
- **Closed after the scan.** `PluginStateRegistry::get_or_insert_with` ran its initializer
  under the registry's std write lock. With several workers (19.1), an initializer that
  reaches a simulated switch point would let another simulated thread block on that std lock
  for real, hanging the run instead of reporting a deadlock. Initializers now run with the
  registry unlocked; when callers race, all get the first value registered.