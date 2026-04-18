# bmux_decoration_plugin

The decoration plugin for BMUX. Owns all pane visual styling (borders,
focus highlighting, decoration characters). Depends on the windows
plugin API for pane lifecycle state, and exposes its own typed API via
`bmux_decoration_plugin_api` for other plugins to query/adjust the
decoration style.

Today the plugin ships an in-memory implementation with four built-in
border styles (`none`, `ascii`, `single`, `double`). ASCII is the
default, matching the characters the core renderer currently paints.
Future work: integrate with a scene-layout protocol so the plugin itself
emits the decoration draw commands instead of piggybacking on core's
renderer.
