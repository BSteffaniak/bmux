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

## Rust Example

```rust
use bmux_plugin::{
    ApiVersion, HostMetadata, PluginCapability, PluginManifest, PluginRegistry,
};

let manifest = PluginManifest::from_toml_str(
    r#"
id = "git.status"
name = "Git Status"
version = "0.1.0"
entry = "libgit_status.dylib"
capabilities = ["commands"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
)?;

let mut registry = PluginRegistry::new();
registry.register_manifest(std::path::Path::new("plugins/git/plugin.toml"), manifest)?;

let host = HostMetadata {
    product_name: "bmux".to_string(),
    product_version: "0.1.0".to_string(),
    plugin_api_version: ApiVersion::new(1, 0),
    plugin_abi_version: ApiVersion::new(1, 0),
};

registry.validate_against_host(&host, &[PluginCapability::Commands])?;
# Ok::<(), bmux_plugin::PluginError>(())
```

## Design Notes

- Prefer plugin-facing DTOs, handles, and service traits over direct access to server internals
- Treat hot-path runtime hooks as explicit high-risk capabilities
- Keep ordinary plugins compatible across bmux releases by stabilizing `bmux_plugin` first
- Leave room for future non-Rust runtimes by defining manifests and capabilities as host concepts

## Related Docs

- `docs/plugin-architecture.md`
- `docs/plugin-versioning.md`
- `examples/native-plugin/README.md`
