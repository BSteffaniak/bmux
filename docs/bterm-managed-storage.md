# Managed companion durable storage

Managed target identity separates transport/configuration scope from the durable
installation. `ManagedTargetIdentity.durable_roots` is captured before routing;
`selection.roots()` must retain exactly its `data_dir` **and** `state_dir`.
Artifact/configuration changes may select new config and transport directories,
not an empty durable installation. External, slot, and borrowed sandbox targets
retain their existing semantics and grant no managed migration authority.

## Implemented writer coordination

The host storage service rereads durable values on every get. It deliberately
has no positive or negative process-local cache: a domain transaction in another
companion must become visible before the next read. Set still publishes the
complete value by synchronized temporary-file replacement and directory sync.
A host set alone is **not** a read/modify/write transaction.

Tabs owns complete arrangement and transfer transactions. Existing arrangement
mutation sections acquire a process mutex plus an independently opened OS file
lock at `data_dir/plugin-storage/bmux.tabs.arrangements.lock`. Transfers use
`bmux.tabs.transfer.lock` across pending intent and placement completion, taking
the arrangement fence only for the corresponding durable mutations. Migration,
initial arrangement creation, snapshot reads/restores, and arrangement mutations
participate. A lock file is retained; only its OS lock represents ownership.
Cross-process acquisition waits at most two seconds and never kills an owner.

The create-only synchronized marker
`plugin-storage/bmux.tabs.arrangements.writer` must contain exactly
`bmux.tabs/arrangements-writer-v2\n`. Missing markers are initialized only
inside
the domain transaction; unknown, partial, or corrupt markers are errors, never
permission to overwrite or select new empty data.

Cluster owns identity/membership and connection-event append transactions under
`data_dir/plugin-storage/bmux.cluster.mutation.lock`. The matching marker is
`plugin-storage/bmux.cluster.writer`, containing exactly
`bmux.cluster/durable-writer-v1\n`. Identity initialization, existing membership
mutation critical sections, leave preparation/commit, endpoint advertisement,
and read-side legacy membership migration use this domain transaction.
Synchronous nested membership helpers retain the actual transaction fence until
the final nested handle drops. Disk reads inside transactions are fresh.

Cluster consensus retains the existing node identity and the redb history at
`state_dir/plugins/bmux.cluster/consensus/<cluster-id>`. The database's existing
exclusive ownership is the writer authority; it is acquired **before** snapshot
publication recovery mutates files. A second companion cannot initialize a new
node over the same history or clean the existing owner's pending publication.
It receives a consensus capability failure. Background consensus bootstrap
failure does not prevent baseline terminal operation. An already advertised
node endpoint is not implicitly changed merely because a new target selected a
different endpoint. No global daemon-lifetime storage lock is introduced.

## Managed snapshot catalog

The snapshot plugin retains the legacy `data/runtime/bmux-snapshot-v1.json`
unchanged. Managed snapshots live in the **same durable installation** as a
versioned catalog at `data/runtime/bmux-managed-snapshots-v1.json`. Its
`targets` object maps each exact target ID to its full target identity and
checksummed combined envelope. Reads validate every retained entry; unsupported
or corrupt catalog data is an explicit error, not an empty layout. Replacing an
entry requires full stored identity equality.

Snapshot-owned bounded OS transactions cover fresh catalog read, entry merge,
and durable commit, preserving other targets' snapshots. Publication uses a
create-only PID/sequence temporary file, sync, rename, and required directory
sync, never a shared truncating `.tmp`. The per-runtime lifecycle fence still
covers participant collection/finalization. Different targets can publish
without replacing each other's entries; old unscoped binaries continue writing
only the separate legacy file, not the managed catalog.

Only a matching target entry is automatically restored. Legacy and other target
layouts are retained and explicitly discoverable through startup diagnostics
and snapshot dry-run, including paths and target IDs. There is no "latest"
fallback, implicit import, or cross-target restore. Snapshot status reports
existence for the selected target, not merely whether another target has a
catalog, and reports catalog read/validation errors.

Managed restore leaves shared `bmux.tabs/arrangements` and
`bmux.workspaces/catalog` with their current domain owners instead of replacing
them from a historical target snapshot. Their sections remain in retained
snapshots for explicit recovery. Same-target runtime sessions, contexts, and
pane layout restore through existing ordered participant dependencies. This
preserves authoritative user arrangement data across builds without treating
another runtime's processes or IDs as new launch authority.

No CLI bootstrap API change is required: the existing snapshot config supplies
the stable legacy path. The plugin captures already-validated managed target ID
and full identity at activation; standalone sandbox/unmanaged snapshot behavior
continues using the supplied legacy path.

## Integration obligations

Bterm's private `managed_storage::validate_managed_storage_roots` is a read-only
path-preservation check, not a writer lease or readiness proof. Wire it under
the
selected target's startup fence before spawn using identity `durable_roots` and
`selection.roots()`. It does not scan or reject unrelated execution receipts.

Pane execution receipts require owning-target routing under the stable state
root. New independent targets must not claim old reservations or interpret old
launch intent as permission to launch again. Existing receipt locations and
supervisor recovery evidence remain available to their original owners; scope
routing is not migration, cleanup, or proof that detached work finished.

These locks coordinate **participating writer implementations only**. A new
marker cannot retroactively fence an old live executable that ignores it.
Managed shared-writer admission must prove the explicit compatible writer
contract for every writer; uncertain legacy ownership is not safe admission.

Other durable domains require their own whole-mutation coordination. In
particular, workspace catalog/history mutations use domain-cached state, and
permissions mutations replace policy documents. Host cache removal cannot make
those caches authoritative or serialize those mutations. Do not treat the tabs
and cluster changes, or the Bterm root check, as certification of every plugin's
shared-writer safety. Domain owners must integrate those contracts before
claiming installation-wide concurrent writable support.

Standalone sandbox handoff and inherited sandbox configuration remain unchanged;
they continue borrowing their explicit isolated roots rather than acquiring
managed installation or lifecycle authority.
