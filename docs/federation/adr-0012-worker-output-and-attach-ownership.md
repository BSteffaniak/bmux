# ADR-0012: Worker Output Transport and Federated Attach Ownership

- **Status:** Accepted
- **Date:** 2026-07-25

## Context

Federated attach must compose one native workspace from control metadata and terminal streams owned by several workers. The generic attach seam is intentionally domain-agnostic, while cluster routing, execution generations, output cursors, snapshot repair, and failover policy are cluster concepts. The worker transport must remain bounded and must not require a new core streaming protocol before correctness is established.

## Decision

### Attach adapter ownership

The federated attach adapter is a plugin-owned client artifact paired with `cluster-plugin`; the current implementation lives in `plugins/cluster-plugin-client`. It registers a provider of the existing generic attach contract only when `bmux.cluster` is bundled and enabled. It may use private helper modules and generated `cluster-plugin-api` clients. Native rendering integration remains an implementation detail of this paired artifact and does not own server policy or stable transport envelopes.

Core attach code receives only neutral provider snapshots, deltas, controls, resume state, and status. It does not parse `cluster://`, choose members, inspect placement, or understand executions.

### View-local versus durable state

Focus, zoom, and the selected logical window are client attach-view state. They are validated against the current scene/control revision but are not proposed through Raft. Workspace and window names, logical layout, pane lifecycle, placement, execution generation, availability, and restart policy are durable cluster control state and mutate only through typed, idempotent control commands.

### Persistent personal arrangements (2026-09-09 amendment)

A personal arrangement is durable, principal-owned metadata, distinct from an attachment's interaction state and from the lifetime of the resources it references. This classification extends the original decision without making existing attach-local controls durable.

- The cluster plugin is the sole authority for federated arrangements. View identity, owner, name, schema version, revision, initialization state, workspace-specific ordered references, and personal label overrides are replicated control metadata. Ingress-local files and presentation caches are not alternate authorities; there is no offline mutation journal or automatic merge path.
- References carry their authority and logical resource identity, never worker-local execution IDs or connection IDs. A view belongs to one authority. Worker replacement and ingress changes do not change its references.
- Arrangement mutations use typed, versioned services with authoritative principal authorization, expected revisions, and idempotent command identities. Success follows the required durable quorum commit. Minority partitions cannot acknowledge arrangement changes. Replicated application remains deterministic and side-effect-free; bounded retry outcomes, initialization state, and any incomplete workflows survive snapshots.
- Multiple attachments explicitly selecting the same owned view share membership, order, and labels, but not active selection, focus, zoom, or navigation history. Independent views remain independent. Temporary follow presentation does not modify personal membership.
- Removing a reference or deleting a view never closes the referenced window. Zero references do not authorize termination. Removed work remains discoverable when authorized; destruction remains an explicit operation enforced by the existing resource authority.
- Presentations and navigation consume the same authoritative arrangement. Disabling the tab strip or sidebar does not mutate it. Removing or invalidating the selected reference resolves or suspends the attachment's input target before more input can reach hidden work; an explicitly empty initialized view remains empty.
- Fresh attachment without a selector resolves the owner's default view; explicit selection and reconnect preserve the chosen view when valid. Resume reauthenticates and reconciles committed arrangement and resource state before enabling input. A missing reference, incomplete catalog, access revocation, and confirmed resource deletion are distinct outcomes. Resume metadata contains no reusable credential and is interpreted by the plugin-owned adapter, not core.

This amendment defines required semantics, not the availability of an implemented capability. New arrangement services and resume representations require explicit version/capability negotiation under ADR-0007. Existing v1 attach controls retain their meaning; unsupported peers must reject arrangement-specific requests rather than silently reinterpret shared workspace order as personal state. Migration seeds an initialized default view once from canonical existing order, preserves intentional emptiness and interruption recovery, and rejects unknown or corrupt authoritative representations. Introducing arrangement support does not silently promote local resources into federation.

Local durable arrangements remain windows-plugin-owned and independently usable without federation. They use local crash-safe storage rather than cluster consensus; local single-user fallback does not bypass federated authentication or authorization.

The reason for this extension is reconnectable personal organization over shared running work: attachment-local files cannot provide one durable authority across ingress failover, while consensus-backed selection would incorrectly couple independent attachments.

### Worker output transport

Version 1 uses bounded long-poll queries through generated `cluster-worker-state/v1` services:

- `output(execution_id, generation, cursor, max_bytes)` returns a bounded contiguous batch, retained-range metadata, a next cursor, and whether output remains immediately pending.
- Each request is bound to exactly one execution generation. A generation change requires control reconciliation and a fresh snapshot.
- A cursor older than retained output returns an explicit gap/retained-start result and triggers `snapshot` repair; the server never fabricates continuity.
- Workers cap `max_bytes`, retained bytes per execution, concurrent polls, and queued response work. Empty polls use a bounded server wait and client cancellation/deadline.
- Ingress polls independent workers concurrently with bounded fan-out so one slow worker cannot block unrelated panes.
- Terminal bytes and snapshots never enter consensus.

Long-poll is the required first implementation because it composes with the existing endpoint-aware typed-service path, its cancellation and retry boundaries are explicit, and it is straightforward to test for bounded memory and cursor repair. A future multiplexed streaming revision may be negotiated as an optional feature only after parity tests prove identical generation, cursor, cancellation, and backpressure semantics. Streaming is not required for the first release and cannot silently replace the v1 contract.

## Consequences

- Cluster policy and URI handling stay out of core architecture.
- View-local focus, zoom, and selected-window changes avoid unnecessary quorum writes while durable logical mutations remain authoritative.
- The first implementation can prioritize correctness and bounded behavior without introducing a second transport stack.
- Poll overhead may be higher than a mature multiplexed stream; performance budgets determine whether a later optional revision is justified.
- Worker output and attach implementation can be tested using generated service clients and the synthetic generic attach-provider harness.

## Acceptance criteria

1. Architecture guardrails find no cluster URI, membership, placement, or execution concepts in core attach layers.
2. Cursor-gap tests prove repair through a complete snapshot and no output is applied across generations.
3. Slow-worker tests prove bounded polling fan-out, queues, retained output, and cancellation.
4. Local and cluster attach providers coexist, and disabling the cluster plugin preserves baseline attach behavior.
5. Any future streaming revision has explicit capability negotiation and behavioral parity tests with v1 long-poll.
6. Arrangement mutations require quorum, validate owner/revision/command identity, and survive snapshot restore and ingress failover without changing logical references.
7. Independent views do not share mutations; attachments selecting one view share durable arrangement changes but retain independent selection and navigation history.
8. Personal removal and view deletion preserve running resources and authorized discovery; selected-reference removal and access loss cannot leave input routed to hidden work.
9. Migration preserves canonical order and intentionally empty initialized views; unsupported capabilities and corrupt or unknown durable formats produce explicit errors rather than fallback reinterpretation.

## Implementation location

The accepted adapter is implemented in `plugins/cluster-plugin-client`; the generic endpoint connector is `AttachEndpointConnector` in `bmux_client`, with the CLI transport implementation in `packages/cli/src/connection.rs`. The core attach runtime never parses cluster targets or imports the cluster API.
