# Plan: deterministic simulation testing for `bmux_pty`

Status: proposed (2026-11-21). The decisions are settled (see "Decisions").
Spans two repositories:

- MoosicBox (`../MoosicBox`): new and changed `switchy_*` crates, and `simvar`
- bmux: the `bmux_pty` port, its simulator, and a simulation TUI built on
  bmux's TUI crates

## Goal

Run `bmux_pty`, unchanged in behaviour and public API, inside a fully
deterministic simulation. One seed decides everything: thread interleavings,
process behaviour, OS faults and timing. Any failure reproduces exactly from the
seed, and a randomised workload checks its invariants on every run.

This is the first fully self-contained deterministic simulation in bmux. It also
proves out the building blocks (`switchy_thread`, `switchy_pty`, the shared
simulation context) for simulating the pane runtime, IPC and the server later.

## Non-goals

- Simulating the pane runtime, server or client. That comes later, using the
  same building blocks.
- Simulating Windows-only code paths (`conpty.rs`, `pair/windows.rs`). Code
  behind `cfg(windows)` does not compile on a Unix host, so the Windows backend
  stays real-only for now.
- Changing `bmux_pty`'s public API or making it async.
- Full canonical-mode line editing in the simulated terminal (see "Terminal
  fidelity").

## Principles

1. **Ship the code you test.** The simulator runs `bmux_pty`'s real logic. Only
   the primitives under it are swapped: threads, sync, time and the OS layer.
2. **Backends are chosen at compile time**, the same way switchy does it: a
   `simulator` feature selects the simulated backend and `native` selects the
   real one. No runtime switches.
3. **No external dependencies in a simulation build.** A `bmux_pty` simulator
   build is std plus our own crates. `libc` and `windows-sys` belong only to the
   native backend.
4. **Every source of non-determinism goes through one seeded context.**
   Scheduling, randomness, time, pids and faults.
5. **Determinism is verified, not assumed.** Every seed can be run twice and its
   trace hashes compared.
6. **Simulator fidelity is verified, not assumed.** One conformance suite runs
   against both the native and simulated backends.

## Architecture

```
            bmux (this repo)                         MoosicBox
 ┌────────────────────────────────────┐
 │ packages/pty/simulator  (bin)      │──uses──▶ simvar (harness: seeds, runs,
 │   workload + invariants + oracle   │           reproduction, trace hash)
 └──────────────┬─────────────────────┘
                │
 ┌──────────────▼─────────────────────┐
 │ bmux_pty  (logic: PtyProcess,      │──uses──▶ switchy_thread  (spawn, sleep,
 │   events, drain grace, writer,     │           Mutex, Condvar, mpsc, atomics;
 │   kill-on-drop, PtyPair wrappers)  │           baton scheduler in simulator)
 │                                    │──uses──▶ switchy_time    (Instant)
 │                                    │──uses──▶ switchy_pty     (OS layer)
 └────────────────────────────────────┘             ├─ native: openpty, poll,
                                                    │  kill, ConPTY (moved from
                                                    │  bmux_pty; libc/windows-sys)
                                                    └─ simulator: process table,
                                                       terminal device, fake
                                                       programs, faults
          all simulators share one per-run SimContext (seed, RNG, clock, pids)
```

## Workstream A: MoosicBox (switchy and simvar)

### A1. Shared per-run simulation context

**Why:** simulated threads are real OS threads (see A3), but switchy's simulator
state (RNG, clock step, epoch offset, and so on) lives in `thread_local!`s on the
simvar run thread. A simulated thread would see none of it.

- Add a `SimContext` held behind an `Arc`. It holds the seed, the RNG, the
  clock (step, multiplier, epoch offset) and slots for other crates' per-run
  state (scheduler, PTY kernel).
- Each thread-local becomes a pointer to the current `SimContext`.
  `switchy_thread::spawn` installs the parent's context in the child.
- The baton (A3) guarantees that only one thread touches the context at a time.
  Its locks are therefore never contended, and access order stays
  deterministic.
- Behaviour for existing simvar users (MoosicBox server and hyperchad
  simulators) must not change. Run their simulations before and after with the
  same seeds and compare.

### A2. Dependency-free core simulator crates

The crates a `bmux_pty` simulation build pulls in must have no external
dependencies:

- **`switchy_random`:** replace `rand_08`'s `SmallRng` with an in-house PRNG.
  ChaCha8 or xoshiro256** takes about 50 to 150 lines. This also fixes
  portability: `SmallRng` is documented as not portable across platforms or
  versions, so seeds would not reproduce elsewhere. Keep the `rand` integration
  behind an optional feature for existing users.
- **`switchy_time`:** replace `scoped-tls` with std `thread_local!` plus `Cell`,
  make `log` optional, and keep `chrono` optional.
- **`switchy_uuid`:** seed from `SIMULATOR_SEED` and reset per run, like the
  rest of switchy. `bmux_pty` doesn't need this, but bmux will.

### A3. New crate `switchy_thread`

API that mirrors std, under `switchy_thread::{thread, sync}`:

- `thread::{spawn, Builder (name), JoinHandle, sleep, yield_now, park,
  current}`
- `sync::{Mutex, RwLock, Condvar, Barrier, mpsc::{channel, sync_channel}}`,
  covering `recv`, `try_recv`, `recv_timeout`, `send` and `try_send`
- `sync::atomic::{AtomicBool, AtomicUsize, AtomicU64, …, fence}`

**Native backend:** std re-exports, with no overhead.

**Simulator backend: the baton scheduler.**

- Each simulated thread is a real OS thread that runs only while it holds the
  baton. A new thread registers and parks before running any user code.
- **Switch points:** every blocking operation, plus every `try_*`, lock and
  unlock, channel operation and atomic access. At each one, the scheduler picks
  the next thread to run using the seeded strategy.
- **Scheduling strategies**:
  - uniform random
  - PCT (random priorities plus d priority-change points), which reliably finds
    bugs that need a specific short sequence of context switches

  Decided: CI alternates the two by seed parity. Configuration can pin
  either one.
- **Time:** a timed wait (`sleep`, `recv_timeout`, `wait_timeout`) parks the
  thread until the simulated clock reaches its deadline.
  `run_until_idle()` runs threads until all of them are blocked or waiting on a
  timer at the current time. The harness then advances the clock. When the only
  pending work is timers, the harness may jump straight to the next deadline.
- **Lifecycle:**
  - OS threads never exit during a run. A finished simulated thread parks until
    teardown. This keeps `thread_local!` destructors from racing the next
    thread.
  - At teardown, every thread is unwound with a cancellation panic at its
    switch point, then joined.
  - Each simulated thread runs inside `catch_unwind`, so a panic still hands the
    baton on.
- **Diagnostics:**
  - **Deadlock:** every thread blocked with no timer pending. Panics with each
    thread's name, the operation it is blocked on, and its backtrace.
  - **Leak:** at teardown, any thread the workload expected to finish is still
    alive.
  - **Watchdog:** a thread holds the baton for more than N seconds of real time.
    This means it blocked outside switchy, for example on a raw std primitive.
- **Trace:** every scheduling decision (thread, operation kind, simulated time)
  is fed into a running hash. Optionally the full decision list is recorded so
  a schedule can be replayed (and later shrunk) directly.
- **Weak-memory mode** (opt-in, later phase): each atomic location keeps a
  history of its stores and each thread a vector clock. A `Relaxed` or `Acquire`
  load returns a seeded choice among the stores the memory model allows. Fences
  are modelled and `SeqCst` is treated conservatively. This is the loom
  technique, run deterministically.
- **Integration with switchy_async's simulator:** its `spawn_blocking` becomes a
  `switchy_thread` spawn instead of running the closure inline, so a blocking
  closure no longer hangs the run. Async tasks and simulated threads then share
  one seeded interleaving. Not needed for `bmux_pty`, but it unblocks
  simulating bmux's mixed code.

### A4. New crate `switchy_pty`

This crate owns the OS layer of pseudo-terminals and the processes running on
them, with both the native and the simulated backend. Decided: the native code
moves here out of `bmux_pty`.

**API**, extracted from today's `bmux_pty` `pair` API plus what
`lib.rs::platform` needs:

- `PtyPair::open(size)`
- `PtyMaster`: `try_clone_reader`, `take_writer`, `resize`, `size`, `tty_name`,
  `foreground_process_group`
- readiness-aware reads and writes with a poll timeout (what `Polled` and
  `write_all` do today)
- `PtySlave::spawn(&SpawnCommand)` with stdio routing for each stream: terminal,
  pipe or null. This expresses `capture_stdout` and `input`.
- `PtyChild`: `id`, `try_wait`, `wait`, `kill`, `killer`
- `signal_group(pgid, signal)` and `hang_up(pid)`
- `SpawnCommand` with its environment capture, plus a `base_environment()`
  source. The real version uses the process environment and `getpwuid`; the
  simulated version uses a simulated user record.

**Native backend:** `pair/unix.rs`, `pair/windows.rs`, `conpty.rs` and the
`libc` helpers from `lib.rs::platform` move here unchanged. `libc` and
`windows-sys` become optional dependencies behind `native`, each specific to its
target as today.

**Simulator backend: an in-memory kernel model.**

- **Process table:**
  - seeded pid allocator, with an aggressive-reuse mode
  - sessions, process groups and controlling terminals
  - parent/child links, zombies and reaping, and exit status (code or signal)
  - which processes hold each terminal end and pipe open
- **Terminal device:**
  - two bounded byte queues; writes block when the queue is full. Capacity is
    set by the personality: about 1 KiB on macOS, about 4 KiB on Linux.
  - window size, with SIGWINCH sent to the foreground group on change
  - SIGHUP to the session when the master closes
  - EIO on master reads once no process holds the slave end
- **Line discipline subset:** ONLCR, ICRNL, ECHO, ISIG (Ctrl-C to SIGINT,
  Ctrl-Z to SIGTSTP, Ctrl-\ to SIGQUIT, sent to the foreground group) and the
  end-of-file character. Raw versus cooked is a single flag.
- **Pipes:** bounded byte queues with the same holder accounting, used for
  `capture_stdout` and `input`.
- **Signals:** each process has a disposition per signal (default, ignore or
  handle). Delivery can be delayed by the fault layer. The default action is
  terminate.
- **Personalities:**
  - Linux: the terminal stays open while anything that inherited it is alive.
  - macOS: the terminal ends with the session.

  This is the platform difference documented in `bmux_pty`'s README. Each run
  picks a personality from its seed, or it is pinned by configuration.
- **Fake programs:** a program is a closure run on a `switchy_thread` simulated
  thread, with a `ProcessCtx`:

  ```rust
  pub trait ProcessCtx {
      fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;   // blocks
      fn write(&mut self, bytes: &[u8]) -> io::Result<usize>;    // may block (backpressure)
      fn on_signal(&mut self, sig: Signal, action: Disposition);
      fn spawn(&mut self, argv: &[&str], opts: ChildOpts) -> io::Result<Pid>;
      fn sleep(&mut self, d: Duration);
      fn winsize(&self) -> PtySize;
      fn exit(self, code: i32) -> !;
  }
  ```

  Programs are found by `argv[0]` in a registry that the simulation sets up.
  `PATH` lookup and working-directory checks go through `switchy_fs` and
  `switchy_env`.
- **Fault layer** (seeded rates, all configurable):
  - EINTR and EAGAIN on read, write and poll
  - short reads and writes
  - `openpty` failure
  - spawn failure (ENOENT, EMFILE, EAGAIN)
  - ESRCH races, where a process exits between a check and a kill
  - delayed signal delivery
  - pid reuse immediately after a reap
- **Oracle API**, for invariant checks rather than the code under test:
  - the bytes each program wrote and read, in order
  - the processes alive in each session
  - the holders of each terminal and pipe
  - the signals each process received
  - live simulated threads
- **`reset()`** is called by simvar at the start of every run.

**Terminal fidelity** (decided: the core subset): the subset above. Canonical-mode
line editing (ICANON with erase and kill, VMIN/VTIME) comes later, only if a test
needs it.

**Snapshot API for the TUI** (used by workstream C): `switchy_pty` exposes a
read-only `KernelSnapshot` built from the oracle. It contains:

- processes, sessions and process groups
- terminals, with their size and holders
- pipes and their holders
- recent signals
- a log of the bytes written to each terminal

`switchy_thread` exposes a `SchedulerSnapshot`:

- each thread's name and state: running, blocked on what, or waiting on a timer
  and its deadline
- pending timers
- a ring buffer of the most recent scheduling decisions
- the running trace hash

Taking a snapshot only reads state. It never draws from the RNG and never
creates a switch point, so observing a run cannot change it.

### A5. simvar integration

- Features `thread` and `pty`, wired through the umbrella `switchy` crate and
  simvar's harness, with reset hooks in `harness/src/lib.rs` `run()`.
- **Bridge:** a host or client tick calls
  `switchy_thread::simulator::run_until_idle()` at the current simulated time.
- **Determinism check:** `SIMULATOR_VERIFY_DETERMINISM=1` runs each seed twice
  and fails on any trace-hash mismatch, reporting the first decision where the
  two runs diverge.
- **Process per seed for replay:** a mode that runs a single seed in a fresh
  process. This removes state shared across runs (statics, parallel runs) when
  reproducing a failure.
- **Reproduction line:** add `SIMULATOR_SCHEDULE_STRATEGY` and the personality
  to the printed `SIMULATOR_SEED=…` command, and record the commit SHA with the
  failure.

### A6. Observer interface in simvar (for workstream C)

Today simvar's display is compiled into `simvar_harness`: ratatui code in
`harness/src/tui.rs`, behind the default `tui` feature, with direct
`DisplayState` calls in the run loop. Replace that with a pluggable observer.

- **New crate `simvar_observer`** with no switchy or simvar dependency, only
  std. It holds:
  - the `SimObserver` trait
  - the event and snapshot data model: run started, step, run finished,
    failure (with its reproduction line), log record, and `Custom(T)`
  - control types for replay (C4)

  Because it has no switchy dependency, bmux's main workspace can depend on it
  without pulling switchy's `simulator` features into the real bmux build.
- **Typed domain data:** `SimBootstrap` gets an associated
  `type Telemetry: Send + 'static` (default `()`), and `Sim` gets
  `emit(Telemetry)`. The PTY simulator emits the kernel and scheduler snapshots
  as its telemetry.
- **Delivery:**
  - Run threads send to the observer over a bounded channel and never block.
  - Steps and snapshots are coalesced to the latest value.
  - Run start, run end, failures and logs are delivered reliably.
  - The observer runs on its own thread, in real time.
- **Determinism rule:** observing must never change a run. Emitting, taking
  snapshots and pausing only read state, and only at step boundaries. They never
  draw from the RNG and never create switch points. Phase T1 checks this: a run
  with and without an observer must produce the same trace hash.
- **Snapshot cadence:** every N steps, or when the observer asks via an atomic
  flag. The request only decides when a read-only snapshot is taken, never what
  happens in the run.
- **Replay control:** a single-run mode in which the harness checks an
  observer-owned controller at each step boundary or scheduling decision:
  - pause, resume, step one decision, step one simulated-time step
  - run to decision K, simulated time T, or a breakpoint predicate

  Pausing only stops real time, so the run continues identically.
- The ratatui TUI becomes `simvar`'s default `RatatuiObserver` (still the `tui`
  feature) and looks the same as today. A headless observer (`log` lines)
  covers CI.

## Workstream B: bmux

### B1. Port `bmux_pty` onto the switchy primitives

The public API does not change. All changes are inside the crate:

| Today | Becomes |
| --- | --- |
| `std::thread::spawn` in `feed_input`, `Events::read`, the writer in `platform::Control::writer`, `report_exit` | `switchy_thread::thread::Builder::new().name(…).spawn` (names show up in traces) |
| `std::sync::mpsc` (`sync_channel`, `channel`, `recv_timeout`) | `switchy_thread::sync::mpsc` |
| `std::sync::Mutex` | `switchy_thread::sync::Mutex` |
| `AtomicBool` (`closed`, `exited`, `took_writer`) | `switchy_thread::sync::atomic::AtomicBool` |
| `Instant::now()` in `report_exit` | `switchy_time::instant_now()` |
| `thread::sleep(HANGUP_GRACE)` and the sleep in the pipes fallback | `switchy_thread::thread::sleep` |
| `libc` read, write, poll and fcntl in `Polled` and `write_all` | `switchy_pty` readiness-aware master I/O |
| `std::process::Command` with `pre_exec` (`setsid`, `TIOCSCTTY`, signal reset, closing inherited descriptors) | `switchy_pty::PtySlave::spawn` with stdio routing |
| `kill(-pgid)` and `hang_up` | `switchy_pty::signal_group` and `switchy_pty::hang_up` |
| `pair` module | thin wrappers over `switchy_pty` that keep today's types and docs |

Cargo features:

```toml
[features]
default   = ["native"]
native    = ["switchy_pty/native", "switchy_thread/native"]
simulator = ["switchy_pty/simulator", "switchy_thread/simulator", "switchy_time/simulator"]
```

Lint: in `bmux_pty`, add `clippy.toml` `disallowed_methods` and
`disallowed_types` for `std::thread::spawn`, `std::thread::sleep`,
`std::sync::{mpsc, Mutex, RwLock, Condvar}`, `std::sync::atomic::*`,
`std::time::Instant::now`, `std::process::Command` and
`std::collections::{HashMap, HashSet}`.

### B2. Conformance suite: real and simulated backends must agree

- Rewrite `packages/pty/tests/{process,pair}.rs` (31 tests today) against a
  small `Program` description, for example `Program::emit("hi").exit(3)` or
  `Program::ignore(SIGHUP).spawn_bg(Program::hold_tty())`.
  - **Native:** the description compiles to an `sh` script, as the tests use
    today.
  - **Simulator:** it compiles to a fake program.
- Every test runs against both backends: the normal `cargo nextest` run for
  native, and the simulator build for simulated.
- A difference is either a simulator bug or a documented personality
  difference, marked per test. This is what keeps the simulator honest.

### B3. `packages/pty/simulator`: the simvar binary

**Workspace placement:** this crate must **not** be a member of the main
workspace. Cargo would merge its `simulator` features into the real `bmux_pty`
during `cargo nextest run`. Make it its own workspace, excluded from the root
one, run through `cargo run --manifest-path packages/pty/simulator/Cargo.toml`
and a `scripts/sim-pty.sh` wrapper.
`scripts/check-workspace-dependencies.sh` needs to know about it.

**Setup:** one simvar host runs a driver simulated thread, plus extra driver
threads for concurrent control (`PtyControl` and `PtyWriter` from other
threads). Programs are registered from the fake-program library.

**Fake-program library:**

| Program | Exercises |
| --- | --- |
| `cat` / echo | input → output ordering |
| `flood(n, chunk)` | bounded queue, backpressure, `ready_events` limits |
| `never_reads` | writer backpressure; dropping with blocked writes |
| `ignores(SIGHUP)` | hang-up then SIGKILL escalation |
| `daemonize(child_holds_tty)` | drain grace, signals after the leader exits, personalities |
| `shell_like` | background jobs, process groups, interrupt reaching everything |
| `query_responder` | terminal query/reply through the writer |
| `exit_now(code)` / `exit_after(d)` / `killed_by(sig)` | exit reporting, signal names |
| `resize_reporter` | SIGWINCH and the last size observed |

**Randomised workload actions** (weights from the seed):

- spawn, with random:
  - program
  - size
  - `capture_stdout`
  - `input`
  - `drain_grace`, including 0
- consume with `next_event`, `try_event`, `ready_events(limit)` or
  `next_event_timeout(random)`
- stall the consumer for a random simulated duration
- write random sizes, including larger than the queue
- `resize`, `interrupt`, `kill`, `stop_output`
- drop the process, including early or mid-write
- act from a second driver thread through a cloned `PtyControl` or `PtyWriter`
- pair level:
  - open a pair
  - spawn several processes on one slave
  - drop the slave and master in random order
  - `take_writer` twice
  - kill through `killer` from another thread
  - drop the writer, which types a newline and the end-of-file character

**Invariants**, checked against the oracle after every action and at the end of
the run:

1. Exactly one `Exited` per process, always last. Nothing is delivered after
   it, and `next_event` returns `None` afterwards.
2. If `output_complete`, the delivered `Output`/`Stdout` bytes equal the bytes
   programs wrote, in order. Otherwise they are a prefix of them.
3. Never more than `QUEUE` events buffered. A stalled consumer blocks the
   program, and memory stays bounded.
4. After `kill()` or a drop, every process in the session is dead within the
   hang-up grace plus some slack. That includes SIGHUP-ignoring programs and
   children that outlive their parent.
5. Signals never reach a process outside the session, even with aggressive pid
   reuse.
6. Writer bytes reach the slave in order. `BrokenPipe` happens only after the
   terminal closed. A program that never reads does not deadlock a drop.
7. A program observes the last resize, and a SIGWINCH was sent after it.
8. `Exited` arrives no later than exit + `drain_grace` + `POLL` + slack in
   simulated time, and no later than `POLL` + slack after `stop_output` once
   the process has exited.
9. `exit_status()` on `PtyControl` is set once the process is reaped and
   before `Exited` is delivered.
10. No deadlock. At the end, no simulated threads are alive except those the
    workload still holds.
11. Pair: `try_wait` and `wait` agree. Killing an exited child succeeds. The
    writer can be taken only once. Output ends when the last holder exits,
    following the personality.

**Initial targets.** These are hypotheses from reading the code, not known
bugs:

- `Control::signal` checks `exited` then `terminal_open` before
  `kill(-pgid)`. There is a window for pgid reuse between the check and the
  kill.
- Ordering between `closed`, `exited` and late output in `Polled::read` and
  `report_exit`.
- Writer threads when a program never reads and the process is dropped.
- The `HANGUP_GRACE` and `HANGUP_CHECKS` escalation under delayed signals.
- Cleanup when `spawn` fails partway (after `open`, before `report_exit`).

### B4. CI and developer workflow

- `scripts/sim-pty.sh`:
  - `SIMULATOR_RUNS=N`, with a seed or a random one
  - `--verify-determinism`
  - `--replay <seed>` (one process, logs on)
- CI:
  - a fixed set of regression seeds, run on every change to
    `packages/pty/**`
  - a nightly job with many random seeds and determinism verification on
  - failures print the reproduction line and commit SHA
- Every failing seed that leads to a fix becomes a regression seed, plus a
  conformance test where possible.
- Developing against MoosicBox: use `[patch.crates-io]` in bmux pointing at
  `../MoosicBox/packages/switchy/*` (the path overrides already commented in
  the root `Cargo.toml`). Before merging the bmux side, publish the new switchy
  release and drop the patch.

## Workstream C: simulation TUI on bmux's TUI stack

The TUI is built on `bmux_tui`, `bmux_tui_runtime` and `bmux_tui_components`,
not ratatui. It plugs in through simvar's observer interface (A6). It is
developer tooling outside the simulated code, so the "no external dependencies"
rule doesn't apply to it.

### Crates and placement

- **`packages/simvar-tui` (`bmux_simvar_tui`)** is in bmux's main workspace. It
  depends only on `simvar_observer` and the bmux TUI crates, never on `simvar`
  or `switchy`, so it is built, linted and tested with the rest of bmux. It
  provides:
  - the generic panels (C1, C2, C4)
  - the `BmuxSimObserver` implementation of `SimObserver`
  - a `SimPanel` extension trait for panels specific to one simulator
- **The PTY-specific panels (C3)** live in `packages/pty/simulator` (outside the
  main workspace, see B3) and register through `SimPanel`. Later simulators for
  the pane runtime, IPC or the server reuse the crate and add their own panels.

### C1. Run dashboard (parity with simvar's TUI, plus failures)

- the parallel run slots: run number, seed, step, simulated time, progress,
  schedule strategy, personality
- runs completed out of the total, and pass/fail counts
- a failure list showing each failure's reproduction line, commit SHA and panic
  message, with the reproduction line copyable to the clipboard
- components: `progress_bar`, `table`, `status_bar`, `toast_stack`,
  `key_hint_bar`

### C2. Scheduler and trace view (generic, from `SchedulerSnapshot`)

- the simulated threads, each with its state, what it is blocked on and its
  timer deadline
- a timeline of scheduling decisions (thread, operation, simulated time)
- the trace hash, and its status when determinism verification is on
- pass/fail status per invariant, from the simulator's telemetry
- components: `table`, `tree_view`, `sparkline` (decision rate), `text_view`

### C3. Live PTY views (PTY-specific, from `KernelSnapshot`)

- a process tree by session and process group, showing each process's state,
  exit status, signal handling and terminal holders
- each simulated terminal's screen, rendered by feeding its byte log into
  `bmux_terminal_grid` and drawing it with `terminal_view`, next to what
  `bmux_pty` delivered to the consumer (to spot missing or reordered output)
- a signal log and the queue depth over time (`sparkline`)

### C4. Replay debugger

- open one seed (with its strategy and personality) in single-run replay mode
  (A6)
- pause, step one decision or one simulated-time step, run to a decision or
  time, and break on a predicate (for example "an invariant failed" or
  "thread X blocks on Y")
- all panels show state as of the current point
- **Seeking backwards** reruns the seed from the start in a fresh process up to
  the target. Determinism makes this exact. Process-local checkpoints aren't
  possible, because simulated threads hold real stacks, so short runs keep
  seeks fast. Recorded decision lists replay directly, without the RNG.

## Phases

Each phase ends with its acceptance check passing.

| # | Phase | Repo | Acceptance |
| --- | --- | --- | --- |
| 0 | Shared `SimContext` (A1) and dependency-free random/time (A2) | MoosicBox | Existing MoosicBox simulators produce identical results for fixed seeds before and after. `switchy_random`/`switchy_time` simulator builds have no external dependencies. |
| 1 | `switchy_thread` native + baton scheduler, deadlock/leak/watchdog diagnostics, trace hash (A3, without weak memory) | MoosicBox | The crate's own tests cover handoff, timed waits, panics, teardown, deadlock reports, and run-twice hash equality over many seeds. |
| 2 | simvar `thread` integration and `SIMULATOR_VERIFY_DETERMINISM` (A5, part) | MoosicBox | A toy multithreaded simulation passes determinism verification. A deliberately non-deterministic one is caught. |
| 3 | `switchy_pty` native: move the OS layer out of `bmux_pty` (A4 native, B1 partial) | both | No behaviour change. The existing `bmux_pty` tests pass on macOS, Linux and Windows. |
| 4 | Port `bmux_pty` onto `switchy_thread` and `switchy_time` (B1 rest) | bmux | Tests pass on all three operating systems with the native backend. The clippy bans are on. |
| 5 | `switchy_pty` simulator: process table, terminal device, pipes, signals, personalities (A4 simulator, no faults) | MoosicBox | Unit tests of the kernel model. |
| 6 | Conformance suite (B2) | bmux | The suite passes on native (3 OSes) and on the simulator, with personality differences listed explicitly. |
| 7 | `bmux_pty` simulator: workload, oracle, invariants (B3), then the fault layer | both | Thousands of seeds run clean, or every failure found is fixed and becomes a regression seed. Determinism verification passes. |
| 8 | CI integration (B4) | bmux | Regression seeds on every PR that touches `packages/pty/**`. Nightly random seeds. |
| 9 | Weak-memory mode (A3) applied to `closed`, `exited` and `took_writer` | both | Mode on in the nightly job. Any finding is fixed or explained. |
| 10 | `simvar_observer` crate and observer refactor; ratatui becomes `RatatuiObserver`; headless observer; snapshot APIs in `switchy_thread` and `switchy_pty` (A6, A4/A3 snapshots) | MoosicBox | The ratatui TUI looks and behaves as before. Trace hashes match with and without an observer attached. |
| 11 | `bmux_simvar_tui` run dashboard (C1) | bmux | Parity with simvar's TUI plus the failure list with reproduction lines. Clippy/nextest/machete pass in the main workspace with no switchy features pulled in. |
| 12 | Scheduler and trace view (C2) | bmux | Shows live thread states and decisions for a running PTY simulation. |
| 13 | Live PTY views (C3) | bmux | The terminal screens of fake programs render live. The process tree and signal log match the oracle. |
| 14 | Replay debugger (C4) and replay control in simvar (A6) | both | Step and seek through a failing seed. A paused-and-resumed replay has the same trace hash as an uninterrupted run. |
| 15 | Next targets: `switchy_async` `spawn_blocking` on simulated threads, then the pane runtime's reader loop and session control | both | Separate plan. |

Phases 0–2 (MoosicBox) and phase 3 can proceed in parallel. Phase 3 is a pure
refactor and is useful on its own. The TUI comes last (phases 10–14): until
then, simvar's existing ratatui TUI and logs are enough. Phase 10 can begin as
soon as phase 7 is producing failures worth looking at.

## Risks

- **A simulator that lies.** The kernel model drifts from real OS behaviour, so
  bugs hide or false alarms appear. Mitigation: the conformance suite (B2) on
  every change, and personality differences that are explicit and documented.
- **Bugs in the baton scheduler itself.** They would make simulations
  non-deterministic. Mitigation: the scheduler's own run-twice tests, and
  determinism verification in nightly CI.
- **Time granularity.** simvar time is `step × step_multiplier` milliseconds,
  but `bmux_pty`'s constants are 10 ms (`POLL`), 50 ms (`HANGUP_GRACE`) and
  250 ms (drain grace). Pin the PTY simulation's step multiplier at 1 ms or
  less, or rely on the harness jumping ahead when only timers are pending.
- **Slow thread handoffs.** Switching between OS threads takes microseconds,
  while `bmux_pty` workloads need thousands to hundreds of thousands of
  switches per run. Measure it in phase 1. Switch-point density on atomics is
  configurable.
- **Changing shared switchy crates.** The `SimContext` refactor touches
  MoosicBox's existing simulators. Mitigation: the fixed-seed comparison in
  phase 0.
- **Coordinating two repositories.** Use `[patch.crates-io]` during
  development and publish switchy before merging bmux.
- **Observers changing runs.** A snapshot or pause that accidentally draws from
  the RNG or creates a switch point would make runs depend on whether a TUI is
  attached. Mitigation: snapshot APIs that only read, and the trace-hash
  comparison with and without an observer (phase 10) and with pauses
  (phase 14).
- **Feature leakage through the TUI crate.** If `bmux_simvar_tui` ever depends
  on `simvar` or `switchy`, the main workspace build turns on switchy's
  simulator backends. Mitigation: `simvar_observer` has no switchy dependency.
  A check in `scripts/check-workspace-dependencies.sh` keeps `simvar` and
  `switchy` simulator features out of main-workspace crates.

## Decisions

Settled on 2026-11-21:

1. **The native OS layer moves into `switchy_pty`.** It sits beside the
   simulator backend, following the switchy pattern. `bmux_pty` keeps the
   orchestration logic.
2. **CI alternates uniform-random and PCT scheduling** by seed parity.
   Configuration can pin either one.
3. **The simulated terminal implements the core subset** of the line discipline
   (A4). Canonical-mode editing waits until a test needs it.
4. **Weak-memory mode comes after the basic simulator** (phase 9).
5. **The TUI plugs in through an observer interface in simvar** (A6). simvar
   keeps ratatui as its default observer, and bmux ships its own frontend built
   on `bmux_tui`.
6. **The first TUI includes all four parts:** run dashboard, scheduler/trace
   view, live PTY views and replay debugger. They are built as the final phases
   (10–14).
7. **The generic TUI crate is `packages/simvar-tui` (`bmux_simvar_tui`)** in
   bmux's main workspace. The PTY-specific panels live in the PTY simulator.
