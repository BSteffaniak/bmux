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
