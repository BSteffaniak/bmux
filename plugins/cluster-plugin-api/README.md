# bmux_cluster_plugin_api

Stable typed contract for the bmux cluster plugin.

The crate contains BPDL schemas, generated service/client modules, stable wire types, and contract smoke tests. Runtime membership, gateway policy, federation state, placement, connection management, and background work belong in `bmux_cluster_plugin`, not this API crate.

## Interfaces

- `cluster-query-v1` — cluster inventory and health queries
- `cluster-command-v1` — current cluster startup and pane mutation commands
- `cluster-connection-events-v1` — persisted connection lifecycle event queries

The `-v1` suffix is part of the BPDL interface identifier. The cluster implementation temporarily accepts the legacy pre-BPDL `/v1` interface aliases during migration, but generated consumers use these canonical identifiers.
