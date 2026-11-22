# Plan: simulations that hunt for bugs

Phase 18 of the [bmux_pty simulation plan](pty-deterministic-simulation-plan.md),
after the [simulation dashboard](simulation-dashboard-plan.md).

## Goal

The simulations run bmux's real pane runtime on a deterministic kernel, but the
workloads are polite:

- one driver does one thing at a time;
- every run uses the same uniform mix of actions;
- the pane runtime's kernel never fails;
- the programs are well-behaved shells;
- a fraction of the runtime's features is exercised;
- invariants check only what each step promised;
- one scheduling strategy and clock rate apply to the whole fleet.

Phase 18 makes the simulations adversarial, while keeping every run a pure
function of its seed (plus, for guided runs, a recipe that the run records).

Simulating the whole server (bmux server, clients and plugins over in-memory
IPC) is **not** part of this phase. It comes after this one is solid.

## Determinism rules (unchanged, restated)

Everything new draws from the run's seeded streams (`switchy_simulation::stream`)
and runs on the run's primitives: `switchy_async` tasks and timers,
`switchy_thread` threads and locks, `switchy_time` clocks, `switchy_env`, the
simulated kernel (`switchy_pty`) and, new in this phase, the run's simulated
filesystem (`switchy_fs`). `SIMULATOR_VERIFY_DETERMINISM=1` replays runs to
check this, and the plugin's `tests/primitives.rs` scan enforces it in source.

## 18.1 Concurrent drivers

The workload becomes 1–4 drivers (drawn per run), each an async task on the
run's executor sharing one `Workload` behind a `switchy_async` mutex that is
released across every await. So:

- a close can race a resize;
- a crash can land mid-open;
- a restart can overlap typing;
- input can arrive while a pane is still starting, before it commits (a
  pane's `Opening` state is visible to the other drivers).

Invariants are written so they hold under overlap. Each check snapshots what it
needs, and a pane that another driver closed or restarted meanwhile is
recognised, not reported.

## 18.2 Swarm testing

Each run draws a **profile**:

- which action kinds are enabled (each on with probability ½, at least one);
- a weight per enabled kind;
- a pane cap;
- the number of steps;
- per-kind fault rates (18.3);
- a program mix (18.4).

One run is all crashes and restarts, another a resize storm, another a flood of
input. The profile is a plain, printable value: it appears in the run's
telemetry and shape tags, and can be forced with `PANE_RUNTIME_SIM_PROFILE` (the
recipe that guided runs in 18.8 record).

## 18.3 Kernel faults

The pane runtime turns on the simulated kernel's faults, drawn per run by the
profile:

- interrupted and short reads and writes;
- spawn failures;
- process id reuse anywhere from 0 to 1.

New fault kinds in `switchy_pty` (MoosicBox), each a `Faults` rate drawn from
the run's stream:

| Fault | Effect |
|---|---|
| `kill` | An outside killer ("OOM killer") sends `SIGKILL` to a random running process. A simulated thread of the kernel injects it at random times. |
| `signal_delay` | Signals are queued and delivered after a random simulated delay, not immediately. |
| `read_error` | Reads from a terminal's master fail with `EIO`, as they do once a terminal is torn down. |
| `write_stall` | Writes to a master block for a random simulated time (a slow consumer). |
| `startup_query` | The Windows startup cursor query is sometimes never sent, or sent late, so the runtime's timeout path runs. |

An invariant that a fault legitimately breaks (a killed pane never answers what
was typed) is relaxed for that pane, not for the run. The runtime must still
notice the exit, clean up, and report an exit exactly once.

## 18.4 Programs that misbehave

Registered alongside the well-behaved shell and chosen by the profile's program
mix. Each is a simulated program with defined behaviour:

| Program | Behaviour |
|---|---|
| `daemonizer` | Double-forks a child into a new session (`setsid`) that outlives the pane. |
| `stubborn` | Ignores `SIGTERM` and `SIGHUP`. |
| `instant` | Exits at once, sometimes before writing anything, sometimes after a burst. |
| `flood` | Writes far more output than the pane drains, so backpressure kicks in. |
| `garbage` | Writes invalid UTF-8 and escape sequences split across writes. |
| `querier` | Sends terminal queries (cursor position, device attributes) and waits for answers. |
| `altscreen` | Switches to the alternate screen and back. |
| `orphaner` | Leaves children holding the terminal after it exits (where Linux and macOS differ). |

Each program defines what the runtime owes it. For example, `daemonizer`'s
escaped session is not the pane's, but the pane's own session must still end on
close.

## 18.5 A wider workload, and files

New actions through the simulation facade (`Panes`), each with its invariant:

- **attach and detach** with grid snapshots: a snapshot shows a state the
  program's output passed through;
- **splits and multi-pane sessions**: closing one pane leaves its siblings
  running;
- **program executions** with exit codes: the reported code is the program's;
- **signals to panes**: delivered to the pane's foreground program;
- **`sweep_on_exit = false`**: a job that ignores hang-ups survives the pane's
  close;
- **snapshots and resurrection**: a crashed runtime restores the panes it had
  persisted;
- **managed scopes and reservations**: a reservation is never handed out twice,
  and a released one is reusable.

### The filesystem

The pane runtime writes session records, atomic state, durable capture,
reservations and readiness files. Today these go to the host's real disk (a
real temporary directory per run). That is a determinism hole: paths, timing
and errors come from the host. Upstream `switchy_fs` now simulates a per-run
POSIX filesystem with durability, crashes and faults. What remains:

**switchy_fs (MoosicBox):**

1. Temporary directory names are drawn from the run's stream, and creation
   retries on collision. Today every one is named `abcdef`, so a second tempdir
   collides.
2. Temporary-directory bookkeeping (`TEMP_DIRS`) moves into the run's
   filesystem. Today it lives per OS thread, and every simulated thread is one.
3. Metadata times (modified, accessed, created) come from `switchy_time`.
4. An optional switch-point hook: file operations call a hook the simulation
   installs. The pane runtime simulation installs `switchy_thread`'s
   `yield_point`, so interleavings around file operations are explored. There is
   no hard dependency on `switchy_thread`.
5. A Windows personality for the filesystem:
   - an open file cannot be removed or renamed over (sharing violation);
   - names are case-insensitive;
   - no symbolic links without privilege.

   It is chosen with the kernel's personality, so a Windows run takes Windows
   file paths.

**pane-runtime-plugin (bmux):**

1. Every `std::fs` and `tempfile` use (about 150 sites) goes through
   `switchy_fs`. Host-only pieces (hashing the running executable, retaining it)
   get simulated stand-ins in `platform/simulated/`.
2. `std::fs` and `tempfile::` join the forbidden list in `tests/primitives.rs`.
3. The simulation uses the run's filesystem, with its fault plan drawn by the
   profile (`EIO`, `ENOSPC`, short writes).
4. A crash (18.3's server crash) becomes `CrashPolicy::process_crash()` for the
   filesystem.
5. A new **power loss** action crashes the filesystem with
   `CrashPolicy::power_loss()`, then recovers. Invariant: recovery never trusts
   a torn record, and every session it can prove it owned is swept.

## 18.6 Global invariants

Checked at quiescent points (after a close, restart, crash or recovery, and at
the end), across everything rather than per step:

- every terminal's master handle count is back to 0 once its pane is gone;
- no orphaned sessions, no leftover crash records, no stray simulated threads;
- every pane that started gets exactly one exit notice;
- what a pane's grid shows is consistent with what its program wrote,
  byte for byte, for the bytes the grid keeps;
- **metamorphic**: the same profile and actions run under Linux, macOS and
  Windows end the same at the pane level, except for documented differences.
  The simulation runs this as a separate mode (`--metamorphic`), with three
  runs per seed.

## 18.7 Scheduling and time

Drawn per run unless forced by the environment:

- the scheduling strategy: random, PCT (with a depth) or alternating;
- weak memory on or off;
- the clock's step multiplier (1, 10 or 100), so timeouts land at or past their
  deadlines in different ways.

`switchy_thread`'s run configuration already comes from the run's context
(`run_with`). The draw goes in the simulation's setup, and the choice is part of
the run's shape.

## 18.8 Coverage-guided runs

`--guided` keeps a corpus of profiles (18.2) that produced new **shapes** (a
new combination of shape tags, invariants exercised and distinct schedule). A
new run either draws a fresh profile from its seed, or takes a corpus entry and
mutates it with its seed (toggle a kind, scale a weight, swap a program, raise
a fault rate).

A guided run's recipe is its seed plus its profile, printed and saved in its
finding. `PANE_RUNTIME_SIM_PROFILE` replays it, and the dashboard's replay
passes it along, so a guided failure is as reproducible as any other.

## 18.10 Keep what's found

- Regression seed files for pane-runtime and local-terminal (the pty
  simulation has one), run by `--regression` and in CI. Every bug this phase
  finds adds its seed (and recipe, if guided).
- The nightly workflow runs the new modes: swarm and faults (by default),
  guided, metamorphic, and the pane runtime with power loss. It uploads the
  findings directory (`SIMULATOR_FINDINGS`) as an artifact.

## Order

1. Swarm profiles, concurrent drivers, scheduling and time variance (18.1, 18.2,
   18.7).
2. Kernel faults, then the new `switchy_pty` faults (18.3).
3. Global invariants (18.6).
4. Programs that misbehave (18.4).
5. switchy_fs gaps, then the plugin's file migration and power loss (18.5).
6. The wider workload (18.5).
7. Guided runs (18.8), then regression seeds and nightly CI (18.10).

Each step lands with its invariants, a fleet run under
`SIMULATOR_VERIFY_DETERMINISM=1`, and fixes for whatever it finds. Each bug
found is described in the implementation notes below.

## Implementation notes

(Filled in as the work lands.)
