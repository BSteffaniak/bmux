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

### 19.1 (workers)

- **switchy_async.** With `thread-simulator`, each step of a simulator runtime (each `tick`,
  each turn of `block_on` or `wait`) polls up to `n` ready tasks at once:
  - the stepping thread polls one, and the runtime's workers poll the others;
  - local (`spawn_local`) and blocking tasks stay on the stepping thread;
  - a task's future is taken with `try_lock`, so a task woken while another worker polls it
    is queued again, not polled twice;
  - `n` is 1 for a current-thread runtime, `worker_threads` for a multi-thread one, else the
    run's `SIMULATOR_ASYNC_WORKERS` (a number, or `swarm`: 1 in one run in four, otherwise 2
    to 4).

  Workers are `switchy_thread` daemon threads in the runtime's thread group. They are started
  by the first step that needs them and fed through a queue: `shutdown_now` kills them, and so
  does the run's end. A first design started helpers per step. Each finished simulated thread
  keeps its OS thread until the run ends, so long runs reached the OS's thread limit, and a
  refused spawn left a scheduled thread that never ran: the run hung.
- **switchy_thread** gained what workers needed:
  - daemon threads (`Builder::daemon`), killed at the run's end and not reported as leaked;
  - group 0 means no group;
  - `Scheduler::killed` and `try_me`.

  A spawn the OS refuses now waits for other runs to free threads, and panics if none do. It
  no longer returns an error that would make the run depend on the machine.
- **bmux.** Simulations default `SIMULATOR_ASYNC_WORKERS` to `swarm`, and the run's shape
  records `workers:N`. The workload's shared state uses `switchy_thread`'s `Mutex`: a std lock
  held across a switch point blocks for real once drivers run on several workers.
  `PluginStateRegistry::get_or_insert_with` runs its initializer unlocked for the same reason.
  `PANE_RUNTIME_SIM_THREADS_AT=N` prints every thread's state at decision N.
- **What workers exposed** (all in the simulators, none in bmux's own code so far):
  - **A shut-down runtime kept running.** A worker of another runtime (the pump stepping the
    server's runtime) can be mid-poll of one of the server's tasks when a crash shuts the
    server down. Killing the group cannot stop a thread outside it, so a crashed server
    spawned a process after the power failure had killed every process.
    - `shutdown_now` now waits, asleep, for such polls to reach their next await.
    - It records which thread polls each task, so it never waits on its own stack or on a
      killed thread.
  - **Processes died with the server's threads.** A program's thread inherited its spawner's
    thread group. A simulated crash therefore killed the server's processes, so recovery was
    never tested against survivors, and a program not started yet left a process running
    forever with no program.
    - Programs now belong to no group: processes outlive their server, as real ones do.
    - A process whose program never starts ends as killed.
  - **Harness noise.** With `SIMULATOR_ON_FAILURE=stop`, runs cut short by another run's
    failure were reported as failed ("threads never finished"), burying the real failure. A
    resumed panic printed only `Any { .. }`.
- **Results.** 2000 fleet runs under `swarm` workers: 0 failures. The regression seeds pass,
  as do 300 determinism-verified runs.

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

### 19.2 (yields at every await, applied in bmux)

- **Applied.** `src/lib.rs` declares each of the plugin's modules through
  `switchy_async::inject_yields_mod! { mod name; }` (switchy_async's `macros` feature,
  already enabled). Under the simulator the macro loads the module's files, recursively, and rewrites
  every `.await` in them, in `async fn`s, `async` blocks and `async` closures in sync
  functions alike, into `yield_after(..).await`: a point where another task may run, with the
  run's seeded probability (`SIMULATOR_YIELD_PROBABILITY`, swarm by default). Natively it
  expands to the plain declaration. The platform layer is covered too: the macro parses every
  file a module names, whichever targets its `cfg`s keep. One declaration per module in
  `lib.rs` covers everything, so new files under a module need nothing.
- **Effect.** One seed (12345) at probability 1 took 294 simulation steps with only
  `select!`'s yields and 785 with the plugin's awaits yielding too.
- **Enforced.** `tests/yields.rs` runs `switchy_async_cargo::uncovered` (MoosicBox's lint,
  now a workspace dev-dependency) over `src/` and fails listing every `async fn`, `async`
  block and `async` closure outside `#[cfg(test)]` that no `inject_yields` covers, and every
  file it could not parse. A module added to `lib.rs` with a plain `mod` fails it. Its
  `EXEMPT` list (file, function, reason) is empty; a second test rejects stale or unexplained
  entries.
- **Inside macros, by hand.** The transformation cannot see into macro invocations, and
  `select!`'s own yield comes only after a branch's handler finishes (not at all when the
  handler leaves with `break` or `return`). Awaits there are written
  `switchy_async::task::yield_after(future).await`, the same rewrite, by hand (a no-op
  natively):
  - the `select!` handlers in the pane task (reader-failure cleanup) and in the padding
    configure transaction;
  - the `fail_captured_startup!` and `stop_pane!` macros in the pane task.

  The padding preview calls awaited inside `matches!` now await first, where the
  transformation sees them. The lint cannot see into macros either, so these are not
  enforced: new awaits in macro invocations need the same treatment.
- **Exemptions.** None. `include!`d test code (`durable_capture_tests.rs`) is `#[cfg(test)]`
  and never in a simulator build.
- **Checks.** Pane-runtime regression seeds, 300 determinism-verified pane-runtime runs (swarm
  yield probability), pty regression seeds and 300 verified local-terminal runs all passed
  with the yields in place; no new failures.

### 19.3 (the pane runtime's profiles)

- **Two new profile faults.** `threadspawn` (switchy_thread's `set_spawn_failures`) and
  `clockjump` (switchy_time's `set_wall_clock_jumps`, per clock advance). Each is:
  - drawn per profile;
  - printed and parsed (the profile string round-trips);
  - mutated by guided runs;
  - applied as the run starts, before the workload spawns anything, and only when on, so
    `SIMULATOR_THREAD_SPAWN_FAILURES` and `SIMULATOR_WALL_CLOCK_JUMPS` still force them.

  A thread spawn failure excuses a failed open or restart, as a kernel spawn failure does.
- **Old seeds draw as before.** The new fields are drawn after every existing one, so each
  old field gets the same draws (a test checks the order), and both faults draw from streams
  of their own, only while on. The one regression seed without a pinned profile draws both
  off. Every regression seed passes.
- **Both stay off in drawn and mutated profiles for now** (`THREAD_SPAWN_FAULTS` and
  `WALL_CLOCK_JUMPS` in `profile.rs`; the draws stay, so turning them on shifts nothing, and
  `threadspawn=` / `clockjump=` in a profile still force them):
  - **Thread spawns.** `switchy_async`'s simulator expects the spawns of its own threads to
    succeed: `spawn_blocking` and the executor's workers call `.expect("a simulated thread
    starts")`. With failures at 0.5% or 2% of spawns, most runs panic there first (every
    pane's startup uses `spawn_blocking`), before the plugin's own `.ok()` paths matter.
    Those paths look sound: a waiter or reader thread that cannot start fails the pane's
    startup and ends its program. Open in switchy_async: report a failed blocking spawn
    through the join handle, and start the executor's workers without failing (or exempt
    its own threads).
  - **Wall clock jumps.** simvar's harness measures each run's elapsed time on the simulated
    wall clock after the run (`end.duration_since(start).unwrap()` in `run_once`), outside
    the run's panic catching. A run whose clock ends behind where it started panics the
    fleet worker: the run disappears (one seed alone prints `0 runs, 0 failed`), and the
    runs it cuts short fail their determinism replays. The base build fails the same way
    with `SIMULATOR_WALL_CLOCK_JUMPS=0.001`; drawn in one profile in four, verified fleets
    of 300 failed 9 and 13 runs this way. Open in simvar: measure runs on the monotonic
    clock. No pane runtime failure from the jumps themselves was seen.

### 19.4 (exploration)

- **`--explore <scenario|all> [--delays N]`.** Six scripted scenarios
  (`simulation/pane-runtime/src/explore.rs`). Each opens a pane (or a pane and a split),
  then two drivers each take one step at once, then the workload's end closes every pane and
  checks that nothing is left. Fixed profile (shell programs, no faults, no id reuse), one
  seed, so only the schedule varies:

  | Scenario | Personality | Setup | Driver 0 | Driver 1 |
  | --- | --- | --- | --- | --- |
  | `restart-during-close` | Windows | open | close pane 0 | restart pane 0 |
  | `crash-during-open` | Linux | open | open | crash |
  | `resize-during-restart` | macOS | open | restart pane 0 | resize pane 0 |
  | `close-split-during-restart` | Linux | open, split | restart pane 1 | close pane 0 |
  | `exit-during-restart` | Linux | open | type `exit` into pane 0 | restart pane 0 |
  | `interrupt-during-startup` | Windows | open | restart pane 0 | interrupt pane 0 |

- **Scripted drivers in the workload.** `Step`s name a pane by the order panes opened, and
  run through the workload's own actions and checks. A script may act on a pane mid-restart
  or one closed meanwhile, which drawn actions never do:
  - a line typed mid-restart is not expected back (either program may read it);
  - a restart of a pane closed meanwhile must leave nothing behind (invariant 9);
  - a restart refused because the pane closed meanwhile is not a violation;
  - sibling checks (invariant 11) run with two scripted drivers, since what one driver does
    to a sibling marks it touched.
- **The bound counts delays, not preemptions.** `switchy_thread`'s `explore_runs` bounds
  preemptions only: a switch because the running thread blocked is free, so every free
  choice is branched on. In a pane runtime run they compound. A scenario has 160 to 1,200
  choice points, many where several threads wake at once; at a bound of 0 preemptions the
  first scenario passed 5,000 schedules (300 a second) with the tree still widening. It
  cannot prune either: the simulated kernel and executor use the scheduler's public switch
  points, so every run is untracked. So `--explore` bounds *delays* instead (Emmi, Qadeer and
  Rakamarić, 2011): every schedule departing from the default at most `N` times, at choice
  points from the drivers' start to the run's end. A schedule within `k` preemptions and no
  other departure is within `k` delays. Runs are made by `switchy_thread::simulator::run_with`
  replaying each schedule, driven as simvar's step loop drives a run. Each run's choice
  points are read through the decision hook, and checked against the scheduler's own count
  and the schedule it followed, so a nondeterministic scenario is reported, not explored
  wrongly. Open in switchy_thread: delay bounding (or a window of choice points) in
  `explore_runs`, and footprints from switchy_pty and switchy_async so sleep sets can prune.
- **Measured** (on a 15-core laptop; with two delays, 12 explorations ran at once beside
  other builds, so those times are 3 to 4 times a run's unloaded cost, and the larger ones
  stopped at a 300,000-run cap):

  | Scenario | Workers | Choice points (window) | 1 delay: runs, time | 2 delays: runs, time |
  | --- | --- | --- | --- | --- |
  | `restart-during-close` | 1 | 356 (275) | 442, 0.8s | 92,291, 534s |
  | `restart-during-close` | 2 | 408 (334) | 460, 1.4s | 109,095, 543s |
  | `crash-during-open` | 1 | 307 (234) | 436, 1.6s | 92,407, 489s |
  | `crash-during-open` | 2 | 164 (93) | 133, 0.5s | 11,362, 91s |
  | `resize-during-restart` | 1 | 510 (437) | 722, 3.5s | 255,407, 1,643s |
  | `resize-during-restart` | 2 | 821 (750) | 1,319, 6.4s | over 300,000 |
  | `close-split-during-restart` | 1 | 747 (553) | 858, 6.3s | over 300,000 |
  | `close-split-during-restart` | 2 | 1,217 (1,014) | 1,926, 14.1s | over 300,000 |
  | `exit-during-restart` | 1 | 325 (252) | 337, 0.7s | 57,065, 352s |
  | `exit-during-restart` | 2 | 632 (561) | 777, 2.6s | over 300,000 |
  | `interrupt-during-startup` | 1 | 160 (79) | 131, 0.1s | 7,120, 57s (16s alone) |
  | `interrupt-during-startup` | 2 | 651 (577) | 881, 6.3s | over 300,000 |

  A run costs 2 to 6 ms. Every scenario to one delay, both worker counts, takes about 40s of
  processor time (12s on the laptop, in parallel). Two delays take roughly the square of the
  one-delay count over two: up to about 1.9 million runs (close-split under two workers),
  a few hours on one core, hence one nightly job per scenario.
- **Found.**
  - A workload race, under two workers (`crash-during-open`, schedule `79=7`): a driver chose
    its action and counted it under way in two locked sections, so a crash between them saw
    nothing under way and tore the server down under the action ("1. opening a pane failed:
    the server died"). Fixed: one locked section, with the generation the action acts on.
  - A scenario check: a restart refused because a scripted close removed its pane meanwhile
    ("runtime not found for session") is no violation.
  - No pane runtime bug: every scenario passes every schedule to one delay, and to two delays
    as far as each ran (table).
- **Failures** are shrunk with `switchy_thread::simulator::shrink` and printed with their
  replay command, `--explore <scenario> --workers N --schedule <schedule>` (or
  `SIMULATOR_SCHEDULE`).
- **Workers.** `--explore` runs each scenario under 1 and 2 workers, each in a process of its
  own (`SIMULATOR_ASYNC_WORKERS` is read once per run), as many processes at once as there
  are processors.
- **CI.** Per-change CI (`ci.yml`, the PTY Simulation job) runs `cargo sim pane-runtime
  --explore all --delays 1`. The nightly (`pty-simulation-nightly.yml`, job `explore`) runs
  each scenario in its own job to its full bound, 2 delays (`cargo sim pane-runtime --explore
  <scenario>`). Three delays is out of reach: it multiplies each bound-2 count by roughly
  the number of choice points again.
