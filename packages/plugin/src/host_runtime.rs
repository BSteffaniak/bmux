//! Host-side service caller trait and `HostRuntimeApi`.
//!
//! These traits require the host-side service dispatch implementation
//! (provided via `bmux_ipc`) and are therefore defined here rather than
//! in the SDK.
//!
//! `HostRuntimeApi` carries only truly generic primitives. Plugins
//! that speak in session/context/pane/client domain terms either use
//! [`ServiceCaller::execute_kernel_request`] for foundational access
//! to core IPC, or typed BPDL services (`call_service`) for
//! cross-plugin orchestration.

use bmux_plugin_sdk::{
    CORE_CLI_COMMAND_CAPABILITY, CORE_CLI_COMMAND_INTERFACE_V1,
    CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1, CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
    CoreCliCommandRequest, CoreCliCommandResponse, LogWriteRequest, PluginCliCommandRequest,
    PluginCliCommandResponse, RecordingWriteEventRequest, RecordingWriteEventResponse, Result,
    ServiceKind, StorageGetRequest, StorageGetResponse, StorageSetRequest,
};
use serde::{Serialize, de::DeserializeOwned};

/// Trait for types that can dispatch cross-plugin service calls.
///
/// The three context types ([`NativeCommandContext`](crate::NativeCommandContext),
/// [`NativeLifecycleContext`](crate::NativeLifecycleContext),
/// [`NativeServiceContext`](crate::NativeServiceContext)) and the
/// long-lived [`TypedServiceCaller`](crate::TypedServiceCaller)
/// implement this trait.
pub trait ServiceCaller {
    /// Dispatch a raw service call with a binary payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the capability is not accessible, the service
    /// is not registered, or the provider returns a transport-level error.
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>>;

    /// Dispatch a typed service call, serializing the request and
    /// deserializing the response.
    ///
    /// # Errors
    ///
    /// Returns an error when the capability is not accessible, the service
    /// is not registered, or the provider returns a transport-level error.
    fn call_service<Request, Response>(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        request: &Request,
    ) -> Result<Response>
    where
        Request: Serialize,
        Response: DeserializeOwned,
    {
        let payload = bmux_plugin_sdk::encode_service_message(request)?;
        let response = self.call_service_raw(capability, kind, interface_id, operation, payload)?;
        bmux_plugin_sdk::decode_service_message(&response)
    }

    /// Dispatch an IPC request directly to the host kernel.
    ///
    /// Used by domain plugins (sessions, contexts, clients, windows)
    /// that own typed services backed by core state the host kernel
    /// exposes over IPC. The typed service layer would otherwise cycle
    /// back into the plugin's own handlers.
    ///
    /// # Errors
    ///
    /// Returns an error when the host kernel bridge is missing
    /// (typical in tests), or the server returns an error response.
    fn execute_kernel_request(
        &self,
        request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload>;
}

/// Generic host runtime API.
///
/// Carries only domain-agnostic primitives: core-CLI command dispatch,
/// plugin-command dispatch, key-value storage, logging, and recording.
/// Every blanket-implemented method is a thin wrapper over a well-known
/// host-provided service (`bmux.storage`, `bmux.logs.write`, etc.).
///
/// Domain-specific conveniences (session/context/pane/client helpers)
/// are not part of this trait. Foundational plugins reach core IPC via
/// [`ServiceCaller::execute_kernel_request`]; non-foundational plugins
/// use typed BPDL services through [`ServiceCaller::call_service`].
pub trait HostRuntimeApi: ServiceCaller {
    /// Run a core built-in CLI command path in-process.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn core_cli_command_run_path(
        &self,
        request: &CoreCliCommandRequest,
    ) -> Result<CoreCliCommandResponse> {
        self.call_service(
            CORE_CLI_COMMAND_CAPABILITY,
            ServiceKind::Command,
            CORE_CLI_COMMAND_INTERFACE_V1,
            CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
            request,
        )
    }

    /// Run a plugin command in-process.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn plugin_command_run(
        &self,
        request: &PluginCliCommandRequest,
    ) -> Result<PluginCliCommandResponse> {
        self.call_service(
            CORE_CLI_COMMAND_CAPABILITY,
            ServiceKind::Command,
            CORE_CLI_COMMAND_INTERFACE_V1,
            CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
            request,
        )
    }

    /// Get a value from plugin storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn storage_get(&self, request: &StorageGetRequest) -> Result<StorageGetResponse> {
        self.call_service(
            "bmux.storage",
            ServiceKind::Query,
            "storage-query/v1",
            "get",
            request,
        )
    }

    /// Set a value in plugin storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn storage_set(&self, request: &StorageSetRequest) -> Result<()> {
        self.call_service(
            "bmux.storage",
            ServiceKind::Command,
            "storage-command/v1",
            "set",
            request,
        )
    }

    /// Write a log message.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn log_write(&self, request: &LogWriteRequest) -> Result<()> {
        self.call_service(
            "bmux.logs.write",
            ServiceKind::Command,
            "logging-command/v1",
            "write",
            request,
        )
    }

    /// Write a custom recording event.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn recording_write_event(
        &self,
        request: &RecordingWriteEventRequest,
    ) -> Result<RecordingWriteEventResponse> {
        use bmux_recording_plugin_api::{RecordingRequest, RecordingResponse};

        let recording_request = RecordingRequest::WriteCustomEvent {
            session_id: request.session_id,
            pane_id: request.pane_id,
            // `HostRuntimeApi` doesn't know which plugin is calling,
            // so we pass an empty source. The caller-side code that
            // needs source identity should dispatch directly via
            // `recording-commands::dispatch` instead.
            source: String::new(),
            name: request.name.clone(),
            payload: serde_json::to_vec(&request.payload).unwrap_or_default(),
        };
        let response: RecordingResponse = self.call_service(
            "bmux.recording.read",
            ServiceKind::Command,
            "recording-commands",
            "dispatch",
            &recording_request,
        )?;
        match response {
            RecordingResponse::CustomEventWritten { accepted } => {
                Ok(RecordingWriteEventResponse { accepted })
            }
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for recording-commands::dispatch".to_string(),
            }),
        }
    }
}

impl<T> HostRuntimeApi for T where T: ServiceCaller + ?Sized {}
