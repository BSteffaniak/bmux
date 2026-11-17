#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::result_large_err)]
#![allow(clippy::unsafe_derive_deserialize)]

//! Plugin SDK for bmux.
//!
//! This crate provides the types, traits, and helpers that plugin authors need.
//! It is intentionally slim — no filesystem discovery, no registry, no dynamic
//! loading.  The host-side machinery lives in the full `bmux_plugin` crate,
//! which re-exports everything from this SDK.
//!
//! # Unified author API
//!
//! Implement [`PluginInstance`] once, declare an inert definition with
//! [`define_plugin!`], and create one instance per host scope. Implementation
//! libraries export no native symbols. A separate thin cdylib calls
//! [`export_plugin_entrypoint!`] to expose the explicitly selected v2 ABI.
//! Typed static BPDL adapters register through the same scoped implementation.
//! Existing `RustPlugin` implementations are adapted per scoped instance by
//! `define_plugin!(rust_plugin = ...)`; there is no other export path.
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
//!     type Contract = bmux_plugin_sdk::NoPluginContract;
//!
//!     fn run_command(&mut self, ctx: NativeCommandContext) -> Result<i32, PluginCommandError> {
//!         match ctx.command.as_str() {
//!             "hello" => { println!("Hello!"); Ok(EXIT_OK) }
//!             _ => Err(PluginCommandError::unknown_command(&ctx.command)),
//!         }
//!     }
//! }
//!
//! // `bpdl/my-plugin.bpdl` declares `plugin example.my version 1;` and a
//! // `world`; `plugin.toml` holds the remaining settings.
//! pub mod declaration {
//!     bmux_plugin_schema_macros::schema! {
//!         source: "bpdl/my-plugin.bpdl",
//!         manifest: "plugin.toml",
//!     }
//! }
//!
//! bmux_plugin_sdk::define_plugin!(
//!     rust_plugin = MyPlugin,
//!     manifest = declaration::PLUGIN_MANIFEST,
//!     roles = vec![bmux_plugin_sdk::ComponentRole::Backend],
//! );
//! ```

pub mod abi;
pub mod definition;
pub use definition::{
    ActivationId, CommandOutcome, ComponentRole, ContractDescriptor, ContractEvidence,
    DeclaredServiceContract, ExtensionDeclaration, InstanceRegistrations, LoadedInstanceId,
    PluginCallContext, PluginCreateContext, PluginDeclarations, PluginDefinition, PluginFactory,
    PluginFuture, PluginHostServices, PluginInstance, PluginLifecycleContext, PluginMetadata,
    PluginResult, RegistrationMutation, RuntimeScopeId,
};

pub mod action_dispatch;
pub mod callback_execution;
pub use callback_execution::{
    PluginBlockingCallback, PluginCallbackAuthority, PluginCallbackAuthoritySource,
    PluginCallbackContext, PluginCallbackEntryGuard, PluginCallbackExecution,
    PluginCallbackExecutor, PluginCallbackSettlement,
};
mod cancellation;
mod capability;
mod command;
mod context;
mod contribution;
mod error;
mod event;
mod host;
mod host_services;
mod ident;
mod process_runtime;
pub mod prompt;
mod ready;
mod rust_plugin;
mod service;
extern crate self as bmux_plugin_sdk;

/// Generated transport contract for the existing versioned argv bridge.
pub mod cli_bridge {
    include!(concat!(env!("OUT_DIR"), "/cli_bridge.rs"));
}

pub mod service_contract;
mod stateful_plugin;
pub mod typed_dispatch;
mod typed_dispatch_client;
mod version;
mod wire_event_sink;

pub use bmux_codec as codec;
pub use bmux_perf_telemetry as perf_telemetry;
/// plug-rt: spawn, blocking work and timers on the host's single runtime.
/// Plugin code uses this instead of building or reaching a runtime directly.
pub use bmux_plugin_runtime::rt;
pub use cancellation::{
    CancellationToken, PluginCancellationToken, PluginInvocationId, ServiceCancellation,
};
pub use capability::{HostScope, PluginFeature};
pub use command::{
    CommandExecutionKind, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
};
pub use context::{
    ActiveKeybinding, CORE_CLI_BRIDGE_MAGIC_V1, CORE_CLI_BRIDGE_PROTOCOL_V1,
    CORE_CLI_COMMAND_CAPABILITY, CORE_CLI_COMMAND_INTERFACE_V1,
    CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1, CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
    CORE_INVOCATION_CAPABILITY, CORE_INVOCATION_INTERFACE_V1,
    CORE_INVOCATION_STREAM_ORIGIN_OPERATION_V1, CoreCliCommandRequest, CoreCliCommandResponse,
    HostKernelBridge, HostKernelBridgeRequest, HostKernelBridgeResponse, NativeCommandContext,
    NativeCommandInvocationSource, NativeLifecycleContext, NativeLifecycleContextV1,
    NativeServiceContext, NativeStreamingServiceContext, PLUGIN_CLI_BRIDGE_MAGIC_V1,
    PluginCliCommandRequest, PluginCliCommandResponse, RegisteredPluginInfo,
    ResolvedAuxiliaryExecutable, SERVICE_ERROR_CANCELLED,
    decode_host_kernel_bridge_cli_command_payload,
    decode_host_kernel_bridge_plugin_command_payload,
    encode_host_kernel_bridge_cli_command_payload,
    encode_host_kernel_bridge_plugin_command_payload,
};
pub use contribution::{
    ContributionRegistrar, PluginContribution, RegistrationContract, RegistrationKind,
};
pub use error::{CapabilityAccessDeniedHint, PluginError, Result};
pub use event::{
    PluginEvent, PluginEventDelivery, PluginEventPayload, PluginEventPublication,
    PluginEventSubscription,
};
pub use host::{
    BoundHostServicePort, HostConnectionInfo, HostMetadata, HostServiceDispatch,
    HostServiceRequest, PluginContext, PluginHost, ResolvedService,
};
pub use host_services::{
    COMMAND_OUTCOME_STATUS_MESSAGE_KEY, HostServiceFuture, HostServiceOwnedRequest, LogWriteLevel,
    LogWriteRequest, PluginCommandOutcome, StorageGetRequest, StorageGetResponse, StorageKey,
    StorageKeyError, StorageSetRequest, VolatileStateClearRequest, VolatileStateGetRequest,
    VolatileStateGetResponse, VolatileStateSetRequest, begin_command_outcome_capture,
    finish_command_outcome_capture, record_command_outcome_metadata, storage_key_is_valid,
};
pub use ident::{CapabilityId, InterfaceId, OperationId, PluginEventKind};

#[macro_export]
macro_rules! storage_key {
    ($key:literal) => {{
        const _: () = assert!(
            $crate::storage_key_is_valid($key),
            "invalid storage key literal; use non-empty [A-Za-z0-9._-]"
        );
        $crate::StorageKey::from_validated_literal($key)
    }};
}
pub use process_runtime::{
    PROCESS_RUNTIME_ENV_PERSISTENT_WORKER, PROCESS_RUNTIME_ENV_PLUGIN_ID,
    PROCESS_RUNTIME_ENV_PROTOCOL, PROCESS_RUNTIME_MAGIC_V1, PROCESS_RUNTIME_PROTOCOL_V1,
    PROCESS_RUNTIME_TRANSPORT_STDIO_V1, ProcessInvocationRequest, ProcessInvocationResponse,
    decode_process_invocation_response, decode_process_runtime_frame,
    encode_process_invocation_request, encode_process_invocation_response,
    encode_process_runtime_frame,
};
pub use ready::{ReadySignalDecl, ReadyStatus, ReadyTracker};
pub use rust_plugin::{
    EXIT_ERROR, EXIT_OK, EXIT_UNAVAILABLE, EXIT_USAGE, HostAsyncHandle, PluginCommandError,
    RustPlugin, TypedServiceRegistrationContext,
};
pub use service::{
    BoundedServiceEventSink, CURRENT_SERVICE_PROTOCOL_VERSION, NoPluginContract, PluginContract,
    PluginService, ProviderId, RegisteredService, ServiceEnvelope, ServiceEnvelopeKind,
    ServiceError, ServiceEvent, ServiceEventSink, ServiceEventSinkHandle,
    ServiceInterfaceDescriptor, ServiceKind, ServiceProtocolVersion, ServiceRequest,
    ServiceResponse, ServiceStreamCompletion, ServiceStreamContext, TypedServiceEventSink,
    decode_service_envelope, decode_service_envelope_with_invocation_id, decode_service_message,
    decode_service_stream_envelopes, encode_service_envelope,
    encode_service_envelope_with_invocation_id, encode_service_message,
    encode_service_stream_envelopes,
};
pub use stateful_plugin::{
    StatefulPlugin, StatefulPluginError, StatefulPluginHandle, StatefulPluginResult,
    StatefulPluginSnapshot,
};
pub use typed_dispatch::{
    InProcessTypedDispatch, PortableServiceProvider, TypedDispatchError, TypedProviderCell,
    TypedServiceHandle, TypedServiceKey, TypedServiceRegistry,
};
pub use typed_dispatch_client::{
    StreamingTypedServiceEndpoint, TypedDispatchClient, TypedDispatchClientError,
    TypedDispatchClientResult, TypedServiceClientError, TypedServiceClientResult,
    TypedServiceEndpoint, invoke_typed_service, invoke_typed_streaming_service,
};
pub use version::{ApiVersion, VersionRange};
pub use wire_event_sink::{
    NoopWireEventSink, WireEventSink, WireEventSinkError, WireEventSinkHandle, WirePullSource,
};

// Prompt types — re-exported at the crate root for convenience.
pub use prompt::{
    PromptEvent, PromptField, PromptFormField, PromptFormFieldKind, PromptFormLayout,
    PromptFormSection, PromptFormValue, PromptOption, PromptPolicy, PromptRequest, PromptResponse,
    PromptSearchMatchMode, PromptValidation, PromptValue, PromptWidth,
};

// Action dispatch types.
pub use action_dispatch::ActionDispatchRequest;

/// Stable bmux plugin API version exposed by this crate.
pub const CURRENT_PLUGIN_API_VERSION: ApiVersion = ApiVersion::new(1, 0);

/// Stable native entrypoint ABI version exposed by this crate.
pub const CURRENT_PLUGIN_ABI_VERSION: ApiVersion = ApiVersion::new(1, 1);

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
    ($context:ident, { $( $endpoint:ty => $handler:expr ),* $(,)? }) => {{
        let __interface = $context.request.service.interface_id.as_str();
        let __operation = $context.request.operation.as_str();
        $(
            if __interface == <$endpoint as $crate::TypedServiceEndpoint>::INTERFACE_ID.as_str()
                && __operation == <$endpoint as $crate::TypedServiceEndpoint>::OPERATION.as_str()
            {
                $crate::handle_service::<
                    <$endpoint as $crate::TypedServiceEndpoint>::Request,
                    <$endpoint as $crate::TypedServiceEndpoint>::Response,
                    _,
                >(&$context, $handler)
            } else
        )*
        {
            $crate::ServiceResponse::error(
                "unsupported_service_operation",
                format!(
                    "plugin '{}' does not support service operation '{}:{}'",
                    $context.plugin_id, __interface, __operation,
                ),
            )
        }
    }};
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
    pub use crate::definition::{
        ActivationId, CommandOutcome, ComponentRole, ContractDescriptor, LoadedInstanceId,
        PluginCallContext, PluginCreateContext, PluginDefinition, PluginFactory, PluginFuture,
        PluginHostServices, PluginInstance, PluginLifecycleContext, PluginMetadata, PluginResult,
        RuntimeScopeId,
    };
    pub use crate::{
        // Action dispatch
        ActionDispatchRequest,
        ContributionRegistrar,
        // Exit codes
        EXIT_ERROR,
        EXIT_OK,
        EXIT_UNAVAILABLE,
        EXIT_USAGE,
        // Context types
        HostAsyncHandle,
        NativeCommandContext,
        NativeLifecycleContext,
        NativeServiceContext,
        // Error type
        PluginCommandError,
        PluginContribution,
        // Events
        PluginEvent,
        PromptEvent,
        // Prompt types
        PromptField,
        PromptFormField,
        PromptFormFieldKind,
        PromptFormSection,
        PromptFormValue,
        PromptOption,
        PromptPolicy,
        PromptRequest,
        PromptResponse,
        PromptSearchMatchMode,
        PromptValidation,
        PromptValue,
        PromptWidth,
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
