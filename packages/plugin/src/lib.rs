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
mod host_services;
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
pub use host::{HostConnectionInfo, HostMetadata, PluginContext, PluginHost};
pub use host_services::{
    ContextCloseRequest, ContextCloseResponse, ContextCreateRequest, ContextCreateResponse,
    ContextCurrentResponse, ContextListResponse, ContextSelectRequest, ContextSelectResponse,
    ContextSelector, ContextSummary, CurrentClientResponse, HostRuntimeApi, LogWriteLevel,
    LogWriteRequest, PaneCloseRequest, PaneCloseResponse, PaneFocusDirection, PaneFocusRequest,
    PaneFocusResponse, PaneListRequest, PaneListResponse, PaneResizeRequest, PaneResizeResponse,
    PaneSelector, PaneSplitDirection, PaneSplitRequest, PaneSplitResponse, PaneSummary,
    PluginCommandEffect, PluginCommandOutcome, RecordingWriteEventRequest,
    RecordingWriteEventResponse, SessionCreateRequest, SessionCreateResponse, SessionKillRequest,
    SessionKillResponse, SessionListResponse, SessionSelectRequest, SessionSelectResponse,
    SessionSelector, SessionSummary, StorageGetRequest, StorageGetResponse, StorageSetRequest,
};
pub use loader::{
    HostKernelBridge, HostKernelBridgeRequest, HostKernelBridgeResponse, LoadedPlugin,
    NativeCommandContext, NativeLifecycleContext, NativePluginLoader, NativeServiceContext,
    RegisteredPluginInfo, ServiceCaller, StaticPluginVtable, load_registered_plugin,
    load_static_plugin,
};
pub use manifest::{
    PluginManifest, PluginManifestCompatibility, PluginManifestKeybindings, PluginRuntime,
};
pub use native_exports::{
    EXIT_ERROR, EXIT_OK, EXIT_UNAVAILABLE, EXIT_USAGE, PluginCommandError, RustPlugin,
};
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

/// Convenience helper for implementing a service operation.
///
/// Handles the common decode-request → run-handler → encode-response pattern
/// that every service provider repeats.  The handler receives the decoded
/// request plus the full [`NativeServiceContext`] and returns either a typed
/// response or a pre-built [`ServiceResponse`] error.
///
/// # Example
///
/// ```ignore
/// ("clipboard-write/v1", "copy_text") => {
///     handle_service(&context, |req: CopyRequest, _ctx| {
///         do_copy(&req.text).map_err(|e| ServiceResponse::error("failed", e.to_string()))?;
///         Ok(())
///     })
/// }
/// ```
pub fn handle_service<Req, Resp, F>(context: &NativeServiceContext, handler: F) -> ServiceResponse
where
    Req: serde::de::DeserializeOwned,
    Resp: serde::Serialize,
    F: FnOnce(Req, &NativeServiceContext) -> std::result::Result<Resp, ServiceResponse>,
{
    let request = match decode_service_message::<Req>(&context.request.payload) {
        Ok(req) => req,
        Err(error) => {
            return ServiceResponse::error("invalid_request", error.to_string());
        }
    };
    match handler(request, context) {
        Ok(response) => match encode_service_message(&response) {
            Ok(payload) => ServiceResponse::ok(payload),
            Err(error) => ServiceResponse::error("response_encode_failed", error.to_string()),
        },
        Err(error_response) => error_response,
    }
}

/// Common imports for plugin authors.
///
/// A typical plugin can replace its individual `use bmux_plugin::{...}` imports
/// with a single `use bmux_plugin::prelude::*;` to get everything needed for
/// commands, services, lifecycle hooks, and host-runtime calls.
pub mod prelude {
    pub use crate::{
        // Exit codes
        EXIT_ERROR,
        EXIT_OK,
        EXIT_UNAVAILABLE,
        EXIT_USAGE,
        // Host runtime API
        HostRuntimeApi,
        // Context types
        NativeCommandContext,
        NativeLifecycleContext,
        NativeServiceContext,
        // Error type
        PluginCommandError,
        // Events
        PluginEvent,
        // Core trait
        RustPlugin,
        // Service types
        ServiceKind,
        ServiceResponse,
        // Codec helpers
        decode_service_message,
        encode_service_message,
        // Service helper
        handle_service,
    };
}

#[doc(hidden)]
pub mod __private {
    pub use crate::native_exports::{
        activate_export, deactivate_export, handle_event_export, invoke_service_export,
        manifest_toml_ptr, plugin_instance, run_command_export,
    };
}

#[macro_export]
macro_rules! export_plugin {
    ($plugin_ty:ty, $manifest_toml:expr $(,)?) => {
        /// When the `static-bundled` feature is active the plugin is compiled
        /// into the host binary and the [`bundled_plugin_vtable!`] macro
        /// provides the symbols instead.  The `export_plugin!` body is
        /// suppressed to avoid duplicate `#[no_mangle]` symbol collisions.
        #[cfg(not(feature = "static-bundled"))]
        const _: () = {
            fn __bmux_plugin_instance() -> &'static ::std::sync::Mutex<$plugin_ty> {
                static INSTANCE: ::std::sync::OnceLock<::std::sync::Mutex<$plugin_ty>> =
                    ::std::sync::OnceLock::new();
                $crate::__private::plugin_instance(&INSTANCE)
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn bmux_plugin_entry_v1() -> *const ::std::ffi::c_char {
                static MANIFEST: ::std::sync::OnceLock<Option<::std::ffi::CString>> =
                    ::std::sync::OnceLock::new();
                $crate::__private::manifest_toml_ptr($manifest_toml, &MANIFEST)
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
    };
}

/// Build a [`StaticPluginVtable`] for a [`RustPlugin`] type.
///
/// Unlike [`export_plugin!`], the generated functions are module-scoped
/// (not `#[no_mangle] extern "C"`), so multiple plugins can coexist in
/// the same binary without symbol collisions.  Each invocation gets its
/// own `OnceLock<Mutex<P>>` static, ensuring plugin state isolation.
///
/// **Important:** For a given plugin type, this macro must be invoked at
/// exactly one call site.  Multiple call sites for the same `$plugin_ty`
/// will produce independent `OnceLock` instances with separate state,
/// which is almost certainly not what you want.
///
/// # Example
///
/// ```ignore
/// let vtable = bmux_plugin::bundled_plugin_vtable!(
///     MyPlugin,
///     include_str!("../plugin.toml"),
/// );
/// ```
#[macro_export]
macro_rules! bundled_plugin_vtable {
    ($plugin_ty:ty, $manifest_toml:expr $(,)?) => {{
        fn __instance() -> &'static ::std::sync::Mutex<$plugin_ty> {
            static INSTANCE: ::std::sync::OnceLock<::std::sync::Mutex<$plugin_ty>> =
                ::std::sync::OnceLock::new();
            $crate::__private::plugin_instance(&INSTANCE)
        }

        fn __entry() -> *const ::std::ffi::c_char {
            static MANIFEST: ::std::sync::OnceLock<Option<::std::ffi::CString>> =
                ::std::sync::OnceLock::new();
            $crate::__private::manifest_toml_ptr($manifest_toml, &MANIFEST)
        }

        fn __run_command_with_context(context: *const ::std::ffi::c_char) -> i32 {
            $crate::__private::run_command_export(__instance(), context)
        }

        fn __activate(context: *const ::std::ffi::c_char) -> i32 {
            $crate::__private::activate_export(__instance(), context)
        }

        fn __deactivate(context: *const ::std::ffi::c_char) -> i32 {
            $crate::__private::deactivate_export(__instance(), context)
        }

        fn __handle_event(event: *const ::std::ffi::c_char) -> i32 {
            $crate::__private::handle_event_export(__instance(), event)
        }

        fn __invoke_service(
            input_ptr: *const u8,
            input_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
        ) -> i32 {
            $crate::__private::invoke_service_export(
                __instance(),
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
            )
        }

        $crate::StaticPluginVtable {
            entry: __entry,
            run_command_with_context: __run_command_with_context,
            activate: __activate,
            deactivate: __deactivate,
            handle_event: __handle_event,
            invoke_service: __invoke_service,
        }
    }};
}
