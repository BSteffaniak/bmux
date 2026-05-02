# bmux_pane_runtime_state

Neutral primitive crate hosting the pure-data types used by the bmux
pane-runtime system:

- Layout tree (`PaneLayoutNode`, `LayoutRect`, `FloatingSurfaceRuntime`)
- Pane identity + launch + resurrection records (`PaneRuntimeMeta`,
  `PaneLaunchSpec`, `PaneResurrectionSnapshot`, `PaneCommandSource`)
- Attach viewport record (`AttachViewport`)
- Error enum (`SessionRuntimeError`)
- Reader/commands trait abstractions + handle newtypes
  (`PaneOutputReader` + `PaneOutputReaderHandle`, etc.)

Depended on by both `packages/server` (connection-scoped consumers) and
the `bmux.pane_runtime` plugin (owner of concrete state). Contains no
PTY, tokio, or terminal parser primitives — those live in the plugin impl crate.
