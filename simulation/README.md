# Simulations

bmux's deterministic simulations, on
[simvar](https://github.com/MoosicBox/MoosicBox/tree/master/packages/simvar)
and switchy's simulated threads, time, terminals and processes. One seed
decides everything in a run (interleavings, faults, timing), so any failure
replays exactly on the same build.

| Simulation                                         | Tests                                                                                                                                              |
| -------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`consensus-cluster`](consensus-cluster/README.md) | a three-node consensus cluster (real driver and redb store on the crash-modelling filesystem) under partitions, lossy links and power-loss crashes |
| [`consensus`](consensus/README.md)                 | the owned consensus engine, model checked: exhaustive search plus seeded random walks with crashes, loss, duplication and membership change        |
| [`pty`](pty/README.md)                             | `bmux_pty` against a simulated kernel                                                                                                              |
| [`pane-runtime`](pane-runtime/README.md)           | the pane runtime plugin on simulated tasks, threads, time and processes                                                                            |
| [`local-terminal`](local-terminal/README.md)       | the plugin-less local terminal's server, over in-memory streams                                                                                    |

This is a Cargo workspace of its own, excluded from bmux's main one, so the
`simulator` features its members enable never reach bmux's real builds.

## Running

```bash
cargo sim pty                                      # works from anywhere in the repo
SIMULATOR_SEED=42 SIMULATOR_RUNS=1 cargo sim pty   # replay one run
cargo sim pane-runtime --tui                       # the dashboard: run continuously
cargo sim pane-runtime --tui 42                    # ... with the run of seed 42 open
cargo sim pane-runtime --fleet                     # continuously, printing findings
cargo sim pty --regression                         # its pinned seeds
cargo sim pane-runtime --check                     # is everything it links deterministic?
cargo sim pane-runtime --coverage                  # did a `runs = false` crate run?
```

`cargo sim NAME [ARGS]` (an alias for `cargo xtask sim`) builds and runs member
`NAME` in release mode. `--check` instead runs `sim-check` (`check/`), which checks
everything `NAME` links with `switchy_cargo`: each local crate scanned by the rules in
its manifest's `[package.metadata.switchy.simulation]`, and every other crate on the
audit list in `determinism.toml`. `--coverage` builds `NAME` instrumented for coverage
(in `target/sim-coverage`), runs it, and fails if a line of a crate marked `runs = false`
ran; it needs LLVM's tools (`rustup component add llvm-tools-preview`, `nix build
.#llvm-tools`, or `LLVM_PROFDATA` and `LLVM_COV`).

Cargo runs in this directory, so builds read `.cargo/config.toml`: `getrandom` (uuid, rand's
OS generator, hasher seeds) draws from the run's seed. Runs are configured through simvar's
environment:

| Variable | Meaning |
| --- | --- |
| `SIMULATOR_RUNS` | Runs (default 1; `forever` until stopped, the default with `--tui` and `--fleet`) |
| `SIMULATOR_ON_FAILURE` | `stop` or `continue` (the default with `--tui` and `--fleet`) |
| `SIMULATOR_MAX_PARALLEL` | Runs at once (default: CPUs) |
| `SIMULATOR_SEED` | The first run's seed (default random) |
| `SIMULATOR_VERIFY_DETERMINISM` | Replay every run and fail if it observed anything differently (its schedule, processes and terminal output, file tree, random draws, clock): `process` replays it in a fresh, perturbed process (what CI runs), `1` in the same process |
| `SIMULATOR_DIGEST_BACKTRACE_AT` | Add a backtrace to this observation of a run's digest (a determinism failure names the first one that differs) |
| `SIMULATOR_SCHEDULE_STRATEGY` | `random`, `pct` or `alternate` (default) |
| `SIMULATOR_WEAK_MEMORY` | `1`: model weak memory for atomics |
| `SIMULATOR_SHRINK` | `1`: shrink a failing run's thread schedule |
| `SIMULATOR_SCHEDULE` | Replay a thread schedule `SIMULATOR_SHRINK` printed |

Each simulation documents its own variables and flags. `support` (crate
`sim_support`) holds what they share: each run's timeline index, events and
shape; scheduler and kernel telemetry for the runs the dashboard watches;
pausing replays at decisions and failures; the Kernel view; steering simvar's
fleet; and `main`, which gives every simulation `--tui [SEED[:EPOCH]]` and
`--fleet`. The dashboard (`bmux_simvar_tui`, see
[its README](../packages/simvar-tui/README.md)) shows the fleet, keeps and
shrinks failures, and travels through any run.

## Linting

```bash
cargo clippy --manifest-path simulation/Cargo.toml --all-targets -- -D warnings
```
