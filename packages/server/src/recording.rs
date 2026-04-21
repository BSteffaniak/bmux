//! Moved to `bmux_recording_plugin::recording_runtime`.
//!
//! This file is intentionally empty and orphaned from the crate's
//! module tree. `RecordingRuntime` now lives in the recording plugin
//! crate. Server writes into it via the `RecordingSink` trait looked
//! up through [`bmux_plugin::PluginStateRegistry`], without taking a
//! dependency on the plugin impl crate.
