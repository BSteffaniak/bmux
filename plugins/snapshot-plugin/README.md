# bmux_snapshot_plugin

Snapshot orchestration plugin for bmux.

Walks the `bmux_snapshot_runtime::StatefulPluginRegistry`, builds a
combined envelope over every registered `StatefulPlugin` participant,
debounces dirty marks, and persists the result to a CLI-configured
file path. Dispatches save/restore/status through the typed
`snapshot-commands::dispatch(SnapshotRequest) -> SnapshotResponse`
service defined in `bmux_snapshot_plugin_api`.
