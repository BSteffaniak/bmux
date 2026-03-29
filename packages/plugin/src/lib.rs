#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Plugin system for bmux.
//!
//! This crate combines the plugin SDK (re-exported from [`bmux_plugin_sdk`])
//! with the host-side loading, registry, and discovery infrastructure.
//!
//! **Plugin authors** should depend on `bmux_plugin_sdk` directly for a slim
//! dependency footprint.  This crate is for the host runtime and tools that
//! need the full plugin lifecycle (loading, validation, discovery).

// ── Re-export everything from the SDK ────────────────────────────────────────
//
// This ensures backward compatibility: code that uses `bmux_plugin::RustPlugin`
// continues to work without changes.

pub use bmux_plugin_sdk::*;

// Also re-export the SDK's prelude and __private modules by name so that
// `bmux_plugin::prelude::*` and macro-generated `$crate::__private::*`
// paths resolve correctly.
#[doc(hidden)]
pub use bmux_plugin_sdk::__private;
pub use bmux_plugin_sdk::prelude;

// ── Host-only modules ────────────────────────────────────────────────────────

mod declaration;
mod discovery;
mod host_runtime;
mod loader;
mod manifest;
mod registry;

pub use declaration::{
    NativePlugin, PluginDeclaration, PluginDependency, PluginEntrypoint, PluginId, PluginLifecycle,
};
pub use discovery::{
    DEFAULT_PLUGIN_MANIFEST_FILE, PluginDiscoveryReport, discover_plugin_manifests,
    discover_plugin_manifests_in_roots, discover_registered_plugins,
    discover_registered_plugins_in_roots,
};
pub use host_runtime::{HostRuntimeApi, ServiceCaller};
pub use loader::{LoadedPlugin, NativePluginLoader, load_registered_plugin, load_static_plugin};
pub use manifest::{
    PluginManifest, PluginManifestCompatibility, PluginManifestKeybindings, PluginRuntime,
};
pub use registry::{
    CapabilityProvider, PluginCompatibilityReport, PluginRegistry, RegisteredPlugin,
    ServiceProvider,
};

/// Default exported symbol used to invoke a plugin command.
pub const DEFAULT_NATIVE_COMMAND_SYMBOL: &str = "bmux_plugin_run_command_v1";

/// Default exported symbol used to invoke a plugin command with host context.
pub const DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL: &str =
    "bmux_plugin_run_command_with_context_v1";

/// Default exported symbol used to activate a plugin lifecycle hook.
pub const DEFAULT_NATIVE_ACTIVATE_SYMBOL: &str = "bmux_plugin_activate_v1";

/// Default exported symbol used to deactivate a plugin lifecycle hook.
pub const DEFAULT_NATIVE_DEACTIVATE_SYMBOL: &str = "bmux_plugin_deactivate_v1";

/// Default exported symbol used to deliver plugin events.
pub const DEFAULT_NATIVE_EVENT_SYMBOL: &str = "bmux_plugin_handle_event_v1";

/// Default exported symbol used to invoke a plugin-provided service.
pub const DEFAULT_NATIVE_SERVICE_SYMBOL: &str = "bmux_plugin_invoke_service_v1";
