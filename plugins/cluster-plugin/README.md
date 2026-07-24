# bmux_cluster_plugin

Bundled server clusters plugin for bmux.

## Overview

This crate owns the `bmux.cluster` plugin domain.

Current scope:

- Read-only cluster inventory and health checks (`cluster hosts/status/doctor`)
- Inventory sourced from `[plugins.settings."bmux.cluster"].clusters`
- Target resolution validated against `[connections.targets]`
- Readiness probes delegated to core remote commands (`remote test` / `remote doctor`)
- `cluster up` creates/reuses `cluster-<name>` session and launches host-bound panes
- Partial-start semantics: unhealthy hosts are reported as degraded while healthy hosts still launch
- `cluster up` supports launch failure policy controls (`--retries`, `--on-failure=continue|abort|prompt`)
- `cluster events` shows connection lifecycle events (`--format text|json`, `--cluster`, `--target`, `--state`, `--since`, `--limit`); `--since` accepts `now`/`0`, unix ms, or relative durations like `15m`, `2h`, `1h30m`
- `cluster pane new` creates an ad-hoc host-bound pane via the generic pane launch API
- `cluster pane move` relocates a pane to a destination host and retargets pane naming
- `cluster pane retry` relaunches a host-bound pane by inferring target from pane naming convention
- Cluster pane target metadata is persisted in plugin storage for robust move/retry behavior
- Connection lifecycle state (`connecting/retrying/degraded/failed`) is tracked in pane metadata
- Panes transition to `ready` only after post-launch health verification succeeds
- `cluster pane retry` supports probe retry policy controls (`--retries`, `--on-failure=abort|continue|prompt`)
- `cluster pane new|move|retry` accept `--cluster` for deterministic gateway routing; in multi-cluster layouts, commands hard-fail when cluster inference is ambiguous
- Gateway overrides are available on cluster commands: `--gateway`, `--gateway-mode=auto|direct|pinned`, `--gateway-policy=balanced|aggressive|conservative`, and `--gateway-no-failover`
- Routed cluster commands support `--dry-run` to run real gateway probes/selection without executing the cluster operation itself (`--format text|json`)
- Routed/special gateway commands support `--why` for concise decision summaries in text and JSON (`decision_summary`)
- `cluster gateway status` reports effective gateway mode, candidate order, preferred gateway cache, cooldown state, and selected candidate hint (`--format text|json`)
- `cluster gateway explain` probes candidates and explains why selection would succeed/fail without mutating gateway runtime cache/cooldown state (`--format text|json`)
- `cluster gateway doctor` inspects gateway candidate health and emits actionable findings (`--format text|json`)
- `cluster gateway why` provides one-shot routing diagnosis, top actions, and recent history (`--format text|json`)
- `cluster gateway history` shows recent gateway routing decisions and observations (`--format text|json`, `--since <duration>`, `--limit <count>`, `--result`, `--reason`, `--candidate`, `--command`)
- `cluster gateway history-export` emits machine-friendly history output (`--format json|ndjson`)
- `cluster gateway history-clear` clears history for one cluster or all clusters (`--cluster`/`--all`, filter flags, `--confirm` for broad non-interactive clears)
- `cluster gateway reset` clears persisted gateway runtime state for one cluster (`--cluster`) or all clusters (`--all`)
- In multi-cluster setups, `cluster gateway status|explain` require explicit cluster selection (`--cluster` or positional cluster name)
- Cluster service interfaces are implemented for query/command/event-list integrations

## Failure Semantics

- `cluster up --on-failure=abort` stops launching additional hosts after the first terminal failure but preserves any panes already launched successfully.
- `cluster up --on-failure=continue` keeps launching remaining hosts and reports failed hosts as degraded.
- `cluster up --on-failure=prompt` asks for interactive retry/continue/abort decisions when prompt runtime is available; if unavailable, it safely falls back to abort.

## Gateway Failover Mode

- Cluster commands can execute through a cluster member gateway before orchestration runs. This is intended for topologies where private cluster members are reachable from peers but not directly from the caller machine.
- `gateway_mode` defaults to `auto` for each cluster:
  - `auto`: try gateway candidates in order until one succeeds.
  - `direct`: run locally (no gateway indirection).
  - `pinned`: require a single configured `gateway_target`.
- In `auto`, candidates come from `gateway_candidates` when provided; otherwise they fall back to cluster `targets`.
- If every gateway candidate fails, cluster command execution hard-fails (no silent direct fallback).
- Auto mode tracks persisted last-known-good/cooldown state, per-candidate stability stats, and breaker state.
- Breaker behavior defaults: open after 3 consecutive failures, then half-open retry after cooldown.
- Cooldown is adaptive per candidate (failure streak increases delay up to `cooldown_max_ms`) and resets on successful execution.
- Cooldown handling is reason-class-aware (auth/service denial back off faster than transient network failures) with bounded jitter (`cooldown_jitter_pct`) to reduce herd retries.
- Half-open probation requires consecutive successes (`breaker_half_open_required_successes`) before closing breaker again.
- History retention and size are policy-driven (`history_retention_ms`, `history_max_entries`).
- Policy presets tune breaker/cooldown/probe defaults:
  - `balanced`: current defaults.
  - `aggressive`: faster failover and shorter cooldown.
  - `conservative`: slower failover and longer stabilization windows.
- Candidate ordering in auto mode is stability-first (latency used as tie-break), with explicit skip reasons (`cooldown` / `breaker_open`) visible in status/explain and dry-run output.
- Text output may truncate long candidate names for alignment only; JSON keeps full candidate values.

### Gateway Output Examples

Text table columns are aligned and consistent across `cluster gateway status`, `cluster gateway explain`, and routed command `--dry-run`:

```text
candidate                 preferred stability  breaker    cooldown_ms  ok    reason         latency_ms skip           detail
prod-a                    true      5000       closed     -            true  ok             42         -              gateway command bridge reachable
prod-b                    false     200000     open       29000        false connect        15         breaker_open   connection refused
```

JSON example for `cluster gateway explain --format json`:

```json
{
  "cluster": "prod",
  "command": "cluster-gateway-explain",
  "mode": "auto",
  "no_failover": false,
  "selected_candidate": "prod-a",
  "result": "success",
  "would_mutate": {
    "enabled": false,
    "last_good": false,
    "cooldown": false,
    "breaker": false,
    "persistence_write": false
  },
  "probes": [
    {
      "candidate": "prod-a",
      "preferred": true,
      "stability_score": 5000,
      "breaker_state": "closed",
      "cooldown_ms": null,
      "skip_reason": null,
      "historical_latency_ms": 42,
      "ok": true,
      "reason": "ok",
      "latency_ms": 42,
      "detail": "gateway command bridge reachable"
    }
  ],
  "failures": []
}
```

JSON example for routed command dry-run (`cluster status prod --dry-run --format json`):

```json
{
  "cluster": "prod",
  "command": "cluster-status",
  "mode": "auto",
  "result": "failure",
  "selected_candidate": null,
  "would_mutate": {
    "enabled": false,
    "last_good": false,
    "cooldown": false,
    "breaker": false,
    "persistence_write": false
  },
  "probes": [],
  "failures": []
}
```

Example:

```toml
[plugins.settings."bmux.cluster".clusters.prod]
targets = ["prod-a", "prod-b", "prod-c"]
gateway_mode = "auto"
# optional; defaults to targets when omitted
# gateway_candidates = ["prod-a", "prod-b"]
```

## Commands

- `cluster up`
- `cluster status`
- `cluster doctor`
- `cluster hosts`
- `cluster events`
- `cluster pane new`
- `cluster pane move`
- `cluster pane retry`
- `cluster gateway status`
- `cluster gateway explain`
- `cluster gateway doctor`
- `cluster gateway why`
- `cluster gateway history`
- `cluster gateway history-export`
- `cluster gateway history-clear`
- `cluster gateway reset`

## Services

- **`cluster-query/v1`**
- **`cluster-command/v1`**
- **`cluster-connection-events/v1`**

## Service Contract Notes

- `cluster-query/v1`
  - `list_clusters` returns settings-resolved cluster inventory.
  - `status` returns host states from probe execution (`ready` or `degraded`).
  - Errors use `list_clusters_failed` / `status_failed`.
- `cluster-command/v1`
  - `up` returns session id plus per-host launch status payload.
  - `pane_new`, `pane_retry`, and `pane_move` return operation result payloads with pane/session ids.
  - Errors use `up_failed`, `pane_new_failed`, `pane_retry_failed`, and `pane_move_failed`.
- `cluster-connection-events/v1`
  - `list` returns persisted lifecycle events ring buffer for cluster connections.
  - Errors use `connection_events_list_failed`.
