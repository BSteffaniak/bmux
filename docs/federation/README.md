# bmux Federation Architecture Decisions

This directory contains the accepted architecture decisions for native bmux server federation. Together they define the product semantics and implementation invariants that must be preserved while executing `local-server-cluster-federation-progress.md`.

## Decision set

| ADR                                                         | Subject                                                     | Status   |
| ----------------------------------------------------------- | ----------------------------------------------------------- | -------- |
| [ADR-0001](adr-0001-federated-workspace-model.md)           | Federated workspace and user-visible model                  | Accepted |
| [ADR-0002](adr-0002-consistency-quorum-leases.md)           | Consistency, quorum, leader forwarding, leases, and fencing | Accepted |
| [ADR-0003](adr-0003-logical-execution-identity.md)          | Logical/execution identity, generations, and idempotency    | Accepted |
| [ADR-0004](adr-0004-worker-loss-and-restart.md)             | Worker loss, recovery, replacement, and restart policy      | Accepted |
| [ADR-0005](adr-0005-attach-resume-and-output-repair.md)     | Attach resume, output cursors, and snapshot repair          | Accepted |
| [ADR-0006](adr-0006-trusted-domain-security.md)             | Trusted-domain membership, delegation, and authorization    | Accepted |
| [ADR-0007](adr-0007-protocol-compatibility-and-upgrades.md) | Protocol compatibility and rolling upgrades                 | Accepted |
| [ADR-0008](adr-0008-reliability-and-performance-budgets.md) | Reliability and performance budgets                         | Accepted |

## Global invariants

01. A federated workspace is one logical workspace even when its pane executions are distributed across workers.
02. Logical identity is independent of ingress, leader, worker, and worker-local runtime identity.
03. Control metadata is quorum replicated; terminal input/output and full terminal state are not written to the consensus log.
04. A minority partition cannot commit control mutations.
05. Gateway or leader loss does not imply worker process loss.
06. A worker accepts mutations only for the current execution generation and valid fencing authority.
07. Duplicate delivery of the same command ID has one logical outcome.
08. Worker loss degrades safely; replacement is explicit or allowed by durable pane policy.
09. Existing local sessions enter federation only through explicit promotion.
10. Local single-server behavior remains available when federation is absent or disabled.
11. Cluster-domain behavior remains in cluster plugins. Core crates expose generic transport, typed-dispatch, attach-provider, and protocol primitives only.

## Terminology

- **Cluster:** Authenticated set of bmux nodes sharing replicated control metadata.
- **Node:** One persistent cluster identity, normally represented by one bmux server installation.
- **Voter:** Node participating in control-plane consensus.
- **Worker:** Node capable of hosting pane executions.
- **Ingress:** Node currently accepting a client connection and composing the attached view.
- **Workspace:** Logical federated session containing windows, layout, and logical panes.
- **Logical pane:** Durable pane identity and policy independent of where a process executes.
- **Execution:** One concrete worker-hosted realization of a logical pane.
- **Generation:** Monotonic logical-pane generation fencing replacement executions.
- **Control revision:** Ordered committed state-machine revision.
- **Output cursor:** Worker-local monotonic byte-stream position for one execution generation.
- **Command ID:** Caller-generated idempotency identity for one logical mutation.
- **Control lease:** Short-lived authority allowing narrowly scoped worker mutations while the issuing control term remains valid.
- **Promotion:** Explicit transaction adopting an existing local session into a federated workspace.

## Architecture boundary review

The decision set was reviewed against `AGENTS.md` on 2026-07-23.

### Plugin-owned domains

The following belong in `plugins/cluster-plugin/**` and stable contracts in `plugins/cluster-plugin-api/**`:

- Membership and node roles
- Consensus state machine and cluster persistence
- Workspaces, logical windows, logical panes, and placement
- Execution assignment, generations, restart policy, and recovery
- Cluster ingress/gateway selection and diagnostics
- Promotion, cluster authorization orchestration, and audit events
- Cluster attach composition and cluster-specific resume interpretation

Transport target resolution and endpoint connection pooling should be owned by a neutral plugin domain such as `bmux.connections`, not by core and not by cluster-specific CLI branches.

### Permitted core primitives

Core architecture may provide only generic mechanisms:

- Endpoint-addressed typed-service invocation
- Generic authenticated byte/stream transports
- Attach-provider registration and neutral attach snapshot/delta contracts
- Cancellation, deadlines, backpressure, logging, recording, and storage primitives
- Neutral identifier newtypes required by generic protocols

Core interfaces must not expose cluster IDs, membership, workspace placement, logical panes, worker policy, or gateway policy. `HostRuntimeApi` remains limited to its approved generic primitives.

### Plugin API boundary

`cluster-plugin-api` may contain BPDL schemas, generated clients/services/events, stable wire/model types, schema tests, and intentional neutral re-exports. It must not contain consensus state, runtime managers, connection pools, registries, lifecycle behavior, background tasks, runtime IO, permission decisions, or handwritten public transport clients.

## Change discipline

When implementation contradicts an accepted ADR, do not silently work around it. Amend or supersede the ADR, record the reason and migration consequences, then update the local progress document. Open implementation choices that do not change these semantics may be resolved without superseding an ADR.
