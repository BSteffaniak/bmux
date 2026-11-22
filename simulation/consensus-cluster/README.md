# consensus-cluster

A bmux owned-consensus cluster (three voters by default, `--nodes 5` for five) under faults. Each host runs the real
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

Snapshot transfer is not yet exercised here (compaction is disabled until the
simulated transport stages snapshots).
