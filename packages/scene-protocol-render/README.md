# bmux_scene_protocol_render

Shared helpers for renderers of \[`bmux_scene_protocol`\] paint commands.

Paint commands are the decoration authoring vocabulary: the `bmux.scene` contract that themes
and scripts write. The decoration renderer lowers them once, per attached terminal's
capabilities, into the host's retained render items (`RenderSceneItem`, a keyed `RenderOp` or
terminal graphic), which is the one vocabulary the attach renderer paints.

This crate holds what that lowering shares: terminal capability matching for capability-gated
commands, and style interpolation for gradients.
