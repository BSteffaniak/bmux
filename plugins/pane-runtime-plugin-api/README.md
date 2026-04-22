# bmux_pane_runtime_plugin_api

Typed public API of the `bmux.pane_runtime` plugin.

Generated from `bpdl/pane-runtime-plugin.bpdl` at compile time via the
`bmux_plugin_schema_macros::schema!` macro. Five interfaces:

- `pane_runtime_state` — queries over pane/session runtime.
- `pane_runtime_commands` — mutating pane + session-runtime commands.
- `attach_runtime_commands` — per-client attach lifecycle commands.
- `attach_runtime_state` — attach-view queries (layout, snapshot,
  pane output batch, pane images).
- `pane_runtime_events` — pane + attach lifecycle event stream.

Plus hand-written capability constants in the `capabilities` module.
