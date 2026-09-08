# BMUX Invariants

These invariants describe conditions that must remain true across valid changes to BMUX. They are acceptance criteria, not implementation suggestions.

An invariant is a durable condition of a valid product or architecture. Contributor workflow and validation commands belong in `AGENTS.md`; design mechanics and rationale belong in `docs/`; implementation status and migration details belong in progress documents. Existing violations do not create implicit exceptions.

## Product boundaries

* **BMUX remains independently usable.** Integrations, remote services, and federation must not become prerequisites for local terminal operation.
* **Plugins are first-class product owners.** Plugins may implement critical product behavior; extensibility is not limited to cosmetic or peripheral features.
* **Baseline operation survives missing domain plugins.** Without windows, sessions, contexts, or clients plugins, baseline single-terminal attachment and execution remain available through neutral runtime mechanisms, not duplicate domain implementations in core.
* **The permissions fallback is local and single-user.** Absence of the permissions plugin preserves permissive single-user operation. It does not waive authentication or authorization required by remote or federated services.
* **Presentation is optional.** Enabling, disabling, or replacing tab strips, sidebars, or decorations does not create, destroy, or reorder authoritative resources.

## Core and domain ownership

* **Core remains domain-agnostic.** Windows, tabs, workspaces, sessions, contexts, clients, panes, permissions, and federation are plugin domains. Core provides neutral execution, transport, storage, and dispatch mechanisms, not their product policy.
* **Infrastructure obeys the same boundary.** Server, client, IPC, session, event, terminal, CLI runtime, plugin SDK, plugin host, and schema/code-generation layers must not acquire domain-specific types, fields, events, convenience APIs, or dispatch branches.
* **Host APIs expose mechanisms, not domain helpers.** Plugins use generic storage, logging, recording, service invocation, and permitted kernel execution. Domain convenience helpers belong in plugins, not shared host infrastructure.
* **`HostRuntimeApi` has a closed generic surface.** Its operations are `core_cli_command_run_path`, `plugin_command_run`, `storage_get`, `storage_set`, `log_write`, and `recording_write_event`. Domain convenience operations must not be added.
* **Kernel access follows the foundational-plugin boundary.** Sessions, contexts, clients, and windows plugins may call kernel primitives through `ServiceCaller::execute_kernel_request`. Other plugins use the foundational plugins' typed BPDL services.
* **Core imports neutral primitives directly.** Domain plugin API dependencies, umbrella re-exports, and compatibility layers must not expose plugin domains through core. Moving a domain type into a shared or `*-state` crate does not make it neutral.

## Plugin contracts and composition

* **Plugin API crates contain contracts, not implementations.** BPDL schemas, generated interfaces, stable wire/model types, schema tests, and intentional neutral re-exports belong in API crates. Concrete runtime state, lifecycle behavior, tasks, I/O, authorization decisions, and orchestration belong in implementation crates.
* **Shared core support is neutrally owned.** Handles, reader/writer traits, no-op fallbacks, and support types needed by core belong in neutral primitive crates rather than plugin API crates.
* **BPDL owns modeled service transport.** Use generated clients and service interfaces rather than public handwritten transport clients or duplicate handwritten request/response envelopes. Private helpers may simplify one workflow, but must not recreate a broad domain IPC compatibility layer.
* **Plugins compose through declared contracts.** A plugin consumes another plugin's typed services and events rather than its private runtime implementation. Public wire contracts do not depend on concrete Rust runtime layout.
* **Versioned interfaces retain their meaning.** Existing interface semantics are not silently changed. Newer interfaces and capabilities are selected through explicit compatibility negotiation.

## Resource identity and personal views

* **Durable identity is independent of presentation and placement.** Names, labels, indexes, connection IDs, and rendered positions are not resource identity. Durable references identify their authority; federated references use logical resource IDs, not worker-local execution IDs.
* **Personal arrangement is separate from resource lifetime.** Removing, hiding, or reordering a reference does not terminate shared work. Zero remaining references does not authorize termination; destruction is an explicit operation enforced by the resource authority.
* **Independent views remain independent.** Changing one personal arrangement does not rearrange another. Attachments deliberately sharing an arrangement share its durable changes, not active selection, focus, or navigation history.
* **Presentations consume one authoritative arrangement.** Tab strips, sidebars, pickers, and navigation do not establish competing ordering or ownership models. Temporary follow presentation does not silently rewrite a personal arrangement.
* **Removed work remains discoverable.** Removing a personal reference does not remove an otherwise authorized running resource from discovery.
* **Selection changes preserve input safety.** Removing or invalidating the selected reference must resolve or suspend the input target; input must not continue into an unintentionally hidden resource.

## State authority and persistence

* **Each state domain has one defined authority.** Caches, rendered projections, connection-local files, and transient process state do not become alternate sources of truth. Durable arrangements remain distinct from attachment-local interaction state.
* **Acknowledged durable writes are durable.** Success is returned only after the storage or consensus commit required by the operation completes. Partial writes must not be accepted as completed state.
* **Mutation preconditions are enforced.** Operations carrying expected revisions, generations, or idempotency identities validate them against authoritative state rather than silently applying to a different resource version.
* **Unknown or corrupt state is not guessed.** Unsupported schema versions and corrupt authoritative data produce explicit errors or recovery states, not silent reinterpretation or an apparently healthy empty installation.
* **Migrations preserve canonical state.** Representation changes preserve data and authority, define interruption recovery, and do not substitute incomplete projections or discard existing data without explicit authorization.

## Attachment and presentation architecture

* **Product-specific attach adapters are plugin-owned.** Core consumes neutral snapshots, deltas, controls, resume state, and status. It does not parse cluster targets or interpret membership, placement, execution generations, or recovery policy.
* **Rendering consumes retained plugin state.** Plugins publish complete owner-scoped presentation snapshots; the terminal frame loop does not call plugins to reconstruct presentation.
* **Plugin composition respects ownership.** Layout requests target the host root, compose deterministically, and cannot target another owner's allocation. Product-specific subdivision stays inside the producer's surface; surface and interaction identities remain owner-scoped.
* **Composition has one authority.** Runtime and plugin surfaces converge on the retained compositor for hit testing, damage, occlusion, focus, and pointer routing. Alternate presentation transports must preserve the same ownership, identity, revision, and lifecycle semantics rather than introduce a competing scene contract.
* **Client presentation does not own server policy.** Rendering and projection do not determine authorization, resource lifetime, or federation control decisions.

## Reusable TUI and interaction

* **TUI layers remain product-neutral and directional.** `bmux_tui` does not depend on components or runtime; `bmux_tui_runtime` depends on primitives, not components. Product behavior remains in applications and plugins.
* **Application and control state is caller-owned.** Framework caches and retained geometry are reconstructible derived data, not application authority. Stable caller-owned identities, not vector positions, identify persistent children and interaction targets.
* **Painting and interaction use the same layout.** Cells, cursors, hit regions, focus, selection, images, and damage use the authoritative geometry, transforms, and clipping. Offscreen content contributes no visible interaction targets.
* **Input is routed to one intended target.** Keyboard and paste events are not broadcast to competing controls. Modal scopes exclude background input and restore only still-valid focus targets.
* **Interaction metadata follows committed output.** Failed presentation does not advance hit, focus, selection, or other scene authority beyond the last successfully displayed frame.
* **Text and selection preserve source boundaries.** Editing respects grapheme boundaries, source ranges respect UTF-8 boundaries, and terminal geometry uses display-cell widths. Selection follows logical source identity rather than reconstructed terminal cells; source changes invalidate stale endpoints. Clipboard and export effects remain application-owned.

## Terminal correctness and runtime safety

* **Capability claims match terminal behavior.** TERM profiles and advertised capabilities must not promise protocol behavior BMUX cannot preserve. Query replies have an explicit owner and return to the correct requester without becoming user input or reaching unrelated executions.
* **Terminal state is execution-scoped.** Parser state, modes, output, and protocol replies must not leak between independent executions. Losing an attachment does not itself authorize terminating its server-owned process.
* **Interactive paths are bounded.** Rendering, input, output retention, queues, and peer fan-out must not require unbounded work or memory. Slow consumers cause explicit backpressure, repair, or disconnection rather than silent stream truncation presented as complete output.
* **Terminal ownership includes restoration.** Normal exit, cancellation, and recoverable errors restore the outer terminal modes owned by BMUX. Failed cleanup is not reported as successful restoration.

## Federation authority and recovery

* **A federated workspace remains one logical workspace.** Ingress, leader, and worker changes do not change logical resource identity. Federation policy remains in cluster plugins; it does not migrate into core.
* **Control metadata uses quorum authority.** Minority partitions cannot commit control mutations. Terminal I/O and full terminal state are not consensus-log payloads. Authorization, mutation preconditions, execution authority, and leases use consistent authoritative reads; stale diagnostic reads identify their revision and staleness.
* **Execution mutations require current fencing authority.** Workers validate the execution identity, generation, and authority before mutation. Replacement advances generation; stale workers and stale updates cannot reactivate superseded executions.
* **Unreachability does not prove process death.** Gateway or leader loss does not imply worker process loss. Worker unreachability alone does not clear or replace execution authority; replacement requires explicit action or durable restart policy.
* **Retries preserve one logical outcome.** Within the defined deduplication interval, duplicate mutation identities return the same logical outcome and conflicting reuse is rejected. Incomplete workflows survive snapshots until terminal resolution.
* **Consensus application is deterministic and side-effect-free.** Applying committed commands does not launch processes, contact peers, perform external authorization, or consult nondeterministic local state. Reconcilers execute idempotent effects from durable intent and resume that work across leadership changes.
* **Consensus persistence preserves recovery authority.** State changes, deduplication outcomes, applied position, and membership maintain their required atomicity. Snapshots preserve recovery state. Storage failure stops the affected node from voting or serving authoritative mutations on uncertain durability.
* **Promotion is explicit.** Existing local resources are not silently adopted into federation or reinterpreted as federated resources.

## Federated attachment and personal state

* **Resume establishes control authority before input.** Reconnect authenticates again, reconciles committed control state, and validates current execution authority before enabling input. A resume descriptor is not a private credential.
* **Output repair respects execution identity.** Cursors belong to one execution and generation. Gaps require a complete snapshot with a coherent continuation watermark; old-generation output cannot be applied to a replacement execution.
* **Durable personal arrangements use cluster authority.** Federated arrangements do not gain an ingress-local alternate authority, offline mutation journal, or automatic merge path. Transient focus, zoom, and active selection remain attach-view state, not consensus writes.
* **Transport changes preserve attachment semantics.** Polling and streaming preserve the same generation validation, cursor repair, cancellation, and backpressure guarantees; protocol changes require explicit negotiation.

## Security and trust

* **Connectivity does not confer authority.** Endpoint access and transport credentials do not alone grant cluster membership or resource permissions. Node identity, principal identity, and transient connection identity are distinct.
* **Authorization is enforced at the authoritative side effect.** Ingress checks do not replace service- or worker-side authorization. Permissive local fallback does not bypass federated trust requirements.
* **Membership changes follow authenticated committed transitions.** Enrollment, role changes, revocation, and credential rotation cannot be bypassed by generic metadata updates. Reconnect and replay do not revive revoked authority.
* **Transport choices preserve trust semantics.** SSH, TLS, Iroh, and other transports must satisfy the same required peer identity and authorization checks.
* **Private credentials remain protected.** Replicated public metadata, ordinary diagnostics, logs, and resume descriptors do not expose reusable private credentials.

## Installation and compatibility

* **Slots and runtime targets remain isolated.** Slot-selected clients and servers do not accidentally cross slot endpoints or owned state. Named-runtime startup, connection, and service management honor the resolved runtime; sandboxed execution does not silently fall back to ordinary user runtime or state locations.
* **Configuration resolution is deterministic.** Entry points honor documented authority and precedence. Inspection does not silently rewrite declaratively managed configuration.
* **Compatibility is explicit at evolving boundaries.** Service, attach, federation, and durable storage contracts define supported representations and reject unsupported ones rather than guessing. Upgrades do not bypass authentication, quorum, fencing, or durable-format validation.

## Invariant evolution

* **Conflicts require an explicit architectural decision.** Conflicts between a request, these invariants, `AGENTS.md`, or accepted ADRs must be surfaced rather than resolved through silent implementation or weaker wording.
* **Exceptions are explicit.** Existing violations and migration states are not implicit exceptions. Intended exceptions require a defined scope and rationale.
* **Invariant changes update the architecture coherently.** Changing an invariant requires corresponding updates to affected architecture documentation, contracts, tests, and mechanical guards.
* **Mechanically checkable boundaries should be enforced.** Important invariants gain dependency checks, compile-time boundaries, architecture guards, or focused tests when practical. Validation commands remain in `AGENTS.md` and scripts.
