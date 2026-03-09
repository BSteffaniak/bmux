# bmux_plugin

Native-first plugin SDK for bmux.

## Goals

- Keep the default plugin authoring path simple
- Support deep native integrations without exposing unstable internals directly
- Separate bmux plugin API and native ABI versioning from the bmux product version
- Make it possible to move more built-in functionality onto plugin-style contracts over time

## What This Crate Provides Today

- Plugin manifest parsing
- Stable plugin declaration types
- Capability tiers and risk classification
- Host service traits for plugin-facing integrations
- Plugin registry and host compatibility validation
- Native plugin entrypoint metadata constants

## Current Scope

This crate now supports native plugin discovery, validation, loading, lifecycle hooks, command execution, and event delivery. Host services are still intentionally narrow and deeper runtime hooks are still being built out.

## Manifest Example

```toml
id = "git.status"
name = "Git Status"
version = "0.1.0"
runtime = "native"
entry = "libgit_status.dylib"
capabilities = ["commands", "event_subscription"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
```

The manifest `entry` should point at the installed plugin bundle artifact, typically a library placed next to the manifest, rather than a hardcoded Cargo `target/` path.

## Rust Example

```rust
use bmux_plugin::{
    CommandExecutionKind, NativeCommandContext, NativeDescriptor, PluginCapability,
    PluginCommand, PluginManifestCompatibility, RustPlugin,
};

#[derive(Default)]
struct HelloPlugin;

impl RustPlugin for HelloPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor {
            id: "hello.example".to_string(),
            display_name: "Hello Example".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            native_abi: PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            description: Some("Small example plugin".to_string()),
            homepage: None,
            capabilities: [PluginCapability::Commands].into_iter().collect(),
            commands: vec![PluginCommand {
                name: "hello".to_string(),
                path: Vec::new(),
                aliases: Vec::new(),
                summary: "Print a greeting".to_string(),
                description: None,
                arguments: Vec::new(),
                execution: CommandExecutionKind::HostCallback,
                expose_in_cli: true,
            }],
            event_subscriptions: Vec::new(),
            lifecycle: Default::default(),
        }
    }

    fn run_command(&mut self, context: NativeCommandContext) -> i32 {
        match context.command.as_str() {
            "hello" => {
                println!("hello from bmux");
                0
            }
            _ => 64,
        }
    }
}

bmux_plugin::export_plugin!(HelloPlugin);
```

## Design Notes

- Prefer plugin-facing DTOs, handles, and service traits over direct access to server internals
- Treat hot-path runtime hooks as explicit high-risk capabilities
- Keep ordinary plugins compatible across bmux releases by stabilizing `bmux_plugin` first
- Leave room for future non-Rust runtimes by defining manifests and capabilities as host concepts
- Keep plugin domain ownership, capability policy, and migration intent in code and tests rather than external markdown planning docs
