# BMUX Plugin Definition Language (BPDL) — Specification

BPDL is the typed interface description language BMUX plugins use to
declare their public contracts. It is a small, purpose-built DSL
designed to be **easy to write simple plugins with** while scaling to
**robust, typed, multi-plugin ecosystems**.

This document is the normative grammar and semantics.

## File structure

A BPDL source file represents **one plugin package**. Each file has a
header declaring the package identity, optional `import` directives
referencing other packages, capability constants, then interfaces and
plugin-level declarations (worlds, commands, contributions, extension
points) in any order.

```bpdl
plugin bmux.tabs version 1;

interface tabs-state {
    // items...
}

interface tabs-events {
    // items...
}
```

## Grammar (informal)

`///` documentation lines may precede every declaration (plugin header,
capability, interface, type, field, case, flag, method, operation, world
entry, command, extension point). Plain `//` comments are ignored.

```
schema     := plugin_header import* capability* top_item*
plugin_header
           := "plugin" dotted_ident "version" integer ";"
import     := "import" ident "=" dotted_ident ";"
capability := "capability" ident "=" dotted_ident ";"
top_item   := annotation* ( interface | world | plugin_command
                          | contribution | extension_point )

annotation := "@" ident ( "(" ( ident | integer ) ")" )?
              // interface: @capability(NAME) @interface-version(N) @major(N)
              // items:     @reentrant (query) @state (events) @default (case)
              // anywhere:  @since(N) @unstable(feature) @deprecated(N)

interface  := "interface" ident "{" ( annotation* interface_item )* "}"
interface_item
           := record | variant | enum | flags | resource
            | query | command | events

record     := "record" ident "{" field_list? "}"
variant    := "variant" ident "{" variant_case_list? "}"
enum       := "enum" ident "{" enum_case_list? "}"
flags      := "flags" ident "{" ident ( "," ident )* ","? "}"
resource   := "resource" ident "{" resource_method* "}"
resource_method
           := "constructor" "(" param_list? ")" ";"
            | "static"? ( "query" | "command" ) ident "(" param_list? ")" "->" type ";"
query      := "query" ident "(" param_list? ")" "->" type ( "emits" type )? ";"
command    := "command" ident "(" param_list? ")" "->" type ( "emits" type )? ";"
events     := "events" type ";"

world      := "world" ident "{" world_item* "}"
world_item := "provide" interface_ref ( "capability" dotted_ident )? ";"
            | "provide" "opaque" ident ( "@" integer )? ":" service_kind
                  "capability" dotted_ident ";"
            | "provide" "capability" dotted_ident ";"
            | "require" "optional"? interface_ref ";"
            | "require" "optional"? "capability" dotted_ident ";"
interface_ref
           := ( ident "." )? ident ( "@" integer )?
service_kind
           := "query" | "command" | "events"

plugin_command
           := "command" ident "(" param_list? ")" ( "title" string )? ";"
contribution
           := "contribute" ( ident "." )? ident "@" integer
                  "{" ( ident ":" literal ( "," ident ":" literal )* ","? )? "}"
extension_point
           := "extension-point" ident "version" integer "{"
                  "data" "{" field_list? "}"
                  ( "interface" "{" ( annotation* ( query | command ) )* "}" )?
              "}"

field      := "@secret"? ident ":" type
variant_case
           := ( "@default" )? ident ( "{" field_list "}" )?
enum_case  := ( "@default" )? ident

type       := primitive
            | named
            | qualified
            | "list" "<" type ">"
            | "map" "<" type "," type ">"
            | "result" "<" type "," type ">"
            | "tuple" "<" type ( "," type )* ">"
            | "stream" "<" type ">"
            | "future" "<" type ">"
            | "own" "<" ( named | qualified ) ">"
            | "borrow" "<" ( named | qualified ) ">"
            | "unit"
            | type "?"

qualified  := ident "." ident           // alias.type-name

primitive  := "bool" | "u8" | "u16" | "u32" | "u64"
            | "i8" | "i16" | "i32" | "i64"
            | "f32" | "f64" | "char" | "string" | "bytes" | "uuid"

literal    := string | integer | "true" | "false" | "[" ( literal ( "," literal )* )? "]"
ident          := [a-zA-Z_] [a-zA-Z0-9_\-]*
dotted_ident   := ident ( "." ( ident | keyword ) )*
integer        := [0-9]+
string         := '"' ( [^"\\\n] | '\"' | '\\' | '\n' | '\t' )* '"'
```

`world`, `resource`, `flags`, `tuple`, `stream`, `future`, `own`, `borrow`,
`provide`, `require`, `opaque`, `contribute`, `extension-point`, `data` and
`title` are contextual: without their following syntax (for example a `<`
after `stream`) they remain ordinary names, so existing schemas keep using
them as field, type and interface names.

### Example

```bpdl
/// Files exposed by an example host.
plugin acme.files version 2;

import clock = acme.clock;

capability READ = acme.files.read;

/// File access.
@capability(READ)
@since(1)
interface files {
    /// Permission bits.
    flags mode { read, write, exec }

    /// An open file handle.
    resource file {
        constructor(path: string);
        query size() -> u64;
        static query exists(path: string) -> bool;
    }

    record stat { pair: tuple<u64, char>, mode: mode, modified: clock.instant }

    @deprecated(2) @since(1)
    query stat(f: borrow<file>) -> stat;
    query open(path: string) -> own<file>;
    query lines(path: string) -> stream<string>;
}

interface changes {
    @state events files.stat;
}

/// What the plugin provides and requires.
world files-plugin {
    provide files;
    provide changes capability acme.files.read;
    provide opaque legacy-files@2: command capability acme.files.write;
    require clock.clock;
    require optional capability acme.storage;
}

command cat(path: string, lines: u32?) title "Print a file";

extension-point file-preview version 1 {
    data { id: string, order: u32, label: string? }
    interface { query render(width: u16) -> list<string>; }
}

contribute file-preview@1 { id: "text", order: 10 }
```

## Naming conventions

Identifiers use `kebab-case` (`pane-state`) or `snake_case`
(`pane_state`). The Rust codegen normalizes `kebab-case` → `snake_case`
for field/module names and to `PascalCase` for type/trait names.

An interface's wire identifier defaults to its source name. Add
`@interface-version(N)` when a stable service uses a slash-versioned wire ID:

```bpdl
@capability(CLUSTER_READ)
@interface-version(1)
interface cluster-query {
    query list-clusters() -> list<string>;
}
```

This generates the Rust module `cluster_query` and wire identifier
`cluster-query/v1`. Versions must be positive `u32` integers. An interface may
have at most one `@capability` and one `@interface-version` annotation; their
order is not significant.

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
| `char`       | `char`           |
| `unit`       | `()`             |

### Containers

| BPDL           | Rust (generated)                     |
| -------------- | ------------------------------------ |
| `T?`           | `Option<T>`                          |
| `list<T>`      | `Vec<T>`                             |
| `map<K, V>`    | `::std::collections::BTreeMap<K, V>` |
| `result<T, E>` | `::std::result::Result<T, E>`        |
| `tuple<A, B>`  | `(A, B)`                             |

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

import windows = bmux.tabs;

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

The generated code includes `impl Default for BorderStyle { fn default() -> Self
{ Self::Ascii } }`.

### `query`

Read-only operations. Synchronous-semantics but returned as `async`
from the generated service trait.

```bpdl
query pane-state(id: uuid) -> pane-state?;
```

#### `@reentrant`

By default the host rejects a callback that would re-enter a plugin
instance which already has a live entry on the same call chain (for
example plugin A → plugin B → plugin A). This prevents deadlocks when
the outer entry holds instance locks. A query that is genuinely safe to
re-enter can opt in at the contract level:

```bpdl
@reentrant query current-stream-provenance() -> result<stream-provenance, identity-error>;
```

Only mark a query `@reentrant` when it is stateless or read-only, takes
no instance-exclusive locks, and never blocks on outbound calls.

- `@reentrant` is accepted only directly before `query`. It is a parse
  error on `command` operations, on operations with `emits`
  (streaming), and on any other item.
- An unmarked query keeps its exact canonical contract bytes, so
  existing interface identities and digests do not change. A marked
  query is encoded with a distinct item tag, because the marker changes
  concurrency semantics and so changes the interface identity.
- Codegen emits `REENTRANT_OPERATIONS: &[&str]` in each service
  interface module and carries it in `SERVICE_DESCRIPTOR`
  (`ServiceInterfaceDescriptor::reentrant_operations`). The generated
  `service_declarations()` copies it into
  `PluginService::reentrant_operations`, which is how the host sees it
  without running plugin code. Manifests may omit the field, and it is
  left out of serialized manifests when empty.
- The host marks a nested provider binding re-entrant only when the
  request kind is `query` and the provider's declaration lists that
  operation for the interface. That binding skips only the same-instance
  ancestor rejection. The ancestry depth limit, liveness, admission and
  the one-author-entry-per-invocation checks all still apply.
- A re-entrant entry never waits on instance-local admission (the
  adapter's instance gate or the callback slots). If its ancestor holds
  the gate, or a writer is queued ahead of it, the call fails immediately
  with an `unavailable` error ("reentrant call would deadlock on
  exclusive instance gate") instead of hanging.
- Native dynamic components on callback authority v1 cannot be told an
  entry is a re-entry, so the host rejects re-entrant entries over that
  ABI.

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

### `flags`

A set of independent named booleans. Rust lowers a `flags` type to a
transparent `u64` bit set (flag *i* is bit *i* in declaration order, so at
most 64 flags), with `empty`, `all`, `contains`, `union` and
`from_bits_truncate`.

### `resource`

A handle type with methods. Values of a resource type are handles:
`own<R>` (or a bare `R`) transfers ownership, `borrow<R>` lends the handle
for one call and is only legal in parameters. `query` methods are
read-only, `command` methods mutate, `static` methods take no receiver and
`constructor` returns a new owned handle.

### `tuple`, `char`, `stream<T>`, `future<T>`

`tuple<A, B, ...>` is a positional product (Rust tuple); `char` is a Unicode
scalar value (Rust `char`). `stream<T>` delivers zero or more values and
`future<T>` exactly one value asynchronously; both may appear in any type
position.

Resources, handles, `stream<T>` and `future<T>` are part of the language,
the IR, contract identity and the WIT lowering. The Rust tier has no
binding for them yet (they need the host handle table and stream
transport): generated Rust fails with an explicit `compile_error!` naming
the construct rather than degrading the contract.

## Documentation and stability

`///` lines document the declaration that follows them. Rust codegen keeps
them on generated types, fields and cases; WIT keeps them as WIT docs.

`@since(N)` and `@deprecated(N)` name package versions (at most the
`plugin ... version` of the file; a deprecation needs an earlier or equal
`@since`), and `@unstable(feature)` gates an item behind a named feature
(and excludes `@since`/`@deprecated`). They lower to the WIT gates
`@since(version = N.0.0)`, `@deprecated(version = N.0.0)` and
`@unstable(feature = ...)`.

Documentation and stability are metadata: neither enters contract
identity.

## Plugin declarations

### `world`

A world lists what the plugin provides (`provide`, WIT `export`) and what
it requires (`require`, WIT `import`):

- `provide iface;` serves a local or imported (`alias.iface`) service
  interface. `@N` selects one member of a versioned family. The
  capability comes from the interface's `@capability`; event interfaces,
  which declare none, must name it: `provide iface capability <id>;`.
- `provide opaque name[@N]: <kind> capability <id>;` routes a service whose
  operations are not modeled in BPDL yet. It carries no contract identity
  and no WIT lowering; every opaque entry is a modeling debt.
- `provide capability <id>;` grants a capability that has no service.
- `require [optional] iface;` and `require [optional] capability <id>;`
  name what the plugin calls; an interface requirement contributes the
  interface's capability.

A plugin's package is the BPDL file whose `plugin` id equals the plugin
id: the plugin API crate's schema when one exists, otherwise a
`bpdl/<plugin>.bpdl` in the plugin crate. It declares exactly one world.

### `command`

A plugin-level `command name(params) title "..."` declares a CLI command.
Parameters must be scalars, optional scalars or `list<string>`; they lower
to long options (`string`, `integer`, `boolean`).

### `extension-point` and `contribute`

An extension point is a host- or plugin-declared contract: a `data`
record every contribution supplies (readable before activation) and an
optional `interface` of operations contributing plugins implement. Its id
is `<package>.<name>/v<version>` and it has its own contract identity.
`contribute name@N { field: literal, ... }` supplies data; validation
checks field names, literal types and required (non-optional) fields.

## Canonical IR and contract identity

After validation the compiler lowers each file into a span-free,
serializable IR (`bmux_plugin_schema::ir::Package`). Every backend reads
the IR: contract identity, the WIT emitter, manifest generation and,
later, other language SDKs. A `PackageSet` holds every package of a
contract closure and resolves imports by package id, like a WIT
`Resolve`. Rust codegen still walks the validated syntax tree.

Interface identity (`SERVICE_CONTRACT`) is a SHA-256 computed from the
IR with the canonical v2 byte scheme documented in
`packages/plugin-schema/src/canonical.rs`. Every construct that existed
before the IR hashes byte-for-byte as before, so all pinned repository
identities (`packages/plugin-schema/tests/fixtures/contract-identities.txt`)
are unchanged. New constructs (flags, resources, tuples, streams, futures,
handles) use type and item tags no earlier schema could produce, which is
why they extend v2 instead of requiring v3. `@major`, documentation and
stability gates are not identity-bearing. Ambiguous or unresolved imported
names produce empty evidence, never a guess.

Contract evolution is exact identity plus multi-version adapters: a wire
change is a new `@interface-version`, and providers serve each version
they support. Additive compatibility is not inferred.

`@major(N)` records the contract major of an interface whose wire id has
no `/vN` suffix (legacy ids such as `presenter-view-v2`). Versioned
interfaces use their version as their major; everything else is major 1.

## WIT lowering

`bmux_plugin_schema::wit::emit` lowers a package to WIT
(`plugin a.b version N` becomes `package a:b@N.0.0`). Records, enums,
flags and resources map directly; variant case payloads become
`<variant>-<case>` records; `uuid` becomes `tuple<u64, u64>`, `bytes`
`list<u8>`, `map<K, V>` `list<tuple<K, V>>`; streaming operations return
`tuple<stream<E>, future<R>>`; `events T` becomes `subscribe: func() ->
stream<T>`; worlds map `provide`/`require` to `export`/`import`;
extension points become `<name>-vN` interfaces and plugin commands a
`plugin-commands` interface. Capabilities, retained-state delivery and
`@secret` survive as documentation; opaque services, bare capabilities
and contributions as comments.

WIT worlds cannot export an interface that other exports `use` types
from, so an interface holding both types and operations lowers to
`<name>-types` (its types) plus `<name>` (operations only, `use`-ing the
types). Functions and types share one WIT namespace per interface, so
name clashes resolve deterministically, never by renaming the wire-facing
BPDL name:

- In a split interface, a function keeps its BPDL name and a clashing
  imported type is brought in under an alias prefixed with its owning
  interface: `stat: func(...)` with `use files-types.{stat as files-stat}`.
  The same alias rule applies to any `use` of a type whose name is
  already taken locally.
- In an unsplit interface (a type and a function declared together), the
  function takes a `-fn` suffix (`stat-fn`).

Lowering never affects contract identity, which is computed from the IR.

`packages/plugin-schema/tests/repo_contracts.rs` lowers every repository
contract schema and validates the result with the reference `wit-parser`.
`cargo run -p bmux_plugin_schema --example emit -- --wit <file>` prints
the lowering of a self-contained schema.

## Manifest generation

Plugin manifests are generated from the package world.
`bmux_plugin_schema::manifest::generate` merges the plugin's `plugin.toml`
settings with:

| Key                     | Generated from                                                                 |
| ----------------------- | ------------------------------------------------------------------------------ |
| `services`              | `provide` entries (capability, kind, `@reentrant` operations)                  |
| `contracts`             | each provided service: wire id, major (`@interface-version`, `@major`, else 1) |
| `provided_capabilities` | provided services and `provide capability`                                     |
| `required_capabilities` | `require` entries                                                              |
| `optional_capabilities` | `require optional` entries                                                     |
| `commands`              | plugin-level `command` declarations, when present                              |

`plugin.toml` keeps only settings BPDL does not model: `id` (checked
against the package id), `name`, `version`, `execution_class`,
`component_roles`, `provided_features`, dependencies, owned namespaces,
event publications, keybindings, `bundled_only` and — until they are
declared in BPDL — CLI commands. Restating a generated key is a compile
error.

`bundled_only = "<reason>"` stays a `plugin.toml` setting rather than a
BPDL declaration: it records how an implementation may be deployed (it
uses host facilities plug-rt does not proxy into a dynamic library), not
what it provides or requires, and it never affects contract identity or
compatibility with consumers. The generator carries it into
`PLUGIN_MANIFEST` verbatim, where `PluginDeclarations` reads it and the
dynamic loader refuses the plugin with a typed error.

A plugin crate generates its manifest at compile time:

```rust
// Package in a plugin API crate (which exports it with `export: export_schema`).
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_tabs_plugin_api::export_schema,
            crate_path: ::bmux_tabs_plugin_api,
        },
        settings: "plugin.toml",
    }
}

// Package in the plugin crate itself.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/clipboard-plugin.bpdl",
        manifest: "plugin.toml",
    }
}
```

Both emit `PLUGIN_ID`, `PLUGIN_MANIFEST` (passed to the plugin
definition, embedded in dynamic artifacts and published by
`scripts/package-plugin.py --install-root` as the sidecar `plugin.toml`,
so the host reads it without running plugin code) and `provides`, which
re-exports exactly the provided interfaces. Typed route tables name
endpoints through it, so a route the world does not declare fails to
compile:

```rust
use crate::declaration::provides::workspaces_state::client as state;
bmux_plugin_sdk::route_service!(context, {
    state::ListWorkspacesEndpoint => |(), ctx| list(ctx),
})
```

## Code generation

The `bmux_plugin_schema_macros::schema!` proc macro takes a braced
argument block:

```rust
// Simple schema with no imports.
bmux_plugin_schema_macros::schema! {
    source: "bpdl/tabs-plugin.bpdl",
}

// Schema that imports types from another plugin.
bmux_plugin_schema_macros::schema! {
    source: "bpdl/decoration-plugin.bpdl",
    imports: {
        windows: {
            source: "../tabs-plugin-api/bpdl/tabs-plugin.bpdl",
            crate_path: ::bmux_tabs_plugin_api,
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

  - For an interface with a capability and operations, a `v<N>` module of
    author bindings generated from the canonical IR: an async provider
    trait (`counter_commands::v1::CounterCommands`, methods taking
    `&author::Call`), a typed `Client` (direct bundled calls or any
    transport through the checked codec), a `dispatch` adapter for
    `PluginInstance::invoke`, a `Portable<P>` checked adapter, `register`
    for the typed registry, and `Provides`/`RequiredInterface` markers.
    An `events` payload defined in the package implements
    `author::ContractEvent`.
- A `commands` module (typed `<Name>Args` and `CommandDecl` constants) for
  plugin-level `command` declarations.
- An `extension_points` module (`<name>_v<N>`: `Data`, `Behavior`,
  `Request`, `Point`) for `extension-point` declarations.

With `manifest:` (or `plugin_manifest!`) the macro also emits `PLUGIN_ID`,
`PLUGIN_MANIFEST`, `SERVICE_CONTRACTS` and the `provides`/`requires`
modules. See `docs/plugin-author-api.md` for how the author API consumes
these items.

Qualified type references are resolved against the `imports` table.
For the example above, `windows.pane-state` emits as
`::bmux_tabs_plugin_api::tabs_state::PaneState`.

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
- `@reentrant` may only annotate a non-streaming `query`.
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
reg.check_compatibility("bmux.tabs", "bmux.decoration", "tabs-events")?;
```

`check_compatibility` returns `Err(Vec<CompatError>)` listing every
facet that doesn't match — version mismatches, missing interfaces,
missing operations, and operation signature disagreements.

## Future extensions (not yet supported)

Planned additions, each additive:

- Rust bindings for resources, handles, `stream<T>` and `future<T>` (needs
  the host handle table and stream transport).
- Rust codegen from the IR instead of the syntax tree.
- CLI command annotations (aliases, short flags, value names, execution
  kind) so every plugin's `[[commands]]` move into BPDL.
- Contribution payload encoding into manifest `[[extensions]]` once
  host extension points (`bmux-extpoints`) consume them.
- Default values on record fields (with serde `#[serde(default)]`).

## Example — windows plugin

See `plugins/tabs-plugin-api/bpdl/tabs-plugin.bpdl` for the
production schema defining the windows plugin's complete public API
(three interfaces: state queries, commands, and a pane-event stream).
