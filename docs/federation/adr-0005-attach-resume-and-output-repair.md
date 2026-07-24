# ADR-0005: Federated Attach Resume, Output Cursors, and Snapshot Repair

- **Status:** Accepted
- **Date:** 2026-07-23

## Context

A federated attach combines control metadata and terminal data from executions on multiple workers. The client may reconnect through another ingress after transport or ingress failure. Terminal output can continue while disconnected, buffers are bounded, and independently sourced streams cannot share one global byte offset.

The design must avoid putting terminal data in consensus while preserving correct rendering, input fencing, and bounded resource use. Existing neutral attach/layout/scene/image protocols should be reused.

## Decision

Use a two-level resume model:

1. A **control revision** orders committed workspace/layout/execution-assignment state.
2. A per-`(ExecutionId, ExecutionGeneration)` **output cursor** orders terminal output bytes/events produced by that worker execution.

A client resumes by reconciling control state first, then each visible execution stream. Cursor gaps repair from a complete terminal snapshot rather than replaying through consensus.

## Attach lifecycle

### Open

1. Resolve `cluster://` to the cluster attach provider and an authenticated ingress.
2. Authenticate the principal and resolve workspace identity.
3. Obtain an initial committed workspace snapshot at control revision `R`.
4. For each visible active execution, obtain a terminal snapshot with generation and stream cursor watermark.
5. Subscribe to control deltas after `R` and execution output after each snapshot watermark.
6. Apply buffered deltas only after validating revision, execution identity, and generation.

### Resume

The client retains a resume descriptor containing:

- Cluster and workspace IDs
- Last fully applied control revision
- Client/view identity needed for permissions/follow behavior
- For each relevant logical pane: execution ID, generation, last applied output cursor, and last snapshot revision/hash metadata
- Attach protocol and feature versions
- Expiration and integrity data when encoded as a token

On reconnect:

1. Authenticate to a candidate ingress.
2. Fetch/validate committed control state from the last revision or receive a fresh snapshot.
3. Discard cursors for executions no longer authoritative.
4. Resume current execution streams from stored cursors.
5. Repair any unavailable cursor range from a full snapshot.
6. Resume input only after current generation and control authority are established.

The descriptor contains no reusable private credential. Authentication is performed for the new connection.

## Output cursor semantics

- Cursor scope is exactly one execution ID and generation.
- Cursors are monotonic unsigned byte/event positions and never wrap; overflow retires the execution stream with a required snapshot/new epoch protocol.
- An output batch states `stream_start`, `stream_end`, and whether a gap precedes it, matching existing pane-runtime concepts where possible.
- Re-delivery is permitted; consumers discard bytes/events whose end cursor is at or below the last applied cursor.
- Overlap is permitted only when byte/event content for the same cursor range is identical.
- A cursor from an old generation is never applied to a new generation.
- Cursor acknowledgement is advisory for retention and flow control, not control-plane durability.

## Snapshot semantics

A full terminal snapshot contains everything required by the neutral attach renderer to resume correctly, including as applicable:

- Terminal grid and scrollback policy payload
- Cursor and terminal modes
- Input mode state
- Title/status metadata owned by the execution
- Image/protocol state or explicit invalidation requiring image refresh
- Synchronized-update state
- Snapshot format version
- Execution ID/generation
- Output cursor watermark after which deltas apply

Snapshot and subsequent deltas use an atomic handoff: the worker captures a snapshot watermark and guarantees deltas strictly after that watermark are retained or streamed. Ingress must not combine a snapshot with an earlier-generation stream.

If some visual state cannot be serialized, the protocol explicitly invalidates that component using neutral attach-view change semantics; it does not silently render stale state.

## Ordering and composition

There is no total order across independent pane output streams. The ingress/client preserves:

- Total control revision order.
- Per-execution output cursor order.
- Causal rule that an execution is not rendered active before its assignment revision is applied.
- Causal rule that output from an execution is not applied after a committed generation replacement/removal.

Rendering order across panes follows normal frame/compositor scheduling and need not invent a distributed timestamp order.

## Retention and backpressure

- Worker output buffers are bounded by bytes and time.
- Initial required minimum retention is **16 MiB per active execution or 60 seconds of output, whichever boundary is reached later**, subject to a configurable global node cap. If the global cap forces earlier eviction, the stream must report a gap and snapshot repair must remain available.
- Slow consumers receive bounded queues and explicit gap/snapshot-repair signals; they cannot grow memory without limit.
- Ingress applies per-client and per-worker flow control so one slow pane/client does not block unrelated panes.
- Snapshot generation is rate-limited and coalesced per execution to prevent reconnect storms.

The exact storage representation remains an implementation decision. These are observable guarantees, not a requirement to retain raw bytes in one specific structure.

## Input during reconnect

- Client input is paused once connection loss is detected.
- A bounded local input queue may retain at most **64 KiB or 2 seconds**, whichever is reached first.
- Queued input is sent only after the same logical pane still has the same authoritative execution generation and fresh control authority.
- If generation changed, lease expired, permission changed, or the queue exceeded its bound, queued input is discarded with visible status; it is never redirected to a replacement implicitly.
- Mutating attach actions use command IDs and normal control-plane idempotency rather than the transient input queue.

## Event and error states

The generic attach provider reports neutral states such as:

- reconnecting with candidate/attempt metadata
- control read-only
- execution unavailable
- output gap requiring snapshot
- snapshot applying
- input paused/dropped
- incompatible resume requiring clean reopen

Cluster-specific explanation is supplied as provider status metadata, not hard-coded branches throughout core attach runtime.

## Consequences

### Positive

- Ingress failover does not require terminal streams in consensus.
- Bounded buffers and snapshot repair handle long disconnects safely.
- Independent streams avoid false global ordering and head-of-line blocking.
- Existing neutral protocol types remain reusable.

### Costs

- Workers must support consistent snapshot/cursor handoff.
- Resume state spans control and multiple execution streams.
- Some queued input is deliberately dropped rather than risk delivery to the wrong execution.

## Rejected alternatives

- **One cluster-wide terminal sequence:** rejected because independent streams would require unnecessary serialization.
- **Consensus-log terminal output:** rejected due to volume and latency.
- **Unlimited output retention:** rejected due to unbounded memory/disk use.
- **Always replay queued input to the logical pane:** rejected because it could reach a replacement process.
- **Blank-screen reconnect without snapshot:** rejected because buffer gaps are normal and recoverable.

## Architecture placement

Generic attach snapshot/delta/cursor/provider mechanisms may live in neutral protocol/runtime crates. Cluster interpretation, worker fan-out, workspace revision reconciliation, candidate selection, and generation routing remain in cluster plugins. Existing local attach remains an implementation of the same generic provider semantics where practical.

## Acceptance criteria

Tests must prove exact resume with retained cursors, snapshot repair after rollover, stale-generation output rejection, bounded slow-consumer memory, correct image/mode invalidation, safe input pause/drop, and successful resume through a different ingress without worker restart.
