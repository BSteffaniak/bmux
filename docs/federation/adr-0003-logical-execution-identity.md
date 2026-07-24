# ADR-0003: Logical Identity, Execution Generations, and Idempotent Commands

- **Status:** Accepted
- **Date:** 2026-07-23

## Context

Local bmux runtime IDs identify objects within one server process. Federation needs identities that remain stable when a client changes ingress, a leader changes, a worker restarts, or a pane is replaced on another worker. Retried distributed operations also need a stable identity separate from transport request IDs.

Conflating logical panes with worker-local pane IDs would either expose placement to users or allow stale workers to become authoritative after replacement.

## Decision

Use separate global logical identities, concrete execution identities, monotonic generations, and command idempotency identities.

## Identifier model

All durable federation identifiers are opaque, globally unique typed values. The initial representation is UUID-compatible 128-bit values generated with a cryptographically secure source. Contracts must not expose untyped strings where a typed BPDL UUID/newtype is available.

- `ClusterId`: persistent cluster identity, created once at cluster initialization.
- `NodeId`: persistent node identity bound to node credentials, never reused by a replacement installation.
- `WorkspaceId`: durable logical workspace identity.
- `LogicalWindowId`: durable logical window identity within a workspace.
- `LogicalPaneId`: durable logical pane identity independent of placement.
- `ExecutionId`: unique identity for one concrete worker execution.
- `CommandId`: caller-generated identity for one logical mutation.
- `PromotionId`: identity for one local-session promotion transaction.

Names are mutable display/reference metadata, not identity. Name resolution either returns one unambiguous identity or a structured ambiguity/not-found error.

## Execution generation

Each logical pane has a `u64` `ExecutionGeneration`:

- Generation begins at `1` when the first execution assignment is committed.
- Any replacement that could coexist with an earlier process increments generation before the replacement receives input.
- Restarting the same command after process exit creates a new execution ID and generation.
- Reconnecting to the same still-running execution does not increment generation.
- Re-adopting an execution after worker/plugin restart does not increment generation if identity and process continuity are proven and no replacement was committed.
- Generation overflow is a terminal integrity error; it must not wrap.

The authoritative tuple is:

```text
(LogicalPaneId, ExecutionGeneration, ExecutionId, NodeId)
```

A logical pane has zero or one authoritative tuple in committed state. Zero represents pending, unavailable without an assigned execution, or permanently exited according to policy.

## Worker-local identity

The execution record may refer to worker-local session, context, and pane IDs. Those values are routing details and must not be used as global identities, durable user references, or command idempotency keys.

Workers persist enough execution metadata to reconcile cluster execution IDs to local runtime state after restart. If process continuity cannot be proven, the worker reports `unknown`/`missing`; it does not fabricate continuity.

## Command idempotency

Every externally initiated control mutation receives a `CommandId` before first transmission. The key is:

```text
(OriginatingPrincipalId, CommandId)
```

The replicated command outcome stores:

- Operation family and canonical request digest
- Committed control revision
- Success response or stable domain error
- Creation/completion status where the operation spans worker side effects
- Retention/compaction metadata

Rules:

1. Repeating the same key and same canonical request returns the recorded result or resumes its recorded workflow.
2. Repeating the key with a different canonical request is rejected as `command_id_conflict`.
3. Transport request IDs are not command IDs.
4. Internal retries preserve the originating command ID and derive deterministic sub-operation IDs.
5. Worker launch/adopt/close operations are idempotent by deterministic sub-operation identity plus execution tuple.
6. The deduplication result is snapshotted with consensus state.
7. Clients may query command outcome when delivery status is unknown.

The minimum deduplication retention is **24 hours after terminal outcome** and must be configurable upward. Active/incomplete workflows are never evicted. Destructive or externally side-effecting workflow records may retain compact tombstones longer. Reducing the guaranteed window requires a compatibility review because clients use it for safe retry.

## Multi-step side effects

Control state and worker process creation cannot be one storage transaction. Use an explicit workflow state machine, for example:

```text
requested -> assignment_committed -> worker_prepared -> active
                                  \-> failed
active -> replacement_committed -> replacement_active -> old_fenced
```

Every transition is idempotent. Recovery scans non-terminal workflows and reconciles them using execution identity and generation. A worker may prepare an execution before activation, but it must not accept user input until authoritative activation/fencing state is established.

## Stale-worker handling

When a worker reports an execution:

- If its tuple matches committed state, it may resume after authority validation.
- If generation is lower than committed state, it is stale and must be quarantined from input; policy may terminate it.
- If generation is higher than committed state, it is invalid/corrupt and must not be adopted automatically.
- If execution ID differs at the current generation, both are quarantined pending operator-visible reconciliation; the committed tuple remains authoritative.
- If the logical pane no longer exists, the execution is orphaned and handled by an explicit orphan policy, never silently published.

## ID exposure

CLI and diagnostics may accept short unique prefixes for convenience, but wire contracts and durable state use full typed IDs. Ambiguous prefixes fail. Logs include cluster, workspace, logical pane, execution, generation, node, command, and revision fields where relevant.

## Consequences

### Positive

- Logical references survive all expected routing and placement changes.
- Old workers cannot reclaim authority merely by reconnecting.
- Lost responses can be retried without duplicate logical outcomes.
- Recovery of multi-step operations is explicit and testable.

### Costs

- Every execution operation carries more identity/fencing metadata.
- Deduplication records require retention and compaction policy.
- Side-effect workflows need reconciliation instead of simple request/response code.

## Rejected alternatives

- **Use worker pane UUID as logical ID:** rejected because replacement and promotion would leak or change identity.
- **Use names as identity:** rejected because names are mutable and may be ambiguous.
- **Infer duplicates from request contents:** rejected because identical legitimate requests may occur and request timing is unreliable.
- **Increment generation only after replacement launches:** rejected because the old execution could remain authorized during launch.
- **Adopt the highest generation reported by workers:** rejected because workers are not the authority for committed placement.

## Architecture placement

Federation ID types and wire records belong in `cluster-plugin-api`; generation, workflow, deduplication, and reconciliation runtime behavior belongs in `cluster-plugin`. Generic core request IDs remain unchanged and are not overloaded with cluster semantics.

## Acceptance criteria

Tests must prove stable logical identity across ingress/leader/worker changes, at-most-one logical outcome for repeated command delivery, conflict rejection for reused command IDs, deterministic recovery of interrupted workflows, and rejection/quarantine of every stale or inconsistent execution tuple.
