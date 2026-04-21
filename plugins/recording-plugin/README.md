# bmux_recording_plugin

Shipped recording plugin for bmux. Owns `RecordingRuntime` (manual +
rolling) and serves recording control operations via typed dispatch.

Server writes into the runtime via the `RecordingSink` trait looked up
through the plugin state registry, so `packages/server` does not
depend on this crate.
