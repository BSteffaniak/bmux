# ADR-0011: Deterministic Federated Control State Machine

- **Status:** Accepted
- **Date:** 2026-07-24

## Context

Before OpenRaft network or storage wiring, BMUX needs a closed deterministic command and response model. Every voter must produce byte-identical logical state and stable command outcomes from the same committed log. Commands cannot read clocks, generate IDs, probe workers, consult configuration, or perform plugin IO while applying.

The state machine must eventually own membership, logical workspaces/windows/layout, logical panes, placement intent, execution generations, restart policy, and command deduplication. Worker side effects are separate workflows driven after intent commits.

## Decision

Use one versioned `ControlCommand` envelope and one versioned `ControlResponse` envelope. Stable wire/model definitions live in `cluster-plugin-api`; application and invariant enforcement live in `cluster-plugin`.

## Command envelope

Every committed application entry contains:

```text
ControlCommand {
  schema_version,
  principal_id,
  command_id,
  issued_at_unix_ms,
  request,
}
```

`principal_id` and `command_id` form the deduplication key. `issued_at_unix_ms` is supplied by the authorized leader and used only for deterministic retention/audit metadata; followers never read their local clocks while applying. Consensus log ID and term are supplied by OpenRaft apply context, not trusted from the request.

The initial command vocabulary is:

- `upsert-member`: add/update public member metadata and capabilities after validated enrollment or rotation.
- `set-member-state`: transition active/revoked/left state with expected credential serial.
- `create-workspace`: create stable workspace identity/name.
- `rename-workspace`: update display name using expected workspace revision.
- `put-window`: create/update logical window and layout payload using expected workspace revision.
- `remove-window`: remove an empty/logically removable window using expected revision.
- `put-pane`: create/update logical pane metadata, restart policy, and placement intent using expected revision.
- `remove-pane`: remove a logical pane using expected generation/revision.
- `assign-execution`: atomically set node, execution ID, and strictly increasing generation using expected prior generation/revision.
- `set-pane-availability`: update committed availability/reason for the current execution tuple.
- `complete-workflow`: record a stable terminal response/error for an intent-driven side-effect workflow.
- `prune-dedup`: remove only terminal dedup records older than the replicated retention cutoff; incomplete workflows are never removed.

Consensus membership configuration changes are driven through OpenRaft's learner/membership APIs. The application `upsert-member` record does not itself grant a Raft vote. Application membership credential activation/revocation and OpenRaft voter changes are coordinated workflows with separately committed steps.

## Deterministic state

```text
ControlState {
  schema_version,
  cluster_id,
  revision,
  members,
  workspaces,
  windows,
  panes,
  dedup,
}
```

All maps are ordered by canonical full ID bytes. No hash-map iteration, locale comparison, filesystem ordering, or floating-point arithmetic is allowed in application logic.

Each logical pane contains:

- `LogicalPaneId` and owning `WorkspaceId`/`LogicalWindowId`;
- mutable name and opaque versioned layout reference;
- restart policy (`manual`, `never`, `on-worker-loss`);
- deterministic placement intent (explicit node and ordered required/preferred labels initially);
- availability and reason;
- current execution tuple or none;
- pane revision.

The execution tuple is `(NodeId, ExecutionGeneration, ExecutionId)`. A replacement must commit a generation greater than the prior generation before any worker activation. Equality/reuse/regression is rejected.

## Apply algorithm

For each committed application entry:

1. Validate envelope and schema version.
2. Canonically encode the request variant and compute its SHA-256 request fingerprint outside of domain comparison ambiguity.
3. Look up `(principal_id, command_id)` in `dedup`.
   - Same fingerprint: return the stored response/workflow state without incrementing revision or reapplying.
   - Different fingerprint: return `command_id_conflict` without mutation.
4. Validate all expected revisions/generations and referential invariants against current state.
5. Compute the new state and response entirely in memory.
6. Increment the global control revision exactly once for a successful state-changing command. Stable rejected domain outcomes do not increment revision.
7. Atomically persist state changes, last-applied metadata, membership metadata, and dedup result in the same storage transaction.
8. Return the canonical response recorded in dedup.

Panics are never an application response. Invalid committed bytes or impossible invariant violations stop the local consensus runtime as corruption.

## Responses and errors

Every response carries:

- `schema_version`;
- `command_id`;
- resulting/current control revision;
- typed success payload or stable typed domain error;
- workflow status (`complete` or `pending`) where side effects remain.

Initial stable domain errors include:

- `command-id-conflict`;
- `not-found` with resource kind and ID;
- `already-exists`;
- `revision-conflict` with expected/current revision;
- `generation-conflict` with expected/current generation;
- `invalid-reference`;
- `invalid-transition`;
- `member-inactive`;
- `incompatible-schema`;
- `quorum-required` (generated before proposal/read, not by deterministic apply);
- `not-leader` with optional authenticated leader hint (generated before proposal).

Authorization failures occur before proposal and are not application state-machine outcomes. The authorized principal and decision inputs are nevertheless bound to the proposed command and audit record as required by ADR-0006.

## Command-specific invariants

### Membership

- Cluster ID is immutable.
- Node ID must match the member public key and signed credential.
- Credential serial changes require a valid coordinated rotation transition.
- Revoked/left members cannot become active through a generic update.
- Application role metadata cannot independently alter OpenRaft voter configuration.

### Workspace/window/layout

- IDs are globally typed UUID-compatible values and never generated during apply.
- Names are metadata, not uniqueness or identity keys.
- A window must reference an existing workspace.
- Layout data is an opaque versioned canonical payload until the logical layout schema is introduced; unknown future versions are rejected, never partially interpreted.

### Logical panes and execution

- A pane references existing workspace/window records.
- Generation zero means no execution has ever been authoritative; concrete executions use generation >= 1.
- `assign-execution` requires exactly the current expected generation and commits a strictly greater generation.
- Availability updates must match the complete current execution tuple.
- Unreachability alone never clears or replaces an execution.
- Default restart policy is `manual`; replacement is never inferred during apply.

### Deduplication

- Request fingerprints use canonical versioned bytes and SHA-256.
- Successful and stable domain-error outcomes are recorded.
- Pending workflow records include deterministic sub-operation IDs derived from principal, command ID, operation family, and step number.
- Terminal records are retained at least 24 hours using replicated timestamps/cutoffs.
- Pending/incomplete workflows survive snapshots and are retained until terminal resolution.

## Canonical encoding

BPDL JSON compatibility is for typed service transport and diagnostics; consensus persistence uses a separate canonical binary envelope:

- unsigned fixed-width integers in big-endian order;
- UUIDs as 16 raw bytes;
- strings as UTF-8 with a big-endian length prefix and schema-defined maximum;
- booleans as exactly `0` or `1`;
- option/list/map lengths as bounded unsigned integers;
- enums as explicitly assigned numeric tags that are never reordered or reused;
- maps encoded in canonical key order;
- no floats, platform-sized integers, unknown fields, or unbounded allocation;
- SHA-256 over the complete versioned request bytes for deduplication.

A codec module in `cluster-plugin` owns these bytes and golden fixtures. Serde-derived Rust layout is not a durable consensus format.

## Reads

- Authorization, mutation preconditions, current execution authority, and lease issuance use OpenRaft linearizable reads.
- Explicit diagnostics/history may request stale local reads and must return applied log ID/revision plus `stale=true`.
- State-machine apply never invokes a read API recursively.

## Side effects

Applying a command never launches/closes a pane, contacts another node, emits terminal data, writes audit sinks, or checks permissions. It commits durable intent and a pending workflow. A leader-side reconciler executes idempotent typed worker operations and submits `complete-workflow` or subsequent generation-controlled commands. Leadership changes resume from replicated workflow state.

## Migration

- Unknown future command schema versions are rejected before proposal.
- Storage startup migrates older application state through explicit idempotent migration steps before joining consensus.
- Rolling upgrades only propose commands supported by the negotiated voter capability floor.
- Snapshot schema includes state schema and canonical-codec versions independently.

## Consequences

### Positive

- Network and storage work can target a closed application contract.
- Duplicate delivery, revision fencing, and generation fencing have one authoritative implementation.
- Side effects remain recoverable and outside deterministic Raft application.
- Golden canonical fixtures support mixed-version and snapshot compatibility testing.

### Costs

- Commands are deliberately explicit and require versioned migrations.
- Multi-step worker workflows require a reconciler and durable pending states.
- Layout and placement schemas must be expanded through compatible tagged versions rather than arbitrary maps.

## Rejected alternatives

- **Generate IDs/timestamps during apply:** followers would diverge.
- **Serialize Rust enums directly with unversioned bincode/JSON:** representation changes would silently alter durable hashes and snapshots.
- **Execute worker effects during apply:** replay, snapshot install, and leader changes would duplicate effects.
- **Deduplicate only successful commands:** stable errors and pending workflows would be retried inconsistently.
- **Use last-write-wins revisions:** stale leaders/workers could overwrite current authority.

## Acceptance criteria

1. BPDL exposes the stable typed IDs, command/response models, and errors without runtime implementation.
2. A canonical codec has golden fixtures for every command/error variant and rejects non-canonical/malformed input.
3. Property tests apply identical command sequences across independently initialized state machines and compare state/snapshot bytes.
4. Tests cover same-command replay, conflicting reuse, revision conflicts, generation regression/reuse, invalid references, and incomplete workflow recovery.
5. Snapshot round trips preserve dedup outcomes and pending workflows exactly.
6. No state-machine apply path reads clocks/randomness/configuration, performs IO, or dispatches plugin services.
