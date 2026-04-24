# bmux_scene_protocol_render

Terminal-ANSI renderer for \[`bmux_scene_protocol`\] paint commands.

The scene-protocol crate defines the wire schema for decoration
output (paint commands, border glyphs, styles). This crate turns a
`SurfaceDecoration` value into the corresponding ANSI/VT-sequence
bytes on a `std::io::Write` target.

Consumers:

- `bmux_attach_pipeline` uses the paint-command executor during frame
  assembly to apply plugin-published decoration output.
- Render extensions (`AttachRenderExtension` implementors in each
  plugin's renderer crate) call `apply_paint_commands` directly to
  paint their surface decoration on top of the pane content.

No decoration-plugin-specific knowledge lives here; this crate is a
generic helper for anything that produces scene-protocol output.
