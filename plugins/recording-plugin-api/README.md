# bmux_recording_plugin_api

Typed public API of the bmux recording plugin. Other plugins and
attach-side callers depend on this crate for typed access to recording
lifecycle operations (start, stop, list, cut, rolling, prune, etc.).

Also hosts the `RecordingSink` trait that `packages/server` uses to
write pane-output and server-event records into the plugin-owned
`RecordingRuntime` without taking a dependency on the recording plugin
impl crate.
