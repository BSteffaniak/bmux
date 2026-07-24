# bmux_cluster_plugin_api

Stable typed contract for the bmux cluster plugin.

The crate contains BPDL schemas, generated service/client modules, stable wire types, and contract smoke tests. Runtime membership, gateway policy, federation state, placement, connection management, and background work belong in `bmux_cluster_plugin`, not this API crate.

## Interfaces

- `cluster-query/v1` — public node identity with an explicit protocol offer, durable role/capability-bearing member listing, cluster inventory, and health queries
- `cluster-command/v1` — durable initialization; signed enrollment/join/leave phases, node-key possession proof, protocol/schema/feature negotiation, and public membership credentials; current cluster startup and pane mutation commands
- `cluster-peer-auth/v1` — generated challenge/prove/authenticate flow for short-lived, audience-bound, single-use mutual node authentication using active signed membership credentials
- `cluster-connection-events/v1` — persisted connection lifecycle event queries

The source interface names produce idiomatic generated modules while the BPDL
`@interface-version(1)` annotation preserves the existing slash-versioned wire
identifiers.
