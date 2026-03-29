# bmux_plugin

Host-side plugin infrastructure for bmux.

> **Writing a plugin?** See [`bmux_plugin_sdk`](../plugin-sdk/README.md) instead -- it has the trait, macros, quick-start guide, and manifest reference.

## What This Crate Provides

This crate is used by the bmux host runtime, not by plugin authors directly. It provides:

- **`PluginRegistry`** -- registers, validates, and indexes discovered plugins
- **`NativePluginLoader`** / **`LoadedPlugin`** -- dynamic (`dlopen`) and static plugin loading
- **`PluginManifest`** -- parses `plugin.toml` manifest files
- **`PluginDeclaration`** -- validated, type-checked plugin metadata
- **`discover_plugin_manifests`** -- filesystem discovery of plugin manifests
- **`ServiceCaller`** -- trait for dispatching cross-plugin service calls (implemented for context types)
- **`HostRuntimeApi`** -- ergonomic host service methods (`session_list`, `storage_get`, `context_create`, etc.)

Plugins that call host services import `ServiceCaller` and `HostRuntimeApi` from this crate. Everything else comes from `bmux_plugin_sdk`.

## Design Notes

- Prefer plugin-facing DTOs, handles, and service traits over direct access to server internals
- Treat hot-path runtime hooks as explicit high-risk host scopes
- Keep ordinary plugins compatible across bmux releases by stabilizing `bmux_plugin_sdk` first
- Leave room for future non-Rust runtimes by defining manifests, host scopes, and plugin features as host concepts
