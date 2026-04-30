# bmux_snapshot_plugin_api

Typed public API of the `bmux.snapshot` plugin.

BPDL-backed contract plus non-transport utilities:

- `SnapshotPluginConfig` — CLI-registered configuration (snapshot file path + debounce window).
- Generated `snapshot-types`, `snapshot-state`, and `snapshot-commands`
  modules from `bpdl/snapshot-plugin.bpdl`.
- `offline_snapshot` module — the `offline_kill_sessions` utility
  CLI subcommands call when the server is down.
