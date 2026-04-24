#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::result_large_err)]

//! Host-side plugin infrastructure for bmux.
//!
//! This crate provides the registry, loader, discovery, and validation
//! machinery that the bmux runtime uses to manage plugins.
//!
//! **Plugin authors** should depend on [`bmux_plugin_sdk`] instead — it
//! provides the [`RustPlugin`](bmux_plugin_sdk::RustPlugin) trait, context
//! types, macros, and everything needed to write a plugin without pulling
//! in the host-side dependencies.

pub mod action_dispatch;
mod declaration;
mod discovery;
mod event_bus;
mod host_runtime;
mod loader;
mod manifest;
mod plugin_state;
pub mod prompt;
mod registry;
mod service_location;
mod static_vtable_registry;
pub mod test_support;
mod typed_service_caller;

pub use bmux_plugin_sdk::PluginEventKind;
pub use declaration::{
    NativePlugin, PluginDeclaration, PluginDependency, PluginEntrypoint, PluginExecutionClass,
    PluginId, PluginLifecycle, PluginOwnedPath,
};
pub use discovery::{
    DEFAULT_PLUGIN_MANIFEST_FILE, PluginDiscoveryReport, discover_plugin_manifests,
    discover_plugin_manifests_in_roots, discover_registered_plugins,
    discover_registered_plugins_in_roots,
};
pub use event_bus::{
    DEFAULT_EVENT_BUS_CAPACITY, EventBus, EventBusError, EventBusResult, global_event_bus,
};
pub use host_runtime::{HostRuntimeApi, ServiceCaller};
pub use loader::{LoadedPlugin, NativePluginLoader, load_registered_plugin, load_static_plugin};
pub use manifest::{
    PluginManifest, PluginManifestCompatibility, PluginManifestKeybindings, PluginRuntime,
};
pub use plugin_state::PluginStateRegistry;
pub use plugin_state::global_registry as global_plugin_state_registry;
pub use registry::{
    CapabilityProvider, PluginCompatibilityReport, PluginRegistry, RegisteredPlugin,
    ServiceProvider,
};
pub use service_location::{ServiceLocation, ServiceLocationMap, global_service_locations};
pub use static_vtable_registry::{register_static_vtable, static_vtable};
pub use typed_service_caller::TypedServiceCaller;

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
