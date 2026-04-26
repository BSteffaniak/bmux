# bmux Perf Benchmarks

Perf coverage is built from three reusable pieces:

1. Runtime code emits phase events through `bmux_perf_telemetry`.
2. Benchmark scripts create fixtures and collect one artifact.
3. `bmux-perf-tools validate-phase-config` reads a TOML config and writes per-report summaries.

## Phase Events

Use `bmux_perf_telemetry::PhasePayload` for new runtime markers. Do not hand-build marker JSON in each caller unless the payload is genuinely ad hoc.

Standard channels:

- `PhaseChannel::Plugin` uses `BMUX_PLUGIN_PHASE_TIMING` and `[bmux-plugin-phase-json]`.
- `PhaseChannel::Attach` uses `BMUX_ATTACH_PHASE_TIMING` and `[bmux-attach-phase-json]`.
- `PhaseChannel::Service` uses `BMUX_SERVICE_PHASE_TIMING` and `[bmux-service-phase-json]`.
- `PhaseChannel::Ipc` uses `BMUX_IPC_PHASE_TIMING` and `[bmux-ipc-phase-json]`.
- `PhaseChannel::Storage` uses `BMUX_PLUGIN_STORAGE_PHASE_TIMING` and `[bmux-storage-phase-json]`.

Common fields should keep stable names:

- Service dimensions: `capability`, `kind`, `interface_id`, `operation`.
- IPC dimensions: `request`, `request_id`, `response`.
- Plugin dimensions: `plugin_id`, `backend`.
- Storage dimensions: `plugin_id`, `key`, `value_len`, `cache_hit` when applicable.
- Timings: use `*_us` fields and keep `total_us` for the full phase.

## Configs

Benchmark SLOs and reports belong in `perf/*.toml`, not runtime code and not shell loops.

Each report selects a phase and numeric field:

```toml
[limit]
core_service = 1

[[reports]]
phase = "core_service"
field = "total_us"
limit = "core_service"
filter = { key = "scenario", value = "storage.cached_get" }
```

Use `tags` for expensive diagnostic reports so normal runs can skip them unless the script enables the tag.

## Profiles

Profile metadata lives in `perf/profiles.toml`.

- `normal`: user-facing latency validation. Enforce SLOs here.
- `diagnostic`: attribution mode with service, IPC, and storage timing. Do not treat strict SLO failures here as product regressions without a matching normal-mode failure.
- `ci`: stable automated validation with conservative limits.
- `stress`: high-scale exploration and trend artifacts. Do not hard gate by default.

## Adding a Benchmark

1. Add runtime phase events only through shared telemetry helpers.
2. Make the runner produce one JSON artifact containing phase events, either marker logs parsed by `bmux-perf-tools` or an `events` array.
3. Add a `perf/<benchmark>.toml` config with reports and limits.
4. Keep the shell script focused on fixture setup, command execution, and one `validate-phase-config` call.
5. Run a normal-mode benchmark for SLO validation and a diagnostic-mode benchmark only for attribution.

## Maintenance Checklist

- No hardcoded SLO thresholds in runtime code.
- No bespoke marker prefix or JSON schema when an existing `PhaseChannel` works.
- No command-specific fast path added only to satisfy a benchmark.
- No plugin-domain convenience helper added to core architecture.
- No diagnostic timing mode used as the only source for user-facing SLOs.
- Every new benchmark has a config file and records its artifact path.
