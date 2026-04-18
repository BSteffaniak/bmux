# BMUX Plugin Definition Language (BPDL) — Specification

BPDL is the typed interface description language BMUX plugins use to
declare their public contracts. It is a small, purpose-built DSL
designed to be **easy to write simple plugins with** while scaling to
**robust, typed, multi-plugin ecosystems**.

This document is the normative grammar and semantics.

## File structure

A BPDL source file represents **one plugin**. Each file has a header
declaring the plugin identity, followed by one or more interfaces.

```bpdl
plugin bmux.windows version 1;

interface windows-state {
    // items...
}

interface windows-events {
    // items...
}
```

## Grammar (informal)

```
schema     := plugin_header interface*
plugin_header
           := "plugin" ident "version" integer ";"
interface  := "interface" ident "{" interface_item* "}"

interface_item
           := record | variant | enum | query | command | events

record     := "record" ident "{" field_list? "}"
variant    := "variant" ident "{" variant_case_list? "}"
enum       := "enum" ident "{" enum_case_list? "}"
query      := "query" ident "(" param_list? ")" "->" type ";"
command    := "command" ident "(" param_list? ")" "->" type ";"
events     := "events" type ";"

field      := ident ":" type
variant_case
           := ident ( "{" field_list "}" )?
enum_case  := ident

type       := primitive
            | named
            | "list" "<" type ">"
            | "result" "<" type "," type ">"
            | "unit"
            | type "?"

primitive  := "bool" | "u8" | "u16" | "u32" | "u64"
            | "i8" | "i16" | "i32" | "i64"
            | "f32" | "f64" | "string" | "bytes" | "uuid"

ident      := [a-zA-Z_] [a-zA-Z0-9_\-.]*
integer    := [0-9]+
```

## Naming conventions

Identifiers use `kebab-case` (`pane-state`) or `snake_case`
(`pane_state`). The Rust codegen normalizes `kebab-case` → `snake_case`
for field/module names and to `PascalCase` for type/trait names.

## Types

### Primitives

| BPDL         | Rust (generated) |
| ------------ | ---------------- |
| `bool`       | `bool`           |
| `u8` … `u64` | `u8` … `u64`     |
| `i8` … `i64` | `i8` … `i64`     |
| `f32`, `f64` | `f32`, `f64`     |
| `string`     | `String`         |
| `bytes`      | `Vec<u8>`        |
| `uuid`       | `::uuid::Uuid`   |
| `unit`       | `()`             |

### Containers

| BPDL           | Rust (generated)              |
| -------------- | ----------------------------- |
| `T?`           | `Option<T>`                   |
| `list<T>`      | `Vec<T>`                      |
| `result<T, E>` | `::std::result::Result<T, E>` |

## Items

### `record`

Structs with named fields.

```bpdl
record pane-state {
    id: uuid,
    focused: bool,
    name: string?,
}
```

### `variant`

Tagged unions. Cases may be unit (`running`) or carry a struct-like
payload (`exited { code: i32 }`). Serde-serialized with a `"kind"` tag
in snake_case.

```bpdl
variant pane-status {
    running,
    exited { exit-code: i32 },
}
```

### `enum`

Pure unit-only tag sets. No payloads. Use `variant` if payloads are
needed.

```bpdl
enum border-style {
    none, ascii, single, double,
}
```

### `query`

Read-only operations. Synchronous-semantics but returned as `async`
from generated client stubs.

```bpdl
query pane-state(id: uuid) -> pane-state?;
```

### `command`

Operations that may mutate state. May be fallible via `result<_, _>`.

```bpdl
command focus-pane(id: uuid) -> result<unit, focus-error>;
```

### `events`

Declares the event type emitted by this interface's event stream. An
interface has **at most one** `events` declaration. Subscribers receive
typed events via `PluginEvent::decode_typed(...)` on the plugin host.

```bpdl
events pane-event;
```

## Code generation

`bmux_plugin_schema_macros::schema!("path/to/file.bpdl")` expands to a
module containing:

- One submodule per `interface` (its name normalized to snake_case).
- Each submodule contains:
  - Rust structs for each `record` (serde-derived).
  - Rust enums for each `variant` (tagged, snake_case serde rename).
  - Rust enums for each `enum` (tagged, snake_case serde rename).
  - A `pub const INTERFACE_ID: &str = "<interface-name>"` matching the
    BPDL name verbatim.
  - A `pub trait <PascalName>` that bundles all queries and commands as
    async methods returning `Pin<Box<dyn Future<...> + Send>>`.

## Semantic rules (validated at compile time)

- Plugin id must be non-empty.
- Type names are unique within an interface.
- All `Named` type references must resolve to a declared type in the
  same interface.
- Variant case names are unique within their variant.
- Enum case names are unique within their enum.
- Operation names (queries + commands) share a namespace and must be
  unique within an interface.
- An interface declares at most one `events` item.

Validation is performed by `bmux_plugin_schema::validate` and produces
`Error::Validate` with a descriptive message on failure.

## Future extensions (not yet supported)

The grammar is intentionally small. Planned additions, each additive:

- `import` statements referencing types from other plugins' schemas.
- Resources (long-lived handles with methods).
- Stream types beyond the single per-interface `events`.
- Default values on record fields (with serde `#[serde(default)]`).
- Inline documentation that survives into generated rustdoc.

## Example — windows plugin

See `plugins/windows-plugin-api/bpdl/windows-plugin.bpdl` for the
production schema defining the windows plugin's complete public API
(three interfaces: state queries, commands, and a pane-event stream).
