# ADR-0009: OpenRaft for the Federated Control Plane

- **Status:** Accepted
- **Date:** 2026-07-24

## Context

BMUX federation needs a production Raft implementation for durable control metadata, leader election, quorum-safe writes, linearizable authorization reads, snapshots, and safe voter changes. The implementation must fit Tokio and plugin-owned typed endpoint transport without placing cluster policy in core.

The two credible maintained Rust candidates evaluated were OpenRaft and TiKV raft-rs. Both are actively maintained and implement Raft, but they expose different integration levels.

## Decision

Use **OpenRaft 0.9.x**, pinned to a reviewed patch release, for the initial federated control plane. Do not adopt the 0.10 alpha line. Upgrades are explicit because OpenRaft's pre-1.0 API and durable types may change.

Implement OpenRaft entirely inside `plugins/cluster-plugin`:

- The cluster plugin owns `RaftTypeConfig`, deterministic request/response types, the network factory, runtime wiring, metrics, storage, snapshots, and lifecycle.
- Peer RPC uses generated cluster-plugin BPDL services over the existing generic `bmux.connections` endpoint boundary.
- Every network connection verifies the expected cluster `NodeId`; OpenRaft explicitly requires the application to prevent wrong-node routing.
- The plugin API crate contains only stable wire contracts and model types, never OpenRaft runtime or storage implementations.
- Core IPC and `HostRuntimeApi` remain unchanged.

OpenRaft's split storage-v2 interfaces (`RaftLogStorage` and `RaftStateMachine`) are the required integration surface. The deprecated combined storage interface is not used.

## Evaluation

| Criterion                   | OpenRaft 0.9.24                                                                                   | TiKV raft-rs 0.7.0                                                                                                                                      |
| --------------------------- | ------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Maintenance                 | Stable release published July 2026; repository active in July 2026                                | Stable crate release is from March 2023, though repository development remains active in May 2026                                                       |
| Async fit                   | Tokio-native async Raft API and async network/storage traits                                      | Synchronous state-machine core; application must build its own async event loop, ticking, Ready persistence ordering, transport, and task orchestration |
| Storage API                 | Separate log and state-machine traits, flush completion, snapshot builder/install APIs            | Low-level `Storage` plus explicit `Ready` handling; more correctness-critical persistence orchestration belongs to BMUX                                 |
| Snapshots                   | First-class snapshot build/install and documented compaction policy                               | Snapshot mechanisms are available but application integration is lower level                                                                            |
| Membership                  | Documented learner-first dynamic membership and joint membership                                  | Joint consensus is supported through configuration changes, with more orchestration left to the application                                             |
| Linearizable reads          | Built-in client/read APIs and leader/quorum machinery                                             | ReadIndex is available, but application drives it through the low-level event loop                                                                      |
| Observability               | Raft metrics watcher and documented cluster-management APIs                                       | Status and internal progress are exposed, but application supplies more monitoring glue                                                                 |
| Deterministic tests         | Network/storage traits are directly substitutable; project includes deterministic simulation work | Strong algorithm test history, but BMUX would need a larger custom harness around the raw node loop                                                     |
| Dependency/integration cost | Larger and pre-1.0, but substantially less custom consensus plumbing                              | Smaller core abstraction, but significantly more BMUX-owned safety-critical code                                                                        |

OpenRaft is selected because it minimizes custom consensus orchestration while directly providing the async storage, snapshot, membership, metrics, and testing seams required by the progress plan. raft-rs remains a capable alternative, but its raw-node integration would require BMUX to own more persistence-ordering and event-loop correctness, contrary to the requirement not to invent consensus infrastructure.

## Version and upgrade policy

- Pin an exact reviewed OpenRaft 0.9 patch version in the workspace lockfile.
- Do not enable compatibility-relaxing features such as follower-log reversion in production.
- Treat OpenRaft upgrades as control-plane storage migrations: review its change log and upgrade guide, run snapshot/WAL compatibility fixtures, and require mixed-version integration tests before changing the pin.
- OpenRaft's node metadata is routing information only. BMUX's signed membership remains the authority for cluster identity, roles, credentials, and revocation.

## Storage implications

This ADR selects the consensus library, not the durable storage engine. The following Phase 4 decision must separately select and document a crash-safe implementation of the log, vote/committed state, state machine, and snapshots. Generic plugin key/value storage is not sufficient for WAL semantics.

Required storage properties are:

- atomic durable vote and committed-state updates;
- ordered append/truncate with explicit flush completion;
- crash-safe snapshot creation, installation, and replacement;
- corruption detection and fail-closed startup;
- schema/version metadata independent from OpenRaft's Rust types;
- a cluster-plugin-owned state directory and no private keys in replicated data.

## Consequences

### Positive

- BMUX uses a maintained async Raft implementation rather than implementing elections, replication, or joint consensus.
- Storage, transport, and deterministic test doubles have explicit trait boundaries.
- Learner catch-up and membership transitions match the voter/observer lifecycle already designed.
- Metrics can drive leader discovery, diagnostics, and operational status.

### Costs and risks

- OpenRaft is pre-1.0 and has unstable APIs; pinning and explicit upgrade work are mandatory.
- BMUX still owns all durable storage correctness and application state-machine determinism.
- Incorrect endpoint-to-node mapping can violate Raft safety, so expected-node authentication is mandatory for every peer RPC.
- OpenRaft is an implementation detail of the cluster plugin and must not leak into stable BPDL contracts.

## Rejected alternatives

- **TiKV raft-rs:** rejected for the initial implementation because its lower-level `RawNode`/`Ready` model leaves more async scheduling, persistence ordering, and snapshot/membership orchestration in BMUX.
- **A custom Raft implementation:** rejected by ADR-0002 and the Phase 4 goal.
- **A permanent coordinator or last-writer-wins replication:** rejected by ADR-0002 because it cannot provide quorum safety and execution fencing.
- **OpenRaft 0.10 alpha:** rejected until a stable release and migration path are available.

## Acceptance criteria

Before the library selection is considered implemented in runtime code:

1. The exact OpenRaft version is pinned and wrapped only in `cluster-plugin`.
2. A storage backend decision records fsync, corruption, and snapshot replacement behavior.
3. Generated peer RPC verifies expected `NodeId` on every connection.
4. Deterministic tests exercise election, leader loss, learner catch-up, joint membership, quorum loss, snapshot install, and duplicate application commands.
5. A three-voter process test repeatedly kills the leader around commit boundaries without split-brain writes or lost committed state.

## Sources reviewed

- OpenRaft 0.9.24 crate documentation and getting-started guide, including its pre-1.0 API warning.
- OpenRaft storage-v2 documentation (`RaftLogStorage`, `RaftStateMachine`, snapshot builder/install).
- OpenRaft dynamic-membership documentation, including learner-first changes, joint membership, and expected-node connection warnings.
- crates.io release metadata and GitHub repository activity for OpenRaft.
- raft-rs 0.7.0 crate documentation, crates.io release metadata, and GitHub repository activity.
