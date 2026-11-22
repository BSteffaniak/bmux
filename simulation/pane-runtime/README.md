# pane-runtime: the pane runtime simulation

Deterministic simulation testing for the pane runtime plugin
(`plugins/pane-runtime-plugin`), the code that runs bmux's panes. Run it with
`cargo sim pane-runtime`; see [the simulation README](../README.md). It runs
on any host: each run's simulated kernel behaves as Linux, macOS or Windows
(drawn from the seed, or `SIMULATOR_PTY_PERSONALITY=linux|macos|windows`), and
the plugin takes that system's paths (on Windows: the pseudo-console's startup
cursor query, output that ends when the console closes, and job-based stops).

Each run starts a pane runtime through the plugin's `simulation` driver (its
real tasks, threads, terminals and process control, built with its
`simulator` feature) whose panes run a simulated shell, then does random
things to it: opens up to three panes, types lines (some start jobs that
ignore hang-ups), resizes, restarts and closes them, waits, and crashes the
server and starts a new one on the same runtime directory. The run's seed
decides every task and thread interleaving, timer, identifier and kernel
choice, so a failure replays exactly on the same build.

Invariants checked:

1. An opened pane's program runs and leads a session of its own.
2. Every line typed into a pane reaches its program, in order.
3. A pane's terminal ends up the size last requested.
4. Closing a pane drains it, cleans up, and leaves nothing running in its
   session.
5. The workload finishes within a budget of simulated time, and every pane's
   threads end with it (no deadlock, livelock or leaked thread).
6. After a crash, once the new server starts, nothing of a session the crashed
   server owned runs (Unix: the new server swept the recorded sessions;
   Windows: closing the jobs ended them), and every process it never owned
   (ids reused or not) still runs.
7. Restarting a pane ends everything in its old program's session, and the new
   program leads a session of its own.

`tests/platform.rs` runs the plugin's platform conformance scenarios (sweeps,
releases, cooperative stops, process identities) against the simulated target
under every personality; the plugin's own tests run them natively.

```bash
cargo sim pane-runtime                                       # one random run
SIMULATOR_RUNS=300 SIMULATOR_VERIFY_DETERMINISM=1 cargo sim pane-runtime
SIMULATOR_SEED=42 SIMULATOR_RUNS=1 cargo sim pane-runtime    # replay one run
PANE_RUNTIME_SIM_STATS=1 cargo sim pane-runtime              # what each run did
SIMULATOR_SEED=42 SIMULATOR_RUNS=1 cargo sim pane-runtime --tui   # watch it
PANE_RUNTIME_SIM_TRACE=1 SIMULATOR_SEED=42 SIMULATOR_RUNS=1 \
  SIMULATOR_VERIFY_DETERMINISM=1 cargo sim pane-runtime      # every decision
```

`--tui` shows the runs in the same dashboard as the pty simulation (scheduler,
invariants, pause, step, go back, breakpoints), with a Panes tab (each pane's
process, size, lines typed and state) and the simulated kernel's processes,
signals and terminal screens.

`PANE_RUNTIME_SIM_TRACE` prints every scheduling decision, so diffing a run
against its replay shows where they part when a seed does not replay;
`PANE_RUNTIME_SIM_BACKTRACE_AT=N` adds where the stopping thread was at
decision `N`.

Found so far: the simulator's `select!` chose among ready branches with a
generator outside the simulation (fixed in `switchy_async`), and pane
identifiers, directory listings and process-wide bounds that leaked between
runs (now seeded, sorted and run-scoped). As Windows: a session released before
its leader was reaped was swept again later, failing on the closed job. With
restarts: the simulated kernel kept a terminal pointing at an ended session,
so closing it hung up whichever session reused the id (fixed in
`switchy_pty`, as POSIX kernels disassociate the terminal).
