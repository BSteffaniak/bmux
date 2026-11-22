# pty: the bmux_pty simulation

Deterministic simulation testing for `bmux_pty`, on
[simvar](https://github.com/MoosicBox/MoosicBox/tree/master/packages/simvar) and
switchy's simulated terminals, processes, threads and time. Run it with
`cargo sim pty`; see [the simulation README](../README.md).

Each run starts one to three driver threads. Each driver runs a random program
from switchy_pty's simulated program library on a `PtyCommand`, then does random
things to it: reads events every way the API allows, stalls, types, resizes,
interrupts, kills, stops output or drops it. Then it checks what it saw against
what really happened in the simulated kernel:

1. Exactly one `Exited`, last.
2. With `output_complete`, delivered output equals what programs wrote;
   otherwise it is a prefix of it.
3. The control handle knows the exit when `Exited` arrives.
4. After a kill or drop, nothing in the process group is left running.
5. Interrupts and kills never reach a session no driver signalled, even with
   process ids reused aggressively.
6. A program reporting window sizes reports the last resize.
7. No deadlock, no leaked simulated thread, no livelock (a budget of simulated
   time).

The seed decides the programs, actions, thread interleavings, kernel faults and
the modelled OS (Linux or macOS), so a failure replays exactly on the same
build.

```bash
cargo sim pty                                      # SIMULATOR_RUNS random runs
SIMULATOR_RUNS=2000 cargo sim pty                  # more
SIMULATOR_SEED=42 SIMULATOR_RUNS=1 cargo sim pty   # replay one run
cargo sim pty --tui                                # the dashboard, running continuously
cargo sim pty --tui 42                             # ... with the run of seed 42 open
SIMULATOR_VERIFY_DETERMINISM=1 cargo sim pty       # replay every run and compare traces
cargo sim pty --regression                         # the seeds in regression-seeds
SIMULATOR_PTY_PID_REUSE=1 cargo sim pty            # reuse every freed process id
SIMULATOR_PTY_FAULTS=0.1 cargo sim pty             # more injected faults
SIMULATOR_WEAK_MEMORY=1 cargo sim pty              # weak memory for atomics
SIMULATOR_SHRINK=1 SIMULATOR_SEED=42 cargo sim pty # shrink a failure's thread schedule
```

## Watching and debugging a run

`--tui` runs the simulation continuously in the
[simulation dashboard](../../packages/simvar-tui/README.md): the fleet's
throughput, outcomes and coverage; failures kept, reproduced, shrunk and
saved; and time travel through any run (its timeline, threads, decisions and
invariants, and the Kernel view: processes by session and process group, the
signal log, and each terminal's screen as programs wrote it next to what
`bmux_pty` delivered). `?` in the dashboard lists every key.

`SIMULATOR_SHRINK=1` shrinks a failing run's thread schedule (the switches it
made beyond the ones it had to) and prints the smallest failing schedule with
its replay command, `SIMULATOR_SCHEDULE=… cargo sim pty`; the dashboard does
this on its own for every failure it finds.

Bugs found so far: a kill or interrupt after the process was reaped could reach
an unrelated newer process that reused its id (fixed in `bmux_pty`: the exited
process stays unreaped while its control handle exists); and a simulator
fidelity bug (a killed simulated process could still start children).
