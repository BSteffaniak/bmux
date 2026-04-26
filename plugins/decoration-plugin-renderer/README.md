# bmux_decoration_plugin_renderer

Client-side render extension for the [bmux decoration
plugin](../decoration-plugin).

The decoration plugin runs in the bmux server process. Every time its internal
state changes, it publishes a retained `DecorationScene` on the typed plugin
event bus. The server relays the current state and live replacements to
streaming clients as `ServerEvent::PluginBusEvent` values over the attach IPC
stream.

This crate provides the **client-side** consumer of those relayed
scenes:

- Registers an [`AttachRenderExtension`][ext] so every render pass
  consults the decoration scene for each visible surface.
- Subscribes to the client-side retained `bmux.scene/scene-protocol` state
  channel and caches the latest `DecorationScene`.
- Delegates per-surface paint to
  \[`bmux_scene_protocol_render::paint::apply_paint_commands`\].

Call \[`install`\] once during CLI bootstrap to register the extension
and set up the subscriber. The function is idempotent; repeat
invocations leave the existing extension in place.

[ext]: bmux_plugin::AttachRenderExtension
