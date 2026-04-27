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
- Plugin dimensions: `plugin_id`, `command_name`, `operation`, `backend`, `measurement_stage` when applicable.
- Storage dimensions: `plugin_id`, `key`, `value_len`, `cache_hit` when applicable.
- Timings: use `*_us` fields and keep `total_us` for the full phase.
- Stage labels: use `measurement_stage = "setup"`, `"cold"`, or `"warm"` when a benchmark separates setup, first-touch, and warmed paths. Warm reports should carry primary SLOs; cold reports should normally be diagnostic.

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
plugin_timing = true
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

Use `filters = [...]` for multi-dimensional reports:

```toml
[[reports]]
phase = "plugin.command"
field = "total_us"
tags = ["plugin"]
filters = [
  { key = "plugin_id", value = "bmux.example" },
  { key = "command_name", value = "do-work" },
  { key = "measurement_stage", value = "warm" },
]
```

Use `tags` for expensive diagnostic reports so normal runs can skip them unless the script enables the tag.

Run manifests directly with:

```sh
bmux-perf-tools run-benchmark --manifest perf/core-services.toml --profile normal
bmux-perf-tools run-benchmark --manifest perf/codec-payloads.toml --profile normal
bmux-perf-tools run-benchmark --manifest perf/generic-ipc.toml --profile normal
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

## Plugin Benchmark Contract

Plugin-owned benchmark coverage should be added without special host code whenever possible:

1. Emit runtime phases through `bmux_plugin_sdk::perf_telemetry` or `bmux_perf_telemetry` directly.
2. Use `PhaseChannel::Plugin` for plugin command/service internals. The stable host command phase is `plugin.command` with `plugin_id`, `command_name`, and `total_us`.
3. Host-owned cold-start phases use generic names such as `plugin.registry_scan`, `plugin.load`, `plugin.typed_services.collect`, `plugin.lifecycle.activate`, `plugin.command.invoke`, and `plugin.process.invoke`.
4. If a plugin has useful internal attribution, emit namespaced phases such as `bmux.<short-name>.<operation>` and include `plugin_id`, `operation`, and `total_us`.
5. Keep emitters gated by `PhaseChannel::enabled()` or `emit(...)`; never compute expensive diagnostic payloads when the channel is disabled.
6. Add reports to a `perf/*.toml` manifest using `tags = ["plugin"]` for diagnostic-only attribution.
7. Keep SLO limits in the manifest. Runtime/plugin code should not contain benchmark thresholds.
8. Prefer generic setup/previsit/measured stages over benchmark-only command shortcuts. If a cold path matters, report it separately from warmed SLOs.

The benchmark runner forwards all standard phase channels when their profile enables them. Plugin owners should not need bespoke stderr parsing or shell wrappers for phase collection.

## Registry

| Benchmark         | Manifest                      | Runner Kind         | Normal SLO                                                |
| ----------------- | ----------------------------- | ------------------- | --------------------------------------------------------- |
| Attach tab switch | `perf/attach-tab-switch.toml` | `attach-tab-switch` | `attach.plugin_command` p99 \<= 8ms, retarget p99 \<= 8ms |
| Codec payloads    | `perf/codec-payloads.toml`    | `codec-payloads`    | codec payload p99 \<= 0.08ms                              |
| Core services     | `perf/core-services.toml`     | `core-services`     | core service p99 \<= 1ms                                  |
| Generic IPC       | `perf/generic-ipc.toml`       | `generic-ipc`       | ping/invoke p99 \<= 5ms                                   |
| Plugin hot path   | `perf/plugin-hot-path.toml`   | `plugin-hot-path`   | static service dispatch p99 \<= 5ms                       |

## Maintenance Checklist

- No hardcoded SLO thresholds in runtime code.
- No bespoke marker prefix or JSON schema when an existing `PhaseChannel` works.
- No command-specific fast path added only to satisfy a benchmark.
- No plugin-domain convenience helper added to core architecture.
- No diagnostic timing mode used as the only source for user-facing SLOs.
- Every new benchmark has a manifest and records its artifact path.
- Shell scripts should not duplicate benchmark setup or phase validation logic.
