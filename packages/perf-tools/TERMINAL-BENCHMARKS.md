# Existing performance scripts and portable terminal benchmarks

`bmux-perf-tools` retains its existing manifest, artifact, and SLO contracts for
service/runtime scripts. The new `bmux_bench` library and `bmux-bench` frontend
own portable terminal experiments and bounded CPU-stage diagnostics.

Do not feed legacy `schema_version: 1` performance JSON into `bmux-bench
compare`:
the integer alone does not identify an artifact family. A portable bundle has
`run.json`, `samples.jsonl`, `summary.json`, `report.md`, and its checksum
manifest.
Unknown or mismatched formats are rejected, never silently converted.

Use `bmux-bench run --internal` for parser/grid, snapshot, plain-text TUI paint,
and encoding diagnosis. Continue using existing scripts for service, plugin,
IPC, and attachment telemetry. Those metrics have different boundaries and are
not a portable terminal score. See [the benchmark frontend](../bench-cli/README.md).
