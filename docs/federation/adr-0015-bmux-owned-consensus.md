# ADR-0015: BMUX-Owned Raft for the Federated Control Plane

- **Status:** Accepted
- **Date:** 2026-09-12
- **Supersedes:** [ADR-0009](adr-0009-consensus-implementation.md)
- **Amends:** [ADR-0002](adr-0002-consistency-quorum-leases.md), [ADR-0010](adr-0010-consensus-storage.md), and [ADR-0011](adr-0011-deterministic-control-state-machine.md)

## Context

BMUX's approved replacement direction is to own and maintain its implementation of established Raft rather than depend on OpenRaft. This transfers protocol correctness, persistence ordering, membership-transition safety, and long-term maintenance responsibility to BMUX. It does not change the federation consistency model or weaken its acceptance criteria.

The existing implementation uses OpenRaft through `consensus_runtime.rs`, `consensus_storage.rs`, `consensus_network.rs`, and `consensus_membership.rs` in `plugins/cluster-plugin`. Startup and service consumers also access its runtime APIs. Replacing a dependency or implementing an isolated election loop does not replace that production path.

## Decision

Implement established Raft under BMUX ownership, initially entirely within `plugins/cluster-plugin`. Do not invent a consensus algorithm, vendor or rename an OpenRaft fork, or create a general-purpose framework for hypothetical consumers. OpenRaft must be absent from production code and the dependency graph when replacement qualification is complete.

This decision authorizes the replacement architecture, not immediate production cutover. The existing OpenRaft implementation remains the current runtime until the replacement and migration meet the gates below. Its existing safety requirements remain binding throughout development.

### Ownership and integration

- A deterministic transition engine consumes peer messages, explicit timer events, proposals, and persistence completion/failure notifications. It emits persistence requests, outbound messages, committed-entry notifications, and leadership/progress updates.
- The engine does not perform I/O, consult implicit clocks, authorize callers, or launch processes. Plugin-owned adapters execute effects and report completion.
- Existing cluster services remain the product entry points. Generated BPDL contracts carry modeled transport; core and `HostRuntimeApi` acquire no cluster helpers or types.
- API crates contain stable contracts, not engine state, runtime tasks, storage implementations, or orchestration.
- Existing control application, deduplication, membership workflows, and worker reconcilers retain their authority. External effects follow committed intent and remain fenced and idempotent.
- Local terminal operation does not require federation.

### Safety and durability

The replacement must preserve election safety, log matching, committed entries, current-term majority commit rules, and safe serialized joint-consensus membership changes. Learners do not gain voting authority through generic member metadata updates.

Term and vote persistence precedes dependent responses; replication acknowledgment follows required durable log persistence. Quorum commit and durable application are distinct stages. Successful application responses follow the required atomic durable transaction covering canonical state, applied position, membership, deduplication outcomes, and unfinished workflows.

Authoritative reads establish current quorum authority and wait for the required applied position. Authorization, mutation preconditions, capability admission, and fencing are checked at their authoritative boundaries, including across asynchronous waits and leadership changes. Loss of quorum cannot authorize control mutation.

Storage failure stops the affected node from voting or serving authoritative mutations on uncertain durability. Corrupt or unsupported durable state is an explicit error, not an empty installation. Snapshot installation and compaction preserve coherent state, membership, applied position, and recovery authority.

Replication batches, outstanding requests, queues, retained buffers, snapshot transfer, application work, and per-peer work must be bounded. Slow peers receive backpressure, catch-up, or explicit failure rather than silent loss.

### Existing storage and application decisions

ADR-0010's redb selection, durable transactions, corruption handling, snapshot publication, cluster binding, and fail-closed behavior remain in force. Its OpenRaft trait names describe the existing adapter; owned persistence operations must preserve their ordering obligations. This decision does not declare existing bytes compatible with new types or authorize an unversioned rewrite.

ADR-0011's deterministic command semantics and canonical application authority remain in force. In the replacement, trusted log identity and committed membership context come from the owned consensus adapter rather than OpenRaft. Product member records still do not themselves grant votes.

### Compatibility and migration

Protocol, storage, and engine compatibility are separate explicit contracts. Similar wire fields or Rust types are not evidence of interoperability. Existing interface meanings must not silently change; new representations require negotiated support and unsupported combinations must be rejected.

A coordinated control-plane maintenance-window cutover is the proposed migration direction, not a ratified operator procedure. Mixed-engine rolling operation is not supported by this decision. Selecting the migration strategy requires a separate recorded decision establishing the authoritative committed boundary, interruption recovery, rollback limits, and prevention of simultaneous old/new authorities.

Migration must preserve logical IDs, canonical state, membership, deduplication, unfinished workflows, execution generations, authority epochs, and outstanding-lease safety. Worker unreachability is not death; cutover alone does not authorize terminating healthy processes or reviving stale execution authority.

## Qualification gates

Production replacement requires all of the following, independently of this ADR's acceptance:

1. Reviewable engine types, transition rules, persistence-before-effect ordering, supported failure assumptions, and compatibility contracts.
2. Deterministic simulation and model checking of safety-critical transitions, with reproducible fault traces.
3. Crash-injection evidence for durable acknowledgments, snapshots, corruption, restart, and unfinished workflow recovery.
4. Three- and five-voter process tests covering partitions, leader changes, quorum loss, membership transitions, slow peers, and recovery.
5. Existing authenticated services, bootstrap, capability publication, control mutations, authoritative reads, and worker reconcilers connected end to end, including cancellation, shutdown, and failure reporting.
6. A ratified and tested migration procedure, including interrupted cutover and supported rollback behavior.
7. Reliability and performance qualification against ADR-0008 and independent review of consensus and persistence safety.
8. Removal of OpenRaft dependencies and obsolete adapters, mechanical boundary guards, updated operational documentation, and repository-required validation as defined in `AGENTS.md`.

Neither a simulator-only implementation nor passing compilation establishes product closure. No architectural or recovery obligation may be deferred as cleanup to declare the replacement complete.

## Consequences

BMUX gains implementation ownership but assumes substantially more safety-critical code and review responsibility than ADR-0009's library-based approach. Existing adapters and tests provide integration evidence, not automatic proof of replacement correctness. Engine delivery, production integration, and migration qualification remain separate obligations.

No invariant is weakened: quorum authority, authentication, deterministic application, durable acknowledgments, execution fencing, resource identity, and plugin ownership keep their existing meaning.
