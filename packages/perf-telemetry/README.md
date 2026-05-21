# bmux_perf_telemetry

Runtime performance telemetry helpers for bmux.

This crate provides neutral phase-timing primitives used by bmux runtime and
plugin code to emit structured performance telemetry. It owns shared helpers for
phase channels, payload construction, buffering, filtering, and marker parsing.

It is intentionally runtime-agnostic and does not contain plugin activation,
product orchestration, or concrete host state.
