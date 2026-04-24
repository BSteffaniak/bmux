#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::result_large_err)]
#![allow(clippy::unsafe_derive_deserialize)]

//! Plugin SDK for bmux.
//!
//! This crate provides the types, traits, and helpers that plugin authors need.
//! It is intentionally slim — no filesystem discovery, no registry, no dynamic
//! loading.  The host-side machinery lives in the full `bmux_plugin` crate,
//! which re-exports everything from this SDK.
//!
//! # Quick start
//!
//! ```ignore
//! use bmux_plugin_sdk::prelude::*;
//!
//! #[derive(Default)]
//! pub struct MyPlugin;
//!
//! impl RustPlugin for MyPlugin {
//!     fn run_command(&mut self, ctx: NativeCommandContext) -> Result<i32, PluginCommandError> {
//!         match ctx.command.as_str() {
//!             "hello" => { println!("Hello!"); Ok(EXIT_OK) }
//!             _ => Err(PluginCommandError::unknown_command(&ctx.command)),
//!         }
//!     }
//! }
//!
//! bmux_plugin_sdk::export_plugin!(MyPlugin, include_str!("../plugin.toml"));
//! ```

pub mod action_dispatch;
mod capability;
mod command;
mod context;
mod error;
mod event;
mod host;
mod host_services;
mod ident;
mod native_exports;
mod process_runtime;
pub mod prompt;
mod ready;
mod service;
mod stateful_plugin;
pub mod typed_dispatch;
mod typed_dispatch_client;
mod version;
mod wire_event_sink;

pub use capability::{HostScope, PluginFeature};
pub use command::{
    CommandExecutionKind, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
};
pub use context::{
    CORE_CLI_BRIDGE_MAGIC_V1, CORE_CLI_BRIDGE_PROTOCOL_V1, CORE_CLI_COMMAND_CAPABILITY,
    CORE_CLI_COMMAND_INTERFACE_V1, CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
    CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1, CoreCliCommandRequest, CoreCliCommandResponse,
    HostKernelBridge, HostKernelBridgeRequest, HostKernelBridgeResponse, NativeCommandContext,
    NativeLifecycleContext, NativeServiceContext, PLUGIN_CLI_BRIDGE_MAGIC_V1,
    PluginCliCommandRequest, PluginCliCommandResponse, RegisteredPluginInfo,
    decode_host_kernel_bridge_cli_command_payload,
    decode_host_kernel_bridge_plugin_command_payload,
    encode_host_kernel_bridge_cli_command_payload,
    encode_host_kernel_bridge_plugin_command_payload,
};
pub use error::{PluginError, Result};
pub use event::{PluginEvent, PluginEventPayload, PluginEventPublication, PluginEventSubscription};
pub use host::{HostConnectionInfo, HostMetadata, PluginContext, PluginHost, ResolvedService};
pub use host_services::{
    LogWriteLevel, LogWriteRequest, PluginCommandOutcome, RecordingWriteEventRequest,
    RecordingWriteEventResponse, StorageGetRequest, StorageGetResponse, StorageSetRequest,
};
pub use ident::{CapabilityId, InterfaceId, OperationId, PluginEventKind};
pub use native_exports::{
    EXIT_ERROR, EXIT_OK, EXIT_UNAVAILABLE, EXIT_USAGE, PluginCommandError, RustPlugin,
    TypedServiceRegistrationContext,
};
pub use process_runtime::{
    PROCESS_RUNTIME_ENV_PERSISTENT_WORKER, PROCESS_RUNTIME_ENV_PLUGIN_ID,
    PROCESS_RUNTIME_ENV_PROTOCOL, PROCESS_RUNTIME_MAGIC_V1, PROCESS_RUNTIME_PROTOCOL_V1,
    PROCESS_RUNTIME_TRANSPORT_STDIO_V1, ProcessInvocationRequest, ProcessInvocationResponse,
    decode_process_invocation_response, decode_process_runtime_frame,
    encode_process_invocation_request, encode_process_runtime_frame,
};
pub use ready::{ReadySignalDecl, ReadyStatus, ReadyTracker};
pub use service::{
    CURRENT_SERVICE_PROTOCOL_VERSION, PluginService, ProviderId, RegisteredService,
    ServiceEnvelope, ServiceEnvelopeKind, ServiceError, ServiceKind, ServiceProtocolVersion,
    ServiceRequest, ServiceResponse, decode_service_envelope, decode_service_message,
    encode_service_envelope, encode_service_message,
};
pub use stateful_plugin::{
    StatefulPlugin, StatefulPluginError, StatefulPluginHandle, StatefulPluginResult,
    StatefulPluginSnapshot,
};
pub use typed_dispatch::{
    InProcessTypedDispatch, TypedDispatchError, TypedProviderCell, TypedServiceHandle,
    TypedServiceKey, TypedServiceRegistry,
};
pub use typed_dispatch_client::{
    TypedDispatchClient, TypedDispatchClientError, TypedDispatchClientResult,
};
pub use version::{ApiVersion, VersionRange};
pub use wire_event_sink::{
    NoopWireEventSink, WireEventSink, WireEventSinkError, WireEventSinkHandle,
};

// Prompt types — re-exported at the crate root for convenience.
pub use prompt::{
    PromptField, PromptOption, PromptPolicy, PromptRequest, PromptResponse, PromptValidation,
    PromptValue, PromptWidth,
};

// Action dispatch types.
pub use action_dispatch::ActionDispatchRequest;

/// Stable bmux plugin API version exposed by this crate.
pub const CURRENT_PLUGIN_API_VERSION: ApiVersion = ApiVersion::new(1, 0);

/// Stable native entrypoint ABI version exposed by this crate.
pub const CURRENT_PLUGIN_ABI_VERSION: ApiVersion = ApiVersion::new(1, 0);

/// Default exported symbol that a native plugin should expose.
pub const DEFAULT_NATIVE_ENTRY_SYMBOL: &str = "bmux_plugin_entry_v1";

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

/// Route inbound service requests to typed handler closures.
///
/// Generates the `match` on `(interface_id, operation)`, wraps each handler
/// in [`handle_service`], and produces a standard "unsupported" error for
/// unrecognised operations.
///
/// # Example
///
/// ```ignore
/// fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
///     bmux_plugin_sdk::route_service!(context, {
///         "clipboard-write/v1", "copy_text" => |req: CopyRequest, _ctx| {
///             do_copy(&req.text).map_err(|e| ServiceResponse::error("failed", e.to_string()))?;
///             Ok(())
///         },
///     })
/// }
/// ```
#[macro_export]
macro_rules! route_service {
    ($context:ident, { $( $interface:literal, $operation:literal => $handler:expr ),* $(,)? }) => {
        match (
            $context.request.service.interface_id.as_str(),
            $context.request.operation.as_str(),
        ) {
            $(
                ($interface, $operation) => {
                    $crate::handle_service(&$context, $handler)
                },
            )*
            (__interface, __operation) => {
                $crate::ServiceResponse::error(
                    "unsupported_service_operation",
                    format!(
                        "plugin '{}' does not support service operation '{}:{}'",
                        $context.plugin_id, __interface, __operation,
                    ),
                )
            }
        }
    };
}

/// Route inbound commands to handler expressions.
///
/// Generates the `match` on `ctx.command.as_str()` and produces a standard
/// "unknown command" error for unrecognised names.  Symmetric with
/// [`route_service!`].
///
/// # Example
///
/// ```ignore
/// fn run_command(&mut self, ctx: NativeCommandContext) -> Result<i32, PluginCommandError> {
///     bmux_plugin_sdk::route_command!(ctx, {
///         "hello" => {
///             println!("Hello!");
///             Ok(EXIT_OK)
///         },
///     })
/// }
/// ```
#[macro_export]
macro_rules! route_command {
    ($ctx:ident, { $( $name:literal => $handler:expr ),* $(,)? }) => {
        match $ctx.command.as_str() {
            $( $name => $handler, )*
            __unknown => Err($crate::PluginCommandError::unknown_command(__unknown)),
        }
    };
}

/// Common imports for plugin authors.
///
/// A typical plugin can replace individual `use bmux_plugin_sdk::{...}` imports
/// with a single `use bmux_plugin_sdk::prelude::*;` to get everything needed
/// for commands, services, lifecycle hooks, and host-runtime calls.
pub mod prelude {
    pub use crate::{
        // Action dispatch
        ActionDispatchRequest,
        // Exit codes
        EXIT_ERROR,
        EXIT_OK,
        EXIT_UNAVAILABLE,
        EXIT_USAGE,
        // Context types
        NativeCommandContext,
        NativeLifecycleContext,
        NativeServiceContext,
        // Error type
        PluginCommandError,
        // Events
        PluginEvent,
        // Prompt types
        PromptField,
        PromptOption,
        PromptPolicy,
        PromptRequest,
        PromptResponse,
        PromptValidation,
        PromptValue,
        PromptWidth,
        // Core trait
        RustPlugin,
        // Service types
        ServiceKind,
        ServiceResponse,
        // Codec helpers
        decode_service_message,
        encode_service_message,
        // Service helpers
        handle_service,
    };
}

#[doc(hidden)]
pub mod __private {
    pub use crate::native_exports::{
        activate_export, deactivate_export, handle_event_export, invoke_service_export,
        manifest_toml_ptr, plugin_instance, register_typed_services_bundled, run_command_export,
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
                input_ptr: *const u8,
                input_len: usize,
            ) -> i32 {
                $crate::__private::run_command_export(
                    __bmux_plugin_instance(),
                    input_ptr,
                    input_len,
                )
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn bmux_plugin_activate_v1(
                input_ptr: *const u8,
                input_len: usize,
            ) -> i32 {
                $crate::__private::activate_export(__bmux_plugin_instance(), input_ptr, input_len)
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn bmux_plugin_deactivate_v1(
                input_ptr: *const u8,
                input_len: usize,
            ) -> i32 {
                $crate::__private::deactivate_export(__bmux_plugin_instance(), input_ptr, input_len)
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn bmux_plugin_handle_event_v1(
                input_ptr: *const u8,
                input_len: usize,
            ) -> i32 {
                $crate::__private::handle_event_export(
                    __bmux_plugin_instance(),
                    input_ptr,
                    input_len,
                )
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
/// the same binary without symbol collisions.
///
/// # Example
///
/// ```ignore
/// let vtable = bmux_plugin_sdk::bundled_plugin_vtable!(
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

        fn __run_command_with_context(input_ptr: *const u8, input_len: usize) -> i32 {
            $crate::__private::run_command_export(__instance(), input_ptr, input_len)
        }

        fn __activate(input_ptr: *const u8, input_len: usize) -> i32 {
            $crate::__private::activate_export(__instance(), input_ptr, input_len)
        }

        fn __deactivate(input_ptr: *const u8, input_len: usize) -> i32 {
            $crate::__private::deactivate_export(__instance(), input_ptr, input_len)
        }

        fn __handle_event(input_ptr: *const u8, input_len: usize) -> i32 {
            $crate::__private::handle_event_export(__instance(), input_ptr, input_len)
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

        fn __register_typed_services(
            context: $crate::TypedServiceRegistrationContext<'_>,
        ) -> $crate::TypedServiceRegistry {
            $crate::__private::register_typed_services_bundled(__instance(), context)
        }

        $crate::StaticPluginVtable {
            entry: __entry,
            run_command_with_context: __run_command_with_context,
            activate: __activate,
            deactivate: __deactivate,
            handle_event: __handle_event,
            invoke_service: __invoke_service,
            register_typed_services: __register_typed_services,
        }
    }};
}

/// Function table for a statically-linked bundled plugin.
#[derive(Clone, Copy)]
pub struct StaticPluginVtable {
    pub entry: fn() -> *const std::ffi::c_char,
    pub run_command_with_context: fn(*const u8, usize) -> i32,
    pub activate: fn(*const u8, usize) -> i32,
    pub deactivate: fn(*const u8, usize) -> i32,
    pub handle_event: fn(*const u8, usize) -> i32,
    pub invoke_service: fn(*const u8, usize, *mut u8, usize, *mut usize) -> i32,
    pub register_typed_services:
        fn(context: TypedServiceRegistrationContext<'_>) -> TypedServiceRegistry,
}

impl std::fmt::Debug for StaticPluginVtable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticPluginVtable").finish_non_exhaustive()
    }
}
