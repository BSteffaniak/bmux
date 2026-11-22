# consensus-cluster

A bmux owned-consensus cluster (three voters by default, `--nodes 5` for five)
under faults. Each host runs the real
consensus driver over the real redb store, whose file I/O goes through the
`switchy_fs` simulator in its own namespace; a crash applies power-loss
semantics (unsynced data and directory changes may be lost) and the host
restarts from its store.

Node 1 founds a single-voter cluster; the workload adds the others as
learners and promotes them through joint consensus, then proposes uniquely
identified commands and issues linearizable reads while a nemesis partitions
(symmetric and one-way), isolates, degrades links and bounces hosts. After
healing, every node's applied state must be identical, contain every
acknowledged write and nothing never proposed; reads must observe every write
acknowledged before they started.

```bash
cargo sim consensus-cluster
SIMULATOR_RUNS=40 cargo sim consensus-cluster
cargo sim consensus-cluster --nodes 5                      # five voters
SIMULATOR_SEED=42 SIMULATOR_RUNS=1 cargo sim consensus-cluster   # replay
SIMULATOR_VERIFY_DETERMINISM=1 cargo sim consensus-cluster
```

Nodes compact every 16 applied entries, so lagging followers regularly catch
up by snapshot install. The nemesis also removes a voter through joint
consensus under faults; healing restores it (learner, then promotion) before
convergence is checked. Every run must reach a verdict: formation, restoration
and convergence have simulated-time deadlines, so a stuck run fails instead of
running out the clock.

Debugging: `SIM_DEBUG=1` prints a fault/membership/acknowledgement history and
per-node state differences on failure; `SIM_TRACE=1` traces every storage
operation per node (`SIM_SUMMARY=1` adds state after each, which perturbs the
schedule).
