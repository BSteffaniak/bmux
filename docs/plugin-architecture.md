# BMUX Plugin Architecture

This document describes the plugin model that BMUX uses to keep the
core runtime domain-agnostic while letting plugins implement rich,
typed behavior.

## Design principles

1. **Core is domain-agnostic.** `packages/server`, `packages/client`,
   `packages/ipc`, `packages/session`, `packages/terminal`, and
   `packages/event` contain no windows-domain or permissions-domain
   logic. Plugins own all product concepts.
2. **Plugins are composable and typed.** Plugins declare their public
   API in BPDL (see `bpdl-spec.md`). Other plugins consume those APIs
   as typed Rust traits generated at compile time.
3. **Easy to write simple plugins, powerful enough for complex ones.**
   A minimal plugin is ~30 lines of Rust. Rich plugins like the
   windows plugin compose naturally with other plugins through the
   typed service and event systems.
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

- `plugins/windows-plugin-api` + `plugins/windows-plugin` (owns pane /
  window / tab lifecycle; exposes state queries, commands, events).
- `plugins/decoration-plugin-api` + `plugins/decoration-plugin` (owns
  pane visual styling; depends on `windows-plugin-api`).

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

## Mouse dispatch

Core's mouse handling is a pure hit-test → event emitter:

1. Hit-test identifies the topmost `AttachSurface` containing the
   click.
2. If the click is inside `content_rect`, core encodes the click in
   the pane's mouse protocol and forwards the bytes to the PTY. This
   is the path the current PR fully implements.
3. If the click is inside an `interactive_region`, core emits a
   `SurfaceRegionClicked` plugin event targeting the region's
   `owning_plugin_id`. The owning plugin's subscribers react. (This
   path is scaffolded — the event stream carries the required data —
   and will be wired into full region-click dispatch in a follow-up.)

Coordinate translation uses `content_rect` so that clicks at the
visual top-left content cell encode as pane-local `(1, 1)` regardless
of the thickness or style of the surrounding decoration.

## Core defaults when plugins are missing

Per AGENTS.md:

- **Windows plugin missing**: baseline single-terminal attach / session
  / pane flow still works. Core falls back to painting a default
  1-cell border in the renderer (`packages/attach_pipeline/src/render.rs`).
- **Permissions plugin missing**: permissive single-user behavior.
- **Decoration plugin missing**: surfaces render with `content_rect == rect - 1 on each side` and the renderer paints the default ASCII
  border. A follow-up will let the scene producer emit
  `content_rect == rect` (no chrome) when no decoration plugin is
  present, and have the renderer no-op on border painting.

## Writing a plugin

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
