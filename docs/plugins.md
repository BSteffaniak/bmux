# BMUX Plugin Architecture

This document describes the plugin model that BMUX uses to keep the
core runtime domain-agnostic while letting plugins implement rich,
typed behavior. It also captures the agreed architecture decisions so
future work stays consistent.

See [`bpdl-spec.md`](./bpdl-spec.md) for the BPDL grammar and
semantics, and the [Plugin SDK README](../packages/plugin-sdk/README.md)
for the author-facing Rust API.

## Design principles

1. **Core is domain-agnostic.** `packages/server`, `packages/client`,
   `packages/ipc`, `packages/session`, `packages/terminal`, and
   `packages/event` must contain no domain-specific logic — no
   windows, sessions, contexts, clients, panes, or permissions
   concepts should appear in type names, fields, operations, or
   event names. Plugins own all product concepts. Runtime core
   scope also includes `packages/cli/src/runtime/**` (Option B
   boundary). Plugin infrastructure (`packages/plugin-sdk`,
   `packages/plugin`, `packages/plugin-schema`,
   `packages/plugin-schema-macros`) must also stay domain-agnostic —
   they provide only generic host primitives (storage, log,
   recording, capability scopes, service dispatch).
2. **Plugins are composable and typed.** Plugins declare their public
   API in BPDL (see [`bpdl-spec.md`](./bpdl-spec.md)). Other plugins
   consume those APIs as typed Rust traits generated at compile time.
3. **Easy to write simple plugins, powerful enough for complex ones.**
   A minimal plugin is ~30 lines of Rust. Rich plugins like the
   windows plugin compose naturally with other plugins through the
   typed service and event systems. Any domain feature should be built
   in a plugin unless there is a strong reason for core runtime
   plumbing.
4. **Rust-first, language-agnostic.** Rust gets the most ergonomic SDK
   because in-tree plugins are native Rust. The BPDL schema and the
   underlying serialized envelope (`ServiceRequest` /
   `ServiceResponse`) are language-agnostic; non-Rust plugins can
   implement the same wire format and gain full inter-plugin
   capabilities.

## Crate topology

### Core (domain-agnostic)

| Crate                           | Role                                        |
| ------------------------------- | ------------------------------------------- |
| `packages/ipc`                  | Scene, surface, and protocol types.         |
| `packages/server`               | Session runtime, scene construction.        |
| `packages/client`               | IPC client library.                         |
| `packages/attach_pipeline`      | Renderer, reconciler, mouse hit-test.       |
| `packages/plugin`               | Plugin host: loader, registry, dispatch.    |
| `packages/plugin-sdk`           | Plugin author API (traits, types, helpers). |
| `packages/plugin-schema`        | BPDL parser, validator, Rust codegen.       |
| `packages/plugin-schema-macros` | `schema!` proc macro.                       |

### Plugins

Plugins live under `plugins/`. Each "real" plugin ships **two crates**:

1. **`<name>-plugin-api`** — the stable typed contract. Generated from a
   `.bpdl` file via `bmux_plugin_schema_macros::schema!`. Contains no
   runtime logic. Consumers of the plugin depend on this crate alone.
2. **`<name>-plugin`** — the implementation. Depends on its `-api`
   crate and whatever other APIs it consumes. Registered with the host
   at runtime.

Examples currently in tree:

- `plugins/sessions-plugin-api` + `plugins/sessions-plugin` (owns
  session lifecycle: list, create, kill, select).
- `plugins/contexts-plugin-api` + `plugins/contexts-plugin` (owns
  context state: list, create, select, close, current).
- `plugins/clients-plugin-api` + `plugins/clients-plugin` (owns
  per-client identity, selected session, follow state).
- `plugins/windows-plugin-api` + `plugins/windows-plugin` (owns pane /
  window / tab lifecycle; exposes state queries, commands, events).
- `plugins/decoration-plugin-api` + `plugins/decoration-plugin` (owns
  pane visual styling; depends on `windows-plugin-api`).
- `plugins/pane-runtime-plugin-api` + `plugins/pane-runtime-plugin`
  (owns pane/session runtime orchestration, attach lifecycle, and
  attach-view queries — see "Pane-runtime ownership" below).
- `plugins/permissions-plugin` (owns role/permission policy).
- `plugins/cluster-plugin` (owns multi-session input broadcasting).

#### Pane-runtime ownership

The pane-runtime-plugin is the sole owner of session/pane lifecycle,
pane mutation, and attach orchestration. It exposes four BPDL interfaces:

- **`pane-runtime-commands`** — `split-pane`, `launch-pane`,
  `focus-pane`, `resize-pane`, `close-pane`, `restart-pane`,
  `zoom-pane`, `pane-direct-input`, `new-session-with-runtime`,
  `kill-session-runtime`, `restore-session-runtime`. Each handler
  performs its own permission check through the sessions-plugin's
  `session-policy-query/v1::check` typed service, runs the
  `SessionRuntimeManagerHandle` operation, and publishes the
  corresponding `Event::*` through the registered `WireEventSinkHandle`.
- **`pane-runtime-state`** — `list-panes` (session_id is optional;
  unset resolves via `FollowState::selected_session(caller_client_id)`)
  and `get-pane`.
- **`attach-runtime-commands`** — `attach-session`, `attach-context`,
  `attach-open`, `attach-input`, `attach-output`, `attach-set-viewport`,
  `detach`, `set-client-attach-policy`. The plugin owns attach-token
  issuance, runtime begin/end attach, `FollowState::attached_stream_session`,
  and per-client `attach_detach_allowed` policy.
- **`attach-runtime-state`** — `attach-layout-state`,
  `attach-snapshot-state`, `attach-pane-snapshot-state`,
  `attach-pane-output-batch`, `attach-pane-images` (serialized as
  JSON-encoded `Vec<AttachPaneImageDelta>`).

The 24+ former IPC variants (`Request::NewSession`, `KillSession`,
`ListSessions`, `ListPanes`, `SplitPane`, `LaunchPane`, `FocusPane`,
`ResizePane`, `ClosePane`, `RestartPane`, `ZoomPane`, `PaneDirectInput`,
`Attach`, `AttachContext`, `AttachOpen`, `AttachInput`, `AttachOutput`,
`AttachSetViewport`, `AttachLayout`, `AttachSnapshot`,
`AttachPaneSnapshot`, `AttachPaneOutputBatch`, `AttachPaneImages`,
`Detach`, `SetClientAttachPolicy`) have been deleted from
`bmux_ipc`. Reintroducing them is blocked by the
`session_lifecycle_ipc_variants_are_absent`,
`pane_mutation_ipc_variants_are_absent`, and
`attach_ipc_variants_are_absent` guardrails.

The attach-input fast path is preserved through
`BmuxClient::send_one_way_attach_input` (and its streaming
counterpart), which sends a fire-and-forget
`Request::InvokeService { operation: "attach-input", ... }` envelope
without waiting for the round-trip ack.

### Host runtime surface for plugins

Two traits live in `packages/plugin` and are implemented for the
three plugin context types (`NativeCommandContext`,
`NativeLifecycleContext`, `NativeServiceContext`) plus the long-lived
`TypedServiceCaller`:

- **`ServiceCaller`** — the generic dispatch primitive. Provides
  `call_service_raw`, `call_service`, and `execute_kernel_request`.
  Domain-agnostic: takes interface ids and operation names as opaque
  strings.
- **`HostRuntimeApi`** — generic convenience methods only. Covers
  `core_cli_command_run_path`, `plugin_command_run`, `storage_get`,
  `storage_set`, `log_write`, `recording_write_event`. No domain
  methods (`pane_*`, `session_*`, `context_*`, `current_client`)
  exist on this trait.

Plugins that want domain-level helpers own them locally. Foundational
plugins (sessions, contexts, clients, windows) reach core IPC through
`ServiceCaller::execute_kernel_request(bmux_ipc::Request::*)`.
Non-foundational plugins speak to foundational plugins through typed
BPDL services (`ServiceCaller::call_service`). Some plugins keep a
private `domain_ipc` module that wraps common patterns; these modules
are plugin-local, never a core dependency.

## Host state registry

Foundational state types are owned by their respective plugin crates.
Each plugin's `activate` callback constructs its default state and
registers it with the process-wide \[`bmux_plugin::PluginStateRegistry`\]:

```rust,ignore
use bmux_plugin::global_plugin_state_registry;
use bmux_clients_plugin::FollowState;
use std::sync::{Arc, RwLock};

impl RustPlugin for ClientsPlugin {
    fn activate(&mut self, _ctx: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        let state = Arc::new(RwLock::new(FollowState::default()));
        global_plugin_state_registry().register::<FollowState>(&state);
        Ok(EXIT_OK)
    }
}
```

The registry is a `TypeId`-keyed typemap holding
`Arc<dyn Any + Send + Sync>` entries. Consumers resolve by concrete
type: `global_plugin_state_registry().get::<FollowState>()`.

**Server state ownership model.** Each plugin's `activate` constructs
an `Arc<RwLock<T>>` holding its concrete state (`FollowState`,
`ContextState`, `SessionManager`, etc.) and registers a trait-object
handle into the process-wide \[`bmux_plugin::PluginStateRegistry`\].
Server code never names the concrete plugin-owned types — it reads +
writes through the registered `*Handle` trait objects
(`FollowStateHandle`, `ContextStateHandle`, `SessionManagerHandle`,
`PerformanceSettingsHandle`, `RecordingSinkHandle`,
`SnapshotOrchestratorHandle`, etc.) defined in neutral primitive
crates (`packages/client-state`, `packages/context-state`,
`packages/session-state`, `packages/recording-runtime`,
`packages/performance-state`, `packages/snapshot-runtime`).

This keeps a strict one-way dependency direction: plugin → core is
allowed, core → plugin is not.

Plugin-owned state type locations:

| Type                                        | Owner plugin             | Location                                                                                      |
| ------------------------------------------- | ------------------------ | --------------------------------------------------------------------------------------------- |
| `FollowState`                               | `clients-plugin`         | `plugins/clients-plugin/src/follow_state.rs`                                                  |
| `ContextState`                              | `contexts-plugin`        | `plugins/contexts-plugin/src/context_state.rs`                                                |
| `SessionManager`                            | `sessions-plugin`        | `plugins/sessions-plugin/src/session_manager.rs`                                              |
| `RecordingRuntime` + sink trait             | `recording-plugin`       | `plugins/recording-plugin/src/recording_runtime.rs` + `packages/recording-runtime/src/lib.rs` |
| `PerformanceCaptureSettings` + rate limiter | `performance-plugin`     | `packages/performance-state/src/lib.rs` + `plugins/performance-plugin-api/src/lib.rs`         |
| Catalog revision counter + snapshot         | `control-catalog-plugin` | `plugins/control-catalog-plugin/src/lib.rs`                                                   |

Concrete state types live in each plugin's impl crate; the matching
`*-plugin-api` / `*-state` crates expose only the reader/writer traits
and handle newtypes used by core. Core crates (`packages/server`,
`packages/client`) reach domain state exclusively through those trait
objects registered in \[`bmux_plugin::PluginStateRegistry`\] — the
"core must not depend on plugin impl crates" rule is enforced
uniformly by the `core_architecture_does_not_depend_on_plugins`
guardrail, which includes `packages/server` in its core-crate list.

The control-catalog plugin is a cross-cutting aggregator: it doesn't
own a dedicated state struct registered in
`PluginStateRegistry`; instead it holds a process-wide `AtomicU64`
revision and reads sessions/contexts/bindings from the other plugins'
registered state on demand. It subscribes to `SessionEvent`,
`ContextEvent`, and `ClientEvent` on the plugin event bus and ticks
its revision whenever any of those domains change, emitting a typed
`CatalogEvent::Changed` on its own bus channel.

The server bridges that typed `CatalogEvent` into the existing
`bmux_ipc::Event::ControlCatalogChanged` wire event via a tokio task
spawned during `BmuxServer::run`, so cross-process attach UIs keep
receiving the same catalog-changed signal they did before the
migration.

Follow orchestration (client A mirrors client B's selected session)
lives entirely in `clients-plugin`. The typed `clients-commands:: set-following` handler mutates plugin-owned `FollowState` directly,
dispatches `contexts-commands::select-context` and
`sessions-commands::reconcile-client-membership` to the other
foundational plugins, and emits typed `ClientEvent::{FollowStarted, FollowStopped, FollowTargetChanged}` on the plugin event bus. The
server's `spawn_client_events_bridge` task maps those typed events to
the legacy wire `Event::{FollowStarted, FollowStopped, FollowTargetChanged}` for cross-process subscribers, following the
same pattern as the control-catalog bridge.

`SessionRuntimeManager` (the heavier pane-runtime struct that owns
PTY handles, layout tree, and floating surfaces) remains in
`packages/server` — it is too entangled with server-specific runtime
primitives (`portable-pty`, tokio channels) to relocate without
pulling those dependencies into a plugin crate. Migrating it is
tracked as future work.

## Persistence

Snapshot persistence (save/restore across server restarts) is owned
by the `bmux.snapshot` plugin. Core crates never name the snapshot
schema — they dispatch through a `SnapshotOrchestratorHandle` trait
object registered in the plugin state registry.

### Combined envelope format

The on-disk format is a `CombinedSnapshotEnvelope` (defined in
`plugins/snapshot-plugin-api/src/envelope.rs`): a monotonic version,
a FNV-1a-64 checksum over the sections, and a `Vec<SectionV1>` where
each section is one participant's opaque payload plus its schema
metadata:

```text
CombinedSnapshotEnvelope {
  version: 1,
  checksum: u64,
  sections: Vec<SectionV1 { id: String, version: u32, bytes: Vec<u8> }>,
}
```

The file lives at
`paths.data_dir/runtime/bmux-snapshot-v1.json`. The filename is
versioned (`v1`) so the envelope format never silently overwrites a
legacy monolithic `server-snapshot-v2.json`.

### `StatefulPlugin` participants

Each participant implements
\[`bmux_plugin_sdk::StatefulPlugin`\] and pushes its
`StatefulPluginHandle` into a shared
`bmux_snapshot_runtime::StatefulPluginRegistry` during `activate`:

| Participant id                  | Owner                               | Payload type             |
| ------------------------------- | ----------------------------------- | ------------------------ |
| `bmux.clients/follow-state`     | `clients-plugin`                    | `FollowStateSnapshot`    |
| `bmux.contexts/context-state`   | `contexts-plugin`                   | `ContextStateSnapshot`   |
| `bmux.sessions/session-manager` | `sessions-plugin`                   | `SessionManagerSnapshot` |
| `bmux.server/pane-runtime`      | server (`pane_runtime_snapshot.rs`) | `PaneRuntimeSnapshotV1`  |

Each participant serializes its own section independently (serde-JSON
inside the opaque `bytes` field). Sections are decoded + routed back
by `id` on restore. Unknown ids are logged + skipped; sections whose
`restore_snapshot` returns `Err` are counted as failures but never
abort the rest of the restore (see `restore_gracefully_skips_*` test).

### Orchestrator

`BmuxSnapshotOrchestrator` in `plugins/snapshot-plugin/src/orchestrator.rs`
implements the `bmux_snapshot_runtime::SnapshotOrchestrator` trait.
It owns:

- `path: Option<PathBuf>` — snapshot file location (None = disabled).
- `dirty_flag: Arc<SnapshotDirtyFlag>` — atomic bit flipped by server
  on every state change.
- `stateful_registry: Arc<RwLock<StatefulPluginRegistry>>` — the
  process-wide registry of participants.

On `activate`, the snapshot plugin spawns a dedicated OS thread that
polls the dirty flag every 200ms. When the flag has been set for at
least `debounce_ms` (1000ms by default), the thread calls
`save_now_blocking()` to atomically rewrite the envelope file.

Server marks dirty via `snapshot_dirty_flag().mark_dirty()` at every
persistence-worthy state change (7 call sites). Server does not
spawn its own flush task — the plugin's debounce thread replaces it.

### IPC wiring

Four IPC variants (`Request::{ServerStatus, ServerSave, ServerRestoreDryRun, ServerRestoreApply}`)
stay on the wire for CLI-facing operations. Their server handlers
delegate to the orchestrator through
`snapshot_orchestrator_handle().as_dyn().save_now_boxed().await`
(and similar for the other three). Server never names the snapshot
schema, never reads or writes the envelope file directly.

On `BmuxServer::run_impl`, after every plugin has activated and the
server's own `ServerPaneRuntimeStateful` participant has registered,
server awaits `restore_if_present_boxed()` to populate the four
participant slices from the on-disk envelope in a single pass.

### Offline path (`offline_kill_sessions`)

When the server is not running, CLI subcommands like `bmux kill-session`
still need to prune killed sessions from the persisted snapshot so
they don't resurrect on the next server start. The
`bmux_snapshot_plugin_api::offline_snapshot::offline_kill_sessions`
utility handles this path — it reads the combined envelope,
decodes each relevant section through the matching neutral primitive
crate (`SessionManagerSnapshot` from `bmux_session_state`,
`ContextStateSnapshot` from `bmux_context_state`,
`FollowStateSnapshot` from `bmux_client_state`, pane-runtime via
`serde_json::Value` since its schema is server-internal), mutates
the in-memory structures, rebuilds the envelope checksum, and writes
atomically.

A file-level `.lock` sidecar (acquired via `O_CREATE|O_EXCL` with 3s
timeout + 50ms retry backoff) serializes concurrent mutations.

### Boundary properties

The persistence boundary is closed:

- `packages/server` carries zero snapshot-schema types. Every
  persistence interaction is one trait-object dispatch through
  `SnapshotOrchestratorHandle`.
- `packages/server/src/persistence.rs` does not exist; the whole
  legacy `SnapshotV4` + `SnapshotManager` pipeline was deleted.
- Plugin-api crates own the wire format (envelope + sections) and
  the offline utility. Plugin impl crates own the orchestrator, the
  debounce thread, and the atomic file I/O.
- CLI drives both paths: online via the `SnapshotOrchestratorHandle`
  IPC dispatch, offline via the `offline_snapshot` module.

Enforced at test time by four architecture guardrails
(`server_does_not_define_snapshot_schema`, `snapshot_plugin_exists`,
`state_plugins_implement_stateful_plugin`,
`server_implements_pane_runtime_stateful`).

## Interaction patterns

Plugins interact through four typed patterns, all declared in BPDL:

1. **Query** (`query`): synchronous read-only lookup. E.g.
   `WindowsState::pane_state(id) -> PaneState?`.
2. **Command** (`command`): write / mutating call. May fail with a
   typed error. E.g. `WindowsCommands::focus_pane(id) -> result<unit, focus-error>`.
3. **Event** (`events`): pub/sub stream with per-interface ordering.
   Publishers emit typed events; subscribers receive them via the
   plugin host's event bus.
4. **Future**: resources (long-lived typed handles), richer streams.

Under the hood, calls travel via the existing `ServiceRequest` /
`ServiceResponse` envelope in the plugin host. Typed dispatch
(`bmux_plugin_sdk::typed_dispatch`) wraps that envelope with
type-erased handles that generated client/server stubs downcast to the
interface trait.

### Event bus

In-process event delivery goes through `bmux_plugin::EventBus`, a
`PluginEventKind`-keyed typemap of
`tokio::sync::broadcast::Sender<Arc<dyn Any + Send + Sync>>`. Each
plugin that emits events calls `register_channel::<E>()` in its
`activate` callback; publishers then call `emit::<E>(event)` and
subscribers call `subscribe::<E>()` to receive an untagged
`Receiver<Arc<E>>`. The global singleton is reachable via
`bmux_plugin::global_event_bus()`. Zero-serialization fanout is used
for in-process subscribers; cross-process subscribers bridge through
the existing `bmux_ipc::Event` stream.

## Context model (canonical)

`Context` is the generic, attachable execution resource in core.

### What a context represents

A context is not a session and not a window by definition. It is a
composable workspace primitive that can back many plugin concepts
(windows, tabs, views, workspaces, etc.).

Each context owns at least:

- pane tree/layout
- focused pane
- attach routing target
- per-context runtime/view state

### Identity and sharing

- `ContextId` is globally unique (UUID).
- Contexts are shareable across plugins.
- Core does not hardcode one plugin as owner of contexts.

### Attributes

Contexts include `attributes: map<string,string>` for plugin
coordination and metadata.

Attributes are for discovery/coordination hints, not direct security
policy decisions.

Recommended naming:

- `core.*` reserved for core-defined keys
- `<plugin_id>.*` for plugin-defined keys

### Session relationship

- Contexts are not always scoped to sessions.
- Core should support contexts as first-class resources without
  mandatory session ownership.
- Session behavior may itself become plugin-owned in the future.

### Activation and close semantics

- On close of the active context, select the most-recent-active
  context (MRU).
- `ContextClose` supports `force`.

### Plugin API direction

Expose generic host service interfaces for context operations:

- `context-query/v1`
- `context-command/v1`

Use typed `bmux_plugin_sdk` host runtime APIs for all plugin access to
core mechanics.

### Command outcome contract

Plugin command execution should support a generic outcome contract
(for keybinding/runtime flows), including selecting a target context
after command success.

This enables behavior like `ctrl-a c` to create and immediately switch
to a newly created context without embedding windows-domain logic in
core runtime.

## Scene and rendering

Each surface in the scene (`bmux_ipc::AttachSurface`) carries:

- `rect` — outer bounds.
- `content_rect` — the PTY interior (authoritative; renderer, PTY
  sizer, image compositor, and mouse hit-tester all read this field).
- `interactive_regions` — named sub-rectangles owned by a plugin that
  route mouse events back to the declaring plugin.

Today the server's scene producer emits a default 1-cell ASCII border
geometry matching what the core renderer paints. When the decoration
plugin's scene-layout integration ships, the decoration plugin will
publish the `content_rect` and `interactive_regions` for each surface
via the layout protocol, and the server will compose those
contributions instead of applying the default geometry.

## Mouse dispatch (architecture)

Core's mouse handling is a pure hit-test → event emitter:

1. Hit-test identifies the topmost `AttachSurface` containing the
   click.
2. If the click is inside `content_rect`, core encodes the click in
   the pane's mouse protocol and forwards the bytes to the PTY.
3. If the click is inside an `interactive_region`, core emits a
   `SurfaceRegionClicked` plugin event targeting the region's
   `owning_plugin_id`. The owning plugin's subscribers react. (This
   path is scaffolded — the event stream carries the required data —
   and will be wired into full region-click dispatch in a follow-up.)

Coordinate translation uses `content_rect` so that clicks at the
visual top-left content cell encode as pane-local `(1, 1)` regardless
of the thickness or style of the surrounding decoration.

## Mouse gestures (config)

Mouse gestures can trigger built-in runtime actions or plugin commands
through `behavior.mouse.gesture_actions`.

```toml
[behavior.mouse]
enabled = true
focus_on_click = true
click_propagation = "focus_and_forward"
focus_on_hover = false
scroll_scrollback = true
wheel_propagation = "forward_only"
scroll_lines_per_tick = 3
exit_scrollback_on_bottom = true

[behavior.mouse.gesture_actions]
click_left = "plugin:bmux.windows:new-window"
hover_focus = "focus_next_pane"
scroll_up = "scroll_up_line"
scroll_down = "scroll_down_line"
```

Supported gesture keys in current core runtime:

- `click_left`
- `hover_focus`
- `scroll_up`
- `scroll_down`

## Permissions and policy

- Enforcement is config/policy-file driven and non-interactive for now.
- No interactive permission prompts at this stage (may be added later).
- Policy actions should be explicit, no aliases.

Examples of explicit action style:

- `context.create`
- `context.select`
- `context.close`
- `context.list`

## Windows plugin mapping

Windows is a plugin UX/domain concept. It should map to generic
contexts rather than forcing core windows types.

Expected behavior:

- `new-window` creates a context
- `switch/next/prev/last-window` select contexts
- `kill-window` closes a context
- `ctrl-a c` immediately switches attach context to the newly created
  context

## Core defaults when plugins are missing

Per AGENTS.md:

- **Windows plugin missing**: baseline single-terminal attach / session
  / pane flow still works. Core falls back to painting a default
  1-cell border in the renderer
  (`packages/attach_pipeline/src/render.rs`).
- **Permissions plugin missing**: permissive single-user behavior.
- **Decoration plugin missing**: surfaces render with
  `content_rect == rect - 1 on each side` and the renderer paints the
  default ASCII border. A follow-up will let the scene producer emit
  `content_rect == rect` (no chrome) when no decoration plugin is
  present, and have the renderer no-op on border painting.

## Writing a plugin

See [`bpdl-spec.md`](./bpdl-spec.md) for full BPDL grammar and
semantics.

Minimal shape:

1. **Write your BPDL schema** (`bpdl/my-plugin.bpdl`):

   ```bpdl
   plugin my.example version 1;

   interface my-state {
       query hello(name: string) -> string;
   }
   ```

2. **Create the API crate** (`my-plugin-api/src/lib.rs`):

   ```rust
   bmux_plugin_schema_macros::schema!("bpdl/my-plugin.bpdl");
   ```

3. **Create the impl crate** (`my-plugin/src/lib.rs`):

   ```rust
   use my_plugin_api::my_state::MyState;

   pub struct MyPlugin;

   impl MyState for MyPlugin {
       fn hello(&self, name: String) -> Pin<Box<dyn Future<...>>> {
           Box::pin(async move { format!("hello {name}") })
       }
   }
   ```

4. **Register with the host** (via the existing plugin-sdk registration
   macros). Consumers import `my_plugin_api` and use the trait
   generically.

See the windows and decoration plugins for reference.

## Guardrails and validation

The `packages/cli/tests/architecture_guardrails.rs` file contains
string-matching tests that fail if forbidden markers appear in the
core crates. The current invariants include:

- `runtime_production_code_is_domain_agnostic` — CLI runtime files
  reference only generic service/plugin APIs.
- `core_packages_do_not_reference_domain_plugin_markers` — core
  crates (`server`, `client`, `session/models`, `event`, `event/models`)
  don't reference windows/permissions interface ids or legacy IPC
  request variants.
- `plugin_production_code_uses_generic_host_api_only` — bundled
  plugins reach core via `ServiceCaller::execute_kernel_request` or
  plugin-api crates, not raw IPC.
- `event_core_crate_has_no_domain_event_types` — the `packages/event`
  crates carry no `SessionEvent`/`PaneEvent`/`ClientEvent`/`InputEvent`
  enums or helper constructors.
- `event_models_crate_has_no_domain_dependencies` — `bmux_event_models`
  never depends on `bmux_session_models` or `bmux_terminal_models`.
- `client_core_crate_has_no_domain_convenience_methods` — the IPC
  client library exposes no `new_session`/`list_contexts`/`split_pane`/
  etc. convenience methods; callers route through
  `BmuxClient::invoke_service_raw` with typed plugin-api payloads.
- `cli_crate_does_not_reexport_domain_types` — `packages/cli/src/lib.rs`
  doesn't re-export `SessionId`/`SessionManager`/`TerminalInstance`/etc.
- `bmux_umbrella_has_no_domain_reexports` — the top-level `bmux` crate
  re-exports only domain-agnostic building blocks.
- `session_models_is_minimal` — `packages/session/models` carries
  only the minimum types the server still needs (`SessionId`,
  `ClientId`, `Session`, `SessionInfo`). Dead types (`LayoutError`,
  `PaneError`, `ClientError`, `ClientInfo`, `SessionError`, `PaneId`)
  are deleted and can't be reintroduced.

When adding functionality, new guardrail tests should be added to
lock in any new structural invariants the change establishes.

- Required validation for runtime/code changes follows `AGENTS.md`.

## Routing policy (config)

Command ownership requirements are host-policy driven, not hardcoded
by plugin ID.

```toml
[plugins.routing]
conflict_mode = "fail_startup"

[[plugins.routing.required_namespaces]]
namespace = "plugin"

[[plugins.routing.required_paths]]
path = ["terminal", "doctor"]
```

Claims may optionally pin ownership to a specific plugin:

```toml
[[plugins.routing.required_namespaces]]
namespace = "playbook"
owner = "example.playbook"
```

Resolution behavior is deterministic:

- exact path claim takes precedence over namespace claim
- conflicting plugin claims fail startup
- unmet required claims fail startup

## Compatibility policy

- Pre-baseline plugin command bridge behavior is intentionally
  unsupported (clean break).
- Current baseline is versioned and explicit:
  - capability: `bmux.commands`
  - service interface: `cli-command/v1`
  - operation: `run_path`
  - bridge protocol marker: `BMUXCMD1`
  - bridge protocol version: `1`
- Future compatibility changes should be additive:
  - add `.../v2` interfaces or operations, do not mutate `v1`
    semantics silently
  - negotiate by advertised capabilities/interfaces before selecting
    newer versions
  - keep compatibility seams in shared constants/helpers rather than
    ad-hoc call sites

## Process runtime protocol v1

`runtime = "process"` plugins communicate with BMUX over framed stdio
messages.

- transport marker: `BMUXPRC1`
- frame layout: `<magic><u32_be_payload_len><payload_bytes>`
- payload encoding: service codec message
  (`encode_service_message` / `decode_service_message`)
- protocol version field in request/response envelopes: `1`

Environment passed to the process runtime:

- `BMUX_PLUGIN_RUNTIME_PROTOCOL=stdio-v1`
- `BMUX_PLUGIN_ID=<plugin-id>`
- `BMUX_PLUGIN_RUNTIME_PERSISTENT_WORKER=1` (only when
  `process_persistent_worker = true`)

Process runtime manifest knobs:

- `entry` - process command/path to execute
- `entry_args` - default process arguments
- `process_persistent_worker = true|false` - optional worker mode
  (reuse one process for multiple invocations)

Runtime behavior and constraints:

- `stdout` is reserved for framed protocol responses only.
- non-protocol diagnostics should be written to `stderr`.
- host enforces a process timeout (default 30000ms).
- timeout may be overridden with `BMUX_PROCESS_PLUGIN_TIMEOUT_MS`.
- if a process exits without framed `stdout`, host treats it as
  unsupported for framed operations.

Examples:

- `examples/process-plugin-node/`
- `examples/process-plugin-python/`

These examples focus on frame transport and process lifecycle behavior
and emit BMUX service-codec-compatible response payloads.

Troubleshooting:

- error: `missing BMUXPRC1 frame prefix`
  - cause: process emitted non-protocol bytes to stdout
  - fix: write diagnostics to stderr only; keep stdout framed
    responses only
- error: `truncated frame header` or `truncated payload`
  - cause: incomplete write to stdout
  - fix: write a single complete frame and flush stdout before exit
- error: process entry is not executable
  - cause: entry path exists but lacks execute permissions
  - fix: `chmod +x <entry>` (or use a launch command like `python3`
    with script args)
- error: process plugin timed out
  - cause: process did not return in time
  - fix: optimize startup/handler path or increase
    `BMUX_PROCESS_PLUGIN_TIMEOUT_MS`

Versioning policy for process runtime mirrors other plugin
compatibility rules:

- keep `v1` semantics stable once published
- introduce `v2+` as additive protocol envelopes/operations
- gate newer behavior via explicit protocol version/capability checks

## Migration direction

As context substrate work lands:

- move pane/layout ownership to context runtime structures
- add context IPC/client/plugin host primitives
- keep fallback behavior when plugins are missing
- add persistence migration from legacy single-target state to default
  context state

## Status

This document reflects current agreed decisions from architecture
discussions and should be updated whenever these decisions change.

Operator workflows and related references:

- [`bpdl-spec.md`](./bpdl-spec.md)
- [`plugin-ops.md`](./plugin-ops.md)
- [`plugin-triage-playbook.md`](./plugin-triage-playbook.md)
- [`plugin-perf-troubleshooting.md`](./plugin-perf-troubleshooting.md)
