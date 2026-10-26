# ADR-0015: BMUX-Owned Raft for the Federated Control Plane

- **Status:** Accepted
- **Date:** 2026-09-12
- **Supersedes:** [ADR-0009](adr-0009-consensus-implementation.md)
- **Amends:** [ADR-0002](adr-0002-consistency-quorum-leases.md), [ADR-0010](adr-0010-consensus-storage.md), and [ADR-0011](adr-0011-deterministic-control-state-machine.md)

## Context

BMUX's approved replacement direction is to own and maintain its implementation
of established Raft rather than depend on OpenRaft. This transfers protocol
correctness, persistence ordering, membership-transition safety, and long-term
maintenance responsibility to BMUX. It does not change the federation
consistency model or weaken its acceptance criteria.

The existing implementation uses OpenRaft through `consensus_runtime.rs`,
`consensus_storage.rs`, `consensus_network.rs`, and `consensus_membership.rs` in
`plugins/cluster-plugin`. Startup and service consumers also access its runtime
APIs. Replacing a dependency or implementing an isolated election loop does not
replace that production path.

## Decision

Implement established Raft under BMUX ownership, initially entirely within
`plugins/cluster-plugin`. Do not invent a consensus algorithm, vendor or rename
an OpenRaft fork, or create a general-purpose framework for hypothetical
consumers. OpenRaft must be absent from production code and the dependency graph
when replacement qualification is complete.

This decision authorizes the replacement architecture, not immediate production
cutover. The existing OpenRaft implementation remains the current runtime until
the replacement and migration meet the gates below. Its existing safety
requirements remain binding throughout development.

### Ownership and integration

- A deterministic transition engine consumes peer messages, explicit timer events, proposals, and persistence completion/failure notifications. It emits persistence requests, outbound messages, committed-entry notifications, and leadership/progress updates.
- The engine does not perform I/O, consult implicit clocks, authorize callers, or launch processes. Plugin-owned adapters execute effects and report completion.
- Existing cluster services remain the product entry points. Generated BPDL contracts carry modeled transport; core and `HostRuntimeApi` acquire no cluster helpers or types.
- API crates contain stable contracts, not engine state, runtime tasks, storage implementations, or orchestration.
- Existing control application, deduplication, membership workflows, and worker reconcilers retain their authority. External effects follow committed intent and remain fenced and idempotent.
- Local terminal operation does not require federation.

### Owned engine model

These are semantic contracts for the implementation, not a claim that runtime
types or a wire codec have been delivered. Engine state and Rust implementation
types belong in `cluster-plugin`; only deliberately modeled transport contracts
belong in its API crate.

#### Identity and persistent state

- `Term` and `LogIndex` are distinct unsigned 64-bit quantities with checked arithmetic. Exhaustion is an explicit fatal/recovery condition, never wraparound. Absence of a log position is represented explicitly, not by reserving a legacy index value: imported OpenRaft entries may use index zero.
- `LogIdentity` retains term, leader NodeId and index. The leader component preserves existing durable identity and diagnostics; voting eligibility is determined by committed/adopted membership, not by a log identity. Candidate freshness is ordered by last-log term then index, not leader NodeId. Matching an existing entry requires its full identity; conflicting content under an identical identity is corruption or a protocol violation, not normal conflict repair.
- `HardState` contains current term and optional voted-for NodeId. At most one candidate can receive a persisted vote per term. The legacy OpenRaft committed-vote flag is not retained as an application commit index and is not silently mapped into one.
- `CommitPosition`, `AppliedPosition`, and `LastPurgedPosition` are distinct optional log identities. Applied cannot exceed committed; purging requires a durable covering snapshot. Recovery must reconstruct a coherent position chain from a qualified durable representation, not infer commitment from the longest retained log.
- `Entry` has a log identity and exactly one payload: leader no-op, canonical application-command bytes, or membership configuration. Command bytes retain their independently versioned application semantics and deduplication identity.
- `Membership` contains an ordered map of NodeId to endpoint metadata and either one nonempty voter set or two nonempty voter sets representing joint consensus. Every voter has a node record; additional records are nonvoting learners. Duplicate IDs or malformed sets are rejected before canonicalization rather than silently collapsed. A joint quorum requires a majority of each set, counting a node once per set. Endpoint presence is not authentication.
- `StoredMembership` pairs the configuration with the log identity that introduced it. Snapshot membership is the configuration at its applied boundary. Runtime must distinguish committed membership from the configuration used for in-flight protocol quorum decisions; it must not promote metrics or application member records into voting authority.
- `SnapshotDescriptor` binds format version, cluster identity, storage snapshot identity, last-included log identity, membership at that position, total bytes and checksum. Canonical control state includes deduplication and unfinished workflows. Snapshot identity is not derived resource identity, an authority credential, or a substitute for its checksum and position.

#### Inputs, effects and completion identities

The deterministic engine receives authenticated peer identity plus decoded
messages, explicit election/heartbeat timer events, bounded local proposals/read
requests, and adapter completion/failure events. Authentication and
authorization occur in plugin adapters; a transport connection is not by itself
an authenticated member.

Each asynchronous effect has a node-incarnation-scoped monotonic identity.
Persistence completions identify the exact submitted batch; outbound RPC
completions identify peer, term, request and stream/transfer as applicable.
Timer events identify the scheduled timer generation. Late completions from
superseded terms, requests, timers, or node incarnations cannot advance current
state. IDs are not reusable private credentials and do not replace durable
application command IDs.

Persistence effects distinguish hard-state updates, log suffix
replacement/append, committed-position updates, atomic application transactions,
and snapshot installation/compaction. A completion reports durable success or
explicit failure, never merely queue admission. Application completion binds its
covered range, applied position and canonical responses. Leadership/progress
notifications are observations, not permission to acknowledge uncommitted work.

#### Peer messages

All messages carry an explicitly negotiated engine protocol version and a term
where applicable. The authenticated envelope binds cluster, sender, recipient,
operation and request identity. Existing Raft RPC v1/v2 and decoder-probe
semantics do not negotiate this engine.

- `RequestVote`: term, candidate NodeId and optional last-log identity. `VoteResponse`: current term, correlated request identity, and granted/rejected outcome. Candidate identity must agree with the authenticated sender.
- `AppendEntries`: term, leader NodeId, optional preceding log identity, bounded contiguous entries and leader commit position. `AppendResponse`: current term, correlated request identity, and either durable matched position or a conflict hint (missing suffix or conflicting term/index). A hint is a repair optimization, never proof that an entry committed. Leader identity must agree with the authenticated sender; an empty append is a heartbeat, not evidence that an arbitrary suffix matches.
- `InstallSnapshotChunk`: term, leader NodeId, transfer identity, immutable descriptor, offset and bounded bytes. `SnapshotResponse`: current term, transfer/request identity and either accepted continuation offset, durably installed position, or explicit rejection. Received bytes and durable installation are distinct outcomes. A transfer cannot change descriptor midway; gaps, conflicting overlap, checksum failure and stale transfers do not publish partial state.
- Read confirmation uses correlated, term-bound heartbeat rounds with fresh read-context identities. A reply to an old heartbeat cannot authorize a new read. The read barrier includes the quorum configuration used and a committed watermark; application must reach that watermark before a response is released.

Wire encodings, numerical message/queue limits, transfer resumption and
capability IDs must be specified and tested before production negotiation is
enabled. These semantic definitions do not claim interoperability with OpenRaft
bodies.

#### Membership configuration used by the protocol

Use Raft joint consensus, not single-server membership changes or
product-member-map quorums. Each node adopts the latest membership entry in its
durable log for elections and replication quorum decisions, including an
uncommitted entry. Its separately tracked committed membership advances only on
commit. Truncating an uncommitted membership suffix restores the effective
configuration from the surviving log or snapshot. Log persistence and
effective-configuration publication are ordered together: no acknowledgment or
quorum decision may depend on an entry that has not become durable locally.

A transition from `C_old` to `C_new` appends `Joint(C_old, C_new)`. The joint
entry and subsequent entries require separate majorities of both sets. The
leader may propose `Uniform(C_new)` only after the joint entry commits; a leader
recovering an already committed joint configuration resumes this durable
transition instead of inventing another target. Once the uniform entry is
durably appended, its effective configuration is `C_new`, including for
committing that uniform entry. Another membership transition is not admitted
until this uniform entry commits. Candidate election rules use the effective
configuration; leader removal takes effect when the final uniform entry commits.
A leader excluded from the new configuration may finish replicating that
transition but cannot count itself in a quorum it no longer belongs to.

Learners replicate without voting and must demonstrate catch-up before voter
promotion. Admission is serialized and tied to the expected effective/committed
membership identities and current term, with authentication, revocation,
capability and mutation preconditions revalidated at proposal. A generic
application membership update does not alter protocol configuration. Durable
intent identifies the authorized target configuration and survives leadership
change; the state machine does not perform admission I/O while applying it.

Read-confirmation rounds bind both the leader term and effective configuration
identity. A membership change invalidates an unfinished round; confirmations
from different configurations cannot be combined. Reads are released only after
current-term leadership is established by a committed current-term entry, a
fresh quorum confirmation succeeds, and application reaches the captured
committed watermark. Old-term entries may commit indirectly through that
current-term entry, not by applying a current majority count directly to an
old-term entry.

#### Wire-contract constraints

Use generated BPDL services with new engine-specific interface identity and
explicit version negotiation, not replacement bodies under existing
`cluster-raft-rpc/v1` or `/v2`. Generated records must express the modeled
messages; do not recreate a public handwritten transport envelope or expose
runtime layouts. The staged-transfer contract below assigns design interface
identifiers and resolves the per-message transfer mechanism; schemas and
allocation guards remain to be implemented and qualified. Existing service
encoding uses typed-stable `bmux_codec` (`packages/plugin-sdk/src/service.rs`);
its legacy positional decoding fallback is not permission to interpret an
owned-engine payload as an OpenRaft payload.

The current core IPC payload maximum is 8 MiB (`packages/ipc/src/frame.rs`),
whereas canonical command byte fields permit up to 16 MiB
(`plugins/cluster-plugin/src/control_codec.rs`). The complete encoded service
envelope, not only raw command bytes, must fit the transport. A smaller append
batch cannot solve a single oversized retained entry. Before specifying
numerical message limits or enabling owned-engine negotiation, define a bounded,
authenticated large-entry transfer mechanism through the existing generic
transport, or explicitly qualify a compatible admission/migration restriction.
Do not raise a core frame limit solely for cluster policy, truncate commands,
discard retained entries, or silently narrow existing command semantics.

That decision must define maximum encoded entry size, chunk size and overhead,
aggregate per-node/per-peer memory and disk quotas, transfer identity and
cancellation, retry after restart, and when a complete assembled entry becomes
eligible for durable log acknowledgment. Snapshot chunks need the same bounded
scheduling discipline but must not be reused as an alternate authority for
uncommitted application entries. Schema and codec tests must cover maximum
encoded envelopes, not only nominal payload sizes. Until this is specified, the
semantic model is intentionally not an implementation-ready wire contract.

#### Bounded large-entry transfer contract

Large entries use plugin-owned staged transfer through generated typed services
over the existing generic endpoint invocation mechanism. Core frame limits do
not change. The staging protocol is not a second log and cannot create a
proposal or application side effect by itself.

Reserve the new modeled interface `cluster-consensus-transfer/v1` for `begin`,
`chunk`, `finish`, and `abort`, and `cluster-consensus-rpc/v1` for owned vote,
append and snapshot control. These are design identifiers, not currently
advertised capabilities. Negotiation requires authenticated support for both
interfaces before admitting an owned-engine peer. Existing RPC v1/v2 and
publication decoder probes keep their meaning. Schemas and generated clients
must be delivered before these identifiers can be advertised; there is no
handwritten parallel client.

The shared transfer header binds cluster ID, sender and recipient NodeIds,
leader term, sender incarnation, monotonically increasing transfer sequence,
purpose, declared total length, and SHA-256 of the complete canonical bytes.
Entry purpose additionally binds log identity and preceding log identity.
Snapshot purpose binds the snapshot descriptor. The authenticated request covers
operation, header, offset and chunk bytes; replay cannot retarget a transfer.
Receivers authenticate before reserving resources and revalidate peer
eligibility and current term on each operation. Transfer identifiers are not
credentials.

- `begin` reserves the entire declared staging length before accepting bytes. It returns accepted-at-offset-zero, an existing matching transfer's continuation offset, or a typed rejection. Conflicting reuse of an identity is rejected. Allocation never trusts an unchecked length conversion, and disk space is not reserved by allocating a same-sized memory buffer.
- `chunk` carries an offset and at most 64 KiB of bytes. Each peer has at most one outstanding chunk request per transfer. The receiver accepts only the next contiguous offset, or an exact replay of the most recently accepted chunk whose bytes/hash match. Other offsets return the expected offset without modifying staged data. A chunk acknowledgment means staging progress only, never durable replication. An empty chunk cannot extend a transfer or refresh its idle lifetime.
- `finish` requires exact total length and checksum, validates canonical framing and entry/descriptor identity, and seals an immutable staged object. Hashing and file I/O are incremental and bounded. Its response means ready-for-use, not appended, committed, applied or snapshot-installed. No missing suffix, gap or invalid checksum is silently repaired as complete data.
- Owned append messages may reference a sealed entry by transfer identity, full log identity, total length and checksum. Normal preceding-log, term and membership rules are rechecked at consumption. Only complete canonical bytes entering the durable log transaction permit a replication acknowledgment. An expired/missing staging reference requests retransmission; it does not advance match position. A duplicate append after a lost response resolves against the durable log, not against staging existence.
- Snapshot installation consumes only snapshot-purpose objects and follows its separate durable install transaction. Entry-purpose objects cannot install application snapshots or bypass quorum commitment.
- `abort` deletes only the matching unconsumed staging object and releases reservations. It cannot truncate the log, cancel committed intent or destroy executions. Successful abort after log consumption means there is no remaining staging object, not that the entry was rejected.

Initial scheduling bounds are 64 KiB raw chunks, 1 MiB complete encoded
transfer-service messages, two active incoming transfers per peer and eight per
node. A node admits at most eight concurrent incoming chunk handlers and eight
outgoing chunk requests; control messages have a separate bounded queue so bulk
transfer cannot consume their entire scheduling capacity. The sender checks the
fully wrapped IPC envelope against the unchanged core maximum, not merely raw
chunk size. Codec tests must establish envelope overhead for these bounds. Bulk
backpressure never suppresses higher-term handling or extends execution
authority.

Staging disk quota is local resource configuration, not a consensus format
capability: reservation failure returns capacity-unavailable without accepting a
proposal or claiming replication. The implementation must supply finite per-peer
and aggregate quotas, validate them at startup, and expose rejected reservations
diagnostically. A leader counts only replicas that completed durable append, not
peers that reserved space. Operators must provision sufficient quota for the
largest retained entry before cutover; migration preflight reports oversized
retained objects explicitly and leaves old canonical state untouched. It cannot
silently reduce supported command semantics.

Staging is disposable after restart: recovery removes only owner-scoped staging
artifacts and starts with no resumable transfer state. The sender begins a new
transfer identity after receiver restart or unknown-transfer response. Live
transport reconnect may query `begin` for the same transfer only after fresh
authentication. Leadership/term change invalidates old-term staging, and a
30-second no-progress timeout releases abandoned reservations. Explicit timer
events drive expiration; repeated `begin`, duplicate chunks, and empty messages
do not renew progress. Cleanup failures retain their reservations and surface an
error rather than claiming capacity has been restored. Shutdown stops admission,
cancels transfer tasks, cleans owned temporary state, and reports incomplete
cleanup.

### Versioned aggregate admission decision

The approved direction is explicitly versioned aggregate command admission, not
extending consensus to carry every multi-gigabyte value admitted by the old
per-field validators. This is an explicit compatibility decision: old interface
meanings and canonical encodings are not silently narrowed. New aggregate limits
require a separately negotiated control-submission contract and a committed
capability/schema transition before enforcement for that contract. Merely
connecting to an owned-engine endpoint does not negotiate new command semantics.

The limit measures the entire encoded canonical command, including principal,
command identity, tags, lengths and all nested fields, not individual byte
fields or compressed network size. Validation computes the size with checked
arithmetic before allocating a complete encoded buffer and rejects oversized
new-contract submissions before proposal. Receiver validation enforces the same
bound. Runtime queue/memory/disk budgets are separate local capacity controls,
not changes to the canonical command format. Exact numerical command limits and
interface schemas require source-grounded sizing and boundary tests before this
decision is implemented.

Migration preflight examines the authoritative retained log, canonical state,
deduplication records and unfinished workflows against supported
representations. An oversized retained record blocks an incompatible cutover
with an explicit diagnostic; it is not truncated, dropped, treated as corruption
solely because the new admission limit is smaller, or silently converted into an
empty installation. Old-contract retries must preserve their durable outcome and
fingerprint semantics. The replacement must either retain an explicitly
supported decoding/application path for migrated history or refuse the
incompatible migration before changing authority. Approval of bounded new
admission is not approval to rewrite history or delete data.

State growth is separate from per-command admission. The existing 256 MiB
whole-state snapshot encoder is an implementation limit, not a new permanent
workspace-size policy authorized by this decision. Durable application, snapshot
generation and recovery must have explicit supported state-size/resource
behavior; if qualification requires record-oriented persistence or streaming
snapshots, that work belongs to the persistence phase and cannot be deferred as
optional cleanup. This decision does not authorize a speculative storage rewrite
before those integration requirements are established.

### Remaining wire qualification

The size investigation is complete enough to rule out choosing limits from
per-field constants: maximum argument/environment lists alone encode to
3,221,422,088 bytes, while two maximum placement-label lists alone encode to
4,295,229,448 bytes, exceeding the existing durable u32 byte-length field. The
versioned aggregate-admission decision above resolves the architectural choice;
concrete command limits, finite staging quota defaults, schema allocation guards
and final append limits still require implementation and validation. Decoder
validation must occur before large allocation, and complete entries must not
force unbounded work in one engine transition. These are required
implementation-model acceptance gaps, not optional performance polish.

#### Error and retry semantics

Admission failures distinguish invalid request/representation, unsupported
protocol, unauthenticated peer, unauthorized mutation, failed precondition,
unavailable capacity and unavailable runtime. Not-leader responses may include
authenticated routing guidance but cannot promise the hinted leader remains
authoritative.

Once a proposal may have entered replication, loss of quorum, timeout,
cancellation or connection loss produces an indeterminate outcome unless durable
state proves a terminal result. Retrying uses the same application mutation
identity; conflicting reuse is rejected. Cancellation stops waiting, not
committed intent or running worker processes. Membership errors distinguish
rejection before proposal from an incomplete workflow after a joint
configuration committed.

Storage failure, corrupt authoritative state, impossible identity conflicts and
arithmetic exhaustion stop the affected authority and require explicit recovery.
They are not translated into healthy empty state, ordinary leader redirection or
successful restoration. Shutdown completion covers owned tasks and pending
responses; dropping a request or stopping a node does not authorize destroying
server-owned executions.

### Safety and durability

The replacement must preserve election safety, log matching, committed entries,
current-term majority commit rules, and safe serialized joint-consensus
membership changes. Learners do not gain voting authority through generic member
metadata updates.

Term and vote persistence precedes dependent responses; replication
acknowledgment follows required durable log persistence. Quorum commit and
durable application are distinct stages. Successful application responses follow
the required atomic durable transaction covering canonical state, applied
position, membership, deduplication outcomes, and unfinished workflows.

Authoritative reads establish current quorum authority and wait for the required
applied position. Authorization, mutation preconditions, capability admission,
and fencing are checked at their authoritative boundaries, including across
asynchronous waits and leadership changes. Loss of quorum cannot authorize
control mutation.

Storage failure stops the affected node from voting or serving authoritative
mutations on uncertain durability. Corrupt or unsupported durable state is an
explicit error, not an empty installation. Snapshot installation and compaction
preserve coherent state, membership, applied position, and recovery authority.

Replication batches, outstanding requests, queues, retained buffers, snapshot
transfer, application work, and per-peer work must be bounded. Slow peers
receive backpressure, catch-up, or explicit failure rather than silent loss.

### Existing storage and application decisions

ADR-0010's redb selection, durable transactions, corruption handling, snapshot
publication, cluster binding, and fail-closed behavior remain in force. Its
OpenRaft trait names describe the existing adapter; owned persistence operations
must preserve their ordering obligations. This decision does not declare
existing bytes compatible with new types or authorize an unversioned rewrite.

ADR-0011's deterministic command semantics and canonical application authority
remain in force. In the replacement, trusted log identity and committed
membership context come from the owned consensus adapter rather than OpenRaft.
Product member records still do not themselves grant votes.

### Compatibility and migration

Protocol, storage, and engine compatibility are separate explicit contracts.
Similar wire fields or Rust types are not evidence of interoperability. Existing
interface meanings must not silently change; new representations require
negotiated support and unsupported combinations must be rejected.

The approved replacement is a clean break for fresh installations. Legacy
import, maintenance-window migration, mixed-engine rolling operation, and
downgrade into OpenRaft are not supported. This supersedes the previously
proposed migration direction; it does not authorize deleting or reinterpreting
existing authoritative data.

The owned runtime must use an explicitly identified owned durable format and
hold exclusive ownership of its store before initialization or recovery.
Startup encountering a legacy or unsupported store fails explicitly without
modifying it; it must not select another directory and present an apparently
empty installation. New-store initialization must be interruption-safe,
distinguish incomplete initialization from corruption, and acknowledge success
only after the required durable commit. Concurrent initializers cannot create
competing authorities. Fresh clusters use new cluster identities and
authenticated enrollment, never adopt old workers or resources implicitly.

No import or rollback workflow is required for this replacement. Normal
owned-format restart, snapshots, unfinished workflows, logical identities,
fencing and lease recovery remain required. Existing workers and stores are
not destroyed by a failed startup; worker unreachability remains distinct from
death. Future migration support would require a separate architectural decision.

## Qualification gates

Production replacement requires all of the following, independently of this
ADR's acceptance:

1. Reviewable engine types, transition rules, persistence-before-effect ordering, supported failure assumptions, and compatibility contracts.
2. Deterministic simulation and model checking of safety-critical transitions, with reproducible fault traces.
3. Crash-injection evidence for durable acknowledgments, snapshots, corruption, restart, and unfinished workflow recovery.
4. Three- and five-voter process tests covering partitions, leader changes, quorum loss, membership transitions, slow peers, and recovery.
5. Existing authenticated services, bootstrap, capability publication, control mutations, authoritative reads, and worker reconcilers connected end to end, including cancellation, shutdown, and failure reporting.
6. A tested fresh-install boundary: explicit legacy/unsupported-store rejection without mutation, exclusive store ownership, interrupted initialization recovery, and owned-format restart safety.
7. Reliability and performance qualification against ADR-0008 and independent review of consensus and persistence safety.
8. Removal of OpenRaft dependencies and obsolete adapters, mechanical boundary guards, updated operational documentation, and repository-required validation as defined in `AGENTS.md`.

Neither a simulator-only implementation nor passing compilation establishes
product closure. No architectural or recovery obligation may be deferred as
cleanup to declare the replacement complete.

## Consequences

BMUX gains implementation ownership but assumes substantially more
safety-critical code and review responsibility than ADR-0009's library-based
approach. Existing adapters and tests provide integration evidence, not
automatic proof of replacement correctness. Engine delivery, production
integration, and fresh-install qualification remain separate obligations.

No invariant is weakened: quorum authority, authentication, deterministic
application, durable acknowledgments, execution fencing, resource identity, and
plugin ownership keep their existing meaning.
