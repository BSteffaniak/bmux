# bmux_recording_protocol

Neutral recording protocol DTOs for bmux.

This crate defines shared recording wire types and frame helpers used by bmux
recording writers, readers, exporters, and runtime integrations. It is a
protocol/model crate only; concrete recording runtime state and file I/O live in
runtime or plugin implementation crates.
