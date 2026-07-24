# ADR-0001: Native Federated Workspace Model

- **Status:** Accepted
- **Date:** 2026-07-23

## Context

bmux currently supports several remote transports and a bundled cluster plugin. `cluster up` creates an outer local session and starts `bmux connect <target> --reconnect-forever` in one pane per configured target. This is useful orchestration, but each remote server remains a nested terminal application with separate session, layout, focus, and command handling.

The intended product is one cohesive bmux experience across several machines. The design must preserve ordinary local bmux operation and the repository rule that sessions, windows, panes, clients, permissions, and cluster policy are plugin domains rather than core architecture.

## Decision

bmux federation presents a **logical workspace** whose windows, layout, and logical panes are cluster-owned metadata. Each logical pane has at most one authoritative execution generation, hosted by a worker node. The user attaches once through any healthy ingress node; the ingress composes one native attach view from control metadata and worker terminal streams.

The canonical target form is:

```text
cluster://<cluster-reference>/<workspace-reference>
```

The target selects a cluster attach provider. It does not identify the permanent owner of the workspace and does not bind the client to one ingress.

### Native pane behavior

A federated pane is rendered as a normal bmux pane using neutral attach/layout/scene/terminal protocol types. The federated path does not launch a nested `bmux connect` client. Input, resize, focus, split, close, rename, zoom, and window navigation address logical entities; the cluster plugin resolves the active execution and authoritative service.

Node placement may be displayed in optional status or diagnostics, but routine commands cannot require the user to know it.

### Logical ownership

The replicated cluster control plane owns:

- Workspace identity and display metadata
- Logical windows and their ordering
- Layout and logical pane placement in that layout
- Placement intent and active execution assignment
- Execution generation and restart policy
- Durable command outcomes needed for idempotency

Workers own:

- PTY/process handles
- Worker-local pane/session identifiers
- Terminal parser/grid and retained output
- Execution-local snapshots and cursors
- Local resource accounting

Ingress nodes own only transient client connection and composition state. Ingress loss cannot change logical ownership or terminate worker executions.

### Local behavior and missing plugins

Local single-server attach remains the default baseline and does not require cluster configuration. If the cluster plugin or its client adapter is unavailable:

- Local targets and ordinary local sessions continue to work.
- Existing named SSH/TLS/Iroh target behavior remains available through its owning connection layer.
- `cluster://` resolution fails explicitly with an actionable missing-provider error.
- Core does not synthesize cluster behavior.

### Existing sessions

Existing node-local sessions are discoverable but remain local. They enter the global namespace only through an explicit promotion transaction. Promotion adopts running panes without restarting them when their runtime can be represented safely. Failure before authoritative commit leaves the local session usable and not partially published.

### Compatibility mode

The current nested orchestration path remains a compatibility mode while native federation is developed. `cluster up` retains existing behavior until native attach has parity and a documented migration is ready. After migration, `cluster up` may become an alias for workspace create/attach; a clearly named legacy mode may remain temporarily.

## User-visible semantics

- A workspace name is a display/reference convenience; stable identity is `WorkspaceId`.
- A logical pane remains in the layout while its worker is unavailable.
- An unavailable pane renders a native placeholder with reason, last worker, generation, and valid recovery actions.
- Gateway/leader reconnect is represented as transient attach status, not a new session.
- Commands are either committed once, rejected, or reported indeterminate with the same command ID available for safe resolution. They never silently execute as a second logical mutation.
- Moving a pane means controlled replacement on another worker. It is not live process migration.
- A workspace may combine newly launched panes and explicitly adopted local panes.

## Non-goals

- Live migration of arbitrary process/PTY operating-system state
- Automatic federation of every local session
- Writable independent minority partitions with later merge
- Initial hostile multi-tenant isolation
- Replication of terminal byte streams through consensus
- Replacing the local pane-runtime implementation with a cluster-specific core runtime

## Architecture placement

Cluster semantics and composition belong in `cluster-plugin` and a plugin-owned client attach adapter. Stable contracts belong in `cluster-plugin-api`. Core attach code may expose a generic provider seam and neutral view protocols but may not branch on cluster identity or contain workspace placement rules.

A neutral connections plugin should own transport target resolution and remote endpoint invocation. Cluster code chooses endpoints according to cluster policy and calls that generic contract.

## Consequences

### Positive

- The user interacts with one native workspace rather than nested multiplexers.
- Ingress and worker lifecycles are decoupled.
- Existing pane runtime and neutral rendering protocols remain reusable.
- Local operation remains simple and independent of federation.

### Costs

- Attach composition must merge independently ordered worker streams.
- Logical command routing and authorization require an authoritative control layer.
- Native federation cannot be delivered by polishing the current nested process model alone.

## Rejected alternatives

- **Keep nested clients permanently:** rejected because focus, layout, permissions, commands, and reconnect remain visibly fragmented.
- **Make one permanent coordinator own the workspace:** rejected because loss of that process would violate ingress/control availability goals.
- **Move session/window/pane federation into core server:** rejected by the plugin architecture boundary and because federation is product-domain behavior.
- **Automatically import every local session:** rejected because it changes ownership and failure semantics without explicit user intent.

## Acceptance criteria

This decision is implemented when one attached client can use a logical workspace with panes on at least three workers, native commands are location-transparent, no nested bmux client is used in the federated path, and disabling the cluster plugin leaves baseline local attach behavior intact.
