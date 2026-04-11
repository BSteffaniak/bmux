# bmux_cluster_plugin

Bundled server clusters plugin for bmux.

## Overview

This crate scaffolds the `bmux.cluster` plugin domain. PR1 only wires command
and service surfaces so the plugin can be bundled and discovered. Behavioral
cluster orchestration is implemented in follow-up slices.

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
