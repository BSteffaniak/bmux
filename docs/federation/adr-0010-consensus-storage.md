# ADR-0010: redb Storage for OpenRaft Durable State

- **Status:** Accepted
- **Date:** 2026-07-24

## Context

OpenRaft requires durable vote/committed state, an ordered log with truncation, an applied state machine, membership metadata, and crash-safe snapshots. Generic plugin key/value storage does not expose the transaction, flush, file-layout, or corruption guarantees required for consensus.

The storage implementation must be plugin-owned, portable across BMUX platforms, easy to test under crash injection, and free from a large native build/runtime dependency.

Candidates evaluated were redb, SQLite through rusqlite, Fjall, RocksDB, and handwritten append-only files.

## Decision

Use **redb 4.x**, exact-pinned to a reviewed patch release, as the embedded storage engine for OpenRaft 0.9.x.

The database lives under a cluster-plugin-owned state directory:

```text
<state_dir>/plugins/bmux.cluster/consensus/<cluster-id>/
  raft.redb
  snapshots/
    <snapshot-id>.tmp
    <snapshot-id>.snapshot
```

The exact path helper must reject traversal and bind an existing database to the expected `ClusterId` and storage schema before opening it.

### Database tables

Use explicit byte-oriented tables so durable compatibility is owned by BMUX rather than Rust type layout:

- `meta`: storage format version, cluster ID, active snapshot ID/checksum, migration state.
- `hard_state`: OpenRaft vote and committed log ID.
- `log`: big-endian log index to versioned canonical entry bytes.
- `state_machine_meta`: last-applied log ID, last membership, application schema version, logical revision.
- `state_machine`: versioned deterministic application records.
- `dedup`: `CommandId` to request fingerprint, terminal/in-progress status, and canonical response/workflow state.

Keys that require ordered iteration use fixed-width big-endian integer encoding. Values use explicit versioned envelopes and canonical serialization. Do not persist OpenRaft internal Rust values with unversioned `bincode` or rely on enum discriminants.

### Durability and transaction rules

- redb write transactions use durable commit for all acknowledged consensus writes.
- `RaftLogStorage::save_vote` durably commits before returning.
- Log append/truncate and committed-state updates use transactions preserving OpenRaft's required ordering. A successful OpenRaft flush callback is fired only after the redb durable commit completes.
- Applying a committed batch updates state-machine records, dedup outcomes, last-applied ID, and last membership in one redb transaction.
- Blocking redb work runs on a bounded `spawn_blocking` path; terminal IO and attach loops never execute database operations directly.
- One writer is serialized per node; reads use independent read transactions.
- A storage IO failure is fatal to the local consensus runtime. The node stops voting/serving authoritative mutations rather than continuing from uncertain durability.

### Snapshot rules

State-machine snapshots are immutable versioned files outside the database so OpenRaft can stream them without holding a redb transaction.

Snapshot creation:

1. Read a consistent redb transaction at a committed `last_applied` and membership.
2. Encode a canonical snapshot envelope containing storage/application schema versions, `ClusterId`, last-applied ID, membership, state records, and dedup records that must survive compaction.
3. Stream to a uniquely named `.tmp` file while hashing.
4. Flush file contents with `sync_all`.
5. Atomically rename to `.snapshot` in the same directory.
6. `sync_all` the snapshot directory.
7. Durably update `meta.active_snapshot` and checksum in redb.
8. Only then expose the snapshot to OpenRaft and make older snapshots eligible for deletion.

Snapshot install:

1. Stream into a new `.tmp` file with a strict size bound and checksum.
2. Validate envelope version, cluster ID, last-applied/membership consistency, key ordering, duplicate keys, and checksum before touching active state.
3. Build a new redb database file from the snapshot in a sibling temporary path.
4. Durably flush the new database and file, then atomically swap it into place with a recoverable manifest.
5. Sync the parent directory, reopen, and run all invariants before acknowledging installation.
6. On interruption, startup uses the manifest and checksums to select the last fully committed database; it never guesses from modification time.

Snapshot building and installation run off terminal data paths and are cancellable before the atomic publication point.

### Startup and corruption behavior

Startup fails closed if any of these occur:

- redb reports corruption or cannot recover the last committed transaction;
- cluster ID or storage format does not match;
- hard state references unavailable log/state-machine history;
- log indices are malformed, unordered, or contain undecodable envelopes;
- last-applied/member metadata conflicts with the active snapshot;
- snapshot checksum or envelope validation fails;
- an interrupted replacement manifest cannot identify one complete valid database.

No automatic reset, truncation, or new cluster initialization is allowed. Recovery requires an explicit operator command that preserves the damaged directory, reports the exact invariant failure, and restores from a verified snapshot or healthy voter.

### Migration policy

- `meta.storage_format_version` and application snapshot schema are independent.
- Every migration is an explicit idempotent step recorded in `meta.migration_state`.
- Migrations write a sibling database, validate it, atomically replace the old file, and preserve the prior database until successful startup.
- Downgrades never rewrite data silently. A binary that cannot read the on-disk version exits with an actionable compatibility error.

## Evaluation

| Candidate                     | Durability/transactions                                                                | Portability and dependency cost                                                                              | Fit                                                                          |
| ----------------------------- | -------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------- |
| redb 4.x                      | Pure-Rust ACID transactions, MVCC readers, crash-safe-by-default copy-on-write B-trees | No C/C++ toolchain or external runtime; one compact embedded dependency                                      | Selected: sufficient semantics with low operational/build cost               |
| SQLite/rusqlite               | Extremely mature WAL/rollback durability and integrity tooling                         | Bundled SQLite adds native compilation/FFI and a larger feature/build surface                                | Strong fallback if redb fault testing exposes unacceptable recovery behavior |
| Fjall                         | Pure-Rust LSM, atomic cross-keyspace batches/transactions, explicit persistence modes  | Background compaction and a larger LSM operational surface than needed for modest control metadata           | Rejected for initial implementation complexity                               |
| RocksDB                       | Mature WAL, column families, snapshots, compaction                                     | Large C++ dependency, long builds, platform packaging burden, substantial tuning surface                     | Rejected for disproportionate dependency and operational cost                |
| Handwritten append-only files | Full format control                                                                    | BMUX would own WAL framing, torn-write recovery, indexing, compaction, transactions, and corruption handling | Rejected as unnecessary custom storage infrastructure                        |

## Consequences

### Positive

- Consensus storage gains transactions and crash recovery without adding native dependencies.
- Ordered log scans and atomic state-machine/dedup updates map directly to redb tables.
- Snapshot files can be streamed independently from database locks.
- Explicit envelopes preserve compatibility across OpenRaft and Rust upgrades.

### Costs and risks

- redb is less battle-tested than SQLite/RocksDB; exhaustive kill/fault/corruption testing is mandatory.
- redb operations are synchronous and require a bounded blocking executor.
- Snapshot/database replacement and directory fsync are BMUX-owned platform-sensitive code.
- A later engine change requires an explicit offline migration/export format.

## Rejected alternatives

- **Generic plugin storage:** lacks the required WAL flush and transactional semantics.
- **Separate files for vote, log, and state machine:** makes atomic cross-component invariants and recovery substantially harder.
- **Best-effort recovery by deleting bad records:** violates Raft durability and fails closed requirements.

## Acceptance criteria

1. Exact redb and OpenRaft versions are pinned.
2. Storage-v2 trait tests cover vote persistence, append/flush, truncation, purge, application, membership, snapshots, and restart.
3. Kill tests terminate the process before/during/after each durable commit and snapshot publication step.
4. Corruption tests alter every durable region and prove startup either recovers the last committed state or fails closed.
5. Snapshot install interruption tests prove atomic selection of old or new complete state.
6. Log compaction preserves required dedup outcomes and never blocks attach/terminal processing.
7. A verified snapshot from a healthy voter restores a replacement node without changing cluster identity.

## Sources reviewed

- redb 4.1 documentation and release metadata: pure Rust, ACID transactions, MVCC, copy-on-write B-trees, crash-safe defaults.
- SQLite/rusqlite release metadata and SQLite WAL/transaction model.
- Fjall 3.1 documentation and release metadata: pure-Rust LSM, cross-keyspace atomic semantics, persistence modes, background maintenance.
- RocksDB Rust crate release metadata and native dependency profile.
- OpenRaft 0.9.24 storage-v2 and snapshot interfaces.
