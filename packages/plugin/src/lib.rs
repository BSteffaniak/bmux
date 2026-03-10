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
mod native_exports;
mod registry;
mod service;
mod version;

pub use capability::{HostScope, PluginFeature};
pub use command::{
    CommandExecutionKind, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
};
pub use declaration::{
    NativePlugin, PluginDeclaration, PluginDependency, PluginEntrypoint, PluginId, PluginLifecycle,
};
pub use discovery::{
    DEFAULT_PLUGIN_MANIFEST_FILE, PluginDiscoveryReport, discover_plugin_manifests,
    discover_plugin_manifests_in_roots, discover_registered_plugins,
    discover_registered_plugins_in_roots,
};
pub use error::{PluginError, Result};
pub use event::{PluginEvent, PluginEventKind, PluginEventPayload, PluginEventSubscription};
pub use host::{
    ClientQueryService, ClientSummary, ClipboardService, ConfigService, EventService, FollowState,
    HostConnectionInfo, HostMetadata, PaneFocusDirection, PaneHandle, PaneLayoutNode, PaneRef,
    PaneSnapshot, PaneSplitDirection, PaneSummary, PermissionEntry, PersistenceRestorePreview,
    PersistenceRestoreResult, PersistenceStatus, PluginContext, PluginHost, PluginStorage,
    PrincipalIdentityInfo, RenderService, ServerStatusInfo, SessionHandle, SessionRef,
    SessionRoleValue, SessionSnapshot, SessionSummary, WindowHandle, WindowRef, WindowSnapshot,
    WindowSummary,
};
pub use loader::{
    LoadedPlugin, NativeCommandContext, NativeDescriptor, NativeLifecycleContext,
    NativePluginLoader, NativeServiceContext, load_registered_plugin,
};
pub use manifest::{PluginManifest, PluginManifestCompatibility, PluginRuntime};
pub use native_exports::RustPlugin;
pub use registry::{
    CapabilityProvider, PluginCompatibilityReport, PluginRegistry, RegisteredPlugin,
    ServiceProvider,
};
pub use service::{
    CURRENT_SERVICE_PROTOCOL_VERSION, PluginService, ProviderId, RegisteredService,
    ServiceEnvelope, ServiceEnvelopeKind, ServiceError, ServiceKind, ServiceProtocolVersion,
    ServiceRequest, ServiceResponse, decode_service_envelope, decode_service_message,
    encode_service_envelope, encode_service_message,
};
pub use version::{ApiVersion, VersionRange};

/// Stable bmux plugin API version exposed by this crate.
pub const CURRENT_PLUGIN_API_VERSION: ApiVersion = ApiVersion::new(1, 0);

/// Stable native entrypoint ABI version exposed by this crate.
pub const CURRENT_PLUGIN_ABI_VERSION: ApiVersion = ApiVersion::new(1, 0);

/// Default exported symbol that a native plugin should expose.
pub const DEFAULT_NATIVE_ENTRY_SYMBOL: &str = "bmux_plugin_entry_v1";

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

#[doc(hidden)]
pub mod __private {
    pub use crate::native_exports::{
        activate_export, deactivate_export, descriptor_ptr, handle_event_export,
        invoke_service_export, plugin_instance, run_command_export,
    };
}

#[macro_export]
macro_rules! export_plugin {
    ($plugin_ty:ty) => {
        fn __bmux_plugin_instance() -> &'static ::std::sync::Mutex<$plugin_ty> {
            static INSTANCE: ::std::sync::OnceLock<::std::sync::Mutex<$plugin_ty>> =
                ::std::sync::OnceLock::new();
            $crate::__private::plugin_instance(&INSTANCE)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bmux_plugin_entry_v1() -> *const ::std::ffi::c_char {
            static DESCRIPTOR: ::std::sync::OnceLock<Option<::std::ffi::CString>> =
                ::std::sync::OnceLock::new();
            $crate::__private::descriptor_ptr(__bmux_plugin_instance(), &DESCRIPTOR)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bmux_plugin_run_command_with_context_v1(
            context: *const ::std::ffi::c_char,
        ) -> i32 {
            $crate::__private::run_command_export(__bmux_plugin_instance(), context)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bmux_plugin_activate_v1(context: *const ::std::ffi::c_char) -> i32 {
            $crate::__private::activate_export(__bmux_plugin_instance(), context)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bmux_plugin_deactivate_v1(context: *const ::std::ffi::c_char) -> i32 {
            $crate::__private::deactivate_export(__bmux_plugin_instance(), context)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bmux_plugin_handle_event_v1(event: *const ::std::ffi::c_char) -> i32 {
            $crate::__private::handle_event_export(__bmux_plugin_instance(), event)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn bmux_plugin_invoke_service_v1(
            input_ptr: *const u8,
            input_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
        ) -> i32 {
            $crate::__private::invoke_service_export(
                __bmux_plugin_instance(),
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
            )
        }
    };
}
