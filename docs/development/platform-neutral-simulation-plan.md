# Plan: one pane runtime for every operating system, simulated as each

Phase 16 of the [bmux_pty simulation plan](pty-deterministic-simulation-plan.md),
following the [pane runtime simulation](pane-runtime-deterministic-simulation-plan.md).

## Goal

The pane runtime's logic is the same code on every operating system. What
differs between them sits behind one platform interface with three
implementations: native Unix, native Windows, and simulated. The simulation
picks an operating system personality (Linux, macOS or Windows) from the
run's seed on any host. It then runs the same core paths that the native
build runs on that system, the Windows-only ones included. Conformance suites
check, on each real operating system, that the native implementation behaves
as the simulated personality says it does.

## Where things stand

Phase 15 made the pane runtime simulatable, with two limits:

- **Unix only.** The simulated kernel models Unix sessions, and the plugin's
  core logic still chooses behaviour with about 35 `cfg(unix)` and
  `cfg(windows)` blocks in `runtime.rs`. Examples are the ConPTY startup
  handshake, the wait for the console to close, the executable search,
  process start identities and working directories, and `ps` inspection. A
  simulation on any host therefore runs the Unix paths of the host's build.
- **Determinism shown only for the paths exercised.** The pane runtime
  simulation verifies replays for its workload. Nothing checked mechanically
  that the plugin uses only simulation-controlled primitives. Environment
  reads went straight to the process environment. Process-wide caches and
  pointer-ordered locking were unexamined, and restart and crash recovery
  were not simulated.

## Design

### Terminal behaviour is data, not configuration

`switchy_pty` describes the terminal backend's behaviour as a value,
`switchy_pty::behavior()`:

| Field | Unix | Windows (ConPTY) |
| --- | --- | --- |
| `startup_cursor_query` (the console asks `CSI 6 n` and holds the program's output until the master answers) | no | yes |
| `output_ends_with_console` (the reader's end of file comes when the master and slave are dropped, not when the programs end) | no | yes |
| `process_groups` (sessions, groups and signals, versus termination only) | yes | no |

The native backends return their operating system's values. The simulator
returns its personality's values. The plugin branches on this value, so every
path is compiled on every host and is reachable in simulation.

### A Windows personality for the simulated kernel

`Personality::Windows`:

- On open, the console writes `CSI 6 n` to the master and holds the slave
  side's output until the master writes a cursor report. The report is
  consumed and never reaches the program as input.
- Output ends when the console is closed, meaning the master and slave
  objects are both dropped. Programs ending do not end it. Closing the
  console ends the processes attached to it.
- There are no process groups or signals from outside. `signal_group` and
  any signal other than kill to another process fail as unsupported.
  `ctrl+c` still interrupts the console's processes, as `CTRL_C_EVENT` does.
  A killed process exits with code 1 and no signal, as `TerminateProcess`
  reports it.
- A process cannot leave the console session it was created in, so a
  session's members stand for a Job Object's: they include descendants that
  outlive their parent.

### The plugin's platform interface

`plugins/pane-runtime-plugin/src/platform/` is the only place that chooses an
operating system, at compile time for native builds and from the kernel's
personality in simulation:

- executable search conventions (the executable bit versus `PATHEXT`), the
  baseline shell, and shell names (`nu.exe` is `nu`)
- process start identity, working directory, and process group inspection
- waiting for a child's exit without reaping it
- private directory permissions
- session control: prepare, adopt, cooperative stop, sweep, occupancy,
  release, crash records and recovery (as today's `session_control`
  backends; the simulated one follows the personality's job or session
  semantics)
- foreground stop and process signals (as today's `native_process`)

Core code outside `platform/` has no `cfg(unix)`, `cfg(windows)` or
`target_os`. A source scan test enforces this, alongside the scan that
allows only simulation-controlled primitives.

### Determinism, mechanically

`tests/primitives.rs` scans the plugin's production source for tasks,
threads, locks, clocks, randomness, environment reads, process spawns,
unordered maps and directory listings that bypass what the simulation
controls. Native backends are exempt as files. Any other line must say why
it is safe with `// simulation-safe: <reason>`. Environment reads go through
`switchy_env`, and each simulated run isolates a fixed environment.

### Conformance on every operating system

The `bmux_pty` conformance suite runs natively on Linux, macOS and Windows,
and on the simulator under each personality. It asserts `behavior()` and the
behaviour it describes: the startup query, when output ends, how kills are
reported, and what `signal_group` does. The plugin gains a matching suite for
its platform interface: start identities, working directories, sweeps, and
cooperative stop.

## Phases

| # | Phase | Repo | Acceptance |
| --- | --- | --- | --- |
| 16.1 | Source scan guard; environment through `switchy_env` with per-run isolation; process-wide caches per run; no pointer-ordered locking | both | guard passes; sims verified |
| 16.2 | `behavior()` and the Windows personality | MoosicBox | simulator tests per personality; `cargo sim pty` regression seeds pass |
| 16.3 | Platform interface; no OS `cfg` in plugin core logic; scan enforces it | bmux | plugin suite passes natively on 3 OSes |
| 16.4 | Pane runtime simulation on any host, personality by seed; restart and crash recovery in the workload | bmux | 300+ verified runs per personality on 3 OSes |
| 16.5 | Conformance: `bmux_pty` suite per personality and natively; platform interface suite | bmux | passes on Linux, macOS, Windows |
