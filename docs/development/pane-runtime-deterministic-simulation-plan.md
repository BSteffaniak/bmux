# Plan: deterministic simulation of the pane runtime

Phase 15 of the [bmux_pty simulation plan](pty-deterministic-simulation-plan.md):
take deterministic simulation testing from `bmux_pty` up to the code that runs
panes. This plan covers the shared async/thread runtime that has to come
first, then the pane runtime itself.

## Goal

A simvar simulation of the pane runtime: panes are spawned, typed into,
resized, restarted, closed and killed on the simulated kernel, from simulated
threads and async tasks scheduled by one seed. A failure then replays exactly,
shrinks, and can be stepped through in the dashboard, like `cargo sim pty`.

## Where things stand

The survey this plan is based on found:

- **`bmux_pty`** is fully simulatable (its `simulator` feature), but nothing
  above it uses that feature. Its two consumers pin `features = ["native"]`.
- **The pane runtime** (`plugins/pane-runtime-plugin`, about 34k lines, 24k of
  them in `runtime.rs`) uses none of the switchy crates:
  - tokio: `spawn` 21×, `spawn_blocking` 7×, `select!`, mpsc/oneshot/watch,
    about 97 `tokio::time` uses, `block_on` inside `std::thread::scope`
  - std threads: the reader and waiter threads per pane, process-identity
    probes, input and capture helpers
  - std sync primitives and about 109 atomics
  - `Instant` (about 123 uses) and `SystemTime`
  - `Uuid::new_v4` for pane and session ids
  - `std::fs`: state, snapshots, crash records, `/proc` scans
  - libc: `waitid`, `getsid`, `kill`, `proc_pidinfo`; Job Objects on Windows
- **Seams already exist:**
  - `runtime/pty.rs` puts `Child`, `ChildKiller` and `Slave` traits over
    `bmux_pty`, and tests already fake a `Child`.
  - `PaneRuntimeHost::detached()` builds a host without an event bus.
  - The plugin is driven through `invoke_service` and reports through the
    `EventBus`.
- **The bare terminal** (`packages/cli/src/runtime/bare_terminal.rs`, 229
  lines) is a single shell over a local socket, with no splits. It uses tokio
  net, io, time and signal, and is Unix only.
- **`switchy_async`'s simulator** runs `spawn_blocking` closures as ordinary
  tasks on its one executor thread. When idle it busy-waits with
  `std::thread::yield_now()`. Under the baton scheduler, a blocking closure or
  a busy wait would starve every simulated thread, so the async and thread
  worlds cannot meet yet.

## Workstream A: one runtime for tasks and threads (MoosicBox)

### A1. The executor runs as a simulated thread

The executor's thread is the run's main simulated thread (as simvar's client
already is). The idle path changes:

- **Idle:** when no task is ready, the executor blocks on the scheduler with a
  `WaitKey` of its own, with its earliest timer as the deadline, instead of
  `yield_now`. Simulated threads then run.
- **Waking:** a waker called from a simulated thread (a oneshot or channel
  send, for example) marks the task ready and wakes the executor's key, so the
  executor resumes at a scheduler decision, by seed.
- **One clock:** async timers and thread deadlines both use `switchy_time`'s
  run clock. When everything is blocked, the scheduler advances the clock to
  the earliest deadline of either kind (it already does this for threads; the
  executor's deadline joins as an ordinary blocked-thread deadline).

### A2. `spawn_blocking` runs on a simulated thread

With the `thread` simulator, `spawn_blocking` starts a `switchy_thread`
thread. Its `JoinHandle` future completes through the waking path above.
Blocking inside a closure then blocks only that simulated thread.

### A3. Tests

- A task awaiting `spawn_blocking` work that waits on a simulated channel fed
  by another task: no deadlock, any seed.
- A timer and a thread sleep with interleaved deadlines fire in clock order.
- Run-twice trace equality over many seeds.
- Existing `switchy_async` simulator tests unchanged.

## Workstream B: the pane runtime on switchy (bmux)

The migration is mechanical but large, so it goes file by file behind the
existing behaviour, with no `simulator` feature enabled until the end.

### B1. Primitives

| Today | Becomes |
| --- | --- |
| `tokio::{spawn, select!, sync, time}` | `switchy_async` equivalents |
| `std::thread`, `std::sync`, atomics | `switchy_thread` |
| `Instant`, `SystemTime` | `switchy_time` |
| `Uuid::new_v4` | ids drawn from the run's seed (a small id helper) |
| `std::fs` | `switchy_fs` (its simulator is an in-memory filesystem) |
| `std::env` reads | read once at activation into configuration |

The `primitives.rs` source scan that guards `bmux_pty` is extended to the
plugin, exempting code marked `// native-only`.

### B2. OS process control behind a trait

`session_control` and `native_process` (`getsid`, `kill`, `waitid` with
`WNOWAIT`, `proc_pidinfo`, `/proc` scans, Job Objects) move behind a
`ProcessControl` trait with a native implementation (today's code) and a
simulated one on `switchy_pty`'s kernel. The simulated kernel already models
sessions, process groups, signals, zombies and pid reuse. It needs a few more
oracle-style calls: session of a pid, a process's start identity (its serial),
and its working directory.

### B3. A host to run it in

A simulation host builds `PaneRuntimeHost::detached()` with an in-memory
`PluginStateRegistry` and a recording `EventBus`, and drives the plugin through
`invoke_service` with typed requests, as the server would.

### B4. The simulation

A new member, `simulation/pane-runtime`, run as `cargo sim pane-runtime`.
Drivers spawn panes running simulated programs, then:

- type into them, resize, restart and close them
- kill them, stop the server mid-operation, and restart it to exercise crash
  recovery and the session sweep

Invariants:

1. Every spawned pane ends exactly once.
2. Output that reached the pane's grid is a prefix of what the program wrote.
3. Input is delivered in order, and admitted or fenced input is never lost or
   duplicated.
4. A closed or killed pane leaves no process of its session running.
5. A sweep after a crash ends exactly the sessions the crashed server owned,
   never a reused pid.
6. Resizes reach the program, the last one last.
7. No deadlock, leaked thread or task, or livelock.

The dashboard gets a panel for panes (state, grid via `bmux_terminal_grid`,
session) next to the existing PTY panel.

### B5. The bare terminal

The bare terminal's server half (`serve_attachment`) is generic over its
stream, so the simulation can drive it over an in-memory duplex and check
detach and reattach and its framing. The client half (tty, signals) stays
native.

## Phases

| # | Phase | Repo | Acceptance |
| --- | --- | --- | --- |
| 15.1 | Executor idles and wakes through the scheduler; one clock (A1) | MoosicBox | A3 tests; existing simulator tests pass |
| 15.2 | `spawn_blocking` on simulated threads (A2) | MoosicBox | A3 tests |
| 15.3 | Pane runtime on switchy primitives (B1), no behaviour change | bmux | the plugin's ~478 tests pass natively on 3 OSes |
| 15.4 | `ProcessControl` trait and its simulated implementation (B2) | both | native tests pass; simulated unit tests of sweep and termination |
| 15.5 | Simulation host and `cargo sim pane-runtime` (B3, B4) | bmux | thousands of seeds clean, or each failure fixed and pinned as a regression seed |
| 15.6 | Bare terminal server over a duplex stream (B5) | bmux | simulated attach/detach scenarios |
| 15.7 | Dashboard panes panel | bmux | panes render live |

15.1–15.2 and 15.3 can proceed in parallel. 15.3 is a refactor with value of its
own (testable time and ids). It is the largest phase, so split it by file:
`execution.rs`, `reservations.rs`, then `runtime.rs` in sections.

## Risks

- **Size.** `runtime.rs` is 24k lines. Mitigation: migrate in small, behaviour-preserving
  commits, each validated by the existing native suite.
- **`block_on` inside threads.** `execution.rs` nests a runtime inside
  `std::thread::scope`. These need restructuring into tasks plus
  `spawn_blocking`, or a per-thread executor that also idles through the
  scheduler.
- **Ids in persisted state.** Seeded ids must not collide across server
  restarts in one run. Draw them from a run-scoped stream, not reset per
  activation.
- **Windows.** Job Objects stay native-only. The simulated `ProcessControl`
  models Unix sessions, as the simulated kernel does.

## Implementation notes

Phases 15.1–15.5 as built (2026-11-21):

- **15.1–15.2 (MoosicBox):** `switchy_async`'s `thread-simulator` feature joins
  its simulator executor to `switchy_thread`'s scheduler: `spawn_blocking` work
  runs on a simulated thread started when the executor reaches it, an idle
  executor blocks on the scheduler (key `WaitKey::Executor`) instead of
  spinning, wakers from any simulated thread wake it, and simvar lets live
  simulated threads run at the end of every step. The simulator also gained
  what the pane runtime leans on: `time::{Instant, sleep_until, timeout_at}`,
  `task::block_in_place`, `RuntimeFlavor`, a nested `Handle::block_on`,
  `Handle::enter`, `select!` preconditions, `is_finished(&self)`, and
  `#[switchy_async::test(flavor = ..., worker_threads = ...)]`.
- **15.3 (bmux):** the pane runtime plugin runs its tasks, timers, `select!`,
  runtime handles and tests through `switchy_async`; threads, locks and
  atomics through `switchy_thread`; clocks through `switchy_time`; identifiers
  through its `ids` module (seeded in simulation); directory listings in file
  name order; and process-wide bounds through `run_shared` (one per simulated
  run). tokio remains only for its runtime-free sync primitives, and std's
  locks where values sit in core's state registry. Files stay on `std::fs`: a
  run's files are its own, listed in a fixed order (moving to `switchy_fs` is
  a follow-up).
- **15.4 (bmux):** with the plugin's `simulator` feature, `session_control` and
  `native_process` have simulated backends on `switchy_pty`'s kernel (which
  gained `signal_process` and process working directories); the crash guard
  and the `/proc` sweep are native only.
- **15.5 (bmux):** `simulation/pane-runtime` drives the plugin through its
  `simulation` facade (`Panes`); see its README for the invariants. A
  mutation that drops some typed input is caught at once (invariant 2).
- **Found:** the simulator's `select!` chose ready branches with a per-thread
  generator outside the simulation, so replays diverged on reused worker
  threads (fixed in `switchy_async`: rotations drawn from the run's seed); a
  pane identifier generated by a path the first migration missed; and
  process-wide counters that let concurrent runs influence each other.
- **15.6 (bmux):** the bare terminal's server half moved into
  `bmux_local_terminal` (generic over its stream, on `switchy_async`), which
  the CLI serves on its socket and `simulation/local-terminal` serves over
  in-memory duplex streams through attach, detach and reattach.
- **15.7 (bmux):** what the pty simulation's dashboard support was became
  `simulation/support` (`sim_support`), shared by the simulations;
  `cargo sim pane-runtime --tui` shows the scheduler, the invariants, a Panes
  tab and the kernel's processes, signals and terminal screens, with pausing,
  stepping, going back and breaking on failures.
- **Not yet:** crash recovery and restart in the simulation (a detached host
  writes no crash records), regression seeds for the new simulations, and
  files on `switchy_fs`.
