# bmux_snapshot_runtime

Neutral primitive crate hosting the snapshot-orchestration traits +
registry + dirty flag used by the `bmux.snapshot` plugin, the server,
and stateful state plugins.

- `StatefulPluginRegistry` — append-only collection of
  `StatefulPluginHandle`s, registered by each participant during
  `activate`.
- `SnapshotDirtyFlag` — atomic dirty-mark + last-marked timestamp.
  Server flips it on state mutations; the snapshot plugin watches it
  in a debounced background task.
- `SnapshotOrchestrator` trait + `SnapshotOrchestratorHandle` — the
  trait-object surface server uses to delegate save/restore/status
  operations without naming the plugin impl crate.
- `RestoreSummary`, `DryRunReport`, `SnapshotStatusReport` — aggregate
  result types.

See `docs/plugins.md` for the architectural motivation.
