# ADR-0013: Deterministic Worker Placement

- **Status:** Accepted
- **Date:** 2026-07-25

## Context

Placement must choose a worker for a logical pane without making retries, leader changes, or map iteration produce different outcomes. It must honor explicit constraints, avoid unhealthy or draining nodes, expose why candidates were accepted or rejected, and prefer stability without overriding safety or user intent.

## Decision

Placement evaluates a quorum-confirmed control-state snapshot plus one bounded, leader-collected observation set. The leader converts all observations into an explicit, ordered placement input before proposal; deterministic state-machine apply never reads health, clocks, capacity, or network state.

### Eligibility filters

Apply these hard filters in order and record the first rejection reason:

1. Active membership and a valid compatible credential/protocol.
2. Worker capability is granted.
3. Node is not drained; a draining node is ineligible for new placement. A cordoned node is also ineligible except when preserving its already-authoritative current execution.
4. Explicit node constraint, when present.
5. Every required label matches exactly.
6. Required capacity floor is available from the bounded observation set.
7. Health is not `unavailable` or `incompatible`. Unknown health is ineligible for automatic placement but may be selected only by an explicit-node request with a visible warning.

An explicit node narrows eligibility; it never bypasses membership, worker capability, compatibility, drain, required-label, capacity, or safety checks.

### Deterministic ranking

Rank eligible candidates lexicographically by the following tuple, lower values first:

1. `preserve_current`: 0 when the current authoritative execution is still eligible and no move/restart was requested; otherwise 1.
2. `preferred_label_misses`: count of preferred labels not matched.
3. `spread_conflicts`: number of active sibling executions in the requested spread group already assigned to the node.
4. `health_rank`: healthy=0, degraded=1, explicit-unknown=2.
5. `capacity_pressure`: integer used/capacity ratio in basis points, rounded down; missing capacity is ineligible unless explicitly selected as above.
6. `locality_rank`: same ingress/region/zone preferences in that order, represented as explicit integer ranks rather than latency floats.
7. Canonical `NodeId` bytes as the final total-order tie-breaker.

No floating-point arithmetic, randomness, wall-clock reads, hash-map iteration order, or live probes participate in ranking. Retries with identical canonical inputs produce byte-identical candidate ordering and explanation output.

### Stability and spread

Previous placement is a preference only after all hard constraints. It prevents gratuitous movement but cannot keep an unhealthy, incompatible, drained, or constraint-violating worker. Spread is soft unless a future schema explicitly marks it required; soft spread cannot make all candidates ineligible.

### Explanations

Every decision records the input revision/observation epoch, selected node, ordered candidate tuples, hard-filter rejection reasons, and whether stability, labels, spread, health, capacity, locality, or the NodeId tie-break selected the winner. Explanations contain no secrets or raw terminal data.

## Consequences

- Placement is reproducible and inspectable across retries and leader changes.
- Explicit requests remain safe rather than becoming an escape hatch.
- Integer ranking avoids architecture-dependent floating-point behavior.
- Health/capacity observations may become stale; their epoch and age are visible, and stale observations fail safe for automatic placement.

## Acceptance criteria

1. Permutation tests produce identical ordering and explanations for identical candidate sets.
2. Tests cover every hard filter and every ranking tier, including canonical NodeId tie-breaking.
3. Unhealthy, incompatible, non-worker, drained, and required-label/capacity failures cannot be selected implicitly.
4. Current placement remains stable when eligible, while explicit move/restart can choose the next ranked worker.
5. No placement observation or decision logic enters core architecture or deterministic apply-time IO.
