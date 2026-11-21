# bmux_pty simulator

Deterministic simulation testing for `bmux_pty`, on
[simvar](https://github.com/MoosicBox/MoosicBox/tree/master/packages/simvar) and
switchy's simulated terminals, processes, threads and time. It is its own Cargo
workspace so its `simulator` features never reach bmux's main workspace.

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
scripts/sim-pty.sh                       # 100 random runs
scripts/sim-pty.sh --runs 2000           # more
scripts/sim-pty.sh --replay SEED         # replay one run, with logs
scripts/sim-pty.sh --regression          # the seeds in regression-seeds
scripts/sim-pty.sh --verify-determinism  # replay every run and compare traces
scripts/sim-pty.sh --replay SEED --tui   # watch one run; space pauses, n/N step
SIMULATOR_PTY_PID_REUSE=1 scripts/sim-pty.sh   # reuse every freed process id
SIMULATOR_PTY_FAULTS=0.1 scripts/sim-pty.sh    # more injected faults
```

Bugs found so far: a kill or interrupt after the process was reaped could reach
an unrelated newer process that reused its id (fixed in `bmux_pty`: the exited
process stays unreaped while its control handle exists); and a simulator
fidelity bug (a killed simulated process could still start children).
