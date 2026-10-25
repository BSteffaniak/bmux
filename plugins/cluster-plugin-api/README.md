# bmux_cluster_plugin_api

Stable typed contract for the bmux cluster plugin.

The crate contains BPDL schemas, generated service/client modules, stable wire
types, and contract smoke tests. Runtime membership, gateway policy, federation
state, placement, connection management, and background work belong in
`bmux_cluster_plugin`, not this API crate.

## Interfaces

- `cluster-query/v1` — public node identity with an explicit protocol offer, durable role/capability-bearing member listing, cluster inventory, and health queries
- `cluster-command/v1` — durable initialization; signed enrollment/join/leave phases, node-key possession proof, protocol/schema/feature negotiation, and public membership credentials; current cluster startup and pane mutation commands
- `cluster-peer-auth/v1` — generated challenge/prove/authenticate flow for short-lived, audience-bound, single-use mutual node authentication using active signed membership credentials
- `cluster-connection-events/v1` — persisted connection lifecycle event queries

The source interface names produce idiomatic generated modules while the BPDL
`@interface-version(1)` annotation preserves the existing slash-versioned wire
identifiers.

## Voter transition authorization v2

The generated voter authorization explicitly requires `protocol_version = 2`.
Its signature binds cluster, action, target, signer, canonical complete current
and proposed member digests, the committed Raft membership log epoch, and a
five-minute expiry. Missing v2 fields and unsupported proof versions fail
closed;
old proofs are never inferred or upgraded. Durable control-log formats are
unchanged. Upgrade cluster peers and bundled plugins together before changing
voters; ordinary observers do not need a voter promotion proof.

Enrollment now returns a promotion authorization signed by the current issuer
voter, not by the joining node. Existing join/leave/revoke commands carry these
proofs automatically. A concurrent membership change or elapsed expiry requires
obtaining a fresh proof; replay is rejected even after restart. A retry after a
lost successful join response receives a fresh signed acknowledgement of the
exact current plan. It is quorum-confirmed, requires the target to already be a
committed Raft voter, and performs no metadata or voter-set mutation. Failed
promotion
can leave locally adopted enrollment state; do not treat the local member list
as evidence of committed promotion. Use authoritative cluster status to inspect
recovery. The existing local `cluster consensus reconcile` workflow reconciles
only already committed membership, not uncommitted enrollment proposals.
