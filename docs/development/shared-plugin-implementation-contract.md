# Shared plugin architecture implementation contract (draft)

Status: approved implementation direction; integration and verification in
progress. The initial source-only stage has ended. Production migration is
incomplete and blocked; see `shared-plugin-integration-checkpoint.md` for
measured results and outstanding acceptance gaps. No invariant is relaxed.

## Scope and workflow

The BMUX worktree is `/Volumes/ehdd/GitHub/wt-bmux/shared-plugin-architecture`;
the Bcode worktree is
`/Volumes/ehdd/GitHub/wt-bcode/shared-plugin-architecture`. These are the only
implementation targets. The coordinator owns this contract, all root manifests,
lockfiles, cross-worktree patches, and shared module-root wiring unless
explicitly delegated below.

The user explicitly defers compilation and all automated verification during the
initial parallel source stage. Do not run cargo, rustc, rustfmt, clippy,
nextest, rebuild/install/smoke scripts, tests, benchmarks, or automated
source/architecture checks in that stage. Reading/discovery and focused manual
diff review are permitted. Write real tests but do not execute them. Required
repository validation is deferred, not waived, and must run in the integration
phase. No commits, staging, pushes, user runtime replacement, invariant edits,
permission bypasses, or child-worker spawning.

## Product outcome

BMUX owns the generic plugin authoring, packaging, ABI, loading, lifecycle,
invocation, registration and frontend mechanisms. Bcode production paths use
them; Bcode keeps its product contracts/policy. A plugin has one implementation
and declaration. Static/dynamic is a packaging decision, never a behavioral
fork. Preserve efficient Rust-native static dispatch. Trusted native code,
coordinated rebuilds, startup loading first. Bundled is the default; explicit
startup selection may override it with a dynamic candidate; ambiguity and
selected-candidate failure are errors, never silent fallback. Compatible
frontend/backend components are installed explicitly. Preserve existing process
protocol support, without interpreting executable pointers across process
boundaries or promising unimplemented parity.

## Shared contract boundaries

M1 owns `bmux_plugin_sdk::definition` and `bmux_plugin_sdk::abi` (new concrete
modules only if used), authoring macros, and SDK contexts. M2 owns
`bmux_plugin_runtime::{instance,invocation}` and existing runtime primitives. M3
owns execution/loader integration. M4 owns generated BPDL service adapters and
typed/streaming dispatch. These are responsibility boundaries, not permission to
create competing types.

### Identity and ownership

Use separately typed identities for runtime scope, loaded instance, activation,
invocation and resource. Plugin ID is logical identity, not cache/lifetime
identity. Artifact identity and component role are separately observed metadata.
Host creates instance/activation/invocation identities; plugin payloads cannot
invent authority. Prefer UUID-backed newtypes using existing UUID dependencies;
no reusable credentials in descriptors or diagnostics.

M2 exports `PluginInstanceId`, `PluginActivationId`, `PluginResourceId`,
`InvocationScope`, `ActivationScope`, and lifecycle admission guards from the
runtime crate. Reuse existing `PluginInvocationId` where possible without
creating dependency cycles: runtime has neutral internal invocation identity,
SDK adapters translate at boundaries. M1 must not make runtime depend on SDK.
Exact identity conversions must be explicit and lossless.

### Instance lifecycle (M2/M3)

States: created -> activating -> active -> closing -> inactive -> destroyed;
failures retain explicit cleanup-required state. Readiness is separate from
activation completion. Registrations are owned by the actual activation and only
published when valid; dependencies may be prebound but inactive providers cannot
serve normal work. On partial activation failure, close admissions, cancel
producers, drain admitted work, clean up all affected owners in reverse
dependency order. A timeout or cancelled observer never authorizes
destruction/unloading.

An activation owns task/subscription/registration resources. An invocation owns
callback capabilities, child work and its event/final-response stream. Frontend
instances own local controllers/surfaces and do not own detached backend work.
Multiple hosts/frontends in one process must not reuse process-global singleton
implementation state by accident.

Callback admission and closure must share an atomic protocol. No gap between
lifecycle check and incrementing admitted work; no lost drain wakeups; handles
are never reused while stale callers can exist. Reentrant nested calls must not
hold an exclusive parent callback mutex while waiting for work that needs that
mutex. Do not transport raw pointer-backed invocation identity.

### Author implementation and packaging (M1/X1)

One generic `PluginDefinition` declares logical metadata, component roles,
required generic capabilities, contract descriptors and registration/factory
functions. A `PluginFactory` creates fresh scoped implementation instances. The
shared `PluginInstance` interface supports lifecycle, unary/streaming
invocation, event delivery and extension operations. Implement the concrete
factory/instance interfaces in M1 first and report the precise signatures early;
use typed invocation services for static calls and encode only in dynamic
adapters. Existing plugin traits may be adapted during migration but must not
become a permanent second runtime.

Expose one author macro `define_plugin!` producing a module-local
`plugin_definition()` factory. It never emits global no-mangle symbols in the
implementation crate. A separate `export_plugin_entrypoint!` in a thin dynamic
packaging target exports the common versioned descriptor/operations ABI.
Existing export macros may temporarily adapt, but final migrated implementation
crates must be export-free. X1 owns concrete thin packaging targets or
generation tooling: do not create speculative crates. Plugin workers use the
shared declaration and expose their implementation; do not each invent
wrapper/build machinery.

ABI uses repr(C) fixed-layout descriptor/function table with size/version
checks, bounded caller-owned buffers, explicit status codes, opaque handles and
explicit create/destroy operations. Document allocation/deallocation owner,
borrowed buffer validity, thread/reentrancy rules, callback closure and panic
behavior. No Rust strings/trait objects/futures/Arc/TypeId/Tokio handles cross
it. Metadata discovery does not activate a plugin. Abort/native faults cannot be
contained by catch_unwind; unwinding panics must not cross extern C. Keep v1
semantics frozen; new capabilities require explicit selection, not fallback
reinterpretation.

### Invocation and BPDL (M3/M4)

One operation contract and provider implementation supplies both typed static
dispatch and generated portable dispatch. No second handwritten broad
request/response transport layer. SDK `TypedDispatchClient` gains an actual
streaming delivery operation with a typed event sink, final response and
cancellation; M4 owns exact API. Existing generated `command ... emits ...` is
real schema support, not a new grammar requirement. Deliver zero or more ordered
events and one final outcome; never silently replace an unsupported event sink
with noop. Checked streaming is supported end-to-end or explicitly rejected at
capability negotiation before effects.

Authority, caller provenance, deadline and cancellation inherit through nested
calls. Serializable cancellation metadata is not live cancellation. Preserve
transport replay identities across buffer growth; nested/sibling calls get
distinct IDs. Buffer growth must never repeat side effects. Expose response
retrieval/acknowledgement or bounded retained outcomes with explicit expiration;
do not blindly reexecute mutations. Domain durable idempotency remains
domain-owned. Event delivery and all queues/retention are bounded with explicit
backpressure/repair/failure. Structured command outcomes/errors cross ABI
explicitly, never via supposedly shared thread locals.

### Host mechanisms and registrations (M6)

Retain the closed HostRuntimeApi. Generic service invocation, owner-scoped
extension contributions, storage, logging and recording remain mechanisms.
Explicit host-owned ports/handles replace dependencies on shared globals. Static
ports use safe Rust handles; dynamic ports use ABI-safe callback/opaque handles,
translated in one adapter. Never serialize executable callbacks into
process/remote protocols. Bound caller identity at the trusted invocation, not
arbitrary payload metadata.

Generic registration entries carry owner instance+activation,
contract/interface/version and bounded typed bytes or static adapter. Duplicate
conflicts fail explicitly. Retained-state subscription must coherently establish
snapshot and subsequent updates; lag causes explicit repair/disconnection.
Readiness is activation-scoped so stale marks cannot ready replacement
instances.

Persistence participant interfaces use snapshot/validate/restore/quiesce
contracts routed through typed BPDL, with static optimization only. Preserve
schema versions, dependency ordering, interruption recovery and authoritative
state. Unknown/corrupt state is not reset silently. Foundational runtime handles
use neutral infrastructure adapter boundaries; do not turn core into domain
dispatch branches or move concrete runtime state into API crates.

### Presentation (M7/B3)

Use the existing retained compositor and geometry/identity/commit authority, not
a new parallel scene system. Adapt author-native rendering locally into the same
retained contract. The wire representation must preserve cells, semantic
regions, logical-source selection, images, hit regions, focus/modal scopes,
cursor, clipping/transforms, owner/revision and lifecycle.
`ComponentViewportPaint` is not just a pixel buffer. Transport/frontend
resources are instance-scoped, bounded and explicitly destroyed; stale
input/selection is rejected. Failed display commits do not advance interaction
authority. Async projection updates are prepared off the terminal frame loop; no
new plugin reconstruction calls from BMUX frame rendering.

Bcode owns its product surface/controller/composer/visual-adapter contracts.
Bridge them through shared extension/resource operations without transferring
raw Rust PaintCx/clap/trait-object layout. Frontend and backend components
explicitly negotiate compatible application contracts; a daemon does not
auto-deliver executable frontend code. Local-only frontend actions remain local
and cannot be routed as model tool calls.

### CLI (B4/M8)

Portable typed command declarations represent existing nested grammar,
arguments, help/completion and daemon requirement classification. Host actions
are Bcode extension results, not new BMUX domain variants. Do not pass
clap::Command or ArgMatches across ABI. Preserve stdout/input safety for CLI vs
attach commands. No output or command errors depend on shared SDK TLS.

## Ownership map

M1: BMUX SDK `lib.rs`, `native_exports.rs`, `native_exports/**`, `context.rs`,
`error.rs`, new definition/abi modules, and SDK README. Excludes M4
dispatch/contracts and M6 host/event/state/readiness modules.
M2: BMUX `packages/plugin-runtime/**`, including its module root/manifests
(non-root manifests allowed only this lane).
M3: BMUX plugin `loader.rs`, loader-specific new modules,
`static_vtable_registry.rs`, `stream_admission.rs`; excludes other plugin
modules and plugin lib root.
M4: BMUX `packages/plugin-schema/**`, `packages/plugin-schema-macros/**`, SDK
`typed_dispatch.rs`, `typed_dispatch_client.rs`, `service.rs`,
`service_contract.rs`, `capability.rs`; no SDK root.
M5: BMUX plugin `discovery.rs`, `manifest.rs`, `declaration.rs`, `registry.rs`.
M6: BMUX plugin `host_runtime.rs`, `typed_service_caller.rs`,
`typed_dispatch_bridge.rs`, `event_bus.rs`, `plugin_state.rs`,
`service_location.rs`, `startup_gate.rs`, `action_dispatch.rs`, `prompt.rs`; SDK
`host.rs`, `host_services.rs`, `event.rs`, `ready.rs`, `stateful_plugin.rs`,
`wire_event_sink.rs`, `contribution.rs`, `cancellation.rs`.
M7: BMUX plugin presentation modules: `render.rs`, `layout.rs`, `surface.rs`,
`component_input.rs`, `component_render.rs`, `component_viewport.rs`+submodules,
`attach_companion.rs`, `attach_presentation_input.rs`; neutral TUI/frame
transport modules if concretely required, never product-specific logic.
M8: BMUX `packages/cli/src/**`, `packages/server/**`, `packages/client/**`,
`packages/config/**`; excludes packages/cli tests assigned X2, root manifests
and plugin code. Do not edit runtime.rs files in SDK/host lanes.
M9: BMUX
sessions,contexts,clients,workspaces,tabs,pane-runtime,permissions,snapshot
plugin implementations and matching APIs. Neutral state/snapshot crates may be
modified only after coordinator ownership grant.
M10: BMUX
tab-bar,sidebar,decoration,theme,presenter,finder,command-palette,prompted-actions
plugins and APIs.
M11: BMUX
cluster,connections,control-catalog,performance,recording,clipboard,broadcast,application-resurrection,playbook,plugin-cli
plugins and APIs. Excludes plugin-cli rebuild/install machinery owned X1.
B1: Bcode `packages/plugin-sdk/**`, `packages/plugin/models/**`; shared monolith
exclusive to B1.
B2: Bcode `packages/plugin/src/**`, `packages/server/**`; excludes models, root
manifests and plugin-owned source.
B3: Bcode `packages/tui/**`; excludes dependency manifests reserved coordinator.
B4: Bcode `packages/cli/**`, `packages/config/**`, `packages/command/**`;
excludes root manifests.
B5: Bcode
auth-broker,auth-broker-provider,bedrock-provider,fake-provider,jev-provider,openai-compatible-provider,web-search
plugins.
B6: Bcode filesystem,file-references,shell,read,git,vim-edit,worktree,ocr,vox
plugins.
B7: Bcode
loop,workflow,question,code-review,document,eval,metrics,model,usage,skills,session-derivation
plugins.
B8: Bcode
blims,compressed-session-search,default-agents,github-review-publisher,opencode-session-import,pi-session-import,prompt-profile,recap,session-milestones,tantivy-session-search
plugins.
X1: Both products' bundled registry wiring (BMUX bundled declaration block is
M8: report proposed generated registration change, do not edit same file),
concrete packaging targets, examples, install/rebuild/scaffolding tooling; BMUX
plugin-cli `src/rebuild.rs`; Bcode `packages/bundled-plugins/**`. Root manifest
edits are coordinator-only.
X2: New cross-mode regression fixtures and acceptance harnesses,
`.github/workflows/**`, existing standalone validation scripts except
install/rebuild tooling, BMUX `packages/cli/tests/**`, both plugin
integration-test directories. No production source owned by others; inline unit
tests are production file owner's responsibility.

All non-root Cargo manifests inside a lane's owned tree may be edited, but new
workspace members/dependency table entries are requested centrally. Shared
plugin lib root is coordinator-owned. No speculative packages. Workers report
progress and requests only in their own session commentary/final, not peer
messages or status files. Read any deeper AGENTS.md before editing. Preserve
others' changes. Report source-ready, pending interfaces, unexecuted tests,
remaining gaps and settled commands honestly.

## Acceptance and deletion gates

Static, dynamic and mixed actual artifacts; correct embedded identity; isolated
feature builds; no callback UAF/lost wakeups; multi-host/frontend isolation; no
duplicate mutation on response growth; live cancellation; streaming
events+final; startup rollback; retained failed cleanup; CLI
grammar/help/actions parity; selection/focus/image/commit correctness; durable
snapshot authority; permissions and provenance on fast paths; plugins
disabled/missing baseline; platform artifact/install tests; measured
static/dynamic performance. Delete Bcode's duplicate runtime/ABI and migration
switch only when production consumers are migrated. User-approved no-validation
stage is not completion; full repository commands run after source convergence.
