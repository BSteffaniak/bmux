# BMUX Plugin Definition Language (BPDL) — Specification

BPDL is the typed interface description language BMUX plugins use to
declare their public contracts. It is a small, purpose-built DSL
designed to be **easy to write simple plugins with** while scaling to
**robust, typed, multi-plugin ecosystems**.

This document is the normative grammar and semantics.

## File structure

A BPDL source file represents **one plugin**. Each file has a header
declaring the plugin identity, optional `import` directives referencing
other plugins' schemas, followed by one or more interfaces.

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
schema     := plugin_header import* interface*
plugin_header
           := "plugin" dotted_ident "version" integer ";"
import     := "import" ident "=" dotted_ident ";"
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
           := ( "@default" )? ident ( "{" field_list "}" )?
enum_case  := ( "@default" )? ident

type       := primitive
            | named
            | qualified
            | "list" "<" type ">"
            | "map" "<" type "," type ">"
            | "result" "<" type "," type ">"
            | "unit"
            | type "?"

qualified  := ident "." ident           // alias.type-name

primitive  := "bool" | "u8" | "u16" | "u32" | "u64"
            | "i8" | "i16" | "i32" | "i64"
            | "f32" | "f64" | "string" | "bytes" | "uuid"

ident          := [a-zA-Z_] [a-zA-Z0-9_\-]*
dotted_ident   := ident ( "." ident )*
integer        := [0-9]+
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

| BPDL           | Rust (generated)                     |
| -------------- | ------------------------------------ |
| `T?`           | `Option<T>`                          |
| `list<T>`      | `Vec<T>`                             |
| `map<K, V>`    | `::std::collections::BTreeMap<K, V>` |
| `result<T, E>` | `::std::result::Result<T, E>`        |

`map<K, V>` lowers to `BTreeMap` for deterministic JSON key order in
RPC payloads. Keys must be one of `string`, `uuid`, or an integer
primitive (`u8`…`u64`, `i8`…`i64`); other key types are rejected at
validation time.

### Qualified references (imports)

A type from another plugin's schema is referenced via
`<alias>.<type-name>`, where the alias is declared at the top of the
file with `import`:

```bpdl
plugin bmux.decoration version 1;

import windows = bmux.windows;

interface decoration-state {
    command focus-imported-pane(source: windows.pane-state)
        -> result<unit, string>;
}
```

At validation time the alias must be declared; at codegen time the
caller supplies a mapping from alias to Rust crate path (see the
`schema!` macro section below) and the qualified reference is emitted
as `::<crate>::<interface>::<Type>`.

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
in snake_case. A single unit case may be marked `@default` to emit an
`impl Default`.

```bpdl
variant pane-status {
    @default running,
    exited { exit-code: i32 },
}
```

### `enum`

Pure unit-only tag sets. No payloads. Use `variant` if payloads are
needed. At most one case may be marked `@default`.

```bpdl
enum border-style {
    none,
    @default ascii,
    single,
    double,
}
```

The generated code includes `impl Default for BorderStyle { fn default() -> Self { Self::Ascii } }`.

### `query`

Read-only operations. Synchronous-semantics but returned as `async`
from the generated service trait.

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

The `bmux_plugin_schema_macros::schema!` proc macro takes a braced
argument block:

```rust
// Simple schema with no imports.
bmux_plugin_schema_macros::schema! {
    source: "bpdl/windows-plugin.bpdl",
}

// Schema that imports types from another plugin.
bmux_plugin_schema_macros::schema! {
    source: "bpdl/decoration-plugin.bpdl",
    imports: {
        windows: {
            source: "../windows-plugin-api/bpdl/windows-plugin.bpdl",
            crate_path: ::bmux_windows_plugin_api,
        },
    },
}
```

The macro expands to a module containing:

- One submodule per `interface` (its name normalized to snake_case).
- Each submodule contains:
  - Rust structs for each `record` (serde-derived).
  - Rust enums for each `variant` (tagged, snake_case serde rename).
  - Rust enums for each `enum` (tagged, snake_case serde rename).
  - `impl Default` for any `enum` or `variant` with a `@default` case.
  - A `pub const INTERFACE_ID: &str = "<interface-name>"` matching the
    BPDL name verbatim.
  - A `pub trait <PascalName>Service` that bundles all queries and
    commands as async methods returning
    `Pin<Box<dyn Future<...> + Send>>`. Consumers call through
    `&dyn Service`; providers `impl` the trait directly.

Qualified type references are resolved against the `imports` table.
For the example above, `windows.pane-state` emits as
`::bmux_windows_plugin_api::windows_state::PaneState`.

### Inline schemas

For small self-contained schemas and proc-macro tests, a sibling
`schema_inline!` macro takes the BPDL source as a string literal
without touching the filesystem:

```rust
bmux_plugin_schema_macros::schema_inline!(r#"
    plugin my.plugin version 1;
    interface iface { record r { id: uuid } }
"#);
```

Only the single-schema form is supported; imports require `schema!`.

## Semantic rules (validated at compile time)

- Plugin id must be non-empty.
- Import aliases within a single schema must be unique.
- Type names are unique within an interface.
- All `Named` type references must resolve to a declared type in the
  same interface.
- Qualified type references (`alias.type-name`) must reference a
  declared `import` alias. When the `schema!` macro supplies the
  imported schema, the type must also exist in that schema.
- Variant case names are unique within their variant.
- Enum case names are unique within their enum.
- At most one case per `enum` or `variant` may be annotated `@default`.
- `@default` on a variant case is legal only when the case is unit
  (no payload).
- Operation names (queries + commands) share a namespace and must be
  unique within an interface.
- An interface declares at most one `events` item.
- `map<K, _>` keys must be one of `string`, `uuid`, or an integer
  primitive (`u8`…`u64`, `i8`…`i64`).
- `record`/`variant` types must be acyclic in their required fields.
  Cycles through `T?`, `list<T>`, or `map<_, T>` _value_ position are
  allowed because the generated Rust compiles; direct `T` fields
  (including via `result<T, _>` or `result<_, T>`) form cycle edges
  and are rejected.

Validation is performed by `bmux_plugin_schema::validate` (or
`validate_with_imports` for full cross-schema resolution) and produces
`Error::Validate` with a descriptive message on failure.

## Runtime registry

`bmux_plugin_schema::registry::SchemaRegistry` is the in-memory store
the plugin host uses at load time:

```rust
let mut reg = SchemaRegistry::new();
reg.register(windows_schema_source)?;
reg.register(decoration_schema_source)?;
reg.check_compatibility("bmux.windows", "bmux.decoration", "windows-events")?;
```

`check_compatibility` returns `Err(Vec<CompatError>)` listing every
facet that doesn't match — version mismatches, missing interfaces,
missing operations, and operation signature disagreements.

## Future extensions (not yet supported)

The grammar is intentionally small. Planned additions, each additive:

- Resources (long-lived handles with methods).
- Stream types beyond the single per-interface `events`.
- Default values on record fields (with serde `#[serde(default)]`).
- Inline documentation that survives into generated rustdoc.

## Example — windows plugin

See `plugins/windows-plugin-api/bpdl/windows-plugin.bpdl` for the
production schema defining the windows plugin's complete public API
(three interfaces: state queries, commands, and a pane-event stream).
