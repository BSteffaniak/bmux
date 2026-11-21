# bmux_decoration_plugin_renderer

Client-side render extension for the [bmux decoration plugin](../decoration-plugin).

The decoration plugin runs in the bmux server process, with one view per attach
connection. A view pushes its connection the changes to its scene as
`DecorationSceneDelta`s (`bmux.scene/scene-delta`, the whole scene first) when the
server can send to it; the attach client reads the whole scene once when it starts.

This crate provides the **client-side** consumer of those scenes:

- `relay_scene_delta_on` and `publish_relayed_scene_on` keep a `SceneReplica` (the
  scene with shared surfaces) current on the attach runtime's presentation bus.
- `renderer_for_bus` registers an [`AttachRenderExtension`][ext] that takes the
  latest replica before each frame, re-deriving only the surfaces a delta changed.
- Each surface's paint commands are lowered into retained scene layers (before and
  after pane content), each with a revision hashed from its inputs, for the attach
  renderer to diff and paint.

[ext]: bmux_plugin::AttachRenderExtension
