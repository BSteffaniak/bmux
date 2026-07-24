# ADR-0007: Federation Protocol Compatibility and Rolling Upgrades

- **Status:** Accepted
- **Date:** 2026-07-23

## Context

Federation adds durable consensus state, typed cluster services, peer RPC, worker execution protocols, and attach resume data. Nodes will not always upgrade simultaneously. An incompatible voter can prevent progress or corrupt durable state if schema evolution is implicit.

bmux already negotiates a core protocol contract and BPDL interfaces are versioned. Federation must build on those mechanisms without coupling cluster release evolution to undocumented Rust serialization layouts.

## Decision

Version federation at four explicit layers and use capability-gated rolling upgrades:

1. **Transport/core wire epoch:** existing bmux connection framing compatibility.
2. **Cluster peer protocol epoch and revision range:** consensus peer, membership, and internal RPC compatibility.
3. **BPDL interface versions/features:** membership, workspace, worker, attach, gateway, discovery, and security contracts.
4. **Durable state-machine schema version:** consensus command/snapshot interpretation.

A node participates only in roles for which all mandatory layers are compatible.

## Compatibility rules

### Wire epochs

An epoch change denotes no safe direct interoperability. Peers with different mandatory epochs fail before membership or state mutation with an actionable incompatibility report. Epoch downgrade is never silently attempted.

### Revision ranges

Within one epoch, peers advertise inclusive supported revision ranges and feature bits. They select the highest mutually supported revision. Unknown optional fields use explicit defaults and must not change old semantics. Unknown required variants/features cause rejection.

### BPDL interfaces

- Existing interface versions are immutable once released.
- Additive compatible operations/fields follow BPDL compatibility rules and defaulting tests.
- Breaking semantics use a new interface version.
- Public plugin API crates use generated clients; no compatibility layer reintroduces handwritten envelopes.
- Internal interfaces are marked unstable but still versioned and negotiated because rolling nodes use them.

### Durable schema

- Every log command and snapshot has an explicit schema version independent of crate version.
- A voter must be able to apply every command the current leader may propose and install the current snapshot format.
- Durable migrations are deterministic state-machine operations or offline/controlled transformations with rollback rules.
- A binary never starts as a voter if local durable state is newer than it understands.
- Downgrade is allowed only while durable state and active feature floor remain supported by the old binary.

## Cluster feature floor

Replicated membership tracks:

- Each node's supported protocol/schema/features
- A cluster **read floor** required to observe current state
- A cluster **write floor** required to propose/apply current commands
- Activated feature gates

A feature that changes replicated state or worker authority activates only after all current voters support its write/apply semantics and required serving nodes support their role-specific interface. Activation is a committed command. Installing new binaries alone does not change state format.

Once a feature writes state that old versions cannot understand, the cluster floor advances and downgrade below it is rejected until an explicit reversible deactivation/migration exists.

## Rolling upgrade procedure

1. Run compatibility preflight and verify quorum/headroom.
2. Upgrade non-voter workers/observers first where possible.
3. Upgrade voters one at a time, preserving quorum and waiting for catch-up/health.
4. Upgrade remaining ingresses/workers.
5. Verify every voter advertises the new capability.
6. Commit feature activation/schema floor advancement separately.
7. Keep old code paths until the documented compatibility window expires and downgrade policy is clear.

Leader election must not select a node unable to apply the current cluster write floor. Incompatible nodes remain non-serving/rejected rather than partially participating.

## Supported rolling window

Before the first stable federation release, compatibility is guaranteed by explicit protocol/schema versions rather than semantic-version labels. At stable release, bmux will support rolling operation between **two adjacent released federation protocol revisions within the same epoch**, provided the cluster feature floor has not advanced beyond the older revision.

This is a minimum guarantee, not permission to infer compatibility from package versions. Tests use protocol fixtures and mixed binaries/revision adapters.

## Attach resume compatibility

Resume descriptors include provider protocol revision and feature requirements. A new ingress either:

- Resumes with a mutually compatible revision,
- Performs a clean snapshot reopen while preserving logical workspace identity, or
- Rejects with an actionable incompatibility error.

It never applies an unknown cursor/snapshot representation optimistically.

## Join and membership changes

Join preflight reports:

- Local and cluster wire epochs
- Peer protocol revision intersection
- BPDL mandatory interface support
- Durable schema read/write support
- Role-specific missing features
- Whether the node may join as observer, worker, ingress, or voter

A node may join in a restricted role if that role is safe, but voter promotion requires full current voter compatibility.

## Consequences

### Positive

- Upgrades and state activation are separate, reducing irreversible surprises.
- Mixed-version behavior is testable from explicit contracts.
- Old nodes cannot corrupt newer durable state.
- Resume can fall back to clean snapshot repair without changing logical identity.

### Costs

- Multiple protocol paths remain during the compatibility window.
- Feature activation and downgrade require operational discipline.
- Durable commands/snapshots need fixtures and migration tests.

## Rejected alternatives

- **Use crate/package version as compatibility:** rejected because it does not describe wire or state semantics.
- **Serde Rust structs without explicit schema version:** rejected because field/variant evolution would be implicit.
- **Activate features when the leader upgrades:** rejected because followers may be unable to apply entries.
- **Require full-cluster stop-the-world upgrades:** rejected as the normal path because it conflicts with HA goals.
- **Best-effort downgrade after new state is written:** rejected because silent misinterpretation is worse than refusal.

## Architecture placement

Core wire negotiation remains generic. Federation-specific peer revisions, feature floors, BPDL versions, and durable schema live in cluster API/implementation crates. Core cannot encode cluster upgrade policy.

## Acceptance criteria

Tests must cover adjacent mixed revisions, incompatible epochs, old/new snapshot install, feature activation only after voter support, voter promotion rejection, clean attach reopen on resume incompatibility, one-node-at-a-time upgrade, and prohibited downgrade after floor advancement.
