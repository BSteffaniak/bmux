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

Status (2026-11-27): 18.1–18.4 and 18.6–18.10 landed. 18.5 landed in part; what is missing is
listed under "Not done yet".

### What landed

- **Profiles and drivers (18.1, 18.2).** `simulation/pane-runtime/src/profile.rs` draws each
  run's profile, and `workload.rs` runs 1–4 drivers on one runtime.
  - A crash cancels the drivers' actions under way, then kills the server.
  - `PANE_RUNTIME_SIM_PROFILE` forces a profile. Its words may be separated by spaces or
    commas.
- **The crash model.** The simulation facade runs its server on an executor of its own,
  driven from the run's executor. A crash abandons the sessions and calls `shutdown_now`:
  every task and thread of the server stops where it is, and its handles close as a dead
  process's would.
- **Kernel faults (18.3).** New in `switchy_pty`:
  - an outside killer, at a rate per simulated second;
  - late signals;
  - master reads failing with EIO;
  - stalled master writes;
  - Windows startup cursor queries that never come or come late.

  Every profile draws its fault rates.
- **Programs (18.4).** `programs.rs` adds a shell plus programs that ignore termination,
  detach a daemon (the new `Process::spawn_session`), orphan a child that holds the terminal,
  exit at once, flood output, write garbage, query the terminal, or switch screens.
- **Files (18.5).**
  - The plugin's file I/O goes through `switchy_fs`, and the primitives scan forbids host
    file I/O.
  - The run's filesystem takes the kernel's personality, every file operation is a
    scheduling switch point (`set_operation_hook`), and file faults are drawn per profile.
  - A crash also crashes the filesystem as a killed process leaves it.
  - The `power` action kills every process and keeps only what was made durable, then a new
    server recovers.
- **Wider workload (18.5, in part).** Splits (the facade now addresses each pane of a
  session), interrupts, shells told to `exit`, and `sweep_on_exit` off (`sweep=0`).
  - Observing clients (2026-11-27): the `observe` action (draw epoch 3, so earlier fields
    draw as before) takes a pane's frames as an unattached client does
    (`observe_pane_frames`' path, `frames_for_panes`) and applies them to a replica of its
    grid in the facade (`Panes::observe`), or, one time in four, stops observing, as a
    client detaching does (its server-side replica is forgotten, so the next frame is a
    reset). Invariant 15: a frame that does not apply, or a replica that, brought up to date
    as the pane closes, shows other than the pane's grid. A mutation that drops every frame
    after a replica's first is caught at once.
- **Invariants (18.6).**
  - 8: each program's exit is acted on at most once.
  - 9: nothing is left once every pane closed: no open master, no stray process, no session
    record.
  - 10: a pane whose program ended says so.
  - 11: closing a pane leaves its siblings alone.
  - 13: a pane's screen is what its input made it. With the `record` feature (which the
    plugin's `simulator` feature turns on), `bmux_terminal_grid`'s streams record the grid
    they started from and every chunk and resize they took in; `check_record` replays that
    into a fresh stream and compares the screens. Checked as each pane closes.
  - 14: a pane takes in what its terminal gave it, in order: the chunks its grid took in are
    consecutive parts of everything the terminal gave its master side (new in `switchy_pty`:
    `terminal_output`, which has the echoes `terminal_written` lacks), but for the runtime's
    own notices and, on Windows, the console's startup query the runtime answers.
  - 4 now also covers a pane whose program already exited: its session is the program's
    serial, not looked up among running processes. Jobs the runtime released (`sweep=0`, once
    the terminal reached end of file) are excused from 4, 6 and 9, as documented in
    `docs/pane-execution.md`; the plugin records each release for the simulation.
  - 5 now also fails an action that never finishes, naming the live threads.
- **Metamorphic runs (18.6).** `--metamorphic` runs each seed as Linux, macOS and Windows,
  each in a process of its own, with the profile calmed (`Profile::calm`: one driver, no
  faults, crashes, restarts, interrupts, power losses or bystanders, no programs that exit at
  once), and compares how many panes opened, resized, closed and failed to open, and whether
  every run passed. Lines typed are not compared: a pane takes input for as long as it serves
  its terminal, which outlasts its program longer on Windows. 300 seeds end alike.
- **Scheduling and clock (18.7).** `SIMULATOR_SCHEDULE_STRATEGY=swarm` and
  `SIMULATOR_WEAK_MEMORY=swarm` (switchy_thread) are every simulation's defaults. Profiles draw
  the clock rate (1, 10 or 100 ms per step).
- **Guided runs (18.8).** `--guided` (`guide.rs`) keeps profiles whose runs reached a new
  outcome, by personality and each action's count bucketed by powers of two. A guided run
  reports its profile as an environment-named simvar property, which goes in its replay
  command (new in simvar).

  On 5000 runs, guided fleets reached 2667–2840 distinct outcomes against 2134 unguided.
- **Regression seeds and CI (18.10).** `sim_support::regression` serves every simulation.
  Each line is `SEED [EPOCH] [NAME=VALUE ...] # note`. CI and the nightly run every
  simulation's seeds, and the nightly fleets continue past failures, upload their findings,
  and include a guided variant.

### Bugs found

In bmux:

1. **Restart reported the live pane exited.** Restarting a pane let the replaced program's
   exit notice name the pane, so the runtime published `PaneEvent::Exited` for the running
   pane, on every restart.
2. **A program exiting before adoption left its session behind.** When a pane's program
   exited before its session was adopted, its children outlived the pane. On Linux, with the
   terminal still held, opening the pane hung; macOS natively has the leak.
3. **A stopped pane could release jobs it should end.** With `sweep_on_exit` off, a stopped
   pane could release the jobs it was meant to end; a pane refused before its commit always
   did.

In MoosicBox:

4. **Epoch-offset replays drew one number too few.** A replay with `SIMULATOR_EPOCH_OFFSET`
   skipped a generator draw, so replays of multi-task runs took different paths (switchy_time).
5. **`block_on` slept through its own future's wake-up.** The simulator's `block_on` polled
   its future with a no-op waker (switchy_async).
6. **Spawning didn't wake the executor.** A spawned task never ran while the executor waited
   for work (switchy_async).
7. **Spawning across runtimes panicked.** Spawning onto one simulator runtime from another
   runtime's task panicked (switchy_async).
8. **tokio's `watch` woke waiters in no seeded order.** switchy_async's simulator now has its
   own deterministic watch, and the plugin uses it (the scan forbids tokio's).
9. **Killed threads parked for good.** A thread killed while its destructor waited on a lock
   held by another victim, or slept, was parked forever, keeping what it held (switchy_thread).
10. **The simulated console mishandled early typing.** It echoed typing before its cursor
    report, and typed withheld bytes without the line discipline (switchy_pty).
11. **Closing a simulated console spared processes ignoring hang-ups.** Real Windows ends
    every attached process (`CTRL_CLOSE_EVENT` cannot be ignored); the simulator let them run
    (switchy_pty).
12. **simvar_harness's own tests no longer compiled.**

In the simulation itself:

- Windows jobs now start with the spawn. This is held by the `Preparation`, as natively.

### Fixed since (2026-11-27)

- **A Windows close hang with `sweep_on_exit` off.** A pane whose shell exited while leaving
  a job attached to the console could fail to close: its capture threads waited for console
  output that only ends once the console closes, and the console closed only after its owner
  retired. Two bugs, both behind `sweep=0`:
  - Supervision ended as soon as the leader's waiter thread finished (its exit channel closed
    right after reporting the exit), so the pane stopped serving the terminal its survivors
    still held; on Windows nothing then closed the console. The pane now keeps serving until
    they release the terminal or it is stopped.
  - The survivors left their Windows job (kill-on-close cleared) the moment the leader exited,
    so a server crash no longer ended them, and Windows keeps no records for recovery to sweep.
    They now stay in the job until the pane lets them go, as Unix's recovery sweep ends them.

  Profiles draw `sweep=0` again (one run in four); both failures are regression seeds.

- **A Windows console closed under the lock callers take (bmux_pty).** The first nightly
  fleets failed 1–2 runs in 5000 with drivers that never finished. When a process exited,
  `bmux_pty`'s exit thread dropped the console while holding the control's lock; closing a
  console waits until its output is read, and with a full event queue the reader waits for
  the caller, who was waiting for the lock to resize. The console is now closed outside the
  lock, and callers use it under the lock without keeping a clone.
- **One failed read of a crash record left a dead server's session running.** Recovery kept a
  record it could not read for a later activation; with file faults, the crashed server's
  jobs ran on until then. Recovery now tries a record three times first.
- **A crash could leave a pane's terminal open (macOS, about 1 run in 3000): the simulated
  crash's mistake.** A crash killed a starting pane's waiter thread just as it released the
  lock on its work, after taking it. A killed thread unwinds from its switch point, here
  inside the guard's destructor, and Rust then leaks the value the enclosing function was
  returning (rust-lang/rust#47949): the waiter's work, holding the pane's execution and its
  terminal. A dead process's handles do not depend on its destructors, so a crash now closes
  every terminal the dead server still held, as the operating system does
  (`switchy_pty::simulator::close_masters`, which now also closes a terminal that already
  hung up, as a macOS terminal does when its leader exits). Found by holding each execution
  in a tracking pointer recording where it was cloned; a regression seed.
- **The simulation's own mistakes**, found by the metamorphic runs and the new invariants:
  on macOS the hung-up terminal releases a shell's leftover jobs at once (documented), which
  invariant 9 counted as leaks; and invariant 4 skipped panes whose program had already
  exited.

### Not done yet

- **Wider workload:** attaching proper (an attached client's frames and its attach snapshot,
  where observing covers the frames), program executions with exit codes, snapshots and
  resurrection, managed scopes and reservations.
