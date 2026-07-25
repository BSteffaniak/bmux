# ADR-0014: Promotion Is One-Way in the First Federation Release

- **Status:** Accepted
- **Date:** 2026-07-25

## Context

Promoting a running local session transfers authoritative identity and mutation ownership into replicated federation state while preserving existing PTYs. Automatically reversing that ownership would require a second distributed transaction across control quorum, worker-local runtime state, layout conversion, permissions, output cursors, and potentially several workers. A partial demotion could create two authorities or lose live processes.

## Decision

The first federation release treats successful promotion as one-way. It does not provide live transactional demotion.

Before commit, promotion remains cancellable and the source local session remains authoritative and unchanged. After the replicated promotion commit and worker finalize complete, the federated workspace is authoritative. The original source metadata is retained as recovery/audit provenance but is not a second writable session.

Users can leave federation through explicit snapshot/export workflows:

- Export logical workspace/window/layout metadata, pane history/snapshots, launch specifications where available, placement and restart policy, and an integrity manifest.
- Restore the export as a new local session or a new federated workspace; restoration creates new local execution identities and does not claim live process migration.
- If a live pane cannot be represented or safely relaunched, export reports it before destructive action and preserves available terminal history.

No command may label export/restore as demotion or imply that running distributed processes moved into one local server. A future live demotion feature requires a superseding ADR and the same prepare/commit/finalize/failure-injection rigor as promotion.

## Consequences

- Promotion has one authoritative ownership transfer and avoids split ownership.
- Operators retain a documented escape path through portable snapshots/export.
- Returning to local execution may restart processes and therefore requires explicit user action.

## Acceptance criteria

1. CLI help and operator documentation state that promotion is one-way in v1 before confirmation.
2. Failure before promotion commit leaves the source local session authoritative and unchanged.
3. Failure after commit recovers toward federated ownership; it never silently rolls back to a second writable local authority.
4. Export includes integrity metadata and clearly reports non-replayable panes.
5. Restore creates new execution identities and never claims live migration.
