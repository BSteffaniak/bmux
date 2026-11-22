# consensus

Model checks bmux's owned consensus engine
(`bmux_cluster_plugin::consensus_engine`)
with `simvar::explore`. The model runs real engines; each node's durable store
changes
only when a persistence effect completes, so a crash recovers exactly what was
durable.

```bash
cargo sim consensus                 # exhaustive search, then random walks
cargo sim consensus --walks-only    # random walks only
SIMULATOR_SEED=7 cargo sim consensus --walks-only   # reproduce a walk run
```

Checked on every explored state: election safety, state machine safety (no index
is
ever committed with two identities), log matching, leader completeness, commit
and
apply within the log, and that no node stops. Liveness examples: a proposal
commits,
and (in walks) a learner is promoted through joint consensus.

`SIMULATOR_EXPLORE_MAX_STATES`, `_MAX_DEPTH`, `_THREADS`, `_WALKS` and
`_WALK_SEED`
adjust the checker; a failing walk prints the seed that replays it alone.
