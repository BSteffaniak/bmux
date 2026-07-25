# ADR-0012: Worker Output Transport and Federated Attach Ownership

- **Status:** Accepted
- **Date:** 2026-07-25

## Context

Federated attach must compose one native workspace from control metadata and terminal streams owned by several workers. The generic attach seam is intentionally domain-agnostic, while cluster routing, execution generations, output cursors, snapshot repair, and failover policy are cluster concepts. The worker transport must remain bounded and must not require a new core streaming protocol before correctness is established.

## Decision

### Attach adapter ownership

The federated attach adapter lives in `cluster-plugin` as a plugin-owned provider of the existing generic attach contract. It may use private helper modules and generated `cluster-plugin-api` clients. If native rendering integration later requires a paired renderer artifact, that artifact remains an implementation detail of the cluster plugin and does not own policy or stable transport envelopes.

Core attach code receives only neutral provider snapshots, deltas, controls, resume state, and status. It does not parse `cluster://`, choose members, inspect placement, or understand executions.

### Worker output transport

Version 1 uses bounded long-poll queries through generated `cluster-worker-state/v1` services:

- `output(execution_id, generation, cursor, max_bytes)` returns a bounded contiguous batch, retained-range metadata, a next cursor, and whether output remains immediately pending.
- Each request is bound to exactly one execution generation. A generation change requires control reconciliation and a fresh snapshot.
- A cursor older than retained output returns an explicit gap/retained-start result and triggers `snapshot` repair; the server never fabricates continuity.
- Workers cap `max_bytes`, retained bytes per execution, concurrent polls, and queued response work. Empty polls use a bounded server wait and client cancellation/deadline.
- Ingress polls independent workers concurrently with bounded fan-out so one slow worker cannot block unrelated panes.
- Terminal bytes and snapshots never enter consensus.

Long-poll is the required first implementation because it composes with the existing endpoint-aware typed-service path, its cancellation and retry boundaries are explicit, and it is straightforward to test for bounded memory and cursor repair. A future multiplexed streaming revision may be negotiated as an optional feature only after parity tests prove identical generation, cursor, cancellation, and backpressure semantics. Streaming is not required for the first release and cannot silently replace the v1 contract.

## Consequences

- Cluster policy and URI handling stay out of core architecture.
- The first implementation can prioritize correctness and bounded behavior without introducing a second transport stack.
- Poll overhead may be higher than a mature multiplexed stream; performance budgets determine whether a later optional revision is justified.
- Worker output and attach implementation can be tested using generated service clients and the synthetic generic attach-provider harness.

## Acceptance criteria

1. Architecture guardrails find no cluster URI, membership, placement, or execution concepts in core attach layers.
2. Cursor-gap tests prove repair through a complete snapshot and no output is applied across generations.
3. Slow-worker tests prove bounded polling fan-out, queues, retained output, and cancellation.
4. Local and cluster attach providers coexist, and disabling the cluster plugin preserves baseline attach behavior.
5. Any future streaming revision has explicit capability negotiation and behavioral parity tests with v1 long-poll.
