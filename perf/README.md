# bmux Perf Benchmarks

Perf coverage is built from three reusable pieces:

1. Runtime code emits phase events through `bmux_perf_telemetry`.
2. `perf/*.toml` manifests define benchmark defaults, profiles, SLOs, and phase reports.
3. `bmux-perf-tools run-benchmark` executes the benchmark and writes one standard artifact.

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

Each benchmark config starts with manifest metadata:

```toml
[benchmark]
name = "core-services"
kind = "core-services"

[defaults]
iterations = 1000
warmup = 100
core_service_limit_ms = 1

[profiles.normal]

[profiles.diagnostic]
loosen_slo = true
```

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

Run manifests directly with:

```sh
bmux-perf-tools run-benchmark --manifest perf/core-services.toml --profile normal
bmux-perf-tools run-benchmark --manifest perf/attach-tab-switch.toml --profile normal --bmux-bin target/debug/bmux
bmux-perf-tools list-benchmarks --dir perf
bmux-perf-tools validate-phase-schema --input /tmp/bmux-benchmark.json
```

The scripts in `scripts/perf-*.sh` are compatibility wrappers around `run-benchmark`.

## Profiles

Profile metadata lives in `perf/profiles.toml`.

- `normal`: user-facing latency validation. Enforce SLOs here.
- `diagnostic`: attribution mode with service, IPC, and storage timing. Do not treat strict SLO failures here as product regressions without a matching normal-mode failure.
- `ci`: stable automated validation with conservative limits.
- `stress`: high-scale exploration and trend artifacts. Do not hard gate by default.

## Adding a Benchmark

1. Add runtime phase events only through shared telemetry helpers.
2. Add a `perf/<benchmark>.toml` manifest with `[benchmark]`, `[defaults]`, `[profiles.*]`, `[limit]`, and `[[reports]]` sections.
3. Prefer implementing the runner in `bmux-perf-tools run-benchmark`; keep shell scripts as wrappers only.
4. Make the runner produce one standard artifact containing `benchmark`, `kind`, `profile`, `scenario`, `events`, `limits`, `latency_ms`, and `raw`.
5. Run a normal-mode benchmark for SLO validation and a diagnostic-mode benchmark only for attribution.

## Registry

| Benchmark         | Manifest                      | Runner Kind         | Normal SLO                                                |
| ----------------- | ----------------------------- | ------------------- | --------------------------------------------------------- |
| Attach tab switch | `perf/attach-tab-switch.toml` | `attach-tab-switch` | `attach.plugin_command` p99 \<= 8ms, retarget p99 \<= 8ms |
| Core services     | `perf/core-services.toml`     | `core-services`     | core service p99 \<= 1ms                                  |
| Plugin hot path   | `perf/plugin-hot-path.toml`   | `plugin-hot-path`   | static service dispatch p99 \<= 5ms                       |

## Maintenance Checklist

- No hardcoded SLO thresholds in runtime code.
- No bespoke marker prefix or JSON schema when an existing `PhaseChannel` works.
- No command-specific fast path added only to satisfy a benchmark.
- No plugin-domain convenience helper added to core architecture.
- No diagnostic timing mode used as the only source for user-facing SLOs.
- Every new benchmark has a manifest and records its artifact path.
- Shell scripts should not duplicate benchmark setup or phase validation logic.
