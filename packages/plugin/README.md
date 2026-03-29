# bmux_plugin

Host-side plugin infrastructure for bmux.

## Crate Architecture

The plugin system is split into two crates:

| Crate | Audience | What it provides |
|-------|----------|-----------------|
| **`bmux_plugin_sdk`** | Plugin authors | `RustPlugin` trait, context types, macros, service helpers, prelude |
| **`bmux_plugin`** | Host runtime | Registry, loader, discovery, manifest parsing, `ServiceCaller`, `HostRuntimeApi` |

**Plugin authors** should depend on `bmux_plugin_sdk` for a slim dependency footprint. Add `bmux_plugin` only if you need host service calls (`HostRuntimeApi`, `ServiceCaller`).

## Quick Start

### 1. Create `plugin.toml`

```toml
id      = "example.hello"
name    = "Hello Plugin"
version = "0.1.0"

[[commands]]
name          = "hello"
summary       = "Print a greeting"
expose_in_cli = true
```

### 2. Create `src/lib.rs`

```rust
use bmux_plugin_sdk::prelude::*;

#[derive(Default)]
pub struct HelloPlugin;

impl RustPlugin for HelloPlugin {
    fn run_command(&mut self, ctx: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(ctx, {
            "hello" => {
                let name = ctx.arguments.first().map_or("world", String::as_str);
                println!("Hello, {name}!");
                Ok(EXIT_OK)
            },
        })
    }
}

bmux_plugin_sdk::export_plugin!(HelloPlugin, include_str!("../plugin.toml"));
```

### 3. Create `Cargo.toml`

```toml
[package]
name    = "my_plugin"
edition = "2021"
version = "0.1.0"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
bmux_plugin_sdk = { version = "..." }

[features]
static-bundled = []
```

The `crate-type` must include `cdylib` (for dynamic loading) and `rlib` (for static bundling). The `static-bundled` feature is used when the plugin is compiled into the host binary.

## Manifest Reference (`plugin.toml`)

### Top-Level Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `id` | string | **Yes** | — | Unique plugin identifier (e.g. `"bmux.clipboard"`, `"example.hello"`) |
| `name` | string | **Yes** | — | Human-readable display name |
| `version` | string | **Yes** | — | Plugin version (semver) |
| `description` | string | No | — | Optional description |
| `homepage` | string | No | — | Optional URL |
| `runtime` | string | No | `"native"` | Runtime type (only `"native"` is supported) |
| `entry` | path | No | — | Path to `.dylib`/`.so` file (only needed for external/dynamic plugins) |
| `entry_symbol` | string | No | `"bmux_plugin_entry_v1"` | FFI entry point symbol name |
| `provider_priority` | integer | No | `0` | Ordering priority when multiple plugins provide the same capability |
| `required_capabilities` | list | No | `[]` | Host capabilities this plugin needs (e.g. `["bmux.commands"]`) |
| `provided_capabilities` | list | No | `[]` | Capabilities this plugin provides to others |
| `provided_features` | list | No | `[]` | Feature flags this plugin provides |

### Compatibility (optional sections)

```toml
[plugin_api]
minimum = "1.0"    # default
maximum = "2.0"    # optional upper bound

[native_abi]
minimum = "1.0"    # default
```

Both sections default to `minimum = "1.0"` with no maximum and can be omitted entirely.

### Commands (`[[commands]]`)

```toml
[[commands]]
name          = "hello"          # required: dispatch name (matches context.command)
summary       = "Print a greeting" # required: short description
expose_in_cli = true             # default: false -- set true to show in CLI
path          = ["greet"]        # optional: CLI subcommand path
aliases       = [["say", "hi"]]  # optional: alternative CLI paths
execution     = "provider_exec"  # default: "provider_exec"
description   = "Longer help text" # optional
```

#### Command Arguments (`[[commands.arguments]]`)

```toml
[[commands.arguments]]
name     = "target"       # required
kind     = "string"       # required: string | integer | boolean | path | choice
required = true           # default: false
position = 0              # optional: positional index
long     = "target"       # optional: --target
short    = "t"            # optional: -t
multiple = true           # default: false
value_name = "TARGET"     # optional: displayed in help
choice_values = ["a","b"] # optional: valid values for kind = "choice"
```

### Services (`[[services]]`)

```toml
[[services]]
capability   = "bmux.clipboard.write"  # required: host capability scope
interface_id = "clipboard-write/v1"    # required: service interface identifier
kind         = "command"               # required: "command" or "query"
```

### Event Subscriptions (`[[event_subscriptions]]`)

```toml
[[event_subscriptions]]
kinds = ["system", "window"]          # event categories
names = ["server_started", "window_created"]  # specific event names
```

### Dependencies (`[[dependencies]]`)

```toml
[[dependencies]]
plugin_id   = "bmux.permissions"
version_req = "=0.0.1-alpha.0"
required    = true                    # default: true
```

### Keybindings (`[keybindings]`)

```toml
[keybindings.runtime]
c = "plugin:bmux.windows:new-window"
"alt+w" = "plugin:bmux.windows:switch-window"

[keybindings.global]
# global keybindings (active outside runtime mode)

[keybindings.scroll]
y = "copy_scrollback"
```

## The `RustPlugin` Trait

All five methods have default implementations. Override only what your plugin needs:

| Method | When to override |
|--------|-----------------|
| `run_command` | Plugin provides CLI commands |
| `invoke_service` | Plugin provides services to other plugins |
| `activate` / `deactivate` | Plugin needs lifecycle setup/teardown |
| `handle_event` | Plugin subscribes to system events |

Commands and lifecycle methods return `Result<i32, PluginCommandError>`. Use the `?` operator with string errors, `io::Error`, `serde_json::Error`, etc. -- they all convert automatically via `From` impls.

Service handlers return `ServiceResponse`. Use `route_service!` to reduce boilerplate.

## Design Notes

- Prefer plugin-facing DTOs, handles, and service traits over direct access to server internals
- Treat hot-path runtime hooks as explicit high-risk host scopes
- Keep ordinary plugins compatible across bmux releases by stabilizing `bmux_plugin_sdk` first
- Leave room for future non-Rust runtimes by defining manifests, host scopes, and plugin features as host concepts
