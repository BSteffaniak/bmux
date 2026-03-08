#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Native-first plugin SDK for bmux.
//!
//! The goal of this crate is to provide a stable, ergonomic contract between
//! the bmux host and plugins without exposing the internal server, CLI, or
//! terminal runtime implementation details directly.

mod capability;
mod command;
mod declaration;
mod discovery;
mod error;
mod event;
mod host;
mod loader;
mod manifest;
mod registry;
mod version;

pub use capability::{PluginCapability, PluginCapabilityTier, PluginRisk};
pub use command::{
    CommandExecutionKind, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
};
pub use declaration::{
    NativePlugin, PluginDeclaration, PluginEntrypoint, PluginId, PluginLifecycle,
};
pub use discovery::{
    DEFAULT_PLUGIN_MANIFEST_FILE, PluginDiscoveryReport, discover_plugin_manifests,
    discover_registered_plugins,
};
pub use error::{PluginError, Result};
pub use event::{PluginEvent, PluginEventKind, PluginEventPayload, PluginEventSubscription};
pub use host::{
    ClipboardService, CommandService, ConfigService, EventService, HostMetadata, PaneHandle,
    PaneService, PluginContext, PluginHost, PluginStorage, RenderService, SessionHandle,
    SessionService, WindowHandle, WindowService,
};
pub use loader::{LoadedPlugin, NativeDescriptor, NativePluginLoader, load_registered_plugin};
pub use manifest::{PluginManifest, PluginManifestCompatibility, PluginRuntime};
pub use registry::{PluginCompatibilityReport, PluginRegistry, RegisteredPlugin};
pub use version::{ApiVersion, VersionRange};

/// Stable bmux plugin API version exposed by this crate.
pub const CURRENT_PLUGIN_API_VERSION: ApiVersion = ApiVersion::new(1, 0);

/// Stable native entrypoint ABI version exposed by this crate.
pub const CURRENT_PLUGIN_ABI_VERSION: ApiVersion = ApiVersion::new(1, 0);

/// Default exported symbol that a native plugin should expose.
pub const DEFAULT_NATIVE_ENTRY_SYMBOL: &str = "bmux_plugin_entry_v1";

/// Default exported symbol used to invoke a plugin command.
pub const DEFAULT_NATIVE_COMMAND_SYMBOL: &str = "bmux_plugin_run_command_v1";
