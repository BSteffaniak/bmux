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
- `cluster events` shows connection lifecycle events (`--format text|json`, `--cluster`, `--target`, `--state`, `--since`, `--limit`)
- `cluster pane new` creates an ad-hoc host-bound pane via the generic pane launch API
- `cluster pane move` relocates a pane to a destination host and retargets pane naming
- `cluster pane retry` relaunches a host-bound pane by inferring target from pane naming convention
- Cluster pane target metadata is persisted in plugin storage for robust move/retry behavior
- Connection lifecycle state (`connecting/retrying/degraded/failed`) is tracked in pane metadata
- `cluster pane retry` supports probe retry policy controls (`--retries`, `--on-failure=abort|continue|prompt`)
- Cluster service interfaces are implemented for query/command/event-list integrations

## Commands (Scaffolded)

- `cluster up`
- `cluster status`
- `cluster doctor`
- `cluster hosts`
- `cluster pane new`
- `cluster pane move`
- `cluster pane retry`

## Services

- **`cluster-query/v1`**
- **`cluster-command/v1`**
- **`cluster-connection-events/v1`**
