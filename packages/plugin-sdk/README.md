# bmux Plugin SDK

Everything you need to write a bmux plugin.

## Quick Start

A bmux plugin is a Rust crate with three files: a manifest, a library, and a `Cargo.toml`.

### 1. `plugin.toml`

```toml
id      = "example.hello"
name    = "Hello Plugin"
version = "0.1.0"

[[commands]]
name          = "hello"
summary       = "Print a greeting"
expose_in_cli = true
```

### 2. `src/lib.rs`

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

### 3. `Cargo.toml`

```toml
[package]
name    = "my_plugin"
edition = "2024"
version = "0.1.0"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
bmux_plugin_sdk = { version = "..." }

[features]
static-bundled = []
```

`crate-type` must include `cdylib` (for dynamic loading) and `rlib` (for static bundling into the host binary).

## The `RustPlugin` Trait

Every plugin implements `RustPlugin`. All five methods have default implementations -- override only what your plugin needs:

| Method | Return type | When to override |
|--------|------------|-----------------|
| `run_command` | `Result<i32, PluginCommandError>` | Plugin provides CLI commands |
| `invoke_service` | `ServiceResponse` | Plugin provides services to other plugins |
| `activate` | `Result<i32, PluginCommandError>` | Plugin needs setup on activation |
| `deactivate` | `Result<i32, PluginCommandError>` | Plugin needs cleanup on deactivation |
| `handle_event` | `Result<i32, PluginCommandError>` | Plugin subscribes to system events |

The trait requires `Default + Send + 'static`. Use `#[derive(Default)]` on your struct.

## Writing Command Plugins

### Dispatching commands

Use `route_command!` to match on the command name and auto-generate the unknown-command fallback:

```rust
fn run_command(&mut self, ctx: NativeCommandContext) -> Result<i32, PluginCommandError> {
    bmux_plugin_sdk::route_command!(ctx, {
        "list"   => handle_list(&ctx),
        "create" => handle_create(&ctx),
    })
}
```

Each arm must evaluate to `Result<i32, PluginCommandError>`. Unrecognised commands automatically return `Err(PluginCommandError::unknown_command(...))`.

### Error handling

`PluginCommandError` implements `From` for common error types, so the `?` operator works naturally:

```rust
fn handle_list(ctx: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let data = std::fs::read_to_string("config.toml")?;  // io::Error -> PluginCommandError
    let parsed: Config = toml::from_str(&data)?;          // toml::de::Error -> PluginCommandError
    println!("{parsed:?}");
    Ok(EXIT_OK)
}
```

Supported conversions: `String`, `&str`, `std::io::Error`, `serde_json::Error`, `toml::de::Error`, `Box<dyn Error>`, `Box<dyn Error + Send + Sync>`.

For custom error codes, construct directly:

```rust
Err(PluginCommandError::new(EXIT_USAGE, "missing required argument"))
Err(PluginCommandError::unavailable("feature not supported on this platform"))
```

### Exit codes

| Constant | Value | Meaning |
|----------|-------|---------|
| `EXIT_OK` | 0 | Success |
| `EXIT_ERROR` | 1 | Generic failure |
| `EXIT_USAGE` | 64 | Bad arguments or unknown command |
| `EXIT_UNAVAILABLE` | 70 | Plugin unavailable |

### Arguments

Commands receive arguments as `ctx.arguments: Vec<String>`. The host CLI layer handles parsing and validation based on the argument declarations in `plugin.toml` -- the plugin receives the parsed values as strings.

## Writing Service Plugins

Service plugins handle inbound requests from other plugins or the host runtime.

### Dispatching services

Use `route_service!` to match on `(interface_id, operation)` pairs. Each handler receives a typed request and returns a typed response:

```rust
fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
    bmux_plugin_sdk::route_service!(context, {
        "my-service/v1", "do_thing" => |req: DoThingRequest, _ctx| {
            let result = do_the_thing(&req.input)?;
            Ok(DoThingResponse { output: result })
        },
    })
}
```

The macro wraps each handler in `handle_service()`, which handles request deserialization, response serialization, and error conversion. Unrecognised operations return a standard "unsupported" error.

Request and response types must implement `serde::Deserialize` and `serde::Serialize` respectively.

### Returning errors from service handlers

Service handler closures return `Result<Resp, ServiceResponse>`. Use `map_err` to convert domain errors:

```rust
"my-service/v1", "create" => |req: CreateRequest, _ctx| {
    create_thing(&req.name)
        .map_err(|e| ServiceResponse::error("create_failed", e.to_string()))
},
```

### The `handle_service` function

If you need more control than `route_service!` provides, use `handle_service` directly:

```rust
handle_service(&context, |req: MyRequest, ctx| {
    // Full access to ctx for host API calls, self for plugin state
    Ok(MyResponse { ... })
})
```

## The Prelude

`use bmux_plugin_sdk::prelude::*` imports the ~16 items most plugins need:

- **Trait:** `RustPlugin`
- **Context types:** `NativeCommandContext`, `NativeLifecycleContext`, `NativeServiceContext`
- **Error type:** `PluginCommandError`
- **Exit codes:** `EXIT_OK`, `EXIT_ERROR`, `EXIT_USAGE`, `EXIT_UNAVAILABLE`
- **Service types:** `ServiceKind`, `ServiceResponse`
- **Events:** `PluginEvent`
- **Helpers:** `handle_service`, `decode_service_message`, `encode_service_message`

Types not in the prelude (import individually when needed):

- Host service DTOs: `SessionSelector`, `StorageGetRequest`, `ContextCreateRequest`, etc.
- Manifest types: `PluginCommand`, `PluginService`, `PluginEventSubscription`
- Capability types: `HostScope`, `PluginFeature`

## When to Also Depend on `bmux_plugin`

The `bmux_plugin` crate provides two traits that live outside the SDK:

- **`ServiceCaller`** -- the low-level trait for dispatching cross-plugin service calls
- **`HostRuntimeApi`** -- ergonomic methods like `ctx.session_list()`, `ctx.storage_get(...)`, `ctx.context_create(...)`

If your plugin calls host services (session management, storage, pane operations, etc.), add `bmux_plugin` as a dependency and import these traits:

```rust
use bmux_plugin::{HostRuntimeApi, ServiceCaller};
```

Simple plugins that only handle commands or receive service calls do **not** need `bmux_plugin`.

## Manifest Reference (`plugin.toml`)

### Top-Level Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `id` | string | **Yes** | -- | Unique plugin identifier (e.g. `"bmux.clipboard"`) |
| `name` | string | **Yes** | -- | Human-readable display name |
| `version` | string | **Yes** | -- | Plugin version (semver) |
| `description` | string | No | -- | Optional description |
| `homepage` | string | No | -- | Optional URL |
| `runtime` | string | No | `"native"` | Runtime type (only `"native"` supported) |
| `entry` | path | No | -- | Path to `.dylib`/`.so` (only for external plugins) |
| `entry_symbol` | string | No | `"bmux_plugin_entry_v1"` | FFI entry symbol |
| `provider_priority` | integer | No | `0` | Ordering when multiple plugins provide the same capability |
| `required_capabilities` | list | No | `[]` | Host capabilities this plugin needs |
| `provided_capabilities` | list | No | `[]` | Capabilities this plugin provides |
| `provided_features` | list | No | `[]` | Feature flags this plugin provides |

### Compatibility (optional)

```toml
[plugin_api]
minimum = "1.0"    # default; omit entire section if 1.0 is fine
maximum = "2.0"    # optional upper bound

[native_abi]
minimum = "1.0"    # default; omit entire section if 1.0 is fine
```

### Commands

```toml
[[commands]]
name          = "hello"             # required -- dispatch name (matches ctx.command)
summary       = "Print a greeting"  # required -- short description for help text
expose_in_cli = true                # default: false -- set true to show as CLI subcommand
path          = ["greet"]           # optional -- CLI subcommand path
aliases       = [["say", "hi"]]    # optional -- alternative CLI paths
execution     = "provider_exec"     # default: "provider_exec"
description   = "Longer help"       # optional
```

#### Command Arguments

```toml
[[commands.arguments]]
name          = "target"       # required
kind          = "string"       # required: string | integer | boolean | path | choice
required      = true           # default: false
position      = 0              # optional: positional index (omit for flags/options)
long          = "target"       # optional: --target
short         = "t"            # optional: -t
multiple      = true           # default: false
value_name    = "TARGET"       # optional: displayed in help
choice_values = ["a", "b"]    # for kind = "choice"
```

### Services

```toml
[[services]]
capability   = "bmux.clipboard.write"    # required -- host capability scope
interface_id = "clipboard-write/v1"      # required -- service interface identifier
kind         = "command"                 # required -- "command" or "query"
```

### Event Subscriptions

```toml
[[event_subscriptions]]
kinds = ["system", "window"]
names = ["server_started", "window_created"]
```

### Dependencies

```toml
[[dependencies]]
plugin_id   = "bmux.permissions"
version_req = "=0.0.1-alpha.0"
required    = true                # default: true
```

### Keybindings

```toml
[keybindings.runtime]
c = "plugin:bmux.windows:new-window"
"alt+w" = "plugin:bmux.windows:switch-window"

[keybindings.scroll]
y = "copy_scrollback"

[keybindings.global]
# global keybindings (active outside runtime mode)
```
