# bmux_recording_runtime

Neutral primitive crate hosting the `RecordingSink` trait + registry
handle for the recording-plugin domain. Both `packages/server` (core
hot-path recording writes) and the recording plugin depend on this
crate.

The concrete `RecordingRuntime` type (with file I/O, rolling windows,
etc.) lives in the recording plugin impl crate (`bmux_recording_plugin`);
this crate hosts only the trait abstraction over the write sink plus
shared metadata types core needs to construct payloads.
