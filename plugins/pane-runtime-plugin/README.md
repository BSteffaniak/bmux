# bmux_pane_runtime_plugin

Owns pane runtime for bmux: PTY handles (spawned via `portable-pty`),
per-session layout tree, per-pane output fanout buffer, per-pane
terminal-protocol + vt100 cursor + shell-integration parsers, per-pane
resurrection state, per-session attach viewport.

Consumed by the server via three trait-object handles registered in
the plugin state registry during `activate`:

- `PaneOutputReaderHandle` — server's `event_push_task` uses this to
  read pane output on behalf of attached clients.
- `PaneRuntimeCommandsHandle` — server's IPC handlers dispatch every
  pane/session mutation through this.
- `StatefulPluginHandle` — the snapshot orchestrator walks this
  during save/restore, carrying the pane-runtime section of the
  combined envelope.

The plugin also registers a `WireEventSinkHandle` consumer (for
publishing `Event::PaneExited` / `Event::PaneRestarted` /
`Event::AttachViewChanged`) and a `RecordingSinkHandle` consumer
(for pane-output recording).
