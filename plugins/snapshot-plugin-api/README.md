# bmux_snapshot_plugin_api

Typed public API of the `bmux.snapshot` plugin.

Hand-written (no BPDL). Hosts:

- `SnapshotPluginConfig` — CLI-registered configuration (snapshot file path + debounce window).
- Capability + interface id constants.
- `SnapshotRequest` / `SnapshotResponse` wire enums for the plugin's
  typed service surface.
- `typed_client` module — async helpers over any
  `bmux_plugin_sdk::TypedDispatchClient` (`save_now`, `status`,
  `restore_dry_run`, `restore_apply`).
- `offline_snapshot` module — the `offline_kill_sessions` utility
  CLI subcommands call when the server is down.
