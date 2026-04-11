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

Cluster pane mutation helpers (`cluster pane ...`) remain stubbed for follow-up slices.

## Commands (Scaffolded)

- `cluster up`
- `cluster status`
- `cluster doctor`
- `cluster hosts`
- `cluster pane new`
- `cluster pane move`
- `cluster pane retry`

## Services (Scaffolded)

- **`cluster-query/v1`**
- **`cluster-command/v1`**
- **`cluster-connection-events/v1`**
