# ADR-0008: Federation Reliability and Performance Budgets

- **Status:** Accepted
- **Date:** 2026-07-23

## Context

“Cohesive and robust” requires measurable expectations. Without budgets, implementations can be logically correct yet visibly disruptive, retain unbounded data, or regress local bmux behavior. Early federation work needs provisional service-level objectives that Phase 11 tests can enforce and refine from evidence.

These targets apply to the first complete trusted-domain federation release. They are engineering acceptance budgets, not an external uptime contract.

## Reference environments

Measure and report at least:

1. **LAN:** three voters/workers, <= 2 ms peer RTT, no packet loss.
2. **Regional WAN:** three voters/workers, <= 50 ms peer RTT, <= 0.1% packet loss.
3. **Mixed worker topology:** three voters plus five workers, 24 active panes, one client.
4. **Fan-out stress:** 100 active panes, four attached clients, bounded synthetic output.

Measurements use release builds on documented hardware. Tests report median, p95, p99 where sample count permits. Failure tests repeat at least 100 times before release gating unless runtime makes a smaller statistically justified count necessary.

## Availability budgets

### Leader election and writes

- LAN leader failure: p95 new leader elected and linearizable mutation accepted within **2 seconds**, p99 within **5 seconds**.
- Regional WAN leader failure: p95 within **5 seconds**, p99 within **10 seconds**.
- No committed mutation loss.
- No duplicate logical mutation under response loss/retry.

### Ingress failover

- LAN ingress loss: p95 client connected to another ingress and control state resumed within **2 seconds**, p99 within **5 seconds**.
- Regional WAN ingress loss: p95 within **5 seconds**, p99 within **10 seconds**.
- Healthy worker processes experience zero restart due solely to ingress/leader loss.
- With retained output, resume introduces zero output cursor gaps.
- With evicted output, snapshot repair completes without rendering stale-generation data.

### Worker failure

- Worker transport failure becomes visibly `suspect` within **4 seconds** under defaults.
- It becomes an actionable unavailable placeholder within **8 seconds** when leader/quorum are healthy.
- Default/manual policy launches zero replacement executions.
- A returning current-generation worker becomes resumable within **5 seconds** on LAN and **15 seconds** on regional WAN after authenticated connectivity returns.

### Quorum loss

- Control mutation rejection/read-only indication occurs within the current lease bound plus **1 second**.
- No lifecycle mutation succeeds without quorum.
- Interactive input cannot continue beyond the maximum 5-second control lease after loss of renewable authority.

## Interactive performance budgets

Measured excluding terminal application response time:

- Local non-federated attach median input-routing overhead regression: **<= 2%**, p95 absolute added latency **<= 1 ms**.
- Federated LAN input from ingress to worker write: p50 **<= 5 ms**, p95 **<= 15 ms**, p99 **<= 30 ms**.
- Regional WAN federation adds no more than **one avoidable application-level RTT** beyond transport RTT for steady-state input; target p95 processing overhead **<= 10 ms**.
- Federated LAN output from worker read to ingress/client delivery: p50 **<= 10 ms**, p95 **<= 30 ms**, p99 **<= 75 ms** under reference load.
- Viewport resize reaches all affected workers p95 **<= 100 ms** on LAN and **<= 300 ms** regional WAN.
- Control actions already on the leader, excluding worker process startup, p95 **<= 100 ms** LAN and **<= 500 ms** regional WAN.

## Attach and repair budgets

- Warm attach to a healthy 24-pane workspace: p95 first useful frame **<= 750 ms** LAN, **<= 2 seconds** regional WAN.
- Warm resume with retained cursors: p95 first updated frame **<= 500 ms** after connection establishment LAN, **<= 1.5 seconds** regional WAN.
- Snapshot repair for a typical 200x60 text pane without large image payloads: p95 **<= 500 ms** LAN, **<= 2 seconds** regional WAN.
- Repair is incremental across panes; one slow pane must not prevent useful frames from healthy panes.

## Resource budgets

### Memory and queues

- All per-client, per-worker, and per-execution queues are bounded and expose drop/gap/backpressure metrics.
- Default retained output target: at least 16 MiB or 60 seconds per active execution as specified by ADR-0005, constrained by a configurable node-wide cap.
- Default node-wide raw retained-output cap: **1 GiB**; exceeding it evicts by documented policy and forces explicit snapshot repair.
- Per-client queued unsent output cap: **8 MiB** default.
- Per-worker connection/control queue cap: **8 MiB** default.
- Reconnect input queue cap: **64 KiB and 2 seconds**.
- Consensus log growth from idle terminal output: **zero bytes**.

### CPU

At steady state with 24 panes each producing 10 KiB/s and one client:

- Federation routing/composition CPU should consume **<= 1 logical core average** on documented reference hardware, excluding terminal parsing already required locally.
- Idle cluster overhead across heartbeats/consensus should average **<= 2% of one logical core per node**.
- Snapshot generation is rate-limited so reconnect storms do not starve active input/output.

### Bandwidth

- Steady-state federation control metadata overhead excluding payload, encryption framing, and transport keepalive should remain **<= 10%** of terminal payload bandwidth in the 24-pane reference workload.
- Idle three-node cluster control traffic target is **<= 10 KiB/s average per node** on stable membership.
- No terminal payload is replicated to voters that are not serving/relaying that execution stream.

### Durable state

- Consensus log and snapshots remain bounded through compaction under a 24-hour mutation soak.
- Command deduplication retains at least the 24-hour guarantee without unbounded growth.
- Snapshot creation must not block interactive input/output for more than **50 ms p99** on LAN reference hardware.

## Correctness budgets

These are zero-tolerance requirements, not percentile targets:

- Duplicate authoritative execution for one logical pane generation: **0**.
- Input accepted by stale execution generation: **0**.
- Committed control mutation lost after acknowledged success: **0**.
- Minority-partition control mutation accepted: **0**.
- Secret/enrollment/private-key material emitted in logs or replicated state: **0**.
- Unbounded queue or retention structure in reachable production paths: **0**.
- Healthy worker process restarted because only ingress/leader failed: **0**.

## Observability requirements

Emit structured measurements for:

- Election start/end and term
- Mutation forward/propose/commit/apply latency
- Ingress reconnect phases
- Resume control revision and per-execution cursor outcomes
- Output gap and snapshot repair duration
- Input pause/drop reason
- Worker suspect/unavailable/recovered transitions
- Lease renewal/expiry and stale-fence rejection
- Queue depths, retained bytes, evictions, and backpressure
- Connection count, retries, and endpoint selection
- Consensus log/snapshot sizes and compaction duration

Metrics/logging must avoid raw terminal content and secret material.

## Gate policy

- Correctness budget failure blocks completion regardless of average performance.
- Missing required telemetry blocks performance sign-off.
- A percentile budget may be revised only with recorded measurements, user-visible impact analysis, and an ADR amendment.
- Local non-federated regression is a release blocker even if federated performance meets target.
- Known flaky failure tests block completion under repository policy.

## Consequences

### Positive

- Architecture choices can be evaluated against observable outcomes.
- Resource bounds and local-regression protection are explicit early.
- Fault testing has concrete success thresholds.

### Costs

- A reproducible multi-node benchmark/fault harness is required.
- Some targets may require optimization after correctness is established.
- Reference hardware and release-build methodology must be maintained.

## Rejected alternatives

- **Define budgets after implementation:** rejected because early design could bake in avoidable latency or unbounded queues.
- **Only measure averages:** rejected because reconnect/election tail latency defines user experience.
- **Permit local regressions as federation cost:** rejected because local baseline must remain healthy without the plugin.
- **Treat correctness as a probabilistic SLO:** rejected for fencing, quorum, idempotency, and secret handling.

## Architecture placement

Telemetry primitives may be core-neutral, but cluster metric definitions and decisions remain cluster-owned. Performance instrumentation must not require cluster-domain APIs in `HostRuntimeApi`.

## Acceptance criteria

Phase 11 must document reference hardware and produce repeatable reports demonstrating every zero-tolerance invariant and percentile/resource target, or amend this ADR with evidence before release.