# Simulations

bmux's deterministic simulations, on
[simvar](https://github.com/MoosicBox/MoosicBox/tree/master/packages/simvar)
and switchy's simulated threads, time, terminals and processes. One seed
decides everything in a run (interleavings, faults, timing), so any failure
replays exactly on the same build.

| Simulation | Tests |
| --- | --- |
| [`pty`](pty/README.md) | `bmux_pty` against a simulated kernel |
| [`pane-runtime`](pane-runtime/README.md) | the pane runtime plugin on simulated tasks, threads, time and processes |
| [`local-terminal`](local-terminal/README.md) | the plugin-less local terminal's server, over in-memory streams |

This is a Cargo workspace of its own, excluded from bmux's main one, so the
`simulator` features its members enable never reach bmux's real builds.

## Running

```bash
cargo sim pty                                      # works from anywhere in the repo
SIMULATOR_SEED=42 SIMULATOR_RUNS=1 cargo sim pty   # replay one run
SIMULATOR_SEED=42 SIMULATOR_RUNS=1 cargo sim pty --tui   # watch it
cargo sim pty --regression                         # its pinned seeds
```

`cargo sim NAME [ARGS]` (an alias for `cargo xtask sim`) builds and runs member
`NAME` in release mode. Runs are configured through simvar's environment:

| Variable | Meaning |
| --- | --- |
| `SIMULATOR_RUNS` | Runs (default 1) |
| `SIMULATOR_MAX_PARALLEL` | Runs at once (default: CPUs) |
| `SIMULATOR_SEED` | The first run's seed (default random) |
| `SIMULATOR_VERIFY_DETERMINISM` | `1`: replay every run, fail if it differs |
| `SIMULATOR_SCHEDULE_STRATEGY` | `random`, `pct` or `alternate` (default) |
| `SIMULATOR_WEAK_MEMORY` | `1`: model weak memory for atomics |
| `SIMULATOR_SHRINK` | `1`: shrink a failing run's thread schedule |
| `SIMULATOR_SCHEDULE` | Replay a thread schedule `SIMULATOR_SHRINK` printed |

Each simulation documents its own variables and flags. `support` (crate
`sim_support`) holds what they share: scheduler and kernel telemetry, pausing
at decisions and failures, the processes-and-terminals panel, and
`run_with_tui`. A simulation's `--tui`
dashboard (`bmux_simvar_tui`) pauses, steps, runs to decisions, breaks on
conditions and goes back; see the [pty simulation](pty/README.md).

## Linting

```bash
cargo clippy --manifest-path simulation/Cargo.toml --all-targets -- -D warnings
```
