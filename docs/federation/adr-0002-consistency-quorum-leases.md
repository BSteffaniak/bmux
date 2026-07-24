# ADR-0002: Quorum Control Plane, Leader Forwarding, Leases, and Fencing

- **Status:** Accepted
- **Date:** 2026-07-23

## Context

Federation requires authoritative membership, workspace, layout, placement, and execution-generation metadata even when clients enter through different nodes. Allowing several nodes to mutate this state independently would permit duplicate executions, stale input routing, conflicting layouts, and unsafe membership changes.

Pane processes and terminal streams must remain available independently from control-leader lifetime. Therefore control metadata consistency and worker data flow require different mechanisms.

## Decision

Use a vetted Raft-family consensus implementation to order durable cluster control mutations. Do not implement a custom consensus algorithm.

### Roles

- **Voter:** stores the consensus log/state and participates in election and quorum.
- **Worker:** hosts pane executions; it may also be a voter.
- **Ingress:** accepts clients and forwards authoritative operations; it may also be a voter and/or worker.
- **Observer/edge:** receives cluster information and may serve ingress traffic without voting, subject to the selected consensus library's supported learner model.

A production high-availability cluster requires at least three voters. One-voter mode may be supported for development or personal use with an explicit non-HA warning. Two voters provide no single-voter-failure write availability and must also warn.

### Write path

1. The caller creates a stable `CommandId` before first delivery.
2. Any ingress accepts the request after authentication and preliminary authorization.
3. A non-leader forwards the request to the current leader, preserving caller identity, deadline, and command ID.
4. The leader validates the command against current state and proposes a deterministic state-machine command.
5. Success is reported only after the entry and its result are durably committed according to consensus guarantees.
6. The deduplication outcome is part of replicated state. A retry with the same command ID and principal returns the same logical outcome.

A response lost after commit yields an unknown transport outcome, not permission to mint a new command ID. The client resolves or retries the original command.

### Read classes

Every query contract declares one of these semantics:

- **Linearizable:** membership authority, permission/revocation authority, current execution generation, mutation result, and any read used to authorize a mutation.
- **Committed bounded-stale:** workspace inventory, layout snapshots, placement explanations, and diagnostics where the response includes its control revision and staleness/leader information.
- **Local observational:** transport latency, heartbeat samples, local worker resource data, and debug history; never used alone to authorize or fence a mutation.

There is no unmarked eventual-consistency mode.

### Quorum loss

Without quorum:

- No membership, workspace/layout, placement, promotion, restart, generation, permission-authority, or durable policy mutation may commit.
- Worker processes continue running.
- Ingress may display committed snapshots and worker output with an explicit `control_read_only` state.
- Reads carry the last committed revision and freshness status.
- Operations requiring current authority fail with a structured quorum-unavailable response.

### Control leases

Workers must not accept indefinite mutation authority from a disconnected or deposed ingress. The leader may issue short-lived, signed/authenticated control leases scoped to:

- Cluster and workspace
- Logical pane and active execution generation
- Allowed operation class, such as input/resize versus lifecycle mutation
- Control term and lease sequence
- Principal/delegation identity
- Audience worker
- Issued-at and monotonic expiration information

Lease rules:

- Lifecycle operations that create, replace, adopt, or close executions require current leader/quorum authorization and are never authorized solely by an expired or cached lease.
- Interactive input and resize may continue during a brief control interruption only under an unexpired lease.
- A worker tracks the highest accepted term and lease sequence for each execution and rejects older authority.
- A lease cannot extend beyond the configured maximum without leader renewal.
- Wall-clock time alone is not trusted for ordering. Lease validation uses authenticated issuance plus bounded duration and a monotonic local deadline established on receipt.
- Node restart invalidates volatile lease acceptance state unless safely restored with current term validation.

The initial maximum interactive control lease is **5 seconds**. This is short enough to bound stale control and long enough to bridge ordinary leader election. Phase 11 tests may justify a lower value; increasing it requires an ADR amendment because it expands split-brain exposure.

### Fencing

Every worker mutation includes:

- `ClusterId`
- `WorkspaceId`
- `LogicalPaneId`
- `ExecutionId`
- `ExecutionGeneration`
- Committed control term/revision or valid lease
- `CommandId` where the operation is idempotent/mutating

The worker rejects:

- A generation below or above its registered active execution
- A different execution ID for the same generation
- A term below its highest authoritative term
- An expired, wrong-audience, wrong-scope, or replayed lease
- A lifecycle mutation lacking current committed authority

### Membership safety

Voter-set changes use the selected library's safe membership-change protocol, normally joint consensus. Nodes do not become voters merely because they possess a transport connection. Removal/revocation and consensus membership are coordinated so a removed node cannot regain control authority with stale credentials.

## State-machine requirements

- Commands are deterministic and contain all nondeterministic decisions made before proposal.
- Application never reads local clocks, random sources, network state, or worker state while applying a committed entry.
- Each applied entry advances a monotonic control revision.
- Command deduplication keys include principal identity to prevent one principal from resolving another's command ID.
- Snapshots include deduplication state for the documented retention horizon.
- Terminal output, heartbeat samples, connection latency, and local resource observations are excluded.

## Consequences

### Positive

- One authoritative mutation order prevents divergent workspace and generation state.
- Leader failure does not terminate worker processes.
- Short leases permit brief interactive continuity while bounding stale input.
- Explicit read classes prevent stale diagnostics from becoming accidental authority.

### Costs

- Three voters are needed for meaningful one-node-failure write availability.
- Quorum loss intentionally rejects control mutations.
- Lease, term, and generation validation adds metadata to worker operations.
- Consensus storage and recovery become critical infrastructure inside the cluster plugin.

## Rejected alternatives

- **Last-writer-wins replicated maps:** rejected because execution creation and input authority cannot be conflict-resolved safely after the fact.
- **Writable minority partitions:** rejected because duplicate process side effects and competing terminal input are not generally reconcilable.
- **Permanent coordinator without consensus:** rejected because it does not survive coordinator loss.
- **Put terminal streams in Raft:** rejected due to latency, volume, log growth, and unnecessary coupling.
- **Unlimited cached ingress authority:** rejected because stale ingresses could control superseded executions.

## Architecture placement

Consensus state machine, leases, membership, and fencing policy live in `cluster-plugin`. Peer transport may use generic endpoint/stream primitives. No consensus- or cluster-specific variants are added to core IPC solely for federation; typed plugin services carry cluster RPC.

## Acceptance criteria

A three-voter test cluster must repeatedly survive leader termination, preserve committed state, reject minority writes, deduplicate response-loss retries, fence stale terms/generations, and keep healthy worker processes running throughout.