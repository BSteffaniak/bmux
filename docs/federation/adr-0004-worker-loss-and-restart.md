# ADR-0004: Worker Loss, Recovery, Replacement, and Restart Policy

- **Status:** Accepted
- **Date:** 2026-07-23

## Context

A worker can become unreachable because of process failure, machine failure, transport loss, maintenance, or partition. Unreachability does not prove that its pane processes stopped. Automatically launching replacements can duplicate shells, deployments, database commands, or other externally visible side effects.

At the same time, a returning worker should be able to restore a pane without losing logical layout or forcing a restart when the original process is still valid.

## Decision

Worker loss defaults to **safe degradation**, not automatic replacement. The logical pane remains in the workspace and transitions through explicit availability states. Replacement is permitted only by durable pane restart policy or an explicit authorized command.

## Availability state model

A logical pane exposes a cluster availability state separate from the local process status:

- `pending`: assignment/workflow exists but no active execution is confirmed.
- `ready`: current execution is reachable and authorized.
- `suspect`: liveness has been missed but failure is not established.
- `unavailable`: current execution cannot be reached; it may still exist.
- `reconciling`: a returning worker/execution is being checked against committed state.
- `replacing`: a new generation has been committed and replacement is in progress.
- `exited`: current process exited and policy does not presently launch another.
- `failed`: workflow reached a durable failure requiring user action or policy retry.
- `quarantined`: execution identity/generation conflicts with authoritative state.

Liveness observations are not consensus truth. The leader commits state changes that affect placement or generation only after applying policy to authenticated observations.

## Failure detection

- Heartbeats and transport errors mark a worker `suspect` locally.
- A configurable suspicion interval plus corroborating probes may mark executions unavailable in committed state.
- A single missed heartbeat does not increment generation or launch a replacement.
- Detection thresholds are operational configuration, not protocol constants.
- Diagnostics report observation time, source, and whether a status is local or committed.

Initial defaults:

- Heartbeat interval: **1 second**.
- Suspect after: **3 consecutive missed heartbeats**.
- Eligible for committed unavailable assessment after: **5 seconds** without authenticated contact, subject to leader/quorum availability.

These defaults optimize local/LAN and normal internet operation; deployments may tune them. They do not trigger replacement unless restart policy allows it.

## Restart policies

Each logical pane has one durable policy:

- `manual` (**default**): never replace automatically. User or authorized automation explicitly requests restart/move.
- `on_confirmed_exit`: replace only after the worker authoritatively reports process exit and the generation is current; unreachability alone is insufficient.
- `on_worker_loss`: replacement may occur after unavailability is committed and a policy-specific grace period expires.
- `never`: preserve terminal history/state but reject restart through ordinary automatic workflows; explicit destructive recreation is a separate action.

Policies may include bounded retry count/backoff and placement constraints. Unbounded tight restart loops are prohibited.

Adopted pre-existing panes default to `manual` because their launch specification may be incomplete or unsafe to replay. A pane cannot use automatic replacement until it has a validated, explicit restartable launch specification.

## Replacement protocol

1. Authoritative control verifies current generation and policy/user authorization.
2. Control commits generation `g + 1`, new execution identity, target worker intent, and a workflow record. This fences generation `g` before the replacement can accept input.
3. The target worker idempotently prepares the execution.
4. Control commits the new execution active after worker confirmation.
5. Input routing switches only to the active generation.
6. If the old worker returns, generation `g` is quarantined and may be terminated by policy.

Failure to launch the replacement does not restore generation `g` authority automatically. Recovery either retries the same generation/workflow safely, chooses another target through a committed transition, or leaves the pane failed/unavailable.

## Worker return and reconciliation

A returning worker reports its node incarnation, local execution inventory, process continuity evidence, execution IDs, generations, and output/snapshot availability.

For each execution:

- Exact current tuple and credible process continuity: resume and mark ready after authority renewal.
- Current tuple but terminal/output state lost: retain process only if safe control is possible; repair display from available snapshot or report degraded state.
- Lower generation: fence and quarantine; never route input.
- Current generation with a different execution ID: quarantine and alert.
- Unknown logical pane: mark orphan; do not publish.

Worker/plugin restart must persist enough local metadata to distinguish a known surviving process from an unrelated PID. PID alone is insufficient due to reuse. Adoption should use existing pane-runtime identity plus a persisted execution binding and process/runtime incarnation evidence.

## Node drain

Draining prevents new placement. Existing panes are handled by policy:

- `manual`/`never`: remain until operator explicitly moves/closes them, unless a force option is separately authorized.
- Restartable panes: may be replaced through the normal generation protocol.
- Drain completion reports blockers rather than silently killing them.

## User experience

Unavailable placeholders display:

- Logical pane name/ID
- Last assigned node
- Execution ID and generation
- Last successful contact/output time
- Restart policy
- Reason and whether quorum is available
- Valid actions: wait, retry contact, restart, move, close, inspect

The UI does not claim a process is dead merely because a node is unreachable.

## Consequences

### Positive

- Network partitions do not silently duplicate side effects.
- Original processes resume naturally when workers return.
- Automatic recovery remains available for explicitly restartable workloads.
- Drain and failure share one generation-safe replacement mechanism.

### Costs

- Some failures require user intervention.
- Liveness, process status, and logical availability must be modeled separately.
- Replacement workflows are slower than optimistic immediate restart.

## Rejected alternatives

- **Always restart elsewhere:** rejected due to duplicate external side effects.
- **Never permit replacement:** rejected because controlled restartable workloads need availability.
- **Treat transport loss as process death:** rejected because partitions are ambiguous.
- **Reuse the same generation for replacement:** rejected because old and new processes could both accept input.

## Architecture placement

Failure policy, restart policy, placement, reconciliation, and placeholders are cluster plugin concerns. Workers reach local pane runtime through typed plugin APIs. Core runtime must not add worker-loss or cluster-restart behavior.

## Acceptance criteria

Tests must prove no replacement occurs under default policy, exact current executions resume, stale executions never receive input, opt-in restart increments generation before activation, and drain reports non-restartable blockers without surprise termination.